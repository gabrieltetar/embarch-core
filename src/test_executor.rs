//! The executor of a started Test run (decision 136): bench by bench in `run.json`'s order, test by
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
use embarch_study_designer::host_study::{HostAction, HostStep, HostStudy, PromptKind};
use embarch_study_designer::verdicts::{Counts, Evidence, Judged, Reason, Rules, StepOutcome, Target, Verdict};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::api::AppState;
use crate::host_study::detail;
use crate::measurements::SignalOrigin;
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

/// What the policy needs from a bench.
pub trait Env {
    /// The run was cancelled (the route, or a study's own cancel).
    fn cancelled(&self) -> bool;
    /// Brings the DUT and the bench boards to the test's starting state; `Err` names what failed.
    fn precondition(&mut self, bench: &RunBench, test: &RunTest) -> Result<(), String>;
    /// Runs the test's study once, judged steps and measurements back.
    fn run_study(&mut self, test: &RunTest) -> StudyRun;
}

/// One test's result in the run.
#[derive(Debug, Clone, PartialEq)]
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
}

/// Every run's progress, by run id; each is shared with the executor that updates it.
pub type ProgressBook = Arc<Mutex<BTreeMap<String, Arc<Mutex<Progress>>>>>;

/// How the run ended, with every test's result in `run.json`'s order.
#[derive(Debug, Clone, PartialEq)]
pub struct RunOutcome {
    /// `completed`, `cancelled` or `deadline`.
    pub status: &'static str,
    pub results: Vec<TestResult>,
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
    Duration::from_millis(bound.saturating_mul(DEADLINE_FACTOR).saturating_add(TEST_ALLOWANCE_MS.saturating_mul(run.tests.len() as u64)))
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
    let publish = |results: &[TestResult], bench: Option<&str>, test: Option<&str>| {
        let mut p = progress.lock().unwrap();
        p.state = "running";
        p.bench = bench.map(str::to_string);
        p.test = test.map(str::to_string);
        p.results = results.to_vec();
    };
    for bench in &run.benches {
        let mut bench_error = false;
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
            } else if after_fail {
                skipped(test, Reason::StoppedAfterFail, "stop_on_fail: an earlier test failed")
            } else if bench_error {
                skipped(test, Reason::StoppedByError, "an earlier test on this bench ended in an error")
            } else if let Some(missing) = test.requires.iter().find(|r| !results.iter().any(|x| &x.test_id == *r && x.judged.verdict == Verdict::Pass)) {
                skipped(test, Reason::RequiresNotMet, &format!("it requires {missing}, which did not pass"))
            } else {
                publish(&results, Some(&bench.name), Some(&test.test_id));
                run_test(test, bench, env, rules, started, limit, &mut ended)
            };
            match result.judged.verdict {
                Verdict::Error => bench_error = true,
                Verdict::Fail if run.options.stop_on_fail => after_fail = true,
                _ => {}
            }
            results.push(result);
            publish(&results, Some(&bench.name), None);
        }
    }
    let status = ended.unwrap_or("completed");
    {
        let mut p = progress.lock().unwrap();
        p.state = status;
        p.bench = None;
        p.test = None;
        p.results = results.clone();
    }
    RunOutcome { status, results }
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

/// The steps of an `events.json`, as the verdict rules read them.
pub fn judge_steps(study: &HostStudy, events: &Value, origin: &dyn Fn(&str) -> Option<SignalOrigin>) -> Vec<JudgeStep> {
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
            let observation = matches!(action, Some(HostAction::Prompt { kind: Some(PromptKind::Observation), .. }));
            let outcome = outcome_of(detail, observation)?;
            let target = match action {
                Some(HostAction::Exchange { signal, .. }) => target_of(origin(signal)),
                Some(HostAction::Instrument { .. }) => Target::Bench,
                _ => Target::Dut,
            };
            Some(JudgeStep { step: name.to_string(), target, outcome })
        })
        .collect()
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
                (Some("dut"), _) => Origin::DutReport,
                (_, Some("instrument")) => Origin::Instrument,
                // `bench_image` is `unverified` for every board until Core can read an image back.
                _ => Origin::BenchBoard { firmware: FirmwareState::Unverified },
            };
            let incomplete: Vec<Reason> = m["incomplete"].as_array().into_iter().flatten().filter_map(|i| serde_json::from_value(i.clone()).ok()).collect();
            let by_person = m["declared_by"].as_array().is_some_and(|d| !d.is_empty() && d.iter().all(|x| x["scope"] == "person"));
            Some(MeasurementRecord {
                measurement: name.to_string(),
                value: MeasuredValue::Number(m["value"].as_f64().unwrap_or(0.0)),
                u: source.map_or(0.0, |s| s.u),
                evidence: Evidence { incomplete, calibration_expired: false },
                origin,
                declared_by: if by_person { DeclaredBy::Person } else { DeclaredBy::Agent },
                calibration_undeclared: source.is_none(),
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
    /// The run was cancelled under a study: set from the study's own cancel flag.
    study_cancelled: bool,
}

impl CoreEnv {
    pub fn new(state: AppState, handle: tokio::runtime::Handle, run_id: String, studies: BTreeMap<String, HostStudy>) -> Self {
        CoreEnv { state, handle, run_id, studies, study_cancelled: false }
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

impl Env for CoreEnv {
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

    fn run_study(&mut self, test: &RunTest) -> StudyRun {
        let abort = |why: String| StudyRun { steps: vec![], measurements: vec![], cancelled: false, aborted: Some(why), study_id: None };
        let Some(study) = self.studies.get(&test.test_id).cloned() else {
            return abort(format!("Core kept no study for {} at the prepare", test.test_id));
        };
        let ran = match crate::host_study::run_study_blocking(&self.state, &self.handle, &study, Scope::Person) {
            Ok(r) => r,
            Err(why) => return abort(why),
        };
        let read = |f: &str| std::fs::read(ran.dir.join(f)).ok().and_then(|b| serde_json::from_slice::<Value>(&b).ok());
        let origin = &crate::measurements::topology_signal_origin;
        let steps = read("events.json").map(|e| judge_steps(&study, &e, origin)).unwrap_or_default();
        let measurements = read("measurements.json").map(|m| judge_measurements(test, &m)).unwrap_or_default();
        self.study_cancelled = ran.cancelled;
        StudyRun { steps, measurements, cancelled: ran.cancelled, aborted: None, study_id: Some(ran.study_id) }
    }
}

/// Starts the executor for a run the go just started, on its own blocking thread, and ends the
/// run through [`crate::test_runs::end_run`] when it is done (`None` scope: Core ended it).
pub fn spawn(state: AppState, run_id: String, run: RunJson, studies: BTreeMap<String, HostStudy>) {
    let handle = tokio::runtime::Handle::current();
    let progress = Arc::new(Mutex::new(Progress { state: "running", ..Default::default() }));
    state.test_progress.lock().unwrap().insert(run_id.clone(), progress.clone());
    tokio::task::spawn_blocking(move || {
        let mut env = CoreEnv::new(state.clone(), handle, run_id.clone(), studies);
        let outcome = drive(&run, &mut env, &progress, &Rules::latest(), deadline(&run));
        let body = json!({
            "run_id": run_id, "status": outcome.status, "ended_utc_ms": crate::study::current_utc_ms(),
            "results": outcome.results.iter().map(TestResult::to_json).collect::<Vec<_>>(),
        });
        if let Err(e) = std::fs::write(crate::test_runs::result_path(state.run_log.data_dir(), &run_id), body.to_string()) {
            tracing::error!(run_id, "could not keep the run's result: {e}");
        }
        let reason = (outcome.status == "deadline").then_some("the run passed its deadline");
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
            "benches": benches.iter().map(|(n, t)| json!({ "name": n, "tests": t, "boards": [] })).collect::<Vec<_>>(),
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
    }

    fn fake(runs: Vec<StudyRun>) -> Fake {
        Fake { runs: runs.into(), precondition_fails: vec![], cancel_after_runs: None, ran: vec![], prepared: vec![] }
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
        let got = judge_steps(&study, &events, &origin);
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
    fn a_measurement_reads_with_its_origin_uncertainty_and_incompleteness() {
        let t = test("t1", "A", 1, &[]);
        let doc = json!({ "measurements": [
            { "name": "vbus", "value": 5012.0, "by": "bench", "instrument": { "name": "dmm", "kind": "instrument" }, "incomplete": [], "declared_by": [ { "scope": "person" } ] },
            { "name": "other", "value": null, "by": "dut", "instrument": null, "incomplete": ["parse_failed"], "declared_by": [ { "scope": "agent" } ] },
        ]});
        let got = judge_measurements(&t, &doc);
        assert_eq!((got[0].origin, got[0].u, got[0].declared_by, got[0].calibration_undeclared), (Origin::Instrument, 10.0, DeclaredBy::Person, false));
        assert_eq!((got[1].origin, got[1].declared_by, got[1].calibration_undeclared), (Origin::DutReport, DeclaredBy::Agent, true));
        assert_eq!(got[1].evidence.incomplete, [Reason::ParseFailed]);
    }
}
