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
}

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
}

/// What one exchange read back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Exchanged {
    pub bytes: Vec<u8>,
    pub until_seen: bool,
    pub truncated: bool,
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
    Ok(())
}

/// Writes `write`, then reads until `until` appears in what was read, the
/// deadline passes, or `max_bytes` is reached — over any `Read + Write`, so
/// every exit is testable with no port opened.
pub fn exchange<P: Read + Write>(
    port: &mut P,
    write: &[u8],
    until: Option<&str>,
    match_after_echo: bool,
    timeout: Duration,
    max_bytes: usize,
) -> Result<Exchanged> {
    port.write_all(write).context("error writing to the signal's port")?;
    port.flush().context("error flushing the signal's port")?;
    let deadline = Instant::now() + timeout;
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
    while Instant::now() < deadline {
        match port.read(&mut buf) {
            Ok(0) => std::thread::sleep(IDLE_SLEEP),
            Ok(n) => {
                let take = n.min(max_bytes - got.len());
                got.extend_from_slice(&buf[..take]);
                if search_from.is_none() {
                    search_from = find(&got, echo, 0).map(|i| i + echo.len());
                }
                if let (Some(x), Some(start)) = (needle, search_from) {
                    if find(&got, x, start).is_some() {
                        return Ok(Exchanged { bytes: got, until_seen: true, truncated: false });
                    }
                }
                if got.len() >= max_bytes {
                    return Ok(Exchanged { bytes: got, until_seen: false, truncated: true });
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::TimedOut => {}
            Err(e) => return Err(e).context("error reading the signal's port"),
        }
    }
    Ok(Exchanged { bytes: got, until_seen: false, truncated: false })
}

/// The first index at or after `from` where `needle` starts in `hay`.
fn find(hay: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    if needle.is_empty() || from >= hay.len() {
        return None;
    }
    hay[from..].windows(needle.len()).position(|w| w == needle).map(|i| i + from)
}

/// Opens `port_name` at `baud`, asserts DTR (a Zephyr CDC ACM shell may wait
/// for it before it prints), optionally drops pending input, and runs one
/// [`exchange`].
pub fn run(signal: &str, port_name: &str, baud: u32, req: &ExchangeRequest) -> Result<ExchangeResponse> {
    let started = Instant::now();
    let mut port = serialport::new(port_name, baud)
        .timeout(Duration::from_millis(50))
        .open()
        .with_context(|| format!("failed to open signal '{signal}' on {port_name} at {baud} baud"))?;
    // Best effort: a plain UART bridge may not support the control line, and
    // that is no reason to refuse the write.
    let _ = port.write_data_terminal_ready(true);
    if req.discard_pending {
        let _ = port.clear(serialport::ClearBuffer::Input);
    }
    let got = exchange(
        &mut port,
        req.write.as_bytes(),
        req.until.as_deref(),
        req.match_after_echo,
        Duration::from_millis(req.timeout_ms),
        MAX_READ_BYTES,
    )?;
    Ok(ExchangeResponse {
        signal: signal.to_string(),
        port: port_name.to_string(),
        baud,
        text: String::from_utf8_lossy(&got.bytes).into_owned(),
        until_seen: got.until_seen,
        truncated: got.truncated,
        elapsed_ms: started.elapsed().as_millis() as u64,
    })
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

    fn req(write: &str) -> ExchangeRequest {
        ExchangeRequest {
            write: write.to_string(),
            until: None,
            timeout_ms: 100,
            discard_pending: true,
            match_after_echo: true,
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
        // Without the echo rule the early prompt ends it, which is the bug.
        let mut q = Fake::new(vec![Ok(b"uart:~$ ".to_vec()), Ok(b"kernel uptime\r\nUptime: 1 ms\r\n".to_vec())]);
        let early = exchange(&mut q, b"kernel uptime\n", Some("uart:~$ "), false, Duration::from_secs(5), 1024).unwrap();
        assert!(early.until_seen && !String::from_utf8_lossy(&early.bytes).contains("Uptime"));
    }

    #[test]
    fn a_console_that_never_stops_is_cut_at_the_byte_cap() {
        let mut p = Fake::new((0..100).map(|_| Ok(vec![b'a'; 64])).collect());
        let got = exchange(&mut p, b"x\n", Some("never"), true, Duration::from_secs(5), 100).unwrap();
        assert_eq!(got.bytes.len(), 100);
        assert!(got.truncated && !got.until_seen);
    }

    #[test]
    fn a_read_error_is_an_error_not_a_short_reply() {
        let mut p = Fake::new(vec![Err(std::io::ErrorKind::BrokenPipe.into())]);
        assert!(exchange(&mut p, b"x\n", None, true, Duration::from_secs(1), 100).is_err());
    }

    #[test]
    fn requests_are_checked_before_any_lock() {
        assert!(check(&req("kernel uptime\n")).is_ok());
        assert!(check(&req("")).unwrap_err().contains("empty"));
        assert!(check(&ExchangeRequest { timeout_ms: MAX_TIMEOUT_MS + 1, ..req("x") }).is_err());
        assert!(check(&ExchangeRequest { timeout_ms: 0, ..req("x") }).is_err());
        assert!(check(&ExchangeRequest { until: Some(String::new()), ..req("x") }).is_err());
        assert!(check(&req(&"x".repeat(MAX_WRITE_BYTES + 1))).unwrap_err().contains("at most"));
    }

    #[test]
    fn a_request_body_needs_only_write() {
        let r: ExchangeRequest = serde_json::from_str(r#"{"write":"help\n"}"#).unwrap();
        assert_eq!(r.timeout_ms, DEFAULT_TIMEOUT_MS);
        assert!(r.discard_pending && r.match_after_echo && r.until.is_none());
    }
}
