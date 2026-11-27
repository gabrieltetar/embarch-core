//! A Test run's own signed record and its run-log entries (decision 144): the run as it ended, a
//! finish, a cancel or a deadline, and the run Core's restart left without an end (decision 153).
//!
//! The record is `test-runs/{run_id}.test-run.dsse.json`, a DSSE envelope over an in-toto Statement
//! of [`crate::signing::TEST_RUN_PREDICATE_TYPE`]. It vouches for `run.json` by its canonical hash
//! and names each selected test with the hash of each study record it made and its verdict, or why
//! it has none. The per-test study record is not changed by this (that is `tasks/core/170`).

use serde_json::{json, Value};

use crate::run_log::RunLog;
use crate::signing::{sha256_of_file, Sealer, RECORD_FILE};
use crate::smoke_check::SmokeFindings;
use crate::test_executor::TestResult;
use embarch_study_designer::hil_expand::{RunJson, RunTest};

/// Why a test the run never reached has no verdict, and why a run Core's restart cut short has none.
pub const NOT_RUN: &str = "not_run";
const INTERRUPTED: &str = "interrupted";

/// What the executor knows when the run ends.
pub struct Ended<'a> {
    pub status: &'a str,
    pub reason: Option<&'a str>,
    pub results: &'a [TestResult],
    /// What a test with no result says: `not_run`, or `interrupted` for a run Core lost.
    pub unreached: &'a str,
    pub smoke: &'a [SmokeFindings],
    /// The call log's range over the run, in the shape a study record carries it.
    pub call_log: Option<Value>,
}

/// The DUT unit the run kept: the first bench's hardware ID.
fn unit_key(smoke: &[SmokeFindings]) -> Option<&str> {
    smoke.iter().find_map(|s| s.unit.as_deref())
}

/// The DUT's image status on `bench`, as the smoke check read it; `None` when the bench was not checked.
fn dut_image(smoke: &[SmokeFindings], bench: &str) -> Option<String> {
    let found = smoke.iter().find(|s| s.bench == bench)?.images.iter().find(|i| i.piece == "the DUT")?;
    serde_json::to_value(&found.status).ok()?["status"].as_str().map(str::to_string)
}

/// One judged verdict as a record states it: `{verdict, reason, qualifiers}`.
fn judged_json(j: &embarch_study_designer::verdicts::Judged) -> Value {
    json!({ "verdict": j.verdict, "reason": j.reason, "qualifiers": j.qualifiers })
}

/// The prompts a study asked, from its `events.json`: each step's id, who answered it and with which
/// token's scope (decision 138). Empty when the study left no events or asked none.
fn prompt_scopes(dir: &std::path::Path) -> Vec<Value> {
    let events: Option<Value> = std::fs::read(dir.join("events.json")).ok().and_then(|b| serde_json::from_slice(&b).ok());
    events
        .and_then(|e| e["steps"].as_array().cloned())
        .into_iter()
        .flatten()
        .filter(|s| s["prompt_id"].is_number())
        .map(|s| json!({ "list": s["list"], "step": s["step_name"], "prompt_id": s["prompt_id"], "detail": s["detail"], "answered_by": s["answered_by"], "answered_scope": s["answered_scope"] }))
        .collect()
}

/// What a study's record says of the test it was run for (decision 145): the run's verdict and the
/// test's, and what the bench's smoke check found it ran against. `index` is the study's place among
/// the test's runs. A bench never smoke-checked leaves those fields null.
fn test_part(run_id: &str, t: &RunTest, result: &TestResult, index: usize, smoke: Option<&SmokeFindings>, prompts: Vec<Value>) -> Value {
    json!({
        "run_id": run_id, "test_id": t.test_id, "bench": t.bench.name, "definition_hash": t.definition_hash,
        "run_index": index + 1, "run_count": t.run_count.max(1),
        "run": result.runs.get(index).map(judged_json), "verdict": judged_json(&result.judged), "note": result.note,
        "unit_key": smoke.and_then(|s| s.unit.clone()),
        "images": smoke.map(|s| &s.images), "identities": smoke.map(|s| &s.identities), "instruments": smoke.map(|s| &s.instruments),
        "prompts": prompts,
    })
}

/// Amends each study record the run's tests made, before the run's own record hashes them. A study
/// that left no record, or one that cannot be amended, is logged and left as it was.
fn amend_studies(sealer: &Sealer, log: &RunLog, run: &RunJson, run_hash: &str, end: &Ended) {
    for t in &run.tests {
        let Some(result) = end.results.iter().find(|r| r.test_id == t.test_id) else { continue };
        let smoke = end.smoke.iter().find(|s| s.bench == t.bench.name);
        for (i, id) in result.study_ids.iter().enumerate() {
            let dir = log.run_dir(id);
            let part = test_part(&run.run_id, t, result, i, smoke, prompt_scopes(&dir));
            if let Err(e) = sealer.amend_for_test(&dir, part, run_hash) {
                tracing::error!(run_id = run.run_id, study_id = id, "could not amend the study's record with its test: {e:#}");
            }
        }
    }
}

fn verdict_of(r: &TestResult) -> String {
    serde_json::to_value(r.judged.verdict).ok().and_then(|v| v.as_str().map(str::to_string)).unwrap_or_default()
}

/// Signs the run's record and enters its tests and the run in the log. `record` is what the prepare
/// kept (`run_hash`, `selection`). The log entries are made even when signing fails, with a null
/// `record_sha256`, so the run is never missing from the log. Returns the record's hash.
pub fn seal(sealer: &Sealer, log: &RunLog, record: &Value, run: &RunJson, end: &Ended) -> Option<String> {
    let run_hash = record["run_hash"].as_str().map(str::to_string).unwrap_or_else(|| run.hash());
    let unit = unit_key(end.smoke);
    amend_studies(sealer, log, run, &run_hash, end);
    let mut tests = Vec::new();
    let mut study_records: Vec<Value> = Vec::new();
    for t in &run.tests {
        let result = end.results.iter().find(|r| r.test_id == t.test_id);
        let studies: Vec<Value> = result
            .map(|r| r.study_ids.iter().map(|id| json!({ "study_id": id, "record_sha256": sha256_of_file(&log.run_dir(id).join(RECORD_FILE)).ok() })).collect())
            .unwrap_or_default();
        study_records.extend(studies.iter().cloned());
        let verdict = result.map(verdict_of);
        let reason = match result {
            Some(r) => r.judged.reason.and_then(|x| serde_json::to_value(x).ok()).unwrap_or(Value::Null),
            None => json!(end.unreached),
        };
        tests.push(json!({
            "test_id": t.test_id, "bench": t.bench.name, "definition_hash": t.definition_hash, "requirements": t.requirements,
            "verdict": verdict, "reason": reason, "note": result.and_then(|r| r.note.clone()),
            "dut_image": dut_image(end.smoke, &t.bench.name), "study_records": studies,
        }));
    }
    let ended = crate::study::current_utc_ms();
    let mut predicate = json!({
        "schema": 1, "run_id": run.run_id, "status": end.status, "reason": end.reason, "ended_utc_ms": ended,
        "unit_key": unit, "selection": record["selection"], "tests": tests,
        "observed": { "core": { "version": env!("CARGO_PKG_VERSION") }, "mode": crate::token_store::mode(), "call_log": end.call_log },
    });
    if let Some(r) = &run.resumes {
        predicate["resumes"] = json!(r);
    }
    let path = crate::test_runs::record_path(log.data_dir(), &run.run_id);
    let mut subjects = vec![("run.json".to_string(), run_hash)];
    if let Ok(h) = sha256_of_file(&crate::test_runs::result_path(log.data_dir(), &run.run_id)) {
        subjects.push(("result.json".to_string(), h));
    }
    let hash = match sealer.seal_test_run(&path, &subjects, predicate).and_then(|()| sha256_of_file(&path)) {
        Ok(h) => Some(h),
        Err(e) => {
            tracing::error!(run_id = run.run_id, "could not sign the Test run's record: {e:#}");
            None
        }
    };
    for t in &run.tests {
        let verdict = end.results.iter().find(|r| r.test_id == t.test_id).map(verdict_of);
        if let Err(e) = log.test_entry(&run.run_id, &t.test_id, &t.definition_hash, &t.requirements, unit, dut_image(end.smoke, &t.bench.name).as_deref(), verdict.as_deref()) {
            tracing::error!(run_id = run.run_id, test = t.test_id, "could not enter the test in the run log: {e:#}");
        }
    }
    if let Err(e) = log.test_run_entry(&run.run_id, end.status, hash.as_deref(), &study_records) {
        tracing::error!(run_id = run.run_id, "could not enter the Test run in the run log: {e:#}");
    }
    hash
}

/// Seals each run Core started and never saw end: the executor died with Core, so the test in
/// flight and every later one is `interrupted` with no verdict, and each test the executor had
/// finished keeps what it signed, read from the checkpoint it left (decision 153). The kept results
/// go to `result.json` first, so the run reads back as `interrupted` with them and the record hashes
/// the file. Run at start, before the first go.
pub fn seal_interrupted(sealer: &Sealer, log: &RunLog) {
    for run_id in crate::test_runs::started_run_ids(log.data_dir()) {
        if crate::test_runs::record_path(log.data_dir(), &run_id).exists() {
            continue;
        }
        // A result the executor wrote is its own end; one this function wrote before dying is redone.
        let kept: Option<Value> = std::fs::read(crate::test_runs::result_path(log.data_dir(), &run_id)).ok().and_then(|b| serde_json::from_slice(&b).ok());
        if kept.is_some_and(|k| k["status"] != INTERRUPTED) {
            continue;
        }
        let Ok(rec) = crate::test_runs::read_record(log.data_dir(), &run_id) else { continue };
        let Ok(run) = serde_json::from_value::<RunJson>(rec["run"].clone()) else { continue };
        let (results, smoke) = crate::test_runs::read_checkpoint(log.data_dir(), &run_id);
        let body = json!({
            "run_id": run_id, "status": INTERRUPTED, "ended_utc_ms": crate::study::current_utc_ms(),
            "results": results.iter().map(TestResult::to_json).collect::<Vec<_>>(), "smoke_checks": smoke,
        });
        if let Err(e) = std::fs::write(crate::test_runs::result_path(log.data_dir(), &run_id), body.to_string()) {
            tracing::error!(run_id, "could not keep the interrupted run's result: {e}");
        }
        let end = Ended { status: INTERRUPTED, reason: Some("Core restarted under the run"), results: &results, unreached: INTERRUPTED, smoke: &smoke, call_log: None };
        seal(sealer, log, &rec, &run, &end);
    }
}
