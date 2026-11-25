//! A device study's session: the Start prompt before the study starts, the Done prompt while it
//! samples, the cap, and the run fields saved beside the results (decision 127).
//!
//! **One prompt machinery.** The prompts live in the host study's slot ([`HostPrompt`]) and go out
//! as `PromptPending` / `PromptResolved` on the study's own event stream; a person answers them
//! with `POST /study/{id}/prompt`, which `embarch-api`'s `answer_prompt` and the UI's prompt modal
//! already call. What a session adds rides on that route as optional fields: a prompt's `session`
//! (which phase, and the run fields) and an answer's `run_fields`.

use axum::http::StatusCode;
use embarch_study_designer::{validate_run_answers, RunFieldValue, SessionSpec};
use serde::{Deserialize, Serialize};
use std::path::Path as FsPath;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use tokio::sync::broadcast;

use crate::host_study::{now_ms, AnsweredBy, HostPrompt, PendingPrompt, PromptAnswer, PromptAnswerBody, PromptSlot, CANCEL_POLL};
use crate::study::StudyEvent;
use crate::token_store::Scope;
use embarch_study_designer::HostList;

/// The file beside a session study's results: the run fields and how the session ended.
pub const SESSION_FILE: &str = "session.json";

/// How long a Start prompt waits for a person before the study is refused. The same hour decision
/// 105 caps a host study's prompt at, for the same reason: a waiting prompt holds the study lock,
/// so a forgotten one has to end on its own. Reasoned, not measured.
pub const START_TIMEOUT: Duration = Duration::from_secs(60 * 60);

/// The prompt ids of a session's two prompts: the study has no other prompt.
const START_PROMPT_ID: u32 = 1;
const DONE_PROMPT_ID: u32 = 2;

/// The name a session prompt carries where a host study's `Prompt` carries its step's.
const START_STEP_NAME: &str = "session start";
const DONE_STEP_NAME: &str = "session done";

/// Which of a session's two prompts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionPhase {
    Start,
    Done,
}

/// One run field as a showing surface needs it to draw the form.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RunFieldInfo {
    pub name: String,
    pub label: String,
    pub required: bool,
}

/// What a prompt says about itself when it belongs to a session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SessionPromptInfo {
    pub phase: SessionPhase,
    /// The Start prompt's run fields, in declared order; empty on Done.
    pub run_fields: Vec<RunFieldInfo>,
}

/// One answered run field as it arrives over HTTP.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunFieldAnswer {
    pub name: String,
    pub value: String,
}

/// A session prompt's slot half: the session it is for and, once a Start answer was accepted,
/// the run fields it carried.
#[derive(Debug, Clone)]
pub struct SessionSlot {
    spec: SessionSpec,
    run_values: Vec<RunFieldValue>,
}

fn refuse(status: StatusCode, msg: impl Into<String>) -> (StatusCode, String) {
    (status, msg.into())
}

impl SessionSlot {
    fn new(spec: SessionSpec) -> Self {
        SessionSlot { spec, run_values: Vec::new() }
    }

    /// Checks an answer to this session's prompt and, for an accepted Start, keeps its run fields.
    ///
    /// **Who may answer.** Start takes the person's token only: the run fields label a recording
    /// as that person's, and an agent would be inventing the sensor serial and the room, so the
    /// agent's token is `403` (as it is for discarding a run it cannot vouch for). Done takes
    /// either token: it only ends a capture the cap would end anyway, so an agent that started
    /// the run may stop it. **Why a refusal leaves the prompt pending:** the person corrects the
    /// form and answers again.
    pub fn check_answer(&mut self, phase: SessionPhase, body: &PromptAnswerBody, scope: Scope) -> Result<(), (StatusCode, String)> {
        match phase {
            SessionPhase::Start => {
                if scope == Scope::Agent {
                    return Err(refuse(
                        StatusCode::FORBIDDEN,
                        "the session's Start prompt takes the person's token: its run fields label the recording as that person's, so an agent's token may not answer it",
                    ));
                }
                if body.answer == PromptAnswer::Cancelled {
                    return Ok(());
                }
                let values = run_values_of(&body.run_fields)?;
                validate_run_answers(&self.spec, &values).map_err(|e| refuse(StatusCode::BAD_REQUEST, e.to_string()))?;
                self.run_values = values;
                Ok(())
            }
            SessionPhase::Done => {
                if body.answer == PromptAnswer::Cancelled {
                    return Err(refuse(StatusCode::BAD_REQUEST, "the session's Done prompt can only be confirmed: it ends the capture, and there is nothing to cancel"));
                }
                if !body.run_fields.is_empty() {
                    return Err(refuse(StatusCode::BAD_REQUEST, "the session's Done prompt asks for no run fields"));
                }
                Ok(())
            }
        }
    }
}

/// An answer's run fields as the study-designer's bounded type, refusing one that does not fit.
fn run_values_of(answers: &[RunFieldAnswer]) -> Result<Vec<RunFieldValue>, (StatusCode, String)> {
    answers
        .iter()
        .enumerate()
        .map(|(i, a)| {
            let name = heapless::String::try_from(a.name.as_str())
                .map_err(|_| refuse(StatusCode::BAD_REQUEST, format!("run_fields[{i}]'s name is longer than a run field name can be")))?;
            let value = heapless::String::try_from(a.value.as_str()).map_err(|_| {
                refuse(
                    StatusCode::BAD_REQUEST,
                    format!("run_fields[{i}] ('{}') is longer than {} bytes", a.name, embarch_study_designer::limits::MAX_RUN_VALUE_LEN),
                )
            })?;
            Ok(RunFieldValue { name, value })
        })
        .collect()
}

/// How a Start prompt ended.
#[derive(Debug, Clone, PartialEq)]
pub enum StartOutcome {
    /// Confirmed, with run fields that passed `validate_run_answers`.
    Confirmed { run_values: Vec<RunFieldValue>, answered_by: AnsweredBy, answered_utc_ms: u64 },
    /// The person cancelled: the study never starts.
    Cancelled(AnsweredBy),
    /// Nobody answered within [`START_TIMEOUT`].
    TimedOut,
    /// `POST /study/{id}/cancel` came while the prompt waited (decision 128).
    StudyCancelled,
}

/// How a session's sampling ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionEnd {
    /// The person confirmed Done before the cap.
    Done(AnsweredBy),
    /// The cap came first.
    Cap,
    /// `POST /study/{id}/cancel` (decision 128).
    Cancelled,
    /// The bench said the study was done (every step reported) before either.
    StudyDone,
    /// The run failed.
    Failed,
}

impl SessionEnd {
    /// `PromptResolved.answer` for the Done prompt.
    fn answer_str(self) -> &'static str {
        match self {
            SessionEnd::Done(_) => "confirmed",
            SessionEnd::Cap => "timed_out",
            SessionEnd::Cancelled => "study_cancelled",
            SessionEnd::StudyDone => "study_ended",
            SessionEnd::Failed => "study_failed",
        }
    }

    /// The reason `session.json` records.
    pub fn ended_by(self) -> &'static str {
        match self {
            SessionEnd::Done(_) => "done",
            SessionEnd::Cap => "cap",
            SessionEnd::Cancelled => "cancelled",
            SessionEnd::StudyDone => "study_done",
            SessionEnd::Failed => "failed",
        }
    }
}

/// Raises a study's session prompts on the study's own slot and event stream.
#[derive(Clone)]
pub struct SessionPrompter {
    prompts: HostPrompt,
    events: broadcast::Sender<StudyEvent>,
    study_id: String,
}

fn publish(p: &SessionPrompter, pending: PendingPrompt, spec: &SessionSpec) {
    *p.prompts.lock().unwrap() = Some(PromptSlot {
        study_id: p.study_id.clone(),
        pending: pending.clone(),
        answer: None,
        extra: None,
        session: Some(SessionSlot::new(spec.clone())),
    });
    let _ = p.events.send(StudyEvent::PromptPending { study_id: p.study_id.clone(), prompt: pending });
}

fn resolved(p: &SessionPrompter, prompt_id: u32, answer: &'static str, answered_by: Option<AnsweredBy>) {
    let _ = p.events.send(StudyEvent::PromptResolved {
        study_id: p.study_id.clone(),
        prompt_id,
        answer,
        answered_by,
        resolved_utc_ms: now_ms(),
    });
}

impl SessionPrompter {
    pub fn new(prompts: HostPrompt, events: broadcast::Sender<StudyEvent>, study_id: String) -> Self {
        SessionPrompter { prompts, events, study_id }
    }

    /// Raises the Start prompt and waits for an answer that passed `validate_run_answers` (an
    /// answer that fails never lands: the route refuses it and the prompt stays), for a cancel,
    /// or for [`START_TIMEOUT`]. Blocking: call it off the async runtime.
    #[cfg(test)]
    pub fn raise_start(&self, spec: &SessionSpec, timeout: Duration) -> StartOutcome {
        self.raise_start_unless_cancelled(spec, timeout, None)
    }

    /// [`Self::raise_start`], also ending when `cancel` is set (the study's cancel route): the
    /// prompt resolves `study_cancelled` and nothing was sent to the bench.
    pub fn raise_start_unless_cancelled(&self, spec: &SessionSpec, timeout: Duration, cancel: Option<&AtomicBool>) -> StartOutcome {
        let opened = now_ms();
        let pending = PendingPrompt {
            prompt_id: START_PROMPT_ID,
            list: HostList::Steps,
            step_index: 0,
            step_name: START_STEP_NAME.to_string(),
            text: spec.start_text.as_str().to_string(),
            bench_config: None,
            kind: None,
            observation: None,
            switched_on: Vec::new(),
            opened_utc_ms: opened,
            deadline_utc_ms: opened + timeout.as_millis() as u64,
            session: Some(SessionPromptInfo {
                phase: SessionPhase::Start,
                run_fields: spec
                    .run_fields
                    .iter()
                    .map(|f| RunFieldInfo { name: f.name.as_str().to_string(), label: f.label.as_str().to_string(), required: f.required })
                    .collect(),
            }),
        };
        publish(self, pending, spec);
        let deadline = Instant::now() + timeout;
        loop {
            {
                let mut g = self.prompts.lock().unwrap();
                let answered = g.as_ref().filter(|s| s.pending.prompt_id == START_PROMPT_ID).and_then(|s| s.answer);
                let end = match answered {
                    Some((PromptAnswer::Confirmed, by)) => {
                        let run_values = g.as_ref().and_then(|s| s.session.as_ref()).map(|s| s.run_values.clone()).unwrap_or_default();
                        Some((StartOutcome::Confirmed { run_values, answered_by: by, answered_utc_ms: now_ms() }, "confirmed", Some(by)))
                    }
                    Some((PromptAnswer::Cancelled, by)) => Some((StartOutcome::Cancelled(by), "cancelled", Some(by))),
                    None if cancel.is_some_and(|c| c.load(Ordering::SeqCst)) => Some((StartOutcome::StudyCancelled, "study_cancelled", None)),
                    None if Instant::now() >= deadline => Some((StartOutcome::TimedOut, "timed_out", None)),
                    None => None,
                };
                if let Some((outcome, answer, by)) = end {
                    *g = None;
                    drop(g);
                    resolved(self, START_PROMPT_ID, answer, by);
                    return outcome;
                }
            }
            std::thread::sleep(CANCEL_POLL);
        }
    }

    /// Raises the Done prompt as sampling starts, its deadline the cap, and returns the watch the
    /// run loop polls. The prompt stays up for the whole capture: answering it is how a person
    /// ends the capture early, and when the cap or the bench ends it first the prompt resolves
    /// (`timed_out` at the cap) so the person sees the end.
    pub fn begin_sampling(&self, spec: &SessionSpec, now: Instant) -> SessionWatch {
        let cap = Duration::from_millis(spec.max_duration_ms as u64);
        let opened = now_ms();
        let pending = PendingPrompt {
            prompt_id: DONE_PROMPT_ID,
            list: HostList::Steps,
            step_index: 0,
            step_name: DONE_STEP_NAME.to_string(),
            text: spec.done_text.as_str().to_string(),
            bench_config: None,
            kind: None,
            observation: None,
            switched_on: Vec::new(),
            opened_utc_ms: opened,
            deadline_utc_ms: opened + cap.as_millis() as u64,
            session: Some(SessionPromptInfo { phase: SessionPhase::Done, run_fields: Vec::new() }),
        };
        publish(self, pending, spec);
        SessionWatch { prompter: self.clone(), cap_deadline: now + cap }
    }
}

/// The run loop's view of the Done prompt and the cap. Dropping it clears the Done prompt, so no
/// exit leaves a prompt nobody can answer on screen.
pub struct SessionWatch {
    prompter: SessionPrompter,
    cap_deadline: Instant,
}

impl SessionWatch {
    /// Whether sampling must stop now: the Done prompt answered, or the cap reached. Answered
    /// wins a tie.
    pub fn poll(&self, now: Instant) -> Option<SessionEnd> {
        let answered = {
            let g = self.prompter.prompts.lock().unwrap();
            g.as_ref().filter(|s| s.pending.prompt_id == DONE_PROMPT_ID).and_then(|s| s.answer)
        };
        match answered {
            Some((_, by)) => Some(SessionEnd::Done(by)),
            None if now >= self.cap_deadline => Some(SessionEnd::Cap),
            None => None,
        }
    }

    /// The deadline to give the dev-bench link's `recv`: the watchdog's own, cut short so the
    /// loop looks at the Done prompt every [`CANCEL_POLL`] and never sleeps past the cap.
    pub fn recv_deadline(&self, watchdog: Instant, now: Instant) -> Instant {
        watchdog.min(self.cap_deadline).min(now + CANCEL_POLL)
    }

    /// Ends the Done prompt on screen with `end`'s answer.
    pub fn resolve(self, end: SessionEnd) {
        let by = match end {
            SessionEnd::Done(by) => Some(by),
            _ => None,
        };
        resolved(&self.prompter, DONE_PROMPT_ID, end.answer_str(), by);
    }
}

impl Drop for SessionWatch {
    fn drop(&mut self) {
        let mut g = self.prompter.prompts.lock().unwrap();
        if g.as_ref().is_some_and(|s| s.study_id == self.prompter.study_id && s.pending.prompt_id == DONE_PROMPT_ID) {
            *g = None;
        }
    }
}

/// `session.json`: what the person answered and how the session ended. Written when the run
/// starts, so the answers outlive a run that fails, and again when it ends.
#[derive(Debug, Clone, Serialize)]
pub struct SessionRecord {
    pub start_text: String,
    pub done_text: String,
    /// The Start prompt's answers, in the order the session declares its fields; a declared field
    /// with no answer is absent.
    pub run_fields: Vec<RunFieldValue>,
    pub max_duration_ms: u32,
    pub export_dir: String,
    pub start_answered_by: AnsweredBy,
    pub start_answered_utc_ms: u64,
    /// When the bench was told to start: the cap runs from here.
    pub sampling_started_utc_ms: Option<u64>,
    pub ended_utc_ms: Option<u64>,
    /// `done`, `cap`, `cancelled`, `study_done` or `failed`.
    pub ended_by: Option<&'static str>,
    pub done_answered_by: Option<AnsweredBy>,
    /// What closed the run after Core asked the bench to stop (decision 128): `study_done` (the
    /// bench answered), `timeout` (it did not within the bound) or `send_failed`. Absent when the
    /// bench's own `StudyDone` came first and no stop was sent.
    pub stop_closed_by: Option<&'static str>,
}

impl SessionRecord {
    pub fn new(spec: &SessionSpec, run_values: &[RunFieldValue], answered_by: AnsweredBy, answered_utc_ms: u64) -> Self {
        let run_fields = spec
            .run_fields
            .iter()
            .filter_map(|f| run_values.iter().find(|v| v.name == f.name).cloned())
            .collect();
        SessionRecord {
            start_text: spec.start_text.as_str().to_string(),
            done_text: spec.done_text.as_str().to_string(),
            run_fields,
            max_duration_ms: spec.max_duration_ms,
            export_dir: spec.export_dir.as_str().to_string(),
            start_answered_by: answered_by,
            start_answered_utc_ms: answered_utc_ms,
            sampling_started_utc_ms: None,
            ended_utc_ms: None,
            ended_by: None,
            done_answered_by: None,
            stop_closed_by: None,
        }
    }

    /// Notes how the session ended.
    pub fn end(&mut self, end: SessionEnd, ended_utc_ms: u64) {
        self.ended_utc_ms = Some(ended_utc_ms);
        self.ended_by = Some(end.ended_by());
        if let SessionEnd::Done(by) = end {
            self.done_answered_by = Some(by);
        }
    }

    /// Writes `session.json` into `dir` through a temporary file, so a reader never sees half of it.
    pub fn write(&self, dir: &FsPath) -> std::io::Result<()> {
        let partial = dir.join(format!("{SESSION_FILE}.partial"));
        std::fs::write(&partial, serde_json::to_vec_pretty(self).map_err(std::io::Error::other)?)?;
        std::fs::rename(&partial, dir.join(SESSION_FILE))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use embarch_study_designer::RunField;
    use std::sync::{Arc, Mutex as StdMutex};

    fn text<const N: usize>(t: &str) -> heapless::String<N> {
        heapless::String::try_from(t).unwrap()
    }

    fn spec(max_duration_ms: u32) -> SessionSpec {
        let mut run_fields = heapless::Vec::new();
        run_fields.push(RunField { name: text("sensor"), label: text("Sensor serial"), required: true }).unwrap();
        run_fields.push(RunField { name: text("note"), label: text("Note"), required: false }).unwrap();
        SessionSpec {
            start_text: text("Place the sensor"),
            run_fields,
            done_text: text("Recording finished"),
            max_duration_ms,
            export_dir: text("exports/mic"),
        }
    }

    fn rig() -> (SessionPrompter, HostPrompt, broadcast::Receiver<StudyEvent>) {
        let prompts: HostPrompt = Arc::new(StdMutex::new(None));
        let (tx, rx) = broadcast::channel(16);
        (SessionPrompter::new(prompts.clone(), tx, "s1".into()), prompts, rx)
    }

    fn answer(prompt_id: u32, answer: PromptAnswer, fields: &[(&str, &str)]) -> PromptAnswerBody {
        PromptAnswerBody {
            prompt_id,
            answer,
            answered_by: AnsweredBy::Ui,
            value: None,
            run_fields: fields.iter().map(|(n, v)| RunFieldAnswer { name: n.to_string(), value: v.to_string() }).collect(),
        }
    }

    fn wait_pending(prompts: &HostPrompt) -> PendingPrompt {
        for _ in 0..500 {
            if let Some(p) = crate::host_study::pending_prompt(prompts, "s1") {
                return p;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        panic!("no prompt was raised");
    }

    #[test]
    fn the_study_waits_on_start_and_only_a_confirmed_answer_with_good_fields_releases_it() {
        let (prompter, prompts, mut events) = rig();
        let s = spec(1000);
        let waiting = std::thread::spawn(move || prompter.raise_start(&s, Duration::from_secs(30)));
        let pending = wait_pending(&prompts);
        // The prompt says what it is and what it asks, and nothing has been released yet.
        let info = pending.session.clone().unwrap();
        assert_eq!(info.phase, SessionPhase::Start);
        assert_eq!(info.run_fields.iter().map(|f| (f.name.as_str(), f.required)).collect::<Vec<_>>(), [("sensor", true), ("note", false)]);
        assert_eq!(pending.text, "Place the sensor");
        std::thread::sleep(Duration::from_millis(80));
        assert!(!waiting.is_finished(), "Start must hold the study until a person answers");
        let ev = serde_json::to_value(events.try_recv().unwrap()).unwrap();
        assert_eq!((ev["kind"].as_str(), ev["session"]["phase"].as_str()), (Some("PromptPending"), Some("start")));

        let body = answer(pending.prompt_id, PromptAnswer::Confirmed, &[("sensor", "SN-7"), ("note", "quiet room")]);
        assert_eq!(crate::host_study::record_answer_scoped(&prompts, "s1", &body, Scope::Person), Ok(true));
        match waiting.join().unwrap() {
            StartOutcome::Confirmed { run_values, answered_by, .. } => {
                assert_eq!(answered_by, AnsweredBy::Ui);
                assert_eq!(run_values.iter().map(|v| (v.name.as_str(), v.value.as_str())).collect::<Vec<_>>(), [("sensor", "SN-7"), ("note", "quiet room")]);
            }
            other => panic!("{other:?}"),
        }
        assert!(prompts.lock().unwrap().is_none(), "the slot is cleared once the study goes on");
        let ev = serde_json::to_value(events.try_recv().unwrap()).unwrap();
        assert_eq!((ev["kind"].as_str(), ev["answer"].as_str()), (Some("PromptResolved"), Some("confirmed")));
    }

    #[test]
    fn answers_that_fail_validation_are_refused_with_a_reason_and_the_prompt_stays() {
        let (prompter, prompts, _events) = rig();
        let s = spec(1000);
        let waiting = std::thread::spawn(move || prompter.raise_start(&s, Duration::from_secs(30)));
        let id = wait_pending(&prompts).prompt_id;
        let refused = |fields: &[(&str, &str)]| {
            crate::host_study::record_answer_scoped(&prompts, "s1", &answer(id, PromptAnswer::Confirmed, fields), Scope::Person).unwrap_err()
        };
        // A required field missing, blank, unknown, repeated or too long: each its own reason.
        let (status, why) = refused(&[("note", "x")]);
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(why.contains("run_fields[0] is required"), "{why}");
        assert!(refused(&[("sensor", "   ")]).1.contains("required"));
        assert!(refused(&[("sensor", "a"), ("colour", "b")]).1.contains("does not declare"));
        assert!(refused(&[("sensor", "a"), ("sensor", "b")]).1.contains("already did"));
        let long = "x".repeat(embarch_study_designer::limits::MAX_RUN_VALUE_LEN + 1);
        assert!(refused(&[("sensor", long.as_str())]).1.contains("is longer than"));
        // None of them was recorded: the prompt is still waiting and nothing is released.
        assert!(crate::host_study::pending_prompt(&prompts, "s1").is_some());
        assert!(!waiting.is_finished());
        let ok = answer(id, PromptAnswer::Confirmed, &[("sensor", "SN-7")]);
        assert_eq!(crate::host_study::record_answer_scoped(&prompts, "s1", &ok, Scope::Person), Ok(true));
        assert!(matches!(waiting.join().unwrap(), StartOutcome::Confirmed { .. }));
    }

    #[test]
    fn the_agents_token_cannot_answer_start_but_can_answer_done() {
        let (prompter, prompts, _events) = rig();
        let s = spec(60_000);
        let p2 = prompter.clone();
        let s2 = s.clone();
        let waiting = std::thread::spawn(move || p2.raise_start(&s2, Duration::from_secs(30)));
        let id = wait_pending(&prompts).prompt_id;
        let body = answer(id, PromptAnswer::Confirmed, &[("sensor", "SN-7")]);
        let (status, why) = crate::host_study::record_answer_scoped(&prompts, "s1", &body, Scope::Agent).unwrap_err();
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert!(why.contains("person's token"), "{why}");
        assert!(!waiting.is_finished());
        assert_eq!(crate::host_study::record_answer_scoped(&prompts, "s1", &body, Scope::Person), Ok(true));
        waiting.join().unwrap();

        let watch = prompter.begin_sampling(&s, Instant::now());
        let done = wait_pending(&prompts);
        assert_eq!(done.session.unwrap().phase, SessionPhase::Done);
        let (status, _) = crate::host_study::record_answer_scoped(&prompts, "s1", &answer(done.prompt_id, PromptAnswer::Cancelled, &[]), Scope::Person).unwrap_err();
        assert_eq!(status, StatusCode::BAD_REQUEST, "Done can only be confirmed");
        let mut with_fields = answer(done.prompt_id, PromptAnswer::Confirmed, &[("sensor", "x")]);
        assert_eq!(crate::host_study::record_answer_scoped(&prompts, "s1", &with_fields, Scope::Agent).unwrap_err().0, StatusCode::BAD_REQUEST);
        with_fields.run_fields.clear();
        assert_eq!(crate::host_study::record_answer_scoped(&prompts, "s1", &with_fields, Scope::Agent), Ok(true));
        drop(watch);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_prompt_route_carries_a_session_through_and_shows_it_on_the_study_status() {
        use crate::api::AppState;
        use axum::extract::{Path, State};
        let state = AppState::new("t".into());
        let prompter = SessionPrompter::new(state.host_prompt.clone(), state.study_events.clone(), "s1".into());
        let s = spec(1000);
        let waiting = std::thread::spawn(move || prompter.raise_start(&s, Duration::from_secs(30)));
        let pending = loop {
            if let Some(p) = crate::host_study::pending_prompt(&state.host_prompt, "s1") {
                break p;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        };
        let wire = serde_json::to_value(&pending).unwrap();
        assert_eq!(wire["session"]["run_fields"][0]["label"], "Sensor serial");
        let body = |fields: serde_json::Value| -> axum::Json<PromptAnswerBody> {
            axum::Json(serde_json::from_value(serde_json::json!({"prompt_id": 1, "answer": "confirmed", "answered_by": "mcp", "run_fields": fields})).unwrap())
        };
        let agent = crate::host_study::answer_prompt_handler(State(state.clone()), Scope::Agent, Path("s1".into()), body(serde_json::json!([{"name": "sensor", "value": "SN-7"}]))).await.unwrap_err();
        assert_eq!(agent.0, StatusCode::FORBIDDEN);
        let bad = crate::host_study::answer_prompt_handler(State(state.clone()), Scope::Person, Path("s1".into()), body(serde_json::json!([]))).await.unwrap_err();
        assert_eq!(bad.0, StatusCode::BAD_REQUEST);
        let ok = crate::host_study::answer_prompt_handler(State(state.clone()), Scope::Person, Path("s1".into()), body(serde_json::json!([{"name": "sensor", "value": "SN-7"}]))).await.unwrap();
        assert_eq!(serde_json::to_value(&ok.0).unwrap()["status"], "recorded");
        let out = tokio::task::spawn_blocking(move || waiting.join().unwrap()).await.unwrap();
        assert!(matches!(out, StartOutcome::Confirmed { answered_by: AnsweredBy::Mcp, .. }));
        // A body with no `run_fields` at all still parses: every existing caller's body is unchanged.
        let old: PromptAnswerBody = serde_json::from_str(r#"{"prompt_id":1,"answer":"confirmed","answered_by":"ui"}"#).unwrap();
        assert!(old.run_fields.is_empty());
    }

    #[test]
    fn a_cancelled_start_or_a_forgotten_one_never_releases_the_study() {
        let (prompter, prompts, _events) = rig();
        let s = spec(1000);
        let p2 = prompter.clone();
        let s2 = s.clone();
        let waiting = std::thread::spawn(move || p2.raise_start(&s2, Duration::from_secs(30)));
        let id = wait_pending(&prompts).prompt_id;
        let cancel = answer(id, PromptAnswer::Cancelled, &[]);
        assert_eq!(crate::host_study::record_answer_scoped(&prompts, "s1", &cancel, Scope::Person), Ok(true));
        assert_eq!(waiting.join().unwrap(), StartOutcome::Cancelled(AnsweredBy::Ui));
        assert_eq!(prompter.raise_start(&s, Duration::from_millis(60)), StartOutcome::TimedOut);
        assert!(prompts.lock().unwrap().is_none());
    }

    #[test]
    fn a_short_cap_stops_sampling_and_the_person_sees_done_end() {
        let (prompter, prompts, mut events) = rig();
        let s = spec(80);
        let t0 = Instant::now();
        let watch = prompter.begin_sampling(&s, t0);
        let pending = wait_pending(&prompts);
        assert_eq!(pending.text, "Recording finished");
        assert_eq!(pending.deadline_utc_ms - pending.opened_utc_ms, 80, "Done's deadline is the cap");
        assert_eq!(watch.poll(t0), None);
        // The loop's receive deadline never reaches past the cap or one poll.
        let far = t0 + Duration::from_secs(3600);
        assert_eq!(watch.recv_deadline(far, t0), t0 + CANCEL_POLL);
        assert_eq!(watch.recv_deadline(far, t0 + Duration::from_millis(70)), t0 + Duration::from_millis(80));
        assert_eq!(watch.recv_deadline(t0 + Duration::from_millis(5), t0), t0 + Duration::from_millis(5));
        let end = loop {
            if let Some(end) = watch.poll(Instant::now()) {
                break end;
            }
            std::thread::sleep(Duration::from_millis(5));
        };
        assert_eq!(end, SessionEnd::Cap);
        assert!(t0.elapsed() >= Duration::from_millis(80));
        watch.resolve(end);
        assert!(prompts.lock().unwrap().is_none());
        let _pending = events.try_recv().unwrap();
        let ev = serde_json::to_value(events.try_recv().unwrap()).unwrap();
        assert_eq!((ev["kind"].as_str(), ev["answer"].as_str(), ev["prompt_id"].as_u64()), (Some("PromptResolved"), Some("timed_out"), Some(2)));
    }

    #[test]
    fn done_answered_ends_sampling_before_the_cap() {
        let (prompter, prompts, mut events) = rig();
        let s = spec(300_000);
        let t0 = Instant::now();
        let watch = prompter.begin_sampling(&s, t0);
        let id = wait_pending(&prompts).prompt_id;
        assert_eq!(watch.poll(t0), None);
        let body = PromptAnswerBody { answered_by: AnsweredBy::Mcp, ..answer(id, PromptAnswer::Confirmed, &[]) };
        assert_eq!(crate::host_study::record_answer_scoped(&prompts, "s1", &body, Scope::Agent), Ok(true));
        let end = watch.poll(t0 + Duration::from_millis(10)).unwrap();
        assert_eq!(end, SessionEnd::Done(AnsweredBy::Mcp));
        assert_eq!(end.ended_by(), "done");
        watch.resolve(end);
        let _pending = events.try_recv().unwrap();
        let ev = serde_json::to_value(events.try_recv().unwrap()).unwrap();
        assert_eq!((ev["answer"].as_str(), ev["answered_by"].as_str()), (Some("confirmed"), Some("mcp")));
        assert!(prompts.lock().unwrap().is_none());
    }

    #[test]
    fn the_bench_finishing_first_resolves_the_prompt_and_dropping_the_watch_clears_it() {
        let (prompter, prompts, mut events) = rig();
        let s = spec(300_000);
        let watch = prompter.begin_sampling(&s, Instant::now());
        watch.resolve(SessionEnd::StudyDone);
        let _pending = events.try_recv().unwrap();
        let ev = serde_json::to_value(events.try_recv().unwrap()).unwrap();
        assert_eq!(ev["answer"].as_str(), Some("study_ended"));
        assert!(prompts.lock().unwrap().is_none());
        // A panic mid-run drops the watch without resolving; the slot is still cleared.
        let watch = prompter.begin_sampling(&s, Instant::now());
        assert!(prompts.lock().unwrap().is_some());
        drop(watch);
        assert!(prompts.lock().unwrap().is_none());
    }

    #[test]
    fn the_run_fields_are_saved_beside_the_results_and_the_end_is_added() {
        let dir = tempfile::tempdir().unwrap();
        let s = spec(5000);
        let values = vec![
            RunFieldValue { name: text("note"), value: text("quiet room") },
            RunFieldValue { name: text("sensor"), value: text("SN-7") },
        ];
        let mut rec = SessionRecord::new(&s, &values, AnsweredBy::Ui, 111);
        rec.write(dir.path()).unwrap();
        let saved: serde_json::Value = serde_json::from_slice(&std::fs::read(dir.path().join(SESSION_FILE)).unwrap()).unwrap();
        // Declared order, not answer order.
        assert_eq!(saved["run_fields"][0]["name"], "sensor");
        assert_eq!(saved["run_fields"][0]["value"], "SN-7");
        assert_eq!(saved["run_fields"][1]["value"], "quiet room");
        assert_eq!((saved["max_duration_ms"].as_u64(), saved["export_dir"].as_str()), (Some(5000), Some("exports/mic")));
        assert!(saved["ended_by"].is_null());
        rec.end(SessionEnd::Cap, 999);
        rec.write(dir.path()).unwrap();
        let saved: serde_json::Value = serde_json::from_slice(&std::fs::read(dir.path().join(SESSION_FILE)).unwrap()).unwrap();
        assert_eq!((saved["ended_by"].as_str(), saved["ended_utc_ms"].as_u64()), (Some("cap"), Some(999)));
        assert!(!dir.path().join("session.json.partial").exists());
    }
}
