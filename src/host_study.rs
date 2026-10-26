//! `POST /host-studies`, `POST /study/{id}/cancel`: a study Core runs itself, one `Exchange` or
//! `Wait` step at a time (decision 95).
//!
//! **A host study is the same bytes as the exchange route, in a sequence.** An `Exchange` step
//! goes through [`crate::api::run_signal_exchange`], the body of `POST /signals/{name}/exchange`,
//! so a step and the route cannot disagree about what a signal accepts. Nothing here compares a
//! value: only "written", "marker seen" and "port present" decide an outcome, plus the one
//! refusal marker a step may declare (`refused_on`, `embarch-study-designer` decision 80).
//!
//! **Teardown runs on every exit** (the end, a failed step, a timeout, a cancel, a panic in a
//! step): [`execute`] runs `HostStudy.teardown` last, whatever stopped the steps, and ignores the
//! cancel flag while it does, so a cancelled run still returns the bench to its safe state.
//!
//! The executor is generic over [`HostBackend`] so every exit path is tested over a scripted
//! fake port; the real backend takes `hw_lock` per `Exchange` step.

use axum::extract::{Json, Path, State};
use axum::http::StatusCode;
use embarch_study_designer::{
    limits::{MAX_FAIL_REASON_LEN, MAX_HOST_REPLY_LEN},
    HostAction, HostList, HostStep, HostStepResult, HostStudy, Outcome,
};
use serde::Serialize;
use std::io::Write as _;
use std::path::{Path as FsPath, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant};
use tokio::sync::broadcast;

use crate::api::{internal_err, AppState};
use crate::exchange::{ExchangeRequest, ExchangeResponse};
use crate::study::{JobRegistry, StudyAcceptedResponse, StudyEvent, StudyJob};

/// What ends a step's wait or a `Wait`, polled this often so a cancel is seen within it.
const CANCEL_POLL: Duration = Duration::from_millis(20);

/// What a step's `detail` says, in one word each. `detail` is the machine-readable half of an
/// outcome: the same `Fail` or `TimedOut` has several causes a caller must tell apart.
pub mod detail {
    pub const MARKER_SEEN: &str = "marker_seen";
    /// The line was written and **zero bytes** came back: a console that said nothing.
    pub const EMPTY_REPLY: &str = "empty_reply";
    /// Bytes came back but `until` never showed before the timeout.
    pub const UNTIL_NOT_SEEN: &str = "until_not_seen";
    pub const REFUSED: &str = "refused";
    pub const PORT_LOST: &str = "port_lost";
    pub const PORT_UNAVAILABLE: &str = "port_unavailable";
    pub const EXCHANGE_FAILED: &str = "exchange_failed";
    pub const WAITED: &str = "waited";
    pub const CANCELLED: &str = "cancelled";
}

/// Why a backend could not run an exchange at all (nothing, or not all of the line, was written).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExchangeFailure {
    /// The signal's port could not be found or may not be written; the text names the ports seen.
    PortUnavailable(String),
    /// Anything else: the lock, an open or write error.
    Failed(String),
}

/// What an `Exchange` step runs against. The real one is [`CoreBackend`]; tests script a port.
pub trait HostBackend {
    fn exchange(&mut self, signal: &str, req: ExchangeRequest) -> Result<ExchangeResponse, ExchangeFailure>;
}

/// One step as the run recorded it: the study-designer result plus what Core saw.
#[derive(Debug, Clone, Serialize)]
pub struct StepRecord {
    #[serde(flatten)]
    pub result: HostStepResult,
    pub list: HostList,
    /// Position in `list`.
    pub index: u32,
    /// One of [`detail`]: why the outcome is what it is.
    pub detail: &'static str,
    /// Fuller text for a `Fail` whose 64-byte `reason` cannot hold it (the ports seen, the error).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    /// Bytes that came back after the write, before the reply cap. `0` for a `Wait`.
    pub reply_bytes: usize,
    pub until_seen: bool,
    pub echo_seen: bool,
    pub port_lost: bool,
    pub started_utc_ms: u64,
    pub ended_utc_ms: u64,
    pub delay_before_ms: u32,
}

/// How a run ended. `completed` is whether every step ran and none stopped the study.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunEnd {
    pub completed: bool,
    pub reason: Option<String>,
    /// Teardown steps that did not pass. They never stop the rest of the teardown.
    pub teardown_failures: u32,
}

fn now_ms() -> u64 {
    crate::exchange::utc_ms()
}

fn fail(reason: &str) -> Outcome {
    let mut end = reason.len().min(MAX_FAIL_REASON_LEN);
    while !reason.is_char_boundary(end) {
        end -= 1;
    }
    let mut s = heapless::String::new();
    let _ = s.push_str(&reason[..end]);
    Outcome::Fail { reason: s }
}

/// `text` cut at [`MAX_HOST_REPLY_LEN`] bytes on a character boundary, and whether it was cut.
pub fn cap_reply(text: &str) -> (String, bool) {
    if text.len() <= MAX_HOST_REPLY_LEN {
        return (text.to_string(), false);
    }
    let mut end = MAX_HOST_REPLY_LEN;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    (text[..end].to_string(), true)
}

/// Sleeps `ms`, polling `cancel` if given. Returns `false` if it was cancelled first.
fn sleep_for(ms: u64, cancel: Option<&AtomicBool>) -> bool {
    let end = Instant::now() + Duration::from_millis(ms);
    loop {
        if cancel.is_some_and(|c| c.load(Ordering::SeqCst)) {
            return false;
        }
        let left = end.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return true;
        }
        std::thread::sleep(left.min(CANCEL_POLL));
    }
}

/// The outcome of a reply. **Compares nothing but markers**: `refused_on` if the step declared
/// one, then whether `until` showed, then whether any byte came back.
fn judge(resp: &ExchangeResponse, refused_on: Option<&str>, timeout_ms: u32) -> (Outcome, &'static str, Option<String>) {
    if let Some(r) = refused_on {
        if resp.text.contains(r) {
            return (fail("refused: the reply holds the refusal marker"), detail::REFUSED, Some(format!("the reply contains '{r}'")));
        }
    }
    if resp.until_seen {
        return (Outcome::Pass, detail::MARKER_SEEN, None);
    }
    if resp.port_lost {
        return (
            fail("port lost after the line was written"),
            detail::PORT_LOST,
            Some(format!("the port went away after the write; {} bytes were read first; the command may have run", resp.bytes_read)),
        );
    }
    if resp.bytes_read == 0 {
        return (
            fail("empty reply: 0 bytes came back"),
            detail::EMPTY_REPLY,
            Some(format!("the line was written (echo not seen) and nothing came back within {timeout_ms} ms")),
        );
    }
    (
        Outcome::TimedOut,
        detail::UNTIL_NOT_SEEN,
        Some(format!("{} bytes came back but the marker did not show within {timeout_ms} ms", resp.bytes_read)),
    )
}

/// Runs one step to a record. `cancel` is `None` in teardown.
fn run_step<B: HostBackend>(
    step: &HostStep,
    list: HostList,
    index: u32,
    backend: &mut B,
    cancel: Option<&AtomicBool>,
) -> StepRecord {
    let started = now_ms();
    let t0 = Instant::now();
    let delayed = step.delay_before_ms == 0 || sleep_for(step.delay_before_ms as u64, cancel);
    let mut rec = StepRecord {
        result: HostStepResult {
            step_name: step.name.clone(),
            outcome: Outcome::Pass,
            reply: String::new(),
            reply_truncated: false,
            answered_by: None,
            elapsed_ms: 0,
            iteration: None,
        },
        list,
        index,
        detail: detail::WAITED,
        note: None,
        reply_bytes: 0,
        until_seen: false,
        echo_seen: false,
        port_lost: false,
        started_utc_ms: started,
        ended_utc_ms: started,
        delay_before_ms: step.delay_before_ms,
    };
    if !delayed {
        rec.result.outcome = fail("cancelled before the step ran");
        rec.detail = detail::CANCELLED;
    } else {
        match &step.action {
            HostAction::Wait { ms } => {
                if !sleep_for(*ms as u64, cancel) {
                    rec.result.outcome = fail("cancelled during the wait");
                    rec.detail = detail::CANCELLED;
                }
            }
            HostAction::Exchange { signal, line, until, match_after_echo, refused_on } => {
                let req = ExchangeRequest {
                    write: format!("{line}\n"),
                    until: Some(until.clone()),
                    timeout_ms: step.timeout_ms as u64,
                    discard_pending: true,
                    match_after_echo: *match_after_echo,
                    port_wait_ms: 0,
                };
                match backend.exchange(signal, req) {
                    Ok(resp) => {
                        let (outcome, d, note) = judge(&resp, refused_on.as_deref(), step.timeout_ms);
                        let (reply, cut) = cap_reply(&resp.text);
                        rec.result.outcome = outcome;
                        rec.result.reply = reply;
                        rec.result.reply_truncated = cut || resp.truncated;
                        rec.detail = d;
                        rec.note = note;
                        rec.reply_bytes = resp.bytes_read;
                        rec.until_seen = resp.until_seen;
                        rec.echo_seen = resp.echo_seen;
                        rec.port_lost = resp.port_lost;
                    }
                    Err(ExchangeFailure::PortUnavailable(msg)) => {
                        rec.result.outcome = fail("the signal's port is not available");
                        rec.detail = detail::PORT_UNAVAILABLE;
                        rec.note = Some(msg);
                    }
                    Err(ExchangeFailure::Failed(msg)) => {
                        rec.result.outcome = fail("the exchange failed before a reply");
                        rec.detail = detail::EXCHANGE_FAILED;
                        rec.note = Some(msg);
                    }
                }
            }
            // Refused at submission; reaching here is a bug, said as a failure rather than a skip.
            other => {
                rec.result.outcome = fail("this action is not executed by this Core");
                rec.detail = detail::EXCHANGE_FAILED;
                rec.note = Some(format!("{other:?}"));
            }
        }
    }
    rec.result.elapsed_ms = t0.elapsed().as_millis() as u64;
    rec.ended_utc_ms = now_ms();
    rec
}

/// Runs `study`: its steps until one stops it or `cancel` is set, then **always** its teardown.
/// `on_step` sees each record the moment it exists, steps and teardown alike.
pub fn execute<B: HostBackend>(
    study: &HostStudy,
    backend: &mut B,
    cancel: &AtomicBool,
    mut on_step: impl FnMut(StepRecord),
) -> RunEnd {
    use std::panic::{catch_unwind, AssertUnwindSafe};

    let mut end = RunEnd { completed: true, reason: None, teardown_failures: 0 };
    let main = catch_unwind(AssertUnwindSafe(|| {
        for (i, step) in study.steps.iter().enumerate() {
            if cancel.load(Ordering::SeqCst) {
                return Some(format!("cancelled before step '{}'", step.name));
            }
            let rec = run_step(step, HostList::Steps, i as u32, backend, Some(cancel));
            let (outcome, d) = (rec.result.outcome.clone(), rec.detail);
            on_step(rec);
            match outcome {
                Outcome::Pass => {}
                _ if d == detail::CANCELLED => return Some(format!("cancelled during step '{}'", step.name)),
                Outcome::Fail { reason } if !step.continue_on_fail => {
                    return Some(format!("step '{}' failed ({reason})", step.name));
                }
                Outcome::TimedOut if !step.continue_on_fail => {
                    return Some(format!("step '{}' timed out ({d})", step.name));
                }
                _ => {}
            }
        }
        None
    }));
    match main {
        Ok(None) => {}
        Ok(Some(why)) => {
            end.completed = false;
            end.reason = Some(why);
        }
        Err(_) => {
            end.completed = false;
            end.reason = Some("a step panicked".to_string());
        }
    }
    // Teardown: every step runs, the cancel flag is not consulted, and a failure is recorded
    // and walked past (`continue_on_fail` has no effect here).
    for (i, step) in study.teardown.iter().enumerate() {
        let rec = run_step(step, HostList::Teardown, i as u32, backend, None);
        if !matches!(rec.result.outcome, Outcome::Pass) {
            end.teardown_failures += 1;
        }
        on_step(rec);
    }
    end
}

// ---- submission checks ------------------------------------------------------

/// What this Core does not execute yet, by name. `Prompt` and `Instrument` are later units of the
/// host-study proposal; `Repeat` and `line_counts` are `core/116`'s; `streams` is accepted for `Text` + `WholeStudy` taps only.
fn unsupported(study: &HostStudy) -> Option<String> {
    let in_list = |steps: &[HostStep], list: &str| {
        steps.iter().enumerate().find_map(|(i, s)| {
            let what = match &s.action {
                HostAction::Prompt { .. } => "Prompt",
                HostAction::Instrument { .. } => "Instrument",
                HostAction::Repeat { .. } => "Repeat",
                HostAction::Exchange { .. } | HostAction::Wait { .. } => return None,
            };
            Some(format!("{list}[{i}] '{}' is a {what} step, which this Core does not execute yet (only Exchange and Wait)", s.name))
        })
    };
    in_list(&study.steps, "steps")
        .or_else(|| in_list(&study.teardown, "teardown"))
        .or_else(|| {
            study.streams.iter().find_map(|t| {
                if !matches!(t.encoding, embarch_study_designer::StreamEncoding::Text) {
                    Some(format!("streams[{}] '{}': only a `Text` tap is captured in a host study (decision 98)", t.id, t.name))
                } else if !matches!(t.scope, embarch_study_designer::StreamScope::WholeStudy) {
                    Some(format!("streams[{}] '{}': only a `WholeStudy` tap is captured in a host study (decision 98)", t.id, t.name))
                } else {
                    None
                }
            })
        })
        .or_else(|| {
            (!study.line_counts.is_empty())
                .then(|| "`line_counts` is not executed by this Core yet: counting is core/116's".to_string())
        })
}

/// Everything checkable with no topology and no lock: the study's own rules, then what this Core
/// executes, then each `Exchange` as the exchange route would check it. A caller error is a `400`.
pub fn check_study(study: &HostStudy) -> Result<(), String> {
    study.validate().map_err(|e| format!("invalid host study: {e:?}"))?;
    if let Some(msg) = unsupported(study) {
        return Err(msg);
    }
    for (list, steps) in [("steps", &study.steps), ("teardown", &study.teardown)] {
        for (i, s) in steps.iter().enumerate() {
            if let HostAction::Exchange { line, until, match_after_echo, .. } = &s.action {
                let req = ExchangeRequest {
                    write: format!("{line}\n"),
                    until: Some(until.clone()),
                    timeout_ms: s.timeout_ms as u64,
                    discard_pending: true,
                    match_after_echo: *match_after_echo,
                    port_wait_ms: 0,
                };
                crate::exchange::check(&req).map_err(|e| format!("{list}[{i}] '{}': {e}", s.name))?;
            }
        }
    }
    Ok(())
}

/// Every signal an `Exchange` names must be declared and writable now. Reads enrollment.
fn check_signals(study: &HostStudy) -> Result<(), String> {
    for (list, steps) in [("steps", &study.steps), ("teardown", &study.teardown)] {
        for (i, s) in steps.iter().enumerate() {
            let HostAction::Exchange { signal, .. } = &s.action else { continue };
            let link = embarch_topology::hardware::find_signal(signal)
                .map_err(|e| format!("could not read the signal table: {e:#}"))?
                .ok_or_else(|| format!("{list}[{i}] '{}': no signal is declared under the name '{signal}'", s.name))?;
            if !link.host_can_write() {
                return Err(format!(
                    "{list}[{i}] '{}': signal '{signal}' is not a direct host-to-dut or bidirectional signal Core may write (embarch-topology decision 37)",
                    s.name
                ));
            }
        }
    }
    Ok(())
}

// ---- the record on disk -----------------------------------------------------

/// `events.json` for a host study: written step by step to `.partial`, renamed at the end, so a
/// Core killed mid-run leaves a `.partial` that `GET /studies` calls `interrupted`. It carries
/// the same `study_name` and `steps` keys a bench study's does, so `GET /study/{id}/steps` and
/// the listing read it unchanged.
pub struct HostEventsWriter {
    file: std::io::BufWriter<std::fs::File>,
    partial: PathBuf,
    done: PathBuf,
    wrote_any: bool,
}

impl HostEventsWriter {
    pub fn start(dir: &FsPath, study_name: &str) -> anyhow::Result<Self> {
        let partial = dir.join("events.json.partial");
        let mut file = std::io::BufWriter::new(std::fs::File::create(&partial)?);
        write!(file, "{{\"kind\":\"host\",\"study_name\":{},\"steps\":[", serde_json::to_string(study_name)?)?;
        file.flush()?;
        Ok(Self { file, partial, done: dir.join("events.json"), wrote_any: false })
    }

    pub fn step(&mut self, rec: &StepRecord) -> anyhow::Result<()> {
        if self.wrote_any {
            write!(self.file, ",")?;
        }
        serde_json::to_writer(&mut self.file, rec)?;
        self.wrote_any = true;
        // Flushed per step: a record is durable the instant the step ends (the bench writer's posture).
        self.file.flush()?;
        Ok(())
    }

    pub fn finish(mut self, end: &RunEnd, streams: &[serde_json::Value]) -> anyhow::Result<()> {
        write!(
            self.file,
            "],\"completed\":{},\"stopped_reason\":{},\"teardown_failures\":{},\"streams\":{}}}",
            end.completed,
            serde_json::to_string(&end.reason)?,
            end.teardown_failures,
            serde_json::to_string(streams)?
        )?;
        self.file.flush()?;
        drop(self.file);
        std::fs::rename(&self.partial, &self.done)?;
        Ok(())
    }
}

// ---- signal taps (decision 98) -----------------------------------------------

/// How long a study's tap may run: six hours. **Reasoned, not measured**: the field's longest
/// run was minutes; the cap only bounds a forgotten tap, as [`crate::capture::MAX_S`] does.
const TAP_MAX_S: u64 = 6 * 3600;

/// What a tap's signal resolves to: the carrier, found live from topology.
pub struct TapTarget {
    pub port_name: String,
    pub baud: u32,
    pub opener: crate::capture::Opener,
    pub identity: Option<crate::exchange::PortIdentity>,
}

type TapError = (StatusCode, String);

/// The real resolver: the signal must be declared and direct (a via-dev-bench route arrives over
/// dev-bench's link and has no port for Core to own).
fn resolve_tap(signal: &str) -> Result<TapTarget, TapError> {
    let link = embarch_topology::hardware::find_signal(signal)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("could not read the signal table: {e:#}")))?
        .ok_or_else(|| (StatusCode::BAD_REQUEST, format!("no signal is declared under the name '{signal}'")))?;
    if matches!(link.route, embarch_topology::hardware::Route::ViaDevBench { .. }) {
        return Err((StatusCode::BAD_REQUEST, format!("signal '{signal}' is a via-dev-bench route; Core taps only a direct one in a host study")));
    }
    let port = embarch_topology::hardware::resolve_signal_port(signal).map_err(|e| (StatusCode::CONFLICT, format!("{e:#}")))?;
    let baud = crate::stream_store::signal_baud_for(&link);
    let name = signal.to_string();
    let opener: crate::capture::Opener = Arc::new(move || {
        let p = embarch_topology::hardware::resolve_signal_port(&name)?;
        let mut sp = serialport::new(p.port_name.as_str(), baud).timeout(crate::exchange::READ_POLL).open()?;
        let _ = sp.write_data_terminal_ready(true);
        Ok(Box::new(sp) as Box<dyn crate::capture::CapturePort>)
    });
    let identity = Some(crate::exchange::PortIdentity::of(&port));
    Ok(TapTarget { port_name: port.port_name, baud, opener, identity })
}

struct LiveTap {
    id: u8,
    name: String,
    signal: String,
    port: String,
    cap: Arc<crate::capture::Capture>,
}

/// A host study's signal taps: one [`crate::capture::Capture`] per tap, opened once at the start
/// and held until the teardown ends. An `Exchange` on a tapped signal finds the capture in the
/// registry (`api::run_signal_exchange`) and runs through its open port, so the port is opened
/// once and nothing the console prints between steps is lost. Every byte it reads lands in
/// `streams/<name>.txt`, one record per line, with Core's receive time in `<name>.arrival.csv`
/// on one monotonic clock shared by all the study's taps. Dropping it stops the captures.
pub struct StudyTaps {
    captures: crate::capture::CaptureRegistry,
    live: Vec<LiveTap>,
    store: Arc<StdMutex<crate::stream_store::StreamStore>>,
}

impl StudyTaps {
    /// Opens every tap's port, `None` when the study declares none. Nothing is half-started: a
    /// failure stops the taps already open. `409` for a port a capture or another tap holds.
    pub fn start(
        captures: &crate::capture::CaptureRegistry,
        tap_ports: &crate::study::TapPorts,
        dir: &FsPath,
        study: &HostStudy,
        resolve: &dyn Fn(&str) -> Result<TapTarget, TapError>,
    ) -> Result<Option<Self>, TapError> {
        if study.streams.is_empty() {
            return Ok(None);
        }
        let store = crate::stream_store::StreamStore::create(dir, &study.streams, &[], crate::stream_store::stream_max_bytes())
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("could not create the study's streams/: {e:#}")))?;
        let mut taps = StudyTaps { captures: captures.clone(), live: vec![], store: Arc::new(StdMutex::new(store)) };
        // One anchor for the whole study: a tap's stamp is this instant plus the monotonic time
        // since, so two consoles' lines order against each other without wall-clock steps.
        let epoch = Instant::now();
        let epoch_utc = now_ms();
        for tap in &study.streams {
            let embarch_study_designer::StreamSource::Signal { name: signal } = &tap.source else { continue };
            let signal = signal.as_str();
            let target = resolve(signal)?;
            if tap_ports.holds(&target.port_name) {
                return Err((StatusCode::CONFLICT, format!("{} is held by a running study's tap", target.port_name)));
            }
            let id = tap.id;
            let store = taps.store.clone();
            let sink: crate::capture::Sink = Arc::new(move |bytes: &[u8]| {
                let stamp = epoch_utc + epoch.elapsed().as_millis() as u64;
                let mut st = store.lock().unwrap();
                // A record per line, so the arrival log gives each its own row.
                for piece in bytes.split_inclusive(|b| *b == b'\n') {
                    st.write_raw(id, piece);
                    st.note_arrival(id, piece, stamp, stamp);
                }
            });
            let claim = tap_ports.claim(&target.port_name);
            let cap = captures
                .start_with_sink(
                    signal,
                    &target.port_name,
                    target.baud,
                    TAP_MAX_S,
                    crate::capture::MAX_BYTES,
                    target.identity,
                    target.opener,
                    Box::new(claim),
                    Some(sink),
                )
                .map_err(|e| match e {
                    crate::capture::StartError::Conflict(m) | crate::capture::StartError::Open(m) => (StatusCode::CONFLICT, m),
                })?;
            taps.live.push(LiveTap { id, name: tap.name.to_string(), signal: signal.to_string(), port: target.port_name, cap });
        }
        Ok(Some(taps))
    }

    /// Stops every capture and waits (briefly) for its thread, so the ports are closed when
    /// this returns.
    fn shutdown(&mut self) {
        for t in &self.live {
            t.cap.stop();
        }
        let end = Instant::now() + Duration::from_secs(2);
        while self.live.iter().any(|t| t.cap.state().running()) && Instant::now() < end {
            std::thread::sleep(Duration::from_millis(10));
        }
        for t in &self.live {
            if self.captures.get(&t.signal).is_some_and(|c| c.id == t.cap.id) {
                self.captures.remove(&t.signal);
            }
        }
    }

    /// Ends the taps and returns each one's entry for `events.json`'s `streams`: what it wrote,
    /// how it ended (`stopped` is the normal end; `expired` or `byte_cap` means it stopped
    /// early and a later exchange opened the port itself) and any gap where the port was lost.
    pub fn finish(mut self) -> Vec<serde_json::Value> {
        self.shutdown();
        let refs = self.store.lock().unwrap().refs();
        self.live
            .iter()
            .map(|t| {
                let read = t.cap.read(0, 0);
                let r = refs.iter().find(|r| r.name.as_str() == t.name);
                serde_json::json!({
                    "id": t.id,
                    "name": t.name,
                    "signal": t.signal,
                    "port": t.port,
                    "state": read.state,
                    "bytes_written": r.map(|r| r.bytes_written).unwrap_or(0),
                    "truncated": r.map(|r| r.truncated).unwrap_or(false) || read.state != crate::capture::State::Stopped,
                    "gaps": read.gaps,
                })
            })
            .collect()
    }
}

impl Drop for StudyTaps {
    fn drop(&mut self) {
        self.shutdown();
    }
}

// ---- the job ----------------------------------------------------------------

/// The cancel flag of the host study in flight, by id. One at a time, like `study_lock`.
pub type HostCancel = Arc<StdMutex<Option<(String, Arc<AtomicBool>)>>>;

/// Runs a host study as a job: events, the record on disk, the job registry. Blocking.
#[allow(clippy::too_many_arguments)]
pub fn run_job<B: HostBackend>(
    study: &HostStudy,
    study_id: &str,
    dir: &FsPath,
    backend: &mut B,
    cancel: &AtomicBool,
    jobs: &JobRegistry,
    events_tx: &broadcast::Sender<StudyEvent>,
    taps: Option<StudyTaps>,
) {
    let mut writer = match HostEventsWriter::start(dir, &study.name) {
        Ok(w) => w,
        Err(e) => {
            crate::study::fail_job(jobs, events_tx, study_id, format!("could not create the study's record: {e:#}"));
            return;
        }
    };
    let mut write_err: Option<String> = None;
    let end = execute(study, backend, cancel, |rec| {
        if let Err(e) = writer.step(&rec) {
            write_err.get_or_insert(format!("{e:#}"));
        }
        if rec.list == HostList::Steps {
            let idx = rec.index;
            crate::study::update_job(jobs, study_id, |j| j.current_step = Some(idx));
        }
        let _ = events_tx.send(StudyEvent::HostStepCompleted {
            study_id: study_id.to_string(),
            list: rec.list,
            step_index: rec.index,
            started_utc_ms: rec.started_utc_ms,
            ended_utc_ms: rec.ended_utc_ms,
            delay_before_ms: rec.delay_before_ms,
            detail: rec.detail.to_string(),
            result: Box::new(rec.result),
        });
    });
    // After the teardown, so a console's last words are held; dropping `taps` on an early
    // return below stops them too.
    let streams = taps.map(StudyTaps::finish).unwrap_or_default();
    if let Some(e) = write_err {
        crate::study::fail_job(jobs, events_tx, study_id, format!("could not write the study's record: {e}"));
        return;
    }
    if let Err(e) = writer.finish(&end, &streams) {
        crate::study::fail_job(jobs, events_tx, study_id, format!("failed to finalize events.json: {e:#}"));
        return;
    }
    let reason = match (&end.reason, end.teardown_failures) {
        (Some(r), 0) => Some(r.clone()),
        (Some(r), n) => Some(format!("{r}; {n} teardown step(s) did not pass")),
        (None, 0) => None,
        (None, n) => Some(format!("{n} teardown step(s) did not pass")),
    };
    crate::study::update_job(jobs, study_id, |j| {
        j.status = "completed".to_string();
        j.reason = reason.clone();
    });
    let _ = events_tx.send(StudyEvent::StatusChanged { study_id: study_id.to_string(), status: "completed".to_string(), reason });
}

/// The real backend: `hw_lock` for each `Exchange` step, then the exchange route's own body.
struct CoreBackend {
    state: AppState,
    handle: tokio::runtime::Handle,
}

impl HostBackend for CoreBackend {
    fn exchange(&mut self, signal: &str, req: ExchangeRequest) -> Result<ExchangeResponse, ExchangeFailure> {
        let _guard = self
            .handle
            .block_on(crate::api::acquire_hw_lock(&self.state, "POST /host-studies"))
            .map_err(|(_, msg)| ExchangeFailure::Failed(msg))?;
        crate::api::run_signal_exchange(&self.state.captures, signal, req).map_err(|(status, msg)| match status {
            StatusCode::CONFLICT | StatusCode::NOT_FOUND => ExchangeFailure::PortUnavailable(msg),
            _ => ExchangeFailure::Failed(msg),
        })
    }
}

/// Releases the study lock and the cancel slot when the job's thread ends, however it ends.
struct JobHold {
    lock: crate::study::StudyLock,
    cancel: HostCancel,
}

impl Drop for JobHold {
    fn drop(&mut self) {
        *self.cancel.lock().unwrap() = None;
        *self.lock.lock().unwrap() = None;
    }
}

/// `POST /host-studies`: check, take `study_lock` (`409` while any study holds it), check the
/// signals against topology, then run on one executor thread and answer `202` with the id.
// route: POST /host-studies
pub async fn post_host_study_handler(
    State(state): State<AppState>,
    Json(study): Json<HostStudy>,
) -> Result<(StatusCode, Json<StudyAcceptedResponse>), (StatusCode, String)> {
    check_study(&study).map_err(|msg| (StatusCode::BAD_REQUEST, msg))?;

    let study_id = crate::study::generate_study_id();
    crate::study::take_study_lock(&state, &study_id)?;
    let hold = JobHold { lock: state.study_lock.clone(), cancel: state.host_cancel.clone() };

    let for_signals = study.clone();
    match tokio::task::spawn_blocking(move || check_signals(&for_signals)).await {
        Ok(Ok(())) => {}
        Ok(Err(msg)) => return Err((StatusCode::BAD_REQUEST, msg)),
        Err(e) => return Err(internal_err(e)),
    }
    let dir = crate::study::study_results_dir(&study_id).map_err(internal_err)?;
    std::fs::create_dir_all(&dir).map_err(internal_err)?;
    let taps = {
        let (st, s, d) = (state.clone(), study.clone(), dir.clone());
        tokio::task::spawn_blocking(move || StudyTaps::start(&st.captures, &st.tap_ports, &d, &s, &resolve_tap))
            .await
            .map_err(internal_err)??
    };

    let cancel = Arc::new(AtomicBool::new(false));
    *state.host_cancel.lock().unwrap() = Some((study_id.clone(), cancel.clone()));
    state.study_jobs.lock().unwrap().insert(
        study_id.clone(),
        StudyJob { status: "running".to_string(), current_step: None, total_steps: Some(study.steps.len() as u32), reason: None },
    );

    let jobs = state.study_jobs.clone();
    let events_tx = state.study_events.clone();
    let id = study_id.clone();
    let handle = tokio::runtime::Handle::current();
    tokio::spawn(async move {
        let jobs_for_panic = jobs.clone();
        let tx_for_panic = events_tx.clone();
        let id_for_panic = id.clone();
        let outcome = tokio::task::spawn_blocking(move || {
            let _hold = hold;
            let mut backend = CoreBackend { state, handle };
            run_job(&study, &id, &dir, &mut backend, &cancel, &jobs, &events_tx, taps);
        })
        .await;
        if let Err(e) = outcome {
            crate::study::fail_job(&jobs_for_panic, &tx_for_panic, &id_for_panic, format!("host study task panicked: {e:?}"));
        }
    });

    Ok((StatusCode::ACCEPTED, Json(StudyAcceptedResponse { study_id, status: "accepted" })))
}

#[derive(Debug, Serialize)]
pub struct CancelResponse {
    study_id: String,
    status: &'static str,
}

/// `POST /study/{id}/cancel`: asks the running host study to stop. `202` once the flag is set:
/// the step in flight finishes (an exchange cannot be interrupted, so within its `timeout_ms`),
/// the remaining steps are skipped and the teardown runs. `409` for a study that is not a running
/// host study, `404` for an id Core has never seen.
// route: POST /study/{study_id}/cancel
pub async fn cancel_study_handler(
    State(state): State<AppState>,
    Path(study_id): Path<String>,
) -> Result<(StatusCode, Json<CancelResponse>), (StatusCode, String)> {
    if let Some((id, flag)) = state.host_cancel.lock().unwrap().as_ref() {
        if *id == study_id {
            flag.store(true, Ordering::SeqCst);
            return Ok((StatusCode::ACCEPTED, Json(CancelResponse { study_id, status: "cancelling" })));
        }
    }
    let job = state.study_jobs.lock().unwrap().get(&study_id).cloned();
    match job {
        Some(j) if j.status == "running" => Err((
            StatusCode::CONFLICT,
            format!("study '{study_id}' is a running bench study; only a host study can be cancelled"),
        )),
        Some(j) => Err((StatusCode::CONFLICT, format!("study '{study_id}' is not running (status: {}); nothing to cancel", j.status))),
        None => Err((StatusCode::NOT_FOUND, format!("no study '{study_id}' is known to this Core"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::io::{Read, Write};

    /// A scripted console: each `exchange` call pops the next scripted reply (bytes to read, or
    /// a failure), runs the real `exchange_with` over a fake port, and records what was written.
    struct Script {
        replies: VecDeque<Result<Vec<u8>, ExchangeFailure>>,
        written: Vec<String>,
        /// Called with the count of exchanges so far, before each runs (to cancel mid-run).
        before: Box<dyn FnMut(usize)>,
        calls: usize,
    }

    struct Port {
        reads: VecDeque<Vec<u8>>,
        wrote: Vec<u8>,
    }

    impl Read for Port {
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

    impl Write for Port {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            self.wrote.extend_from_slice(b);
            Ok(b.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl Script {
        fn new(replies: Vec<Result<Vec<u8>, ExchangeFailure>>) -> Self {
            Script { replies: replies.into(), written: vec![], before: Box::new(|_| {}), calls: 0 }
        }
    }

    impl HostBackend for Script {
        fn exchange(&mut self, signal: &str, req: ExchangeRequest) -> Result<ExchangeResponse, ExchangeFailure> {
            (self.before)(self.calls);
            self.calls += 1;
            let reply = self.replies.pop_front().unwrap_or(Ok(vec![]))?;
            let mut port = Port { reads: VecDeque::from([reply]), wrote: vec![] };
            let got = crate::exchange::exchange_with(
                &mut port,
                req.write.as_bytes(),
                req.until.as_deref(),
                req.match_after_echo,
                Duration::from_millis(req.timeout_ms),
                1 << 16,
                |_, _| {},
            )
            .map_err(|e| ExchangeFailure::Failed(format!("{e:#}")))?;
            self.written.push(String::from_utf8(port.wrote).unwrap());
            Ok(crate::exchange::response(signal, "FAKE", 115_200, 1, 1, got, || None, || crate::exchange::CoreState::unavailable("test")))
        }
    }

    fn ex(name: &str, line: &str, until: &str) -> HostStep {
        HostStep {
            name: name.into(),
            action: HostAction::Exchange { signal: "pd".into(), line: line.into(), until: until.into(), match_after_echo: true, refused_on: None },
            timeout_ms: 80,
            continue_on_fail: false,
            delay_before_ms: 0,
        }
    }

    fn wait(name: &str, ms: u32) -> HostStep {
        HostStep { name: name.into(), action: HostAction::Wait { ms }, timeout_ms: 0, continue_on_fail: false, delay_before_ms: 0 }
    }

    fn study(steps: Vec<HostStep>, teardown: Vec<HostStep>) -> HostStudy {
        HostStudy { name: "t".into(), steps, teardown, streams: vec![], line_counts: vec![] }
    }

    fn run(s: &HostStudy, b: &mut Script, cancel: &AtomicBool) -> (RunEnd, Vec<StepRecord>) {
        let mut recs = vec![];
        let end = execute(s, b, cancel, |r| recs.push(r));
        (end, recs)
    }

    fn ok_reply(line: &str, until: &str) -> Result<Vec<u8>, ExchangeFailure> {
        Ok(format!("{line}\r\nresult\r\n{until}").into_bytes())
    }

    #[test]
    fn a_clean_run_passes_every_step_then_runs_the_teardown() {
        let s = study(vec![ex("a", "pd sink 5", "pd: "), wait("w", 5)], vec![ex("safe", "pd sink 5", "pd: ")]);
        let mut b = Script::new(vec![ok_reply("pd sink 5", "pd: ok"), ok_reply("pd sink 5", "pd: ok")]);
        let (end, recs) = run(&s, &mut b, &AtomicBool::new(false));
        assert!(end.completed && end.reason.is_none() && end.teardown_failures == 0);
        assert_eq!(recs.iter().map(|r| (r.list, r.index)).collect::<Vec<_>>(), [(HostList::Steps, 0), (HostList::Steps, 1), (HostList::Teardown, 0)]);
        assert!(recs.iter().all(|r| r.result.outcome == Outcome::Pass));
        assert_eq!(b.written, ["pd sink 5\n", "pd sink 5\n"]);
        assert!(recs[0].result.reply.contains("result") && recs[0].reply_bytes > 0 && recs[0].until_seen);
    }

    #[test]
    fn teardown_runs_after_a_failed_step_and_the_rest_is_skipped() {
        let s = study(
            vec![ex("a", "x", "pd: "), ex("b", "y", "pd: "), ex("never", "z", "pd: ")],
            vec![ex("safe", "pd sink 5", "pd: ")],
        );
        let mut b = Script::new(vec![
            ok_reply("x", "pd: ok"),
            Err(ExchangeFailure::PortUnavailable("COM9 not found; ports seen: COM3".into())),
            ok_reply("pd sink 5", "pd: ok"),
        ]);
        let (end, recs) = run(&s, &mut b, &AtomicBool::new(false));
        assert!(!end.completed);
        assert!(end.reason.unwrap().contains("step 'b' failed"));
        assert_eq!(recs.len(), 3, "a, b, then the teardown; 'never' is skipped");
        assert_eq!(recs[1].detail, detail::PORT_UNAVAILABLE);
        assert!(recs[1].note.as_deref().unwrap().contains("ports seen: COM3"));
        assert_eq!(recs[2].list, HostList::Teardown);
        assert_eq!(b.written.last().unwrap(), "pd sink 5\n");
    }

    #[test]
    fn teardown_runs_after_a_timeout() {
        let s = study(vec![ex("a", "x", "pd: ")], vec![ex("safe", "pd sink 5", "pd: ")]);
        // Bytes come back but the marker never does: a timeout, not an empty reply.
        let mut b = Script::new(vec![Ok(b"x\r\nsome output\r\n".to_vec()), ok_reply("pd sink 5", "pd: ok")]);
        let (end, recs) = run(&s, &mut b, &AtomicBool::new(false));
        assert_eq!(recs[0].result.outcome, Outcome::TimedOut);
        assert_eq!(recs[0].detail, detail::UNTIL_NOT_SEEN);
        assert!(!end.completed && end.reason.unwrap().contains("timed out"));
        assert_eq!(recs[1].list, HostList::Teardown);
        assert_eq!(recs[1].result.outcome, Outcome::Pass);
    }

    #[test]
    fn teardown_runs_after_a_cancel_between_steps_and_ignores_the_flag() {
        let s = study(vec![ex("a", "x", "pd: "), ex("b", "y", "pd: ")], vec![ex("safe1", "s1", "pd: "), ex("safe2", "s2", "pd: ")]);
        let cancel = Arc::new(AtomicBool::new(false));
        let mut b = Script::new(vec![ok_reply("x", "pd: ok"), ok_reply("s1", "pd: ok"), ok_reply("s2", "pd: ok")]);
        let c = cancel.clone();
        // Cancel arrives while step 'a' is in flight (before its exchange returns).
        b.before = Box::new(move |n| {
            if n == 0 {
                c.store(true, Ordering::SeqCst);
            }
        });
        let (end, recs) = run(&s, &mut b, &cancel);
        assert!(!end.completed && end.reason.unwrap().contains("cancelled before step 'b'"));
        // 'a' finished (an exchange is not interrupted), 'b' did not run, both teardown steps did.
        assert_eq!(recs.iter().map(|r| r.result.step_name.as_str()).collect::<Vec<_>>(), ["a", "safe1", "safe2"]);
        assert!(recs[1..].iter().all(|r| r.result.outcome == Outcome::Pass));
        assert_eq!(b.written, ["x\n", "s1\n", "s2\n"]);
    }

    #[test]
    fn a_cancel_during_a_wait_stops_the_wait_early_and_runs_the_teardown() {
        let s = study(vec![wait("long", 5_000), ex("never", "y", "pd: ")], vec![ex("safe", "s", "pd: ")]);
        let cancel = Arc::new(AtomicBool::new(false));
        let c = cancel.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(60));
            c.store(true, Ordering::SeqCst);
        });
        let mut b = Script::new(vec![ok_reply("s", "pd: ok")]);
        let t = Instant::now();
        let (end, recs) = run(&s, &mut b, &cancel);
        assert!(t.elapsed() < Duration::from_secs(2), "{:?}", t.elapsed());
        assert!(!end.completed && end.reason.unwrap().contains("cancelled during step 'long'"));
        assert_eq!(recs[0].detail, detail::CANCELLED);
        assert_eq!(recs[1].list, HostList::Teardown);
        assert_eq!(recs[1].result.outcome, Outcome::Pass);
    }

    #[test]
    fn a_zero_byte_reply_is_its_own_detail_apart_from_a_marker_that_never_showed() {
        let s = study(vec![HostStep { continue_on_fail: true, ..ex("quiet", "x", "pd: ") }, HostStep { continue_on_fail: true, ..ex("chatty", "y", "pd: ") }], vec![]);
        let mut b = Script::new(vec![Ok(vec![]), Ok(b"y\r\nnoise\r\n".to_vec())]);
        let (end, recs) = run(&s, &mut b, &AtomicBool::new(false));
        assert!(end.completed, "continue_on_fail walks past both");
        assert_eq!((recs[0].detail, recs[0].reply_bytes), (detail::EMPTY_REPLY, 0));
        assert!(matches!(recs[0].result.outcome, Outcome::Fail { .. }));
        assert_eq!(recs[1].detail, detail::UNTIL_NOT_SEEN);
        assert_eq!(recs[1].result.outcome, Outcome::TimedOut);
        assert!(recs[1].reply_bytes > 0);
    }

    #[test]
    fn only_markers_decide_a_value_in_the_reply_changes_nothing() {
        let s = study(vec![ex("a", "x", "pd: ")], vec![]);
        for volts in ["5", "20", "-1", "garbage"] {
            let mut b = Script::new(vec![Ok(format!("x\r\nvbus {volts} V\r\npd: ").into_bytes())]);
            let (end, recs) = run(&s, &mut b, &AtomicBool::new(false));
            assert!(end.completed, "{volts}");
            assert_eq!(recs[0].result.outcome, Outcome::Pass);
        }
        // The one declared refusal marker is the exception, and it is a marker, not a value.
        let refusing = study(vec![HostStep { action: HostAction::Exchange { signal: "pd".into(), line: "x".into(), until: "pd: ".into(), match_after_echo: true, refused_on: Some("rejected".into()) }, ..ex("a", "x", "pd: ") }], vec![]);
        let mut b = Script::new(vec![Ok(b"x\r\npd: rejected\r\n".to_vec())]);
        let (end, recs) = run(&refusing, &mut b, &AtomicBool::new(false));
        assert!(!end.completed);
        assert_eq!(recs[0].detail, detail::REFUSED);
    }

    #[test]
    fn a_failing_teardown_step_is_recorded_and_the_rest_still_run() {
        let mut bad = ex("t1", "a", "pd: ");
        bad.continue_on_fail = false;
        let s = study(vec![ex("a", "x", "pd: ")], vec![bad, ex("t2", "b", "pd: ")]);
        let mut b = Script::new(vec![ok_reply("x", "pd: ok"), Err(ExchangeFailure::Failed("boom".into())), ok_reply("b", "pd: ok")]);
        let (end, recs) = run(&s, &mut b, &AtomicBool::new(false));
        assert!(end.completed && end.teardown_failures == 1);
        assert_eq!(recs.len(), 3);
        assert_eq!(recs[1].detail, detail::EXCHANGE_FAILED);
        assert_eq!(recs[2].result.outcome, Outcome::Pass);
    }

    #[test]
    fn a_panicking_step_still_runs_the_teardown() {
        let s = study(vec![ex("a", "x", "pd: ")], vec![ex("safe", "s", "pd: ")]);
        let mut b = Script::new(vec![ok_reply("s", "pd: ok")]);
        let mut armed = true;
        b.before = Box::new(move |_| {
            if std::mem::take(&mut armed) {
                panic!("backend bug");
            }
        });
        let (end, recs) = run(&s, &mut b, &AtomicBool::new(false));
        assert!(!end.completed && end.reason.unwrap().contains("panicked"));
        assert_eq!(recs.len(), 1);
        assert_eq!(recs[0].list, HostList::Teardown);
        assert_eq!(recs[0].result.outcome, Outcome::Pass);
    }

    #[test]
    fn a_long_reply_is_cut_and_marked_not_cut_silently() {
        let (text, cut) = cap_reply(&"é".repeat(MAX_HOST_REPLY_LEN));
        assert!(cut && text.len() <= MAX_HOST_REPLY_LEN && text.chars().all(|c| c == 'é'));
        assert_eq!(cap_reply("short"), ("short".to_string(), false));
    }

    #[test]
    fn what_this_core_does_not_run_is_refused_by_name_at_submission() {
        let s = study(vec![ex("a", "x", "pd: ")], vec![]);
        assert!(check_study(&s).is_ok());
        let prompt = HostStep { name: "ask".into(), action: HostAction::Prompt { text: "plug in".into() }, timeout_ms: 1000, continue_on_fail: false, delay_before_ms: 0 };
        let e = check_study(&study(vec![ex("a", "x", "pd: "), prompt], vec![])).unwrap_err();
        assert!(e.contains("steps[1] 'ask' is a Prompt step"), "{e}");
        let repeat = HostStep { name: "loop".into(), action: HostAction::Repeat { count: Some(2), values: vec![], steps: vec![ex("i", "x", "pd: ")] }, timeout_ms: 0, continue_on_fail: false, delay_before_ms: 0 };
        assert!(check_study(&study(vec![repeat], vec![])).unwrap_err().contains("Repeat"));
        let e = check_study(&study(vec![ex("a", "x", "pd: ")], vec![HostStep { name: "i".into(), action: HostAction::Instrument { instrument: "load".into(), line: "OUTP OFF".into(), query: false }, timeout_ms: 500, continue_on_fail: false, delay_before_ms: 0 }])).unwrap_err();
        assert!(e.contains("teardown[0]") && e.contains("Instrument"), "{e}");
        let mut with_counts = study(vec![ex("a", "x", "pd: ")], vec![]);
        with_counts.line_counts.push(embarch_study_designer::host_study::LineCount { name: "n".into(), tap: "t".into(), pattern: "p".into(), kind: Default::default() });
        // It fails the study's own check first (no such tap); either way it is refused at submission.
        assert!(check_study(&with_counts).is_err());
        // An exchange timeout the exchange route would refuse is refused here, by step.
        let mut long = ex("slow", "x", "pd: ");
        long.timeout_ms = 60_000;
        assert!(check_study(&study(vec![long], vec![])).unwrap_err().contains("steps[0] 'slow'"));
    }

    #[test]
    fn the_record_on_disk_reads_back_through_the_steps_route_shape() {
        let dir = tempfile::tempdir().unwrap();
        let s = study(vec![ex("a", "x", "pd: "), ex("b", "y", "pd: ")], vec![ex("safe", "s", "pd: ")]);
        let mut b = Script::new(vec![ok_reply("x", "pd: ok"), Ok(vec![]), ok_reply("s", "pd: ok")]);
        let jobs: JobRegistry = Default::default();
        jobs.lock().unwrap().insert("id1".into(), StudyJob { status: "running".into(), current_step: None, total_steps: Some(2), reason: None });
        let (tx, mut rx) = broadcast::channel(16);
        run_job(&s, "id1", dir.path(), &mut b, &AtomicBool::new(false), &jobs, &tx, None);
        let job = jobs.lock().unwrap().get("id1").cloned().unwrap();
        assert_eq!(job.status, "completed");
        assert!(job.reason.unwrap().contains("step 'b' failed"));
        assert_eq!(job.current_step, Some(1));
        let v: serde_json::Value = serde_json::from_slice(&std::fs::read(dir.path().join("events.json")).unwrap()).unwrap();
        assert_eq!(v["completed"], false);
        let steps = v["steps"].as_array().unwrap();
        assert_eq!(steps.len(), 3);
        assert_eq!(steps[1]["detail"], "empty_reply");
        assert_eq!(steps[1]["reply_bytes"], 0);
        assert_eq!(steps[2]["list"], "Teardown");
        assert!(steps[0]["reply"].as_str().unwrap().contains("result"));
        let resp = crate::study::steps_for_test(&String::from_utf8(std::fs::read(dir.path().join("events.json")).unwrap()).unwrap());
        assert_eq!(resp.steps.len(), 3);
        assert_eq!(resp.steps[1].detail.as_deref(), Some("empty_reply"));
        assert_eq!(resp.steps[1].reply_bytes, Some(0));
        assert_eq!(resp.steps[0].elapsed_ms, Some(steps[0]["elapsed_ms"].as_u64().unwrap()));
        assert!(resp.timed);
        // Three step events and a status change went out live.
        let mut kinds = vec![];
        while let Ok(e) = rx.try_recv() {
            kinds.push(match e {
                StudyEvent::HostStepCompleted { .. } => "step",
                StudyEvent::StatusChanged { .. } => "status",
                _ => "other",
            });
        }
        assert_eq!(kinds, ["step", "step", "step", "status"]);
    }

    #[tokio::test]
    async fn a_second_study_is_refused_409_while_any_study_holds_the_lock() {
        let state = AppState::new("t".into());
        *state.study_lock.lock().unwrap() = Some("study-abc".to_string());
        let s = study(vec![ex("a", "x", "pd: ")], vec![]);
        let err = post_host_study_handler(State(state.clone()), Json(s.clone())).await.unwrap_err();
        assert_eq!(err.0, StatusCode::CONFLICT);
        assert!(err.1.contains("study-abc"), "{}", err.1);
        // The refused call did not disturb the holder.
        assert_eq!(state.study_lock.lock().unwrap().as_deref(), Some("study-abc"));
        // A bad study is a 400 before the lock is looked at.
        let bad = study(vec![], vec![]);
        assert_eq!(post_host_study_handler(State(state), Json(bad)).await.unwrap_err().0, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn cancel_finds_the_running_host_study_and_says_why_otherwise() {
        let state = AppState::new("t".into());
        let flag = Arc::new(AtomicBool::new(false));
        *state.host_cancel.lock().unwrap() = Some(("h1".into(), flag.clone()));
        let (code, _) = cancel_study_handler(State(state.clone()), Path("h1".into())).await.unwrap();
        assert_eq!(code, StatusCode::ACCEPTED);
        assert!(flag.load(Ordering::SeqCst));
        state.study_jobs.lock().unwrap().insert("b1".into(), StudyJob { status: "running".into(), current_step: None, total_steps: None, reason: None });
        state.study_jobs.lock().unwrap().insert("d1".into(), StudyJob { status: "completed".into(), current_step: None, total_steps: None, reason: None });
        let e = cancel_study_handler(State(state.clone()), Path("b1".into())).await.unwrap_err();
        assert_eq!(e.0, StatusCode::CONFLICT);
        assert!(e.1.contains("bench study"));
        assert_eq!(cancel_study_handler(State(state.clone()), Path("d1".into())).await.unwrap_err().0, StatusCode::CONFLICT);
        assert_eq!(cancel_study_handler(State(state), Path("nope".into())).await.unwrap_err().0, StatusCode::NOT_FOUND);
    }

    // ---- taps (decision 98) ----

    /// One end of a virtual serial pair: the study side is `Console` (a `CapturePort`), the test
    /// side feeds the console's output and reads what the study wrote. A write echoes the line
    /// back with a prompt, as a Zephyr shell does.
    #[derive(Clone, Default)]
    struct Pair {
        incoming: Arc<StdMutex<VecDeque<Vec<u8>>>>,
        written: Arc<StdMutex<Vec<u8>>>,
        opens: Arc<std::sync::atomic::AtomicUsize>,
    }

    impl Pair {
        fn feed(&self, b: &[u8]) {
            self.incoming.lock().unwrap().push_back(b.to_vec());
        }
        fn opener(&self) -> crate::capture::Opener {
            let me = self.clone();
            Arc::new(move || {
                me.opens.fetch_add(1, Ordering::SeqCst);
                Ok(Box::new(me.clone()) as Box<dyn crate::capture::CapturePort>)
            })
        }
    }

    impl Read for Pair {
        fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
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

    impl Write for Pair {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            self.written.lock().unwrap().extend_from_slice(b);
            let mut reply = b.to_vec();
            reply.extend_from_slice(b"result\r\n> ");
            self.incoming.lock().unwrap().push_back(reply);
            Ok(b.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl crate::capture::CapturePort for Pair {
        fn set_timeout(&mut self, _d: Duration) {}
    }

    /// The real backend's route minus topology: a tapped signal's exchange goes through its
    /// capture, as `api::run_signal_exchange` does.
    struct ViaCapture {
        captures: crate::capture::CaptureRegistry,
        replies: Vec<(String, String)>,
    }

    impl HostBackend for ViaCapture {
        fn exchange(&mut self, signal: &str, req: ExchangeRequest) -> Result<ExchangeResponse, ExchangeFailure> {
            let cap = self.captures.running(signal).ok_or_else(|| ExchangeFailure::PortUnavailable("no tap".into()))?;
            let r = cap.exchange(req, || crate::exchange::CoreState::unavailable("test")).map_err(|e| ExchangeFailure::Failed(format!("{e:#}")))?;
            self.replies.push((signal.to_string(), r.text.clone()));
            Ok(r)
        }
    }

    fn tap(id: u8, name: &str, signal: &str) -> embarch_study_designer::StreamTap {
        embarch_study_designer::StreamTap {
            id,
            name: name.try_into().unwrap(),
            source: embarch_study_designer::StreamSource::Signal { name: signal.try_into().unwrap() },
            encoding: embarch_study_designer::StreamEncoding::Text,
            scope: embarch_study_designer::StreamScope::WholeStudy,
        }
    }

    fn tapped_ex(name: &str, signal: &str, line: &str) -> HostStep {
        let mut s = ex(name, line, "> ");
        if let HostAction::Exchange { signal: sg, .. } = &mut s.action {
            *sg = signal.into();
        }
        s.timeout_ms = 2000;
        s
    }

    fn wait_for(cond: impl Fn() -> bool) {
        let end = Instant::now() + Duration::from_secs(5);
        while !cond() {
            assert!(Instant::now() < end, "timed out waiting");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn two_tapped_consoles_with_exchanges_on_both_are_each_opened_once_and_lose_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let (pd, dut) = (Pair::default(), Pair::default());
        let captures = crate::capture::CaptureRegistry::default();
        let ports = crate::study::TapPorts::default();
        let mut s = study(
            vec![tapped_ex("pd-1", "pd", "pd status"), wait("gap", 150), tapped_ex("dut-1", "dut", "pd role"), tapped_ex("pd-2", "pd", "pd max")],
            vec![],
        );
        s.streams = vec![tap(0, "pd console", "pd"), tap(1, "dut console", "dut")];
        assert!(check_study(&s).is_ok());
        let (pd2, dut2) = (pd.clone(), dut.clone());
        let resolve = move |sig: &str| -> Result<TapTarget, TapError> {
            let p = if sig == "pd" { &pd2 } else { &dut2 };
            Ok(TapTarget { port_name: format!("FAKE-{sig}"), baud: 115_200, opener: p.opener(), identity: None })
        };
        let taps = StudyTaps::start(&captures, &ports, dir.path(), &s, &resolve).unwrap().unwrap();
        assert!(ports.holds("FAKE-pd") && ports.holds("FAKE-dut"));
        // Output the consoles print between steps, while nothing is exchanging.
        pd.feed(b"pd: PPS timeout\r\npd: hard reset\r\n");
        wait_for(|| captures.running("pd").unwrap().read(0, 0).total_bytes > 0);
        let feeder = {
            let dut = dut.clone();
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(60));
                dut.feed(b"dut: attach\ndut: contract 9V\n");
            })
        };
        let mut b = ViaCapture { captures: captures.clone(), replies: vec![] };
        let mut recs = vec![];
        let end = execute(&s, &mut b, &AtomicBool::new(false), |r| recs.push(r));
        feeder.join().unwrap();
        assert!(end.completed, "{end:?}");
        assert!(recs.iter().all(|r| matches!(r.result.outcome, Outcome::Pass)), "{recs:?}");
        // Opened once each, though pd was exchanged on twice.
        assert_eq!((pd.opens.load(Ordering::SeqCst), dut.opens.load(Ordering::SeqCst)), (1, 1));
        let streams = taps.finish();
        assert!(!ports.holds("FAKE-pd") && !ports.holds("FAKE-dut"), "ports are released");
        assert_eq!(streams.len(), 2);
        assert_eq!(streams[0]["state"], "stopped");

        let raw = |n: &str| String::from_utf8(std::fs::read(dir.path().join("streams").join(n)).unwrap()).unwrap();
        let pd_raw = raw("pd_console.txt");
        let dut_raw = raw("dut_console.txt");
        // What the console printed between steps is there, in order, before the first reply.
        assert!(pd_raw.starts_with("pd: PPS timeout\r\npd: hard reset\r\n"), "{pd_raw:?}");
        assert!(dut_raw.starts_with("dut: attach\ndut: contract 9V\n"), "{dut_raw:?}");
        // Each exchange's reply is exactly the tap's bytes from its write to `until`.
        let pd_replies: Vec<&String> = b.replies.iter().filter(|(s, _)| s == "pd").map(|(_, t)| t).collect();
        assert_eq!(pd_replies.len(), 2);
        let tail = pd_raw.strip_prefix("pd: PPS timeout\r\npd: hard reset\r\n").unwrap();
        assert_eq!(tail, format!("{}{}", pd_replies[0], pd_replies[1]));
        assert_eq!(&*pd.written.lock().unwrap(), b"pd status\npd max\n");
        assert_eq!(&*dut.written.lock().unwrap(), b"pd role\n");
        assert_eq!(streams[0]["bytes_written"], pd_raw.len());
    }

    #[test]
    fn each_stored_line_has_a_receive_time_on_one_clock_across_taps() {
        let dir = tempfile::tempdir().unwrap();
        let (a, c) = (Pair::default(), Pair::default());
        let captures = crate::capture::CaptureRegistry::default();
        let ports = crate::study::TapPorts::default();
        let mut s = study(vec![wait("w", 5)], vec![]);
        s.streams = vec![tap(0, "a", "a"), tap(1, "c", "c")];
        let (a2, c2) = (a.clone(), c.clone());
        let resolve = move |sig: &str| -> Result<TapTarget, TapError> {
            let p = if sig == "a" { &a2 } else { &c2 };
            Ok(TapTarget { port_name: format!("FAKE-{sig}"), baud: 115_200, opener: p.opener(), identity: None })
        };
        let taps = StudyTaps::start(&captures, &ports, dir.path(), &s, &resolve).unwrap().unwrap();
        // One read holding two lines, then a line on the other console, then a split line.
        a.feed(b"a1\na2\n");
        wait_for(|| captures.running("a").unwrap().read(0, 0).total_bytes == 6);
        std::thread::sleep(Duration::from_millis(80));
        c.feed(b"c1\n");
        wait_for(|| captures.running("c").unwrap().read(0, 0).total_bytes == 3);
        std::thread::sleep(Duration::from_millis(80));
        a.feed(b"a3 par");
        wait_for(|| captures.running("a").unwrap().read(0, 0).total_bytes == 12);
        a.feed(b"t\n");
        wait_for(|| captures.running("a").unwrap().read(0, 0).total_bytes == 14);
        let _ = taps.finish();
        let rows = |n: &str| -> Vec<(u64, u64, u64)> {
            std::fs::read_to_string(dir.path().join("streams").join(n))
                .unwrap()
                .lines()
                .skip(1)
                .map(|l| {
                    let v: Vec<u64> = l.split(',').map(|x| x.parse().unwrap()).collect();
                    (v[0], v[1], v[2])
                })
                .collect()
        };
        let ar = rows("a.arrival.csv");
        let cr = rows("c.arrival.csv");
        // Lines are separate records: offsets 0, 3, 6 (the partial "a3 par"), 12 ("t\n").
        assert_eq!(ar.iter().map(|r| (r.0, r.2)).collect::<Vec<_>>(), [(0, 3), (3, 3), (6, 6), (12, 2)]);
        assert_eq!(cr.iter().map(|r| (r.0, r.2)).collect::<Vec<_>>(), [(0, 3)]);
        // One clock: a's stamps never go backwards, and c1 (fed 80 ms after a1/a2, 80 ms before a3) sits between.
        assert!(ar.windows(2).all(|w| w[0].1 <= w[1].1), "{ar:?}");
        assert!(ar[1].1 <= cr[0].1 && cr[0].1 <= ar[2].1, "a {ar:?} c {cr:?}");
        assert!(cr[0].1 - ar[1].1 >= 40 && ar[2].1 - cr[0].1 >= 40, "a {ar:?} c {cr:?}");
        assert!(ar[0].1 > 1_700_000_000_000);
    }

    #[test]
    fn a_tap_on_a_port_something_else_holds_is_a_409_and_leaves_nothing_open() {
        let dir = tempfile::tempdir().unwrap();
        let (a, c) = (Pair::default(), Pair::default());
        let captures = crate::capture::CaptureRegistry::default();
        let ports = crate::study::TapPorts::default();
        let _held = ports.claim("FAKE-c");
        let mut s = study(vec![wait("w", 5)], vec![]);
        s.streams = vec![tap(0, "a", "a"), tap(1, "c", "c")];
        let (a2, c2) = (a.clone(), c.clone());
        let resolve = move |sig: &str| -> Result<TapTarget, TapError> {
            let p = if sig == "a" { &a2 } else { &c2 };
            Ok(TapTarget { port_name: format!("FAKE-{sig}"), baud: 115_200, opener: p.opener(), identity: None })
        };
        let err = StudyTaps::start(&captures, &ports, dir.path(), &s, &resolve).err().unwrap();
        assert_eq!(err.0, StatusCode::CONFLICT);
        assert!(err.1.contains("FAKE-c"), "{}", err.1);
        // The first tap was stopped and released when the second refused.
        assert!(!ports.holds("FAKE-a") && captures.running("a").is_none());
        // A capture already running on the signal is refused the same way, naming it.
        let cap = crate::capture::start_fake(&captures, "a", b"");
        let err = StudyTaps::start(&captures, &ports, dir.path(), &s, &resolve).err().unwrap();
        assert!(err.1.contains(&cap.id), "{}", err.1);
        cap.stop();
    }

    #[test]
    fn only_a_text_whole_study_tap_is_accepted_and_the_rest_are_refused_by_name() {
        let mut s = study(vec![ex("a", "x", "pd: ")], vec![]);
        s.streams = vec![tap(0, "pd console", "pd")];
        assert!(check_study(&s).is_ok());
        s.streams[0].encoding = embarch_study_designer::StreamEncoding::Raw;
        assert!(check_study(&s).unwrap_err().contains("streams[0] 'pd console'"));
        s.streams[0].encoding = embarch_study_designer::StreamEncoding::Text;
        s.streams[0].scope = embarch_study_designer::StreamScope::Steps { from: 0, to: 0 };
        assert!(check_study(&s).unwrap_err().contains("WholeStudy"));
    }
}
