use anyhow::{Context, Result};
use std::io::Read;
use std::time::{Duration, Instant};

/// Ceiling on `GET /serial-log`'s `duration_ms`, checked in `api.rs` before
/// `hw_lock` is even taken.
///
/// **10,000 is reasoned, not measured.** `embarch-core-client`'s
/// `default_serial_timeout_secs()` is 15 (`embarch-api/crates/embarch-core-client/src/lib.rs`),
/// so a `duration_ms` anywhere near that meets or exceeds the client's own
/// deadline while Core goes on holding `hw_lock` for the whole span
/// (`embarch-doc/embarch-core/interfaces.md`'s caller-side-ceiling note,
/// `tasks/core/009`). This cap sits comfortably under that client timeout
/// rather than matching it exactly, so a caller using the shared client's
/// default never has to think about the two numbers lining up.
pub const MAX_DURATION_MS: u64 = 10_000;

/// Byte cap on one `/serial-log` capture — "in the spirit of"
/// `stream_store::EMBARCH_STREAM_MAX_BYTES`, but two orders of magnitude
/// smaller: this is a bounded diagnostic *snapshot* of console lines
/// (`interfaces.md`'s own description of the route), not a bulk stream tap.
///
/// **1 MiB is reasoned, not measured** — far more than a well-behaved
/// console emits inside [`MAX_DURATION_MS`], small enough that a DUT stuck
/// spamming one line forever can't grow the response past what a caller can
/// parse, or hold `hw_lock` collecting bytes nobody asked for.
pub const DEFAULT_MAX_BYTES: usize = 1024 * 1024;

/// Env var overriding [`DEFAULT_MAX_BYTES`], following the same convention
/// as `stream_store::STREAM_MAX_BYTES_ENV`.
pub const MAX_BYTES_ENV: &str = "EMBARCH_SERIAL_LOG_MAX_BYTES";

/// How long a read that returns `Ok(0)` (no data, no error — e.g. a `Read`
/// that has hit an EOF-like state rather than a real timeout) sleeps before
/// polling again. Without this, a reader that never times out and never
/// blocks turns the loop into a busy-spin on a `spawn_blocking` thread for
/// the whole deadline.
const IDLE_SLEEP: Duration = Duration::from_millis(10);

/// `EMBARCH_SERIAL_LOG_MAX_BYTES`, or [`DEFAULT_MAX_BYTES`]. An unparseable
/// value warns and falls back, the same posture as
/// `stream_store::stream_max_bytes`.
pub fn serial_log_max_bytes() -> usize {
    match std::env::var(MAX_BYTES_ENV) {
        Err(_) => DEFAULT_MAX_BYTES,
        Ok(raw) => match raw.trim().parse::<usize>() {
            Ok(v) => v,
            Err(_) => {
                tracing::warn!(
                    "{MAX_BYTES_ENV}='{raw}' isn't a byte count; using the default of {DEFAULT_MAX_BYTES}"
                );
                DEFAULT_MAX_BYTES
            }
        },
    }
}

/// One `/serial-log` capture's result: the decoded lines, and whether the
/// byte cap cut it short before `duration_ms` (or the source) ran out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CaptureResult {
    pub lines: Vec<String>,
    pub truncated: bool,
}

/// Open a serial port, capture output for `duration_ms` up to `max_bytes`,
/// and return it as lines.
///
/// This is a UART/USB-serial console (the target's stdout/log output) — a
/// separate physical connection from the JTAG/SWD debug probe in `hardware.rs`.
/// Most boards expose both: a probe for flashing/debug, and a serial adapter
/// for the running firmware's log output.
pub fn read_log(port: &str, baud: u32, duration_ms: u64, max_bytes: usize) -> Result<CaptureResult> {
    let mut conn = serialport::new(port, baud)
        .timeout(Duration::from_millis(200))
        .open()
        .with_context(|| format!("failed to open serial port '{port}'"))?;

    capture(&mut conn, duration_ms, max_bytes)
}

/// The read loop itself, over any [`Read`] — split out from [`read_log`] so
/// the deadline, the byte cap, and the `Ok(0)` idle path are all exercisable
/// with a fake source and no port ever opened.
fn capture<R: Read>(reader: &mut R, duration_ms: u64, max_bytes: usize) -> Result<CaptureResult> {
    let deadline = Instant::now() + Duration::from_millis(duration_ms);
    let mut buf = [0u8; 1024];
    let mut collected: Vec<u8> = Vec::new();
    let mut truncated = false;

    while Instant::now() < deadline {
        if collected.len() >= max_bytes {
            truncated = true;
            break;
        }

        match reader.read(&mut buf) {
            Ok(0) => std::thread::sleep(IDLE_SLEEP),
            Ok(n) => {
                let remaining = max_bytes - collected.len();
                if n > remaining {
                    collected.extend_from_slice(&buf[..remaining]);
                    truncated = true;
                    break;
                }
                collected.extend_from_slice(&buf[..n]);
            }
            Err(e) if e.kind() == std::io::ErrorKind::TimedOut => {}
            Err(e) => return Err(e).context("error reading from serial port"),
        }
    }

    let text = String::from_utf8_lossy(&collected);
    Ok(CaptureResult {
        lines: text.lines().map(|l| l.to_string()).collect(),
        truncated,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::collections::VecDeque;
    use std::io;

    /// A fake serial source: a queue of "reads", each either data or a
    /// `TimedOut` error (the real `serialport` timeout's shape) or `Ok(0)`
    /// (the idle/EOF-like case). Once the queue is empty it repeats the
    /// last entry forever, so a test can run past its scripted reads to
    /// exercise the deadline.
    #[derive(Clone)]
    enum Step {
        Data(&'static [u8]),
        TimedOut,
        Idle,
    }

    struct FakeReader {
        steps: VecDeque<Step>,
        repeat: Step,
        calls: Cell<u32>,
    }

    impl FakeReader {
        fn new(steps: Vec<Step>, repeat: Step) -> Self {
            FakeReader {
                steps: steps.into(),
                repeat,
                calls: Cell::new(0),
            }
        }
    }

    impl Read for FakeReader {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            self.calls.set(self.calls.get() + 1);
            let step = self.steps.pop_front().unwrap_or_else(|| self.repeat.clone());
            match step {
                Step::Data(d) => {
                    let n = d.len().min(buf.len());
                    buf[..n].copy_from_slice(&d[..n]);
                    Ok(n)
                }
                Step::TimedOut => Err(io::Error::new(io::ErrorKind::TimedOut, "timed out")),
                Step::Idle => Ok(0),
            }
        }
    }

    #[test]
    fn deadline_stops_the_read_with_no_port_ever_opened() {
        let mut reader = FakeReader::new(vec![], Step::TimedOut);
        let start = Instant::now();
        let result = capture(&mut reader, 30, DEFAULT_MAX_BYTES).unwrap();
        assert!(start.elapsed() >= Duration::from_millis(30));
        // Generous ceiling: proves this returns promptly rather than
        // hanging well past its own deadline.
        assert!(start.elapsed() < Duration::from_millis(500));
        assert_eq!(result.lines, Vec::<String>::new());
        assert!(!result.truncated);
    }

    #[test]
    fn byte_cap_truncates_before_the_deadline_and_says_so() {
        // An endless stream of data — with no cap this would run for the
        // whole `duration_ms`. A tight `max_bytes` should cut it off almost
        // immediately.
        let mut reader = FakeReader::new(vec![], Step::Data(b"hello\n"));
        let start = Instant::now();
        let result = capture(&mut reader, 5_000, 10).unwrap();
        assert!(result.truncated);
        assert!(result.lines.iter().map(|l| l.len()).sum::<usize>() <= 10);
        assert!(
            start.elapsed() < Duration::from_millis(500),
            "byte cap should end the read long before the 5s deadline"
        );
    }

    #[test]
    fn zero_byte_reads_stop_spinning() {
        // If `Ok(0)` didn't yield, this would call `read` millions of times
        // in 100ms. Bound the call count to prove it's sleeping between
        // polls rather than busy-looping.
        let mut reader = FakeReader::new(vec![], Step::Idle);
        capture(&mut reader, 100, DEFAULT_MAX_BYTES).unwrap();
        assert!(
            reader.calls.get() < 100,
            "expected the idle sleep to bound poll count, got {} calls",
            reader.calls.get()
        );
    }

    #[test]
    fn under_cap_reads_are_unaffected() {
        let mut reader = FakeReader::new(vec![Step::Data(b"line one\nline two\n")], Step::TimedOut);
        let result = capture(&mut reader, 50, DEFAULT_MAX_BYTES).unwrap();
        assert_eq!(result.lines, vec!["line one".to_string(), "line two".to_string()]);
        assert!(!result.truncated);
    }
}
