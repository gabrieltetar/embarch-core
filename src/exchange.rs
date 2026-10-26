//! `POST /signals/{name}/exchange` — write text to a declared signal and read
//! what comes back (decision 79).
//!
//! The DUT's console is a declared signal like the outpost's trace, but one
//! Core may write: `embarch-topology` decision 37 makes a `Direct` route
//! declared `host-to-dut` or `bidirectional` writable
//! ([`SignalLink::host_can_write`]). An exchange is one request and its
//! reply — a shell command and its output — not a session: the port is
//! opened, written, read until a marker or a deadline, and closed, all
//! inside one `hw_lock` hold, so nothing else on the bench waits on an idle
//! console.
//!
//! **Bytes in, bytes out, nothing interpreted.** The reply is returned as
//! lossy UTF-8 with the DUT's own escape codes left in, the same posture as
//! dev-bench's pass-through (`embarch-outpost` decision 11): stripping
//! colour or parsing a prompt is the caller's business, and a caller that
//! wants the raw console still gets it.
//!
//! [`SignalLink::host_can_write`]: embarch_topology::hardware::SignalLink::host_can_write

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::io::{Read, Write};
use std::time::{Duration, Instant};

/// Ceiling on one exchange's `timeout_ms`: [`crate::serial::MAX_DURATION_MS`],
/// for the same reason — under the shared client's own 15 s request timeout,
/// so Core never holds `hw_lock` for a reply nobody is still waiting for.
pub const MAX_TIMEOUT_MS: u64 = crate::serial::MAX_DURATION_MS;

/// The read poll: the port's per-call timeout while reading.
pub(crate) const READ_POLL: Duration = Duration::from_millis(50);

/// `timeout_ms` when the caller gives none: long enough for a shell command
/// on a slow console, short enough to keep `hw_lock` free.
pub const DEFAULT_TIMEOUT_MS: u64 = 2_000;

/// Ceiling on what one exchange writes. A shell line, not a file transfer:
/// a firmware image goes through `/flash` or `/bootload`.
pub const MAX_WRITE_BYTES: usize = 4096;

/// Ceiling on what one exchange reads: a console spamming one line forever
/// cannot grow the reply, or hold `hw_lock`, past this.
pub const MAX_READ_BYTES: usize = 256 * 1024;

const IDLE_SLEEP: Duration = Duration::from_millis(5);

/// After `until` matches mid-line, how long the port must be quiet before the read ends there
/// (decision 85): a prompt has nothing after it and returns at once; a verdict word follows a
/// marker such as `pd: ` within one UART frame burst.
pub const QUIET_GAP: Duration = Duration::from_millis(40);

#[derive(Debug, Clone, Deserialize)]
pub struct ExchangeRequest {
    /// Written as given: a shell command needs its own line ending.
    pub write: String,
    /// Stop reading as soon as this appears in the reply (a shell prompt, a
    /// known last line). Without it, the read runs to `timeout_ms`.
    #[serde(default)]
    pub until: Option<String>,
    #[serde(default = "default_timeout_ms")]
    pub timeout_ms: u64,
    /// Drop whatever the port already held before writing, so the reply is
    /// this command's and not the tail of an earlier log line. On by default.
    #[serde(default = "default_true")]
    pub discard_pending: bool,
    /// Look for `until` only after the echo of what was written. On by
    /// default: a Zephyr shell prints a fresh prompt the moment the port is
    /// opened (DTR), before the command's own output, and that prompt would
    /// otherwise end the read early. Off for a console that does not echo.
    #[serde(default = "default_true")]
    pub match_after_echo: bool,
    /// How long to wait for the signal's port to be enumerable before giving up, polling every
    /// [`PORT_POLL_MS`]. 0 (the default) answers at once. A USB CDC ACM console is gone while its
    /// device resets, so a caller that just flashed or reset the DUT waits here instead of
    /// sleeping and retrying (decision 79, amended).
    #[serde(default)]
    pub port_wait_ms: u64,
}

/// How often a port wait looks for the port again.
pub const PORT_POLL_MS: u64 = 250;

fn default_timeout_ms() -> u64 {
    DEFAULT_TIMEOUT_MS
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ExchangeResponse {
    pub signal: String,
    pub port: String,
    pub baud: u32,
    /// Everything read after the write, lossy UTF-8, escape codes included.
    pub text: String,
    /// Whether `until` was seen. `false` with no `until` given.
    pub until_seen: bool,
    /// The read hit [`MAX_READ_BYTES`] before `until` or the deadline.
    pub truncated: bool,
    pub elapsed_ms: u64,
    /// When the exchange began, UTC ms since the Unix epoch: the clock a capture's
    /// `started_utc_ms` is on, so a reply and a capture's lines merge exactly (decision 94).
    pub started_utc_ms: u64,
    /// How many bytes came back after the write (decision 85). `0` is a console that said nothing.
    pub bytes_read: usize,
    /// The echo of the written line (without its line ending) was seen in the reply: the
    /// console's shell is alive and reading. `false` with a non-empty reply is a console that
    /// talks but does not echo, or one that is not the shell.
    pub echo_seen: bool,
    /// `write_all` and `flush` both succeeded, so the line went out. Always `true` in an
    /// answer: a write that fails is an error, which says nothing was written.
    pub written: bool,
    /// The port disappeared mid-read (a command that drops a USB CDC ACM device, a cable pull).
    /// `text` holds what was read before the loss; the command did run.
    pub port_lost: bool,
    /// The port's USB identity, added when nothing came back at all, so a USB-UART bridge is
    /// told from the DUT's own USB device (decision 85).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub port_identity: Option<PortIdentity>,
    /// The DUT core's run state from `DHCSR`, added when nothing came back and the signal's
    /// role has an enrolled probe. Never halts the core (decision 85).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub core_state: Option<CoreState>,
}

/// What a USB serial port says it is.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PortIdentity {
    pub vendor_id: Option<u16>,
    pub product_id: Option<u16>,
    pub product: Option<String>,
    pub serial_number: Option<String>,
}

impl PortIdentity {
    pub fn of(port: &embarch_topology::hardware::DetectedPort) -> Self {
        PortIdentity {
            vendor_id: port.vendor_id,
            product_id: port.product_id,
            product: port.product.clone(),
            serial_number: port.serial_number.clone(),
        }
    }
}

/// The DUT core as `DHCSR` shows it (decision 85), or why that could not be read.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CoreState {
    /// The raw register, when it was read.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dhcsr: Option<u32>,
    pub s_halt: Option<bool>,
    pub s_sleep: Option<bool>,
    pub s_lockup: Option<bool>,
    pub s_reset_st: Option<bool>,
    /// An instruction retired between the two reads: the core is executing, not stuck.
    pub s_retire_st: Option<bool>,
    /// Why there is no register: the role has no probe, or the probe could not be used.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unavailable: Option<String>,
}

impl CoreState {
    pub fn unavailable(why: impl Into<String>) -> Self {
        CoreState {
            dhcsr: None,
            s_halt: None,
            s_sleep: None,
            s_lockup: None,
            s_reset_st: None,
            s_retire_st: None,
            unavailable: Some(why.into()),
        }
    }

    /// Decodes a `DHCSR` read (ARMv6-M/v7-M/v8-M all place these bits alike).
    pub fn from_dhcsr(v: u32) -> Self {
        let bit = |n: u32| Some(v & (1 << n) != 0);
        CoreState {
            dhcsr: Some(v),
            s_halt: bit(17),
            s_sleep: bit(18),
            s_lockup: bit(19),
            s_retire_st: bit(24),
            s_reset_st: bit(25),
            unavailable: None,
        }
    }
}

/// The run state of the core behind `role`: looks up the role's probe with `lookup`, then
/// reads `DHCSR` with `read`. Two seams so the no-probe and probe-absent branches are
/// testable with no probe (decision 85).
pub fn core_state_for(
    role: &str,
    lookup: impl FnOnce(&str) -> Result<Option<(String, String)>>,
    read: impl FnOnce(&str, &str) -> Result<u32>,
) -> CoreState {
    match lookup(role) {
        Err(e) => CoreState::unavailable(format!("enrollment could not be read: {e:#}")),
        Ok(None) => CoreState::unavailable(format!("nothing is enrolled under the role '{role}'")),
        Ok(Some((chip, probe))) if probe.is_empty() => {
            let _ = chip;
            CoreState::unavailable(format!("the role '{role}' has no enrolled probe"))
        }
        Ok(Some((chip, probe))) => match read(&chip, &probe) {
            Ok(v) => CoreState::from_dhcsr(v),
            Err(e) => CoreState::unavailable(format!("the probe could not read the core: {e:#}")),
        },
    }
}

/// What one exchange read back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Exchanged {
    pub bytes: Vec<u8>,
    pub until_seen: bool,
    pub truncated: bool,
    pub echo_seen: bool,
    pub port_lost: bool,
}

/// Checks a request before `hw_lock` is taken: a caller error is a `400`,
/// never a held lock.
pub fn check(req: &ExchangeRequest) -> Result<(), String> {
    if req.write.is_empty() {
        return Err("`write` is empty: an exchange needs something to send".to_string());
    }
    if req.write.len() > MAX_WRITE_BYTES {
        return Err(format!(
            "`write` is {} bytes; an exchange sends at most {MAX_WRITE_BYTES}",
            req.write.len()
        ));
    }
    if req.timeout_ms == 0 || req.timeout_ms > MAX_TIMEOUT_MS {
        return Err(format!(
            "timeout_ms={} is outside 1..={MAX_TIMEOUT_MS}",
            req.timeout_ms
        ));
    }
    if req.until.as_deref() == Some("") {
        return Err("`until` is empty: leave it out to read until the timeout".to_string());
    }
    if req.port_wait_ms > MAX_TIMEOUT_MS {
        return Err(format!("port_wait_ms={} is above {MAX_TIMEOUT_MS}", req.port_wait_ms));
    }
    Ok(())
}

/// The signal's port, waiting up to `wait_ms` for it to be enumerable: only a declared direct
/// port that is not there now is waited for; any other failure answers at once.
pub fn resolve_port_waiting(name: &str, wait_ms: u64) -> anyhow::Result<embarch_topology::hardware::DetectedPort> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(wait_ms);
    loop {
        match embarch_topology::hardware::resolve_signal_port(name) {
            Ok(port) => return Ok(port),
            Err(e)
                if e.downcast_ref::<embarch_topology::hardware::SignalMismatch>()
                    .is_some_and(|m| m.declared_port_serial.is_some())
                    && std::time::Instant::now() < deadline =>
            {
                std::thread::sleep(std::time::Duration::from_millis(PORT_POLL_MS));
            }
            Err(e) => return Err(e),
        }
    }
}

/// Writes `write`, then reads until `until` appears in what was read, the
/// deadline passes, or `max_bytes` is reached — over any `Read + Write`, so
/// every exit is testable with no port opened.
#[cfg(test)]
pub fn exchange<P: Read + Write>(
    port: &mut P,
    write: &[u8],
    until: Option<&str>,
    match_after_echo: bool,
    timeout: Duration,
    max_bytes: usize,
) -> Result<Exchanged> {
    exchange_with(port, write, until, match_after_echo, timeout, max_bytes, |_, _| {})
}

/// Writes all of `write` within `deadline`: a `TimedOut`, `WouldBlock`, `Interrupted` or
/// zero-length write is a throttled device, not a failure, and is retried from the byte
/// count the port reported. Past the deadline it fails with the count of bytes written.
fn write_with_deadline<P: Write>(
    port: &mut P,
    write: &[u8],
    deadline: Instant,
    mut set_timeout: impl FnMut(&mut P, Duration),
) -> Result<()> {
    use std::io::ErrorKind::{Interrupted, TimedOut, WouldBlock};
    let mut sent = 0;
    while sent < write.len() {
        // A blocking write must not outlast the deadline: hand the port what remains.
        set_timeout(port, deadline.saturating_duration_since(Instant::now()).max(Duration::from_millis(1)));
        let stalled = match port.write(&write[sent..]) {
            Ok(0) => true,
            Ok(n) => {
                sent += n;
                false
            }
            Err(e) if matches!(e.kind(), TimedOut | WouldBlock | Interrupted) => true,
            Err(e) => {
                return Err(e).with_context(|| {
                    if sent == 0 {
                        "error writing to the signal's port: nothing was written".to_string()
                    } else {
                        format!("error writing to the signal's port after {sent} of {} bytes", write.len())
                    }
                })
            }
        };
        if stalled {
            if Instant::now() >= deadline {
                anyhow::bail!(
                    "the signal's port took only {sent} of {} bytes before the write deadline{}",
                    write.len(),
                    if sent == 0 { " (nothing was written)" } else { "" }
                );
            }
            std::thread::sleep(IDLE_SLEEP);
        }
    }
    Ok(())
}

/// [`exchange`] with `set_timeout` called with the port's per-call timeout: what remains of
/// the write deadline before each write, then the short read poll. One `timeout` covers the
/// write and the read together, so an exchange ends within `timeout` plus one read poll.
pub(crate) fn exchange_with<P: Read + Write>(
    port: &mut P,
    write: &[u8],
    until: Option<&str>,
    match_after_echo: bool,
    timeout: Duration,
    max_bytes: usize,
    mut set_timeout: impl FnMut(&mut P, Duration),
) -> Result<Exchanged> {
    let deadline = Instant::now() + timeout;
    write_with_deadline(port, write, deadline, &mut set_timeout)?;
    port.flush().context("error flushing the signal's port (the line was written, so it may have gone out)")?;
    set_timeout(port, READ_POLL);
    let needle = until.map(str::as_bytes);
    // The written line without its line ending: what a shell echoes back.
    let echo = {
        let mut e = write;
        while let [rest @ .., b'\r' | b'\n'] = e {
            e = rest;
        }
        e
    };
    let mut search_from: Option<usize> = if match_after_echo && !echo.is_empty() { None } else { Some(0) };
    let mut buf = [0u8; 1024];
    let mut got: Vec<u8> = Vec::new();
    // Where the first `until` match ends, and when the last byte after it arrived.
    let mut matched_end: Option<usize> = None;
    let mut last_data = Instant::now();
    let done = |got: Vec<u8>, until_seen: bool, truncated: bool, port_lost: bool| {
        let echo_seen = !echo.is_empty() && find(&got, echo, 0).is_some();
        Ok(Exchanged { bytes: got, until_seen, truncated, echo_seen, port_lost })
    };
    // At least one read pass, even when the write used the whole budget.
    loop {
        match port.read(&mut buf) {
            Ok(0) => std::thread::sleep(IDLE_SLEEP),
            Ok(n) => {
                let take = n.min(max_bytes - got.len());
                got.extend_from_slice(&buf[..take]);
                last_data = Instant::now();
                if search_from.is_none() {
                    search_from = find(&got, echo, 0).map(|i| i + echo.len());
                }
                if let (Some(x), Some(start), None) = (needle, search_from, matched_end) {
                    matched_end = find(&got, x, start).map(|i| i + x.len());
                }
                if got.len() >= max_bytes {
                    return done(got, matched_end.is_some(), true, false);
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::TimedOut => {}
            // The port went away after the line was written: the command ran, and what was read
            // is the answer so far.
            Err(_) => return done(got.clone(), matched_end.is_some(), false, true),
        }
        // A marker found mid-line ends the read at the end of that line, or once the port
        // has been quiet for QUIET_GAP, whichever is first (decision 85).
        if let Some(end) = matched_end {
            if got[end..].iter().any(|&b| b == b'\n' || b == b'\r') || last_data.elapsed() >= QUIET_GAP {
                return done(got, true, false, false);
            }
        }
        if Instant::now() >= deadline {
            break;
        }
    }
    done(got, matched_end.is_some(), false, false)
}

/// The first index at or after `from` where `needle` starts in `hay`.
fn find(hay: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    if needle.is_empty() || from >= hay.len() {
        return None;
    }
    hay[from..].windows(needle.len()).position(|w| w == needle).map(|i| i + from)
}

/// Now, UTC milliseconds since the Unix epoch (decision 94).
pub fn utc_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

/// Opens `port`, asserts DTR (a Zephyr CDC ACM shell may wait for it before it prints),
/// optionally drops pending input, and runs one exchange. `core_state` is called only when
/// nothing came back, to add the DUT core's run state (decision 85).
pub fn run(
    signal: &str,
    port: &embarch_topology::hardware::DetectedPort,
    baud: u32,
    req: &ExchangeRequest,
    core_state: impl FnOnce() -> CoreState,
) -> Result<ExchangeResponse> {
    let started = Instant::now();
    let started_utc_ms = utc_ms();
    let port_name = port.port_name.as_str();
    let mut sp = serialport::new(port_name, baud)
        .timeout(READ_POLL)
        .open()
        .with_context(|| format!("failed to open signal '{signal}' on {port_name} at {baud} baud: nothing was written"))?;
    // Best effort: a plain UART bridge may not support the control line, and
    // that is no reason to refuse the write.
    let _ = sp.write_data_terminal_ready(true);
    if req.discard_pending {
        let _ = sp.clear(serialport::ClearBuffer::Input);
    }
    // serialport uses one timeout for both directions and a 50 ms write cut short loses bytes
    // on Windows, so the write gets what remains of `timeout_ms` (decision 79).
    let got = exchange_with(
        &mut sp,
        req.write.as_bytes(),
        req.until.as_deref(),
        req.match_after_echo,
        Duration::from_millis(req.timeout_ms),
        MAX_READ_BYTES,
        |p, d| {
            let _ = p.set_timeout(d);
        },
    )?;
    Ok(response(signal, port_name, baud, started.elapsed().as_millis() as u64, started_utc_ms, got, || Some(PortIdentity::of(port)), core_state))
}

/// Builds the answer to an exchange, shared by [`run`] (its own port) and a capture's
/// (the capture's open port, `capture.rs`). `identity` and `core_state` run only when nothing
/// came back (decision 85).
#[allow(clippy::too_many_arguments)]
pub(crate) fn response(
    signal: &str,
    port_name: &str,
    baud: u32,
    elapsed_ms: u64,
    started_utc_ms: u64,
    got: Exchanged,
    identity: impl FnOnce() -> Option<PortIdentity>,
    core_state: impl FnOnce() -> CoreState,
) -> ExchangeResponse {
    let silent = got.bytes.is_empty();
    ExchangeResponse {
        signal: signal.to_string(),
        port: port_name.to_string(),
        baud,
        text: String::from_utf8_lossy(&got.bytes).into_owned(),
        until_seen: got.until_seen,
        truncated: got.truncated,
        elapsed_ms,
        started_utc_ms,
        bytes_read: got.bytes.len(),
        echo_seen: got.echo_seen,
        written: true,
        port_lost: got.port_lost,
        port_identity: if silent { identity() } else { None },
        core_state: silent.then(core_state),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;

    /// A port that records what was written and replies from a script of
    /// read results, one per `read` call.
    struct Fake {
        written: Vec<u8>,
        reads: VecDeque<std::io::Result<Vec<u8>>>,
    }

    impl Fake {
        fn new(reads: Vec<std::io::Result<Vec<u8>>>) -> Self {
            Fake { written: Vec::new(), reads: reads.into() }
        }
    }

    impl Read for Fake {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            match self.reads.pop_front() {
                None => Err(std::io::ErrorKind::TimedOut.into()),
                Some(Err(e)) => Err(e),
                Some(Ok(b)) => {
                    buf[..b.len()].copy_from_slice(&b);
                    Ok(b.len())
                }
            }
        }
    }

    impl Write for Fake {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.written.extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// Takes at most `cap` bytes per call; every call stalls first (a `TimedOut`
    /// error, as a throttled CDC ACM device gives) unless `never` is false.
    struct Throttled {
        inner: Fake,
        cap: usize,
        stall_every_call: bool,
        accept: bool,
        stalled: bool,
    }

    impl Read for Throttled {
        fn read(&mut self, b: &mut [u8]) -> std::io::Result<usize> {
            self.inner.read(b)
        }
    }

    impl Write for Throttled {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            if self.stall_every_call && !self.stalled || !self.accept {
                self.stalled = true;
                std::thread::sleep(Duration::from_millis(60));
                return Err(std::io::ErrorKind::TimedOut.into());
            }
            self.stalled = false;
            let n = buf.len().min(self.cap);
            self.inner.written.extend_from_slice(&buf[..n]);
            Ok(n)
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn a_throttled_device_gets_the_whole_line_in_order() {
        let line: Vec<u8> = (0..357u32).map(|i| b'a' + (i % 26) as u8).collect();
        let mut p = Throttled { inner: Fake::new(vec![Ok(b"ok$ ".to_vec())]), cap: 16, stall_every_call: true, accept: true, stalled: false };
        let t = Instant::now();
        let got = exchange(&mut p, &line, Some("ok$ "), false, Duration::from_secs(30), 1024).unwrap();
        assert!(got.until_seen);
        assert_eq!(p.inner.written, line);
        assert!(t.elapsed() > Duration::from_millis(50));
    }

    #[test]
    fn a_device_that_never_takes_the_rest_fails_by_the_deadline_with_the_count() {
        let mut p = Throttled { inner: Fake::new(vec![]), cap: 4, stall_every_call: false, accept: true, stalled: false };
        p.accept = true;
        // Accept 4 bytes once, then stall forever.
        struct Once(Throttled, bool);
        impl Read for Once {
            fn read(&mut self, b: &mut [u8]) -> std::io::Result<usize> {
                self.0.read(b)
            }
        }
        impl Write for Once {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                if !self.1 {
                    self.1 = true;
                    return self.0.write(buf);
                }
                self.0.accept = false;
                self.0.write(buf)
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let mut o = Once(p, false);
        let t = Instant::now();
        let e = exchange(&mut o, b"0123456789", None, true, Duration::from_millis(150), 1024).unwrap_err();
        assert!(format!("{e:#}").contains("only 4 of 10 bytes"), "{e:#}");
        assert!(t.elapsed() >= Duration::from_millis(150) && t.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn a_line_taken_just_inside_the_deadline_leaves_the_read_what_remains_not_a_fresh_timeout() {
        // Stalls 60 ms per call, so the 4 bytes arrive ~300 ms in; no marker ever comes.
        let p = Throttled { inner: Fake::new(vec![]), cap: 1, stall_every_call: true, accept: true, stalled: false };
        let mut p = p;
        let timeout = Duration::from_millis(400);
        let t = Instant::now();
        let got = exchange(&mut p, b"abcd", Some("never"), false, timeout, 1024).unwrap();
        assert!(!got.until_seen);
        assert_eq!(p.inner.written, b"abcd");
        let took = t.elapsed();
        assert!(took >= timeout, "{took:?}");
        // The bound: timeout_ms plus one read poll, not twice timeout_ms.
        assert!(took < timeout + Duration::from_millis(250), "{took:?}");
    }

    fn req(write: &str) -> ExchangeRequest {
        ExchangeRequest {
            write: write.to_string(),
            until: None,
            timeout_ms: 100,
            discard_pending: true,
            match_after_echo: true,
            port_wait_ms: 0,
        }
    }

    #[test]
    fn writes_then_stops_at_the_marker_even_split_across_reads() {
        let mut p = Fake::new(vec![Ok(b"kernel uptime\r\nUptime: 42 ms\r\nuart:~".to_vec()), Ok(b"$ ".to_vec())]);
        let got = exchange(&mut p, b"kernel uptime\n", Some("uart:~$ "), true, Duration::from_secs(5), 1024).unwrap();
        assert_eq!(p.written, b"kernel uptime\n");
        assert!(got.until_seen && !got.truncated);
        assert!(String::from_utf8_lossy(&got.bytes).contains("Uptime: 42 ms"));
    }

    #[test]
    fn without_a_marker_it_reads_to_the_deadline() {
        let mut p = Fake::new(vec![Ok(b"line\r\n".to_vec())]);
        let t = Instant::now();
        let got = exchange(&mut p, b"x\n", None, true, Duration::from_millis(60), 1024).unwrap();
        assert!(t.elapsed() >= Duration::from_millis(60));
        assert_eq!(got.bytes, b"line\r\n");
        assert!(!got.until_seen && !got.truncated);
    }

    #[test]
    fn the_prompt_a_shell_prints_on_open_does_not_end_the_read() {
        // What a Zephyr CDC ACM shell sent on 2026-10-06: a prompt the
        // moment DTR rose, then the echo, the output, and the prompt again.
        let mut p = Fake::new(vec![
            Ok(b"\n\x1b[1;32muart:~$ \x1b[m".to_vec()),
            Ok(b"kernel uptime\r\nUptime: 17774 ms\r\n".to_vec()),
            Ok(b"\x1b[1;32muart:~$ \x1b[m".to_vec()),
        ]);
        let got = exchange(&mut p, b"kernel uptime\n", Some("uart:~$ "), true, Duration::from_secs(5), 1024).unwrap();
        assert!(got.until_seen);
        assert!(String::from_utf8_lossy(&got.bytes).contains("Uptime: 17774 ms"));
        // Without the echo rule the early prompt is the match; the read then runs on to the end of
        // that line (decision 85), so it still ends at the prompt's line, not at the real one.
        let mut q = Fake::new(vec![Ok(b"uart:~$ ".to_vec()), Ok(b"kernel uptime\r\nUptime: 1 ms\r\n".to_vec())]);
        let early = exchange(&mut q, b"kernel uptime\n", Some("uart:~$ "), false, Duration::from_secs(5), 1024).unwrap();
        assert!(early.until_seen && !String::from_utf8_lossy(&early.bytes).contains("Uptime: 1 ms\r\nuart"));
    }

    #[test]
    fn a_console_that_never_stops_is_cut_at_the_byte_cap() {
        let mut p = Fake::new((0..100).map(|_| Ok(vec![b'a'; 64])).collect());
        let got = exchange(&mut p, b"x\n", Some("never"), true, Duration::from_secs(5), 100).unwrap();
        assert_eq!(got.bytes.len(), 100);
        assert!(got.truncated && !got.until_seen);
    }

    #[test]
    fn zero_bytes_back_says_so_with_no_echo() {
        let mut p = Fake::new(vec![]);
        let got = exchange(&mut p, b"help\n", Some("$ "), true, Duration::from_millis(60), 100).unwrap();
        assert!(got.bytes.is_empty() && !got.echo_seen && !got.until_seen && !got.port_lost);
        assert_eq!(p.written, b"help\n");
    }

    #[test]
    fn echo_then_silence_is_a_live_shell_that_said_nothing_more() {
        let mut p = Fake::new(vec![Ok(b"help\r\n".to_vec())]);
        let got = exchange(&mut p, b"help\n", Some("$ "), true, Duration::from_millis(60), 100).unwrap();
        assert!(got.echo_seen && !got.until_seen && !got.port_lost);
    }

    #[test]
    fn a_read_error_after_the_write_keeps_the_partial_text_and_says_the_port_was_lost() {
        let mut p = Fake::new(vec![Ok(b"usb off\r\nbye\r\n".to_vec()), Err(std::io::ErrorKind::BrokenPipe.into())]);
        let got = exchange(&mut p, b"usb off\n", None, true, Duration::from_secs(1), 100).unwrap();
        assert!(got.port_lost && got.echo_seen);
        assert_eq!(got.bytes, b"usb off\r\nbye\r\n");
        assert_eq!(p.written, b"usb off\n");
        // Even with nothing read, the loss is reported rather than erred.
        let mut q = Fake::new(vec![Err(std::io::ErrorKind::BrokenPipe.into())]);
        let lost = exchange(&mut q, b"x\n", None, true, Duration::from_secs(1), 100).unwrap();
        assert!(lost.port_lost && lost.bytes.is_empty());
    }

    #[test]
    fn a_marker_cut_mid_line_reads_on_to_the_verdict() {
        let mut p = Fake::new(vec![Ok(b"pd sink 5\r\npd: o".to_vec()), Ok(b"k\r\n".to_vec())]);
        let got = exchange(&mut p, b"pd sink 5\n", Some("pd: "), true, Duration::from_secs(5), 1024).unwrap();
        assert!(got.until_seen);
        assert!(got.bytes.ends_with(b"pd: ok\r\n"), "{:?}", String::from_utf8_lossy(&got.bytes));
    }

    #[test]
    fn a_prompt_with_nothing_after_it_returns_after_the_quiet_gap_not_the_deadline() {
        let mut p = Fake::new(vec![Ok(b"x\r\nuart:~$ ".to_vec())]);
        let t = Instant::now();
        let got = exchange(&mut p, b"x\n", Some("uart:~$ "), true, Duration::from_secs(5), 1024).unwrap();
        assert!(got.until_seen);
        assert!(t.elapsed() >= QUIET_GAP && t.elapsed() < Duration::from_secs(1), "{:?}", t.elapsed());
    }

    #[test]
    fn dhcsr_bits_decode_and_the_unavailable_branches_say_why() {
        let c = CoreState::from_dhcsr((1 << 17) | (1 << 25));
        assert_eq!((c.s_halt, c.s_sleep, c.s_lockup, c.s_reset_st, c.s_retire_st), (Some(true), Some(false), Some(false), Some(true), Some(false)));
        let no_probe = core_state_for("dut", |_| Ok(Some(("stm32g071rb".into(), String::new()))), |_, _| panic!("no probe, no read"));
        assert!(no_probe.dhcsr.is_none() && no_probe.unavailable.unwrap().contains("no enrolled probe"));
        let none = core_state_for("dut", |_| Ok(None), |_, _| panic!("not enrolled, no read"));
        assert!(none.unavailable.unwrap().contains("nothing is enrolled"));
        let absent = core_state_for("dut", |_| Ok(Some(("c".into(), "SER".into()))), |_, _| anyhow::bail!("no probe attached"));
        assert!(absent.unavailable.unwrap().contains("no probe attached"));
        let ok = core_state_for("dut", |_| Ok(Some(("c".into(), "SER".into()))), |c, p| {
            assert_eq!((c, p), ("c", "SER"));
            Ok(1 << 18)
        });
        assert_eq!(ok.s_sleep, Some(true));
    }

    #[test]
    fn a_write_that_fails_is_an_error_that_says_nothing_was_written() {
        struct Dead;
        impl Read for Dead {
            fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
                Err(std::io::ErrorKind::BrokenPipe.into())
            }
        }
        impl Write for Dead {
            fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
                Err(std::io::ErrorKind::BrokenPipe.into())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let e = exchange(&mut Dead, b"x\n", None, true, Duration::from_secs(1), 100).unwrap_err();
        assert!(format!("{e:#}").contains("nothing was written"), "{e:#}");
    }

    #[test]
    fn requests_are_checked_before_any_lock() {
        assert!(check(&req("kernel uptime\n")).is_ok());
        assert!(check(&req("")).unwrap_err().contains("empty"));
        assert!(check(&ExchangeRequest { timeout_ms: MAX_TIMEOUT_MS + 1, ..req("x") }).is_err());
        assert!(check(&ExchangeRequest { timeout_ms: 0, ..req("x") }).is_err());
        assert!(check(&ExchangeRequest { until: Some(String::new()), ..req("x") }).is_err());
        assert!(check(&ExchangeRequest { port_wait_ms: MAX_TIMEOUT_MS, ..req("x") }).is_ok());
        assert!(check(&ExchangeRequest { port_wait_ms: MAX_TIMEOUT_MS + 1, ..req("x") }).is_err());
        assert!(check(&req(&"x".repeat(MAX_WRITE_BYTES + 1))).unwrap_err().contains("at most"));
    }

    #[test]
    fn a_request_body_needs_only_write() {
        let r: ExchangeRequest = serde_json::from_str(r#"{"write":"help\n"}"#).unwrap();
        assert_eq!(r.timeout_ms, DEFAULT_TIMEOUT_MS);
        assert!(r.discard_pending && r.match_after_echo && r.until.is_none());
    }
}
