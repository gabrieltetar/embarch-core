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

/// Open the first attached probe. Single-board scope for now, so "first" is
/// unambiguous; if you add a second probe later, this is the one place that
/// needs to grow a selector (by serial number, most likely).
fn open_first_probe() -> Result<probe_rs::probe::Probe> {
    let lister = Lister::new();
    let probes = lister.list_all();
    let probe_info = probes
        .first()
        .context("no debug probe found — check the USB connection (and usbipd attach, if Core is on a Pi and the probe is elsewhere)")?;
    probe_info.open().context("failed to open debug probe")
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
pub fn flash(chip: &str, firmware_path: &Path, format: &str, base_address: Option<u64>) -> Result<()> {
    let format = parse_format(format, base_address)?;
    let probe = open_first_probe()?;

    let mut session = probe
        .attach(chip, Permissions::default())
        .with_context(|| format!("failed to attach to target '{chip}'"))?;

    flashing::download_file(&mut session, firmware_path, format).context("flashing failed")?;

    Ok(())
}

/// Reset the target chip via the first attached probe.
pub fn reset(chip: &str) -> Result<()> {
    let probe = open_first_probe()?;

    let mut session = probe
        .attach(chip, Permissions::default())
        .with_context(|| format!("failed to attach to target '{chip}'"))?;

    let mut core = session.core(0).context("failed to select core 0")?;
    core.reset().context("reset failed")?;

    Ok(())
}
