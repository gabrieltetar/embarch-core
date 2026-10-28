//! `POST /host-studies`, `POST /study/{id}/cancel`, `POST /study/{id}/prompt`: a study Core runs
//! itself, one `Exchange`, `Instrument`, `Wait` or `Prompt` step at a time (decisions 95, 104, 105).
//!
//! **A `Prompt` waits for a person and nothing else** (decision 105): it publishes the prompt
//! (`PromptPending`, and `pending_prompt` on `GET /study/{id}` since SSE keeps no replay), and the
//! step ends on an answer from `POST /study/{id}/prompt`, its deadline, or a cancel of the study.
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
    HostAction, HostList, HostStep, HostStepResult, HostStudy, IterationMarker, IterationRef, Outcome, VALUE_PLACEHOLDER,
};
use serde::{Deserialize, Serialize};
use std::io::Write as _;
use std::path::{Path as FsPath, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant};
use tokio::sync::broadcast;

use crate::api::{internal_err, AppState};
use crate::exchange::{ExchangeRequest, ExchangeResponse};
use embarch_topology::hardware::{Ack, InstrumentExchange, InstrumentReply};
use crate::host_counts::RepeatSpan;
use crate::study::{JobRegistry, StudyAcceptedResponse, StudyEvent, StudyJob};

/// The submitted `HostStudy`, kept in the study's directory.
pub const STUDY_FILE: &str = "study.json";

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
    /// An `Instrument` query got a whole reply line.
    pub const REPLIED: &str = "replied";
    /// An `Instrument` set answered the instrument's declared `ack_ok`.
    pub const ACKNOWLEDGED: &str = "acknowledged";
    /// An `Instrument` set's reply held neither declared acknowledgement, or nothing came back:
    /// the set is not known to have been carried out (decision 104).
    pub const UNACKNOWLEDGED: &str = "unacknowledged";
    /// An `Instrument` set on an instrument that declares no acknowledgement: written, not read.
    pub const WRITTEN: &str = "written";
    /// A `Prompt` a person confirmed (decision 105).
    pub const CONFIRMED: &str = "confirmed";
    /// A `Prompt` a person cancelled: the step fails, so the study stops unless `continue_on_fail`.
    pub const PROMPT_CANCELLED: &str = "prompt_cancelled";
    /// A `Prompt` nobody answered by its deadline.
    pub const PROMPT_TIMED_OUT: &str = "prompt_timed_out";
}

/// The longest a `Prompt` may wait: one hour (decision 105). **Reasoned, not measured**: the
/// field's longest gap between two confirmations was minutes, and a prompt holds `study_lock`, so
/// a forgotten one has to end on its own. The step's own `timeout_ms` is the deadline; there is
/// no default, because the designer already requires every step to state one.
pub const MAX_PROMPT_TIMEOUT_MS: u32 = 60 * 60 * 1000;

/// A person's answer to a `Prompt`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PromptAnswer {
    Confirmed,
    Cancelled,
}

/// Which surface relayed an answer. Recorded in the step's `answered_by`; never used to decide
/// anything, since every surface relays the same person.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AnsweredBy {
    Ui,
    Mcp,
    Cli,
}

impl AnsweredBy {
    pub fn as_str(self) -> &'static str {
        match self {
            AnsweredBy::Ui => "ui",
            AnsweredBy::Mcp => "mcp",
            AnsweredBy::Cli => "cli",
        }
    }
}

/// How a prompt ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptEnd {
    Answered(PromptAnswer, AnsweredBy),
    /// Nobody answered by the deadline.
    TimedOut,
    /// The study was cancelled while the prompt waited.
    StudyCancelled,
}

impl PromptEnd {
    /// `PromptResolved.answer`: `confirmed`, `cancelled`, `timed_out` or `study_cancelled`.
    pub fn answer_str(self) -> &'static str {
        match self {
            PromptEnd::Answered(PromptAnswer::Confirmed, _) => "confirmed",
            PromptEnd::Answered(PromptAnswer::Cancelled, _) => "cancelled",
            PromptEnd::TimedOut => "timed_out",
            PromptEnd::StudyCancelled => "study_cancelled",
        }
    }

    fn answered_by(self) -> Option<AnsweredBy> {
        match self {
            PromptEnd::Answered(_, by) => Some(by),
            _ => None,
        }
    }
}

/// What a `Prompt` step asks its backend for.
#[derive(Debug, Clone)]
pub struct PromptRequest {
    pub list: HostList,
    pub step_index: u32,
    pub step_name: String,
    pub text: String,
    pub timeout_ms: u32,
}

/// What the backend says about a prompt it ran: how it ended, and the id and deadline it was
/// published under, which the step's record keeps.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PromptRun {
    pub end: PromptEnd,
    pub prompt_id: u32,
    pub deadline_utc_ms: u64,
}

/// The prompt a host study is waiting on, as `PromptPending` and `GET /study/{id}` show it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PendingPrompt {
    /// 1-based within the study. An answer names it, so an answer meant for a prompt that has
    /// already ended can never be taken for the next one.
    pub prompt_id: u32,
    pub list: HostList,
    /// Position in `list`; for a prompt inside a `Repeat`, the `Repeat`'s position.
    pub step_index: u32,
    pub step_name: String,
    pub text: String,
    pub opened_utc_ms: u64,
    pub deadline_utc_ms: u64,
}

/// The one prompt slot: what is pending and, once a person has answered, the answer, which the
/// executor picks up within [`CANCEL_POLL`].
#[derive(Debug, Clone)]
pub struct PromptSlot {
    pub study_id: String,
    pub pending: PendingPrompt,
    pub answer: Option<(PromptAnswer, AnsweredBy)>,
}

/// Shared by the executor and `POST /study/{id}/prompt`. One at a time, like `study_lock`.
pub type HostPrompt = Arc<StdMutex<Option<PromptSlot>>>;

/// Waits on `slot` for an answer to `prompt_id` until `timeout`, looking every [`CANCEL_POLL`] so
/// a cancel of the study is seen within it. The slot is cleared **under the same lock as the last
/// look**, so an answer either lands before the clear and counts, or finds no prompt and is
/// refused `409`: never accepted and then lost.
pub fn wait_for_answer(slot: &HostPrompt, prompt_id: u32, timeout: Duration, cancel: Option<&AtomicBool>) -> PromptEnd {
    let deadline = Instant::now() + timeout;
    loop {
        {
            let mut g = slot.lock().unwrap();
            let ours = g.as_ref().is_some_and(|s| s.pending.prompt_id == prompt_id);
            let answered = g.as_ref().filter(|_| ours).and_then(|s| s.answer);
            let end = match answered {
                Some((answer, by)) => Some(PromptEnd::Answered(answer, by)),
                None if cancel.is_some_and(|c| c.load(Ordering::SeqCst)) => Some(PromptEnd::StudyCancelled),
                None if Instant::now() >= deadline => Some(PromptEnd::TimedOut),
                None => None,
            };
            if let Some(end) = end {
                if ours {
                    *g = None;
                }
                return end;
            }
        }
        let left = deadline.saturating_duration_since(Instant::now());
        std::thread::sleep(CANCEL_POLL.min(left).max(Duration::from_millis(1)));
    }
}

/// `POST /study/{id}/prompt`'s body.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PromptAnswerBody {
    pub prompt_id: u32,
    pub answer: PromptAnswer,
    pub answered_by: AnsweredBy,
}

/// Records `body` against `study_id`'s pending prompt. `Ok(false)` when that study has no prompt
/// pending; `Err` (a `409`) for an answer to another prompt or to one already answered.
pub fn record_answer(slot: &HostPrompt, study_id: &str, body: &PromptAnswerBody) -> Result<bool, String> {
    let mut g = slot.lock().unwrap();
    let Some(s) = g.as_mut().filter(|s| s.study_id == study_id) else { return Ok(false) };
    if s.pending.prompt_id != body.prompt_id {
        return Err(format!(
            "prompt {} is not the one waiting: prompt {} is ('{}'), so this answer was meant for a prompt that already ended",
            body.prompt_id, s.pending.prompt_id, s.pending.text
        ));
    }
    if let Some((answer, by)) = s.answer {
        return Err(format!("prompt {} was already answered ({answer:?}, from {})", body.prompt_id, by.as_str()));
    }
    s.answer = Some((body.answer, body.answered_by));
    Ok(true)
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
    /// One command to a declared instrument (decision 104).
    fn instrument(&mut self, name: &str, req: InstrumentExchange) -> Result<InstrumentReply, ExchangeFailure>;
    /// One `Prompt`: publish it, then wait for a person's answer until its deadline or a cancel of
    /// the study (`cancel`, `None` in the teardown), and say how it ended (decision 105).
    fn prompt(&mut self, req: PromptRequest, cancel: Option<&AtomicBool>) -> PromptRun;
}

/// One step as the run recorded it: the study-designer result plus what Core saw.
#[derive(Debug, Clone, Serialize)]
pub struct StepRecord {
    #[serde(flatten)]
    pub result: HostStepResult,
    pub list: HostList,
    /// Position in `list`; for a step inside a `Repeat`, the `Repeat`'s position.
    pub index: u32,
    /// Position inside the `Repeat`'s `steps`; `None` for a step that is not in one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub inner_index: Option<u32>,
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
    /// A `Prompt`'s id and deadline (decision 105); absent on every other step. Who answered is
    /// the result's own `answered_by`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt_id: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt_deadline_utc_ms: Option<u64>,
}

/// How a run ended. `completed` is whether every step ran and none stopped the study.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunEnd {
    pub completed: bool,
    pub reason: Option<String>,
    /// Teardown steps that did not pass. They never stop the rest of the teardown.
    pub teardown_failures: u32,
    /// One marker per pass that started, in order, on the study clock.
    pub iterations: Vec<IterationMarker>,
    /// When each `Repeat` that started ran, on the study clock.
    pub spans: Vec<RepeatSpan>,
}

/// The study's one clock: an anchor taken once plus the monotonic time since, so a pass marker, a
/// step's times and a tap's per-line stamps all compare without a wall-clock step in between.
#[derive(Debug, Clone, Copy)]
pub struct StudyClock {
    epoch: Instant,
    epoch_utc: u64,
}

impl StudyClock {
    pub fn new() -> Self {
        Self { epoch: Instant::now(), epoch_utc: now_ms() }
    }

    pub fn now(&self) -> u64 {
        self.epoch_utc + self.epoch.elapsed().as_millis() as u64
    }
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

/// The outcome of an `Instrument` step (decision 104). A query passes on a whole reply line; a
/// set passes on the declared `ack_ok`, or on the write alone when the instrument declares none.
/// **An unacknowledged set fails**: the action was not carried out as far as anyone can tell.
fn judge_instrument(r: &InstrumentReply, timeout_ms: u32) -> (Outcome, &'static str, Option<String>) {
    if r.query {
        return if r.terminated {
            (Outcome::Pass, detail::REPLIED, None)
        } else if r.reply.is_empty() {
            (
                fail("empty reply: the query got nothing back"),
                detail::EMPTY_REPLY,
                Some(format!("'{}' was written and nothing came back within {timeout_ms} ms", r.line)),
            )
        } else {
            (
                Outcome::TimedOut,
                detail::UNTIL_NOT_SEEN,
                Some(format!("part of a reply came back but no line end within {timeout_ms} ms")),
            )
        };
    }
    match r.ack {
        None => (Outcome::Pass, detail::WRITTEN, None),
        Some(Ack::Ok) => (Outcome::Pass, detail::ACKNOWLEDGED, None),
        Some(Ack::Refused) => (
            fail("refused: the instrument answered its ack_err"),
            detail::REFUSED,
            Some(format!("'{}' answered '{}'", r.line, r.reply)),
        ),
        Some(Ack::Unacknowledged) => (
            fail("unacknowledged: the set was not confirmed"),
            detail::UNACKNOWLEDGED,
            Some(if r.reply.is_empty() {
                format!("'{}' got no answer within {timeout_ms} ms", r.line)
            } else {
                format!("'{}' answered '{}', which is neither declared acknowledgement", r.line, r.reply)
            }),
        ),
    }
}

/// Runs one step to a record. `cancel` is `None` in teardown.
fn run_step<B: HostBackend>(
    step: &HostStep,
    list: HostList,
    index: u32,
    backend: &mut B,
    cancel: Option<&AtomicBool>,
    clock: StudyClock,
) -> StepRecord {
    let started = clock.now();
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
        inner_index: None,
        detail: detail::WAITED,
        note: None,
        reply_bytes: 0,
        until_seen: false,
        echo_seen: false,
        port_lost: false,
        started_utc_ms: started,
        ended_utc_ms: started,
        delay_before_ms: step.delay_before_ms,
        prompt_id: None,
        prompt_deadline_utc_ms: None,
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
            HostAction::Instrument { instrument, line, query } => {
                let req = InstrumentExchange { line: line.clone(), query: *query, timeout_ms: step.timeout_ms as u64, port_wait_ms: 0 };
                match backend.instrument(instrument, req) {
                    Ok(r) => {
                        let (outcome, d, note) = judge_instrument(&r, step.timeout_ms);
                        let (reply, cut) = cap_reply(&r.reply);
                        rec.result.outcome = outcome;
                        rec.result.reply = reply;
                        rec.result.reply_truncated = cut;
                        rec.detail = d;
                        rec.note = note;
                        rec.reply_bytes = r.reply.len();
                        rec.until_seen = r.terminated;
                    }
                    Err(ExchangeFailure::PortUnavailable(msg)) => {
                        rec.result.outcome = fail("the instrument's port is not available");
                        rec.detail = detail::PORT_UNAVAILABLE;
                        rec.note = Some(msg);
                    }
                    Err(ExchangeFailure::Failed(msg)) => {
                        rec.result.outcome = fail("the instrument command failed before a reply");
                        rec.detail = detail::EXCHANGE_FAILED;
                        rec.note = Some(msg);
                    }
                }
            }
            HostAction::Prompt { text } => {
                let req = PromptRequest { list, step_index: index, step_name: step.name.clone(), text: text.clone(), timeout_ms: step.timeout_ms };
                let run = backend.prompt(req, cancel);
                rec.prompt_id = Some(run.prompt_id);
                rec.prompt_deadline_utc_ms = Some(run.deadline_utc_ms);
                rec.result.answered_by = run.end.answered_by().map(|by| by.as_str().to_string());
                match run.end {
                    PromptEnd::Answered(PromptAnswer::Confirmed, _) => rec.detail = detail::CONFIRMED,
                    PromptEnd::Answered(PromptAnswer::Cancelled, _) => {
                        rec.result.outcome = fail("the operator cancelled the prompt");
                        rec.detail = detail::PROMPT_CANCELLED;
                    }
                    PromptEnd::TimedOut => {
                        rec.result.outcome = Outcome::TimedOut;
                        rec.detail = detail::PROMPT_TIMED_OUT;
                    }
                    PromptEnd::StudyCancelled => {
                        rec.result.outcome = fail("cancelled while the prompt waited");
                        rec.detail = detail::CANCELLED;
                    }
                }
            }
            // A `Repeat` is walked by `walk`, never run as one step; reaching here is a bug, said
            // as a failure rather than a skip.
            other => {
                rec.result.outcome = fail("this action is not executed by this Core");
                rec.detail = detail::EXCHANGE_FAILED;
                rec.note = Some(format!("{other:?}"));
            }
        }
    }
    rec.result.elapsed_ms = t0.elapsed().as_millis() as u64;
    rec.ended_utc_ms = clock.now();
    rec
}

/// What a run accumulates beside the step records.
struct Acc {
    iterations: Vec<IterationMarker>,
    spans: Vec<RepeatSpan>,
}

/// `step` with `{value}` replaced in an `Exchange` or `Instrument` line.
fn substitute(step: &HostStep, value: &str) -> HostStep {
    let mut s = step.clone();
    match &mut s.action {
        HostAction::Exchange { line, .. } | HostAction::Instrument { line, .. } => *line = line.replace(VALUE_PLACEHOLDER, value),
        _ => {}
    }
    s
}

/// Why `rec` stops the study, or `None` to go on. Teardown never asks.
fn stop_reason(step: &HostStep, rec: &StepRecord) -> Option<String> {
    match &rec.result.outcome {
        Outcome::Pass => None,
        _ if rec.detail == detail::CANCELLED => Some(format!("cancelled during step '{}'", step.name)),
        Outcome::Fail { reason } if !step.continue_on_fail => Some(format!("step '{}' failed ({reason})", step.name)),
        Outcome::TimedOut if !step.continue_on_fail => Some(format!("step '{}' timed out ({})", step.name, rec.detail)),
        _ => None,
    }
}

/// Runs one `Repeat`: a marker as each pass starts, then the inner steps with `{value}` filled
/// in, each record tagged with its pass. Returns why it stopped the study, if it did. Each inner
/// step's own `continue_on_fail` governs; the `Repeat`'s own has no effect.
#[allow(clippy::too_many_arguments)]
fn run_repeat<B: HostBackend>(
    step: &HostStep,
    inner: &[HostStep],
    count: Option<u32>,
    values: &[String],
    list: HostList,
    index: u32,
    backend: &mut B,
    cancel: Option<&AtomicBool>,
    clock: StudyClock,
    acc: &mut Acc,
    on_step: &mut dyn FnMut(StepRecord),
) -> Option<String> {
    let cancelled = || cancel.is_some_and(|c| c.load(Ordering::SeqCst));
    if step.delay_before_ms > 0 && !sleep_for(step.delay_before_ms as u64, cancel) {
        return Some(format!("cancelled during step '{}'", step.name));
    }
    let passes: Vec<Option<&String>> = if values.is_empty() { (0..count.unwrap_or(0)).map(|_| None).collect() } else { values.iter().map(Some).collect() };
    for (pass, entry) in passes.iter().enumerate() {
        let pass = pass as u32;
        if cancelled() {
            return Some(format!("cancelled before pass {} of '{}'", pass + 1, step.name));
        }
        acc.iterations.push(IterationMarker {
            list,
            repeat_index: index,
            pass,
            value: entry.map(|v| v.to_string()),
            started_utc_ms: clock.now(),
        });
        let value = entry.map(|v| v.to_string()).unwrap_or_else(|| (pass + 1).to_string());
        for (j, s) in inner.iter().enumerate() {
            if cancelled() {
                return Some(format!("cancelled before step '{}'", s.name));
            }
            let mut rec = run_step(&substitute(s, &value), list, index, backend, cancel, clock);
            rec.inner_index = Some(j as u32);
            rec.result.iteration = Some(IterationRef { list, repeat_index: index, pass });
            let why = if list == HostList::Steps { stop_reason(s, &rec) } else { None };
            on_step(rec);
            if why.is_some() {
                return why;
            }
        }
    }
    None
}

/// Runs `steps` in order. In the main list a step that fails, times out or is cancelled stops the
/// walk (unless `continue_on_fail`); in the teardown nothing stops it and failures are counted.
#[allow(clippy::too_many_arguments)]
fn walk<B: HostBackend>(
    steps: &[HostStep],
    list: HostList,
    backend: &mut B,
    cancel: Option<&AtomicBool>,
    clock: StudyClock,
    acc: &mut Acc,
    on_step: &mut dyn FnMut(StepRecord),
) -> Option<String> {
    for (i, step) in steps.iter().enumerate() {
        let i = i as u32;
        if cancel.is_some_and(|c| c.load(Ordering::SeqCst)) {
            return Some(format!("cancelled before step '{}'", step.name));
        }
        if let HostAction::Repeat { count, values, steps: inner } = &step.action {
            let started = clock.now();
            let why = run_repeat(step, inner, *count, values, list, i, backend, cancel, clock, acc, on_step);
            acc.spans.push(RepeatSpan { list, repeat_index: i, name: step.name.clone(), started_utc_ms: started, ended_utc_ms: clock.now() });
            if why.is_some() {
                return why;
            }
            continue;
        }
        let rec = run_step(step, list, i, backend, cancel, clock);
        let why = if list == HostList::Steps { stop_reason(step, &rec) } else { None };
        on_step(rec);
        if why.is_some() {
            return why;
        }
    }
    None
}

/// Runs `study`: its steps until one stops it or `cancel` is set, then **always** its teardown.
/// `on_step` sees each record the moment it exists, steps and teardown alike.
pub fn execute<B: HostBackend>(
    study: &HostStudy,
    backend: &mut B,
    cancel: &AtomicBool,
    clock: StudyClock,
    mut on_step: impl FnMut(StepRecord),
) -> RunEnd {
    use std::panic::{catch_unwind, AssertUnwindSafe};

    let mut acc = Acc { iterations: vec![], spans: vec![] };
    let mut end = RunEnd { completed: true, reason: None, teardown_failures: 0, iterations: vec![], spans: vec![] };
    let main = catch_unwind(AssertUnwindSafe(|| {
        walk(&study.steps, HostList::Steps, backend, Some(cancel), clock, &mut acc, &mut on_step)
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
    let mut failures = 0u32;
    walk(&study.teardown, HostList::Teardown, backend, None, clock, &mut acc, &mut |rec: StepRecord| {
        if !matches!(rec.result.outcome, Outcome::Pass) {
            failures += 1;
        }
        on_step(rec);
    });
    end.teardown_failures = failures;
    end.iterations = acc.iterations;
    end.spans = acc.spans;
    end
}

// ---- submission checks ------------------------------------------------------

/// Every step that runs, with where it is: the list's own steps, and for a `Repeat` its inner
/// steps (one level, the designer's rule). `(list name, position, step)`; an inner step's
/// position reads `"{repeat}.{inner}"`.
fn flat_steps(study: &HostStudy) -> Vec<(&'static str, String, &HostStep)> {
    let mut out = Vec::new();
    for (list, steps) in [("steps", &study.steps), ("teardown", &study.teardown)] {
        for (i, s) in steps.iter().enumerate() {
            if let HostAction::Repeat { steps: inner, .. } = &s.action {
                for (j, t) in inner.iter().enumerate() {
                    out.push((list, format!("{i}.{j}"), t));
                }
            } else {
                out.push((list, i.to_string(), s));
            }
        }
    }
    out
}

/// What this Core does not execute, by name. Every action runs since decision 105 (`Prompt`, also
/// inside a `Repeat`); `streams` is accepted for `Text` + `WholeStudy` taps only.
fn unsupported(study: &HostStudy) -> Option<String> {
    study.streams.iter().find_map(|t| {
        if !matches!(t.encoding, embarch_study_designer::StreamEncoding::Text) {
            Some(format!("streams[{}] '{}': only a `Text` tap is captured in a host study (decision 98)", t.id, t.name))
        } else if !matches!(t.scope, embarch_study_designer::StreamScope::WholeStudy) {
            Some(format!("streams[{}] '{}': only a `WholeStudy` tap is captured in a host study (decision 98)", t.id, t.name))
        } else {
            None
        }
    })
}

/// Everything checkable with no topology and no lock: the study's own rules, then what this Core
/// executes, then each `Exchange` as the exchange route would check it. A caller error is a `400`.
pub fn check_study(study: &HostStudy) -> Result<(), String> {
    study.validate().map_err(|e| format!("invalid host study: {e:?}"))?;
    if let Some(msg) = unsupported(study) {
        return Err(msg);
    }
    for (list, pos, s) in flat_steps(study) {
        if matches!(s.action, HostAction::Prompt { .. }) && s.timeout_ms > MAX_PROMPT_TIMEOUT_MS {
            return Err(format!(
                "{list}[{pos}] '{}': a Prompt waits at most {MAX_PROMPT_TIMEOUT_MS} ms (decision 105); this one asks for {} ms",
                s.name, s.timeout_ms
            ));
        }
        if let HostAction::Exchange { until, match_after_echo, .. } = &s.action {
            // A `{value}` is checked as the first thing it will become.
            let probe = substitute(s, "1");
            let HostAction::Exchange { line: probe_line, .. } = &probe.action else { continue };
            let req = ExchangeRequest {
                write: format!("{probe_line}\n"),
                until: Some(until.clone()),
                timeout_ms: s.timeout_ms as u64,
                discard_pending: true,
                match_after_echo: *match_after_echo,
                port_wait_ms: 0,
            };
            crate::exchange::check(&req).map_err(|e| format!("{list}[{pos}] '{}': {e}", s.name))?;
        }
        if let HostAction::Instrument { query, .. } = &s.action {
            let probe = substitute(s, "1");
            let HostAction::Instrument { line: probe_line, .. } = &probe.action else { continue };
            let req = InstrumentExchange { line: probe_line.clone(), query: *query, timeout_ms: s.timeout_ms as u64, port_wait_ms: 0 };
            crate::instrument::check(&req).map_err(|e| format!("{list}[{pos}] '{}': {e}", s.name))?;
        }
    }
    Ok(())
}

/// Every signal an `Exchange` names must be declared and writable now, and every instrument an
/// `Instrument` step names must be declared (decision 104). Reads enrollment.
fn check_signals(study: &HostStudy) -> Result<(), String> {
    for (list, pos, s) in flat_steps(study) {
        let HostAction::Instrument { instrument, .. } = &s.action else { continue };
        if embarch_topology::hardware::instrument::get(instrument)
            .map_err(|e| format!("could not read the instrument table: {e:#}"))?
            .is_none()
        {
            return Err(format!("{list}[{pos}] '{}': no instrument is declared under the name '{instrument}'", s.name));
        }
    }
    for (list, pos, s) in flat_steps(study) {
        let HostAction::Exchange { signal, .. } = &s.action else { continue };
        let link = embarch_topology::hardware::find_signal(signal)
            .map_err(|e| format!("could not read the signal table: {e:#}"))?
            .ok_or_else(|| format!("{list}[{pos}] '{}': no signal is declared under the name '{signal}'", s.name))?;
        if !link.host_can_write() {
            return Err(format!(
                "{list}[{pos}] '{}': signal '{signal}' is not a direct host-to-dut or bidirectional signal Core may write (embarch-topology decision 37)",
                s.name
            ));
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
            "],\"completed\":{},\"stopped_reason\":{},\"teardown_failures\":{},\"streams\":{},\"iterations\":{},\"repeats\":{}}}",
            end.completed,
            serde_json::to_string(&end.reason)?,
            end.teardown_failures,
            serde_json::to_string(streams)?,
            serde_json::to_string(&end.iterations)?,
            serde_json::to_string(&end.spans)?
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
    /// The clock every tap stamps with; the run's markers and step times use it too.
    pub clock: StudyClock,
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
        let clock = StudyClock::new();
        let mut taps = StudyTaps { clock, captures: captures.clone(), live: vec![], store: Arc::new(StdMutex::new(store)) };
        // One anchor for the whole study ([`StudyClock`]), so two consoles' lines order against
        // each other without wall-clock steps.
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
                let stamp = clock.now();
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
    // The submitted study is kept beside its record, so a count can be rendered again later.
    match serde_json::to_vec_pretty(study) {
        Ok(b) => {
            if let Err(e) = std::fs::write(dir.join(STUDY_FILE), b) {
                tracing::warn!("could not keep the submitted study beside its record: {e}");
            }
        }
        Err(e) => tracing::warn!("could not serialise the submitted study: {e}"),
    }
    let clock = taps.as_ref().map(|t| t.clock).unwrap_or_else(StudyClock::new);
    let mut write_err: Option<String> = None;
    let end = execute(study, backend, cancel, clock, |rec| {
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

/// The real backend: `hw_lock` for each `Exchange` step, then the exchange route's own body; for a
/// `Prompt`, the shared prompt slot and the study's event channel, and no hardware at all.
struct CoreBackend {
    state: AppState,
    handle: tokio::runtime::Handle,
    study_id: String,
    /// Prompts published so far; the next one's id is this plus one.
    prompts: u32,
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

    fn instrument(&mut self, name: &str, req: InstrumentExchange) -> Result<InstrumentReply, ExchangeFailure> {
        let _guard = self
            .handle
            .block_on(crate::api::acquire_hw_lock(&self.state, "POST /host-studies"))
            .map_err(|(_, msg)| ExchangeFailure::Failed(msg))?;
        crate::api::run_instrument_exchange(name, req).map_err(|(status, msg)| match status {
            StatusCode::CONFLICT | StatusCode::NOT_FOUND => ExchangeFailure::PortUnavailable(msg),
            _ => ExchangeFailure::Failed(msg),
        })
    }

    /// No `hw_lock`: a prompt touches no hardware, and holding the lock while a person walks to
    /// the bench would refuse every flash for that long.
    fn prompt(&mut self, req: PromptRequest, cancel: Option<&AtomicBool>) -> PromptRun {
        self.prompts += 1;
        let opened = now_ms();
        let pending = PendingPrompt {
            prompt_id: self.prompts,
            list: req.list,
            step_index: req.step_index,
            step_name: req.step_name,
            text: req.text,
            opened_utc_ms: opened,
            deadline_utc_ms: opened + req.timeout_ms as u64,
        };
        *self.state.host_prompt.lock().unwrap() = Some(PromptSlot { study_id: self.study_id.clone(), pending: pending.clone(), answer: None });
        let _ = self.state.study_events.send(StudyEvent::PromptPending { study_id: self.study_id.clone(), prompt: pending.clone() });
        let end = wait_for_answer(&self.state.host_prompt, pending.prompt_id, Duration::from_millis(req.timeout_ms as u64), cancel);
        let _ = self.state.study_events.send(StudyEvent::PromptResolved {
            study_id: self.study_id.clone(),
            prompt_id: pending.prompt_id,
            answer: end.answer_str(),
            answered_by: end.answered_by(),
            resolved_utc_ms: now_ms(),
        });
        PromptRun { end, prompt_id: pending.prompt_id, deadline_utc_ms: pending.deadline_utc_ms }
    }
}

/// Releases the study lock, the cancel slot and the prompt slot when the job's thread ends,
/// however it ends.
struct JobHold {
    lock: crate::study::StudyLock,
    cancel: HostCancel,
    prompt: HostPrompt,
}

impl Drop for JobHold {
    fn drop(&mut self) {
        *self.prompt.lock().unwrap() = None;
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
    let hold = JobHold { lock: state.study_lock.clone(), cancel: state.host_cancel.clone(), prompt: state.host_prompt.clone() };

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
            let mut backend = CoreBackend { state, handle, study_id: id.clone(), prompts: 0 };
            run_job(&study, &id, &dir, &mut backend, &cancel, &jobs, &events_tx, taps);
        })
        .await;
        if let Err(e) = outcome {
            crate::study::fail_job(&jobs_for_panic, &tx_for_panic, &id_for_panic, format!("host study task panicked: {e:?}"));
        }
    });

    Ok((StatusCode::ACCEPTED, Json(StudyAcceptedResponse { study_id, status: "accepted" })))
}

// ---- the per-pass line-count view (core/116) ---------------------------------

#[derive(Debug, Serialize)]
pub struct LineCountsResponse {
    study_id: String,
    line_counts: Vec<crate::host_counts::CountView>,
}

#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LineCountsBody {
    line_counts: Vec<embarch_study_designer::LineCount>,
}

/// Reads one tap's raw bytes and arrival rows (offsets rebased onto what the raw file still holds).
/// A tap's bytes and its `(byte_offset, core_rx_utc_ms)` rows.
type TapFiles = (Vec<u8>, Vec<(u64, u64)>);

fn read_tap(streams_dir: &FsPath, name: &str) -> Result<Option<TapFiles>, String> {
    let index = crate::stream_store::read_index(streams_dir).map_err(|e| format!("{e:#}"))?;
    let Some(entry) = index.as_ref().and_then(|i| i.find(name)) else { return Ok(None) };
    let raw = crate::stream_store::read_capture(streams_dir, &entry.raw_file, false).map_err(|e| format!("{e:#}"))?.unwrap_or_default();
    let Some(arrival) = entry.arrival_file.as_deref() else { return Ok(Some((raw, vec![]))) };
    let csv = crate::stream_store::read_capture(streams_dir, arrival, true).map_err(|e| format!("{e:#}"))?.unwrap_or_default();
    let (csv, _) = crate::stream_store::rebase_byte_arrivals(&csv, raw.len() as u64);
    let rows = String::from_utf8_lossy(&csv)
        .lines()
        .skip(1)
        .filter_map(|l| {
            let mut it = l.splitn(3, ',');
            Some((it.next()?.parse().ok()?, it.next()?.parse().ok()?))
        })
        .collect();
    Ok(Some((raw, rows)))
}

/// Renders `counts` (or the stored study's own) over the finished run in `dir`. Blocking.
fn render_counts_for(
    dir: &FsPath,
    study_id: &str,
    counts: Option<Vec<embarch_study_designer::LineCount>>,
) -> Result<LineCountsResponse, (StatusCode, String)> {
    let study: HostStudy = std::fs::read(dir.join(STUDY_FILE))
        .map_err(|_| (StatusCode::NOT_FOUND, format!("study '{study_id}' has no {STUDY_FILE}: it is not a host study this Core ran, or was run before Core kept the submitted study")))
        .and_then(|b| serde_json::from_slice(&b).map_err(internal_err))?;
    let events: serde_json::Value = std::fs::read(dir.join("events.json"))
        .map_err(|_| (StatusCode::CONFLICT, format!("study '{study_id}' has not finished: counts are rendered over a finished run")))
        .and_then(|b| serde_json::from_slice(&b).map_err(internal_err))?;
    let markers: Vec<embarch_study_designer::IterationMarker> =
        serde_json::from_value(events.get("iterations").cloned().unwrap_or_default()).unwrap_or_default();
    let spans: Vec<crate::host_counts::RepeatSpan> =
        serde_json::from_value(events.get("repeats").cloned().unwrap_or_default()).unwrap_or_default();
    let counts = counts.unwrap_or_else(|| study.line_counts.clone());
    for c in &counts {
        if !study.streams.iter().any(|t| t.name.as_str() == c.tap) {
            return Err((StatusCode::BAD_REQUEST, format!("count '{}' names tap '{}', which the study does not declare", c.name, c.tap)));
        }
    }
    let streams_dir = dir.join(crate::stream_store::STREAMS_DIR);
    let mut taps = std::collections::HashMap::new();
    for c in &counts {
        if !taps.contains_key(&c.tap) {
            let t = read_tap(&streams_dir, &c.tap).map_err(|e| internal_err(anyhow::anyhow!(e)))?;
            taps.insert(c.tap.clone(), t);
        }
    }
    let tap_of = |name: &str| {
        taps.get(name).and_then(|t| t.as_ref()).map(|(raw, arrivals)| crate::host_counts::TapInput { raw, arrivals })
    };
    let line_counts = crate::host_counts::render(&counts, &tap_of, &markers, &spans).map_err(|m| (StatusCode::BAD_REQUEST, m))?;
    Ok(LineCountsResponse { study_id: study_id.to_string(), line_counts })
}

/// `GET /study/{id}/line-counts`: the study's own `line_counts`, counted over the finished run's
/// raw tap files, per pass and in total. A view: no expected value, no verdict.
// route: GET /study/{study_id}/line-counts
pub async fn line_counts_handler(Path(study_id): Path<String>) -> Result<Json<LineCountsResponse>, (StatusCode, String)> {
    let dir = crate::study::study_results_dir(&study_id).map_err(internal_err)?;
    tokio::task::spawn_blocking(move || render_counts_for(&dir, &study_id, None)).await.map_err(internal_err)?.map(Json)
}

/// `POST /study/{id}/line-counts`: the same view with the counts in the body, so a corrected
/// pattern is counted over the finished run without running the bench again. Nothing is stored.
// route: POST /study/{study_id}/line-counts
pub async fn line_counts_rerender_handler(
    Path(study_id): Path<String>,
    Json(body): Json<LineCountsBody>,
) -> Result<Json<LineCountsResponse>, (StatusCode, String)> {
    let dir = crate::study::study_results_dir(&study_id).map_err(internal_err)?;
    tokio::task::spawn_blocking(move || render_counts_for(&dir, &study_id, Some(body.line_counts)))
        .await
        .map_err(internal_err)?
        .map(Json)
}

#[derive(Debug, Serialize)]
pub struct PromptAnswerResponse {
    study_id: String,
    prompt_id: u32,
    answer: PromptAnswer,
    answered_by: AnsweredBy,
    status: &'static str,
}

/// `POST /study/{id}/prompt`: a person's answer to the host study's pending `Prompt`, relayed by
/// the UI, an MCP tool or the CLI (decision 105). `200` once recorded; the step ends within
/// [`CANCEL_POLL`]. `409` for an answer naming a prompt that is not the one waiting, one already
/// answered, a running study with no prompt waiting, or a study not running; `404` for an id Core
/// has never seen. **Not lease-gated**: the person at the bench answers from whichever surface is
/// in front of them, and the prompt id is what keeps a stale answer from landing on a later prompt.
// route: POST /study/{study_id}/prompt
pub async fn answer_prompt_handler(
    State(state): State<AppState>,
    Path(study_id): Path<String>,
    Json(body): Json<PromptAnswerBody>,
) -> Result<Json<PromptAnswerResponse>, (StatusCode, String)> {
    match record_answer(&state.host_prompt, &study_id, &body) {
        Ok(true) => {
            return Ok(Json(PromptAnswerResponse {
                study_id,
                prompt_id: body.prompt_id,
                answer: body.answer,
                answered_by: body.answered_by,
                status: "recorded",
            }))
        }
        Err(msg) => return Err((StatusCode::CONFLICT, msg)),
        Ok(false) => {}
    }
    let job = state.study_jobs.lock().unwrap().get(&study_id).cloned();
    match job {
        Some(j) if j.status == "running" => Err((
            StatusCode::CONFLICT,
            format!("study '{study_id}' has no prompt waiting: it was answered, it timed out, or the study is a bench study"),
        )),
        Some(j) => Err((StatusCode::CONFLICT, format!("study '{study_id}' is not running (status: {}); there is no prompt to answer", j.status))),
        None => Err((StatusCode::NOT_FOUND, format!("no study '{study_id}' is known to this Core"))),
    }
}

/// The pending prompt of `study_id` for `GET /study/{id}`, or `None`. One already answered and
/// not yet picked up by the executor (at most [`CANCEL_POLL`]) is not shown.
pub fn pending_prompt(slot: &HostPrompt, study_id: &str) -> Option<PendingPrompt> {
    slot.lock().unwrap().as_ref().filter(|s| s.study_id == study_id && s.answer.is_none()).map(|s| s.pending.clone())
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
        instrument_replies: VecDeque<InstrumentScript>,
        /// How each `Prompt` ends, in order; an unscripted one times out.
        prompt_ends: VecDeque<PromptEnd>,
        /// What each `Prompt` asked.
        prompts: Vec<PromptRequest>,
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
            Script {
                replies: replies.into(),
                written: vec![],
                before: Box::new(|_| {}),
                calls: 0,
                instrument_replies: VecDeque::new(),
                prompt_ends: VecDeque::new(),
                prompts: vec![],
            }
        }
    }

    /// A scripted instrument answer: `(reply, terminated, ack)`, or a failure.
    type InstrumentScript = Result<(&'static str, bool, Option<Ack>), ExchangeFailure>;

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

        fn instrument(&mut self, name: &str, req: InstrumentExchange) -> Result<InstrumentReply, ExchangeFailure> {
            self.written.push(format!("{name}: {}", req.line));
            let (reply, terminated, ack) = self.instrument_replies.pop_front().unwrap_or(Ok(("", false, None)))?;
            Ok(InstrumentReply {
                instrument: name.into(),
                port: "FAKE".into(),
                baud: 9600,
                line: req.line,
                query: req.query,
                reply: reply.into(),
                terminated,
                ack,
                flush_reply: String::new(),
                paced_ms: 0,
                elapsed_ms: 1,
                started_utc_ms: 1,
            })
        }

        fn prompt(&mut self, req: PromptRequest, cancel: Option<&AtomicBool>) -> PromptRun {
            self.prompts.push(req);
            let end = match self.prompt_ends.pop_front() {
                Some(end) => end,
                None if cancel.is_some_and(|c| c.load(Ordering::SeqCst)) => PromptEnd::StudyCancelled,
                None => PromptEnd::TimedOut,
            };
            PromptRun { end, prompt_id: self.prompts.len() as u32, deadline_utc_ms: 1_000 * self.prompts.len() as u64 }
        }
    }

    fn inst(name: &str, line: &str, query: bool) -> HostStep {
        HostStep {
            name: name.into(),
            action: HostAction::Instrument { instrument: "eload".into(), line: line.into(), query },
            timeout_ms: 500,
            continue_on_fail: true,
            delay_before_ms: 0,
        }
    }

    #[test]
    fn instrument_steps_pass_on_a_reply_or_an_ack_and_fail_unacknowledged() {
        let s = study(
            vec![
                inst("idn", "*IDN?", true),
                inst("on", "CH1:SW ON", false),
                inst("bad", "CH1:SW MAYBE", false),
                inst("refused", "VOLT1:VMAX 999", false),
                inst("quiet", "MEAS1:ALL?", true),
                inst("plain", "SYST:LOCA", false),
                inst("gone", "*IDN?", true),
            ],
            vec![],
        );
        assert!(check_study(&s).is_ok());
        let mut b = Script::new(vec![]);
        b.instrument_replies = VecDeque::from([
            Ok(("R ET5420A+", true, None)),
            Ok(("Rexecu success", true, Some(Ack::Ok))),
            Ok(("Rcmd err", true, Some(Ack::Unacknowledged))),
            Ok(("Rexecu err", true, Some(Ack::Refused))),
            Ok(("", false, None)),
            Ok(("", false, None)),
            Err(ExchangeFailure::PortUnavailable("instrument 'eload' is not enumerated now".into())),
        ]);
        let (_, recs) = run(&s, &mut b, &AtomicBool::new(false));
        let got: Vec<(&str, bool)> = recs.iter().map(|r| (r.detail, matches!(r.result.outcome, Outcome::Pass))).collect();
        assert_eq!(
            got,
            vec![
                (detail::REPLIED, true),
                (detail::ACKNOWLEDGED, true),
                (detail::UNACKNOWLEDGED, false),
                (detail::REFUSED, false),
                (detail::EMPTY_REPLY, false),
                (detail::WRITTEN, true),
                (detail::PORT_UNAVAILABLE, false),
            ]
        );
        // The reply is recorded raw, prefix included; nothing is parsed.
        assert_eq!(recs[0].result.reply, "R ET5420A+");
        assert!(recs[2].note.as_deref().unwrap().contains("Rcmd err"));
        assert_eq!(b.written[0], "eload: *IDN?");
    }

    #[test]
    fn an_instrument_step_is_checked_at_submission_and_takes_a_value() {
        let repeat = HostStep {
            name: "loads".into(),
            action: HostAction::Repeat { count: None, values: vec!["1".into(), "2".into()], steps: vec![inst("set", "CURR1 {value}", false)] },
            timeout_ms: 0,
            continue_on_fail: false,
            delay_before_ms: 0,
        };
        assert!(check_study(&study(vec![repeat], vec![])).is_ok());
        // Two commands in one line: the study's own check refuses the control character first,
        // and `instrument::check` would refuse it after.
        assert!(check_study(&study(vec![inst("two", "A\nB", false)], vec![])).is_err());
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

    fn ask(name: &str, timeout_ms: u32) -> HostStep {
        HostStep { name: name.into(), action: HostAction::Prompt { text: format!("{name}: plug in the pack") }, timeout_ms, continue_on_fail: false, delay_before_ms: 0 }
    }

    // ---- prompts (decision 105) ----

    #[test]
    fn a_confirmed_prompt_passes_and_records_who_answered_and_the_deadline() {
        let s = study(vec![ask("plug", 5_000), wait("w", 1)], vec![]);
        let mut b = Script::new(vec![]);
        b.prompt_ends.push_back(PromptEnd::Answered(PromptAnswer::Confirmed, AnsweredBy::Ui));
        let (end, recs) = run(&s, &mut b, &AtomicBool::new(false));
        assert!(end.completed, "{:?}", end.reason);
        assert_eq!(recs[0].result.outcome, Outcome::Pass);
        assert_eq!(recs[0].detail, detail::CONFIRMED);
        assert_eq!(recs[0].result.answered_by.as_deref(), Some("ui"));
        assert_eq!((recs[0].prompt_id, recs[0].prompt_deadline_utc_ms), (Some(1), Some(1_000)));
        assert_eq!(b.prompts[0].text, "plug: plug in the pack");
        assert_eq!((b.prompts[0].list, b.prompts[0].step_index, b.prompts[0].timeout_ms), (HostList::Steps, 0, 5_000));
        // Not a prompt: no prompt fields in the record.
        assert_eq!(recs[1].prompt_id, None);
        let row = serde_json::to_value(&recs[1]).unwrap();
        assert!(row.get("prompt_id").is_none() && row.get("prompt_deadline_utc_ms").is_none());
    }

    #[test]
    fn a_cancelled_prompt_fails_the_step_stops_the_study_and_runs_the_teardown() {
        let s = study(vec![ask("plug", 5_000), wait("never", 1)], vec![ex("safe", "pd sink 5", "pd: ")]);
        let mut b = Script::new(vec![ok_reply("pd sink 5", "pd: ok")]);
        b.prompt_ends.push_back(PromptEnd::Answered(PromptAnswer::Cancelled, AnsweredBy::Mcp));
        let (end, recs) = run(&s, &mut b, &AtomicBool::new(false));
        assert!(!end.completed);
        assert!(end.reason.as_deref().unwrap().contains("'plug' failed"), "{:?}", end.reason);
        assert!(matches!(recs[0].result.outcome, Outcome::Fail { .. }));
        assert_eq!(recs[0].detail, detail::PROMPT_CANCELLED);
        assert_eq!(recs[0].result.answered_by.as_deref(), Some("mcp"));
        assert_eq!(recs.iter().map(|r| (r.list, r.index)).collect::<Vec<_>>(), [(HostList::Steps, 0), (HostList::Teardown, 0)]);
    }

    #[test]
    fn an_unanswered_prompt_times_out_and_continue_on_fail_walks_past_it() {
        let mut lenient = ask("plug", 5_000);
        lenient.continue_on_fail = true;
        let s = study(vec![lenient, ask("strict", 5_000), wait("never", 1)], vec![]);
        let mut b = Script::new(vec![]);
        let (end, recs) = run(&s, &mut b, &AtomicBool::new(false));
        assert_eq!(recs.len(), 2);
        assert!(recs.iter().all(|r| r.result.outcome == Outcome::TimedOut && r.detail == detail::PROMPT_TIMED_OUT));
        assert!(recs.iter().all(|r| r.result.answered_by.is_none()));
        assert_eq!(recs[1].prompt_id, Some(2));
        assert!(end.reason.as_deref().unwrap().contains("'strict' timed out (prompt_timed_out)"), "{:?}", end.reason);
    }

    #[test]
    fn a_study_cancelled_under_a_prompt_ends_it_and_a_teardown_prompt_still_asks() {
        let s = study(vec![ask("plug", 5_000)], vec![ask("unplug", 5_000)]);
        let mut b = Script::new(vec![]);
        b.prompt_ends.extend([PromptEnd::StudyCancelled, PromptEnd::Answered(PromptAnswer::Confirmed, AnsweredBy::Cli)]);
        let (end, recs) = run(&s, &mut b, &AtomicBool::new(false));
        assert_eq!(recs[0].detail, detail::CANCELLED);
        assert!(end.reason.as_deref().unwrap().contains("cancelled during step 'plug'"), "{:?}", end.reason);
        // The teardown's own prompt ran, and was asked with no cancel flag to see.
        assert_eq!((recs[1].list, recs[1].detail), (HostList::Teardown, detail::CONFIRMED));
        assert_eq!(recs[1].result.answered_by.as_deref(), Some("cli"));
        assert_eq!(end.teardown_failures, 0);
    }

    fn slot_with(study_id: &str, prompt_id: u32) -> HostPrompt {
        let pending = PendingPrompt {
            prompt_id,
            list: HostList::Steps,
            step_index: 0,
            step_name: "plug".into(),
            text: "plug in".into(),
            opened_utc_ms: 1,
            deadline_utc_ms: 2,
        };
        Arc::new(StdMutex::new(Some(PromptSlot { study_id: study_id.into(), pending, answer: None })))
    }

    fn body(prompt_id: u32, answer: PromptAnswer) -> PromptAnswerBody {
        PromptAnswerBody { prompt_id, answer, answered_by: AnsweredBy::Ui }
    }

    #[test]
    fn an_answer_is_recorded_once_and_only_against_the_prompt_it_names() {
        let slot = slot_with("h1", 3);
        assert_eq!(record_answer(&slot, "other", &body(3, PromptAnswer::Confirmed)), Ok(false));
        let e = record_answer(&slot, "h1", &body(2, PromptAnswer::Confirmed)).unwrap_err();
        assert!(e.contains("prompt 3 is ('plug in')"), "{e}");
        assert_eq!(record_answer(&slot, "h1", &body(3, PromptAnswer::Cancelled)), Ok(true));
        let e = record_answer(&slot, "h1", &body(3, PromptAnswer::Confirmed)).unwrap_err();
        assert!(e.contains("already answered"), "{e}");
        // An answered prompt is no longer shown as pending.
        assert_eq!(pending_prompt(&slot, "h1"), None);
        assert_eq!(wait_for_answer(&slot, 3, Duration::from_secs(5), None), PromptEnd::Answered(PromptAnswer::Cancelled, AnsweredBy::Ui));
        assert!(slot.lock().unwrap().is_none(), "the executor clears the slot it picked the answer from");
    }

    #[test]
    fn the_wait_ends_on_an_answer_from_another_thread_a_deadline_or_a_cancel() {
        let slot = slot_with("h1", 1);
        assert!(pending_prompt(&slot, "h1").is_some() && pending_prompt(&slot, "h2").is_none());
        let answerer = {
            let slot = slot.clone();
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(60));
                record_answer(&slot, "h1", &body(1, PromptAnswer::Confirmed))
            })
        };
        let t0 = Instant::now();
        assert_eq!(wait_for_answer(&slot, 1, Duration::from_secs(10), None), PromptEnd::Answered(PromptAnswer::Confirmed, AnsweredBy::Ui));
        assert!(t0.elapsed() < Duration::from_secs(2));
        assert_eq!(answerer.join().unwrap(), Ok(true));

        let slot = slot_with("h1", 1);
        assert_eq!(wait_for_answer(&slot, 1, Duration::from_millis(50), None), PromptEnd::TimedOut);
        assert!(slot.lock().unwrap().is_none());
        // After the deadline cleared the slot, a late answer finds no prompt rather than being lost.
        assert_eq!(record_answer(&slot, "h1", &body(1, PromptAnswer::Confirmed)), Ok(false));

        let slot = slot_with("h1", 1);
        let cancel = AtomicBool::new(true);
        assert_eq!(wait_for_answer(&slot, 1, Duration::from_secs(10), Some(&cancel)), PromptEnd::StudyCancelled);
    }

    #[test]
    fn prompt_events_and_the_status_body_carry_the_prompt_flat() {
        let pending = slot_with("h1", 4).lock().unwrap().clone().unwrap().pending;
        let ev = serde_json::to_value(StudyEvent::PromptPending { study_id: "h1".into(), prompt: pending.clone() }).unwrap();
        assert_eq!(ev["kind"], "PromptPending");
        assert_eq!((ev["prompt_id"].as_u64(), ev["text"].as_str(), ev["list"].as_str()), (Some(4), Some("plug in"), Some("Steps")));
        assert_eq!(ev["deadline_utc_ms"], 2);
        let ev = serde_json::to_value(StudyEvent::PromptResolved {
            study_id: "h1".into(),
            prompt_id: 4,
            answer: PromptEnd::Answered(PromptAnswer::Confirmed, AnsweredBy::Mcp).answer_str(),
            answered_by: Some(AnsweredBy::Mcp),
            resolved_utc_ms: 9,
        })
        .unwrap();
        assert_eq!((ev["answer"].as_str(), ev["answered_by"].as_str()), (Some("confirmed"), Some("mcp")));
        let b: PromptAnswerBody = serde_json::from_str(r#"{"prompt_id":4,"answer":"cancelled","answered_by":"cli"}"#).unwrap();
        assert_eq!((b.answer, b.answered_by), (PromptAnswer::Cancelled, AnsweredBy::Cli));
        assert!(serde_json::from_str::<PromptAnswerBody>(r#"{"prompt_id":4,"answer":"yes","answered_by":"cli"}"#).is_err());
        assert!(serde_json::from_str::<PromptAnswerBody>(r#"{"prompt_id":4,"answer":"confirmed","answered_by":"agent"}"#).is_err());
    }

    #[tokio::test]
    async fn the_prompt_route_records_an_answer_or_says_why_not() {
        let state = AppState::new("t".into());
        *state.host_prompt.lock().unwrap() = slot_with("h1", 1).lock().unwrap().clone();
        let ok = answer_prompt_handler(State(state.clone()), Path("h1".into()), Json(body(1, PromptAnswer::Confirmed))).await.unwrap();
        assert_eq!(ok.0.status, "recorded");
        let e = answer_prompt_handler(State(state.clone()), Path("h1".into()), Json(body(1, PromptAnswer::Confirmed))).await.unwrap_err();
        assert_eq!(e.0, StatusCode::CONFLICT);
        *state.host_prompt.lock().unwrap() = None;
        state.study_jobs.lock().unwrap().insert("r1".into(), StudyJob { status: "running".into(), current_step: None, total_steps: None, reason: None });
        state.study_jobs.lock().unwrap().insert("d1".into(), StudyJob { status: "completed".into(), current_step: None, total_steps: None, reason: None });
        let e = answer_prompt_handler(State(state.clone()), Path("r1".into()), Json(body(1, PromptAnswer::Confirmed))).await.unwrap_err();
        assert_eq!(e.0, StatusCode::CONFLICT);
        assert!(e.1.contains("no prompt waiting"), "{}", e.1);
        let e = answer_prompt_handler(State(state.clone()), Path("d1".into()), Json(body(1, PromptAnswer::Confirmed))).await.unwrap_err();
        assert_eq!(e.0, StatusCode::CONFLICT);
        let e = answer_prompt_handler(State(state), Path("nope".into()), Json(body(1, PromptAnswer::Confirmed))).await.unwrap_err();
        assert_eq!(e.0, StatusCode::NOT_FOUND);
    }

    fn study(steps: Vec<HostStep>, teardown: Vec<HostStep>) -> HostStudy {
        HostStudy { name: "t".into(), steps, teardown, streams: vec![], line_counts: vec![] }
    }

    fn run(s: &HostStudy, b: &mut Script, cancel: &AtomicBool) -> (RunEnd, Vec<StepRecord>) {
        let mut recs = vec![];
        let end = execute(s, b, cancel, StudyClock::new(), |r| recs.push(r));
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

    fn repeat_step(name: &str, count: Option<u32>, values: &[&str], inner: Vec<HostStep>) -> HostStep {
        HostStep {
            name: name.into(),
            action: HostAction::Repeat { count, values: values.iter().map(|v| v.to_string()).collect(), steps: inner },
            timeout_ms: 0,
            continue_on_fail: false,
            delay_before_ms: 0,
        }
    }

    #[test]
    fn a_repeat_substitutes_the_pass_number_or_the_entry_and_marks_each_pass() {
        let counted = repeat_step("x3", Some(3), &[], vec![ex("go", "pd hr {value}", "ok"), wait("settle", 1)]);
        let sweep = repeat_step("sweep", None, &["5", "9"], vec![ex("pps", "pd pps {value}", "ok")]);
        let s = study(vec![wait("first", 1), counted, sweep], vec![]);
        let mut b = Script::new(vec![
            ok_reply("pd hr 1", "ok"),
            ok_reply("pd hr 2", "ok"),
            ok_reply("pd hr 3", "ok"),
            ok_reply("pd pps 5", "ok"),
            ok_reply("pd pps 9", "ok"),
        ]);
        let (end, recs) = run(&s, &mut b, &AtomicBool::new(false));
        assert!(end.completed, "{:?}", end.reason);
        assert_eq!(b.written, ["pd hr 1\n", "pd hr 2\n", "pd hr 3\n", "pd pps 5\n", "pd pps 9\n"]);
        // Markers: one per pass, in order, the `values` entry kept and a `count` repeat's `None`.
        let m: Vec<_> = end.iterations.iter().map(|m| (m.repeat_index, m.pass, m.value.clone())).collect();
        assert_eq!(m, [(1, 0, None), (1, 1, None), (1, 2, None), (2, 0, Some("5".into())), (2, 1, Some("9".into()))]);
        assert!(end.iterations.windows(2).all(|w| w[0].started_utc_ms <= w[1].started_utc_ms));
        // Each inner record is tagged with its pass; a step outside a repeat is not.
        assert_eq!(recs.len(), 1 + 6 + 2);
        assert_eq!(recs[0].result.iteration, None);
        let it = recs[3].result.iteration.as_ref().unwrap();
        assert_eq!((it.repeat_index, it.pass, recs[3].inner_index), (1, 1, Some(0)));
        // The marker precedes the pass's first step on one clock; the spans cover their passes.
        assert!(end.iterations[1].started_utc_ms <= recs[3].started_utc_ms);
        assert_eq!(end.spans.len(), 2);
        assert!(end.spans[0].started_utc_ms <= end.iterations[0].started_utc_ms && end.iterations[2].started_utc_ms <= end.spans[0].ended_utc_ms);
    }

    #[test]
    fn a_failing_step_in_a_repeat_stops_the_study_and_the_teardown_still_runs() {
        let rep = repeat_step("x3", Some(3), &[], vec![ex("go", "pd hr {value}", "ok")]);
        let s = study(vec![rep], vec![ex("down", "pd off", "ok")]);
        // Pass 1 passes, pass 2 gets no reply.
        let mut b = Script::new(vec![ok_reply("pd hr 1", "ok"), Ok(vec![]), ok_reply("pd off", "ok")]);
        let (end, recs) = run(&s, &mut b, &AtomicBool::new(false));
        assert!(!end.completed);
        assert_eq!(b.written, ["pd hr 1\n", "pd hr 2\n", "pd off\n"]);
        assert_eq!(end.iterations.len(), 2);
        assert_eq!(recs.last().unwrap().list, HostList::Teardown);
        assert_eq!(end.spans.len(), 1);
    }

    #[test]
    fn a_cancel_before_a_pass_skips_the_rest_of_the_repeat() {
        let rep = repeat_step("x4", Some(4), &[], vec![ex("go", "x {value}", "ok")]);
        let s = study(vec![rep], vec![]);
        let cancel = Arc::new(AtomicBool::new(false));
        let c2 = cancel.clone();
        let mut b = Script::new((1..=4).map(|n| ok_reply(&format!("x {n}"), "ok")).collect());
        b.before = Box::new(move |n| {
            if n == 1 {
                c2.store(true, Ordering::SeqCst);
            }
        });
        let (end, _) = run(&s, &mut b, &cancel);
        assert!(!end.completed);
        // Pass 2's exchange ran (the flag was set as it started); pass 3 never began.
        assert_eq!(end.iterations.len(), 2);
    }

    #[test]
    fn what_this_core_does_not_run_is_refused_by_name_at_submission() {
        let s = study(vec![ex("a", "x", "pd: ")], vec![]);
        assert!(check_study(&s).is_ok());
        // A Prompt runs since decision 105, at top level, inside a Repeat and in the teardown; one
        // asking to wait longer than an hour is refused by step.
        let prompt = ask("ask", 1000);
        assert!(check_study(&study(vec![ex("a", "x", "pd: "), prompt.clone()], vec![prompt.clone()])).is_ok());
        let repeat = |inner: HostStep| HostStep { name: "loop".into(), action: HostAction::Repeat { count: Some(2), values: vec![], steps: vec![inner] }, timeout_ms: 0, continue_on_fail: false, delay_before_ms: 0 };
        assert!(check_study(&study(vec![repeat(ex("i", "x {value}", "pd: "))], vec![])).is_ok());
        assert!(check_study(&study(vec![repeat(prompt)], vec![])).is_ok());
        let e = check_study(&study(vec![repeat(ask("forever", MAX_PROMPT_TIMEOUT_MS + 1))], vec![])).unwrap_err();
        assert!(e.contains("steps[0.0] 'forever'") && e.contains("decision 105"), "{e}");
        assert!(check_study(&study(vec![ask("hour", MAX_PROMPT_TIMEOUT_MS)], vec![])).is_ok());
        // An Instrument step runs too (decision 104).
        assert!(check_study(&study(vec![ex("a", "x", "pd: ")], vec![HostStep { name: "i".into(), action: HostAction::Instrument { instrument: "load".into(), line: "OUTP OFF".into(), query: false }, timeout_ms: 500, continue_on_fail: false, delay_before_ms: 0 }])).is_ok());
        let mut with_counts = study(vec![ex("a", "x", "pd: ")], vec![]);
        with_counts.line_counts.push(embarch_study_designer::host_study::LineCount { name: "n".into(), tap: "t".into(), pattern: "p".into(), kind: Default::default() });
        // A count on a tap the study does not declare is the study's own check.
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

        fn instrument(&mut self, _name: &str, _req: InstrumentExchange) -> Result<InstrumentReply, ExchangeFailure> {
            Err(ExchangeFailure::Failed("no instrument in this test".into()))
        }

        fn prompt(&mut self, _req: PromptRequest, _cancel: Option<&AtomicBool>) -> PromptRun {
            PromptRun { end: PromptEnd::TimedOut, prompt_id: 1, deadline_utc_ms: 0 }
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
        let end = execute(&s, &mut b, &AtomicBool::new(false), StudyClock::new(), |r| recs.push(r));
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

    /// The fixture study of `embarch-study-designer` (8 Hard Resets, four patterns, two taps)
    /// rendered over a canned capture written through the real stream store.
    #[test]
    fn the_fixture_study_renders_its_eight_counts_from_the_raw_files() {
        use crate::host_counts::PassCount;
        let study: HostStudy = serde_json::from_str(include_str!("../../embarch-study-designer/tests/fixtures/host_pd_hard_reset_counts.json")).unwrap();
        study.validate().unwrap();
        assert_eq!(study.line_counts.len(), 8);
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(STUDY_FILE), serde_json::to_vec(&study).unwrap()).unwrap();
        // Hard Reset x8: pass k starts at 1000 + 6000 k (repeat 1, ends 49000); PPS sweep: 7 passes
        // from 49000 every 3000 (repeat 2, ends 70000).
        let mut iterations = vec![];
        for k in 0..8u64 {
            iterations.push(serde_json::json!({"list": "Steps", "repeat_index": 1, "pass": k, "value": null, "started_utc_ms": 1000 + 6000 * k}));
        }
        for (k, v) in ["5", "9", "12", "15", "18", "20", "21"].iter().enumerate() {
            iterations.push(serde_json::json!({"list": "Steps", "repeat_index": 2, "pass": k, "value": v, "started_utc_ms": 49000 + 3000 * k as u64}));
        }
        let repeats = serde_json::json!([
            {"list": "Steps", "repeat_index": 1, "name": "hard reset x8", "started_utc_ms": 1000, "ended_utc_ms": 48999},
            {"list": "Steps", "repeat_index": 2, "name": "PPS sweep", "started_utc_ms": 49000, "ended_utc_ms": 70000}
        ]);
        std::fs::write(
            dir.path().join("events.json"),
            serde_json::to_vec(&serde_json::json!({"kind": "host", "study_name": study.name, "steps": [], "iterations": iterations, "repeats": repeats})).unwrap(),
        )
        .unwrap();
        {
            let mut st = crate::stream_store::StreamStore::create(dir.path(), &study.streams, &[], crate::stream_store::stream_max_bytes()).unwrap();
            let mut put = |id: u8, t: u64, text: &str| {
                st.write_raw(id, text.as_bytes());
                st.note_arrival(id, text.as_bytes(), t, t);
            };
            put(0, 100, "pd: Hard Reset (boot)\n"); // before pass 1: outside
            for k in 0..8u64 {
                let t = 1000 + 6000 * k;
                if k == 4 {
                    // One line in two records: its first byte lands in pass 4, its tail 3.9 s later.
                    put(0, t + 100, "pd: Hard Re");
                    put(0, t + 4000, "set received\n");
                } else {
                    put(0, t + 100, "pd: Hard Reset received\n");
                }
                put(0, t + 1500, "pd: contract in force 5V\n");
                if k == 2 {
                    put(0, t + 2000, "pd: rx overrun\n");
                    put(0, t + 2100, "pd: rx overrun\n");
                }
                if k != 2 {
                    put(1, t + 200, "dut: Hard Reset\n");
                }
                put(1, t + 1600, "dut: contract in force\n");
            }
            for k in 0..7u64 {
                put(0, 49000 + 3000 * k + 500, "pd: contract in force 9V\n");
            }
            put(1, 80000, "dut: rx message error\n"); // after the last pass: outside
        }
        let r = render_counts_for(dir.path(), "s1", None).unwrap();
        assert_eq!(r.line_counts.len(), 8);
        let by = |n: &str| r.line_counts.iter().find(|c| c.name == n).unwrap();
        let passes = |c: &crate::host_counts::CountView, rep: usize| c.repeats[rep].passes.iter().map(|p: &PassCount| p.count).collect::<Vec<_>>();

        let hr = by("Hard Reset (pd-bench console)");
        // 8 in passes, once each (the split line is one, in pass 4), plus the boot line outside.
        assert_eq!((hr.total, hr.outside_passes), (9, 1));
        assert_eq!(passes(hr, 0), [1; 8]);
        assert_eq!(passes(hr, 1), [0; 7]);
        // 1 boot + 8 resets + 8 contracts + 2 overruns + 7 PPS contracts.
        assert_eq!(hr.tap_lines, 26);
        let cf = by("contract in force (pd-bench console)");
        assert_eq!((cf.total, cf.outside_passes), (15, 0));
        assert_eq!((passes(cf, 0), passes(cf, 1)), (vec![1; 8], vec![1; 7]));
        assert_eq!(cf.repeats[1].passes.iter().map(|p| p.value.clone().unwrap()).collect::<Vec<_>>(), ["5", "9", "12", "15", "18", "20", "21"]);
        let ov = by("rx overrun (pd-bench console)");
        assert_eq!((ov.total, passes(ov, 0)), (2, vec![0, 0, 2, 0, 0, 0, 0, 0]));
        assert_eq!(by("rx message error (pd-bench console)").total, 0);
        let dh = by("Hard Reset (dut console)");
        assert_eq!((dh.total, passes(dh, 0)), (7, vec![1, 1, 0, 1, 1, 1, 1, 1]));
        assert_eq!(by("contract in force (dut console)").total, 8);
        let de = by("rx message error (dut console)");
        assert_eq!((de.total, de.outside_passes), (1, 1));
        assert_eq!(by("rx overrun (dut console)").total, 0);

        // A corrected pattern is counted over the same finished run, no bench needed.
        let fixed = vec![embarch_study_designer::LineCount { name: "reset, either word order".into(), tap: "pd-bench console".into(), pattern: "Hard Re.*received".into(), kind: embarch_study_designer::PatternKind::Regex }];
        let r2 = render_counts_for(dir.path(), "s1", Some(fixed)).unwrap();
        assert_eq!((r2.line_counts[0].total, r2.line_counts[0].outside_passes), (8, 0));
        // A tap the study never declared is a 400.
        let bad = vec![embarch_study_designer::LineCount { name: "n".into(), tap: "nope".into(), pattern: "x".into(), kind: Default::default() }];
        assert_eq!(render_counts_for(dir.path(), "s1", Some(bad)).unwrap_err().0, StatusCode::BAD_REQUEST);
    }
}
