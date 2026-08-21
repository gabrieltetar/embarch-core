use anyhow::{Context, Result};
use probe_rs::flashing::{self, BinOptions, Format};
use probe_rs::probe::list::Lister;
use probe_rs::Permissions;
use std::path::Path;

/// One attached debug probe, as reported by probe-rs.
#[derive(Debug, serde::Serialize)]
pub struct ProbeInfo {
    pub identifier: String,
    pub vendor_id: u16,
    pub product_id: u16,
    pub serial_number: Option<String>,
}

/// List every debug probe probe-rs can currently see over USB
/// (ST-Link, J-Link, CMSIS-DAP, FTDI, ESP USB-JTAG, etc).
pub fn list_probes() -> Result<Vec<ProbeInfo>> {
    let lister = Lister::new();
    let probes = lister.list_all();

    Ok(probes
        .iter()
        .map(|p| ProbeInfo {
            identifier: p.identifier.clone(),
            vendor_id: p.vendor_id,
            product_id: p.product_id,
            serial_number: p.serial_number.clone(),
        })
        .collect())
}

/// Parse a user-supplied format string into probe-rs's Format enum.
///
/// `base_address` only applies to `Format::Bin` — a raw binary has no
/// self-describing load address the way ELF/hex/uf2/idf do, so the caller
/// has to supply one. Silently ignored for every other format (documented at
/// the one caller that threads it through, `flash` below), rather than an
/// error, since a caller that always passes the same base_address regardless
/// of format (e.g. one config field covering several projects) shouldn't
/// have to special-case the format first.
fn parse_format(format: &str, base_address: Option<u64>) -> Result<Format> {
    match format.to_lowercase().as_str() {
        "elf" => Ok(Format::Elf(Default::default())),
        "bin" => Ok(Format::Bin(BinOptions {
            base_address,
            skip: 0,
        })),
        "hex" => Ok(Format::Hex),
        "uf2" => Ok(Format::Uf2),
        "idf" => Ok(Format::Idf(Default::default())),
        other => {
            anyhow::bail!("unknown firmware format '{other}' (expected elf/bin/hex/uf2/idf)")
        }
    }
}

/// Resolves which attached probe a call means, matched against
/// `ProbeInfo.serial_number` (`design.md` §3 decision 9) — real, not just
/// documented: `open_first_probe`'s own prior doc comment said this was
/// still single-probe-only, a real drift from what that decision already
/// claimed, found the first time a second probe (dev-bench's own) was
/// actually attached alongside a DUT's and picking the wrong one failed
/// loudly (a genuine attach error, wrong debug interface for the wrong
/// chip — not a silent misfire).
///
/// `None` behaves as before when exactly one probe is attached. With more
/// than one and no serial given, this is now a loud, named error rather
/// than silently picking whichever happened to enumerate first — a wrong
/// pick is a worse failure mode than an explicit one.
///
/// `pub(crate)`, not just a private helper inside `open_probe` below: this
/// is also `board_gate.rs`'s one and only source of "which probe did this
/// call mean" (`enforce`/`enroll`) — the exact selection rule, shared
/// rather than copied a second time, closing the same class of gap that let
/// this decision's own serial-number selector go silently unimplemented for
/// as long as it did (see this decision's own text in `design.md`).
pub(crate) fn resolve_probe(probe_serial: Option<&str>) -> Result<probe_rs::probe::DebugProbeInfo> {
    let lister = Lister::new();
    let probes = lister.list_all();

    if probes.is_empty() {
        anyhow::bail!(
            "no debug probe found — check the USB connection (and usbipd attach, if Core is \
             on a Pi and the probe is elsewhere)"
        );
    }

    if let Some(serial) = probe_serial {
        return probes
            .into_iter()
            .find(|p| p.serial_number.as_deref() == Some(serial))
            .with_context(|| {
                format!("no attached probe has serial_number '{serial}'")
            });
    }

    if probes.len() > 1 {
        let known: Vec<String> = probes
            .iter()
            .map(|p| format!("{} (serial={:?})", p.identifier, p.serial_number))
            .collect();
        anyhow::bail!(
            "more than one debug probe is attached and no probe_serial was given — pass one \
             to disambiguate. Attached probes: {known:?}"
        );
    }

    Ok(probes.into_iter().next().expect("checked non-empty above"))
}

/// Opens `probe_serial`'s probe if given, or the sole attached probe when
/// omitted (`resolve_probe` above resolves which one; this just opens it).
/// `embarch_topology::hardware::validate_serial` (the board-identity gate,
/// formerly this crate's own `board_gate.rs`) opens the exact same probe
/// again for its own gate-check attach, a separate connection from
/// `flash`/`reset`'s own subsequent attach (`design.md` §5: probe attach is
/// per-call, never held open across calls).
pub(crate) fn open_probe(probe_serial: Option<&str>) -> Result<probe_rs::probe::Probe> {
    resolve_probe(probe_serial)?.open().context("failed to open debug probe")
}

/// Resolves which attached probe a call means (same rule as `open_probe`)
/// and returns its USB serial number — what the board-identity gate
/// (`embarch_topology::hardware::validate_serial`) keys on. A probe with no
/// serial number can't be gated at all, since enrollment itself has nothing
/// to key on either (`embarch-topology`'s own `enroll` has the same
/// requirement).
fn resolved_serial(probe_serial: Option<&str>) -> Result<String> {
    let info = resolve_probe(probe_serial)?;
    info.serial_number.clone().ok_or_else(|| {
        anyhow::anyhow!(
            "the resolved probe ({}) reports no USB serial number — board-identity gating \
             requires one to key on",
            info.identifier
        )
    })
}

/// Flash a firmware image onto the given chip using the first attached probe.
///
/// `chip` must match a probe-rs target name (e.g. "STM32F407VG", "nRF52840_xxAA",
/// "esp32c5"). `firmware_path` is a path Core can read locally — the caller is
/// responsible for getting the file onto whatever machine Core runs on.
///
/// `base_address` is only meaningful for `format = "bin"` — a raw binary has
/// no self-describing load address (unlike ELF/hex/uf2, and unlike `idf`,
/// which builds its own bootloader+partition-table+app image from an ELF's
/// ESP-IDF app-descriptor section, `embarch-dev-bench/design.md`'s ESP JTAG
/// decision). Zephyr's own ESP32 `west flash` merges bootloader+partition-
/// table+app into one flat image (its build already logs the merge address,
/// e.g. `0x2000`) and writes it as one `esptool write-flash <addr> zephyr.bin`
/// call — `Format::Idf` doesn't apply to that image at all (Zephyr doesn't
/// emit the ESP-IDF app descriptor `Format::Idf`'s `espflash`-backed loader
/// requires, confirmed by inspecting a real built `zephyr.elf`'s sections/
/// symbols — no `esp_app_desc` symbol, no `.flash.appdesc` section). `bin` at
/// the same merge address `west flash` would have used is the mechanism that
/// actually matches what Zephyr already produces.
pub fn flash(
    chip: &str,
    firmware_path: &Path,
    format: &str,
    base_address: Option<u64>,
    probe_serial: Option<&str>,
) -> Result<()> {
    let format = parse_format(format, base_address)?;
    let gated_serial = resolved_serial(probe_serial)?;
    embarch_topology::hardware::validate_serial(&gated_serial)
        .context("board-identity gate refused this flash")?;
    let probe = open_probe(probe_serial)?;

    let mut session = probe
        .attach(chip, Permissions::default())
        .with_context(|| format!("failed to attach to target '{chip}'"))?;

    flashing::download_file(&mut session, firmware_path, format).context("flashing failed")?;

    Ok(())
}

/// Reset the target chip via the attached probe (`probe_serial` disambiguates
/// when more than one is attached, same as `flash` above).
///
/// A real finding, Milestone 3 (Study Designer: Feature-Branch Iteration),
/// 2026-08-20, narrowed down after an initial overcorrection: switching both
/// `flash` and `reset` to `Probe::attach_under_reset` (tried first) broke
/// attach entirely against the real healthband DUT — a clean, immediate
/// `Timeout while attaching to target under reset` (probe-rs's own error
/// text names the cause: the target's physical reset pin isn't connected to
/// the debug connector, true of this DUT's board). `attach_under_reset`
/// reverted to plain `attach` below for that reason — it's not safe to force
/// everywhere.
///
/// The actual, narrower problem it was chasing was dev-bench's ESP32-C5
/// specifically: its USB-Serial/JTAG peripheral's *core*-level reset (what
/// `Core::reset` below already did, and still does) doesn't re-sample the
/// chip's boot-strapping pins, so it can stay latched in ROM download mode
/// across a reset instead of returning to normal SPI boot — which also
/// explains this milestone's earlier `HelloAck` decode failures (a chip
/// stuck in its ROM bootloader was never running the app's serial protocol
/// at all, no framing bug required). The real fix for that is a genuine
/// hardware reset pulse via the *probe's* own reset line
/// ([`probe_rs::probe::Probe::target_reset`]) before attaching at all — not
/// an attach-time behavior, since flashing/attach were never what failed.
/// Best-effort: a probe/board without a wired reset pin (this same DUT,
/// again) returns a clean `NotImplemented`/`CommandNotSupportedByProbe`, not
/// a real failure, and that's not fatal — it just means this step does
/// nothing for that target, same as before this fix existed. Any other
/// error is logged, not propagated, since this step is a best-effort
/// improvement over the `Core::reset()` below, not a replacement for it.
///
/// **`target_reset()`'s `NotImplemented` doesn't mean "no wired reset pin" —
/// confirmed 2026-08-21, decision 16's new log file live against the real
/// ESP32-C5.** That first live run showed `target_reset()` returning the
/// quiet no-op branch for dev-bench's own probe, not the DUT's — reading
/// `probe-rs` 0.31.0's own `espusbjtag` driver source explains why:
/// `target_reset()` there is a hardcoded `Err(NotImplemented)`, full stop,
/// regardless of wiring. But the same driver's lower-level
/// `target_reset_assert`/`target_reset_deassert` genuinely do call
/// `self.protocol.set_reset(..)` — a real signal `probe-rs` just never wired
/// the convenience `target_reset()` method to compose them for this probe.
/// So the fallback below drives assert/hold/deassert by hand whenever
/// `target_reset()` itself comes back unimplemented, before giving up on a
/// real pulse — this is what actually gives dev-bench's ESP32-C5 the
/// boot-strap-pin re-sample decision 21 was written to provide; relying on
/// `target_reset()` alone silently never did.
pub fn reset(chip: &str, probe_serial: Option<&str>) -> Result<()> {
    let gated_serial = resolved_serial(probe_serial)?;
    embarch_topology::hardware::validate_serial(&gated_serial)
        .context("board-identity gate refused this reset")?;
    let mut probe = open_probe(probe_serial)?;

    match probe.target_reset() {
        Ok(()) => {
            tracing::info!("hardware target_reset: probe reported a genuine hardware reset pulse")
        }
        Err(
            probe_rs::probe::DebugProbeError::NotImplemented { .. }
            | probe_rs::probe::DebugProbeError::CommandNotSupportedByProbe { .. },
        ) => match reset_via_assert_deassert(&mut probe) {
            Ok(()) => tracing::info!(
                "hardware target_reset: target_reset() itself is unimplemented on this probe, \
                 but a manual assert/deassert pulse succeeded"
            ),
            Err(
                probe_rs::probe::DebugProbeError::NotImplemented { .. }
                | probe_rs::probe::DebugProbeError::CommandNotSupportedByProbe { .. },
            ) => tracing::info!(
                "hardware target_reset: probe has neither target_reset() nor \
                 assert/deassert support (or no wired reset pin at all) — skipping, falling \
                 through to a software core reset only"
            ),
            Err(e) => tracing::warn!(
                "hardware target_reset: manual assert/deassert pulse failed, continuing with \
                 a software core reset only: {e}"
            ),
        },
        Err(e) => tracing::warn!(
            "hardware target_reset failed, continuing with a software core reset only: {e}"
        ),
    }

    let mut session = probe
        .attach(chip, Permissions::default())
        .with_context(|| format!("failed to attach to target '{chip}'"))?;

    let mut core = session.core(0).context("failed to select core 0")?;
    core.reset().context("reset failed")?;

    Ok(())
}

/// How long to hold the reset line asserted in [`reset_via_assert_deassert`]
/// before deasserting. An informed guess (typical hard-reset pulse widths
/// are tens of milliseconds), not yet validated against real dev-bench
/// timing — same "placeholder, narrow later if wrong" posture `study.rs`'s
/// own `WATCHDOG_GRACE_MS` already carries in this codebase.
const RESET_PULSE_MS: u64 = 50;

/// Manually drives [`probe_rs::probe::Probe::target_reset_assert`]/
/// [`probe_rs::probe::Probe::target_reset_deassert`] as a fallback for
/// probes whose `target_reset()` convenience method is unimplemented —
/// `espusbjtag` (embarch-dev-bench's ESP32-C5 probe) is exactly this case:
/// `target_reset()` there is a hardcoded `Err(NotImplemented)`, but the
/// assert/deassert primitives it would otherwise compose from are real,
/// confirmed by reading `probe-rs` 0.31.0's own driver source (see `reset`'s
/// own doc comment above for the full narrative). Propagates whatever error
/// `assert`/`deassert` themselves return — `reset`'s caller treats
/// `NotImplemented`/`CommandNotSupportedByProbe` from *this* function the
/// same way it already treats them from `target_reset()` itself: a clean,
/// non-fatal no-op, not a failure.
fn reset_via_assert_deassert(
    probe: &mut probe_rs::probe::Probe,
) -> Result<(), probe_rs::probe::DebugProbeError> {
    probe.target_reset_assert()?;
    std::thread::sleep(std::time::Duration::from_millis(RESET_PULSE_MS));
    probe.target_reset_deassert()
}
