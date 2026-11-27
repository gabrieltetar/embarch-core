//! The executor of a started Test run (decisions 136, 143): bench by bench in `run.json`'s order, test by
//! test, each test's precondition first, each run of it one host study through the existing
//! executor with its teardown, each verdict from the shared crate's `judge_run` and `judge_test`.
//!
//! **The policy is separate from the bench.** [`drive`] walks `run.json` against an [`Env`], so
//! every path of the failure policy (a fail goes on, an error ends its bench, a cancel ends the
//! run, `stop_on_fail`, `requires`) is tested over a fake. [`CoreEnv`] is the real one: Core's own
//! reset path, a bench board's idle line through the exchange route's body, and
//! `host_study::run_study_blocking` for the study.
//!
//! What a step addressed is derived here from the topology, never from the study: a signal whose
//! origin is the `dut` role is [`Target::Dut`]; another role, a bench board or an instrument is
//! [`Target::Bench`] ([`target_of`]).

use embarch_study_designer::hil_expand::{RunBench, RunBoard, RunJson, RunTest};
use embarch_study_designer::hil_judge::{self, DeclaredBy, FirmwareState, MeasuredValue, MeasurementRecord, Origin, StepRecord as JudgeStep};
use embarch_study_designer::hil_test::Precondition;
use embarch_study_designer::host_study::{HostList, HostAction, HostStep, HostStudy, PromptKind};
use embarch_study_designer::verdicts::{Counts, Evidence, Judged, Reason, Rules, StepOutcome, Target, Verdict};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::api::AppState;
use crate::host_study::{detail, RunContext};
use crate::measurements::SignalOrigin;
use crate::smoke_check::{BenchAccess, BenchNeeds, SmokeFindings};
use crate::token_store::Scope;

/// The time a test is allowed beyond its worst-case study time, for its precondition and the
/// seal. **Chosen, not measured.**
const TEST_ALLOWANCE_MS: u64 = 120_000;
/// The multiple of `duration_bound_ms` a run may take before the executor ends it: a prompt's
/// deadline is already in the bound, so only a stuck backend reaches this. **Chosen, not measured.**
const DEADLINE_FACTOR: u64 = 2;
/// How long a bench board's idle line waits for a reply. `run.json` carries no `until` for it, so
/// this is the quiet wait. **Chosen, not measured.**
const IDLE_WAIT_MS: u64 = 500;
/// How long a precondition waits for a port that a reset has taken away to come back.
const PORT_WAIT_MS: u64 = 5_000;
/// How long the setup prompt between two benches waits for the person (decision 139), and what each
/// bench after the first adds to the run's deadline. **Chosen, not measured**: a person moving a
/// cable and enrolling a board.
const SETUP_WAIT_MS: u64 = 900_000;
/// How long a bench board's hello line and an instrument's `*IDN?` wait for a reply in the smoke
/// check (decision 143). **Chosen, not measured.**
const SMOKE_WAIT_MS: u64 = 2_000;
/// How long the smoke check waits for an instrument's port to enumerate. **Chosen, not measured.**
const SMOKE_PORT_WAIT_MS: u64 = 5_000;

/// What one run of a test's study left, ready for [`hil_judge::judge_run`].
pub struct StudyRun {
    pub steps: Vec<JudgeStep>,
    pub measurements: Vec<MeasurementRecord>,
    /// The study, or the run under it, was cancelled.
    pub cancelled: bool,
    /// Core could not run the study at all (the study was not kept, a signal is not declared,
    /// the lock was taken): the setup's failure, an `error`.
    pub aborted: Option<String>,
    /// The study's id in Core's run log and results folder; `None` when none was made.
    pub study_id: Option<String>,
}

/// How the setup prompt before a bench ended (decision 139).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SetupEnd {
    /// Someone confirmed the bench is set up.
    Confirmed,
    /// Someone declined it.
    Declined,
    /// Nobody answered by the deadline.
    TimedOut,
    /// The run was cancelled while it waited.
    Cancelled,
}

/// What the policy needs from a bench.
pub trait Env {
    /// The setup window before `bench`, the next saved bench (decision 139): raises a `setup`
    /// prompt naming it and what is `missing`, admits the setup calls while it waits, and says how
    /// it ended. The default has no window.
    fn setup(&mut self, _bench: &RunBench, _config: &str, _missing: &[String]) -> SetupEnd {
        SetupEnd::Confirmed
    }
    /// The smoke check before `bench`'s first test (decision 143), over the `tests` it will run.
    /// The default checks nothing.
    fn smoke_check(&mut self, bench: &RunBench, _tests: &[&RunTest]) -> SmokeFindings {
        SmokeFindings::new(&bench.name)
    }
    /// The run was cancelled (the route, or a study's own cancel).
    fn cancelled(&self) -> bool;
    /// Brings the DUT and the bench boards to the test's starting state; `Err` names what failed.
    fn precondition(&mut self, bench: &RunBench, test: &RunTest) -> Result<(), String>;
    /// Runs the test's study once, judged steps and measurements back.
    fn run_study(&mut self, test: &RunTest) -> StudyRun;
    /// Called each time a test ends, with every result so far and every smoke check made: the real
    /// bench keeps them so a Core restart keeps the finished verdicts (decision 153). The default
    /// keeps nothing.
    fn checkpoint(&mut self, _results: &[TestResult], _smoke: &[SmokeFindings]) {}
}

/// One test's result in the run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TestResult {
    pub test_id: String,
    pub bench: String,
    pub judged: Judged,
    /// The counts of the test's runs; `None` for a test that did not run.
    pub counts: Option<Counts>,
    /// Each run's verdict, in order.
    pub runs: Vec<Judged>,
    pub study_ids: Vec<String>,
    /// What decided an `error` or a skip, in words.
    pub note: Option<String>,
}

impl TestResult {
    pub fn to_json(&self) -> Value {
        json!({
            "test_id": self.test_id, "bench": self.bench,
            "verdict": self.judged.verdict, "reason": self.judged.reason, "qualifiers": self.judged.qualifiers,
            "counts": self.counts, "runs": self.runs.iter().map(|r| json!({"verdict": r.verdict, "reason": r.reason})).collect::<Vec<_>>(),
            "study_ids": self.study_ids, "note": self.note,
        })
    }
}

/// What a watcher of the run reads, kept under the run's id in `AppState`.
#[derive(Debug, Clone, Default)]
pub struct Progress {
    /// `running`, then `completed`, `cancelled` or `deadline` once it ends.
    pub state: &'static str,
    pub bench: Option<String>,
    pub test: Option<String>,
    pub results: Vec<TestResult>,
    /// What each bench's smoke check found, in bench order, kept for the run record.
    pub smoke: Vec<SmokeFindings>,
}

/// Every run's progress, by run id; each is shared with the executor that updates it.
pub type ProgressBook = Arc<Mutex<BTreeMap<String, Arc<Mutex<Progress>>>>>;

/// How the run ended, with every test's result in `run.json`'s order.
#[derive(Debug, Clone, PartialEq)]
pub struct RunOutcome {
    /// `completed`, `cancelled` or `deadline`.
    pub status: &'static str,
    pub results: Vec<TestResult>,
    /// What each bench's smoke check found, in bench order.
    pub smoke: Vec<SmokeFindings>,
}

fn skipped(test: &RunTest, why: Reason, note: &str) -> TestResult {
    TestResult {
        test_id: test.test_id.clone(),
        bench: test.bench.name.clone(),
        judged: hil_judge::skip(why).expect("a skip reason"),
        counts: None,
        runs: vec![],
        study_ids: vec![],
        note: Some(note.into()),
    }
}

fn cancelled(test: &RunTest, note: &str) -> TestResult {
    TestResult {
        test_id: test.test_id.clone(),
        bench: test.bench.name.clone(),
        judged: Judged { verdict: Verdict::Cancelled, reason: None, qualifiers: vec![] },
        counts: None,
        runs: vec![],
        study_ids: vec![],
        note: Some(note.into()),
    }
}

fn errored(test: &RunTest, why: Reason, note: String, study_ids: Vec<String>) -> TestResult {
    let judged = Judged { verdict: Verdict::Error, reason: Some(why), qualifiers: vec![] };
    TestResult { test_id: test.test_id.clone(), bench: test.bench.name.clone(), judged: judged.clone(), counts: None, runs: vec![judged], study_ids, note: Some(note) }
}

/// The run's worst-case duration, with the allowance each test gets, which the executor holds the
/// run to ([`DEADLINE_FACTOR`]).
pub fn deadline(run: &RunJson) -> Duration {
    let bound = embarch_study_designer::hil_expand::duration_bound(run);
    let setups = (run.benches.len() as u64).saturating_sub(1);
    Duration::from_millis(
        bound.saturating_mul(DEADLINE_FACTOR).saturating_add(TEST_ALLOWANCE_MS.saturating_mul(run.tests.len() as u64)).saturating_add(SETUP_WAIT_MS.saturating_mul(setups)),
    )
}

/// Walks `run` against `env` under the failure policy (`notes/hil/03-test.md`, "A run", item 5):
/// a **fail** ends only its test; an **error** ends its bench (`stopped_by_error`) and the run
/// goes on to the next; a **cancel** ends the run (the rest `cancelled`); `stop_on_fail` ends it
/// at the first fail (`stopped_after_fail`); a test whose `requires` did not pass is
/// `requires_not_met`. `progress` is updated as it goes.
pub fn drive(run: &RunJson, env: &mut dyn Env, progress: &Mutex<Progress>, rules: &Rules, limit: Duration) -> RunOutcome {
    let started = Instant::now();
    let mut results: Vec<TestResult> = Vec::new();
    let mut ended: Option<&'static str> = None;
    let mut after_fail = false;
    let mut smoke: Vec<SmokeFindings> = Vec::new();
    // The DUT unit the first bench read: a later bench reading another is refused (decision 143).
    let mut unit: Option<(String, String)> = None;
    let publish = |results: &[TestResult], smoke: &[SmokeFindings], bench: Option<&str>, test: Option<&str>| {
        let mut p = progress.lock().unwrap();
        p.state = "running";
        p.bench = bench.map(str::to_string);
        p.test = test.map(str::to_string);
        p.results = results.to_vec();
        p.smoke = smoke.to_vec();
    };
    for (n, bench) in run.benches.iter().enumerate() {
        let mut bench_error = false;
        // The setup window before every bench after the first. It names nothing as missing: the
        // smoke check runs after it, on whatever the person set up (decisions 139, 143).
        let mut setup_failed: Option<&str> = None;
        if n > 0 && ended.is_none() && !env.cancelled() {
            publish(&results, &smoke, Some(&bench.name), None);
            let config = bench.tests.iter().find_map(|id| run.tests.iter().find(|t| &t.test_id == id)).map_or("", |t| t.bench.config.as_str());
            match env.setup(bench, config, &[]) {
                SetupEnd::Confirmed => {}
                SetupEnd::Declined => setup_failed = Some("the setup prompt for this bench was declined"),
                SetupEnd::TimedOut => setup_failed = Some("the setup prompt for this bench timed out"),
                SetupEnd::Cancelled => ended = Some("cancelled"),
            }
        }
        // The smoke check before the bench's first test, the first bench included.
        let mut smoke_failed: Option<String> = None;
        if ended.is_none() && setup_failed.is_none() && !env.cancelled() {
            publish(&results, &smoke, Some(&bench.name), None);
            let tests: Vec<&RunTest> = bench.tests.iter().filter_map(|id| run.tests.iter().find(|t| &t.test_id == id)).collect();
            let mut found = env.smoke_check(bench, &tests);
            if let Some(id) = found.unit.clone() {
                match &unit {
                    None => unit = Some((bench.name.clone(), id)),
                    Some((first, kept)) if *kept != id => found.failures.push(format!(
                        "the DUT unit: bench '{first}' read hardware ID {kept}, this bench reads {id}; one unit goes through every bench"
                    )),
                    Some(_) => {}
                }
            }
            if !found.failures.is_empty() {
                smoke_failed = Some(found.failure_text());
            }
            smoke.push(found);
        }
        for id in &bench.tests {
            let Some(test) = run.tests.iter().find(|t| &t.test_id == id) else { continue };
            if ended.is_none() && env.cancelled() {
                ended = Some("cancelled");
            }
            if ended.is_none() && started.elapsed() > limit {
                ended = Some("deadline");
            }
            let result = if let Some(e) = ended {
                cancelled(test, if e == "deadline" { "the run passed its deadline" } else { "the run was cancelled" })
            } else if let Some(why) = setup_failed.take() {
                bench_error = true;
                errored(test, Reason::BenchStepFailed, why.to_string(), vec![])
            } else if let Some(why) = &smoke_failed {
                errored(test, Reason::SmokeCheck, why.clone(), vec![])
            } else if after_fail {
                skipped(test, Reason::StoppedAfterFail, "stop_on_fail: an earlier test failed")
            } else if bench_error {
                skipped(test, Reason::StoppedByError, "an earlier test on this bench ended in an error")
            } else if let Some(missing) = test.requires.iter().find(|r| !results.iter().any(|x| &x.test_id == *r && x.judged.verdict == Verdict::Pass)) {
                skipped(test, Reason::RequiresNotMet, &format!("it requires {missing}, which did not pass"))
            } else {
                publish(&results, &smoke, Some(&bench.name), Some(&test.test_id));
                run_test(test, bench, env, rules, started, limit, &mut ended)
            };
            match result.judged.verdict {
                Verdict::Error => bench_error = true,
                Verdict::Fail if run.options.stop_on_fail => after_fail = true,
                _ => {}
            }
            results.push(result);
            env.checkpoint(&results, &smoke);
            publish(&results, &smoke, Some(&bench.name), None);
        }
    }
    let status = ended.unwrap_or("completed");
    {
        let mut p = progress.lock().unwrap();
        p.state = status;
        p.bench = None;
        p.test = None;
        p.results = results.clone();
        p.smoke = smoke.clone();
    }
    RunOutcome { status, results, smoke }
}

/// [`drive`] with the host held awake for the whole run: held at the go, released on every way out
/// (decision 152).
pub fn drive_awake(keeper: &mut dyn crate::keep_awake::KeepAwake, run: &RunJson, env: &mut dyn Env, progress: &Mutex<Progress>, rules: &Rules, limit: Duration) -> RunOutcome {
    let _hold = crate::keep_awake::Hold::new(keeper);
    drive(run, env, progress, rules, limit)
}

/// One test: its precondition once, then `run_count` runs, each judged; the test is the worst run.
#[allow(clippy::too_many_arguments)]
fn run_test(test: &RunTest, bench: &RunBench, env: &mut dyn Env, rules: &Rules, started: Instant, limit: Duration, ended: &mut Option<&'static str>) -> TestResult {
    if let Err(why) = env.precondition(bench, test) {
        return errored(test, Reason::BenchStepFailed, format!("the precondition {:?} failed: {why}", test.precondition), vec![]);
    }
    let mut runs: Vec<Judged> = Vec::new();
    let mut study_ids: Vec<String> = Vec::new();
    for _ in 0..test.run_count.max(1) {
        if env.cancelled() {
            *ended = Some("cancelled");
            runs.push(Judged { verdict: Verdict::Cancelled, reason: None, qualifiers: vec![] });
            break;
        }
        if started.elapsed() > limit {
            *ended = Some("deadline");
            runs.push(Judged { verdict: Verdict::Cancelled, reason: None, qualifiers: vec![] });
            break;
        }
        let ran = env.run_study(test);
        if let Some(id) = &ran.study_id {
            study_ids.push(id.clone());
        }
        if let Some(why) = ran.aborted {
            return errored(test, Reason::BenchStepFailed, format!("Core could not run the study: {why}"), study_ids);
        }
        if ran.cancelled {
            *ended = Some("cancelled");
            runs.push(Judged { verdict: Verdict::Cancelled, reason: None, qualifiers: vec![] });
            break;
        }
        let judged = match hil_judge::judge_run(test, &ran.steps, &ran.measurements, rules) {
            Ok(j) => j,
            Err(e) => return errored(test, Reason::BenchStepFailed, format!("the run could not be judged: {e:?}"), study_ids),
        };
        let stop = judged.verdict == Verdict::Error;
        runs.push(judged);
        if stop {
            break;
        }
    }
    match hil_judge::judge_test(&runs, rules) {
        Ok(v) => TestResult {
            test_id: test.test_id.clone(),
            bench: bench.name.clone(),
            judged: v.judged,
            counts: Some(v.counts),
            runs,
            study_ids,
            note: None,
        },
        Err(e) => errored(test, Reason::BenchStepFailed, format!("the test could not be judged: {e:?}"), study_ids),
    }
}

// ---- what a step addressed, and what a study left ----------------------------------------

/// What a signal addressed: the `dut` role is the DUT, every other role and every bench board is
/// the bench. A signal topology no longer declares is the DUT's, as `measurements.json` reads it.
pub fn target_of(origin: Option<SignalOrigin>) -> Target {
    match origin {
        Some(SignalOrigin::Role(role)) if role != embarch_topology::hardware::DUT_ROLE => Target::Bench,
        Some(SignalOrigin::Board(_)) => Target::Bench,
        _ => Target::Dut,
    }
}

/// A [`Target`] as `events.json`'s step rows spell it: `dut` or `bench`.
pub fn target_name(t: Target) -> &'static str {
    match t {
        Target::Dut => "dut",
        Target::Bench => "bench",
    }
}

fn walk_steps<'a>(steps: &'a [HostStep], out: &mut BTreeMap<&'a str, &'a HostAction>) {
    for s in steps {
        out.insert(s.name.as_str(), &s.action);
        if let HostAction::Repeat { steps, .. } = &s.action {
            walk_steps(steps, out);
        }
    }
}

/// A host step's `detail` as the verdict rules read it. `None` for a step that says nothing about
/// the DUT or the bench: one cut short by a cancel.
fn outcome_of(detail: &str, observation: bool) -> Option<StepOutcome> {
    Some(match detail {
        detail::CANCELLED => return None,
        detail::REFUSED => StepOutcome::RefusedOn,
        detail::EMPTY_REPLY => StepOutcome::EmptyReply,
        detail::UNTIL_NOT_SEEN | detail::UNACKNOWLEDGED => StepOutcome::UntilNotSeen,
        detail::PORT_LOST | detail::PORT_UNAVAILABLE | detail::EXCHANGE_FAILED => StepOutcome::PortLost,
        detail::PROMPT_CANCELLED => StepOutcome::PromptCancelled { observation },
        detail::PROMPT_TIMED_OUT => StepOutcome::PromptTimedOut { observation },
        _ => StepOutcome::Pass,
    })
}

/// The steps of an `events.json`, as the verdict rules read them. Whether a prompt asked a person
/// what they saw is `run.json`'s `prompt_kinds` (`kinds`), never the study's own `kind` field: a
/// study cannot relabel its prompt (decision 138).
pub fn judge_steps(study: &HostStudy, kinds: &BTreeMap<String, PromptKind>, events: &Value, origin: &dyn Fn(&str) -> Option<SignalOrigin>) -> Vec<JudgeStep> {
    let mut actions = BTreeMap::new();
    walk_steps(&study.steps, &mut actions);
    walk_steps(&study.teardown, &mut actions);
    events["steps"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|s| {
            let name = s["step_name"].as_str()?;
            let detail = s["detail"].as_str().unwrap_or("");
            let action = actions.get(name).copied();
            let observation = kinds.get(name) == Some(&PromptKind::Observation);
            let outcome = outcome_of(detail, observation)?;
            let target = match action {
                // The row's own `target` is what the study recorded from its one lookup; the
                // live `origin` serves only a row written without it (decision 155).
                Some(HostAction::Exchange { signal, .. }) => match s["target"].as_str() {
                    Some("bench") => Target::Bench,
                    Some("dut") => Target::Dut,
                    _ => target_of(origin(signal)),
                },
                Some(HostAction::Instrument { .. }) => Target::Bench,
                _ => Target::Dut,
            };
            Some(JudgeStep { step: name.to_string(), target, outcome })
        })
        .collect()
}

/// The UTC calendar date `yyyy-mm-dd` of a millisecond timestamp, to compare with a calibration's
/// `due` (the civil-from-days algorithm; no date crate is a dependency).
fn utc_date(ms: u64) -> String {
    let z = (ms / 86_400_000) as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}")
}

/// The measurements of a `measurements.json`, as the verdict rules read them. `u` comes from the
/// test's source for the measurement; a measurement with no source has none declared, and says so.
pub fn judge_measurements(test: &RunTest, doc: &Value) -> Vec<MeasurementRecord> {
    doc["measurements"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|m| {
            let name = m["name"].as_str()?;
            let source = test.sources.iter().find(|s| s.measurement == name);
            let origin = match (m["by"].as_str(), m["instrument"]["kind"].as_str()) {
                (Some("person"), _) => Origin::PersonObservation,
                (Some("dut"), _) => Origin::DutReport,
                (_, Some("instrument")) => Origin::Instrument,
                // `bench_image` is `unverified` for every board until Core can read an image back.
                _ => Origin::BenchBoard { firmware: FirmwareState::Unverified },
            };
            let incomplete: Vec<Reason> = m["incomplete"].as_array().into_iter().flatten().filter_map(|i| serde_json::from_value(i.clone()).ok()).collect();
            // An instrument's own calibration speaks for its values (decision 140): past its
            // due date on the day the value was read, the value is expired; none declared, it
            // says so. Every other origin keeps the source-based reading.
            let calibration = &m["instrument"]["calibration"];
            let on_instrument = origin == Origin::Instrument;
            // A calibration the smoke check found for another serial is not applied (decision 143):
            // it speaks for nothing, so the value has none declared and cannot be expired by it.
            let calibration_applied = calibration["applied"] != false;
            let calibration_expired = on_instrument
                && calibration_applied
                && calibration["due"].as_str().is_some_and(|due| due < utc_date(m["at_utc_ms"].as_u64().unwrap_or(0)).as_str());
            let calibration_undeclared = if on_instrument { calibration.is_null() || !calibration_applied } else { source.is_none() };
            let by_person = m["declared_by"].as_array().is_some_and(|d| !d.is_empty() && d.iter().all(|x| x["scope"] == "person"));
            Some(MeasurementRecord {
                measurement: name.to_string(),
                value: match m["value"].as_str() {
                    Some(choice) => MeasuredValue::Choice(choice.to_string()),
                    None => MeasuredValue::Number(m["value"].as_f64().unwrap_or(0.0)),
                },
                u: source.map_or(0.0, |s| s.u),
                evidence: Evidence { incomplete, calibration_expired },
                origin,
                declared_by: if by_person { DeclaredBy::Person } else { DeclaredBy::Agent },
                calibration_undeclared,
            })
        })
        .collect()
}

// ---- the real bench ------------------------------------------------------------------------

/// Core's own bench: its reset path, the exchange route's body and the host-study executor.
pub struct CoreEnv {
    state: AppState,
    handle: tokio::runtime::Handle,
    run_id: String,
    /// The study each test runs, kept at the prepare (decision 136).
    studies: BTreeMap<String, HostStudy>,
    /// The steps of each test's study that energise, by the crate's `energises`, worked out at the
    /// prepare (decision 138). A record kept before that has none.
    energising: BTreeMap<String, BTreeSet<String>>,
    /// The run was cancelled under a study: set from the study's own cancel flag.
    study_cancelled: bool,
    /// Setup prompts raised so far; the next one's id is this plus one (decision 139).
    setups: u32,
}

impl CoreEnv {
    pub fn new(
        state: AppState,
        handle: tokio::runtime::Handle,
        run_id: String,
        studies: BTreeMap<String, HostStudy>,
        energising: BTreeMap<String, BTreeSet<String>>,
    ) -> Self {
        CoreEnv { state, handle, run_id, studies, energising, study_cancelled: false, setups: 0 }
    }

    fn reset_dut(&self) -> Result<(), String> {
        let dut = embarch_topology::hardware::find_enrolled_by_role(embarch_topology::hardware::DUT_ROLE)
            .map_err(|e| format!("{e:#}"))?
            .ok_or("no DUT is enrolled")?;
        let _guard = self.handle.block_on(crate::api::acquire_hw_lock(&self.state, "a test run's precondition")).map_err(|(_, m)| m)?;
        crate::hardware::reset(&dut.chip, dut.probe_serial.as_deref()).map_err(|e| format!("{e:#}"))
    }

    fn send_idle(&self, board: &RunBoard) -> Result<(), String> {
        let _guard = self.handle.block_on(crate::api::acquire_hw_lock(&self.state, "a test run's precondition")).map_err(|(_, m)| m)?;
        // The board's signal is named by its part id (assumed, decision 136): run.json carries no other.
        let req = crate::exchange::ExchangeRequest {
            write: format!("{}\n", board.idle),
            until: None,
            timeout_ms: IDLE_WAIT_MS,
            discard_pending: true,
            match_after_echo: true,
            port_wait_ms: PORT_WAIT_MS,
        };
        crate::api::run_signal_exchange(&self.state.captures, &board.part, req).map(|_| ()).map_err(|(_, m)| format!("{}: {m}", board.part))
    }
}

/// The smoke check's reach into Core's own bench: the topology tables, the probes through the
/// identity gate, the boards' consoles and the instruments' ports, each under `hw_lock`.
struct CoreBench<'a>(&'a CoreEnv);

impl CoreBench<'_> {
    fn lock(&self) -> Result<crate::api::HwGuard, String> {
        self.0.handle.block_on(crate::api::acquire_hw_lock(&self.0.state, "a test run's smoke check")).map_err(|(_, m)| m)
    }

    /// The enrolled board a part id's signal leaves, and how to validate it.
    fn board_of(&self, part: &str) -> Result<embarch_topology::hardware::EnrolledBoard, String> {
        let link = embarch_topology::hardware::find_signal(part)
            .map_err(|e| format!("{e:#}"))?
            .ok_or_else(|| format!("no signal is declared under '{part}'"))?;
        embarch_topology::hardware::find_signal_origin_board(&link).map_err(|e| format!("{e:#}"))?.ok_or_else(|| format!("no board is enrolled where signal '{part}' leaves"))
    }

    fn validated_id(&self, board: &embarch_topology::hardware::EnrolledBoard) -> Result<String, String> {
        let _guard = self.lock()?;
        let v = match &board.probe_serial {
            Some(serial) => crate::probe_hold::around(serial, crate::probe_hold::Use::Core, || embarch_topology::hardware::validate_serial_timed(serial)),
            None => crate::probe_hold::around_role(&board.role, || embarch_topology::hardware::validate_role_timed(&board.role)),
        };
        v.map(|v| v.board.hardware_id.unwrap_or_default()).map_err(|e| format!("{e:#}"))
    }

    fn read_image(&self, board: crate::readback::Board, enrolled: Option<embarch_topology::hardware::EnrolledBoard>) -> crate::readback::ImageStatus {
        use crate::readback::{readback, ImageStatus};
        let refuse = |reason: String| match board {
            crate::readback::Board::Dut => ImageStatus::Claim { reason },
            crate::readback::Board::Bench => ImageStatus::Unverified { reason },
        };
        let Some(enrolled) = enrolled else { return refuse("no board is enrolled to read".into()) };
        let Some(serial) = enrolled.probe_serial.clone() else { return readback(None::<&mut probe_rs::Core<'_>>, board, None, None) };
        let entry = self.0.state.images.entry_for_probe(&serial);
        let list = crate::readback::bench_list_from_env().ok();
        let _guard = match self.lock() {
            Ok(g) => g,
            Err(e) => return refuse(e),
        };
        crate::live::with_core(&enrolled.chip, &serial, "a test run's smoke check", |core, _| Ok(readback(Some(core), board, entry.as_ref(), list.as_ref())))
            .unwrap_or_else(|e| refuse(format!("{e:#}")))
    }
}

impl BenchAccess for CoreBench<'_> {
    fn signal_declared(&mut self, name: &str) -> Result<bool, String> {
        embarch_topology::hardware::find_signal(name).map(|s| s.is_some()).map_err(|e| format!("{e:#}"))
    }

    fn role_declared(&mut self, role: &str) -> Result<bool, String> {
        embarch_topology::hardware::find_enrolled_by_role(role).map(|b| b.is_some()).map_err(|e| format!("{e:#}"))
    }

    fn instrument_declared(&mut self, name: &str) -> Result<bool, String> {
        embarch_topology::hardware::instrument::get(name).map(|i| i.is_some()).map_err(|e| format!("{e:#}"))
    }

    fn dut_identity(&mut self) -> Result<String, String> {
        let dut = embarch_topology::hardware::find_enrolled_by_role(embarch_topology::hardware::DUT_ROLE)
            .map_err(|e| format!("{e:#}"))?
            .ok_or("no DUT is enrolled")?;
        self.validated_id(&dut)
    }

    fn board_identity(&mut self, part: &str) -> Result<String, String> {
        let board = self.board_of(part)?;
        self.validated_id(&board)
    }

    fn hello(&mut self, part: &str, hello: &str) -> Result<String, String> {
        let _guard = self.lock()?;
        let req = crate::exchange::ExchangeRequest {
            write: format!("{hello}\n"),
            until: None,
            timeout_ms: SMOKE_WAIT_MS,
            discard_pending: true,
            match_after_echo: true,
            port_wait_ms: PORT_WAIT_MS,
        };
        let got = crate::api::run_signal_exchange(&self.0.state.captures, part, req).map_err(|(_, m)| m)?;
        // The echo of the line is not an answer: a console that only echoes is not running its shell.
        let reply = got.text.replacen(hello, "", 1).trim().to_string();
        if reply.is_empty() { Err("nothing came back but the echo".into()) } else { Ok(reply) }
    }

    fn idn(&mut self, instrument: &str) -> Result<String, String> {
        let _guard = self.lock()?;
        let asked = (|| -> Result<String, String> {
            let inst = embarch_topology::hardware::instrument::get(instrument).map_err(|e| format!("{e:#}"))?.ok_or("not declared")?;
            let port = crate::instrument::find_port_waiting(&inst, SMOKE_PORT_WAIT_MS).map_err(|e| format!("{e:#}"))?.ok_or("its port is not enumerated")?;
            let req = embarch_topology::hardware::InstrumentExchange { line: "*IDN?".into(), query: true, timeout_ms: SMOKE_WAIT_MS, port_wait_ms: 0 };
            let got = crate::instrument::run(&inst, &port, &req).map_err(|e| format!("{e:#}"))?;
            let reply = got.reply.trim().to_string();
            if reply.is_empty() { Err("the reply was empty".to_string()) } else { Ok(reply) }
        })();
        crate::instrument::note_identity(instrument, asked.as_deref().ok());
        asked
    }

    fn image(&mut self, part: Option<&str>) -> crate::readback::ImageStatus {
        match part {
            None => {
                let dut = embarch_topology::hardware::find_enrolled_by_role(embarch_topology::hardware::DUT_ROLE).ok().flatten();
                self.read_image(crate::readback::Board::Dut, dut)
            }
            Some(part) => {
                let board = self.board_of(part).ok();
                self.read_image(crate::readback::Board::Bench, board)
            }
        }
    }
}

impl Env for CoreEnv {
    fn smoke_check(&mut self, bench: &RunBench, tests: &[&RunTest]) -> SmokeFindings {
        let mut needs = BenchNeeds::default();
        let add = |v: &mut Vec<String>, s: &str| {
            if !v.iter().any(|x| x == s) {
                v.push(s.to_string());
            }
        };
        for b in &bench.boards {
            add(&mut needs.signals, &b.part);
        }
        for t in tests {
            let Some(study) = self.studies.get(&t.test_id) else { continue };
            for (_, _, step) in crate::host_study::flat_steps(study) {
                match &step.action {
                    HostAction::Exchange { signal, .. } => add(&mut needs.signals, signal),
                    HostAction::Instrument { instrument, .. } => add(&mut needs.instruments, instrument),
                    _ => {}
                }
            }
        }
        for s in &needs.signals.clone() {
            if let Ok(Some(link)) = embarch_topology::hardware::find_signal(s) {
                if let embarch_topology::hardware::SignalOrigin::Role(r) = link.origin() {
                    if r != embarch_topology::hardware::DUT_ROLE {
                        add(&mut needs.roles, r);
                    }
                }
            }
        }
        crate::smoke_check::check_bench(&mut CoreBench(self), bench, &needs)
    }

    fn setup(&mut self, bench: &RunBench, config: &str, missing: &[String]) -> SetupEnd {
        use crate::host_study::{PendingPrompt, PromptSlot, PromptEnd, PromptAnswer};
        self.setups += 1;
        let opened = crate::study::current_utc_ms();
        let missing_text = if missing.is_empty() { "nothing is checked yet, so nothing is listed as missing".to_string() } else { missing.join(", ") };
        let pending = PendingPrompt {
            prompt_id: self.setups,
            list: HostList::Steps,
            step_index: 0,
            step_name: format!("setup:{}", bench.name),
            text: format!("Set up the bench '{}' (configuration '{}') for the next tests. Missing: {missing_text}.", bench.name, config),
            bench_config: Some(config.to_string()),
            kind: Some(PromptKind::Setup),
            observation: None,
            switched_on: vec![],
            opened_utc_ms: opened,
            deadline_utc_ms: opened + SETUP_WAIT_MS,
            session: None,
        };
        // The window opens with the prompt and closes when it ends, whichever way it ends.
        *self.state.setup_window.lock().unwrap() = Some(self.run_id.clone());
        *self.state.host_prompt.lock().unwrap() = Some(PromptSlot { study_id: self.run_id.clone(), pending: pending.clone(), answer: None, extra: None, session: None });
        let _ = self.state.study_events.send(crate::study::StudyEvent::PromptPending { study_id: self.run_id.clone(), prompt: pending.clone() });
        let (end, extra) = crate::host_study::wait_for_answer_until(&self.state.host_prompt, pending.prompt_id, Duration::from_millis(SETUP_WAIT_MS), &|| self.cancelled());
        *self.state.setup_window.lock().unwrap() = None;
        let _ = self.state.study_events.send(crate::study::StudyEvent::PromptResolved {
            study_id: self.run_id.clone(),
            prompt_id: pending.prompt_id,
            answer: end.answer_str().to_string(),
            answered_by: match end {
                PromptEnd::Answered(_, by) => Some(by),
                _ => None,
            },
            resolved_utc_ms: crate::study::current_utc_ms(),
        });
        let scope = extra.as_ref().map(|e| e.scope);
        if let Err(e) = self.state.run_log.test_run_setup_ended(&self.run_id, &bench.name, end.answer_str(), scope) {
            tracing::error!(run_id = self.run_id, "could not enter the setup prompt's end in the run log: {e:#}");
        }
        match end {
            PromptEnd::Answered(PromptAnswer::Confirmed, _) => SetupEnd::Confirmed,
            PromptEnd::Answered(PromptAnswer::Cancelled, _) => SetupEnd::Declined,
            PromptEnd::TimedOut => SetupEnd::TimedOut,
            PromptEnd::StudyCancelled => SetupEnd::Cancelled,
        }
    }

    fn cancelled(&self) -> bool {
        self.study_cancelled || self.state.lease.run_in_flight().as_deref() != Some(self.run_id.as_str())
    }

    fn precondition(&mut self, bench: &RunBench, test: &RunTest) -> Result<(), String> {
        match test.precondition {
            Precondition::None => Ok(()),
            Precondition::Reset => {
                self.reset_dut().map_err(|e| format!("the DUT would not reset: {e}"))?;
                bench.boards.iter().try_for_each(|b| self.send_idle(b))
            }
            // run.json carries a board's idle and hello lines only: no command for these, so
            // Core cannot perform them, and a test that asks is an error rather than a silent skip.
            other => Err(format!("{other:?} is declared by the bench, and run.json carries no command for it")),
        }
    }

    fn checkpoint(&mut self, results: &[TestResult], smoke: &[SmokeFindings]) {
        if let Err(e) = crate::test_runs::write_checkpoint(self.state.run_log.data_dir(), &self.run_id, results, smoke) {
            tracing::error!(run_id = self.run_id, "could not keep the run's checkpoint: {e}");
        }
    }

    fn run_study(&mut self, test: &RunTest) -> StudyRun {
        let abort = |why: String| StudyRun { steps: vec![], measurements: vec![], cancelled: false, aborted: Some(why), study_id: None };
        let Some(study) = self.studies.get(&test.test_id).cloned() else {
            return abort(format!("Core kept no study for {} at the prepare", test.test_id));
        };
        let ctx_kinds: BTreeMap<String, PromptKind> = test.prompt_kinds.iter().map(|k| (k.step.clone(), k.kind)).collect();
        let ctx = RunContext {
            kinds: ctx_kinds.clone(),
            energising: self.energising.get(&test.test_id).cloned().unwrap_or_default(),
        };
        let ran = match crate::host_study::run_study_blocking(&self.state, &self.handle, &study, Scope::Person, ctx) {
            Ok(r) => r,
            Err(why) => return abort(why),
        };
        let read = |f: &str| std::fs::read(ran.dir.join(f)).ok().and_then(|b| serde_json::from_slice::<Value>(&b).ok());
        let origin = &crate::measurements::topology_signal_origin;
        let steps = read("events.json").map(|e| judge_steps(&study, &ctx_kinds, &e, origin)).unwrap_or_default();
        let measurements = read("measurements.json").map(|m| judge_measurements(test, &m)).unwrap_or_default();
        self.study_cancelled = ran.cancelled;
        StudyRun { steps, measurements, cancelled: ran.cancelled, aborted: None, study_id: Some(ran.study_id) }
    }
}

/// Starts the executor for a run the go just started, on its own blocking thread, and ends the
/// run through [`crate::test_runs::end_run`] when it is done (`None` scope: Core ended it).
pub fn spawn(state: AppState, run_id: String, run: RunJson, studies: BTreeMap<String, HostStudy>, energising: BTreeMap<String, BTreeSet<String>>) {
    let handle = tokio::runtime::Handle::current();
    let progress = Arc::new(Mutex::new(Progress { state: "running", ..Default::default() }));
    state.test_progress.lock().unwrap().insert(run_id.clone(), progress.clone());
    tokio::task::spawn_blocking(move || {
        let mut env = CoreEnv::new(state.clone(), handle, run_id.clone(), studies, energising);
        let calls = crate::call_log::RunCalls::begin(&state.call_log);
        let outcome = drive_awake(&mut crate::keep_awake::HostKeepAwake, &run, &mut env, &progress, &Rules::latest(), deadline(&run));
        let body = json!({
            "run_id": run_id, "status": outcome.status, "ended_utc_ms": crate::study::current_utc_ms(),
            "results": outcome.results.iter().map(TestResult::to_json).collect::<Vec<_>>(),
            "smoke_checks": outcome.smoke,
        });
        if let Err(e) = std::fs::write(crate::test_runs::result_path(state.run_log.data_dir(), &run_id), body.to_string()) {
            tracing::error!(run_id, "could not keep the run's result: {e}");
        }
        let reason = (outcome.status == "deadline").then_some("the run passed its deadline");
        // The run's own signed record and log entries, whichever way it ended (decision 144).
        match crate::test_runs::read_record(state.run_log.data_dir(), &run_id) {
            Ok(rec) => {
                let end = crate::test_run_record::Ended {
                    status: outcome.status,
                    reason,
                    results: &outcome.results,
                    unreached: crate::test_run_record::NOT_RUN,
                    smoke: &outcome.smoke,
                    call_log: calls.and_then(|c| c.at_end()).map(|(v, _)| v),
                };
                crate::test_run_record::seal(&state.sealer, &state.run_log, &rec, &run, &end);
            }
            Err(e) => tracing::error!(run_id, "could not read the run's record to sign it: {e:?}"),
        }
        crate::test_runs::end_run(&state, &run_id, outcome.status, reason, None);
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;

    fn test(id: &str, bench: &str, run_count: u32, requires: &[&str]) -> RunTest {
        serde_json::from_value(json!({
            "test_id": id, "title": id, "definition_hash": "0".repeat(64),
            "study": { "kind": "host", "hash": "0".repeat(64), "file": format!("{id}.json") },
            "requirements": [], "revision_range": { "from": "3.0", "to": null },
            "bench": { "name": bench, "config": "default" },
            "precondition": "reset", "requires": requires, "run_count": run_count,
            "decision_rule": { "kind": "simple_acceptance", "w_multiple": 0.0 },
            "limits": [ { "measurement": "vbus", "unit": "mV", "lower": 4750.0, "upper": 5250.0 } ],
            "setup_terms": [], "sources": [ { "measurement": "vbus", "unit": "mV", "range_min": 0.0, "range_max": 9000.0, "resolution": 1.0, "U": 10.0, "k": 2.0 } ],
            "prompt_kinds": [], "hooks": [], "step_budget_ms": 1000
        }))
        .unwrap_or_else(|e| panic!("{e}"))
    }

    fn run_of(benches: &[(&str, &[&str])], tests: Vec<RunTest>, stop_on_fail: bool) -> RunJson {
        serde_json::from_value(json!({
            "schema": 1, "run_id": "r1", "suite": { "id": "s", "version": "1" }, "plan_hash": null,
            "standard": { "name": "usb-pd", "revision": "3.2" },
            "dut": { "hardware_id": null, "declared_serial": "u", "board_revision": "A", "capabilities": [], "hooks": [] },
            "options": { "stop_on_fail": stop_on_fail },
            "benches": benches.iter().map(|(n, t)| json!({ "name": n, "tests": t, "boards": [ { "part": "pd", "idle": "idle", "hello": "hello" } ] })).collect::<Vec<_>>(),
            "tests": tests,
        }))
        .unwrap_or_else(|e| panic!("{e}"))
    }

    /// A scripted bench: each study run pops the next outcome.
    struct Fake {
        runs: VecDeque<StudyRun>,
        precondition_fails: Vec<String>,
        cancel_after_runs: Option<usize>,
        ran: Vec<String>,
        prepared: Vec<String>,
        /// The benches a setup prompt was raised for, with the configuration, and how each ends.
        setups: Vec<(String, String)>,
        setup_ends: VecDeque<SetupEnd>,
        /// One scripted bench access per smoke check, in order; none left is a pass.
        smoke: VecDeque<crate::smoke_check::testing::Fake>,
        smoked: Vec<String>,
        /// How many results each checkpoint held.
        checkpoints: Vec<usize>,
    }

    fn fake(runs: Vec<StudyRun>) -> Fake {
        Fake { runs: runs.into(), precondition_fails: vec![], cancel_after_runs: None, ran: vec![], prepared: vec![], setups: vec![], setup_ends: VecDeque::new(), smoke: VecDeque::new(), smoked: vec![], checkpoints: vec![] }
    }

    fn steps(outcome: StepOutcome, target: Target) -> Vec<JudgeStep> {
        vec![JudgeStep { step: "s".into(), target, outcome }]
    }

    fn measured(v: f64) -> Vec<MeasurementRecord> {
        vec![MeasurementRecord {
            measurement: "vbus".into(),
            value: MeasuredValue::Number(v),
            u: 10.0,
            evidence: Evidence::default(),
            origin: Origin::Instrument,
            declared_by: DeclaredBy::Person,
            calibration_undeclared: false,
        }]
    }

    fn pass() -> StudyRun {
        StudyRun { steps: steps(StepOutcome::Pass, Target::Dut), measurements: measured(5000.0), cancelled: false, aborted: None, study_id: Some("st".into()) }
    }
    fn fail() -> StudyRun {
        StudyRun { steps: steps(StepOutcome::Pass, Target::Dut), measurements: measured(6000.0), cancelled: false, aborted: None, study_id: Some("st".into()) }
    }
    fn lost_bench_port() -> StudyRun {
        StudyRun { steps: steps(StepOutcome::PortLost, Target::Bench), measurements: vec![], cancelled: false, aborted: None, study_id: Some("st".into()) }
    }

    impl Env for Fake {
        fn checkpoint(&mut self, results: &[TestResult], _: &[SmokeFindings]) {
            self.checkpoints.push(results.len());
        }
        fn setup(&mut self, bench: &RunBench, config: &str, missing: &[String]) -> SetupEnd {
            assert!(missing.is_empty(), "the setup prompt names nothing: the smoke check runs after it");
            self.setups.push((bench.name.clone(), config.to_string()));
            self.setup_ends.pop_front().unwrap_or(SetupEnd::Confirmed)
        }
        fn smoke_check(&mut self, bench: &RunBench, _: &[&RunTest]) -> SmokeFindings {
            self.smoked.push(bench.name.clone());
            let mut access = self.smoke.pop_front().unwrap_or_default();
            let needs = crate::smoke_check::testing::needs();
            crate::smoke_check::check_bench(&mut access, bench, &needs)
        }
        fn cancelled(&self) -> bool {
            self.cancel_after_runs.is_some_and(|n| self.ran.len() >= n)
        }
        fn precondition(&mut self, _: &RunBench, t: &RunTest) -> Result<(), String> {
            self.prepared.push(t.test_id.clone());
            if self.precondition_fails.contains(&t.test_id) {
                return Err("no reset".into());
            }
            Ok(())
        }
        fn run_study(&mut self, t: &RunTest) -> StudyRun {
            self.ran.push(t.test_id.clone());
            self.runs.pop_front().expect("a scripted run")
        }
    }

    fn go(run: &RunJson, env: &mut Fake) -> RunOutcome {
        drive(run, env, &Mutex::new(Progress::default()), &Rules::latest(), Duration::from_secs(60))
    }

    fn verdicts(o: &RunOutcome) -> Vec<(String, Verdict, Option<Reason>)> {
        o.results.iter().map(|r| (r.test_id.clone(), r.judged.verdict, r.judged.reason)).collect()
    }

    #[test]
    fn a_setup_prompt_is_raised_before_every_bench_after_the_first_and_names_it() {
        let run = run_of(&[("a", &["t1"]), ("b", &["t2"])], vec![test("t1", "a", 1, &[]), test("t2", "b", 1, &[])], false);
        let mut env = fake(vec![pass(), pass()]);
        let out = go(&run, &mut env);
        assert_eq!(env.setups, vec![("b".to_string(), "default".to_string())]);
        assert_eq!(out.status, "completed");
        assert!(out.results.iter().all(|r| r.judged.verdict == Verdict::Pass));
    }

    #[test]
    fn a_declined_or_timed_out_setup_is_an_error_on_that_bench_and_the_run_goes_on() {
        for end in [SetupEnd::Declined, SetupEnd::TimedOut] {
            let run = run_of(&[("a", &["t1"]), ("b", &["t2", "t3"]), ("c", &["t4"])], vec![test("t1", "a", 1, &[]), test("t2", "b", 1, &[]), test("t3", "b", 1, &[]), test("t4", "c", 1, &[])], false);
            let mut env = fake(vec![pass(), pass()]);
            env.setup_ends = VecDeque::from([end]);
            let out = go(&run, &mut env);
            let v = verdicts(&out);
            assert_eq!(v[1], ("t2".to_string(), Verdict::Error, Some(Reason::BenchStepFailed)), "{end:?}");
            assert_eq!(v[2].2, Some(Reason::StoppedByError));
            assert_eq!(v[3].1, Verdict::Pass, "bench c still runs");
            assert_eq!(env.ran, vec!["t1", "t4"]);
            assert_eq!(env.setups.len(), 2);
        }
    }

    use crate::readback::ImageStatus;
    use crate::smoke_check::testing::Fake as Access;

    #[test]
    fn a_bench_failing_its_smoke_check_skips_its_tests_as_smoke_check_errors_naming_the_piece() {
        let cases: Vec<(Access, &str)> = vec![
            (Access { missing_signal: Some("pd"), ..Default::default() }, "signal 'pd': not declared"),
            (Access { dut_id: Some(Err("mismatch".into())), ..Default::default() }, "the DUT: validate failed"),
            (Access { silent_hello: true, ..Default::default() }, "bench board 'pd': no answer to its hello line"),
            (Access { silent_idn: true, ..Default::default() }, "instrument 'dmm': no answer to *IDN?"),
            (Access { dut_image: Some(ImageStatus::Differs { address: 0, length: 4 }), ..Default::default() }, "the DUT: its image differs"),
        ];
        for (access, piece) in cases {
            let run = run_of(&[("a", &["t1", "t2"]), ("b", &["t3"])], vec![test("t1", "a", 1, &[]), test("t2", "a", 1, &[]), test("t3", "b", 1, &[])], false);
            let mut env = fake(vec![pass()]);
            env.smoke = VecDeque::from([access]);
            let out = go(&run, &mut env);
            let v = verdicts(&out);
            assert_eq!(v[0], ("t1".to_string(), Verdict::Error, Some(Reason::SmokeCheck)), "{piece}");
            assert_eq!(v[1], ("t2".to_string(), Verdict::Error, Some(Reason::SmokeCheck)), "{piece}");
            assert!(out.results[0].note.as_deref().unwrap().contains(piece), "{:?}", out.results[0].note);
            assert_eq!(v[2].1, Verdict::Pass, "bench b still runs");
            assert_eq!(env.ran, vec!["t3"]);
            assert_eq!(env.smoked, vec!["a", "b"], "the first bench is checked too");
            assert_eq!(out.smoke.len(), 2);
        }
    }

    #[test]
    fn a_different_dut_unit_on_a_later_bench_refuses_that_bench_and_the_first_stands() {
        let run = run_of(&[("a", &["t1"]), ("b", &["t2"])], vec![test("t1", "a", 1, &[]), test("t2", "b", 1, &[])], false);
        let mut env = fake(vec![pass(), pass()]);
        env.smoke = VecDeque::from([Access::default(), Access { dut_id: Some(Ok("DUT-2".into())), ..Default::default() }]);
        let out = go(&run, &mut env);
        let v = verdicts(&out);
        assert_eq!(v[0].1, Verdict::Pass, "bench a's result stands");
        assert_eq!(v[1], ("t2".to_string(), Verdict::Error, Some(Reason::SmokeCheck)));
        assert!(out.results[1].note.as_deref().unwrap().contains("DUT-1") && out.results[1].note.as_deref().unwrap().contains("DUT-2"));
        assert_eq!(env.ran, vec!["t1"]);
    }

    #[test]
    fn a_passing_smoke_check_keeps_its_findings_on_the_progress_and_the_outcome() {
        let run = run_of(&[("a", &["t1"])], vec![test("t1", "a", 1, &[])], false);
        let mut env = fake(vec![pass()]);
        let progress = Mutex::new(Progress::default());
        let out = drive(&run, &mut env, &progress, &Rules::latest(), Duration::from_secs(60));
        assert_eq!(out.smoke[0].unit.as_deref(), Some("DUT-1"));
        assert_eq!(out.smoke[0].instruments[0].reply, "ACME,dmm,S1,1.0");
        assert_eq!(progress.lock().unwrap().smoke, out.smoke);
    }

    #[test]
    fn a_cancel_during_the_setup_prompt_ends_the_run() {
        let run = run_of(&[("a", &["t1"]), ("b", &["t2"])], vec![test("t1", "a", 1, &[]), test("t2", "b", 1, &[])], false);
        let mut env = fake(vec![pass()]);
        env.setup_ends = VecDeque::from([SetupEnd::Cancelled]);
        let out = go(&run, &mut env);
        assert_eq!(out.status, "cancelled");
        assert_eq!(out.results[1].judged.verdict, Verdict::Cancelled);
        assert_eq!(env.ran, vec!["t1"]);
    }

    #[test]
    fn the_deadline_allows_each_setup_prompt_after_the_first_bench() {
        let one = run_of(&[("a", &["t1"])], vec![test("t1", "a", 1, &[])], false);
        let two = run_of(&[("a", &["t1"]), ("b", &["t2"])], vec![test("t1", "a", 1, &[]), test("t2", "b", 1, &[])], false);
        assert!(deadline(&two) >= deadline(&one) + Duration::from_millis(SETUP_WAIT_MS));
    }

    /// The real setup prompt: raised under the run's id with kind `setup`, the window open while it
    /// waits and shut after, and the agent's answer recorded as relayed (decision 139).
    #[test]
    fn the_real_setup_prompt_opens_the_window_and_records_an_agents_answer_as_relayed() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let state = AppState::new("tok".to_string());
        *state.run_lock.lock().unwrap() = Some("r1".to_string());
        let mut env = CoreEnv::new(state.clone(), rt.handle().clone(), "r1".to_string(), BTreeMap::new(), BTreeMap::new());
        let bench: RunBench = serde_json::from_value(json!({ "name": "b", "tests": [], "boards": [] })).unwrap();
        let answerer = {
            let state = state.clone();
            std::thread::spawn(move || {
                let start = Instant::now();
                loop {
                    if let Some(p) = crate::host_study::pending_prompt(&state.host_prompt, "r1") {
                        assert_eq!(p.kind, Some(PromptKind::Setup));
                        assert!(p.text.contains("'b'") && p.text.contains("'default'"), "{}", p.text);
                        assert_eq!(state.setup_window.lock().unwrap().as_deref(), Some("r1"));
                        let body: crate::host_study::PromptAnswerBody =
                            serde_json::from_value(json!({ "prompt_id": p.prompt_id, "answer": "confirmed", "answered_by": "cli" })).unwrap();
                        crate::host_study::record_answer_scoped(&state.host_prompt, "r1", &body, Scope::Agent).unwrap();
                        return;
                    }
                    assert!(start.elapsed() < Duration::from_secs(10), "no setup prompt appeared");
                    std::thread::sleep(Duration::from_millis(5));
                }
            })
        };
        assert_eq!(env.setup(&bench, "default", &[]), SetupEnd::Confirmed);
        answerer.join().unwrap();
        assert_eq!(*state.setup_window.lock().unwrap(), None);
        let entry = state.run_log.lines().unwrap().into_iter().map(|l| l["entry"].clone()).find(|e| e["type"] == "test_run_setup_ended").expect("an entry");
        assert_eq!((entry["scope"].as_str(), entry["relayed"].as_bool(), entry["bench"].as_str()), (Some("agent"), Some(true), Some("b")));
    }

    #[test]
    fn a_fail_in_test_one_still_runs_test_two() {
        let run = run_of(&[("A", &["t1", "t2"])], vec![test("t1", "A", 1, &[]), test("t2", "A", 1, &[])], false);
        let mut env = fake(vec![fail(), pass()]);
        let o = go(&run, &mut env);
        assert_eq!(o.status, "completed");
        assert_eq!(verdicts(&o), [("t1".into(), Verdict::Fail, Some(Reason::Limit)), ("t2".into(), Verdict::Pass, None)]);
        assert_eq!(env.prepared, ["t1", "t2"], "a precondition before every test");
    }

    #[test]
    fn an_error_on_bench_a_skips_the_rest_of_a_and_runs_bench_b() {
        let run = run_of(
            &[("A", &["a1", "a2"]), ("B", &["b1"])],
            vec![test("a1", "A", 1, &[]), test("a2", "A", 1, &[]), test("b1", "B", 1, &[])],
            false,
        );
        let mut env = fake(vec![lost_bench_port(), pass()]);
        let o = go(&run, &mut env);
        assert_eq!(
            verdicts(&o),
            [
                ("a1".into(), Verdict::Error, Some(Reason::PortLost)),
                ("a2".into(), Verdict::Skipped, Some(Reason::StoppedByError)),
                ("b1".into(), Verdict::Pass, None),
            ]
        );
        assert_eq!(env.ran, ["a1", "b1"]);
    }

    #[test]
    fn a_failed_precondition_is_an_error_that_ends_its_bench() {
        let run = run_of(&[("A", &["a1", "a2"]), ("B", &["b1"])], vec![test("a1", "A", 1, &[]), test("a2", "A", 1, &[]), test("b1", "B", 1, &[])], false);
        let mut env = fake(vec![pass()]);
        env.precondition_fails = vec!["a1".into()];
        let o = go(&run, &mut env);
        assert_eq!(verdicts(&o)[0].1, Verdict::Error);
        assert_eq!(verdicts(&o)[1], ("a2".into(), Verdict::Skipped, Some(Reason::StoppedByError)));
        assert_eq!(verdicts(&o)[2].1, Verdict::Pass);
        assert_eq!(env.ran, ["b1"], "no study ran on a bench whose precondition failed");
    }

    #[test]
    fn a_cancel_marks_every_remaining_test_cancelled() {
        let run = run_of(&[("A", &["a1", "a2"]), ("B", &["b1"])], vec![test("a1", "A", 1, &[]), test("a2", "A", 1, &[]), test("b1", "B", 1, &[])], false);
        let mut env = fake(vec![pass()]);
        env.cancel_after_runs = Some(1);
        let o = go(&run, &mut env);
        assert_eq!(o.status, "cancelled");
        assert_eq!(verdicts(&o), [("a1".into(), Verdict::Pass, None), ("a2".into(), Verdict::Cancelled, None), ("b1".into(), Verdict::Cancelled, None)]);
        assert_eq!(env.ran, ["a1"]);
    }

    #[test]
    fn a_study_cancelled_under_its_test_cancels_the_test_and_the_rest() {
        let run = run_of(&[("A", &["a1", "a2"])], vec![test("a1", "A", 1, &[]), test("a2", "A", 1, &[])], false);
        let mut cut = pass();
        cut.cancelled = true;
        let mut env = fake(vec![cut]);
        let o = go(&run, &mut env);
        assert_eq!(o.status, "cancelled");
        assert_eq!(verdicts(&o).iter().map(|v| v.1).collect::<Vec<_>>(), [Verdict::Cancelled, Verdict::Cancelled]);
    }

    #[test]
    fn stop_on_fail_skips_the_tests_after_the_first_fail() {
        let run = run_of(&[("A", &["t1", "t2"]), ("B", &["t3"])], vec![test("t1", "A", 1, &[]), test("t2", "A", 1, &[]), test("t3", "B", 1, &[])], true);
        let mut env = fake(vec![fail()]);
        let o = go(&run, &mut env);
        assert_eq!(
            verdicts(&o),
            [
                ("t1".into(), Verdict::Fail, Some(Reason::Limit)),
                ("t2".into(), Verdict::Skipped, Some(Reason::StoppedAfterFail)),
                ("t3".into(), Verdict::Skipped, Some(Reason::StoppedAfterFail)),
            ]
        );
        assert_eq!(o.status, "completed");
    }

    #[test]
    fn a_test_requiring_a_failed_one_is_requires_not_met() {
        let run = run_of(&[("A", &["t1", "t2", "t3"])], vec![test("t1", "A", 1, &[]), test("t2", "A", 1, &["t1"]), test("t3", "A", 1, &[])], false);
        let mut env = fake(vec![fail(), pass()]);
        let o = go(&run, &mut env);
        assert_eq!(verdicts(&o)[1], ("t2".into(), Verdict::Skipped, Some(Reason::RequiresNotMet)));
        assert_eq!(verdicts(&o)[2].1, Verdict::Pass, "an unrelated test still runs");
    }

    #[test]
    fn a_run_count_of_three_with_one_failing_run_is_a_fail_with_counts() {
        let run = run_of(&[("A", &["t1"])], vec![test("t1", "A", 3, &[])], false);
        let mut env = fake(vec![pass(), fail(), pass()]);
        let o = go(&run, &mut env);
        let r = &o.results[0];
        assert_eq!(r.judged.verdict, Verdict::Fail);
        assert_eq!(r.counts.map(|c| (c.pass, c.fail)), Some((2, 1)));
        assert_eq!(r.runs.len(), 3);
        assert_eq!(env.prepared.len(), 1, "the precondition is before the test, not each run");
    }

    #[test]
    fn a_lost_port_on_a_bench_board_is_error_and_until_not_seen_on_the_dut_is_fail() {
        let run = run_of(&[("A", &["bench", "dut"])], vec![test("bench", "A", 1, &[]), test("dut", "A", 1, &[])], false);
        // `bench`'s port is lost: an error, which skips `dut`; judged apart, `dut`'s until_not_seen fails.
        let mut env = fake(vec![lost_bench_port()]);
        assert_eq!(go(&run, &mut env).results[0].judged, Judged { verdict: Verdict::Error, reason: Some(Reason::PortLost), qualifiers: vec![] });
        let run = run_of(&[("A", &["dut"])], vec![test("dut", "A", 1, &[])], false);
        let mut env = fake(vec![StudyRun { steps: steps(StepOutcome::UntilNotSeen, Target::Dut), measurements: vec![], cancelled: false, aborted: None, study_id: None }]);
        let j = go(&run, &mut env).results[0].judged.clone();
        assert_eq!((j.verdict, j.reason), (Verdict::Fail, Some(Reason::StepTimedOut)));
    }

    #[test]
    fn a_study_core_could_not_run_is_an_error() {
        let run = run_of(&[("A", &["t1", "t2"])], vec![test("t1", "A", 1, &[]), test("t2", "A", 1, &[])], false);
        let mut env = fake(vec![StudyRun { steps: vec![], measurements: vec![], cancelled: false, aborted: Some("a signal is not declared".into()), study_id: None }]);
        let o = go(&run, &mut env);
        assert_eq!(verdicts(&o)[0].1, Verdict::Error);
        assert!(o.results[0].note.as_deref().unwrap().contains("not declared"));
        assert_eq!(verdicts(&o)[1], ("t2".into(), Verdict::Skipped, Some(Reason::StoppedByError)));
    }

    fn awake(env: &mut Fake, run: &RunJson) -> (Vec<&'static str>, RunOutcome) {
        let rec = crate::keep_awake::testing::Recorder::default();
        let o = drive_awake(&mut rec.clone(), run, env, &Mutex::new(Progress::default()), &Rules::latest(), Duration::from_secs(60));
        let seen = rec.0.lock().unwrap().clone();
        (seen, o)
    }

    #[test]
    fn a_run_holds_the_host_awake_once_and_releases_it_when_it_completes() {
        let run = run_of(&[("A", &["t1"])], vec![test("t1", "A", 1, &[])], false);
        let (seen, o) = awake(&mut fake(vec![pass()]), &run);
        assert_eq!(o.status, "completed");
        assert_eq!(seen, ["hold", "release"]);
    }

    #[test]
    fn a_cancelled_run_releases_the_hold() {
        let run = run_of(&[("A", &["t1", "t2"])], vec![test("t1", "A", 1, &[]), test("t2", "A", 1, &[])], false);
        let mut env = fake(vec![pass()]);
        env.cancel_after_runs = Some(1);
        let (seen, o) = awake(&mut env, &run);
        assert_eq!(o.status, "cancelled");
        assert_eq!(seen, ["hold", "release"]);
    }

    #[test]
    fn a_run_with_an_error_releases_the_hold() {
        let run = run_of(&[("A", &["t1"])], vec![test("t1", "A", 1, &[])], false);
        let mut env = fake(vec![StudyRun { steps: vec![], measurements: vec![], cancelled: false, aborted: Some("a signal is not declared".into()), study_id: None }]);
        let (seen, o) = awake(&mut env, &run);
        assert_eq!(o.results[0].judged.verdict, Verdict::Error);
        assert_eq!(seen, ["hold", "release"]);
    }

    #[test]
    fn a_run_that_panics_releases_the_hold() {
        let run = run_of(&[("A", &["t1"])], vec![test("t1", "A", 1, &[])], false);
        let rec = crate::keep_awake::testing::Recorder::default();
        let mut keeper = rec.clone();
        // No scripted study run: the fake panics inside the drive.
        let mut env = fake(vec![]);
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            drive_awake(&mut keeper, &run, &mut env, &Mutex::new(Progress::default()), &Rules::latest(), Duration::from_secs(60))
        }));
        assert!(r.is_err());
        assert_eq!(*rec.0.lock().unwrap(), ["hold", "release"]);
    }

    #[test]
    fn a_run_past_its_deadline_cancels_what_is_left() {
        let run = run_of(&[("A", &["t1", "t2"])], vec![test("t1", "A", 1, &[]), test("t2", "A", 1, &[])], false);
        let mut env = fake(vec![pass(), pass()]);
        let o = drive(&run, &mut env, &Mutex::new(Progress::default()), &Rules::latest(), Duration::ZERO);
        assert_eq!(o.status, "deadline");
        assert!(o.results.iter().all(|r| r.judged.verdict == Verdict::Cancelled));
        assert!(env.ran.is_empty());
    }

    #[test]
    fn progress_names_the_bench_and_test_and_the_verdicts_so_far() {
        struct Peek<'a>(&'a Mutex<Progress>, Vec<(Option<String>, Option<String>, usize)>);
        impl Env for Peek<'_> {
            fn cancelled(&self) -> bool {
                false
            }
            fn precondition(&mut self, _: &RunBench, _: &RunTest) -> Result<(), String> {
                Ok(())
            }
            fn run_study(&mut self, _: &RunTest) -> StudyRun {
                let p = self.0.lock().unwrap();
                self.1.push((p.bench.clone(), p.test.clone(), p.results.len()));
                pass()
            }
        }
        let run = run_of(&[("A", &["t1", "t2"])], vec![test("t1", "A", 1, &[]), test("t2", "A", 1, &[])], false);
        let progress = Mutex::new(Progress::default());
        let mut env = Peek(&progress, vec![]);
        drive(&run, &mut env, &progress, &Rules::latest(), Duration::from_secs(60));
        assert_eq!(env.1, [(Some("A".into()), Some("t1".into()), 0), (Some("A".into()), Some("t2".into()), 1)]);
        assert_eq!(progress.lock().unwrap().state, "completed");
    }

    #[test]
    fn a_signal_is_the_duts_only_when_its_origin_is_the_dut_role() {
        assert_eq!(target_of(Some(SignalOrigin::Role("dut".into()))), Target::Dut);
        assert_eq!(target_of(Some(SignalOrigin::Role("partner".into()))), Target::Bench);
        assert_eq!(target_of(Some(SignalOrigin::Board("serial".into()))), Target::Bench);
        assert_eq!(target_of(None), Target::Dut);
    }

    #[test]
    fn events_become_steps_charged_by_what_they_addressed() {
        let study: HostStudy = serde_json::from_value(json!({
            "name": "s", "steps": [
                { "name": "dut-line", "timeout_ms": 100, "action": { "Exchange": { "signal": "dut-console", "line": "x", "until": "ok" } } },
                { "name": "partner-line", "timeout_ms": 100, "action": { "Exchange": { "signal": "partner", "line": "x", "until": "ok" } } },
                { "name": "meter", "timeout_ms": 100, "action": { "Instrument": { "instrument": "dmm", "line": "READ?", "query": true } } }
            ], "teardown": []
        }))
        .unwrap_or_else(|e| panic!("{e}"));
        let events = json!({ "steps": [
            { "step_name": "dut-line", "detail": "until_not_seen" },
            { "step_name": "partner-line", "detail": "port_lost" },
            { "step_name": "meter", "detail": "empty_reply" },
            { "step_name": "meter", "detail": "cancelled" },
        ]});
        let origin = |s: &str| match s {
            "dut-console" => Some(SignalOrigin::Role("dut".into())),
            "partner" => Some(SignalOrigin::Board("sn1".into())),
            _ => None,
        };
        let got = judge_steps(&study, &BTreeMap::new(), &events, &origin);
        let shown: Vec<(&str, Target, StepOutcome)> = got.iter().map(|s| (s.step.as_str(), s.target, s.outcome)).collect();
        assert_eq!(
            shown,
            [
                ("dut-line", Target::Dut, StepOutcome::UntilNotSeen),
                ("partner-line", Target::Bench, StepOutcome::PortLost),
                ("meter", Target::Bench, StepOutcome::EmptyReply),
            ],
            "a step a cancel cut short says nothing"
        );
    }

    #[test]
    fn a_refusal_judges_by_the_target_the_row_recorded_not_a_second_lookup() {
        let study: HostStudy = serde_json::from_value(json!({
            "name": "s", "steps": [
                { "name": "dut-line", "timeout_ms": 100, "action": { "Exchange": { "signal": "dut-console", "line": "x", "until": "ok" } } },
                { "name": "partner-line", "timeout_ms": 100, "action": { "Exchange": { "signal": "partner", "line": "x", "until": "ok" } } }
            ], "teardown": []
        }))
        .unwrap_or_else(|e| panic!("{e}"));
        let events = json!({ "steps": [
            { "step_name": "dut-line", "signal": "dut-console", "target": "dut", "detail": "refused" },
            { "step_name": "partner-line", "signal": "partner", "target": "bench", "detail": "refused" },
        ]});
        // A topology that has since changed its mind: the recorded targets still stand.
        let flipped = |s: &str| match s {
            "dut-console" => Some(SignalOrigin::Board("sn1".into())),
            _ => Some(SignalOrigin::Role("dut".into())),
        };
        let steps = judge_steps(&study, &BTreeMap::new(), &events, &flipped);
        assert_eq!(steps.iter().map(|s| s.target).collect::<Vec<_>>(), [Target::Dut, Target::Bench]);
        let rules = Rules::latest();
        let verdicts: Vec<(Verdict, Option<Reason>)> = steps.iter().map(|s| rules.step_verdict(s.target, s.outcome).map(|j| (j.verdict, j.reason)).unwrap()).collect();
        assert_eq!(verdicts, [(Verdict::Fail, Some(Reason::StepFailed)), (Verdict::Error, Some(Reason::BenchStepFailed))]);
    }

    #[test]
    fn an_unanswered_prompt_is_judged_by_the_kind_run_json_gives_it_never_the_studys_own() {
        // The study calls both prompts what it likes; `run.json` says which is the observation.
        let study: HostStudy = serde_json::from_value(json!({
            "name": "s", "steps": [
                { "name": "look", "timeout_ms": 100, "action": { "Prompt": { "text": "?", "kind": "setup" } } },
                { "name": "plug", "timeout_ms": 100, "action": { "Prompt": { "text": "?", "kind": "observation", "answer": { "name": "x", "answers": ["a"] } } } }
            ], "teardown": []
        }))
        .unwrap_or_else(|e| panic!("{e}"));
        let kinds = BTreeMap::from([("look".to_string(), PromptKind::Observation), ("plug".to_string(), PromptKind::Action)]);
        let events = json!({ "steps": [
            { "step_name": "look", "detail": "prompt_timed_out" },
            { "step_name": "plug", "detail": "prompt_timed_out" },
            { "step_name": "look", "detail": "prompt_cancelled" },
        ]});
        let steps = judge_steps(&study, &kinds, &events, &|_| None);
        let rules = Rules::latest();
        let judged: Vec<(Verdict, Option<Reason>)> =
            steps.iter().map(|s| rules.step_verdict(s.target, s.outcome).map(|j| (j.verdict, j.reason)).unwrap()).collect();
        assert_eq!(
            judged,
            [
                (Verdict::Inconclusive, Some(Reason::ObservationUnanswered)),
                (Verdict::Error, Some(Reason::PromptNotConfirmed)),
                (Verdict::Inconclusive, Some(Reason::ObservationUnanswered)),
            ]
        );
    }

    #[test]
    fn a_persons_choice_reads_as_a_person_observation_judged_by_the_person() {
        let t = test("t1", "A", 1, &[]);
        let doc = json!({ "measurements": [
            { "name": "led", "value": "green", "by": "person", "instrument": null, "incomplete": [], "declared_by": [ { "scope": "person" } ] },
            { "name": "vbus", "value": 4981.0, "by": "person", "instrument": null, "incomplete": [], "declared_by": [ { "scope": "person" } ] },
        ]});
        let got = judge_measurements(&t, &doc);
        assert_eq!((got[0].origin, got[0].value.clone(), got[0].declared_by), (Origin::PersonObservation, MeasuredValue::Choice("green".into()), DeclaredBy::Person));
        assert_eq!((got[1].origin, got[1].value.clone(), got[1].u), (Origin::PersonObservation, MeasuredValue::Number(4981.0), 10.0));
    }

    #[test]
    fn a_utc_date_is_read_from_milliseconds() {
        assert_eq!(utc_date(0), "1970-01-01");
        assert_eq!(utc_date(1_709_164_800_000), "2024-02-29");
        assert_eq!(utc_date(1_709_251_199_999), "2024-02-29");
        assert_eq!(utc_date(1_709_251_200_000), "2024-03-01");
    }

    #[test]
    fn an_instruments_calibration_expires_or_is_undeclared_per_value() {
        let test = test("t1", "A", 1, &[]);
        let doc = json!({"measurements": [
            { "name": "a", "value": 1.0, "by": "bench", "at_utc_ms": 1_709_251_200_000u64, "instrument": { "name": "dmm", "kind": "instrument", "calibration": { "due": "2024-02-29" } }, "incomplete": [], "declared_by": [] },
            { "name": "b", "value": 1.0, "by": "bench", "at_utc_ms": 1_709_164_800_000u64, "instrument": { "name": "dmm", "kind": "instrument", "calibration": { "due": "2024-02-29" } }, "incomplete": [], "declared_by": [] },
            { "name": "c", "value": 1.0, "by": "bench", "at_utc_ms": 1_709_164_800_000u64, "instrument": { "name": "dmm", "kind": "instrument" }, "incomplete": [], "declared_by": [] },
        ]});
        let got = judge_measurements(&test, &doc);
        assert_eq!(got.iter().map(|g| (g.evidence.calibration_expired, g.calibration_undeclared)).collect::<Vec<_>>(), [(true, false), (false, false), (false, true)]);
    }

    #[test]
    fn a_calibration_not_applied_reads_as_undeclared_and_never_expired() {
        let test = test("t1", "A", 1, &[]);
        let doc = json!({"measurements": [
            { "name": "a", "value": 1.0, "by": "bench", "at_utc_ms": 1_709_251_200_000u64, "instrument": { "name": "dmm", "kind": "instrument", "calibration": { "due": "2024-02-29", "applied": false } }, "incomplete": [], "declared_by": [] },
        ]});
        let got = judge_measurements(&test, &doc);
        assert_eq!((got[0].evidence.calibration_expired, got[0].calibration_undeclared), (false, true));
    }

    #[test]
    fn a_measurement_reads_with_its_origin_uncertainty_and_incompleteness() {
        let t = test("t1", "A", 1, &[]);
        let doc = json!({ "measurements": [
            { "name": "vbus", "value": 5012.0, "by": "bench", "at_utc_ms": 0, "instrument": { "name": "dmm", "kind": "instrument", "calibration": { "due": "2099-01-01" } }, "incomplete": [], "declared_by": [ { "scope": "person" } ] },
            { "name": "other", "value": null, "by": "dut", "instrument": null, "incomplete": ["parse_failed"], "declared_by": [ { "scope": "agent" } ] },
        ]});
        let got = judge_measurements(&t, &doc);
        assert_eq!((got[0].origin, got[0].u, got[0].declared_by, got[0].calibration_undeclared), (Origin::Instrument, 10.0, DeclaredBy::Person, false));
        assert_eq!((got[1].origin, got[1].declared_by, got[1].calibration_undeclared), (Origin::DutReport, DeclaredBy::Agent, true));
        assert_eq!(got[1].evidence.incomplete, [Reason::ParseFailed]);
    }

    // ---- the run's own record and log entries (decision 144) ----

    use crate::run_log::RunLog;
    use crate::signing::{Envelope, Sealer};

    fn entries_of(log: &RunLog, kind: &str) -> Vec<Value> {
        log.lines().unwrap().into_iter().map(|l| l["entry"].clone()).filter(|e| e["type"] == kind).collect()
    }

    fn sealed(run: &RunJson, o: &RunOutcome, log: &RunLog, sealer: &Sealer) -> (Option<String>, Value) {
        let rec = json!({ "run_hash": run.hash(), "selection": { "entries": [ { "test_id": "a1", "status": "selected" } ] } });
        let end = crate::test_run_record::Ended { status: o.status, reason: None, results: &o.results, unreached: crate::test_run_record::NOT_RUN, smoke: &o.smoke, call_log: None };
        let hash = crate::test_run_record::seal(sealer, log, &rec, run, &end);
        let env: Envelope = serde_json::from_slice(&std::fs::read(crate::test_runs::record_path(log.data_dir(), &run.run_id)).unwrap()).unwrap();
        let payload = crate::signing::tests::standard_dsse_check(&env, &sealer.key().public_key_hex()).expect("the record verifies");
        (hash, serde_json::from_slice(&payload).unwrap())
    }

    #[test]
    fn a_run_cancelled_on_its_second_bench_is_signed_with_every_test_a_verdict_or_a_reason() {
        let log = RunLog::temp();
        let sealer = Sealer::ephemeral();
        std::fs::create_dir_all(log.run_dir("st")).unwrap();
        std::fs::write(log.run_dir("st").join(crate::signing::RECORD_FILE), b"study record").unwrap();
        let mut run = run_of(&[("A", &["a1"]), ("B", &["b1", "b2"])], vec![test("a1", "A", 1, &[]), test("b1", "B", 1, &[]), test("b2", "B", 1, &[]), test("loose", "A", 1, &[])], false);
        run.resumes = Some("earlier".into());
        let mut env = fake(vec![pass()]);
        env.cancel_after_runs = Some(1);
        let o = go(&run, &mut env);
        assert_eq!(o.status, "cancelled");
        let (hash, st) = sealed(&run, &o, &log, &sealer);
        assert_eq!(st["predicateType"], crate::signing::TEST_RUN_PREDICATE_TYPE);
        assert_eq!(st["subject"][0]["name"], "run.json");
        assert_eq!(st["subject"][0]["digest"]["sha256"], run.hash().as_str());
        let p = &st["predicate"];
        assert_eq!((p["status"].as_str(), p["unit_key"].as_str(), p["resumes"].as_str()), (Some("cancelled"), Some("DUT-1"), Some("earlier")));
        assert_eq!(p["selection"]["entries"][0]["test_id"], "a1");
        let t = |i: usize| (p["tests"][i]["test_id"].as_str().unwrap(), p["tests"][i]["verdict"].as_str(), p["tests"][i]["reason"].as_str());
        assert_eq!([t(0), t(1), t(2), t(3)], [("a1", Some("pass"), None), ("b1", Some("cancelled"), None), ("b2", Some("cancelled"), None), ("loose", None, Some("not_run"))]);
        assert!(p["tests"][1]["note"].as_str().unwrap().contains("cancelled"));
        assert_eq!(p["tests"][0]["study_records"][0]["study_id"], "st");
        assert_eq!(p["tests"][0]["study_records"][0]["record_sha256"], crate::signing::sha256_of_file(&log.run_dir("st").join(crate::signing::RECORD_FILE)).unwrap().as_str());
        assert_eq!(p["tests"][0]["dut_image"], "holds");
        assert!(p["tests"][1]["dut_image"].is_null(), "bench B was never smoke-checked");

        let tests = entries_of(&log, "test");
        assert_eq!(tests.len(), 4);
        assert_eq!(tests[0]["test_id"], "a1");
        assert_eq!((tests[0]["verdict"].as_str(), tests[0]["unit_key"].as_str(), tests[0]["dut_image"].as_str()), (Some("pass"), Some("DUT-1"), Some("holds")));
        assert_eq!(tests[0]["definition_hash"], "0".repeat(64).as_str());
        let whole = entries_of(&log, "test_run");
        assert_eq!(whole.len(), 1);
        assert_eq!((whole[0]["record_sha256"].as_str(), whole[0]["status"].as_str()), (hash.as_deref(), Some("cancelled")));
        assert!(log.verify().unwrap().is_ok());
    }

    #[test]
    fn a_tests_study_record_carries_its_verdict_what_it_ran_against_and_run_json_and_still_verifies() {
        let log = RunLog::temp();
        let dir = log.run_dir("st");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("events.json"), json!({ "steps": [
            { "list": "Steps", "step_name": "look", "prompt_id": 1, "detail": "confirmed", "answered_by": "cli", "answered_scope": "person" },
            { "list": "Steps", "step_name": "send", "detail": "ok" },
        ]}).to_string()).unwrap();
        let plain = Sealer::ephemeral();
        let job = crate::signing::SealJob { sealer: plain.clone(), kind: "host", build: None, submitted_by: None, calls: None };
        plain.seal(&job, &dir, "st", 7).unwrap();
        let before: Value = serde_json::from_slice(&crate::signing::tests::standard_dsse_check(&serde_json::from_slice(&std::fs::read(dir.join(crate::signing::RECORD_FILE)).unwrap()).unwrap(), &plain.key().public_key_hex()).unwrap()).unwrap();
        assert!(before["predicate"].get("test").is_none());

        let run = run_of(&[("A", &["a1"])], vec![test("a1", "A", 1, &[])], false);
        let o = go(&run, &mut fake(vec![pass()]));
        let rec = json!({ "run_hash": run.hash(), "selection": null });
        let end = crate::test_run_record::Ended { status: o.status, reason: None, results: &o.results, unreached: crate::test_run_record::NOT_RUN, smoke: &o.smoke, call_log: None };
        let hash = crate::test_run_record::seal(&plain, &log, &rec, &run, &end);
        let env: Envelope = serde_json::from_slice(&std::fs::read(dir.join(crate::signing::RECORD_FILE)).unwrap()).unwrap();
        let st: Value = serde_json::from_slice(&crate::signing::tests::standard_dsse_check(&env, &plain.key().public_key_hex()).expect("the amended record verifies")).unwrap();
        let names: Vec<&str> = st["subject"].as_array().unwrap().iter().map(|s| s["name"].as_str().unwrap()).collect();
        assert_eq!(names, ["events.json", "run.json"]);
        assert_eq!(st["subject"][1]["digest"]["sha256"], run.hash().as_str());
        // everything else in the Statement is as `seal` wrote it
        let mut rest = st["predicate"].clone();
        let t = rest.as_object_mut().unwrap().remove("test").unwrap();
        assert_eq!(rest, before["predicate"]);
        assert_eq!((t["test_id"].as_str(), t["verdict"]["verdict"].as_str(), t["run"]["verdict"].as_str(), t["unit_key"].as_str()), (Some("a1"), Some("pass"), Some("pass"), Some("DUT-1")));
        assert_eq!(t["images"][0]["piece"], "the DUT");
        assert_eq!(t["images"][0]["status"], "holds");
        assert!(t["identities"].as_array().unwrap().iter().any(|i| i["piece"] == "the DUT" && i["hardware_id"] == "DUT-1"));
        assert!(t["instruments"].is_array());
        assert_eq!(t["prompts"], json!([{ "list": "Steps", "step": "look", "prompt_id": 1, "detail": "confirmed", "answered_by": "cli", "answered_scope": "person" }]));
        // the run's record names the amended record's hash
        let run_rec: Envelope = serde_json::from_slice(&std::fs::read(crate::test_runs::record_path(log.data_dir(), "r1")).unwrap()).unwrap();
        let rp: Value = serde_json::from_slice(&crate::signing::tests::standard_dsse_check(&run_rec, &plain.key().public_key_hex()).unwrap()).unwrap();
        assert_eq!(rp["predicate"]["tests"][0]["study_records"][0]["record_sha256"].as_str(), Some(crate::signing::sha256_of_file(&dir.join(crate::signing::RECORD_FILE)).unwrap().as_str()));
        assert!(hash.is_some());
        // the chain binds the amended file's hash, which is not the one the study's `run` entry
        // (when it has one) would have held for the sealed record (decision 146)
        let on_disk = crate::signing::sha256_of_file(&dir.join(crate::signing::RECORD_FILE)).unwrap();
        let whole = entries_of(&log, "test_run");
        assert_eq!(whole[0]["study_records"][0]["study_id"], "st");
        assert_eq!(whole[0]["study_records"][0]["record_sha256"].as_str(), Some(on_disk.as_str()));
        assert_eq!(log.bound_record_sha256("st").unwrap().as_deref(), Some(on_disk.as_str()));
        assert!(log.verify().unwrap().is_ok());
    }

    #[test]
    fn a_test_the_run_never_reached_names_not_run_and_a_resumes_less_run_omits_the_key() {
        let log = RunLog::temp();
        let sealer = Sealer::ephemeral();
        let run = run_of(&[("A", &["a1"])], vec![test("a1", "A", 1, &[]), test("orphan", "A", 1, &[])], false);
        let mut o = go(&run, &mut fake(vec![pass()]));
        o.results.truncate(1);
        let (_, st) = sealed(&run, &o, &log, &sealer);
        assert!(st["predicate"].get("resumes").is_none());
        assert_eq!((st["predicate"]["tests"][1]["verdict"].is_null(), st["predicate"]["tests"][1]["reason"].as_str()), (true, Some("not_run")));
        assert!(entries_of(&log, "test")[1]["verdict"].is_null());
    }

    #[test]
    fn every_finished_test_is_checkpointed_as_it_ends() {
        let run = run_of(&[("A", &["a1", "a2"])], vec![test("a1", "A", 1, &[]), test("a2", "A", 1, &[])], false);
        let mut env = fake(vec![pass(), pass()]);
        go(&run, &mut env);
        assert_eq!(env.checkpoints, vec![1, 2]);
    }

    #[test]
    fn a_run_core_lost_is_sealed_interrupted_once_keeping_the_verdicts_it_had_finished() {
        let log = RunLog::temp();
        let sealer = Sealer::ephemeral();
        let run = run_of(&[("A", &["a1", "a2", "a3"])], vec![test("a1", "A", 1, &[]), test("a2", "A", 1, &[]), test("a3", "A", 1, &[])], false);
        let dir = log.data_dir().join("test-runs");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("r1.json"), json!({ "run_id": "r1", "run_hash": run.hash(), "run": run, "selection": null }).to_string()).unwrap();
        std::fs::write(dir.join("r1.started.json"), "{}").unwrap();
        // The executor finished a1 and was in a2 when Core went down.
        let done = go(&run, &mut fake(vec![pass(), pass(), pass()]));
        let mut smoke = done.smoke.clone();
        smoke.iter_mut().for_each(|s| s.unit = Some("DUT-1".into()));
        crate::test_runs::write_checkpoint(log.data_dir(), "r1", &done.results[..1], &smoke).unwrap();
        crate::test_run_record::seal_interrupted(&sealer, &log);
        crate::test_run_record::seal_interrupted(&sealer, &log);
        let env: Envelope = serde_json::from_slice(&std::fs::read(crate::test_runs::record_path(log.data_dir(), "r1")).unwrap()).unwrap();
        let st: Value = serde_json::from_slice(&crate::signing::tests::standard_dsse_check(&env, &sealer.key().public_key_hex()).unwrap()).unwrap();
        let p = &st["predicate"];
        assert_eq!((p["status"].as_str(), p["unit_key"].as_str()), (Some("interrupted"), Some("DUT-1")));
        let t = |i: usize| (p["tests"][i]["test_id"].as_str().unwrap(), p["tests"][i]["verdict"].as_str(), p["tests"][i]["reason"].as_str());
        assert_eq!([t(0), t(1), t(2)], [("a1", Some("pass"), None), ("a2", None, Some("interrupted")), ("a3", None, Some("interrupted"))]);
        assert!(st["subject"].as_array().unwrap().iter().any(|s| s["name"] == "result.json"));
        let kept: Value = serde_json::from_slice(&std::fs::read(crate::test_runs::result_path(log.data_dir(), "r1")).unwrap()).unwrap();
        assert_eq!((kept["status"].as_str(), kept["results"].as_array().map(Vec::len)), (Some("interrupted"), Some(1)));
        assert_eq!(entries_of(&log, "test_run").len(), 1);
        assert_eq!(entries_of(&log, "test")[0]["verdict"], "pass");
    }

    #[test]
    fn a_run_core_lost_before_any_test_ended_is_sealed_interrupted_with_none() {
        let log = RunLog::temp();
        let sealer = Sealer::ephemeral();
        let run = run_of(&[("A", &["a1"])], vec![test("a1", "A", 1, &[])], false);
        let dir = log.data_dir().join("test-runs");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("r1.json"), json!({ "run_id": "r1", "run_hash": run.hash(), "run": run, "selection": null }).to_string()).unwrap();
        std::fs::write(dir.join("r1.started.json"), "{}").unwrap();
        crate::test_run_record::seal_interrupted(&sealer, &log);
        let env: Envelope = serde_json::from_slice(&std::fs::read(crate::test_runs::record_path(log.data_dir(), "r1")).unwrap()).unwrap();
        let st: Value = serde_json::from_slice(&crate::signing::tests::standard_dsse_check(&env, &sealer.key().public_key_hex()).unwrap()).unwrap();
        assert_eq!((st["predicate"]["status"].as_str(), st["predicate"]["tests"][0]["reason"].as_str()), (Some("interrupted"), Some("interrupted")));
        assert!(st["predicate"]["tests"][0]["verdict"].is_null());
    }
}
