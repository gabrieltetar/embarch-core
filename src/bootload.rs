//! `POST /bootload`: a signed image into a DUT's MCUboot serial-recovery
//! bootloader over its USB CDC ACM port — the second way firmware reaches a
//! DUT, beside a probe's `POST /flash` (decision 77;
//! `embarch-doc/bootload-proposal.md`).
//!
//! SMP itself is `embarch-smp`, which never opens a port. What lives here is
//! everything around it: which port, getting the DUT into its bootloader, and
//! saying afterwards what happened. Ports are found and opened through
//! [`Bench`], so the tests drive the whole flow against `embarch_smp::sim`
//! with no device attached.

use std::io::{Read, Write};
use std::time::{Duration, Instant};

use axum::http::StatusCode;
use embarch_smp::client::{DEFAULT_FIRST_CHUNK_TIMEOUT, DEFAULT_TIMEOUT};
use embarch_smp::{Client, Fragmentation, ImageInfo, UploadOptions};
use embarch_topology::hardware::{BootloadPorts, UsbPortId};
use serde::Serialize;

use crate::study::TapPorts;

type Failure = (StatusCode, String);

/// How long a read on the bootloader's port waits before reporting nothing.
/// Short, because `embarch-smp` keeps its own deadline and treats a
/// `TimedOut` read as "nothing yet".
const READ_TIMEOUT: Duration = Duration::from_millis(50);

/// A CDC ACM port ignores its line rate; `serialport` still wants one.
const CDC_ACM_BAUD: u32 = 115_200;

/// How long each phase may take. **Every default is a placeholder, not a
/// measurement**: no bootload has run on a real DUT yet, and these are
/// replaced by numbers timed on the first one (`bootload-proposal.md`,
/// Open).
#[derive(Debug, Clone, Copy)]
pub struct Timeouts {
    /// For the bootloader to enumerate after the entry command, and then to
    /// open — a port can be listed a moment before it will open.
    pub enter: Duration,
    /// For the first chunk's reply, which waits on the bootloader erasing
    /// the slot. `embarch-smp`'s default.
    pub first_chunk: Duration,
    /// For every later reply. `embarch-smp`'s default.
    pub chunk: Duration,
    /// For the application to enumerate again after the reset.
    pub app_return: Duration,
    /// Between enumeration checks.
    pub poll: Duration,
}

pub const PLACEHOLDER_TIMEOUTS: Timeouts = Timeouts {
    enter: Duration::from_secs(10),
    first_chunk: DEFAULT_FIRST_CHUNK_TIMEOUT,
    chunk: DEFAULT_TIMEOUT,
    app_return: Duration::from_secs(15),
    poll: Duration::from_millis(100),
};

/// What ends the entry command's line. The DUT's shell decides which it
/// reads, so the project declares it; `\n` when it says nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LineEnding {
    #[default]
    Lf,
    Cr,
    CrLf,
}

impl LineEnding {
    pub fn parse(s: &str) -> Result<Self, String> {
        match s.trim().to_ascii_lowercase().as_str() {
            "lf" | "\\n" => Ok(LineEnding::Lf),
            "cr" | "\\r" => Ok(LineEnding::Cr),
            "crlf" | "\\r\\n" => Ok(LineEnding::CrLf),
            other => Err(format!("invalid entry_line_ending '{other}' (expected lf, cr or crlf)")),
        }
    }

    fn bytes(self) -> &'static [u8] {
        match self {
            LineEnding::Lf => b"\n",
            LineEnding::Cr => b"\r",
            LineEnding::CrLf => b"\r\n",
        }
    }
}

/// One bootload, as the caller asked for it.
#[derive(Debug, Clone)]
pub struct Plan {
    pub image: Vec<u8>,
    /// `image` on the wire: which image slot pair, 0 on a single-image DUT.
    pub image_index: u32,
    /// Typed at the application's shell to reboot into serial recovery.
    /// `None` means the DUT must already be sitting in its bootloader.
    pub entry_command: Option<String>,
    pub line_ending: LineEnding,
    /// The declared buffer (`CONFIG_BOOT_SERIAL_MAX_RECEIVE_SIZE`), or the
    /// conservative default when none is declared (`embarch-smp` decision 7).
    pub fragmentation: Fragmentation,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum EnteredVia {
    AlreadyInBootloader,
    ShellCommand,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BootloadResult {
    pub bytes: usize,
    pub requests: usize,
    /// Start to finish, the wait for the application included.
    pub duration_ms: u64,
    /// The upload alone: the number to compare across buffer sizes.
    pub upload_ms: u64,
    pub entered_via: EnteredVia,
    pub bootloader_port: String,
    /// Whether the application's port enumerated again after the reset.
    /// **Reported, not asserted**: an image the bootloader took and the
    /// application then failed to start is a real outcome, not a transport
    /// error. `None` when no application identity is declared, so there was
    /// nothing to watch for.
    pub app_reappeared: Option<bool>,
}

/// A port, once open.
pub trait Link: Read + Write {}
impl<T: Read + Write + ?Sized> Link for T {}

/// Where ports come from: live enumeration and `serialport` in Core, a
/// simulated DUT in the tests.
pub trait Bench {
    /// The one live port matching `id`, by name. `Ok(None)` while it is not
    /// enumerated, which is normal mid-reset.
    fn find(&mut self, id: &UsbPortId) -> Result<Option<String>, Failure>;
    fn open(&mut self, port: &str) -> std::io::Result<Box<dyn Link>>;
}

/// The bench as it is: ports matched by `embarch-topology`'s enumeration,
/// opened with `serialport`.
pub struct UsbBench;

impl Bench for UsbBench {
    fn find(&mut self, id: &UsbPortId) -> Result<Option<String>, Failure> {
        match embarch_topology::hardware::bootload::find(id) {
            Ok(port) => Ok(port.map(|p| p.port_name)),
            Err(e) if e.downcast_ref::<embarch_topology::hardware::AmbiguousUsbPort>().is_some() => {
                Err((StatusCode::CONFLICT, format!("{e:#}")))
            }
            Err(e) => Err(crate::api::internal_err(e)),
        }
    }

    fn open(&mut self, port: &str) -> std::io::Result<Box<dyn Link>> {
        let mut conn = serialport::new(port, CDC_ACM_BAUD).timeout(READ_TIMEOUT).open()?;
        // A Zephyr CDC ACM can hold its output until the host asserts DTR,
        // which a terminal does on open and a bare `open` need not.
        conn.write_data_terminal_ready(true)?;
        Ok(Box::new(conn))
    }
}

/// Refuses anything that is not an MCUboot image **before** the device is
/// touched: serial recovery writes over the running application, so an
/// unsigned `zephyr.bin` accepted here would leave the DUT with nothing to
/// boot and no message saying why.
pub fn check_image(bytes: &[u8]) -> Result<ImageInfo, String> {
    let info = ImageInfo::parse(bytes).map_err(|e| {
        format!(
            "{e}. Bootload takes the signed image — `zephyr.signed.bin`, which imgtool writes \
             when the build has MCUboot — not `zephyr.bin`"
        )
    })?;
    if !info.has_hash() {
        return Err(
            "the MCUboot image carries no SHA TLV, and MCUboot validates every image against one \
             at boot; sign it with imgtool"
                .to_string(),
        );
    }
    Ok(info)
}

/// The whole flow, blocking: into the bootloader, upload, reset, and watch
/// for the application. `on_writing` runs once, just before the first chunk
/// goes out — from there on the application that was running is gone,
/// whatever the outcome.
pub fn run(
    bench: &mut impl Bench,
    ports: &BootloadPorts,
    plan: &Plan,
    taps: &TapPorts,
    t: &Timeouts,
    on_writing: impl FnOnce(),
) -> Result<BootloadResult, Failure> {
    let started = Instant::now();
    let boot_id = &ports.bootloader;

    // Every declared port a tap is reading is refused before anything is
    // written, and an ambiguous identity surfaces here rather than midway.
    let boot_now = bench.find(boot_id)?;
    let app_now = match &ports.app {
        Some(app) => bench.find(app)?,
        None => None,
    };
    for port in boot_now.iter().chain(&app_now) {
        refuse_if_tapped(taps, port)?;
    }

    let (entered_via, boot_port) = match boot_now {
        Some(port) => (EnteredVia::AlreadyInBootloader, port),
        None => {
            let Some(app) = &ports.app else {
                return Err(refused(format!(
                    "the bootloader ({boot_id}) is not enumerated, and no application port is \
                     declared to send an entry command to; put the DUT into its bootloader, or \
                     declare the application's identity with PUT /bootload/ports"
                )));
            };
            let Some(command) = &plan.entry_command else {
                return Err(refused(format!(
                    "the bootloader ({boot_id}) is not enumerated and no entry command was given; \
                     declare the project's `[bootload] entry_command`, or put the DUT into its \
                     bootloader first"
                )));
            };
            let Some(app_port) = app_now else {
                return Err(refused(format!(
                    "neither the application ({app}) nor the bootloader ({boot_id}) is enumerated; \
                     is the DUT plugged in?"
                )));
            };
            send_entry_command(bench, &app_port, command, plan.line_ending)?;
            let port = wait_for(bench, boot_id, t.enter, t.poll)?.ok_or_else(|| {
                device(format!(
                    "sent `{command}` to {app_port}, and the bootloader ({boot_id}) did not \
                     enumerate within {:?} (a placeholder, not yet measured); check that this is \
                     the DUT's command and that its firmware has MCUboot serial recovery",
                    t.enter
                ))
            })?;
            refuse_if_tapped(taps, &port)?;
            (EnteredVia::ShellCommand, port)
        }
    };

    let link = open_with_retry(bench, &boot_port, t.enter, t.poll)?;
    let mut client = Client::new(link)
        .with_fragmentation(plan.fragmentation)
        .map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))?;
    client.timeout = t.chunk;
    let options = UploadOptions {
        image: plan.image_index,
        first_timeout: t.first_chunk,
        timeout: t.chunk,
        ..UploadOptions::default()
    };

    tracing::info!(
        port = boot_port,
        bytes = plan.image.len(),
        entered_via = ?entered_via,
        "bootload: uploading"
    );
    on_writing();
    let upload_started = Instant::now();
    let total = plan.image.len() as u64;
    let uploaded = client.upload(&plan.image, &options, |off| {
        tracing::debug!(off, total, "bootload: chunk accepted");
    });
    let summary = match uploaded {
        Ok(summary) => summary,
        Err(e) => {
            return Err(device(format!(
                "upload to {boot_port} failed: {e}; the DUT is left in its bootloader and the next \
                 bootload starts over{}",
                console(&mut client)
            )))
        }
    };
    let upload_ms = upload_started.elapsed().as_millis() as u64;

    if let Err(e) = client.reset() {
        return Err(device(format!(
            "the image is written ({} bytes, {} requests) but the reset request failed: {e}{}",
            summary.bytes,
            summary.requests,
            console(&mut client)
        )));
    }
    let printed = client.take_serial_bytes();
    if !printed.is_empty() {
        tracing::info!(port = boot_port, "bootload: the bootloader printed: {}", String::from_utf8_lossy(&printed));
    }
    // Closed before waiting, so the port is free the moment the DUT is.
    drop(client);

    let app_reappeared = match &ports.app {
        Some(app) => Some(wait_for(bench, app, t.app_return, t.poll)?.is_some()),
        None => None,
    };

    let result = BootloadResult {
        bytes: summary.bytes,
        requests: summary.requests,
        duration_ms: started.elapsed().as_millis() as u64,
        upload_ms,
        entered_via,
        bootloader_port: boot_port,
        app_reappeared,
    };
    tracing::info!(?result, "bootload: done");
    Ok(result)
}

fn refused(msg: String) -> Failure {
    (StatusCode::CONFLICT, msg)
}

/// The DUT did not do what the flow needed of it.
fn device(msg: String) -> Failure {
    (StatusCode::BAD_GATEWAY, msg)
}

fn refuse_if_tapped(taps: &TapPorts, port: &str) -> Result<(), Failure> {
    if taps.holds(port) {
        return Err(refused(format!(
            "{port} is being read by a running study's signal tap; bootload once the study ends"
        )));
    }
    Ok(())
}

/// Whatever the bootloader printed outside SMP, for an error message: on a
/// console-sharing port it is often the only account of why it went quiet.
fn console<T: Read + Write>(client: &mut Client<T>) -> String {
    let bytes = client.take_serial_bytes();
    if bytes.is_empty() {
        return String::new();
    }
    let text = String::from_utf8_lossy(&bytes[..bytes.len().min(512)]);
    format!("; the bootloader printed: {}", text.trim())
}

fn send_entry_command(bench: &mut impl Bench, port: &str, command: &str, ending: LineEnding) -> Result<(), Failure> {
    let failed = |e: std::io::Error| device(format!("couldn't send the entry command to {port}: {e}"));
    let mut shell = bench.open(port).map_err(failed)?;
    let mut line = command.as_bytes().to_vec();
    line.extend_from_slice(ending.bytes());
    shell.write_all(&line).map_err(failed)?;
    shell.flush().map_err(failed)?;
    tracing::info!(port, command, "bootload: sent the entry command");
    Ok(())
}

/// Polls until `id` enumerates or `within` passes.
fn wait_for(bench: &mut impl Bench, id: &UsbPortId, within: Duration, poll: Duration) -> Result<Option<String>, Failure> {
    let deadline = Instant::now() + within;
    loop {
        if let Some(port) = bench.find(id)? {
            return Ok(Some(port));
        }
        if Instant::now() >= deadline {
            return Ok(None);
        }
        std::thread::sleep(poll);
    }
}

fn open_with_retry(bench: &mut impl Bench, port: &str, within: Duration, poll: Duration) -> Result<Box<dyn Link>, Failure> {
    let deadline = Instant::now() + within;
    loop {
        match bench.open(port) {
            Ok(link) => return Ok(link),
            Err(e) if Instant::now() >= deadline => {
                return Err(device(format!("couldn't open the bootloader's port {port}: {e}")))
            }
            Err(_) => std::thread::sleep(poll),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use embarch_smp::sim::{SimBootloader, SimConfig};
    use std::io;
    use std::sync::{Arc, Mutex};

    const APP: &str = "COM8";
    const BOOT: &str = "COM9";

    fn app_id() -> UsbPortId {
        "2fe3:0004".parse().unwrap()
    }

    fn boot_id() -> UsbPortId {
        "2fe3:000c".parse().unwrap()
    }

    fn ports() -> BootloadPorts {
        BootloadPorts { role: "dut".into(), app: Some(app_id()), bootloader: boot_id() }
    }

    /// A DUT on the bench: running its application or sitting in its
    /// bootloader, never both.
    struct Dut {
        sim: SimBootloader,
        in_bootloader: bool,
        app_alive: bool,
        /// What the application's shell received.
        shell: Vec<u8>,
        /// The line the shell reboots into its bootloader on.
        reboots_on: Vec<u8>,
        /// Whether the application starts after a reset.
        app_boots: bool,
        /// Enumeration checks that miss the bootloader after it was entered,
        /// as a real re-enumeration does.
        enumerates_after: usize,
        resets_seen: usize,
    }

    #[derive(Clone)]
    struct SimBench(Arc<Mutex<Dut>>);

    impl SimBench {
        fn new(in_bootloader: bool) -> Self {
            SimBench(Arc::new(Mutex::new(Dut {
                sim: SimBootloader::new(SimConfig::default()),
                in_bootloader,
                app_alive: !in_bootloader,
                shell: Vec::new(),
                reboots_on: b"mcuboot\n".to_vec(),
                app_boots: true,
                enumerates_after: if in_bootloader { 0 } else { 3 },
                resets_seen: 0,
            })))
        }

        fn dut(&self) -> std::sync::MutexGuard<'_, Dut> {
            self.0.lock().unwrap()
        }
    }

    impl Bench for SimBench {
        fn find(&mut self, id: &UsbPortId) -> Result<Option<String>, Failure> {
            let mut dut = self.dut();
            if dut.sim.resets() > dut.resets_seen {
                dut.resets_seen = dut.sim.resets();
                dut.in_bootloader = false;
                dut.app_alive = dut.app_boots;
            }
            if *id == boot_id() && dut.in_bootloader {
                if dut.enumerates_after > 0 {
                    dut.enumerates_after -= 1;
                    return Ok(None);
                }
                return Ok(Some(BOOT.into()));
            }
            if *id == app_id() && dut.app_alive {
                return Ok(Some(APP.into()));
            }
            Ok(None)
        }

        fn open(&mut self, port: &str) -> io::Result<Box<dyn Link>> {
            match port {
                APP => Ok(Box::new(Shell(self.0.clone()))),
                BOOT => Ok(Box::new(BootPort(self.0.clone()))),
                _ => Err(io::Error::new(io::ErrorKind::NotFound, "no such port")),
            }
        }
    }

    struct Shell(Arc<Mutex<Dut>>);

    impl Write for Shell {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            let mut dut = self.0.lock().unwrap();
            dut.shell.extend_from_slice(buf);
            if dut.shell.ends_with(&dut.reboots_on) {
                dut.in_bootloader = true;
                dut.app_alive = false;
            }
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl Read for Shell {
        fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
            Err(io::ErrorKind::TimedOut.into())
        }
    }

    struct BootPort(Arc<Mutex<Dut>>);

    impl Read for BootPort {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            self.0.lock().unwrap().sim.read(buf)
        }
    }

    impl Write for BootPort {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().sim.write(buf)
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    const QUICK: Timeouts = Timeouts {
        enter: Duration::from_millis(300),
        first_chunk: Duration::from_millis(300),
        chunk: Duration::from_millis(300),
        app_return: Duration::from_millis(300),
        poll: Duration::from_millis(1),
    };

    /// A minimal image `ImageInfo::parse` accepts: header, body, and a TLV
    /// area holding one SHA-256 entry (or an unknown type, `hashed: false`).
    fn image(body: usize, hashed: bool) -> Vec<u8> {
        let mut img = Vec::new();
        img.extend(embarch_smp::image::IMAGE_MAGIC.to_le_bytes());
        img.extend(0u32.to_le_bytes()); // load address
        img.extend(32u16.to_le_bytes()); // header size
        img.extend(0u16.to_le_bytes()); // protected TLV size
        img.extend((body as u32).to_le_bytes());
        img.extend(0u32.to_le_bytes()); // flags
        img.extend([1, 2]);
        img.extend(3u16.to_le_bytes());
        img.extend(4u32.to_le_bytes()); // build number
        img.extend(0u32.to_le_bytes()); // padding to 32
        img.extend((0..body).map(|i| (i * 7 % 251) as u8));
        img.extend(embarch_smp::image::TLV_INFO_MAGIC.to_le_bytes());
        img.extend((4u16 + 4 + 32).to_le_bytes());
        img.extend((if hashed { 0x10u16 } else { 0x7fu16 }).to_le_bytes());
        img.extend(32u16.to_le_bytes());
        img.extend([0xab; 32]);
        img
    }

    fn plan(command: Option<&str>) -> Plan {
        Plan {
            image: image(20_000, true),
            image_index: 0,
            entry_command: command.map(str::to_string),
            line_ending: LineEnding::Lf,
            fragmentation: Fragmentation::default(),
        }
    }

    #[test]
    fn a_dut_already_in_its_bootloader_is_sent_no_command() {
        let mut bench = SimBench::new(true);
        let plan = plan(Some("mcuboot"));
        let mut erased = 0;
        let result = run(&mut bench, &ports(), &plan, &TapPorts::default(), &QUICK, || erased += 1).unwrap();
        assert_eq!(result.entered_via, EnteredVia::AlreadyInBootloader);
        assert_eq!(result.bootloader_port, BOOT);
        assert_eq!(result.bytes, plan.image.len());
        assert_eq!(result.app_reappeared, Some(true));
        assert_eq!(erased, 1);
        let dut = bench.dut();
        assert!(dut.shell.is_empty(), "no command when none is needed");
        assert_eq!(dut.sim.image(), &plan.image[..]);
        assert_eq!(dut.sim.resets(), 1);
    }

    #[test]
    fn the_entry_command_gets_it_there_and_the_application_comes_back() {
        let mut bench = SimBench::new(false);
        let plan = plan(Some("mcuboot"));
        let result = run(&mut bench, &ports(), &plan, &TapPorts::default(), &QUICK, || {}).unwrap();
        assert_eq!(result.entered_via, EnteredVia::ShellCommand);
        assert_eq!(result.app_reappeared, Some(true));
        let dut = bench.dut();
        assert_eq!(dut.shell, b"mcuboot\n");
        assert_eq!(dut.sim.image(), &plan.image[..]);
    }

    #[test]
    fn the_declared_line_ending_is_what_goes_out() {
        let mut bench = SimBench::new(false);
        bench.dut().reboots_on = b"mcuboot\r\n".to_vec();
        let plan = Plan { line_ending: LineEnding::CrLf, ..plan(Some("mcuboot")) };
        run(&mut bench, &ports(), &plan, &TapPorts::default(), &QUICK, || {}).unwrap();
        assert_eq!(bench.dut().shell, b"mcuboot\r\n");
    }

    #[test]
    fn an_application_that_does_not_come_back_is_reported_not_raised() {
        let mut bench = SimBench::new(true);
        bench.dut().app_boots = false;
        let result = run(&mut bench, &ports(), &plan(None), &TapPorts::default(), &QUICK, || {}).unwrap();
        assert_eq!(result.app_reappeared, Some(false));
        assert_eq!(bench.dut().sim.resets(), 1, "the image still landed and was reset into");
    }

    #[test]
    fn with_no_application_declared_there_is_nothing_to_watch_for() {
        let mut bench = SimBench::new(true);
        let ports = BootloadPorts { app: None, ..ports() };
        let result = run(&mut bench, &ports, &plan(None), &TapPorts::default(), &QUICK, || {}).unwrap();
        assert_eq!(result.app_reappeared, None);
    }

    #[test]
    fn with_no_way_in_nothing_is_written() {
        // Not in the bootloader and no command given.
        let mut bench = SimBench::new(false);
        let mut erased = false;
        let (status, msg) = run(&mut bench, &ports(), &plan(None), &TapPorts::default(), &QUICK, || erased = true).unwrap_err();
        assert_eq!(status, StatusCode::CONFLICT);
        assert!(msg.contains("entry_command"), "{msg}");
        assert!(!erased);

        // A command, but no application port to type it at.
        let no_app = BootloadPorts { app: None, ..ports() };
        let (status, msg) = run(&mut bench, &no_app, &plan(Some("mcuboot")), &TapPorts::default(), &QUICK, || {}).unwrap_err();
        assert_eq!(status, StatusCode::CONFLICT);
        assert!(msg.contains("no application port"), "{msg}");

        // Neither port enumerated.
        bench.dut().app_alive = false;
        let (status, msg) = run(&mut bench, &ports(), &plan(Some("mcuboot")), &TapPorts::default(), &QUICK, || {}).unwrap_err();
        assert_eq!(status, StatusCode::CONFLICT);
        assert!(msg.contains("plugged in"), "{msg}");

        let dut = bench.dut();
        assert!(dut.shell.is_empty());
        assert!(dut.sim.seen().is_empty());
    }

    #[test]
    fn a_command_the_firmware_does_not_answer_times_out_and_names_it() {
        let mut bench = SimBench::new(false);
        let (status, msg) = run(&mut bench, &ports(), &plan(Some("reboot")), &TapPorts::default(), &QUICK, || {}).unwrap_err();
        assert_eq!(status, StatusCode::BAD_GATEWAY);
        assert!(msg.contains("`reboot`") && msg.contains("placeholder"), "{msg}");
        assert!(bench.dut().sim.seen().is_empty());
    }

    #[test]
    fn a_port_a_study_tap_is_reading_is_refused_before_anything_is_written() {
        for (in_bootloader, tapped) in [(true, BOOT), (false, APP), (true, "com9")] {
            let mut bench = SimBench::new(in_bootloader);
            let taps = TapPorts::default();
            let _claim = taps.claim(tapped);
            let (status, msg) = run(&mut bench, &ports(), &plan(Some("mcuboot")), &taps, &QUICK, || {}).unwrap_err();
            assert_eq!(status, StatusCode::CONFLICT);
            assert!(msg.to_lowercase().contains(&tapped.to_lowercase()) && msg.contains("tap"), "{msg}");
            let dut = bench.dut();
            assert!(dut.shell.is_empty() && dut.sim.seen().is_empty());
        }
        // The claim is released with the tap.
        let taps = TapPorts::default();
        drop(taps.claim(BOOT));
        assert!(!taps.holds(BOOT));
    }

    #[test]
    fn a_declared_buffer_is_fewer_requests() {
        let count = |fragmentation| {
            let mut bench = SimBench::new(true);
            let plan = Plan { fragmentation, ..plan(None) };
            run(&mut bench, &ports(), &plan, &TapPorts::default(), &QUICK, || {}).unwrap().requests
        };
        let (default, declared) = (count(Fragmentation::default()), count(Fragmentation::buffer_size(1024)));
        assert!(declared * 5 < default, "declared 1024: {declared}, default: {default}");
    }

    #[test]
    fn a_buffer_declared_larger_than_the_bootloaders_fails_on_the_first_chunk() {
        let mut bench = SimBench::new(true);
        let plan = Plan { fragmentation: Fragmentation::buffer_size(4096), ..plan(None) };
        let (status, msg) = run(&mut bench, &ports(), &plan, &TapPorts::default(), &QUICK, || {}).unwrap_err();
        assert_eq!(status, StatusCode::BAD_GATEWAY);
        assert!(msg.contains("upload to COM9 failed"), "{msg}");
        assert_eq!(bench.dut().sim.resets(), 0);
    }

    #[test]
    fn only_a_hashed_mcuboot_image_passes_the_check() {
        assert!(check_image(&image(100, true)).is_ok());
        let unsigned = check_image(&[0u8; 4096]).unwrap_err();
        assert!(unsigned.contains("zephyr.signed.bin"), "{unsigned}");
        let unhashed = check_image(&image(100, false)).unwrap_err();
        assert!(unhashed.contains("SHA"), "{unhashed}");
        let truncated = image(100, true);
        assert!(check_image(&truncated[..truncated.len() - 1]).is_err());
    }

    #[test]
    fn line_endings_parse() {
        assert_eq!(LineEnding::parse("LF").unwrap(), LineEnding::Lf);
        assert_eq!(LineEnding::parse("cr").unwrap(), LineEnding::Cr);
        assert_eq!(LineEnding::parse(" crlf ").unwrap(), LineEnding::CrLf);
        assert!(LineEnding::parse("nl").is_err());
    }

    #[test]
    fn the_result_serializes_with_kebab_case_entry() {
        let json = serde_json::to_value(EnteredVia::AlreadyInBootloader).unwrap();
        assert_eq!(json, "already-in-bootloader");
        assert_eq!(serde_json::to_value(EnteredVia::ShellCommand).unwrap(), "shell-command");
    }
}
