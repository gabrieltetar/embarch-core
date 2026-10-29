//! `/instruments` and `POST /instruments/{name}/exchange`: one command to a
//! declared bench instrument and its reply (decision 104).
//!
//! **The instrument's own line facts drive every byte**: its terminators, its
//! pacing and its acknowledgement strings come from the declaration
//! (`embarch-topology` decision 49), so the exchange route and a host study's
//! `Instrument` step cannot disagree about them, and neither carries a model's
//! quirks in code.
//!
//! **One exchange is: open, a flush line, the command, its reply, close**, all
//! inside one `hw_lock` hold. The flush line is the instrument's write
//! terminator alone, sent right after opening and its answer dropped from the
//! reply: an instrument whose parser holds a partial line from an earlier
//! session answers the next command as an error, and an empty line clears it.
//! What the flush line got back is still returned (`flush_reply`), so a
//! poisoned parser is visible rather than silent.
//!
//! **Nothing is parsed.** A query's reply is the raw line, terminator removed,
//! any prefix the instrument adds kept: a value is exact or absent, never a
//! number this module guessed at.

use anyhow::{Context, Result};
use embarch_topology::hardware::instrument::judge_ack;
use embarch_topology::hardware::{Instrument, InstrumentExchange, InstrumentReply};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use crate::exchange::{exchange_with, write_with_deadline, MAX_READ_BYTES, MAX_TIMEOUT_MS, MAX_WRITE_BYTES, READ_POLL};

/// How long the flush line's answer is waited for. An instrument that says
/// nothing to an empty line costs this once per exchange.
pub const FLUSH_WAIT: Duration = Duration::from_millis(300);

/// Checks a request before `hw_lock` is taken: a caller error is a `400`.
pub fn check(req: &InstrumentExchange) -> Result<(), String> {
    if req.line.is_empty() {
        return Err("`line` is empty: an instrument exchange needs a command".to_string());
    }
    if req.line.contains(['\r', '\n']) {
        return Err("`line` holds a CR or LF: one command per exchange, and the instrument's own terminator is added".to_string());
    }
    if req.line.len() > MAX_WRITE_BYTES {
        return Err(format!("`line` is {} bytes; an exchange sends at most {MAX_WRITE_BYTES}", req.line.len()));
    }
    if req.timeout_ms == 0 || req.timeout_ms > MAX_TIMEOUT_MS {
        return Err(format!("timeout_ms={} is outside 1..={MAX_TIMEOUT_MS}", req.timeout_ms));
    }
    if req.port_wait_ms > MAX_TIMEOUT_MS {
        return Err(format!("port_wait_ms={} is above {MAX_TIMEOUT_MS}", req.port_wait_ms));
    }
    Ok(())
}

/// When each instrument's last exchange ended (not when it was last written),
/// process-wide: the route and a host study's steps share it, so pacing holds
/// across both.
fn last_sent() -> &'static Mutex<HashMap<String, Instant>> {
    static LAST: OnceLock<Mutex<HashMap<String, Instant>>> = OnceLock::new();
    LAST.get_or_init(|| Mutex::new(HashMap::new()))
}

/// How long until `min_interval` has passed since the last exchange with
/// `name` ended (the reply read, or the write flushed when nothing is read).
fn pace_left(name: &str, min_interval: Duration) -> Duration {
    match last_sent().lock().unwrap().get(name) {
        Some(at) => min_interval.saturating_sub(at.elapsed()),
        None => Duration::ZERO,
    }
}

/// Sleeps out the instrument's pacing and returns how long that was.
fn pace(name: &str, min_interval: Duration) -> Duration {
    let left = pace_left(name, min_interval);
    if !left.is_zero() {
        std::thread::sleep(left);
    }
    left
}

/// Stamps the end of an exchange with `name`: the instant pacing runs from.
fn mark_exchanged(name: &str) {
    last_sent().lock().unwrap().insert(name.to_string(), Instant::now());
}

/// What [`talk`] read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Talked {
    pub reply: String,
    pub terminated: bool,
    pub flush_reply: String,
    pub paced: Duration,
}

/// The reply line in `bytes`: everything before the first `terminator`, and
/// whether one was there.
fn reply_line(bytes: &[u8], terminator: &str) -> (String, bool) {
    let text = String::from_utf8_lossy(bytes);
    match text.find(terminator) {
        Some(i) => (text[..i].to_string(), true),
        None => (text.into_owned(), false),
    }
}

/// The whole conversation over any `Read + Write`, so every exit is testable
/// with no port: pace, the flush line, pace, the command, and its reply when
/// one is read (a query, or a set on an instrument that declares `ack_ok`).
pub(crate) fn talk<P: Read + Write>(
    port: &mut P,
    inst: &Instrument,
    req: &InstrumentExchange,
    mut set_timeout: impl FnMut(&mut P, Duration),
) -> Result<Talked> {
    let min = Duration::from_millis(inst.min_interval_ms as u64);
    let mut paced = pace(&inst.name, min);
    let rt = inst.read_terminator.as_str();
    let flush = exchange_with(port, inst.write_terminator.as_bytes(), Some(rt), false, FLUSH_WAIT, MAX_READ_BYTES, &mut set_timeout)
        .context("error sending the flush line: the command was not sent")?;
    mark_exchanged(&inst.name);
    let (flush_reply, _) = reply_line(&flush.bytes, rt);
    paced += pace(&inst.name, min);

    let mut line = req.line.clone().into_bytes();
    line.extend_from_slice(inst.write_terminator.as_bytes());
    let timeout = Duration::from_millis(req.timeout_ms);
    let (reply, terminated) = if req.query || inst.acknowledges() {
        let got = exchange_with(port, &line, Some(rt), false, timeout, MAX_READ_BYTES, &mut set_timeout)?;
        reply_line(&got.bytes, rt)
    } else {
        write_with_deadline(port, &line, Instant::now() + timeout, &mut set_timeout)?;
        port.flush().context("error flushing the instrument's port (the line was written, so it may have gone out)")?;
        (String::new(), false)
    };
    mark_exchanged(&inst.name);
    Ok(Talked { reply, terminated, flush_reply, paced })
}

/// Opens the instrument's port at its baud, drops pending input, and runs
/// [`talk`].
pub fn run(inst: &Instrument, port: &embarch_topology::hardware::DetectedPort, req: &InstrumentExchange) -> Result<InstrumentReply> {
    let started = Instant::now();
    let started_utc_ms = crate::exchange::utc_ms();
    let name = port.port_name.as_str();
    let mut sp = serialport::new(name, inst.baud)
        .timeout(READ_POLL)
        .open()
        .with_context(|| format!("failed to open instrument '{}' on {name} at {} baud: nothing was written", inst.name, inst.baud))?;
    // Best effort, as for a signal exchange: a bridge may not support it.
    let _ = sp.write_data_terminal_ready(true);
    let _ = sp.clear(serialport::ClearBuffer::Input);
    let t = talk(&mut sp, inst, req, |p, d| {
        let _ = p.set_timeout(d);
    })?;
    Ok(InstrumentReply {
        instrument: inst.name.clone(),
        port: name.to_string(),
        baud: inst.baud,
        line: req.line.clone(),
        query: req.query,
        ack: if req.query { None } else { judge_ack(inst, &t.reply) },
        reply: t.reply,
        terminated: t.terminated,
        flush_reply: t.flush_reply,
        paced_ms: t.paced.as_millis() as u64,
        elapsed_ms: started.elapsed().as_millis() as u64,
        started_utc_ms,
    })
}

/// The instrument's one live port, waiting up to `wait_ms` for it to be
/// enumerated. An identity matching several ports answers at once.
pub fn find_port_waiting(inst: &Instrument, wait_ms: u64) -> Result<Option<embarch_topology::hardware::DetectedPort>> {
    let deadline = Instant::now() + Duration::from_millis(wait_ms);
    loop {
        if let Some(p) = embarch_topology::hardware::instrument::find_port(inst)? {
            return Ok(Some(p));
        }
        if Instant::now() >= deadline {
            return Ok(None);
        }
        std::thread::sleep(Duration::from_millis(crate::exchange::PORT_POLL_MS));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use embarch_topology::hardware::{Ack, InstrumentProtocol};
    use std::collections::VecDeque;

    /// A fake instrument: each write that ends a line pops the next scripted
    /// answer into the read queue.
    struct Fake {
        answers: VecDeque<Vec<u8>>,
        reads: VecDeque<Vec<u8>>,
        wrote: Vec<u8>,
    }

    impl Fake {
        fn new(answers: &[&str]) -> Self {
            Fake { answers: answers.iter().map(|a| a.as_bytes().to_vec()).collect(), reads: VecDeque::new(), wrote: vec![] }
        }
    }

    impl Read for Fake {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            match self.reads.pop_front() {
                None => Err(std::io::ErrorKind::TimedOut.into()),
                Some(b) => {
                    buf[..b.len()].copy_from_slice(&b);
                    Ok(b.len())
                }
            }
        }
    }

    impl Write for Fake {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            self.wrote.extend_from_slice(b);
            if b.ends_with(b"\n") {
                if let Some(a) = self.answers.pop_front() {
                    if !a.is_empty() {
                        self.reads.push_back(a);
                    }
                }
            }
            Ok(b.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn load(name: &str, interval_ms: u32, acks: bool) -> Instrument {
        Instrument {
            name: name.into(),
            protocol: InstrumentProtocol::Scpi,
            port: "1a86:7523".parse().unwrap(),
            baud: 9600,
            write_terminator: "\n".into(),
            read_terminator: "\r\n".into(),
            min_interval_ms: interval_ms,
            ack_ok: acks.then(|| "Rexecu success".into()),
            ack_err: acks.then(|| "Rexecu err".into()),
        }
    }

    fn ask(line: &str, query: bool) -> InstrumentExchange {
        InstrumentExchange { line: line.into(), query, timeout_ms: 300, port_wait_ms: 0 }
    }

    #[test]
    fn a_query_sends_the_flush_line_then_the_command_and_returns_the_raw_line() {
        let inst = load("q1", 0, true);
        let mut port = Fake::new(&["Rcmd err\r\n", "R ET5420A+ V1.0\r\n"]);
        let t = talk(&mut port, &inst, &ask("*IDN?", true), |_, _| {}).unwrap();
        assert_eq!(String::from_utf8(port.wrote).unwrap(), "\n*IDN?\n");
        assert_eq!((t.reply.as_str(), t.terminated), ("R ET5420A+ V1.0", true));
        // The poisoned parser's answer is kept apart from the reply, not dropped silently.
        assert_eq!(t.flush_reply, "Rcmd err");
    }

    #[test]
    fn a_set_is_judged_by_the_declared_acks_and_unknown_answers_are_not_ok() {
        let inst = load("s1", 0, true);
        for (answer, want) in [
            ("Rexecu success\r\n", Ack::Ok),
            ("Rexecu err\r\n", Ack::Refused),
            ("Rcmd err\r\n", Ack::Unacknowledged),
            ("", Ack::Unacknowledged),
        ] {
            let mut port = Fake::new(&["", answer]);
            let t = talk(&mut port, &inst, &ask("CH1:SW ON", false), |_, _| {}).unwrap();
            assert_eq!(judge_ack(&inst, &t.reply), Some(want), "{answer:?}");
        }
    }

    #[test]
    fn a_set_on_an_instrument_with_no_acks_is_written_and_not_read() {
        let inst = load("s2", 0, false);
        let mut port = Fake::new(&["", "should not be read\r\n"]);
        let t = talk(&mut port, &inst, &ask("SYST:LOCA", false), |_, _| {}).unwrap();
        assert_eq!(String::from_utf8(port.wrote).unwrap(), "\nSYST:LOCA\n");
        assert_eq!((t.reply.as_str(), t.terminated), ("", false));
        assert_eq!(judge_ack(&inst, &t.reply), None);
    }

    #[test]
    fn a_query_that_gets_nothing_is_not_terminated() {
        let inst = load("q2", 0, false);
        let mut port = Fake::new(&["", ""]);
        let t = talk(&mut port, &inst, &ask("MEAS1:ALL?", true), |_, _| {}).unwrap();
        assert_eq!((t.reply.as_str(), t.terminated), ("", false));
    }

    #[test]
    fn commands_to_one_instrument_are_paced_by_its_min_interval() {
        let inst = load("paced", 120, true);
        let started = Instant::now();
        let mut port = Fake::new(&["", "Rexecu success\r\n", "", "Rexecu success\r\n"]);
        talk(&mut port, &inst, &ask("CH1:SW ON", false), |_, _| {}).unwrap();
        let t = talk(&mut port, &inst, &ask("CH1:SW OFF", false), |_, _| {}).unwrap();
        // Four writes, each at least 120 ms after the one before it.
        assert!(started.elapsed() >= Duration::from_millis(360), "{:?}", started.elapsed());
        assert!(t.paced >= Duration::from_millis(100), "{:?}", t.paced);
        // Another instrument is not held back by this one's pacing.
        assert!(pace_left("someone-else", Duration::from_millis(120)).is_zero());
    }

    #[test]
    fn a_request_is_checked_before_the_lock() {
        assert!(check(&ask("*IDN?", true)).is_ok());
        assert!(check(&ask("", true)).is_err());
        assert!(check(&ask("A\nB", false)).unwrap_err().contains("one command per exchange"));
        assert!(check(&InstrumentExchange { timeout_ms: 0, ..ask("x", true) }).is_err());
        assert!(check(&InstrumentExchange { timeout_ms: MAX_TIMEOUT_MS + 1, ..ask("x", true) }).is_err());
        assert!(check(&InstrumentExchange { port_wait_ms: MAX_TIMEOUT_MS + 1, ..ask("x", true) }).is_err());
    }
}
