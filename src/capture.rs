//! `POST|GET|DELETE /signals/{name}/capture` — a background capture of one declared signal's
//! port (decision 92).
//!
//! `/serial-log` and an exchange each hold `hw_lock` for their whole read, so a log capture on
//! one console starves every exchange on the other, and neither outlasts the client's 15 s
//! request timeout. A capture is a thread that owns **only its own port** and never takes
//! `hw_lock`: flash, reset and exchanges on other signals run while it records, and the lock
//! is still never held across an agent's thinking time (decision 79's reason for refusing a
//! held-open session).
//!
//! **One owner per port.** An exchange on the *captured* signal does not open the port a
//! second time (a COM port opens once): it is handed to the capture's thread, which writes
//! through its open handle and reads the reply through [`Tee`], so the reply's bytes also land
//! in the capture. A read error (a USB CDC ACM device re-enumerating) puts the capture in
//! `reopening`: it marks the gap in the byte stream and reopens by resolving the signal again.
//! A UART bridge does not drop on a board's reset, so a boot log is held without a gap.

use anyhow::{Context, Result};
use serde::Serialize;
use std::collections::HashMap;
use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender, SyncSender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::exchange::{ExchangeRequest, ExchangeResponse, Exchanged, CoreState, PortIdentity};

/// Longest a capture may run: ten minutes. **Reasoned, not measured**: the field's longest
/// trace was 60 s, and a capture is not bounded by the client's request timeout (each `GET` is
/// short), so the cap only bounds a forgotten capture holding a port.
pub const MAX_S: u64 = 600;
/// `max_s` when the caller gives none.
pub const DEFAULT_S: u64 = 60;
/// Largest `max_bytes`: 16 MiB held in memory per capture. Reasoned: a UART console at
/// 921600 baud is ~90 KB/s, so 600 s of it is 54 MB; the cap ends a runaway console first.
pub const MAX_BYTES: usize = 16 * 1024 * 1024;
/// `max_bytes` when the caller gives none (1 MiB, the `/serial-log` default).
pub const DEFAULT_BYTES: usize = 1024 * 1024;
/// Most one `GET` returns.
pub const MAX_READ: usize = 4 * 1024 * 1024;
/// `GET`'s `limit` when none is given.
pub const DEFAULT_READ: usize = 256 * 1024;
/// How long a lost port is retried between opens.
const REOPEN_POLL: Duration = Duration::from_millis(250);
const IDLE_SLEEP: Duration = Duration::from_millis(5);
/// Slack past an exchange's own `timeout_ms` before its caller gives up on the capture thread.
const JOB_SLACK: Duration = Duration::from_secs(3);

/// A port a capture owns: readable, writable, with the per-call timeout settable.
pub trait CapturePort: Read + Write + Send {
    fn set_timeout(&mut self, d: Duration);
}

impl CapturePort for Box<dyn serialport::SerialPort> {
    fn set_timeout(&mut self, d: Duration) {
        let _ = serialport::SerialPort::set_timeout(self.as_mut(), d);
    }
}

/// Opens (or reopens) the capture's port. Production resolves the signal again each call, so a
/// re-enumerated device is found under its new name.
pub type Opener = Arc<dyn Fn() -> Result<Box<dyn CapturePort>> + Send + Sync>;

/// Called on the capture's thread with every run of bytes it just read (an exchange's reply
/// included), in order, once the capture's own buffer holds them. A host study's tap persists
/// through it (decision 98); the markers a capture inserts for a lost port are not passed.
pub type Sink = Arc<dyn Fn(&[u8]) + Send + Sync>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum State {
    Running,
    /// The port is gone (read error); retrying the open.
    Reopening,
    /// Ended by `DELETE`.
    Stopped,
    /// `max_s` passed.
    Expired,
    /// `max_bytes` reached.
    ByteCap,
}

impl State {
    pub fn running(self) -> bool {
        matches!(self, State::Running | State::Reopening)
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Gap {
    /// Byte offset in the capture where the marker was written.
    pub offset: u64,
    pub lost_at_ms: u64,
    /// `None` while the port is still away.
    pub reopened_at_ms: Option<u64>,
}

/// When one line's first byte was received (decision 94).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct LineRx {
    /// Byte offset of the line's first byte, in the same space as `offset`, `next_offset` and
    /// `gaps`.
    pub offset: u64,
    /// Milliseconds after the capture started (`started_utc_ms`), read off the `read()` that
    /// first returned the line's first byte.
    pub rx_ms: u64,
}

struct Inner {
    buf: Vec<u8>,
    gaps: Vec<Gap>,
    /// One entry per line start, ascending by `offset`.
    lines: Vec<LineRx>,
    /// The next byte appended begins a line.
    at_line_start: bool,
    state: State,
}

impl Inner {
    /// Appends `bytes` and stamps every line whose first byte is among them with `now_ms`.
    fn push(&mut self, bytes: &[u8], now_ms: u64) {
        let base = self.buf.len() as u64;
        for (i, b) in bytes.iter().enumerate() {
            if self.at_line_start {
                self.lines.push(LineRx { offset: base + i as u64, rx_ms: now_ms });
                self.at_line_start = false;
            }
            if *b == b'\n' {
                self.at_line_start = true;
            }
        }
        self.buf.extend_from_slice(bytes);
    }
}

struct Job {
    req: ExchangeRequest,
    core_state: Box<dyn FnOnce() -> CoreState + Send>,
    reply: SyncSender<Result<ExchangeResponse>>,
}

pub struct Capture {
    pub id: String,
    pub signal: String,
    pub port: String,
    pub baud: u32,
    started: Instant,
    /// `started` as UTC milliseconds since the Unix epoch (decision 94).
    started_utc_ms: u64,
    deadline: Instant,
    max_bytes: usize,
    identity: Option<PortIdentity>,
    inner: Mutex<Inner>,
    stop: AtomicBool,
    jobs: Mutex<Sender<Job>>,
    sink: Option<Sink>,
}

/// What `GET` and `DELETE` answer.
#[derive(Debug, Serialize)]
pub struct CaptureRead {
    pub id: String,
    pub signal: String,
    pub port: String,
    pub baud: u32,
    pub state: State,
    pub running: bool,
    /// The offset this answer starts at.
    pub offset: u64,
    /// Pass this as `after` to read on.
    pub next_offset: u64,
    pub total_bytes: u64,
    /// Lossy UTF-8, escape codes left in. Never ends mid-character while running.
    pub text: String,
    pub elapsed_ms: u64,
    pub gaps: Vec<Gap>,
    /// The capture's start, UTC ms since the Unix epoch: the clock an exchange reply's
    /// `started_utc_ms` is also on (decision 94).
    pub started_utc_ms: u64,
    /// The lines whose first byte lies in `[offset, next_offset)`, with when it was received.
    pub lines: Vec<LineRx>,
}

/// Why a capture could not start.
#[derive(Debug)]
pub enum StartError {
    Conflict(String),
    Open(String),
}

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

impl Capture {
    fn elapsed_ms(&self) -> u64 {
        self.started.elapsed().as_millis() as u64
    }

    pub fn state(&self) -> State {
        self.inner.lock().unwrap().state
    }

    pub fn stop(&self) {
        self.stop.store(true, Ordering::SeqCst);
    }

    /// Everything from byte `after`, at most `limit` bytes.
    pub fn read(&self, after: u64, limit: usize) -> CaptureRead {
        let inner = self.inner.lock().unwrap();
        let total = inner.buf.len();
        let from = (after as usize).min(total);
        let to = from.saturating_add(limit).min(total);
        let mut slice = &inner.buf[from..to];
        let mut next = to;
        if inner.state.running() || to < total {
            // An incomplete multi-byte character at the end waits for its next byte.
            if let Err(e) = std::str::from_utf8(slice) {
                if e.error_len().is_none() {
                    slice = &slice[..e.valid_up_to()];
                    next = from + slice.len();
                }
            }
        }
        CaptureRead {
            id: self.id.clone(),
            signal: self.signal.clone(),
            port: self.port.clone(),
            baud: self.baud,
            state: inner.state,
            running: inner.state.running(),
            offset: from as u64,
            next_offset: next as u64,
            total_bytes: total as u64,
            text: String::from_utf8_lossy(slice).into_owned(),
            elapsed_ms: self.elapsed_ms(),
            gaps: inner.gaps.clone(),
            started_utc_ms: self.started_utc_ms,
            lines: inner.lines[inner.lines.partition_point(|l| l.offset < from as u64)
                ..inner.lines.partition_point(|l| l.offset < next as u64)]
                .to_vec(),
        }
    }

    /// Runs an exchange through the capture's open port; blocking. `Err` when the port is away
    /// or the capture ended before it answered.
    pub fn exchange(
        &self,
        req: ExchangeRequest,
        core_state: impl FnOnce() -> CoreState + Send + 'static,
    ) -> Result<ExchangeResponse> {
        let wait = Duration::from_millis(req.timeout_ms) + JOB_SLACK;
        let (reply, rx) = sync_channel1();
        self.jobs
            .lock()
            .unwrap()
            .send(Job { req, core_state: Box::new(core_state), reply })
            .map_err(|_| anyhow::anyhow!("the capture on '{}' has ended", self.signal))?;
        rx.recv_timeout(wait)
            .map_err(|_| anyhow::anyhow!("the capture on '{}' did not answer in time", self.signal))?
    }
}

fn sync_channel1() -> (SyncSender<Result<ExchangeResponse>>, Receiver<Result<ExchangeResponse>>) {
    std::sync::mpsc::sync_channel(1)
}

/// Reads through to the capture's buffer, so an exchange's reply is also captured.
struct Tee<'a> {
    port: &'a mut dyn CapturePort,
    cap: &'a Capture,
}

impl Read for Tee<'_> {
    fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
        let n = self.port.read(out)?;
        append(self.cap, &out[..n]);
        Ok(n)
    }
}

impl Write for Tee<'_> {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        self.port.write(b)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.port.flush()
    }
}

/// Appends up to the byte cap. Returns whether the cap is now reached.
fn append(cap: &Capture, bytes: &[u8]) -> bool {
    let (full, taken) = {
        let mut inner = cap.inner.lock().unwrap();
        let room = cap.max_bytes.saturating_sub(inner.buf.len());
        let now = cap.elapsed_ms();
        let taken = bytes.len().min(room);
        inner.push(&bytes[..taken], now);
        (inner.buf.len() >= cap.max_bytes, taken)
    };
    if let (Some(sink), true) = (&cap.sink, taken > 0) {
        sink(&bytes[..taken]);
    }
    full
}

fn finish(cap: &Capture, state: State) {
    cap.inner.lock().unwrap().state = state;
}

fn mark_lost(cap: &Capture) {
    let mut inner = cap.inner.lock().unwrap();
    let offset = inner.buf.len() as u64;
    let now = cap.elapsed_ms();
    inner.push(b"\n[embarch: port lost]\n", now);
    inner.gaps.push(Gap { offset, lost_at_ms: now, reopened_at_ms: None });
    inner.state = State::Reopening;
}

fn mark_reopened(cap: &Capture) {
    let mut inner = cap.inner.lock().unwrap();
    let now = cap.elapsed_ms();
    if let Some(g) = inner.gaps.last_mut() {
        g.reopened_at_ms = Some(now);
    }
    inner.push(b"[embarch: port reopened]\n", now);
    inner.state = State::Running;
}

fn run_job(cap: &Capture, port: &mut dyn CapturePort, job: Job) -> bool {
    let started = Instant::now();
    let started_utc_ms = crate::exchange::utc_ms();
    let mut tee = Tee { port, cap };
    let r = crate::exchange::exchange_with(
        &mut tee,
        job.req.write.as_bytes(),
        job.req.until.as_deref(),
        job.req.match_after_echo,
        Duration::from_millis(job.req.timeout_ms),
        crate::exchange::MAX_READ_BYTES,
        |t, d| t.port.set_timeout(d),
    );
    let lost = matches!(&r, Ok(g) if g.port_lost);
    let answer = r.map(|got: Exchanged| {
        crate::exchange::response(
            &cap.signal,
            &cap.port,
            cap.baud,
            started.elapsed().as_millis() as u64,
            started_utc_ms,
            got,
            || cap.identity.clone(),
            job.core_state,
        )
    });
    let _ = job.reply.send(answer);
    lost
}

fn thread_main(
    cap: Arc<Capture>,
    opener: Opener,
    mut port: Option<Box<dyn CapturePort>>,
    rx: Receiver<Job>,
    _claim: Box<dyn Send>,
) {
    let mut buf = [0u8; 4096];
    loop {
        if cap.stop.load(Ordering::SeqCst) {
            return finish(&cap, State::Stopped);
        }
        if Instant::now() >= cap.deadline {
            return finish(&cap, State::Expired);
        }
        if cap.inner.lock().unwrap().buf.len() >= cap.max_bytes {
            return finish(&cap, State::ByteCap);
        }
        let Some(p) = port.as_mut() else {
            while let Ok(job) = rx.try_recv() {
                let _ = job.reply.send(Err(anyhow::anyhow!(
                    "the capture's port on '{}' is away (reopening): nothing was written",
                    cap.signal
                )));
            }
            std::thread::sleep(REOPEN_POLL);
            if let Ok(mut np) = opener() {
                np.set_timeout(crate::exchange::READ_POLL);
                port = Some(np);
                mark_reopened(&cap);
            }
            continue;
        };
        let mut lost = false;
        while let Ok(job) = rx.try_recv() {
            lost |= run_job(&cap, p.as_mut(), job);
            if lost {
                break;
            }
        }
        if !lost {
            match p.read(&mut buf) {
                Ok(0) => std::thread::sleep(IDLE_SLEEP),
                Ok(n) => {
                    append(&cap, &buf[..n]);
                }
                Err(e) if e.kind() == std::io::ErrorKind::TimedOut => {}
                Err(_) => lost = true,
            }
        }
        if lost {
            port = None;
            mark_lost(&cap);
        }
    }
}

/// Every capture in this process, by signal name. A finished capture stays readable until the
/// next one on its signal replaces it or `DELETE` drops it.
#[derive(Clone, Default)]
pub struct CaptureRegistry(Arc<Mutex<HashMap<String, Arc<Capture>>>>);

impl CaptureRegistry {
    pub fn get(&self, signal: &str) -> Option<Arc<Capture>> {
        self.0.lock().unwrap().get(signal).cloned()
    }

    /// The capture on `signal` that is still recording, if any.
    pub fn running(&self, signal: &str) -> Option<Arc<Capture>> {
        self.get(signal).filter(|c| c.state().running())
    }

    pub fn remove(&self, signal: &str) {
        self.0.lock().unwrap().remove(signal);
    }

    /// Starts a capture. The port is opened here, so an open failure is the caller's answer;
    /// `claim` is held until the capture's thread ends (`AppState::tap_ports`).
    #[allow(clippy::too_many_arguments)]
    pub fn start(
        &self,
        signal: &str,
        port_name: &str,
        baud: u32,
        max_s: u64,
        max_bytes: usize,
        identity: Option<PortIdentity>,
        opener: Opener,
        claim: Box<dyn Send>,
    ) -> Result<Arc<Capture>, StartError> {
        self.start_with_sink(signal, port_name, baud, max_s, max_bytes, identity, opener, claim, None)
    }

    /// [`start`](Self::start) with a [`Sink`] that sees every byte the capture reads.
    #[allow(clippy::too_many_arguments)]
    pub fn start_with_sink(
        &self,
        signal: &str,
        port_name: &str,
        baud: u32,
        max_s: u64,
        max_bytes: usize,
        identity: Option<PortIdentity>,
        opener: Opener,
        claim: Box<dyn Send>,
        sink: Option<Sink>,
    ) -> Result<Arc<Capture>, StartError> {
        let mut map = self.0.lock().unwrap();
        if let Some(c) = map.get(signal) {
            if c.state().running() {
                return Err(StartError::Conflict(format!(
                    "signal '{signal}' already has capture {} running; DELETE it first",
                    c.id
                )));
            }
        }
        let mut port = opener().with_context(|| format!("failed to open signal '{signal}' on {port_name} at {baud} baud"))
            .map_err(|e| StartError::Open(format!("{e:#}")))?;
        port.set_timeout(crate::exchange::READ_POLL);
        let (tx, rx) = channel();
        let now = Instant::now();
        let started_utc_ms = crate::exchange::utc_ms();
        let cap = Arc::new(Capture {
            id: format!("cap-{}", NEXT_ID.fetch_add(1, Ordering::SeqCst)),
            signal: signal.to_string(),
            port: port_name.to_string(),
            baud,
            started: now,
            started_utc_ms,
            deadline: now + Duration::from_secs(max_s),
            max_bytes,
            identity,
            inner: Mutex::new(Inner { buf: Vec::new(), gaps: Vec::new(), lines: Vec::new(), at_line_start: true, state: State::Running }),
            stop: AtomicBool::new(false),
            jobs: Mutex::new(tx),
            sink,
        });
        let c = Arc::clone(&cap);
        std::thread::Builder::new()
            .name(format!("capture-{signal}"))
            .spawn(move || thread_main(c, opener, Some(port), rx, claim))
            .map_err(|e| StartError::Open(format!("failed to start the capture thread: {e}")))?;
        map.insert(signal.to_string(), Arc::clone(&cap));
        Ok(cap)
    }
}

/// Checks `max_s` and `max_bytes` before anything opens.
pub fn check(max_s: u64, max_bytes: usize) -> Result<(), String> {
    if max_s == 0 || max_s > MAX_S {
        return Err(format!("max_s={max_s} is outside 1..={MAX_S}"));
    }
    if max_bytes == 0 || max_bytes > MAX_BYTES {
        return Err(format!("max_bytes={max_bytes} is outside 1..={MAX_BYTES}"));
    }
    Ok(())
}

/// A capture over a scripted port, for `api.rs`'s route tests: no port is ever opened.
#[cfg(test)]
pub(crate) fn start_fake(reg: &CaptureRegistry, signal: &str, incoming: &[u8]) -> Arc<Capture> {
    struct P(Option<Vec<u8>>);
    impl Read for P {
        fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
            match self.0.take() {
                Some(c) => {
                    out[..c.len()].copy_from_slice(&c);
                    Ok(c.len())
                }
                None => {
                    std::thread::sleep(Duration::from_millis(2));
                    Err(std::io::ErrorKind::TimedOut.into())
                }
            }
        }
    }
    impl Write for P {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            Ok(b.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    impl CapturePort for P {
        fn set_timeout(&mut self, _d: Duration) {}
    }
    let data = incoming.to_vec();
    let opener: Opener = Arc::new(move || Ok(Box::new(P(Some(data.clone()))) as Box<dyn CapturePort>));
    reg.start(signal, "FAKE", 115200, 30, 4096, None, opener, Box::new(()))
        .map_err(|e| format!("{e:?}"))
        .unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;

    /// A port fed from a shared script; an exhausted script reads as a timeout. `fail` makes
    /// every read an error, the way a re-enumerating CDC ACM device does.
    #[derive(Clone, Default)]
    struct Wire {
        incoming: Arc<Mutex<VecDeque<Vec<u8>>>>,
        written: Arc<Mutex<Vec<u8>>>,
        fail: Arc<AtomicBool>,
        // What a write echoes back, like a shell does.
        echo: bool,
    }

    impl Wire {
        fn feed(&self, b: &[u8]) {
            self.incoming.lock().unwrap().push_back(b.to_vec());
        }
    }

    impl Read for Wire {
        fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
            if self.fail.load(Ordering::SeqCst) {
                return Err(std::io::Error::new(std::io::ErrorKind::BrokenPipe, "gone"));
            }
            match self.incoming.lock().unwrap().pop_front() {
                Some(c) => {
                    out[..c.len()].copy_from_slice(&c);
                    Ok(c.len())
                }
                None => {
                    std::thread::sleep(Duration::from_millis(2));
                    Err(std::io::ErrorKind::TimedOut.into())
                }
            }
        }
    }

    impl Write for Wire {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            self.written.lock().unwrap().extend_from_slice(b);
            if self.echo {
                let mut reply = b.to_vec();
                reply.extend_from_slice(b"\nok\n> ");
                self.incoming.lock().unwrap().push_back(reply);
            }
            Ok(b.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl CapturePort for Wire {
        fn set_timeout(&mut self, _d: Duration) {}
    }

    fn opener(w: Wire) -> Opener {
        Arc::new(move || {
            if w.fail.load(Ordering::SeqCst) {
                anyhow::bail!("not enumerable");
            }
            Ok(Box::new(w.clone()) as Box<dyn CapturePort>)
        })
    }

    fn start(reg: &CaptureRegistry, sig: &str, w: &Wire, max_s: u64, max_bytes: usize) -> Arc<Capture> {
        reg.start(sig, "FAKE", 115200, max_s, max_bytes, None, opener(w.clone()), Box::new(()))
            .map_err(|e| format!("{e:?}"))
            .unwrap()
    }

    fn wait(cond: impl Fn() -> bool) {
        let end = Instant::now() + Duration::from_secs(5);
        while !cond() {
            assert!(Instant::now() < end, "timed out waiting");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn req(write: &str, until: Option<&str>) -> ExchangeRequest {
        serde_json::from_value(serde_json::json!({"write": write, "until": until, "timeout_ms": 2000})).unwrap()
    }

    #[test]
    fn records_and_reads_from_an_offset() {
        let reg = CaptureRegistry::default();
        let w = Wire::default();
        let c = start(&reg, "dut", &w, 30, 1024);
        w.feed(b"boot line 1\n");
        wait(|| c.read(0, 100).total_bytes == 12);
        w.feed(b"boot line 2\n");
        wait(|| c.read(0, 100).total_bytes == 24);
        let r = c.read(12, 100);
        assert_eq!(r.text, "boot line 2\n");
        assert_eq!(r.next_offset, 24);
        assert!(r.running);
        assert_eq!(c.read(0, 5).text, "boot ");
    }

    #[test]
    fn a_second_capture_on_a_running_signal_is_a_conflict_naming_the_first() {
        let reg = CaptureRegistry::default();
        let w = Wire::default();
        let c = start(&reg, "dut", &w, 30, 1024);
        let err = reg.start("dut", "FAKE", 1, 30, 10, None, opener(w.clone()), Box::new(())).err().unwrap();
        match err {
            StartError::Conflict(m) => assert!(m.contains(&c.id), "{m}"),
            e => panic!("{e:?}"),
        }
        c.stop();
        wait(|| !c.state().running());
        // A finished capture is replaced, not a conflict.
        let c2 = start(&reg, "dut", &w, 30, 1024);
        assert_ne!(c.id, c2.id);
        c2.stop();
    }

    #[test]
    fn stop_expiry_and_the_byte_cap_each_end_it() {
        let reg = CaptureRegistry::default();
        let w = Wire::default();
        let c = start(&reg, "a", &w, 30, 1024);
        c.stop();
        wait(|| c.state() == State::Stopped);

        let c = start(&reg, "b", &w, 1, 1024);
        wait(|| c.state() == State::Expired);

        w.feed(b"0123456789abcdef");
        let c = start(&reg, "c", &w, 30, 10);
        wait(|| c.state() == State::ByteCap);
        let r = c.read(0, 100);
        assert_eq!(r.text, "0123456789");
        assert!(!r.running);
    }

    #[test]
    fn a_finished_capture_stays_readable() {
        let reg = CaptureRegistry::default();
        let w = Wire::default();
        w.feed(b"last words\n");
        let c = start(&reg, "dut", &w, 30, 1024);
        wait(|| c.read(0, 100).total_bytes == 11);
        c.stop();
        wait(|| !c.state().running());
        assert_eq!(reg.get("dut").unwrap().read(0, 100).text, "last words\n");
        assert!(reg.running("dut").is_none());
    }

    #[test]
    fn an_exchange_goes_through_the_capture_and_its_bytes_land_in_it() {
        let reg = CaptureRegistry::default();
        let w = Wire { echo: true, ..Wire::default() };
        let c = start(&reg, "bench", &w, 30, 4096);
        w.feed(b"noise\n");
        wait(|| c.read(0, 100).total_bytes == 6);
        let resp = c.exchange(req("pd status\n", Some("> ")), || CoreState::unavailable("test")).unwrap();
        assert!(resp.until_seen && resp.echo_seen && resp.written, "{resp:?}");
        assert!(resp.text.contains("ok"));
        assert_eq!(&*w.written.lock().unwrap(), b"pd status\n");
        // The capture holds the earlier noise and the exchange's reply, byte for byte.
        let all = c.read(0, 4096).text;
        assert!(all.starts_with("noise\n"));
        assert!(all.ends_with(&resp.text), "{all:?} vs {:?}", resp.text);
    }

    #[test]
    fn a_lost_port_is_marked_and_reopened() {
        let reg = CaptureRegistry::default();
        let w = Wire::default();
        let c = start(&reg, "dut", &w, 30, 4096);
        w.feed(b"before\n");
        wait(|| c.read(0, 100).total_bytes == 7);
        w.fail.store(true, Ordering::SeqCst);
        wait(|| c.state() == State::Reopening);
        // An exchange while the port is away says so rather than hanging.
        let e = c.exchange(req("x\n", None), || CoreState::unavailable("t")).unwrap_err();
        assert!(format!("{e}").contains("away"), "{e}");
        w.fail.store(false, Ordering::SeqCst);
        wait(|| c.state() == State::Running);
        w.feed(b"after\n");
        wait(|| c.read(0, 4096).text.ends_with("after\n"));
        let r = c.read(0, 4096);
        assert!(r.text.contains("[embarch: port lost]") && r.text.contains("[embarch: port reopened]"), "{}", r.text);
        assert_eq!(r.gaps.len(), 1);
        assert_eq!(r.gaps[0].offset, 7);
        assert!(r.gaps[0].reopened_at_ms.is_some());
    }

    #[test]
    fn a_read_never_ends_inside_a_character_while_running() {
        let reg = CaptureRegistry::default();
        let w = Wire::default();
        let c = start(&reg, "dut", &w, 30, 4096);
        let bytes = "aé".as_bytes(); // 'é' is two bytes
        w.feed(&bytes[..2]);
        wait(|| c.read(0, 100).total_bytes == 2);
        let r = c.read(0, 100);
        assert_eq!((r.text.as_str(), r.next_offset), ("a", 1));
        w.feed(&bytes[2..]);
        wait(|| c.read(0, 100).total_bytes == 3);
        assert_eq!(c.read(1, 100).text, "é");
    }

    #[test]
    fn lines_carry_receive_times_and_offsets_match_text_across_a_gap() {
        let reg = CaptureRegistry::default();
        let w = Wire::default();
        let c = start(&reg, "dut", &w, 30, 4096);
        w.feed(b"first\n");
        wait(|| c.read(0, 100).total_bytes == 6);
        std::thread::sleep(Duration::from_millis(200));
        w.feed(b"sec");
        wait(|| c.read(0, 100).total_bytes == 9);
        w.feed(b"ond\n");
        wait(|| c.read(0, 100).total_bytes == 13);
        std::thread::sleep(Duration::from_millis(50));
        w.feed(b"third\n");
        wait(|| c.read(0, 100).total_bytes == 19);
        let r = c.read(0, 100);
        // A line is stamped when its first byte arrives, so "second" is placed by "sec".
        assert_eq!(r.lines.iter().map(|l| l.offset).collect::<Vec<_>>(), [0, 6, 13]);
        let dt = r.lines[1].rx_ms - r.lines[0].rx_ms;
        // Both stamps bracket the sleep (the first is taken before `wait` returns, the second after
        // the feed), so 200 ms is the floor (less 1 ms for each stamp's truncation); the ceiling is only a scheduler-stall allowance.
        assert!((199..=10_000).contains(&dt), "two lines 200 ms apart were {dt} ms apart");
        assert!(r.lines[2].rx_ms >= r.lines[1].rx_ms);
        assert!(r.started_utc_ms > 1_700_000_000_000);
        // A range returns only the lines that start inside it.
        let tail = c.read(6, 100);
        assert_eq!(tail.lines.iter().map(|l| l.offset).collect::<Vec<_>>(), [6, 13]);
        // Across a gap: markers are lines too, and every offset is the start of a line in text.
        w.fail.store(true, Ordering::SeqCst);
        wait(|| c.state() == State::Reopening);
        w.fail.store(false, Ordering::SeqCst);
        wait(|| c.state() == State::Running);
        w.feed(b"after\n");
        wait(|| c.read(0, 4096).text.ends_with("after\n"));
        let r = c.read(0, 4096);
        let bytes = r.text.as_bytes();
        assert_eq!(r.lines.len(), r.text.lines().count());
        for l in &r.lines {
            let o = l.offset as usize;
            assert!(o == 0 || bytes[o - 1] == b'\n', "offset {o} is not a line start");
        }
        let lost = r.text.find("[embarch: port lost]").unwrap() as u64;
        assert!(r.lines.iter().any(|l| l.offset == lost));
        assert_eq!(r.gaps[0].offset + 1, lost);
    }

    #[test]
    fn caps_are_checked_before_anything_opens() {
        assert!(check(1, 1).is_ok());
        assert!(check(MAX_S, MAX_BYTES).is_ok());
        assert!(check(0, 1).is_err());
        assert!(check(MAX_S + 1, 1).is_err());
        assert!(check(1, 0).is_err());
        assert!(check(1, MAX_BYTES + 1).is_err());
    }
}
