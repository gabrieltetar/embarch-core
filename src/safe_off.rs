//! Core's safe-off (decision 156): every output the bench declares is switched
//! off at each start and at each graceful stop, from the declarations alone.
//!
//! An instrument's `safe_off` lines and a signal's one `safe_off` line
//! (`embarch-topology` decision 49) are sent with the line facts the
//! declaration carries: an instrument's own terminators, pacing and
//! acknowledgement through [`crate::instrument::run`], a signal's line through
//! the same function an `Exchange` step uses. **Nothing here builds a command**:
//! a line is sent as declared or not at all. A declaration with no line, a port
//! that is not enumerated, or a send that fails is recorded and the rest still
//! run, because one dead port must not leave another output on.
//!
//! A kill, a crash or a power loss runs none of this; the next start does.

use anyhow::Result;
use embarch_topology::hardware::{Instrument, SignalLink};
use serde::Serialize;

/// How long a safe-off line's acknowledgement or a signal's reply is waited for.
/// Chosen, not measured: a set command answers well inside it, and a stop must
/// not hang on a silent instrument.
pub const SAFE_OFF_TIMEOUT_MS: u64 = 1000;

/// What happened to one declaration (or one line of it), as it is entered in the
/// call log's `start` and `stop` entries.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SafeOffResult {
    /// `instrument`, `signal`, or `declarations` (the table itself could not be read).
    pub kind: &'static str,
    pub name: String,
    /// The line sent, or that failed; absent when the declaration has none.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line: Option<String>,
    /// One of [`SENT`], [`NONE_DECLARED`], [`PORT_ABSENT`], [`NOT_WRITABLE`], [`FAILED`].
    pub result: &'static str,
    /// What came back, for a [`SENT`] line (empty when the instrument is not read).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reply: Option<String>,
    /// Why, for anything but [`SENT`] and [`NONE_DECLARED`].
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

pub const SENT: &str = "sent";
pub const NONE_DECLARED: &str = "no safe-off declared";
pub const PORT_ABSENT: &str = "port absent";
pub const NOT_WRITABLE: &str = "not writable";
pub const FAILED: &str = "failed";

/// What one send came to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Sent {
    /// The line went out; the reply is what was read back.
    Reply(String),
    /// The declared port is not enumerated now.
    PortAbsent(String),
    Failed(String),
}

/// The bench as the safe-off sees it: the declarations and the two senders. A
/// seam so every branch runs with no port.
pub trait Bench: Send + Sync {
    fn instruments(&self) -> Result<Vec<Instrument>>;
    fn signals(&self) -> Result<Vec<SignalLink>>;
    fn send_instrument(&self, inst: &Instrument, line: &str) -> Sent;
    fn send_signal(&self, link: &SignalLink, line: &str) -> Sent;
}

fn result(kind: &'static str, name: &str, line: Option<&str>, result: &'static str) -> SafeOffResult {
    SafeOffResult { kind, name: name.to_string(), line: line.map(str::to_string), result, reply: None, error: None }
}

fn from_sent(kind: &'static str, name: &str, line: &str, sent: Sent) -> SafeOffResult {
    let mut r = result(kind, name, Some(line), SENT);
    match sent {
        Sent::Reply(reply) => r.reply = Some(reply),
        Sent::PortAbsent(e) => {
            r.result = PORT_ABSENT;
            r.error = Some(e);
        }
        Sent::Failed(e) => {
            r.result = FAILED;
            r.error = Some(e);
        }
    }
    r
}

/// Sends every declared safe-off, instruments first (declaration order), then
/// signals. Never fails: a table that cannot be read is one `declarations`
/// result, and the caller serves on (the fail-closed start is a later phase).
pub fn run(bench: &dyn Bench) -> Vec<SafeOffResult> {
    let mut out = Vec::new();
    match bench.instruments() {
        Err(e) => out.push(SafeOffResult { error: Some(format!("{e:#}")), ..result("declarations", "instruments", None, FAILED) }),
        Ok(list) => {
            for inst in &list {
                if inst.safe_off.is_empty() {
                    out.push(result("instrument", &inst.name, None, NONE_DECLARED));
                    continue;
                }
                for line in &inst.safe_off {
                    let r = from_sent("instrument", &inst.name, line, bench.send_instrument(inst, line));
                    let absent = r.result == PORT_ABSENT;
                    out.push(r);
                    // A port that is not there is not there for the next line either.
                    if absent {
                        break;
                    }
                }
            }
        }
    }
    match bench.signals() {
        Err(e) => out.push(SafeOffResult { error: Some(format!("{e:#}")), ..result("declarations", "signals", None, FAILED) }),
        Ok(list) => {
            for link in &list {
                let Some(line) = link.safe_off.as_deref() else {
                    out.push(result("signal", &link.name, None, NONE_DECLARED));
                    continue;
                };
                if !link.host_can_write() {
                    let why = "the signal's direction or route is not one Core writes (embarch-topology decision 37)".to_string();
                    out.push(SafeOffResult { error: Some(why), ..result("signal", &link.name, Some(line), NOT_WRITABLE) });
                    continue;
                }
                out.push(from_sent("signal", &link.name, line, bench.send_signal(link, line)));
            }
        }
    }
    out
}

/// The real bench: the declared instruments and signals, opened for real.
pub struct LiveBench {
    pub captures: crate::capture::CaptureRegistry,
}

impl Bench for LiveBench {
    fn instruments(&self) -> Result<Vec<Instrument>> {
        embarch_topology::hardware::instrument::list()
    }

    fn signals(&self) -> Result<Vec<SignalLink>> {
        embarch_topology::hardware::list_signals()
    }

    fn send_instrument(&self, inst: &Instrument, line: &str) -> Sent {
        let port = match crate::instrument::find_port_waiting(inst, 0) {
            Ok(Some(p)) => p,
            Ok(None) => return Sent::PortAbsent(format!("instrument '{}' ({}) is not enumerated now", inst.name, inst.port)),
            Err(e) => return Sent::Failed(format!("{e:#}")),
        };
        let req = embarch_topology::hardware::InstrumentExchange {
            line: line.to_string(),
            query: false,
            timeout_ms: SAFE_OFF_TIMEOUT_MS,
            port_wait_ms: 0,
        };
        match crate::instrument::run(inst, &port, &req) {
            Ok(reply) => Sent::Reply(reply.reply),
            Err(e) => Sent::Failed(format!("{e:#}")),
        }
    }

    fn send_signal(&self, link: &SignalLink, line: &str) -> Sent {
        // A capture on the signal holds its port; the exchange goes through the capture's handle.
        if self.captures.running(&link.name).is_none() {
            if let Err(e) = embarch_topology::hardware::resolve_signal_port(&link.name) {
                return Sent::PortAbsent(format!("{e:#}"));
            }
        }
        let req = crate::exchange::ExchangeRequest {
            write: format!("{line}\n"),
            until: None,
            timeout_ms: SAFE_OFF_TIMEOUT_MS,
            discard_pending: true,
            match_after_echo: true,
            port_wait_ms: 0,
        };
        match crate::api::run_signal_exchange(&self.captures, &link.name, req) {
            Ok(r) => Sent::Reply(r.text),
            Err((_, msg)) => Sent::Failed(msg),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use embarch_topology::hardware::{InstrumentProtocol, Route, SignalDirection};
    use std::sync::Mutex;

    #[derive(Default)]
    struct Fake {
        instruments: Vec<Instrument>,
        signals: Vec<SignalLink>,
        absent: Vec<String>,
        fail: Vec<String>,
        sent: Mutex<Vec<String>>,
    }

    impl Bench for Fake {
        fn instruments(&self) -> Result<Vec<Instrument>> {
            Ok(self.instruments.clone())
        }
        fn signals(&self) -> Result<Vec<SignalLink>> {
            Ok(self.signals.clone())
        }
        fn send_instrument(&self, inst: &Instrument, line: &str) -> Sent {
            self.send(&inst.name, line)
        }
        fn send_signal(&self, link: &SignalLink, line: &str) -> Sent {
            self.send(&link.name, line)
        }
    }

    impl Fake {
        fn send(&self, name: &str, line: &str) -> Sent {
            if self.absent.iter().any(|a| a == name) {
                return Sent::PortAbsent("not enumerated".into());
            }
            if self.fail.iter().any(|a| a == name) {
                return Sent::Failed("write failed".into());
            }
            self.sent.lock().unwrap().push(format!("{name}: {line}"));
            Sent::Reply("ok".into())
        }
    }

    fn inst(name: &str, lines: &[&str]) -> Instrument {
        Instrument {
            name: name.into(),
            protocol: InstrumentProtocol::Scpi,
            port: "1a86:7523".parse().unwrap(),
            baud: 9600,
            write_terminator: "\n".into(),
            read_terminator: "\r\n".into(),
            min_interval_ms: 0,
            ack_ok: None,
            ack_err: None,
            safe_off: lines.iter().map(|l| l.to_string()).collect(),
            metrology: Vec::new(),
            calibration: None,
        }
    }

    fn sig(name: &str, line: Option<&str>, direction: SignalDirection) -> SignalLink {
        SignalLink {
            name: name.into(),
            origin_role: String::new(),
            origin_board: Some("000123".into()),
            direction,
            route: Route::Direct { port_serial: "S".to_string(), interface: None },
            baud: None,
            safe_off: line.map(str::to_string),
        }
    }

    #[test]
    fn every_declared_line_is_sent_in_order_and_a_declaration_with_none_is_listed() {
        let bench = Fake {
            instruments: vec![inst("eload", &["INP OFF", "CURR 0"]), inst("dmm", &[])],
            signals: vec![sig("relay", Some("gpio clear 4"), SignalDirection::HostToDut), sig("scope", None, SignalDirection::HostToDut)],
            ..Default::default()
        };
        let got = run(&bench);
        assert_eq!(*bench.sent.lock().unwrap(), ["eload: INP OFF", "eload: CURR 0", "relay: gpio clear 4"]);
        let shape: Vec<_> = got.iter().map(|r| (r.kind, r.name.as_str(), r.result)).collect();
        assert_eq!(
            shape,
            [
                ("instrument", "eload", SENT),
                ("instrument", "eload", SENT),
                ("instrument", "dmm", NONE_DECLARED),
                ("signal", "relay", SENT),
                ("signal", "scope", NONE_DECLARED),
            ]
        );
        assert_eq!(got[0].reply.as_deref(), Some("ok"));
        assert_eq!(got[2].line, None);
    }

    #[test]
    fn an_absent_port_or_a_failed_send_is_recorded_and_the_rest_still_run() {
        let bench = Fake {
            instruments: vec![inst("gone", &["OUTP OFF", "VOLT 0"]), inst("bad", &["OUTP OFF"]), inst("psu", &["OUTP OFF"])],
            signals: vec![sig("relay", Some("gpio clear 4"), SignalDirection::HostToDut)],
            absent: vec!["gone".into()],
            fail: vec!["bad".into()],
            ..Default::default()
        };
        let got = run(&bench);
        // The absent instrument's second line is not tried: one entry, port absent.
        let shape: Vec<_> = got.iter().map(|r| (r.name.as_str(), r.result)).collect();
        assert_eq!(shape, [("gone", PORT_ABSENT), ("bad", FAILED), ("psu", SENT), ("relay", SENT)]);
        assert_eq!(got[1].error.as_deref(), Some("write failed"));
        assert_eq!(*bench.sent.lock().unwrap(), ["psu: OUTP OFF", "relay: gpio clear 4"]);
    }

    #[test]
    fn a_signal_core_does_not_write_is_listed_not_sent() {
        let bench = Fake { signals: vec![sig("rx-only", Some("gpio clear 4"), SignalDirection::DutToHost)], ..Default::default() };
        let got = run(&bench);
        assert_eq!((got[0].result, bench.sent.lock().unwrap().len()), (NOT_WRITABLE, 0));
    }

    #[test]
    fn a_table_that_cannot_be_read_is_one_failed_result_and_the_other_table_still_runs() {
        struct Broken;
        impl Bench for Broken {
            fn instruments(&self) -> Result<Vec<Instrument>> {
                anyhow::bail!("enrollment file is not TOML")
            }
            fn signals(&self) -> Result<Vec<SignalLink>> {
                Ok(vec![sig("relay", Some("gpio clear 4"), SignalDirection::HostToDut)])
            }
            fn send_instrument(&self, _: &Instrument, _: &str) -> Sent {
                unreachable!()
            }
            fn send_signal(&self, _: &SignalLink, _: &str) -> Sent {
                Sent::Reply(String::new())
            }
        }
        let got = run(&Broken);
        assert_eq!((got[0].kind, got[0].result), ("declarations", FAILED));
        assert!(got[0].error.as_deref().unwrap().contains("not TOML"));
        assert_eq!((got[1].name.as_str(), got[1].result), ("relay", SENT));
    }
}
