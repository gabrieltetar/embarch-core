//! Which program actually writes an image to a target, and how Core decides
//! (design.md §3 decision 48).
//!
//! # Why this module exists
//!
//! Until 2026-08-27 Core had exactly one flashing implementation —
//! `probe_rs::flashing::download_file` — used for every target regardless of
//! vendor. That is the defect this module closes, and it is worth stating
//! precisely, because "use nrfutil instead of probe-rs" is the small version
//! of it:
//!
//! **A Zephyr board declares how it is programmed, and Core overrode that
//! declaration with one hardcoded backend.** `board.cmake` for the reference-dut
//! DUT names three runners — `nrfutil` (first, so `west flash`'s default),
//! then `jlink`, then `pyocd`, with `--device=nRF54L15_M33 --speed=4000`
//! already filled in for the SEGGER one. Core used none of them. The same
//! failure would reach any board whose vendor runner is not
//! probe-rs-equivalent; the Nordic part is just where it surfaced.
//!
//! # The evidence that this is dangerous rather than merely impure
//!
//! `hardware::flash`'s own comment already recorded half of it on 2026-08-25:
//! `do_chip_erase` on the nRF54L15 **left the board unbootable**, and the only
//! thing that recovered it was `west flash --erase` through Nordic's own
//! `nrfutil` runner. That finding was handled by avoiding chip erase, which
//! treated a symptom — probe-rs models this target as one flat NVM region
//! `0x0..0x180000` and its nRF54L sequence implements only
//! `debug_device_unlock`, so *nothing* in the target description knows this
//! part stores code in **RRAM** rather than in the NVMC flash every older nRF
//! uses. Erase/write granularity on RRAM is vendor-defined and the generic
//! path does not implement it.
//!
//! So the rule below is deliberately a **refusal**, not a preference: on a
//! chip family whose vendor semantics probe-rs does not implement, Core does
//! not flash with probe-rs at all. Failing loudly beats writing plausible
//! bytes into RRAM.
//!
//! # Why the tools are not bundled
//!
//! None of the three can ship inside Core, and the reason is licensing rather
//! than effort. SEGGER's J-Link software is proprietary and redistribution
//! requires an agreement with SEGGER; `nrfjprog` is Nordic-proprietary **and**
//! links SEGGER's `JLinkARM` library, so it inherits that restriction (Nordic
//! has also deprecated it in favour of nRF Util); and `nrfutil` is a
//! bootstrapping launcher that downloads its own command packages at runtime,
//! so a vendored copy would still need the network and would drift. They are
//! also per-OS native binaries version-coupled to probe firmware.
//!
//! Hence [`Backend::discover`]: look for the tool, and if it is absent say
//! exactly which one and how to install it. Same posture `embarch-api` already
//! takes toward `west_binary` — a configured path, never a vendored binary.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};

/// Overrides for each tool's location, for a machine that has one somewhere
/// [`Backend::discover`] does not look. Checked before `PATH`.
pub const JLINK_EXE_ENV: &str = "EMBARCH_JLINK_EXE";
pub const NRFUTIL_EXE_ENV: &str = "EMBARCH_NRFUTIL_EXE";
pub const NRFJPROG_EXE_ENV: &str = "EMBARCH_NRFJPROG_EXE";
/// Forces a backend by name (`probe-rs`, `jlink`, `nrfutil`, `nrfjprog`),
/// including forcing `probe-rs` back on for a family this module refuses it
/// for. An escape hatch for a bench this table is wrong about — it exists so
/// being wrong here costs a config line rather than a Core release.
pub const FLASH_BACKEND_ENV: &str = "EMBARCH_FLASH_BACKEND";

/// A program that can write an image to a target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Backend {
    /// probe-rs in-process, Core's original and still the default for every
    /// family not listed in [`requires_vendor_tool`].
    ProbeRs,
    /// SEGGER's J-Link Commander, driven by a script file.
    JLink { exe: PathBuf },
    /// Nordic's nRF Util (`nrfutil device program`).
    NrfUtil { exe: PathBuf },
    /// Nordic's legacy nRF Command Line Tools. Deprecated upstream; supported
    /// because a bench that already has it working should not be forced to
    /// migrate to close this bug.
    NrfJprog { exe: PathBuf },
}

impl Backend {
    pub fn name(&self) -> &'static str {
        match self {
            Backend::ProbeRs => "probe-rs",
            Backend::JLink { .. } => "jlink",
            Backend::NrfUtil { .. } => "nrfutil",
            Backend::NrfJprog { .. } => "nrfjprog",
        }
    }
}

/// Does this chip need a vendor tool, i.e. is probe-rs known-unsafe for it?
///
/// **Matched on the family prefix, not an exact model list, and that is the
/// conservative direction.** A new nRF54L part this table has never heard of
/// gets refused rather than flashed by a backend that does not model its RRAM;
/// the cost of a wrong refusal is an error message and an env var, and the
/// cost of a wrong permit is an unbootable board.
pub fn requires_vendor_tool(chip: &str) -> bool {
    let c = chip.to_ascii_lowercase();
    // nRF54L: code lives in RRAM. probe-rs declares one flat NVM region and no
    // RRAM-aware erase/write. This is the family the 2026-08-25 chip-erase
    // incident happened on.
    c.starts_with("nrf54l")
}

/// The tool this chip's vendor actually supports, in the order to try.
fn preferred_for(chip: &str) -> &'static [&'static str] {
    let c = chip.to_ascii_lowercase();
    if c.starts_with("nrf") {
        // `nrfutil` first because that is what the board's own `board.cmake`
        // includes first, and so what `west flash` would pick. `jlink` second
        // because it is a vendor loader keyed off `--device`, and on a Nordic
        // bench it is usually already installed.
        &["nrfutil", "jlink", "nrfjprog"]
    } else {
        &[]
    }
}

/// Well-known install locations, searched after `PATH`. Windows installers
/// for both vendors put their binaries somewhere not on `PATH` by default,
/// which is exactly the case on this suite's own bench.
fn extra_candidates(tool: &str) -> Vec<PathBuf> {
    let mut out = Vec::new();
    match tool {
        "jlink" => {
            // Newest version directory wins; SEGGER installs side by side and
            // leaves every older one in place.
            for root in ["C:/Program Files/SEGGER", "C:/Program Files (x86)/SEGGER"] {
                let Ok(entries) = std::fs::read_dir(root) else { continue };
                let mut versions: Vec<PathBuf> = entries
                    .filter_map(|e| e.ok())
                    .map(|e| e.path())
                    .filter(|p| p.file_name().is_some_and(|n| n.to_string_lossy().starts_with("JLink")))
                    .collect();
                versions.sort();
                for dir in versions.into_iter().rev() {
                    out.push(dir.join("JLink.exe"));
                    out.push(dir.join("JLinkExe"));
                }
            }
            out.push(PathBuf::from("/opt/SEGGER/JLink/JLinkExe"));
            out.push(PathBuf::from("/usr/bin/JLinkExe"));
        }
        "nrfutil" => {
            out.push(PathBuf::from("C:/Program Files/Nordic Semiconductor/nrf-util/nrfutil.exe"));
            out.push(PathBuf::from("/usr/bin/nrfutil"));
            out.push(PathBuf::from("/usr/local/bin/nrfutil"));
        }
        "nrfjprog" => {
            out.push(PathBuf::from(
                "C:/Program Files/Nordic Semiconductor/nrf-command-line-tools/bin/nrfjprog.exe",
            ));
            out.push(PathBuf::from("/usr/local/bin/nrfjprog"));
            out.push(PathBuf::from("/usr/bin/nrfjprog"));
        }
        _ => {}
    }
    out
}

/// Can *this* process actually execute that file?
///
/// **Found by wiring `embarch-umbrella doctor` to this module, 2026-08-27.**
/// A Windows `embarch-core.exe` launched from WSL2 gets WSL's own `PATH`
/// entries merged into its environment by the interop layer, and
/// `Path::is_file()` happily resolves `/usr/bin/nrfutil` through the WSL
/// filesystem redirector. So discovery picked a **Linux ELF binary a Windows
/// process cannot exec**, and would have failed at spawn time with something
/// unrecognisable.
///
/// It also made `doctor` lie in the specific way `doctor` exists to prevent:
/// run through interop it reported `nrfutil`, while the *service* — whose
/// environment is the system `PATH`, with no WSL entries — resolves `jlink`.
/// A check that reports a backend the thing it is checking would never choose
/// is worse than no check.
fn usable_here(p: &Path) -> bool {
    if !p.is_file() {
        return false;
    }
    let s = p.to_string_lossy().to_ascii_lowercase();
    if cfg!(target_os = "windows") {
        // Both spellings of the WSL redirector, plus any POSIX-rooted path
        // that reached us through it.
        if s.starts_with("\\\\wsl") || s.starts_with("//wsl") || s.starts_with('/') {
            return false;
        }
        return matches!(
            p.extension().and_then(|e| e.to_str()).map(|e| e.to_ascii_lowercase()).as_deref(),
            Some("exe" | "cmd" | "bat" | "com")
        );
    }
    // The mirror case: a `.exe` on the PATH of a Linux Core is a Windows
    // binary, reachable over /mnt/c and equally unexecutable.
    !s.ends_with(".exe")
}

fn env_override(tool: &str) -> Option<PathBuf> {
    let key = match tool {
        "jlink" => JLINK_EXE_ENV,
        "nrfutil" => NRFUTIL_EXE_ENV,
        "nrfjprog" => NRFJPROG_EXE_ENV,
        _ => return None,
    };
    std::env::var_os(key).map(PathBuf::from).filter(|p| usable_here(p))
}

/// `PATH` lookup without spawning anything — spawning a flashing tool merely
/// to find out whether it exists is not a harmless probe.
fn on_path(tool: &str) -> Option<PathBuf> {
    let exe_names: &[&str] = match tool {
        "jlink" => &["JLinkExe", "JLink.exe", "JLink"],
        "nrfutil" => &["nrfutil", "nrfutil.exe"],
        "nrfjprog" => &["nrfjprog", "nrfjprog.exe"],
        _ => return None,
    };
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        for name in exe_names {
            let candidate = dir.join(name);
            if usable_here(&candidate) {
                return Some(candidate);
            }
        }
    }
    None
}

fn locate(tool: &str) -> Option<PathBuf> {
    env_override(tool)
        .or_else(|| on_path(tool))
        .or_else(|| extra_candidates(tool).into_iter().find(|p| usable_here(p)))
}

fn build(tool: &str, exe: PathBuf) -> Option<Backend> {
    match tool {
        "jlink" => Some(Backend::JLink { exe }),
        "nrfutil" => Some(Backend::NrfUtil { exe }),
        "nrfjprog" => Some(Backend::NrfJprog { exe }),
        "probe-rs" => Some(Backend::ProbeRs),
        _ => None,
    }
}

/// Picks the backend for `chip`, or explains what to install.
///
/// `EMBARCH_FLASH_BACKEND` wins outright, including forcing `probe-rs` back on
/// for a refused family.
pub fn discover(chip: &str) -> Result<Backend> {
    if let Ok(forced) = std::env::var(FLASH_BACKEND_ENV) {
        let forced = forced.trim().to_ascii_lowercase();
        if forced == "probe-rs" {
            tracing::warn!(
                "{FLASH_BACKEND_ENV}=probe-rs forces probe-rs for '{chip}'{}",
                if requires_vendor_tool(chip) {
                    " — a family Core otherwise refuses it for, because probe-rs does not \
                     model this part's RRAM erase/write semantics. A chip erase on this \
                     family has left a board unbootable before."
                } else {
                    ""
                }
            );
            return Ok(Backend::ProbeRs);
        }
        let Some(exe) = locate(&forced) else {
            bail!("{FLASH_BACKEND_ENV}='{forced}' but no such tool was found — {}", install_hint(&forced));
        };
        return build(&forced, exe)
            .with_context(|| format!("{FLASH_BACKEND_ENV}='{forced}' is not a known backend"));
    }

    if !requires_vendor_tool(chip) {
        return Ok(Backend::ProbeRs);
    }

    let wanted = preferred_for(chip);
    for tool in wanted {
        if let Some(exe) = locate(tool) {
            return build(tool, exe).context("internal: unknown preferred backend");
        }
    }

    bail!(
        "refusing to flash '{chip}' with probe-rs, and none of its vendor tools is installed \
         on the machine running embarch-core.\n\n\
         Why the refusal: this part stores code in RRAM, not the NVMC flash older nRF devices \
         use. probe-rs declares it as one flat NVM region with no RRAM-aware erase/write, and a \
         chip erase through that path has already left this exact board unbootable \
         (2026-08-25) — only Nordic's own runner recovered it.\n\n\
         Install ONE of these, on the embarch-core machine (not the build machine):\n  \
         - nRF Util  — {}\n  \
         - SEGGER J-Link — {}\n  \
         - nrfjprog (deprecated) — {}\n\n\
         Already installed somewhere unusual? Point Core at it with {JLINK_EXE_ENV} / \
         {NRFUTIL_EXE_ENV} / {NRFJPROG_EXE_ENV}. To override the choice entirely, set \
         {FLASH_BACKEND_ENV}.",
        install_hint("nrfutil"),
        install_hint("jlink"),
        install_hint("nrfjprog"),
    )
}

fn install_hint(tool: &str) -> String {
    match tool {
        "jlink" => "https://www.segger.com/downloads/jlink/ (J-Link Software and Documentation Pack)".into(),
        "nrfutil" => "https://www.nordicsemi.com/Products/Development-tools/nRF-Util, then `nrfutil install device`".into(),
        "nrfjprog" => "part of nRF Command Line Tools, https://www.nordicsemi.com/Products/Development-tools/nrf-command-line-tools".into(),
        other => format!("unknown tool '{other}'"),
    }
}

/// Runs an external backend. `erase` maps to each tool's own
/// erase-what-the-image-touches mode — deliberately never a full chip erase,
/// for the reason in this module's header.
pub fn run(
    backend: &Backend,
    chip: &str,
    firmware_path: &Path,
    format: &str,
    base_address: Option<u64>,
    probe_serial: Option<&str>,
    erase: bool,
) -> Result<()> {
    // **Every vendor tool infers the image format from the file extension,
    // and Core's uploaded artifact has none.** `POST /flash` streams the
    // firmware into a `tempfile` named `embarch-core-flash-XXXXXX`, which is
    // fine for probe-rs (told the format explicitly) and fatal for the others:
    // J-Link answers `File is of unknown / unsupported format` *after* having
    // already erased the chip, which is the worst possible moment to fail.
    // Found on the first real vendor flash, 2026-08-27.
    //
    // The extension cannot be fixed where the temp file is created, because
    // multipart fields arrive in stream order and `firmware` may precede
    // `format`. So it is staged here, where both are known.
    let staged = stage_with_extension(firmware_path, format)?;
    let firmware_path = staged.as_deref().unwrap_or(firmware_path);

    let mut cmd = match backend {
        Backend::ProbeRs => bail!("internal: probe-rs is flashed in-process, not spawned"),
        Backend::NrfUtil { exe } => {
            let mut c = Command::new(exe);
            c.args(["device", "program", "--firmware"]).arg(firmware_path);
            // ERASE_RANGES_TOUCHED_BY_FIRMWARE is nRF Util's own name for what
            // `--erase` should mean; ERASE_NONE still programs the covered
            // range, it just leaves everything else alone.
            c.arg("--options").arg(if erase {
                "chip_erase_mode=ERASE_RANGES_TOUCHED_BY_FIRMWARE"
            } else {
                "chip_erase_mode=ERASE_NONE"
            });
            if let Some(sn) = probe_serial {
                c.arg("--serial-number").arg(sn);
            }
            c
        }
        Backend::NrfJprog { exe } => {
            let mut c = Command::new(exe);
            c.arg("--program").arg(firmware_path);
            c.arg(if erase { "--sectorerase" } else { "--sectoranduicrerase" });
            if let Some(sn) = probe_serial {
                c.arg("--snr").arg(sn);
            }
            c
        }
        Backend::JLink { exe } => {
            // J-Link Commander takes a script rather than flags for the
            // load/verify/reset sequence. `-ExitOnError 1` matters: without it
            // Commander reports a failed load and still exits 0.
            let script = jlink_script(firmware_path, format, base_address, erase)?;
            let script_path = std::env::temp_dir().join(format!(
                "embarch-jlink-{}.jlink",
                std::process::id()
            ));
            std::fs::write(&script_path, script)
                .with_context(|| format!("writing J-Link script to {}", script_path.display()))?;
            let mut c = Command::new(exe);
            c.args(["-device", &jlink_device(chip), "-if", "SWD", "-speed", "4000"]);
            c.args(["-autoconnect", "1", "-NoGui", "1", "-ExitOnError", "1"]);
            if let Some(sn) = probe_serial {
                c.args(["-SelectEmuBySN", sn]);
            }
            c.arg("-CommanderScript").arg(&script_path);
            c
        }
    };

    let label = backend.name();
    let out = cmd
        .output()
        .with_context(|| format!("failed to run the '{label}' flash backend"))?;

    if !out.status.success() {
        // Both vendor tools put the useful half on stdout, not stderr.
        bail!(
            "'{label}' failed to flash '{chip}' ({}).\n--- stdout ---\n{}\n--- stderr ---\n{}",
            out.status,
            String::from_utf8_lossy(&out.stdout).trim(),
            String::from_utf8_lossy(&out.stderr).trim(),
        );
    }
    tracing::info!("flashed '{chip}' with {label}");
    Ok(())
}

/// SEGGER names Nordic's dual-core parts by core, and gets the RRAM loader
/// from that name — so this mapping is what makes the J-Link backend correct
/// rather than merely present. `board.cmake` for `dut_dev` already pins the
/// same string.
fn jlink_device(chip: &str) -> String {
    let c = chip.to_ascii_lowercase();
    if c.starts_with("nrf54l15") {
        "nRF54L15_M33".to_string()
    } else if c.starts_with("nrf54l10") {
        "nRF54L10_M33".to_string()
    } else if c.starts_with("nrf54l05") {
        "nRF54L05_M33".to_string()
    } else {
        chip.to_string()
    }
}

/// Gives the image a name the vendor tool can recognise, when it does not
/// already have one. Returns `None` when the path is already fine, so the
/// common case copies nothing.
fn stage_with_extension(firmware_path: &Path, format: &str) -> Result<Option<tempfile::TempPath>> {
    let want = match format.trim().to_ascii_lowercase().as_str() {
        "hex" | "ihex" => "hex",
        "bin" | "binary" => "bin",
        "elf" => "elf",
        other => bail!("don't know what file extension a '{other}' image should have for a vendor flashing tool"),
    };
    let have = firmware_path.extension().and_then(|e| e.to_str()).unwrap_or_default().to_ascii_lowercase();
    if have == want || (want == "hex" && have == "ihex") {
        return Ok(None);
    }
    let temp = tempfile::Builder::new()
        .prefix("embarch-flash-")
        .suffix(&format!(".{want}"))
        .tempfile()
        .context("creating a staging file for the vendor flashing tool")?;
    // **`into_temp_path()` rather than keeping the `NamedTempFile`, and this
    // is load-bearing on Windows**: a `NamedTempFile` holds the file open, and
    // J-Link then answers `Failed to open file` — after erasing the chip.
    // `TempPath` closes the handle and keeps both the path and the
    // delete-on-drop. Found on the second real vendor flash, 2026-08-27.
    let path = temp.into_temp_path();
    std::fs::copy(firmware_path, &path).with_context(|| {
        format!("staging {} as .{want} for the vendor flashing tool", firmware_path.display())
    })?;
    Ok(Some(path))
}

fn jlink_script(
    firmware_path: &Path,
    format: &str,
    base_address: Option<u64>,
    erase: bool,
) -> Result<String> {
    let mut s = String::from("si SWD\nspeed 4000\nr\n");
    if erase {
        s.push_str("erase\n");
    }
    match format.trim().to_ascii_lowercase().as_str() {
        "bin" | "binary" => {
            // A raw image carries no addresses, so J-Link needs one. Refusing
            // beats defaulting to 0 and writing an image to the wrong place.
            let Some(addr) = base_address else {
                bail!(
                    "a 'bin' image needs a base address to flash with J-Link, and none was given \
                     — pass base_address, or use a hex artifact"
                );
            };
            s.push_str(&format!("loadbin \"{}\",0x{addr:X}\n", firmware_path.display()));
        }
        _ => {
            // `loadfile` picks the format from the extension and honours the
            // hex's own addresses, which is what makes this equivalent to
            // `west flash`.
            s.push_str(&format!("loadfile \"{}\"\n", firmware_path.display()));
        }
    }
    s.push_str("r\ng\nqc\n");
    Ok(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nrf54l_requires_a_vendor_tool_and_other_families_do_not() {
        assert!(requires_vendor_tool("nRF54L15"));
        assert!(requires_vendor_tool("nrf54l15_cpuapp"));
        assert!(requires_vendor_tool("nRF54L10"));
        // The families probe-rs has always handled here stay on probe-rs —
        // this change is a refusal for one family, not a rewrite of flashing.
        assert!(!requires_vendor_tool("nRF52840"));
        assert!(!requires_vendor_tool("esp32c5"));
        assert!(!requires_vendor_tool("nRF5340_xxAA_APP"));
    }

    /// The prefix match is deliberately open-ended: a part this table has
    /// never seen is refused rather than flashed by a backend that does not
    /// model its RRAM.
    #[test]
    fn an_unknown_nrf54l_part_is_still_refused() {
        assert!(requires_vendor_tool("nRF54L47"));
    }

    #[test]
    fn a_non_vendor_family_discovers_probe_rs_without_any_tool_installed() {
        // No env overrides in play for this chip, and no vendor tool needed.
        assert_eq!(discover("esp32c5").unwrap(), Backend::ProbeRs);
    }

    #[test]
    fn jlink_device_names_carry_the_core_suffix_segger_expects() {
        assert_eq!(jlink_device("nRF54L15"), "nRF54L15_M33");
        assert_eq!(jlink_device("nrf54l15_cpuapp"), "nRF54L15_M33");
        // Unmapped parts pass through rather than being guessed at.
        assert_eq!(jlink_device("nRF52840_xxAA"), "nRF52840_xxAA");
    }

    /// `erase` must never become a chip erase in any backend — that is the
    /// specific operation that left a board unbootable.
    #[test]
    fn no_backend_maps_erase_to_a_full_chip_erase() {
        let script = jlink_script(Path::new("/tmp/x.hex"), "hex", None, true).unwrap();
        assert!(script.contains("erase\n"));
        assert!(!script.contains("erase_chip"));
        // And with erase off, nothing erases at all.
        let no_erase = jlink_script(Path::new("/tmp/x.hex"), "hex", None, false).unwrap();
        assert!(!no_erase.contains("\nerase\n"));
    }

    /// A Windows Core launched from WSL2 sees WSL's PATH; a Linux binary
    /// found there must never be selected, because it cannot be exec'd.
    #[test]
    fn a_binary_this_os_cannot_execute_is_never_usable() {
        if cfg!(target_os = "windows") {
            assert!(!usable_here(Path::new("/usr/bin/nrfutil")));
            assert!(!usable_here(Path::new("\\\\wsl.localhost\\Ubuntu\\usr\\bin\\nrfutil")));
        } else {
            // The mirror case, reachable over /mnt/c from a Linux Core.
            assert!(!usable_here(Path::new("/mnt/c/Program Files/SEGGER/JLink.exe")));
        }
        // A path that does not exist is not usable regardless of shape.
        assert!(!usable_here(Path::new("/definitely/not/here/nrfutil")));
    }

    #[test]
    fn the_jlink_script_loads_and_runs() {
        let script = jlink_script(Path::new("/tmp/fw.hex"), "hex", None, false).unwrap();
        assert!(script.contains("loadfile \"/tmp/fw.hex\""));
        assert!(script.ends_with("r\ng\nqc\n"), "must reset and go, then quit: {script}");
    }

    /// The failure that cost an erased board: a vendor tool infers the format
    /// from the extension, and Core's uploaded artifact has none.
    #[test]
    fn an_extensionless_artifact_is_staged_with_one() {
        let src = tempfile::Builder::new().prefix("embarch-core-flash-").tempfile().unwrap();
        std::fs::write(src.path(), b":00000001FF\n").unwrap();
        let staged = stage_with_extension(src.path(), "hex").unwrap().expect("must stage");
        assert_eq!(staged.extension().unwrap(), "hex");
        assert_eq!(std::fs::read(&staged).unwrap(), b":00000001FF\n");
        // A path that is already right is left alone rather than copied.
        assert!(stage_with_extension(&staged, "hex").unwrap().is_none());
    }

    /// A raw image has no addresses of its own, so guessing one would write
    /// the firmware to the wrong place.
    #[test]
    fn a_bin_image_without_a_base_address_is_refused_rather_than_guessed() {
        assert!(jlink_script(Path::new("/tmp/x.bin"), "bin", None, false).is_err());
        let ok = jlink_script(Path::new("/tmp/x.bin"), "bin", Some(0x1000), false).unwrap();
        assert!(ok.contains("loadbin \"/tmp/x.bin\",0x1000"));
    }

    /// The refusal has to name the tools and how to get them — an error that
    /// only says "no backend" moves the problem rather than closing it.
    #[test]
    fn the_refusal_message_names_every_tool_and_its_override() {
        // Guard against a developer machine that happens to have one of these.
        if preferred_for("nRF54L15").iter().any(|t| locate(t).is_some()) {
            return;
        }
        let err = discover("nRF54L15").unwrap_err().to_string();
        assert!(err.contains("RRAM"), "must say why probe-rs is refused: {err}");
        assert!(err.contains("nRF Util"));
        assert!(err.contains("J-Link"));
        assert!(err.contains(FLASH_BACKEND_ENV));
        assert!(err.contains(JLINK_EXE_ENV));
    }
}
