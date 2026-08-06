//! Auto-detection of `embarch-dev-bench`'s serial port.
//!
//! Implements `embarch-dev-bench/design.md` §3 decision 12: dev-bench does not
//! own a USB descriptor of its own (the nRF54L15 SoC has no USB device
//! peripheral at all), so the port Core sees is enumerated by the DK's
//! on-board SEGGER J-Link chip. Detection is therefore "SEGGER's VID plus a
//! product-string/serial-number heuristic", not a custom VID/PID match.
//!
//! No hardware is opened here — this only reads USB descriptors already
//! enumerated by the OS. Actually opening the port and running the
//! `Hello`/`HelloAck` handshake belongs to the (not yet implemented)
//! `study.rs` bridge; this module just answers "which port is it?".
//!
//! Precedence mirrors `token_store::resolve_token`'s explicit-env-var-wins
//! shape: `EMBARCH_DEV_BENCH_PORT` short-circuits detection entirely, and the
//! remaining env vars narrow the heuristic rather than replace it.

use anyhow::{bail, Context, Result};
use serde::Serialize;
use serialport::{SerialPortInfo, SerialPortType};

/// SEGGER's USB vendor ID — every on-board J-Link (and every standalone one)
/// enumerates its VCOM interfaces under this VID.
pub const SEGGER_VID: u16 = 0x1366;

/// Default product-string needle, in `normalize`d form. Deliberately matches
/// both Linux's bare `J-Link` and Windows' `JLink CDC UART Port` friendly
/// name — see `normalize`.
pub const DEFAULT_PRODUCT_NEEDLE: &str = "jlink";

/// Bypass detection entirely and use this port name verbatim.
pub const ENV_PORT: &str = "EMBARCH_DEV_BENCH_PORT";
/// The specific J-Link serial number recorded once at setup — what
/// disambiguates dev-bench from a second SEGGER-probed board in the same lab
/// (`embarch-dev-bench/design.md` §3 decision 12).
pub const ENV_SERIAL: &str = "EMBARCH_DEV_BENCH_SERIAL";
/// Overrides `DEFAULT_PRODUCT_NEEDLE`. Set it to the empty string to drop the
/// product-string check and match on VID alone.
pub const ENV_PRODUCT: &str = "EMBARCH_DEV_BENCH_PRODUCT";
/// USB interface number, for the case one J-Link exposes more than one VCOM
/// (`embarch-dev-bench/design.md` §4's open item on exactly this).
pub const ENV_INTERFACE: &str = "EMBARCH_DEV_BENCH_INTERFACE";

/// One detected port, plus whatever USB identity the OS reported for it.
/// Every USB field is optional because `ENV_PORT` can name a port that isn't
/// USB-enumerable at all (a raw `/dev/ttyS0`, a virtual COM port).
#[derive(Debug, Clone, Serialize)]
pub struct DevBenchPort {
    pub port_name: String,
    /// `"env-override"` or `"segger-vid-match"` — which rule produced this.
    pub detected_by: &'static str,
    pub vendor_id: Option<u16>,
    pub product_id: Option<u16>,
    pub serial_number: Option<String>,
    pub product: Option<String>,
    pub interface: Option<u8>,
}

/// No port matched. Distinct from every other detection failure so callers can
/// treat "dev-bench isn't plugged in" (a normal, expected state) differently
/// from "the heuristic is ambiguous" (a configuration problem) — `api.rs` maps
/// this one to `404`.
#[derive(Debug)]
pub struct NotFound {
    /// Ports carrying SEGGER's VID before the serial/product/interface filters
    /// narrowed them — a non-zero count means a SEGGER probe *is* attached and
    /// the narrowing filters excluded it, which is a very different fix.
    pub segger_ports_seen: usize,
    pub total_ports_seen: usize,
}

impl std::fmt::Display for NotFound {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "no embarch-dev-bench serial port found ({} serial port(s) visible, {} with SEGGER's VID {:#06x})",
            self.total_ports_seen, self.segger_ports_seen, SEGGER_VID
        )?;
        if self.segger_ports_seen > 0 {
            write!(
                f,
                " — a SEGGER probe is attached but was excluded by {ENV_SERIAL}/{ENV_PRODUCT}/{ENV_INTERFACE}; relax or correct them"
            )?;
        } else {
            write!(
                f,
                " — check dev-bench's USB connection (and `usbipd attach`, if Core and the board are on different hosts)"
            )?;
        }
        Ok(())
    }
}

impl std::error::Error for NotFound {}

/// The narrowing rules applied on top of the VID match.
#[derive(Debug, Default, Clone)]
pub struct Filter {
    /// Compared against the port's USB serial number, `normalize`d on both
    /// sides so `760001234` and `760001234 ` (or a `J-Link`-prefixed form)
    /// still match.
    pub serial: Option<String>,
    /// Substring match against the port's product string, `normalize`d on both
    /// sides. A port reporting *no* product string is not excluded — an absent
    /// descriptor field is unknown, not disproof.
    pub product_needle: Option<String>,
    pub interface: Option<u8>,
}

impl Filter {
    /// Reads `ENV_SERIAL`/`ENV_PRODUCT`/`ENV_INTERFACE`. An unset
    /// `ENV_PRODUCT` means "use `DEFAULT_PRODUCT_NEEDLE`"; an explicitly empty
    /// one means "don't filter on the product string at all".
    pub fn from_env() -> Result<Self> {
        let product_needle = match std::env::var(ENV_PRODUCT) {
            Ok(v) if v.trim().is_empty() => None,
            Ok(v) => Some(normalize(&v)),
            Err(_) => Some(DEFAULT_PRODUCT_NEEDLE.to_string()),
        };

        let interface = match env_nonempty(ENV_INTERFACE) {
            Some(v) => Some(v.parse::<u8>().with_context(|| {
                format!("{ENV_INTERFACE} must be a USB interface number 0-255, got '{v}'")
            })?),
            None => None,
        };

        Ok(Self {
            serial: env_nonempty(ENV_SERIAL),
            product_needle,
            interface,
        })
    }
}

fn env_nonempty(key: &str) -> Option<String> {
    std::env::var(key)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

/// Lowercase, alphanumerics only. This is what lets one default needle cover
/// every spelling of the same probe across platforms: Linux reports `J-Link`,
/// Windows' friendly name is `JLink CDC UART Port`, and a standalone probe may
/// report `SEGGER J-Link`. All three normalize to something containing
/// `jlink`.
fn normalize(s: &str) -> String {
    s.chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .flat_map(|c| c.to_lowercase())
        .collect()
}

fn as_candidate(info: &SerialPortInfo) -> Option<DevBenchPort> {
    let SerialPortType::UsbPort(usb) = &info.port_type else {
        return None;
    };

    Some(DevBenchPort {
        port_name: info.port_name.clone(),
        detected_by: "segger-vid-match",
        vendor_id: Some(usb.vid),
        product_id: Some(usb.pid),
        serial_number: usb.serial_number.clone(),
        product: usb.product.clone(),
        interface: usb.interface,
    })
}

/// Applies the VID + serial/product/interface rules to an already-enumerated
/// port list. Split out from [`detect`] so the whole heuristic is unit-testable
/// with no hardware and no environment variables involved.
pub fn select(ports: &[SerialPortInfo], filter: &Filter) -> Result<DevBenchPort> {
    let mut candidates: Vec<DevBenchPort> = ports
        .iter()
        .filter_map(as_candidate)
        .filter(|c| c.vendor_id == Some(SEGGER_VID))
        .collect();
    let segger_ports_seen = candidates.len();

    if let Some(serial) = &filter.serial {
        let want = normalize(serial);
        candidates.retain(|c| c.serial_number.as_deref().map(normalize) == Some(want.clone()));
    }
    if let Some(needle) = &filter.product_needle {
        candidates.retain(|c| {
            c.product
                .as_deref()
                .is_none_or(|p| normalize(p).contains(needle))
        });
    }
    if let Some(interface) = filter.interface {
        candidates.retain(|c| c.interface == Some(interface));
    }

    // Lowest interface index first, then port name — so "pick the first VCOM"
    // below is deterministic rather than dependent on enumeration order.
    candidates.sort_by(|a, b| {
        a.interface
            .cmp(&b.interface)
            .then_with(|| a.port_name.cmp(&b.port_name))
    });

    if candidates.len() > 1 {
        // A Nordic DK's on-board J-Link commonly exposes more than one CDC
        // interface off a single USB connection, so VID+product alone can't
        // pick between them (embarch-dev-bench/design.md §4). Two interfaces
        // of the *same* probe (identical serial number) is the expected,
        // recoverable case: take the lowest interface index, which is the
        // primary VCOM — the one the DK's UART0 is bridged to. Two *different*
        // probes is genuinely ambiguous and refuses to guess.
        let one_probe = candidates
            .iter()
            .all(|c| c.serial_number == candidates[0].serial_number);
        let interfaces_known = candidates.iter().all(|c| c.interface.is_some());

        if !(one_probe && interfaces_known) {
            bail!(
                "ambiguous embarch-dev-bench detection — {} candidate ports match:\n{}\nset {ENV_SERIAL} to the intended J-Link's serial number, {ENV_INTERFACE} to its VCOM interface number, or {ENV_PORT} to the port name directly",
                candidates.len(),
                describe(&candidates)
            );
        }

        tracing::warn!(
            "{} VCOM interfaces on one J-Link ({:?}) match; using the lowest interface index ({}). Set {ENV_INTERFACE} to pick a different one.\n{}",
            candidates.len(),
            candidates[0].serial_number,
            candidates[0].port_name,
            describe(&candidates)
        );
    }

    if candidates.is_empty() {
        return Err(anyhow::Error::new(NotFound {
            segger_ports_seen,
            total_ports_seen: ports.len(),
        }));
    }

    Ok(candidates.remove(0))
}

fn describe(candidates: &[DevBenchPort]) -> String {
    candidates
        .iter()
        .map(|c| {
            format!(
                "  {} (pid {:#06x}, serial {:?}, product {:?}, interface {:?})",
                c.port_name,
                c.product_id.unwrap_or(0),
                c.serial_number,
                c.product,
                c.interface
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Finds dev-bench's port on this machine. `ENV_PORT` wins outright; otherwise
/// the SEGGER-VID heuristic runs against the OS's enumerated ports.
///
/// Blocking (`serialport::available_ports` is synchronous) — call it from
/// `spawn_blocking` on the async side, per `embarch-core/design.md` §3.7.
pub fn detect() -> Result<DevBenchPort> {
    let filter = Filter::from_env()?;
    let ports = serialport::available_ports().context("failed to enumerate serial ports")?;

    if let Some(name) = env_nonempty(ENV_PORT) {
        return Ok(explicit(&name, &ports));
    }

    select(&ports, &filter)
}

/// An explicitly-configured port is returned whether or not the OS currently
/// enumerates it — the operator said which port it is, so a missing port is
/// the subsequent open's error to report, not detection's. Enumerated USB
/// metadata is attached when available, purely for diagnostics.
fn explicit(name: &str, ports: &[SerialPortInfo]) -> DevBenchPort {
    let matched = ports
        .iter()
        .find(|p| p.port_name.eq_ignore_ascii_case(name))
        .and_then(as_candidate);

    match matched {
        Some(mut port) => {
            port.detected_by = "env-override";
            port
        }
        None => DevBenchPort {
            port_name: name.to_string(),
            detected_by: "env-override",
            vendor_id: None,
            product_id: None,
            serial_number: None,
            product: None,
            interface: None,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serialport::UsbPortInfo;

    fn usb(
        port_name: &str,
        vid: u16,
        product: Option<&str>,
        serial: Option<&str>,
        interface: Option<u8>,
    ) -> SerialPortInfo {
        SerialPortInfo {
            port_name: port_name.to_string(),
            port_type: SerialPortType::UsbPort(UsbPortInfo {
                vid,
                pid: 0x0105,
                serial_number: serial.map(str::to_string),
                manufacturer: Some("SEGGER".to_string()),
                product: product.map(str::to_string),
                interface,
            }),
        }
    }

    fn default_filter() -> Filter {
        Filter {
            serial: None,
            product_needle: Some(DEFAULT_PRODUCT_NEEDLE.to_string()),
            interface: None,
        }
    }

    #[test]
    fn picks_the_only_segger_port_among_noise() {
        let ports = vec![
            SerialPortInfo {
                port_name: "/dev/ttyS0".to_string(),
                port_type: SerialPortType::Unknown,
            },
            usb("/dev/ttyACM0", 0x0483, Some("STM32 STLink"), None, Some(2)),
            usb(
                "/dev/ttyACM1",
                SEGGER_VID,
                Some("J-Link"),
                Some("760001"),
                Some(0),
            ),
        ];

        let found = select(&ports, &default_filter()).unwrap();
        assert_eq!(found.port_name, "/dev/ttyACM1");
        assert_eq!(found.detected_by, "segger-vid-match");
        assert_eq!(found.vendor_id, Some(SEGGER_VID));
    }

    #[test]
    fn windows_friendly_name_matches_the_same_default_needle() {
        let ports = vec![usb(
            "COM4",
            SEGGER_VID,
            Some("JLink CDC UART Port"),
            Some("760001"),
            Some(0),
        )];

        assert_eq!(select(&ports, &default_filter()).unwrap().port_name, "COM4");
    }

    #[test]
    fn a_port_reporting_no_product_string_is_not_excluded() {
        let ports = vec![usb(
            "/dev/ttyACM0",
            SEGGER_VID,
            None,
            Some("760001"),
            Some(0),
        )];

        assert_eq!(
            select(&ports, &default_filter()).unwrap().port_name,
            "/dev/ttyACM0"
        );
    }

    #[test]
    fn absence_is_reported_as_not_found() {
        let ports = vec![usb(
            "/dev/ttyACM0",
            0x0483,
            Some("STM32 STLink"),
            None,
            Some(2),
        )];

        let err = select(&ports, &default_filter()).unwrap_err();
        let not_found = err.downcast_ref::<NotFound>().expect("NotFound");
        assert_eq!(not_found.segger_ports_seen, 0);
        assert_eq!(not_found.total_ports_seen, 1);
    }

    #[test]
    fn a_filtered_out_segger_port_is_still_counted_in_not_found() {
        let ports = vec![usb(
            "/dev/ttyACM0",
            SEGGER_VID,
            Some("J-Link"),
            Some("760001"),
            Some(0),
        )];
        let filter = Filter {
            serial: Some("999999".to_string()),
            ..default_filter()
        };

        let err = select(&ports, &filter).unwrap_err();
        assert_eq!(
            err.downcast_ref::<NotFound>()
                .expect("NotFound")
                .segger_ports_seen,
            1
        );
    }

    #[test]
    fn serial_number_disambiguates_two_probes() {
        let ports = vec![
            usb(
                "/dev/ttyACM0",
                SEGGER_VID,
                Some("J-Link"),
                Some("760001"),
                Some(0),
            ),
            usb(
                "/dev/ttyACM1",
                SEGGER_VID,
                Some("J-Link"),
                Some("760002"),
                Some(0),
            ),
        ];

        // Ambiguous without a serial number: two distinct probes, no basis to choose.
        let err = select(&ports, &default_filter()).unwrap_err();
        assert!(err.downcast_ref::<NotFound>().is_none());
        assert!(format!("{err}").contains("ambiguous"));

        let filter = Filter {
            serial: Some("760002".to_string()),
            ..default_filter()
        };
        assert_eq!(select(&ports, &filter).unwrap().port_name, "/dev/ttyACM1");
    }

    #[test]
    fn two_vcoms_on_one_probe_resolve_to_the_lowest_interface() {
        let ports = vec![
            usb(
                "/dev/ttyACM1",
                SEGGER_VID,
                Some("J-Link"),
                Some("760001"),
                Some(2),
            ),
            usb(
                "/dev/ttyACM0",
                SEGGER_VID,
                Some("J-Link"),
                Some("760001"),
                Some(0),
            ),
        ];

        assert_eq!(
            select(&ports, &default_filter()).unwrap().port_name,
            "/dev/ttyACM0"
        );

        let filter = Filter {
            interface: Some(2),
            ..default_filter()
        };
        assert_eq!(select(&ports, &filter).unwrap().port_name, "/dev/ttyACM1");
    }

    #[test]
    fn one_probe_with_unknown_interfaces_stays_ambiguous() {
        let ports = vec![
            usb(
                "/dev/ttyACM0",
                SEGGER_VID,
                Some("J-Link"),
                Some("760001"),
                None,
            ),
            usb(
                "/dev/ttyACM1",
                SEGGER_VID,
                Some("J-Link"),
                Some("760001"),
                None,
            ),
        ];

        let err = select(&ports, &default_filter()).unwrap_err();
        assert!(format!("{err}").contains("ambiguous"));
    }

    #[test]
    fn an_empty_product_needle_matches_on_vid_alone() {
        let ports = vec![usb(
            "/dev/ttyACM0",
            SEGGER_VID,
            Some("Some Other SEGGER Thing"),
            Some("760001"),
            Some(0),
        )];

        assert!(select(&ports, &default_filter()).is_err());
        let filter = Filter {
            product_needle: None,
            ..default_filter()
        };
        assert_eq!(select(&ports, &filter).unwrap().port_name, "/dev/ttyACM0");
    }

    #[test]
    fn explicit_override_wins_and_keeps_enumerated_metadata() {
        let ports = vec![usb(
            "/dev/ttyACM3",
            SEGGER_VID,
            Some("J-Link"),
            Some("760001"),
            Some(0),
        )];

        let found = explicit("/dev/ttyACM3", &ports);
        assert_eq!(found.detected_by, "env-override");
        assert_eq!(found.serial_number.as_deref(), Some("760001"));

        // Not enumerable (yet) is still honored, just without USB metadata.
        let missing = explicit("/dev/ttyUSB9", &ports);
        assert_eq!(missing.port_name, "/dev/ttyUSB9");
        assert_eq!(missing.vendor_id, None);
    }
}
