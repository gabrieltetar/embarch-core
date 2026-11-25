//! Finding and opening a dev bench's samples port (EMB-480, decision 126).
//!
//! The dev bench enumerates as one USB device with three CDC ACM ports: control,
//! samples and SMP (`embarch-dev-bench` decision 61), each carrying an interface
//! string. **The OS does not hand that string to Core**: `serialport`'s
//! `UsbPortInfo` has a VID, PID, serial, manufacturer, product and interface
//! *number*, and no interface string. So the samples port is found from what
//! Core does know, the control port's recorded identity (decision 125's
//! `POST /dev-bench/link`, `embarch-topology` decision 50), and the order the
//! firmware declares the ports in: control, then samples, then SMP, which is
//! also their interface-number order. A COM number is never part of it.
//!
//! **Nothing here is known to hold on a real bench yet.** That the OS reports
//! the interface numbers in the firmware's order, and which interface of a CDC
//! ACM function it reports, is owed to a bench run.

use axum::http::StatusCode;
use embarch_topology::hardware::UsbPortId;
use serialport::{SerialPortInfo, SerialPortType};
use std::io::Read;

use crate::study::{TapPortClaim, TapPorts};

/// The dev bench firmware's USB vendor ID (`embarch-dev-bench` decision 61: a
/// local-use ID, not a reserved one). A recorded control port under any other
/// VID is a J-Link UART or another device, whose neighbours are not samples
/// ports.
pub const DEV_BENCH_USB_VID: u16 = 0xEBAC;

/// How many ports after the control port in interface order the samples port
/// sits: the firmware declares control, samples, SMP, in that order.
const SAMPLES_AFTER_CONTROL: usize = 1;

/// How long one read of the samples port waits before the reader checks its
/// stop flag again.
pub const SAMPLES_READ_TIMEOUT_MS: u64 = 200;

/// A refusal: the study does not start, and the reason says what to fix.
#[derive(Debug, PartialEq, Eq)]
pub struct SamplesPortRefusal {
    pub status: StatusCode,
    pub reason: String,
}

fn refuse(status: StatusCode, reason: impl Into<String>) -> SamplesPortRefusal {
    SamplesPortRefusal { status, reason: reason.into() }
}

/// One enumerated USB serial port, reduced to what the match reads.
struct UsbSerial<'a> {
    name: &'a str,
    vid: u16,
    pid: u16,
    serial: Option<&'a str>,
    interface: Option<u8>,
}

fn usb_ports(ports: &[SerialPortInfo]) -> Vec<UsbSerial<'_>> {
    ports
        .iter()
        .filter_map(|p| match &p.port_type {
            SerialPortType::UsbPort(u) => Some(UsbSerial {
                name: &p.port_name,
                vid: u.vid,
                pid: u.pid,
                serial: u.serial_number.as_deref(),
                interface: u.interface,
            }),
            _ => None,
        })
        .collect()
}

/// The samples port of the dev bench whose control port is `control`, among
/// the enumerated `ports`. Pure, so the whole decision is testable with no
/// hardware.
///
/// Refuses, never guesses: an unrecorded identity, one with no serial or
/// interface to anchor on, a VID that is not the dev bench's, a bench that is
/// not enumerated and a control port with no port after it are each their own
/// reason.
pub fn find_samples_port(
    control: Option<&UsbPortId>,
    ports: &[SerialPortInfo],
) -> Result<String, SamplesPortRefusal> {
    let Some(control) = control else {
        return Err(refuse(
            StatusCode::CONFLICT,
            "this study has an analog source, and the dev bench's control port has no recorded USB \
             identity to find its samples port from (record it with POST /dev-bench/link control_port)",
        ));
    };
    if control.vid != DEV_BENCH_USB_VID {
        return Err(refuse(
            StatusCode::CONFLICT,
            format!(
                "this study has an analog source, and the dev bench's recorded control port ({control}) is not \
                 a dev bench USB device (vendor {DEV_BENCH_USB_VID:04x}), so it has no samples port"
            ),
        ));
    }
    let (Some(serial), Some(control_interface)) = (control.serial.as_deref(), control.interface) else {
        return Err(refuse(
            StatusCode::CONFLICT,
            format!(
                "this study has an analog source, and the dev bench's recorded control port ({control}) lacks \
                 a serial or an interface, which the samples port is found from (record both with \
                 POST /dev-bench/link control_port)"
            ),
        ));
    };

    let mut device: Vec<UsbSerial<'_>> = usb_ports(ports)
        .into_iter()
        .filter(|p| p.vid == control.vid && p.pid == control.pid && p.serial == Some(serial))
        .collect();
    if device.is_empty() {
        return Err(refuse(
            StatusCode::NOT_FOUND,
            format!("the dev bench ({control}) is not enumerated, so its samples port cannot be opened (is it plugged in?)"),
        ));
    }
    if device.iter().any(|p| p.interface.is_none()) {
        return Err(refuse(
            StatusCode::CONFLICT,
            format!("the OS reports no interface number for the dev bench's ports ({control}), so its samples port cannot be told from the others"),
        ));
    }
    device.sort_by_key(|p| p.interface);
    let Some(at) = device.iter().position(|p| p.interface == Some(control_interface)) else {
        return Err(refuse(
            StatusCode::NOT_FOUND,
            format!("the dev bench is enumerated, but not its control port ({control}), so its samples port cannot be placed"),
        ));
    };
    match device.get(at + SAMPLES_AFTER_CONTROL) {
        Some(port) => Ok(port.name.to_string()),
        None => Err(refuse(
            StatusCode::NOT_FOUND,
            format!("the dev bench ({control}) enumerates no port after its control port, so it has no samples port"),
        )),
    }
}

/// An open samples port as the reader sees it: bytes in, nothing out.
pub type SamplesStream = Box<dyn Read + Send>;

/// The samples port, open and claimed, on its way to the reader thread.
pub struct OpenedSamples {
    pub port_name: String,
    pub port: SamplesStream,
    /// Held until the reader has stopped and the port is closed, so
    /// `POST /bootload` and a second study see the port as taken.
    pub claim: TapPortClaim,
}

/// Finds the samples port among `ports` and opens it through `open`, refusing
/// the study when it is absent, when something already holds it, or when the
/// open fails. `open` is not called for a port that is not found.
pub fn open_samples_port(
    control: Option<&UsbPortId>,
    ports: &[SerialPortInfo],
    tap_ports: &TapPorts,
    open: &dyn Fn(&str) -> Result<SamplesStream, String>,
) -> Result<OpenedSamples, SamplesPortRefusal> {
    let port_name = find_samples_port(control, ports)?;
    if tap_ports.holds(&port_name) {
        return Err(refuse(
            StatusCode::CONFLICT,
            format!("the dev bench's samples port {port_name} is held by a running study"),
        ));
    }
    let port = open(&port_name).map_err(|e| {
        refuse(StatusCode::CONFLICT, format!("failed to open the dev bench's samples port {port_name}: {e}"))
    })?;
    let claim = tap_ports.claim(&port_name);
    Ok(OpenedSamples { port_name, port, claim })
}

/// The real thing: the enrolled bench's recorded identity against the ports
/// the OS lists now, opened exclusively. Blocking: call from `spawn_blocking`.
pub fn open_enrolled_samples_port(tap_ports: &TapPorts) -> Result<OpenedSamples, SamplesPortRefusal> {
    let control = embarch_topology::hardware::dev_bench_control_port()
        .map_err(|e| refuse(StatusCode::INTERNAL_SERVER_ERROR, format!("couldn't read the dev bench's recorded control port: {e:#}")))?;
    let ports = serialport::available_ports()
        .map_err(|e| refuse(StatusCode::INTERNAL_SERVER_ERROR, format!("failed to enumerate serial ports: {e:#}")))?;
    open_samples_port(control.as_ref(), &ports, tap_ports, &|name| {
        let port = serialport::new(name, crate::dev_bench_link::DEV_BENCH_BAUD)
            .timeout(std::time::Duration::from_millis(SAMPLES_READ_TIMEOUT_MS))
            .open()
            .map_err(|e| format!("{e:?}"))?;
        // Whatever the OS held from before this study belongs to no capture
        // (the same reason a signal tap clears its port); a failure is not
        // fatal, since the decoder refuses a prefix it cannot place.
        if let Err(e) = port.clear(serialport::ClearBuffer::Input) {
            tracing::warn!("could not clear buffered input on {name} before capturing samples: {e:?}");
        }
        Ok(Box::new(port) as Box<dyn Read + Send>)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn port(name: &str, vid: u16, serial: &str, interface: Option<u8>) -> SerialPortInfo {
        SerialPortInfo {
            port_name: name.to_string(),
            port_type: SerialPortType::UsbPort(serialport::UsbPortInfo {
                vid,
                pid: 0x0102,
                serial_number: Some(serial.to_string()),
                manufacturer: None,
                product: None,
                interface,
            }),
        }
    }

    fn bench(serial: &str) -> Vec<SerialPortInfo> {
        // Listed out of order on purpose: the name says nothing about the role.
        vec![
            port("COM9", DEV_BENCH_USB_VID, serial, Some(4)),
            port("COM7", DEV_BENCH_USB_VID, serial, Some(0)),
            port("COM8", DEV_BENCH_USB_VID, serial, Some(2)),
        ]
    }

    fn control(serial: Option<&str>, interface: Option<u8>) -> UsbPortId {
        UsbPortId { vid: DEV_BENCH_USB_VID, pid: 0x0102, serial: serial.map(str::to_string), interface }
    }

    #[test]
    fn the_samples_port_follows_the_control_port_in_interface_order() {
        let ports = bench("ABCD");
        assert_eq!(find_samples_port(Some(&control(Some("ABCD"), Some(0))), &ports).unwrap(), "COM8");
    }

    #[test]
    fn another_benchs_ports_and_other_devices_are_never_picked() {
        let mut ports = bench("ABCD");
        ports.extend(bench("WXYZ").into_iter().map(|mut p| {
            p.port_name = p.port_name.replace("COM", "OTHER");
            p
        }));
        ports.push(port("COM1", 0x1366, "ABCD", Some(1)));
        assert_eq!(find_samples_port(Some(&control(Some("WXYZ"), Some(0))), &ports).unwrap(), "OTHER8");
    }

    #[test]
    fn a_missing_or_unplaceable_samples_port_is_a_refusal_that_says_why() {
        let wanted = control(Some("ABCD"), Some(0));
        // Not plugged in.
        let r = find_samples_port(Some(&wanted), &[]).unwrap_err();
        assert_eq!(r.status, StatusCode::NOT_FOUND);
        assert!(r.reason.contains("not enumerated"), "{}", r.reason);
        // Only the control port enumerated.
        let r = find_samples_port(Some(&wanted), &bench("ABCD")[1..2]).unwrap_err();
        assert!(r.reason.contains("no samples port"), "{}", r.reason);
        // The control port is the last one: nothing follows it.
        let r = find_samples_port(Some(&control(Some("ABCD"), Some(4))), &bench("ABCD")).unwrap_err();
        assert!(r.reason.contains("no samples port"), "{}", r.reason);
        // Nothing recorded, no anchor, a J-Link's identity.
        assert_eq!(find_samples_port(None, &bench("ABCD")).unwrap_err().status, StatusCode::CONFLICT);
        assert!(find_samples_port(Some(&control(None, Some(0))), &bench("ABCD")).unwrap_err().reason.contains("lacks"));
        let jlink = UsbPortId { vid: 0x1366, pid: 0x1051, serial: Some("1".into()), interface: Some(0) };
        assert!(find_samples_port(Some(&jlink), &bench("ABCD")).unwrap_err().reason.contains("not a dev bench"));
        // The OS reporting no interface numbers.
        let blind = vec![port("COM7", DEV_BENCH_USB_VID, "ABCD", None), port("COM8", DEV_BENCH_USB_VID, "ABCD", None)];
        assert!(find_samples_port(Some(&wanted), &blind).unwrap_err().reason.contains("no interface number"));
    }

    #[test]
    fn a_port_that_is_not_found_is_never_opened_and_a_held_one_is_refused() {
        let tap_ports = TapPorts::default();
        let opened = std::cell::Cell::new(0);
        let open = |_: &str| -> Result<SamplesStream, String> {
            opened.set(opened.get() + 1);
            Ok(Box::new(std::io::empty()))
        };
        assert!(open_samples_port(Some(&control(Some("ABCD"), Some(0))), &[], &tap_ports, &open).is_err());
        assert_eq!(opened.get(), 0);

        let ports = bench("ABCD");
        let held = open_samples_port(Some(&control(Some("ABCD"), Some(0))), &ports, &tap_ports, &open).unwrap();
        assert_eq!((held.port_name.as_str(), opened.get()), ("COM8", 1));
        let again = open_samples_port(Some(&control(Some("ABCD"), Some(0))), &ports, &tap_ports, &open).err().unwrap();
        assert!(again.reason.contains("held by a running study"), "{}", again.reason);
        drop(held);
        assert!(!tap_ports.holds("COM8"), "the claim goes with the opened port");
    }
}
