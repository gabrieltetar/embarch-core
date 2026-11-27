//! Core's side of publish for a Test run (decision 148): `POST /test-runs/{run_id}/report-data`
//! and `POST /test-runs/{run_id}/publish`.
//!
//! Everything here is read from what Core holds: the run's prepared record, its signed record and
//! result, the study records the run amended, and the run log. Nothing comes from embarch-api but
//! the list of the files it made. Every verdict is `embarch-study-designer`'s roll-up over the
//! per-test verdicts the executor judged and signed; this module decides none of them. The
//! signing, the manifest and the log segments are `publish.rs`'s, shared with a Study's publish.

use axum::{
    extract::{Path as UrlPath, State},
    http::StatusCode,
    response::Response,
    Json,
};
use embarch_study_designer::hil_expand::RunJson;
use embarch_study_designer::verdicts::{Judged, Qualifier, Reason, Rules, Verdict};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use crate::api::AppState;
use crate::publish::{
    attestation, call_log_claim, call_log_segment_of, check_api_names, check_files, core_run_files, internal, manifest_names, safe_name, served, sha256_hex, sign_manifest, statement_bytes,
    statement_of, write_file, Failure, Manifest, PublishRequest, PUBLISH_FILES, REPORT_DATA_ENVELOPE_FILE, REPORT_DATA_FILE, REPORT_DATA_PREDICATE_TYPE,
};
use crate::signing::sha256_of_file;
use crate::token_store::Scope;

/// The statement every Test report carries, beside the verdicts (`notes/hil/03-test.md`, "The report").
pub const FIXED_STATEMENT: &str =
    "Pre-compliance evidence. Not a certification by any standards organisation or certification programme, and not an accredited test report.";
/// What the data says where no atlas was joined (`notes/hil/04-premade-suites.md`).
const NO_ATLAS: &str = "no atlas was joined: the standard's own requirement count is absent";

/// The zip-relative names of the Test run's own files.
const RUN_FILE: &str = "run.json";
const RESULT_FILE: &str = "result.json";
const RECORD_NAME: &str = "test-run.dsse.json";
/// A study run's files sit under this folder of the zip, by study id.
const STUDIES_DIR: &str = "studies";

fn publish_dir(state: &AppState, run_id: &str, revision: u64) -> PathBuf {
    state.run_log.data_dir().join("test-runs").join(format!("{run_id}.publish")).join(format!("r{revision}"))
}

fn is_entry(l: &Value, kind: &str, run_id: &str) -> bool {
    l["entry"]["type"] == kind && l["entry"]["run_id"] == run_id
}

/// The number of publishes of the run, plus one.
fn revision_of(lines: &[Value], run_id: &str) -> u64 {
    lines.iter().filter(|l| is_entry(l, "published", run_id)).count() as u64 + 1
}

fn digest_of(statement: &Value, name: &str) -> Option<String> {
    statement["subject"].as_array()?.iter().find(|s| s["name"] == name).and_then(|s| s["digest"]["sha256"].as_str()).map(str::to_string)
}

/// A Test run that has ended and whose records are all still there.
struct Held {
    statement: Value,
    entry: Value,
    record_path: PathBuf,
    run_bytes: Vec<u8>,
    result: Option<Value>,
    result_path: Option<PathBuf>,
    study_ids: Vec<String>,
}

fn conflict(msg: String) -> Failure {
    (StatusCode::CONFLICT, msg)
}

/// The run, refused `404` when Core never prepared it and `409` when it has not ended, has no
/// signed record, or a file its record binds is gone or is not the one the chain names. A study
/// run the retention sweep took is `409` too: handing a Test run's files back is not built.
fn ended_run(state: &AppState, lines: &[Value], run_id: &str) -> Result<Held, Failure> {
    if !crate::test_runs::is_run_id(run_id) {
        return Err((StatusCode::NOT_FOUND, format!("Core has no prepared run {run_id}")));
    }
    let store = state.run_log.data_dir();
    let prepared = crate::test_runs::read_record(store, run_id)?;
    let entry = lines
        .iter()
        .rev()
        .find(|l| is_entry(l, "test_run", run_id))
        .map(|l| l["entry"].clone())
        .ok_or_else(|| conflict(format!("run {run_id} has not ended, so there is nothing to publish")))?;
    let record_path = crate::test_runs::record_path(store, run_id);
    let logged = entry["record_sha256"].as_str();
    if logged.is_none() || !record_path.is_file() {
        return Err(conflict(format!("run {run_id} has no signed record (signing failed when it ended), so Core will not publish it")));
    }
    if sha256_of_file(&record_path).map_err(internal)?.as_str() != logged.unwrap_or_default() {
        return Err(conflict(format!("run {run_id}'s record is not the file the run log bound")));
    }
    let statement = statement_of(&record_path)?;
    let run: RunJson = serde_json::from_value(prepared["run"].clone()).map_err(internal)?;
    if digest_of(&statement, RUN_FILE).as_deref() != Some(run.hash().as_str()) {
        return Err(internal(format!("run {run_id}'s prepared run.json is not the one its record names")));
    }
    let result_path = crate::test_runs::result_path(store, run_id);
    let (result, result_path) = match digest_of(&statement, RESULT_FILE) {
        None => (None, None),
        Some(want) => {
            let ok = result_path.is_file() && sha256_of_file(&result_path).map_err(internal)? == want;
            if !ok {
                return Err(conflict(format!("run {run_id}'s result.json is gone or is not the file its record names")));
            }
            let bytes = std::fs::read(&result_path).map_err(internal)?;
            (Some(serde_json::from_slice(&bytes).map_err(internal)?), Some(result_path))
        }
    };
    let mut study_ids = Vec::new();
    for s in entry["study_records"].as_array().into_iter().flatten() {
        let Some(id) = s["study_id"].as_str() else { continue };
        if lines.iter().any(|l| l["entry"]["type"] == "discarded" && l["entry"]["study_id"] == id) {
            return Err(conflict(format!("study {id}'s captures were discarded, so run {run_id} cannot be published whole")));
        }
        if let Some(want) = s["record_sha256"].as_str() {
            let file = state.run_log.run_dir(id).join(crate::signing::RECORD_FILE);
            if !file.is_file() || sha256_of_file(&file).map_err(internal)? != want {
                return Err(conflict(format!("study {id}'s files are gone (the retention sweep took them) or its record changed: run {run_id} cannot be published")));
            }
        }
        study_ids.push(id.to_string());
    }
    Ok(Held { statement, entry, record_path, run_bytes: run.canonical().into_bytes(), result, result_path, study_ids })
}

/// What an executor result says of a test, `None` when the run left none (a test it never reached).
fn judged_of(result: Option<&Value>) -> Option<Judged> {
    let r = result?;
    Some(Judged {
        verdict: serde_json::from_value::<Verdict>(r["verdict"].clone()).ok()?,
        reason: serde_json::from_value::<Option<Reason>>(r["reason"].clone()).ok().flatten(),
        qualifiers: serde_json::from_value(r["qualifiers"].clone()).unwrap_or_default(),
    })
}

/// A test the run never reached is a gap by definition: the run stopped before it. The roll-up
/// reads it as skipped for `stopped_by_error` (assumed, decision 148); the data names the real
/// reason, `not_run` or `interrupted`.
fn unreached() -> Judged {
    Judged { verdict: Verdict::Skipped, reason: Some(Reason::StoppedByError), qualifiers: vec![] }
}

/// A person's or the agent's verdict for a test, as the roll-up reads it.
fn manual_judged(verdict: &str, relayed: bool) -> Judged {
    let v = match verdict {
        "pass" => Verdict::Pass,
        "fail" => Verdict::Fail,
        _ => Verdict::Inconclusive,
    };
    let q = if relayed { Qualifier::Relayed } else { Qualifier::RestsOnPersonObservation };
    Judged { verdict: v, reason: None, qualifiers: vec![q] }
}

struct TestRow {
    test_id: String,
    definition_hash: String,
    requirements: Vec<String>,
    judged: Judged,
    reached: bool,
    reason: Value,
}

struct ManualRow {
    judged: Judged,
    requirements: Vec<String>,
    matched: bool,
    counted: bool,
    relayed: bool,
}

fn strings(v: &Value) -> Vec<String> {
    v.as_array().into_iter().flatten().filter_map(|x| x.as_str().map(str::to_string)).collect()
}

fn latest_entry(lines: &[Value], kind: &str, unit_key: Option<&str>) -> Value {
    lines
        .iter()
        .rev()
        .find(|l| l["entry"]["type"] == kind && unit_key.is_none_or(|k| l["entry"]["unit_key"] == k))
        .map(|l| l["entry"].clone())
        .unwrap_or(Value::Null)
}

/// Every manual verdict entered for the unit, with whether it counts. A person's counts when it
/// names a test of this run and is the latest of its `(test, definition)`; the agent's is listed
/// apart and never counts (decision 147).
fn manual_rows(lines: &[Value], unit_key: Option<&str>, tests: &[TestRow]) -> (Vec<ManualRow>, Vec<Value>) {
    let Some(unit) = unit_key else { return (vec![], vec![]) };
    let mut latest: BTreeMap<(String, String), usize> = BTreeMap::new();
    for (i, l) in lines.iter().enumerate() {
        let e = &l["entry"];
        if e["type"] == "manual_verdict" && e["unit_key"] == unit && e["relayed"] != true {
            latest.insert((e["test_id"].as_str().unwrap_or("").to_string(), e["definition_hash"].as_str().unwrap_or("").to_string()), i);
        }
    }
    let (mut rows, mut listed) = (Vec::new(), Vec::new());
    for (i, l) in lines.iter().enumerate() {
        let e = &l["entry"];
        if e["type"] != "manual_verdict" || e["unit_key"] != unit {
            continue;
        }
        let (id, hash) = (e["test_id"].as_str().unwrap_or(""), e["definition_hash"].as_str().unwrap_or(""));
        let test = tests.iter().find(|t| t.test_id == id && t.definition_hash == hash);
        let relayed = e["relayed"] == true;
        let counted = !relayed && test.is_some() && latest.get(&(id.to_string(), hash.to_string())) == Some(&i);
        let judged = manual_judged(e["verdict"].as_str().unwrap_or(""), relayed);
        listed.push(json!({
            "log_index": i, "test_id": id, "definition_hash": hash, "verdict": e["verdict"], "reason": e["reason"], "note": e["note"],
            "dut_image": e["dut_image"], "scope": e["scope"], "relayed": relayed, "counted": counted,
            "in_run": test.is_some(), "at_utc_ms": e["at_utc_ms"],
            "attribution": if relayed { "manual verdict, relayed by the agent" } else { "manual verdict, the person's" },
        }));
        rows.push(ManualRow { judged, requirements: test.map(|t| t.requirements.clone()).unwrap_or_default(), matched: test.is_some(), counted, relayed });
    }
    (rows, listed)
}

/// Every `test` entry on the unit, failed ones included, grouped by test, requirement IDs, DUT
/// image and definition hash, so a rename or a rebuild hides none (`notes/hil/06`, "Publish").
fn runs_on_unit(lines: &[Value], unit_key: Option<&str>) -> Vec<Value> {
    let Some(unit) = unit_key else { return vec![] };
    type Key = (String, String, String, String);
    let mut groups: BTreeMap<Key, Vec<Value>> = BTreeMap::new();
    for (i, l) in lines.iter().enumerate() {
        let e = &l["entry"];
        if e["type"] != "test" || e["unit_key"] != unit {
            continue;
        }
        let key = (
            e["test_id"].as_str().unwrap_or("").to_string(),
            strings(&e["requirements"]).join(","),
            e["dut_image"].as_str().unwrap_or("").to_string(),
            e["definition_hash"].as_str().unwrap_or("").to_string(),
        );
        groups.entry(key).or_default().push(json!({ "run_id": e["run_id"], "verdict": e["verdict"], "log_index": i }));
    }
    groups
        .into_iter()
        .map(|((test_id, reqs, image, hash), runs)| {
            json!({
                "test_id": test_id, "requirements": reqs.split(',').filter(|r| !r.is_empty()).collect::<Vec<_>>(),
                "dut_image": if image.is_empty() { Value::Null } else { Value::from(image) }, "definition_hash": hash, "runs": runs,
            })
        })
        .collect()
}

/// `report-data.json` for the next revision of a Test run, from what Core holds. Deterministic for
/// a given log and files: no clock and no log head, so publish can check what it signs.
/// Each limit of a test with the decision rule, `U` and `w` its verdict was judged on (decision 154).
/// `U` and `k` are the test's declared source for the measurement, which is what the executor's
/// `judge_measurements` reads; a measurement with no source has none (`u`, `k` null, `u_declared`
/// false) and was judged with `w = 0`, so `w` says 0 and not an invented band.
fn limits_of(t: &embarch_study_designer::hil_expand::RunTest) -> Value {
    let rule = &t.decision_rule;
    Value::Array(
        t.limits
            .iter()
            .map(|l| {
                let source = t.sources.iter().find(|s| s.measurement == l.measurement);
                let guard = rule.kind != "simple_acceptance";
                let u = source.map(|s| s.u);
                json!({
                    "measurement": l.measurement, "unit": l.unit, "lower": l.lower, "upper": l.upper, "expect": l.expect,
                    "rule": rule.kind, "w_multiple": rule.w_multiple,
                    "u": u, "k": source.map(|s| s.k), "u_declared": source.is_some(),
                    "w": if guard { rule.w_multiple * u.unwrap_or(0.0) } else { 0.0 },
                })
            })
            .collect(),
    )
}

fn test_report_data(state: &AppState, lines: &[Value], held: &Held, run_id: &str, publisher: Scope) -> Result<(Value, u64), Failure> {
    let prepared = crate::test_runs::read_record(state.run_log.data_dir(), run_id)?;
    let run: RunJson = serde_json::from_value(prepared["run"].clone()).map_err(internal)?;
    let predicate = &held.statement["predicate"];
    let unit_key = predicate["unit_key"].as_str();
    let results = held.result.as_ref().and_then(|r| r["results"].as_array());

    let mut rows: Vec<TestRow> = Vec::new();
    let mut tests = Vec::new();
    for t in predicate["tests"].as_array().into_iter().flatten() {
        let id = t["test_id"].as_str().unwrap_or("");
        let result = results.and_then(|rs| rs.iter().find(|r| r["test_id"] == id));
        let judged = judged_of(result);
        let reached = judged.is_some();
        let run_test = run.tests.iter().find(|r| r.test_id == id && Some(r.definition_hash.as_str()) == t["definition_hash"].as_str());
        tests.push(json!({
            "test_id": id, "bench": t["bench"], "definition_hash": t["definition_hash"], "requirements": t["requirements"],
            "verdict": t["verdict"], "reason": t["reason"], "qualifiers": result.map_or(json!([]), |r| r["qualifiers"].clone()),
            "counts": result.map_or(Value::Null, |r| r["counts"].clone()), "runs": result.map_or(json!([]), |r| r["runs"].clone()),
            "note": t["note"], "dut_image": t["dut_image"],
            "decision_rule": run_test.map_or(Value::Null, |r| json!({ "kind": r.decision_rule.kind, "w_multiple": r.decision_rule.w_multiple })),
            "limits": run_test.map_or(Value::Null, limits_of),
            "study_ids": t["study_records"].as_array().into_iter().flatten().map(|s| s["study_id"].clone()).collect::<Vec<_>>(),
        }));
        rows.push(TestRow {
            test_id: id.to_string(),
            definition_hash: t["definition_hash"].as_str().unwrap_or("").to_string(),
            requirements: strings(&t["requirements"]),
            judged: judged.unwrap_or_else(unreached),
            reached,
            reason: t["reason"].clone(),
        });
    }

    let (manual, manual_listed) = manual_rows(lines, unit_key, &rows);
    let rules = Rules::latest();
    let rule_err = |e: embarch_study_designer::verdicts::RuleError| internal(format!("{e:?}"));
    let req_ids: BTreeSet<&String> = rows.iter().flat_map(|t| t.requirements.iter()).collect();
    let mut requirements = serde_json::Map::new();
    for req in req_ids {
        let mut items: Vec<Judged> = Vec::new();
        let (mut ids, mut skipped_tests) = (Vec::new(), Vec::new());
        for t in rows.iter().filter(|t| t.requirements.contains(req)) {
            items.push(t.judged.clone());
            ids.push(t.test_id.clone());
            if t.judged.verdict == Verdict::Skipped {
                let reason = if t.reached { serde_json::to_value(t.judged.reason).map_err(internal)? } else { t.reason.clone() };
                skipped_tests.push(json!({ "test_id": t.test_id, "reason": reason }));
            }
        }
        items.extend(manual.iter().filter(|m| m.matched && m.requirements.contains(req) && (m.counted || m.relayed)).map(|m| m.judged.clone()));
        let mut v = serde_json::to_value(rules.requirement(&items).map_err(rule_err)?).map_err(internal)?;
        v["tests"] = json!(ids);
        v["skipped_tests"] = json!(skipped_tests);
        requirements.insert(req.clone(), v);
    }

    let all: Vec<Judged> = rows.iter().map(|t| t.judged.clone()).collect();
    let mut summary = rules.combine(&all).map_err(rule_err)?;
    if unit_key.is_none() {
        summary.qualifiers.push(Qualifier::UnitIdentityNotRead);
        summary.qualifiers.sort_unstable();
        summary.qualifiers.dedup();
    }

    // What did not run: a selected test the run left skipped or never reached, and every test of
    // the suite the selection left out, each with its reason.
    let mut not_run: Vec<Value> = rows
        .iter()
        .filter(|t| t.judged.verdict == Verdict::Skipped)
        .map(|t| {
            let reason = if t.reached { serde_json::to_value(t.judged.reason).unwrap_or(Value::Null) } else { t.reason.clone() };
            json!({ "source": "run", "test_id": t.test_id, "requirements": t.requirements, "reason": reason })
        })
        .collect();
    let mut by_status: BTreeMap<String, u64> = BTreeMap::new();
    for e in predicate["selection"]["entries"].as_array().into_iter().flatten() {
        let status = e["status"].as_str().unwrap_or("").to_string();
        if status != "selected" {
            not_run.push(json!({ "source": "selection", "test_id": e["test_id"], "status": e["status"], "reason": e["reason"] }));
        }
        *by_status.entry(status).or_default() += 1;
    }
    let coverage = json!({
        "tests_in_run": rows.len(),
        "tests_judged": rows.iter().filter(|t| t.reached && t.judged.verdict != Verdict::Skipped).count(),
        "tests_skipped": rows.iter().filter(|t| t.reached && t.judged.verdict == Verdict::Skipped).count(),
        "tests_not_reached": rows.iter().filter(|t| !t.reached).count(),
        "selection": by_status,
    });

    let go_by = lines.iter().rev().find(|l| is_entry(l, "test_run_started", run_id)).map_or(Value::Null, |l| l["entry"]["scope"].clone());
    let unit_declared = unit_key.map_or(Value::Null, |k| latest_entry(lines, "unit_declared", Some(k)));
    let person = latest_entry(lines, "person_declared", None);
    let person_acted = go_by == "person"
        || publisher == Scope::Person
        || !unit_declared.is_null()
        || !person.is_null()
        || manual.iter().any(|m| m.counted);
    let revision = revision_of(lines, run_id);
    let mut data = json!({
        "schema": 1,
        "kind": "test_run",
        "run_id": run_id,
        "revision": revision,
        "run_status": predicate["status"],
        "reason": predicate["reason"],
        "resumes": predicate["resumes"],
        "suite": { "id": run.suite.id, "version": run.suite.version, "commit": run.suite.commit },
        "standard": { "name": run.standard.name, "revision": run.standard.revision, "requirement_count": null, "atlas": { "joined": false, "note": NO_ATLAS } },
        "unit": {
            "unit_key": unit_key, "identity_read": unit_key.is_some(),
            "declared_serial": run.dut.declared_serial, "board_revision": run.dut.board_revision,
            "declared": if unit_declared.is_null() { Value::Null } else {
                json!({ "serial": unit_declared["serial"], "board_revision": unit_declared["board_revision"], "scope": unit_declared["scope"], "at_utc_ms": unit_declared["at_utc_ms"] })
            },
        },
        "person": if person.is_null() { Value::Null } else { json!({ "name": person["name"], "scope": person["scope"], "at_utc_ms": person["at_utc_ms"] }) },
        "go_by": go_by,
        "published_by": publisher.as_str(),
        "statement": FIXED_STATEMENT,
        "attestation": attestation(person_acted),
        "summary": summary,
        "requirements": requirements,
        "tests": tests,
        "manual_verdicts": manual_listed,
        "not_run": not_run,
        "coverage": coverage,
        "runs_on_unit": runs_on_unit(lines, unit_key),
        "rules": rules.behaviours().iter().map(|(n, b)| json!({ "name": n, "behaviour": b })).collect::<Vec<_>>(),
        "core": { "version": env!("CARGO_PKG_VERSION"), "keyid": state.sealer.key().keyid() },
        "record_sha256": held.entry["record_sha256"],
        "study_ids": held.study_ids,
    });
    if let Some(claim) = call_log_claim(&predicate["observed"]) {
        data["call_log"] = claim;
    }
    Ok((data, revision))
}

/// Writes and signs `report-data.json` for the Test run's next revision, and answers it. Asking
/// again replaces it; nothing is recorded in the log until `publish`.
// route: POST /test-runs/{run_id}/report-data
pub async fn test_report_data_handler(State(state): State<AppState>, UrlPath(run_id): UrlPath<String>, scope: Scope) -> Result<Json<Value>, Failure> {
    tokio::task::spawn_blocking(move || {
        let lines = state.run_log.lines().map_err(internal)?;
        let held = ended_run(&state, &lines, &run_id)?;
        let (data, revision) = test_report_data(&state, &lines, &held, &run_id, scope)?;
        let bytes = serde_json::to_vec(&data).map_err(internal)?;
        let sha = sha256_hex(&bytes);
        let statement = statement_bytes(vec![json!({ "name": REPORT_DATA_FILE, "digest": { "sha256": sha } })], REPORT_DATA_PREDICATE_TYPE, data.clone())?;
        let dir = publish_dir(&state, &run_id, revision);
        write_file(&dir.join(REPORT_DATA_FILE), &bytes)?;
        write_file(&dir.join(REPORT_DATA_ENVELOPE_FILE), &state.sealer.sign_statement(&statement).map_err(internal)?)?;
        write_file(&dir.join(RUN_FILE), &held.run_bytes)?;
        Ok(Json(json!({ "revision": revision, "report_data_sha256": sha, "keyid": state.sealer.key().keyid(), "report_data": data })))
    })
    .await
    .map_err(internal)?
}

/// A file of a Test run's revision, by the name the zip gives it, and the same rules as a Study's
/// (decision 116): one of Core's five publish files, or any file the revision's signed manifest
/// names; at the pending revision (opened by `report-data`, no manifest yet), the run's own files.
/// Nothing else is served, and a name that is not a plain relative path is refused before any path
/// is built. A name the manifest carries that Core did not make (embarch-api's) is `404`, as it is on
/// a Study.
// route: GET /test-runs/{run_id}/publish/{revision}/files/{*name}
pub async fn test_file_handler(
    State(state): State<AppState>,
    UrlPath((run_id, revision, name)): UrlPath<(String, u64, String)>,
) -> Result<Response, Failure> {
    let not_found = || (StatusCode::NOT_FOUND, format!("revision {revision} of {run_id} has no file {name}"));
    if !crate::test_runs::is_run_id(&run_id) || !safe_name(&name) {
        return Err(not_found());
    }
    let path = tokio::task::spawn_blocking({
        let (state, run_id, name) = (state.clone(), run_id.clone(), name.clone());
        move || -> Result<Option<PathBuf>, Failure> {
            let rdir = publish_dir(&state, &run_id, revision);
            if PUBLISH_FILES.contains(&name.as_str()) {
                return Ok(Some(rdir.join(&name)));
            }
            let store = state.run_log.data_dir();
            let path = match name.as_str() {
                RUN_FILE => rdir.join(RUN_FILE),
                RESULT_FILE => crate::test_runs::result_path(store, &run_id),
                RECORD_NAME => crate::test_runs::record_path(store, &run_id),
                n => match n.strip_prefix(STUDIES_DIR).and_then(|r| r.strip_prefix('/')).and_then(|r| r.split_once('/')) {
                    Some((id, rest)) => state.run_log.run_dir(id).join(rest),
                    None => return Ok(None),
                },
            };
            match manifest_names(&rdir, &name)? {
                Some(true) => Ok(Some(path)),
                Some(false) => Ok(None),
                // No manifest yet: only the pending revision, once `report-data` has opened it,
                // serves the run's own files, the ones the manifest will sign.
                None => {
                    let lines = state.run_log.lines().map_err(internal)?;
                    if revision != revision_of(&lines, &run_id) || !rdir.join(REPORT_DATA_FILE).is_file() {
                        return Ok(None);
                    }
                    let held = ended_run(&state, &lines, &run_id)?;
                    let own = match name.strip_prefix(STUDIES_DIR).and_then(|r| r.strip_prefix('/')).and_then(|r| r.split_once('/')) {
                        Some((id, rest)) => held.study_ids.iter().any(|s| s == id) && core_run_files(&state.run_log.run_dir(id))?.iter().any(|f| f == rest),
                        None => name != RESULT_FILE || held.result_path.is_some(),
                    };
                    Ok(own.then_some(path))
                }
            }
        }
    })
    .await
    .map_err(internal)??;
    let bytes = tokio::fs::read(path.ok_or_else(not_found)?).await.map_err(|_| not_found())?;
    Ok(served(bytes))
}

/// Signs the manifest over every file the Test run and its study runs produced and records the
/// publish, with the run log's head, in the log.
// route: POST /test-runs/{run_id}/publish
pub async fn test_publish_handler(
    State(state): State<AppState>,
    UrlPath(run_id): UrlPath<String>,
    scope: Scope,
    Json(req): Json<PublishRequest>,
) -> Result<Json<Value>, Failure> {
    check_files(&req.files)?;
    tokio::task::spawn_blocking(move || publish_run(&state, &run_id, scope, req)).await.map_err(internal)?
}

fn publish_run(state: &AppState, run_id: &str, scope: Scope, req: PublishRequest) -> Result<Json<Value>, Failure> {
    let first = state.run_log.lines().map_err(internal)?;
    let held = ended_run(state, &first, run_id)?;
    let (data, revision) = test_report_data(state, &first, &held, run_id, scope)?;
    let rdir = publish_dir(state, run_id, revision);
    let on_disk = std::fs::read(rdir.join(REPORT_DATA_FILE))
        .map_err(|_| conflict("ask for report-data first: POST /test-runs/{id}/report-data, then build the zip's files from it".to_string()))?;
    let bytes = serde_json::to_vec(&data).map_err(internal)?;
    if on_disk != bytes {
        return Err(conflict(
            "report-data.json no longer matches what Core would write now (a verdict, a declaration or a run on the unit was entered since, or the publishing token's scope differs from the one that asked); ask for report-data again".into(),
        ));
    }

    // Hash the files before taking the log lock; they do not move.
    let mut subjects: Vec<(String, String)> = Vec::new();
    for id in &held.study_ids {
        let dir = state.run_log.run_dir(id);
        for name in core_run_files(&dir)? {
            subjects.push((format!("{STUDIES_DIR}/{id}/{name}"), sha256_of_file(&dir.join(&name)).map_err(internal)?));
        }
    }
    write_file(&rdir.join(RUN_FILE), &held.run_bytes)?;
    subjects.push((RUN_FILE.to_string(), sha256_hex(&held.run_bytes)));
    if let Some(p) = &held.result_path {
        subjects.push((RESULT_FILE.to_string(), sha256_of_file(p).map_err(internal)?));
    }
    subjects.push((RECORD_NAME.to_string(), sha256_of_file(&held.record_path).map_err(internal)?));
    for name in [REPORT_DATA_FILE, REPORT_DATA_ENVELOPE_FILE] {
        subjects.push((name.to_string(), sha256_of_file(&rdir.join(name)).map_err(internal)?));
    }
    check_api_names(&req.files, &subjects)?;
    let call_segment = call_log_segment_of(state, &held.statement)?;
    let report_data_sha256 = sha256_hex(&bytes);
    let record_sha256 = data["record_sha256"].clone();

    let entry = state.run_log.append_with(|lines, head| {
        // Everything has to have held between the check above and now.
        if !lines.iter().any(|l| is_entry(l, "test_run", run_id)) {
            return Err(conflict(format!("run {run_id} is not in the run log")));
        }
        if revision_of(lines, run_id) != revision {
            return Err(conflict(format!("revision {revision} was published meanwhile; ask for report-data again")));
        }
        let supersedes = lines
            .iter()
            .rfind(|l| is_entry(l, "published", run_id))
            .map(|l| json!({ "revision": revision - 1, "manifest_sha256": l["entry"]["manifest_sha256"] }));
        let m = Manifest {
            id_key: "run_id", id: run_id, revision, rdir: &rdir, subjects: &subjects, call_segment: call_segment.as_deref(),
            files: &req.files, scope, record_sha256: &record_sha256, report_data_sha256: &report_data_sha256,
        };
        sign_manifest(state, &m, lines, head, supersedes)
    })?;
    Ok(Json(json!({ "revision": revision, "entry": entry, "keyid": state.sealer.key().keyid() })))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::run_log::RunLog;
    use crate::signing::{Envelope, SealJob};
    use tower::ServiceExt as _;

    const RID: &str = "run-1";
    const UNIT: &str = "DUT-1";

    fn hash(c: &str) -> String {
        c.repeat(64)
    }

    fn test_json(id: &str, bench: &str, requirements: &[&str]) -> Value {
        json!({
            "test_id": id, "title": id, "definition_hash": hash(id.get(1..2).unwrap_or("0")),
            "study": { "kind": "host", "hash": hash("0"), "file": format!("{id}.json") },
            "requirements": requirements, "revision_range": { "from": "3.0", "to": null },
            "bench": { "name": bench, "config": "default" },
            "precondition": "reset", "requires": [], "run_count": 1,
            "decision_rule": { "kind": "simple_acceptance", "w_multiple": 0.0 },
            "limits": [], "setup_terms": [], "sources": [], "prompt_kinds": [], "hooks": [], "step_budget_ms": 1000
        })
    }

    /// A test that states a guard band of 1 x U over a limit its source declares `U = 10`, `k = 2` for.
    fn guarded(mut t: Value) -> Value {
        t["decision_rule"] = json!({ "kind": "guard_band", "w_multiple": 1.0 });
        t["limits"] = json!([ { "measurement": "vbus", "unit": "mV", "lower": 4750.0, "upper": 5250.0 } ]);
        t["sources"] = json!([ { "measurement": "vbus", "unit": "mV", "range_min": 0.0, "range_max": 9000.0, "resolution": 1.0, "U": 10.0, "k": 2.0 } ]);
        t
    }

    /// A test under simple acceptance with a limit and no declared source for it.
    fn limited(mut t: Value) -> Value {
        t["limits"] = json!([ { "measurement": "ibus", "unit": "mA", "lower": null, "upper": 3000.0 } ]);
        t
    }

    fn run_json() -> RunJson {
        serde_json::from_value(json!({
            "schema": 1, "run_id": RID, "suite": { "id": "usb-pd", "version": "1" }, "plan_hash": null,
            "standard": { "name": "usb-pd", "revision": "3.2" },
            "dut": { "hardware_id": UNIT, "declared_serial": "u1", "board_revision": "A", "capabilities": [], "hooks": [] },
            "options": { "stop_on_fail": false },
            "benches": [ { "name": "A", "tests": ["t1", "t2", "t3"], "boards": [ { "part": "pd", "idle": "idle", "hello": "hello" } ] } ],
            "tests": [ guarded(test_json("t1", "A", &["R1", "R2"])), limited(test_json("t2", "A", &["R1"])), test_json("t3", "A", &["R2"]) ],
        }))
        .unwrap_or_else(|e| panic!("{e}"))
    }

    fn seal_study(state: &AppState, id: &str) -> String {
        let dir = state.run_log.run_dir(id);
        std::fs::create_dir_all(dir.join("streams")).unwrap();
        std::fs::write(dir.join("streams/tap.bin"), id.as_bytes()).unwrap();
        std::fs::write(dir.join("events.json"), r#"{"study_name":"fixture","steps":[{"outcome":"Pass"}],"provenance":{}}"#).unwrap();
        state.run_log.begin(id, "host", Scope::Agent);
        let job = SealJob { sealer: state.sealer.clone(), kind: "host", build: None, submitted_by: Some(Scope::Agent), calls: None };
        state.sealer.seal(&job, &dir, id, 1).unwrap();
        state.run_log.end(id, "completed", None);
        sha256_of_file(&dir.join(crate::signing::RECORD_FILE)).unwrap()
    }

    /// A Test run as the executor leaves it: t1 passes, t2 fails, t3 is skipped because what it
    /// `requires` was not met. Plus an earlier failing run of t1 on the same unit.
    fn finished(state: &AppState) -> RunLog {
        let log = state.run_log.clone();
        let run = run_json();
        let store = log.data_dir().to_path_buf();
        std::fs::create_dir_all(store.join("test-runs")).unwrap();
        let selection = json!({ "entries": [
            { "test_id": "t1", "status": "selected" }, { "test_id": "t2", "status": "selected" }, { "test_id": "t3", "status": "selected" },
            { "test_id": "t4", "status": "hook_unmapped", "reason": "hook h has no mapping" },
            { "test_id": "t5", "status": "not_selected" },
        ] });
        std::fs::write(store.join("test-runs/run-1.json"), json!({ "run_id": RID, "run_hash": run.hash(), "run": run, "selection": selection }).to_string()).unwrap();
        let (h1, h2) = (seal_study(state, "st1"), seal_study(state, "st2"));
        let results = json!({ "run_id": RID, "status": "completed", "results": [
            { "test_id": "t1", "bench": "A", "verdict": "pass", "reason": null, "qualifiers": ["bench_firmware_unreleased"], "counts": null, "runs": [{ "verdict": "pass", "reason": null }], "study_ids": ["st1"], "note": null },
            { "test_id": "t2", "bench": "A", "verdict": "fail", "reason": "limit", "qualifiers": [], "counts": null, "runs": [{ "verdict": "fail", "reason": "limit" }], "study_ids": ["st2"], "note": null },
            { "test_id": "t3", "bench": "A", "verdict": "skipped", "reason": "requires_not_met", "qualifiers": [], "counts": null, "runs": [], "study_ids": [], "note": "t2 failed" },
        ] });
        std::fs::write(crate::test_runs::result_path(&store, RID), results.to_string()).unwrap();
        let tests = json!([
            { "test_id": "t1", "bench": "A", "definition_hash": run.tests[0].definition_hash, "requirements": ["R1", "R2"], "verdict": "pass", "reason": null, "note": null, "dut_image": "holds", "study_records": [{ "study_id": "st1", "record_sha256": h1 }] },
            { "test_id": "t2", "bench": "A", "definition_hash": run.tests[1].definition_hash, "requirements": ["R1"], "verdict": "fail", "reason": "limit", "note": null, "dut_image": "holds", "study_records": [{ "study_id": "st2", "record_sha256": h2 }] },
            { "test_id": "t3", "bench": "A", "definition_hash": run.tests[2].definition_hash, "requirements": ["R2"], "verdict": "skipped", "reason": "requires_not_met", "note": "t2 failed", "dut_image": "holds", "study_records": [] },
        ]);
        let predicate = json!({
            "schema": 1, "run_id": RID, "status": "completed", "reason": null, "ended_utc_ms": 1, "unit_key": UNIT, "selection": selection, "tests": tests,
            "observed": { "core": { "version": "x" }, "call_log": null },
        });
        let subjects = vec![("run.json".to_string(), run.hash()), ("result.json".to_string(), sha256_of_file(&crate::test_runs::result_path(&store, RID)).unwrap())];
        let record = crate::test_runs::record_path(&store, RID);
        state.sealer.seal_test_run(&record, &subjects, predicate).unwrap();
        // An earlier run of t1 on the unit, which failed, then the run itself.
        log.test_entry("run-0", "t1", &run.tests[0].definition_hash, &["R1".into(), "R2".into()], Some(UNIT), Some("holds"), Some("fail")).unwrap();
        log.test_run_started(RID, &run.hash(), Scope::Person, "s").unwrap();
        for (t, v) in [("t1", "pass"), ("t2", "fail"), ("t3", "skipped")] {
            let r = run.tests.iter().find(|x| x.test_id == t).unwrap();
            log.test_entry(RID, t, &r.definition_hash, &r.requirements, Some(UNIT), Some("holds"), Some(v)).unwrap();
        }
        let studies = [json!({ "study_id": "st1", "record_sha256": h1 }), json!({ "study_id": "st2", "record_sha256": h2 })];
        log.test_run_entry(RID, "completed", Some(&sha256_of_file(&record).unwrap()), &studies).unwrap();
        log
    }

    async fn send(state: &AppState, uri: &str, body: &str) -> (StatusCode, Value) {
        let req = axum::http::Request::builder()
            .method("POST")
            .uri(uri)
            .header("authorization", "Bearer agent")
            .header("content-type", "application/json")
            .body(axum::body::Body::from(body.to_string()))
            .unwrap();
        let r = crate::api::build_router(state.clone()).oneshot(req).await.unwrap();
        let status = r.status();
        let bytes = axum::body::to_bytes(r.into_body(), 1 << 24).await.unwrap();
        (status, serde_json::from_slice(&bytes).unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into_owned())))
    }

    fn judged(v: Verdict, r: Option<Reason>) -> Judged {
        Judged { verdict: v, reason: r, qualifiers: vec![] }
    }

    #[tokio::test]
    async fn report_data_carries_the_crates_requirement_roll_up_and_is_signed() {
        let state = AppState::new("agent".to_string());
        finished(&state);
        let (s, b) = send(&state, "/test-runs/run-1/report-data", "").await;
        assert_eq!(s, StatusCode::OK, "{b}");
        let d = &b["report_data"];
        let r1 = serde_json::to_value(Rules::latest().requirement(&[Judged { qualifiers: vec![Qualifier::BenchFirmwareUnreleased], ..judged(Verdict::Pass, None) }, judged(Verdict::Fail, Some(Reason::Limit))]).unwrap()).unwrap();
        for k in ["verdict", "counts", "qualifiers", "n_of_m", "skipped", "relayed_listed_apart"] {
            assert_eq!(d["requirements"]["R1"][k], r1[k], "R1.{k}");
        }
        assert_eq!(d["requirements"]["R1"]["verdict"], "fail");
        assert_eq!(d["requirements"]["R1"]["counts"]["pass"], 1);
        assert_eq!(d["requirements"]["R1"]["qualifiers"], json!(["bench_firmware_unreleased"]));
        assert_eq!((d["kind"].as_str(), d["statement"].as_str()), (Some("test_run"), Some(FIXED_STATEMENT)));
        assert_eq!(d["summary"]["verdict"], "fail");
        assert_eq!(d["standard"]["requirement_count"], Value::Null);
        assert_eq!(d["standard"]["atlas"]["joined"], false);
        assert_eq!((d["go_by"].as_str(), d["published_by"].as_str()), (Some("person"), Some("agent")));
        // The signed copy is the file's content and verifies as a DSSE v1 envelope.
        let dir = publish_dir(&state, RID, 1);
        let env: Envelope = serde_json::from_slice(&std::fs::read(dir.join(REPORT_DATA_ENVELOPE_FILE)).unwrap()).unwrap();
        let payload = crate::signing::tests::standard_dsse_check(&env, &state.sealer.key().public_key_hex()).expect("verifies");
        let signed: Value = serde_json::from_slice(&payload).unwrap();
        assert_eq!(signed["predicate"], serde_json::from_slice::<Value>(&std::fs::read(dir.join(REPORT_DATA_FILE)).unwrap()).unwrap());
    }

    #[tokio::test]
    async fn each_tests_limits_state_their_decision_rule_w_and_u_as_judged() {
        let state = AppState::new("agent".to_string());
        finished(&state);
        let (s, b) = send(&state, "/test-runs/run-1/report-data", "").await;
        assert_eq!(s, StatusCode::OK, "{b}");
        let tests = &b["report_data"]["tests"];
        assert_eq!(tests[0]["decision_rule"], json!({ "kind": "guard_band", "w_multiple": 1.0 }));
        let l = &tests[0]["limits"][0];
        assert_eq!((l["measurement"].as_str(), l["lower"].as_f64(), l["upper"].as_f64()), (Some("vbus"), Some(4750.0), Some(5250.0)));
        assert_eq!((l["rule"].as_str(), l["w_multiple"].as_f64(), l["u"].as_f64(), l["k"].as_f64(), l["w"].as_f64(), l["u_declared"].as_bool()), (Some("guard_band"), Some(1.0), Some(10.0), Some(2.0), Some(10.0), Some(true)));
        let l = &tests[1]["limits"][0];
        assert_eq!((l["rule"].as_str(), l["w"].as_f64(), l["u_declared"].as_bool()), (Some("simple_acceptance"), Some(0.0), Some(false)));
        assert_eq!((l["u"].clone(), l["k"].clone(), l["lower"].clone()), (Value::Null, Value::Null, Value::Null));
        assert_eq!(tests[2]["limits"], json!([]));
    }

    #[tokio::test]
    async fn a_test_skipped_for_a_gap_makes_its_requirement_not_verified_and_is_listed_with_its_reason() {
        let state = AppState::new("agent".to_string());
        finished(&state);
        let (_, b) = send(&state, "/test-runs/run-1/report-data", "").await;
        let d = &b["report_data"];
        assert_eq!(d["requirements"]["R2"]["verdict"], "not_verified");
        assert_eq!(d["requirements"]["R2"]["skipped_tests"], json!([{ "test_id": "t3", "reason": "requires_not_met" }]));
        let not_run = d["not_run"].as_array().unwrap();
        assert!(not_run.iter().any(|n| n["source"] == "run" && n["test_id"] == "t3" && n["reason"] == "requires_not_met"));
        assert!(not_run.iter().any(|n| n["source"] == "selection" && n["test_id"] == "t4" && n["status"] == "hook_unmapped"));
        assert_eq!(d["coverage"]["selection"]["not_selected"], 1);
        assert_eq!((d["coverage"]["tests_judged"].as_u64(), d["coverage"]["tests_skipped"].as_u64()), (Some(2), Some(1)));
    }

    #[tokio::test]
    async fn every_earlier_run_of_a_test_on_the_unit_is_listed_failed_ones_included() {
        let state = AppState::new("agent".to_string());
        finished(&state);
        let (_, b) = send(&state, "/test-runs/run-1/report-data", "").await;
        let group = b["report_data"]["runs_on_unit"].as_array().unwrap().iter().find(|g| g["test_id"] == "t1").unwrap().clone();
        let runs: Vec<(&str, &str)> = group["runs"].as_array().unwrap().iter().map(|r| (r["run_id"].as_str().unwrap(), r["verdict"].as_str().unwrap())).collect();
        assert_eq!(runs, [("run-0", "fail"), ("run-1", "pass")]);
        assert_eq!((group["requirements"].clone(), group["dut_image"].as_str()), (json!(["R1", "R2"]), Some("holds")));
    }

    #[tokio::test]
    async fn a_relayed_manual_verdict_is_listed_apart_and_a_persons_counts_and_declarations_are_read_back() {
        let state = AppState::new("agent".to_string());
        let log = finished(&state);
        let h = run_json().tests[2].definition_hash.clone();
        log.manual_verdict("t3", &h, UNIT, None, "pass", "looked", None, Scope::Agent).unwrap();
        log.unit_declared(UNIT, "SN-7", "B").unwrap();
        log.person_declared("Ada").unwrap();
        let (_, b) = send(&state, "/test-runs/run-1/report-data", "").await;
        let d = &b["report_data"];
        assert_eq!(d["requirements"]["R2"]["relayed_listed_apart"], 1);
        assert_eq!(d["requirements"]["R2"]["counts"]["pass"], 1, "only t1's pass; the relayed one is not counted");
        let m = &d["manual_verdicts"][0];
        assert_eq!((m["relayed"].clone(), m["counted"].clone(), m["attribution"].as_str()), (json!(true), json!(false), Some("manual verdict, relayed by the agent")));
        assert_eq!((d["unit"]["declared"]["serial"].as_str(), d["person"]["name"].as_str()), (Some("SN-7"), Some("Ada")));
        // A person's verdict for the same test counts.
        log.manual_verdict("t3", &h, UNIT, None, "pass", "measured", None, Scope::Person).unwrap();
        let (_, b) = send(&state, "/test-runs/run-1/report-data", "").await;
        assert_eq!(b["report_data"]["requirements"]["R2"]["counts"]["pass"], 2);
        assert_eq!(b["report_data"]["requirements"]["R2"]["qualifiers"], json!(["bench_firmware_unreleased", "rests_on_person_observation"]));
    }

    #[tokio::test]
    async fn publish_signs_a_manifest_over_every_study_runs_files_and_enters_it_in_the_run_log() {
        let state = AppState::new("agent".to_string());
        let log = finished(&state);
        assert!(log.unpublished_ids().unwrap().contains("st1"));
        let (s, _) = send(&state, "/test-runs/run-1/publish", r#"{"files":[]}"#).await;
        assert_eq!(s, StatusCode::CONFLICT, "report-data first");
        let (s, _) = send(&state, "/test-runs/run-1/report-data", "").await;
        assert_eq!(s, StatusCode::OK);
        let pdf = "cd".repeat(32);
        let (s, b) = send(&state, "/test-runs/run-1/publish", &format!(r#"{{"files":[{{"name":"report.pdf","sha256":"{pdf}"}}]}}"#)).await;
        assert_eq!(s, StatusCode::OK, "{b}");
        let dir = publish_dir(&state, RID, 1);
        let env_bytes = std::fs::read(dir.join(crate::publish::MANIFEST_FILE)).unwrap();
        let env: Envelope = serde_json::from_slice(&env_bytes).unwrap();
        let st: Value = serde_json::from_slice(&crate::signing::tests::standard_dsse_check(&env, &state.sealer.key().public_key_hex()).expect("verifies")).unwrap();
        let names: Vec<&str> = st["subject"].as_array().unwrap().iter().map(|s| s["name"].as_str().unwrap()).collect();
        for want in [
            "studies/st1/events.json", "studies/st1/record.dsse.json", "studies/st1/streams/tap.bin", "studies/st2/events.json", "studies/st2/record.dsse.json",
            "studies/st2/streams/tap.bin", "run.json", "result.json", "test-run.dsse.json", "report-data.json", "report-data.dsse.json", "run-log-segment.jsonl",
        ] {
            assert!(names.contains(&want), "{want} in {names:?}");
        }
        assert!(!names.contains(&"report.pdf"));
        assert_eq!(st["predicate"]["run_id"], RID);
        let digest = |n: &str| st["subject"].as_array().unwrap().iter().find(|s| s["name"] == n).unwrap()["digest"]["sha256"].as_str().unwrap().to_string();
        assert_eq!(digest("studies/st2/streams/tap.bin"), sha256_hex(b"st2"));
        assert_eq!(digest("run.json"), sha256_hex(&std::fs::read(dir.join("run.json")).unwrap()));
        assert_eq!(digest("run.json"), run_json().hash(), "the file is the canonical bytes the record's run.json hash is over");
        // The log holds the manifest's hash against the run, and the studies are released.
        let entry = log.lines().unwrap().into_iter().map(|l| l["entry"].clone()).rfind(|e| e["type"] == "published").unwrap();
        assert_eq!((entry["run_id"].as_str(), entry["study_id"].is_null()), (Some(RID), true));
        assert_eq!(entry["manifest_sha256"], sha256_hex(&env_bytes).as_str());
        assert!(log.unpublished_ids().unwrap().is_empty());
        assert!(log.verify().unwrap().is_ok());
        // A second publish is revision 2, naming the first.
        send(&state, "/test-runs/run-1/report-data", "").await;
        let (s, b) = send(&state, "/test-runs/run-1/publish", r#"{"files":[]}"#).await;
        assert_eq!((s, b["revision"].clone()), (StatusCode::OK, json!(2)));
        let env: Envelope = serde_json::from_slice(&std::fs::read(publish_dir(&state, RID, 2).join(crate::publish::MANIFEST_FILE)).unwrap()).unwrap();
        let st2: Value = serde_json::from_slice(&crate::signing::tests::standard_dsse_check(&env, &state.sealer.key().public_key_hex()).unwrap()).unwrap();
        assert_eq!(st2["predicate"]["supersedes"]["manifest_sha256"], sha256_hex(&env_bytes).as_str());
    }

    async fn get(state: &AppState, uri: &str) -> (StatusCode, Vec<u8>) {
        let req = axum::http::Request::builder().method("GET").uri(uri).header("authorization", "Bearer agent").body(axum::body::Body::empty()).unwrap();
        let r = crate::api::build_router(state.clone()).oneshot(req).await.unwrap();
        let status = r.status();
        (status, axum::body::to_bytes(r.into_body(), 1 << 24).await.unwrap().to_vec())
    }

    #[tokio::test]
    async fn a_revisions_files_are_served_pending_and_signed_and_nothing_else() {
        let state = AppState::new("agent".to_string());
        finished(&state);
        let url = |r: u64, n: &str| format!("/test-runs/{RID}/publish/{r}/files/{n}");
        // Before report-data opens the revision, nothing of the run is served.
        assert_eq!(get(&state, &url(1, "run.json")).await.0, StatusCode::NOT_FOUND);
        assert_eq!(get(&state, &url(1, "studies/st1/events.json")).await.0, StatusCode::NOT_FOUND);
        send(&state, "/test-runs/run-1/report-data", "").await;
        let mut served = Vec::new();
        for n in ["run.json", "result.json", "test-run.dsse.json", "studies/st1/events.json", "studies/st2/streams/tap.bin", "report-data.json", "report-data.dsse.json"] {
            let (s, b) = get(&state, &url(1, n)).await;
            assert_eq!(s, StatusCode::OK, "{n}");
            served.push((n, sha256_hex(&b)));
        }
        assert_eq!(get(&state, &url(1, "studies/st2/streams/tap.bin")).await.1, b"st2");
        // Not a file of the run, not a study the run did not make, not a path out, not the pending
        // revision's manifest, not another revision.
        for n in ["../run-log.jsonl", "studies/st1/../../run-log.jsonl", "studies/st9/events.json", "studies/st1", "studies", "nothing.json", "report.pdf", "manifest.dsse.json", "/etc/passwd"] {
            assert_eq!(get(&state, &url(1, n)).await.0, StatusCode::NOT_FOUND, "{n}");
        }
        assert_eq!(get(&state, &url(2, "run.json")).await.0, StatusCode::NOT_FOUND);
        assert_eq!(get(&state, "/test-runs/nope/publish/1/files/run.json").await.0, StatusCode::NOT_FOUND);
        // The bytes are the ones the manifest then signs, and a file embarch-api made is not Core's to serve.
        let pdf = "cd".repeat(32);
        let (s, _) = send(&state, "/test-runs/run-1/publish", &format!(r#"{{"files":[{{"name":"report.pdf","sha256":"{pdf}"}}]}}"#)).await;
        assert_eq!(s, StatusCode::OK);
        let (s, env) = get(&state, &url(1, "manifest.dsse.json")).await;
        assert_eq!(s, StatusCode::OK);
        let env: Envelope = serde_json::from_slice(&env).unwrap();
        let st: Value = serde_json::from_slice(&crate::signing::tests::standard_dsse_check(&env, &state.sealer.key().public_key_hex()).unwrap()).unwrap();
        for (n, sha) in served {
            let subject = st["subject"].as_array().unwrap().iter().find(|x| x["name"] == n).unwrap();
            assert_eq!(subject["digest"]["sha256"], sha.as_str(), "{n}");
            assert_eq!(get(&state, &url(1, n)).await.0, StatusCode::OK, "{n} after the manifest");
        }
        assert_eq!(get(&state, &url(1, "report.pdf")).await.0, StatusCode::NOT_FOUND);
        assert_eq!(get(&state, &url(1, "studies/st9/events.json")).await.0, StatusCode::NOT_FOUND);
        // Revision 2 is pending but not yet opened.
        assert_eq!(get(&state, &url(2, "run.json")).await.0, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn a_run_that_has_not_ended_is_unknown_or_conflicts_and_a_swept_study_is_refused() {
        let state = AppState::new("agent".to_string());
        assert_eq!(send(&state, "/test-runs/nope/report-data", "").await.0, StatusCode::NOT_FOUND);
        let log = finished(&state);
        std::fs::remove_dir_all(log.run_dir("st2")).unwrap();
        let (s, b) = send(&state, "/test-runs/run-1/report-data", "").await;
        assert_eq!(s, StatusCode::CONFLICT);
        assert!(b.as_str().unwrap().contains("st2"), "{b}");
        let state = AppState::new("agent".to_string());
        std::fs::create_dir_all(state.run_log.data_dir().join("test-runs")).unwrap();
        std::fs::write(state.run_log.data_dir().join("test-runs/run-9.json"), json!({ "run_id": "run-9", "run_hash": "x", "run": run_json() }).to_string()).unwrap();
        assert_eq!(send(&state, "/test-runs/run-9/report-data", "").await.0, StatusCode::CONFLICT);
    }
}
