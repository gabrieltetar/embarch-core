//! A prepared Test run and its go card (decision 134): `POST /test-runs` takes
//! `run.json` with the hashes of the definitions it was expanded from, re-expands
//! it through `embarch-study-designer`'s `hil_expand::expand` from what Core keeps
//! (decision 133), and registers a prepared run under its id only when Core
//! reproduces it. `GET /test-runs/{run_id}/card` renders that run's card from
//! Core's record, and carries a go nonce for the person's token only.
//!
//! Preparing starts nothing, powers nothing and takes no bench. `POST /test-runs/{run_id}/go`
//! spends the nonce and moves the session's lease to the run (decision 135); the executor
//! that runs the studies (EMB-366) is not here: a started run stays so until [`end_run`].

use axum::{
    extract::{Path as UrlPath, State},
    http::StatusCode,
    Json,
};
use embarch_study_designer::hil_check::energises;
use embarch_study_designer::hil_expand::{
    duration_bound, expand, Confirmation, DutIdentity, ExpandContext, Expansion, RunJson, RunTest, SuiteContent, Surface,
};
use embarch_study_designer::hil_test::{definition_hash, derived_diff, Capability, HookMapping, Plan, SuiteBench, SuiteManifest, TestDef};
use embarch_study_designer::host_study::{HostAction, HostStep, HostStudy};
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashSet};
use std::path::PathBuf;

use crate::api::AppState;
use crate::definitions::{self, Definitions, Standing};
use crate::token_store::Scope;

type Reply = Result<(StatusCode, Json<Value>), (StatusCode, String)>;

/// The folder under the store a prepared run is kept in.
const TEST_RUNS_DIR: &str = "test-runs";
/// A run id is a file name; this bounds it. Chosen, not measured.
const MAX_RUN_ID_LEN: usize = 128;
/// The keys an array element is named by in a refusal, first present wins.
const LABEL_KEYS: &[&str] = &["test_id", "measurement", "name", "part", "step"];

/// `POST /test-runs`' body.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PrepareRequest {
    pub run: RunJson,
    /// The hash of the test plan, a kept `plan` definition.
    pub plan: String,
    /// The hash of the kept suite manifest; without it the plan's suite id and
    /// version name a premade suite of Core's checkout.
    #[serde(default)]
    pub suite: Option<String>,
    /// The suite's study files by `study.file`. Core has no copy of a study to
    /// check these against (decision 134, open).
    #[serde(default)]
    pub studies: BTreeMap<String, HostStudy>,
}

fn internal(e: anyhow::Error) -> (StatusCode, String) {
    (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}"))
}

fn refuse(why: impl Into<String>) -> (StatusCode, String) {
    (StatusCode::UNPROCESSABLE_ENTITY, why.into())
}

async fn blocking<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> Result<T, (StatusCode, String)> {
    tokio::task::spawn_blocking(f).await.map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))
}

/// A string somebody other than Core wrote (a definition, a study, `run.json`),
/// marked so a surface escapes it rather than rendering it.
fn agent_text(s: &str) -> Value {
    json!({ "agent_text": s })
}

fn agent_opt(s: &Option<String>) -> Value {
    s.as_deref().map(agent_text).unwrap_or(Value::Null)
}

/// A free value somebody other than Core wrote (a capability's value, a hook's
/// `{"shell": cmd}`, a derived test's field), with every string inside it marked
/// as `agent_text`. Object keys stay plain; numbers, booleans and null pass.
fn agent_value(v: &Value) -> Value {
    match v {
        Value::String(s) => agent_text(s),
        Value::Array(a) => Value::Array(a.iter().map(agent_value).collect()),
        Value::Object(o) => Value::Object(o.iter().map(|(k, v)| (k.clone(), agent_value(v))).collect()),
        other => other.clone(),
    }
}

/// Whether `id` is safe as a file name.
fn is_run_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= MAX_RUN_ID_LEN && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

// ---- the first differing key -------------------------------------------------------

fn label(v: &Value, i: usize) -> String {
    LABEL_KEYS.iter().find_map(|k| v.get(*k).and_then(Value::as_str)).map(str::to_string).unwrap_or_else(|| i.to_string())
}

fn show(v: &Value) -> String {
    let s = v.to_string();
    if s.chars().count() > 80 {
        format!("{}...", s.chars().take(80).collect::<String>())
    } else {
        s
    }
}

/// The first key (in sorted order) at which `sent` and `core` differ, as a path
/// naming each array element by its `test_id`, `measurement` or `name`.
fn first_diff(sent: &Value, core: &Value, path: &str) -> Option<String> {
    match (sent, core) {
        (Value::Object(a), Value::Object(b)) => {
            let keys: std::collections::BTreeSet<&String> = a.keys().chain(b.keys()).collect();
            keys.into_iter().find_map(|k| {
                let at = if path.is_empty() { k.to_string() } else { format!("{path}.{k}") };
                match (a.get(k), b.get(k)) {
                    (Some(x), Some(y)) => first_diff(x, y, &at),
                    (Some(x), None) => Some(format!("{at}: sent {}, Core's expansion has no such key", show(x))),
                    (None, Some(y)) => Some(format!("{at}: not sent, Core's expansion has {}", show(y))),
                    (None, None) => None,
                }
            })
        }
        (Value::Array(a), Value::Array(b)) => {
            for i in 0..a.len().max(b.len()) {
                match (a.get(i), b.get(i)) {
                    (Some(x), Some(y)) => {
                        if let Some(d) = first_diff(x, y, &format!("{path}[{}]", label(x, i))) {
                            return Some(d);
                        }
                    }
                    (Some(x), None) => return Some(format!("{path}[{}]: sent, Core's expansion has no such element", label(x, i))),
                    (None, Some(y)) => return Some(format!("{path}[{}]: not sent, Core's expansion has it", label(y, i))),
                    (None, None) => {}
                }
            }
            None
        }
        (x, y) if x == y => None,
        (x, y) => Some(format!("{path}: sent {}, Core's expansion has {}", show(x), show(y))),
    }
}

/// `run.json` with every `confirmed` blanked: a confirmation is Core's record,
/// which the caller cannot write and has no route to read, so it is not part of
/// what the caller's copy must reproduce.
fn without_confirmations(run: &RunJson) -> RunJson {
    let mut r = run.clone();
    r.dut.capabilities.iter_mut().for_each(|c| c.confirmed = None);
    r.dut.hooks.iter_mut().for_each(|h| h.confirmed = None);
    r
}

// ---- resolving what the request names ------------------------------------------------

/// A kept definition of `kind`, parsed.
fn kept<T: serde::de::DeserializeOwned>(defs: &Definitions, hash: &str, kind: &str) -> Result<T, (StatusCode, String)> {
    let stored = defs.get(hash).map_err(internal)?.ok_or_else(|| refuse(format!("Core holds no definition {hash}: post it to POST /definitions first")))?;
    if stored["kind"] != kind {
        return Err(refuse(format!("{hash} is a {}, not a {kind}", stored["kind"].as_str().unwrap_or("?"))));
    }
    serde_json::from_value(stored["definition"].clone()).map_err(|e| refuse(format!("the kept {kind} {hash} no longer parses: {e}")))
}

/// What the suite is made of, as Core resolves it.
struct Resolved {
    manifest: SuiteManifest,
    manifest_hash: String,
    plan: Plan,
    plan_hash: String,
    tests: Vec<TestDef>,
}

fn resolve(defs: &Definitions, req: &PrepareRequest) -> Result<Resolved, (StatusCode, String)> {
    if !definitions::is_hash(&req.plan) {
        return Err((StatusCode::BAD_REQUEST, "plan must be a 64-character lowercase hex hash".into()));
    }
    let plan: Plan = kept(defs, &req.plan, "plan")?;
    let manifest: SuiteManifest = match &req.suite {
        Some(h) if !definitions::is_hash(h) => return Err((StatusCode::BAD_REQUEST, "suite must be a 64-character lowercase hex hash".into())),
        Some(h) => kept(defs, h, "suite")?,
        None => defs.premade_suite(&plan.suite.id, &plan.suite.version).ok_or_else(|| {
            refuse(format!("Core reads no premade suite {}@{} from its checkout; name a kept suite manifest in `suite`", plan.suite.id, plan.suite.version))
        })?,
    };
    let mut tests = Vec::new();
    for line in &manifest.tests {
        let def: TestDef = match defs.get(&line.hash).map_err(internal)? {
            Some(v) if v["kind"] == "test" => serde_json::from_value(v["definition"].clone()).map_err(|e| refuse(format!("the kept test {} no longer parses: {e}", line.hash)))?,
            _ => defs.premade_test(&line.hash).ok_or_else(|| {
                refuse(format!("Core holds no test {} ({}): neither kept nor premade; post it to POST /definitions first", line.test_id, line.hash))
            })?,
        };
        if def.hash() != line.hash {
            return Err(refuse(format!("test {} does not hash to the manifest's line ({})", line.test_id, line.hash)));
        }
        tests.push(def);
    }
    Ok(Resolved { manifest_hash: manifest.hash(), plan_hash: definition_hash(&plan), manifest, plan, tests })
}

/// Core's own record of what was confirmed about the plan's DUT inputs.
fn confirmations(defs_log: &[(String, u64, String)], plan: &Plan) -> Vec<Confirmation> {
    let latest = |hash: &str| defs_log.iter().filter(|(h, _, _)| h == hash).max_by_key(|(_, at, _)| *at);
    let surface = |s: &str| if s == "ui" { Surface::Ui } else { Surface::Cli };
    let mut out = Vec::new();
    for c in &plan.dut.capabilities {
        if let Some((hash, at, s)) = latest(&definition_hash(c)) {
            out.push(Confirmation { scope: format!("capability:{}", c.name), hash: hash.clone(), at_utc_ms: *at, surface: surface(s) });
        }
    }
    for h in &plan.dut.hooks {
        if let Some((hash, at, s)) = latest(&definition_hash(h)) {
            out.push(Confirmation { scope: format!("hook:{}", h.hook), hash: hash.clone(), at_utc_ms: *at, surface: surface(s) });
        }
    }
    out
}

// ---- the card ------------------------------------------------------------------------------

fn prompts(steps: &[HostStep], list: &str, out: &mut Vec<Value>) {
    for s in steps {
        match &s.action {
            HostAction::Prompt { text, kind, bench_config, .. } => out.push(json!({
                "list": list, "step": agent_text(&s.name), "text": agent_text(text),
                "kind": kind, "bench_config": agent_opt(bench_config), "timeout_ms": s.timeout_ms,
            })),
            HostAction::Repeat { steps, .. } => prompts(steps, list, out),
            _ => {}
        }
    }
}

/// Every step that energises, by the crate's `energises`, with the line it sends.
fn energising(steps: &[HostStep], bench: &SuiteBench, list: &str, out: &mut Vec<Value>) {
    for s in steps {
        match &s.action {
            HostAction::Repeat { steps, .. } => energising(steps, bench, list, out),
            HostAction::Exchange { signal: target, line, .. } | HostAction::Instrument { instrument: target, line, .. } if energises(s, bench) => {
                out.push(json!({ "list": list, "step": agent_text(&s.name), "target": agent_text(target), "line": agent_text(line) }));
            }
            _ => {}
        }
    }
}

fn limits(rt: &RunTest) -> Vec<Value> {
    rt.limits
        .iter()
        .map(|l| {
            let src = rt.sources.iter().find(|s| s.measurement == l.measurement);
            let (u, k) = (src.map(|s| s.u), src.map(|s| s.k));
            json!({
                "measurement": agent_text(&l.measurement), "unit": agent_opt(&l.unit),
                "lower": l.lower, "upper": l.upper, "expect": agent_opt(&l.expect),
                "U": u, "k": k, "w": u.map(|u| u * rt.decision_rule.w_multiple),
            })
        })
        .collect()
}

/// A test's origin on the card: premade (identical to a test Core reads), a copy
/// of one with its diff, or custom.
fn origin(defs: &Definitions, def: &TestDef, hash: &str) -> (&'static str, Option<Value>) {
    if defs.standing("test", hash, &HashSet::new()) == Standing::Premade {
        return ("premade", None);
    }
    match def.derived_from.as_deref() {
        None => ("custom", None),
        Some(from) => match defs.premade_original(from) {
            Some(original) => {
                let fields: Vec<Value> = derived_diff(def, &original)
                    .iter()
                    .map(|d| json!({ "field": agent_text(&d.field), "old": agent_value(&d.old), "new": agent_value(&d.new) }))
                    .collect();
                ("derived", Some(json!({ "from": agent_text(from), "fields": fields })))
            }
            None => ("derived", Some(json!({ "from": agent_text(from), "unavailable": "Core holds no premade test of that id" }))),
        },
    }
}

/// The part of the card that does not change while the run waits: everything
/// `run.json` and the definitions decide. What is unconfirmed is read live.
fn static_card(defs: &Definitions, res: &Resolved, exp: &Expansion, studies: &BTreeMap<String, HostStudy>) -> Value {
    let run = &exp.run;
    let by_def: BTreeMap<String, &TestDef> = res.tests.iter().map(|t| (t.hash(), t)).collect();
    let mut tests = Vec::new();
    for rt in &run.tests {
        let study = exp.studies.iter().find(|s| s.test_id == rt.test_id).map(|s| &s.study).or_else(|| studies.get(&rt.study.file));
        let bench = res.manifest.benches.iter().find(|b| b.name == rt.bench.name);
        let (mut asks, mut powers) = (Vec::new(), Vec::new());
        if let (Some(study), Some(bench)) = (study, bench) {
            prompts(&study.steps, "steps", &mut asks);
            prompts(&study.teardown, "teardown", &mut asks);
            energising(&study.steps, bench, "steps", &mut powers);
            energising(&study.teardown, bench, "teardown", &mut powers);
        }
        let (orig, diff) = by_def.get(&rt.definition_hash).map(|d| origin(defs, d, &rt.definition_hash)).unwrap_or(("custom", None));
        tests.push(json!({
            "test_id": agent_text(&rt.test_id), "title": agent_text(&rt.title), "definition_hash": rt.definition_hash,
            "origin": orig, "diff": diff, "requirements": rt.requirements.iter().map(|r| agent_text(r)).collect::<Vec<_>>(),
            "precondition": rt.precondition, "run_count": rt.run_count, "step_budget_ms": rt.step_budget_ms,
            "prompts": asks, "energising": powers, "limits": limits(rt),
            "decision_rule": { "kind": rt.decision_rule.kind, "w_multiple": rt.decision_rule.w_multiple },
            "setup_terms": rt.setup_terms.iter().map(|t| json!({ "name": agent_text(&t.name), "unit": agent_text(&t.unit), "U": t.u, "k": t.k })).collect::<Vec<_>>(),
            "study": { "file": agent_text(&rt.study.file), "hash": rt.study.hash, "bytes_checked_against_hash": false },
        }));
    }
    let benches: Vec<Value> = run.benches.iter().map(|b| json!({ "name": agent_text(&b.name), "tests": b.tests.iter().map(|t| agent_text(t)).collect::<Vec<_>>() })).collect();
    json!({
        "suite": { "id": agent_text(&run.suite.id), "version": agent_text(&run.suite.version) },
        "standard": { "name": agent_text(&run.standard.name) },
        "dut": {
            "declared_serial": agent_text(&run.dut.declared_serial), "board_revision": agent_text(&run.dut.board_revision),
            "capabilities": run.dut.capabilities.iter().map(|c| json!({ "name": agent_text(&c.name), "value": agent_value(&c.value), "provenance": c.provenance })).collect::<Vec<_>>(),
            "hooks": run.dut.hooks.iter().map(|h| json!({ "name": agent_text(&h.name), "maps_to": agent_value(&h.maps_to) })).collect::<Vec<_>>(),
        },
        "stop_on_fail": run.options.stop_on_fail,
        "benches": benches, "tests": tests, "duration_bound_ms": duration_bound(run),
    })
}

/// The hashes a person may still owe a confirmation on, by kind and name.
fn inputs(res: &Resolved) -> Vec<Value> {
    let mut v = vec![json!({ "kind": "suite", "name": format!("{}@{}", res.manifest.id, res.manifest.version), "hash": res.manifest_hash })];
    v.extend(res.plan.dut.capabilities.iter().map(|c: &Capability| json!({ "kind": "capability", "name": c.name, "hash": definition_hash(c) })));
    v.extend(res.plan.dut.hooks.iter().map(|h: &HookMapping| json!({ "kind": "hook", "name": h.hook, "hash": definition_hash(h) })));
    v.extend(res.tests.iter().map(|t| json!({ "kind": "test", "name": t.test_id, "hash": t.hash() })));
    v
}

fn run_path(store: &std::path::Path, run_id: &str) -> PathBuf {
    store.join(TEST_RUNS_DIR).join(format!("{run_id}.json"))
}

// ---- the routes ----------------------------------------------------------------------------------

/// Re-expands `run.json` from what Core keeps and registers a prepared run under
/// its id; `201`. A `run.json` Core does not reproduce is `422` naming the first
/// differing key; a reused `run_id` is `409`. Nothing is started or powered.
// route: POST /test-runs
pub async fn prepare_handler(State(state): State<AppState>, Json(req): Json<PrepareRequest>) -> Reply {
    if !is_run_id(&req.run.run_id) {
        return Err((StatusCode::BAD_REQUEST, format!("run_id must be 1 to {MAX_RUN_ID_LEN} characters of letters, digits, `-` and `_`")));
    }
    let (defs, log) = (state.definitions.clone(), state.run_log.clone());
    let holder = state.lease.view().map(|v| v.holder);
    blocking(move || -> Reply {
        let path = run_path(log.data_dir(), &req.run.run_id);
        if path.exists() {
            return Err((StatusCode::CONFLICT, format!("run {} is already prepared: a run id is used once", req.run.run_id)));
        }
        let res = resolve(&defs, &req)?;
        if let Some(h) = &req.run.plan_hash {
            if *h != res.plan_hash {
                return Err(refuse(format!("plan_hash: sent {h}, the plan named hashes to {}", res.plan_hash)));
            }
        }
        let ctx = ExpandContext {
            run_id: req.run.run_id.clone(),
            plan_hash: req.run.plan_hash.clone(),
            standard: req.run.standard.clone(),
            identity: DutIdentity {
                hardware_id: req.run.dut.hardware_id.clone(),
                declared_serial: req.run.dut.declared_serial.clone(),
                board_revision: req.run.dut.board_revision.clone(),
            },
            resumes: req.run.resumes.clone(),
            // Core has no saved-bench list yet: the benches the caller's run uses
            // are the ones taken to exist (decision 134, assumed).
            benches_present: req.run.benches.iter().map(|b| b.name.clone()).collect(),
            confirmations: confirmations(&log.confirmations().map_err(internal)?, &res.plan),
        };
        let content = SuiteContent { manifest: &res.manifest, tests: &res.tests, studies: &req.studies };
        let exp = expand(&content, &res.plan, &res.plan.dut, &ctx).map_err(|e| refuse(format!("the definitions do not expand: {e:?}")))?;
        let (sent, core) = (without_confirmations(&req.run), without_confirmations(&exp.run));
        if sent.hash() != core.hash() {
            let (a, b) = (serde_json::to_value(&sent).unwrap_or_default(), serde_json::to_value(&core).unwrap_or_default());
            let at = first_diff(&a, &b, "").unwrap_or_else(|| "the canonical forms differ".into());
            return Err(refuse(format!("run.json is not what Core expands from its confirmed definitions and inputs; first difference at {at}")));
        }
        let run_hash = exp.run.hash();
        let record = json!({
            "run_id": exp.run.run_id, "run_hash": run_hash, "submitted_hash": req.run.hash(),
            "run": exp.run, "plan_hash": res.plan_hash, "suite_hash": res.manifest_hash,
            "lease_holder": holder, "prepared_utc_ms": crate::study::current_utc_ms(),
            "card": static_card(&defs, &res, &exp, &req.studies), "inputs": inputs(&res),
        });
        write_new(&path, &record).map_err(|e| match e.kind() {
            std::io::ErrorKind::AlreadyExists => (StatusCode::CONFLICT, format!("run {} is already prepared: a run id is used once", req.run.run_id)),
            _ => internal(e.into()),
        })?;
        Ok((StatusCode::CREATED, Json(json!({
            "run_id": exp.run.run_id, "run_hash": run_hash, "lease_holder": holder,
            "duration_bound_ms": duration_bound(&exp.run),
        }))))
    })
    .await?
}

/// Writes `record` to `path` whole, failing with `AlreadyExists` if `path` is
/// taken: written beside it and hard-linked into place, so two prepares of one
/// id cannot both succeed and a crash leaves no half file at the id.
fn write_new(path: &std::path::Path, record: &Value) -> std::io::Result<()> {
    let dir = path.parent().unwrap_or(std::path::Path::new("."));
    std::fs::create_dir_all(dir)?;
    let tmp = dir.join(format!("{}.{}.tmp", path.file_stem().and_then(|s| s.to_str()).unwrap_or("run"), std::process::id()));
    std::fs::write(&tmp, serde_json::to_vec(record)?)?;
    let linked = std::fs::hard_link(&tmp, path);
    let _ = std::fs::remove_file(&tmp);
    linked
}

/// The prepared run's card from Core's record, with every input still
/// unconfirmed as the log stands now. The person's token also gets a go nonce
/// for this card, single use, ten minutes; the agent's gets none.
// route: GET /test-runs/{run_id}/card
pub async fn card_handler(State(state): State<AppState>, UrlPath(run_id): UrlPath<String>, scope: Scope) -> Reply {
    if !is_run_id(&run_id) {
        return Err((StatusCode::NOT_FOUND, format!("Core has no prepared run {run_id}")));
    }
    let (defs, log) = (state.definitions.clone(), state.run_log.clone());
    blocking(move || -> Reply {
        let bytes = std::fs::read(run_path(log.data_dir(), &run_id)).map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => (StatusCode::NOT_FOUND, format!("Core has no prepared run {run_id}")),
            _ => internal(e.into()),
        })?;
        let rec: Value = serde_json::from_slice(&bytes).map_err(|e| internal(e.into()))?;
        let confirmed = log.confirmed_hashes().map_err(internal)?;
        let unconfirmed: Vec<Value> = rec["inputs"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|i| defs.standing(i["kind"].as_str().unwrap_or(""), i["hash"].as_str().unwrap_or(""), &confirmed) == Standing::Unconfirmed)
            .map(|i| json!({ "kind": i["kind"], "name": agent_text(i["name"].as_str().unwrap_or("")), "hash": i["hash"] }))
            .collect();
        let mut card = rec["card"].clone();
        card["unconfirmed"] = unconfirmed.into();
        let run_hash = rec["run_hash"].as_str().unwrap_or("");
        let now = crate::study::current_utc_ms();
        let (nonce, expires) = if scope == Scope::Person {
            (Some(defs.issue_nonce(&go_key(&run_id, run_hash), now)), Some(now + definitions::NONCE_TTL_MS))
        } else {
            (None, None)
        };
        Ok((StatusCode::OK, Json(json!({
            "run_id": run_id, "run_hash": run_hash, "lease_holder": rec["lease_holder"], "prepared_utc_ms": rec["prepared_utc_ms"],
            "card": card, "nonce": nonce, "expires_utc_ms": expires,
        }))))
    })
    .await?
}

/// `POST /test-runs/{run_id}/go`'s body: what the card issued, which Core checks against its record.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GoRequest {
    pub run_id: String,
    pub run_hash: String,
    pub nonce: String,
}

fn conflict(why: impl Into<String>) -> (StatusCode, String) {
    (StatusCode::CONFLICT, why.into())
}

fn read_record(store: &std::path::Path, run_id: &str) -> Result<Value, (StatusCode, String)> {
    let bytes = std::fs::read(run_path(store, run_id)).map_err(|e| match e.kind() {
        std::io::ErrorKind::NotFound => (StatusCode::NOT_FOUND, format!("Core has no prepared run {run_id}")),
        _ => internal(e.into()),
    })?;
    serde_json::from_slice(&bytes).map_err(|e| internal(e.into()))
}

/// Where a go leaves its mark: written once, so a run starts once however many cards were issued.
fn started_path(store: &std::path::Path, run_id: &str) -> PathBuf {
    store.join(TEST_RUNS_DIR).join(format!("{run_id}.started.json"))
}

/// The inputs a prepared run lists that are still unconfirmed, read from the log as it stands now.
fn unconfirmed_inputs(defs: &Definitions, log: &crate::run_log::RunLog, rec: &Value) -> Result<Vec<String>, (StatusCode, String)> {
    let confirmed = log.confirmed_hashes().map_err(internal)?;
    Ok(rec["inputs"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|i| defs.standing(i["kind"].as_str().unwrap_or(""), i["hash"].as_str().unwrap_or(""), &confirmed) == Standing::Unconfirmed)
        .map(|i| format!("{} {}", i["kind"].as_str().unwrap_or("?"), i["name"].as_str().unwrap_or("?")))
        .collect())
}

/// Starts a prepared run on the person's go and moves the session's lease to it (decision 135).
/// `403` to the agent's token (`person_middleware`). Refused, naming why: `422` for a body that
/// is not the card's (`mismatched`), or a nonce that is `replayed` or `expired`; `409` for a run
/// already started, an input still unconfirmed, or a session that no longer holds the lease.
/// The go does not re-expand: the run keeps the hash it was prepared under, and a confirmation
/// made since is read live, not folded into `run.json`.
// route: POST /test-runs/{run_id}/go
pub async fn go_handler(State(state): State<AppState>, UrlPath(run_id): UrlPath<String>, scope: Scope, Json(req): Json<GoRequest>) -> Reply {
    if scope != Scope::Person {
        return Err((StatusCode::FORBIDDEN, crate::token_store::GO_REFUSAL.to_string()));
    }
    if !is_run_id(&run_id) {
        return Err((StatusCode::NOT_FOUND, format!("Core has no prepared run {run_id}")));
    }
    let (defs, log, lease) = (state.definitions.clone(), state.run_log.clone(), state.lease.clone());
    blocking(move || -> Reply {
        let rec = read_record(log.data_dir(), &run_id)?;
        let run_hash = rec["run_hash"].as_str().unwrap_or("").to_string();
        if req.run_id != run_id {
            return Err(refuse(format!("mismatched: the body names run {}, the route run {run_id}", req.run_id)));
        }
        if req.run_hash != run_hash {
            return Err(refuse(format!("mismatched: the body carries run hash {}, Core holds {run_hash} for run {run_id}", req.run_hash)));
        }
        if started_path(log.data_dir(), &run_id).exists() {
            return Err(conflict(format!("run {run_id} was already started: a run is started once, prepare another")));
        }
        let owed = unconfirmed_inputs(&defs, &log, &rec)?;
        if !owed.is_empty() {
            return Err(conflict(format!("not started: still unconfirmed: {}", owed.join(", "))));
        }
        let prepared_by = rec["lease_holder"].as_str().map(str::to_string);
        // The nonce is spent only when nothing else would refuse the go.
        lease.can_start_run(prepared_by.as_deref()).map_err(|e| conflict(format!("not started: {}", e.why())))?;
        if let Err(why) = defs.spend_nonce(&req.nonce, &go_key(&run_id, &run_hash), crate::study::current_utc_ms()) {
            let named = if why.contains("expired") { "expired" } else if why.contains("another card") { "mismatched" } else { "replayed" };
            return Err(refuse(format!("{named}: {why}")));
        }
        lease.start_run(&run_id, prepared_by.as_deref()).map_err(|e| conflict(format!("not started: {}", e.why())))?;
        let now = crate::study::current_utc_ms();
        let mark = json!({ "run_id": run_id, "run_hash": run_hash, "started_utc_ms": now, "go_by": scope.as_str() });
        if let Err(e) = write_new(&started_path(log.data_dir(), &run_id), &mark) {
            lease.end_run(&run_id);
            return Err(match e.kind() {
                std::io::ErrorKind::AlreadyExists => conflict(format!("run {run_id} was already started")),
                _ => internal(e.into()),
            });
        }
        let session = prepared_by.unwrap_or_default();
        if let Err(e) = log.test_run_started(&run_id, &run_hash, scope, &session) {
            tracing::error!(run_id, "could not enter the run's start in the run log: {e:#}");
        }
        Ok((StatusCode::OK, Json(json!({ "run_id": run_id, "run_hash": run_hash, "state": "started", "started_utc_ms": now, "lease_holder": session }))))
    })
    .await?
}

/// Ends a started run (decision 135): releases the run lock, gives the lease back to its session
/// under the old token with its TTL restarted, and records the end. `by` is the cancelling
/// token's scope, `None` when the executor (EMB-366) ends it. `false` when `run_id` is not the run
/// in flight. The seam the executor calls when its studies are done.
pub fn end_run(state: &AppState, run_id: &str, status: &str, reason: Option<&str>, by: Option<Scope>) -> bool {
    if !state.lease.end_run(run_id) {
        return false;
    }
    if let Err(e) = state.run_log.test_run_ended(run_id, status, reason, by) {
        tracing::error!(run_id, "could not enter the run's end in the run log: {e:#}");
    }
    true
}

/// Cancels the run in flight, from either token (`403` to neither); not lease-gated, so the
/// run's hold does not refuse it. `409` when `run_id` is not started. Recorded with the token's scope.
// route: POST /test-runs/{run_id}/cancel
pub async fn cancel_handler(State(state): State<AppState>, UrlPath(run_id): UrlPath<String>, scope: Scope) -> Reply {
    if end_run(&state, &run_id, "cancelled", Some("cancelled by request"), Some(scope)) {
        return Ok((StatusCode::OK, Json(json!({ "run_id": run_id, "state": "cancelled", "by": scope.as_str() }))));
    }
    match state.lease.run_in_flight() {
        Some(other) => Err(conflict(format!("run {run_id} is not the run in flight ({other} is): nothing was cancelled"))),
        None => Err(conflict(format!("run {run_id} is not started: nothing to cancel"))),
    }
}

/// What a go nonce is issued for: the run's id and `run.json`'s hash. Not a
/// definition hash, so a confirmation's nonce cannot spend it or the reverse.
pub fn go_key(run_id: &str, run_hash: &str) -> String {
    format!("go:{run_id}:{run_hash}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::build_router;
    use crate::definitions::tests::checkout;
    use axum::{body::Body, http::Request};
    use embarch_study_designer::hil_expand::Standard;
    use embarch_study_designer::hil_test::SuiteTest;
    use tower::ServiceExt as _;

    const TEST_JSON: &str = include_str!("../tests/fixtures/hil-tests/test.json");
    const STUDY_JSON: &str = include_str!("../tests/fixtures/hil-tests/study.json");
    const SUITE_JSON: &str = include_str!("../tests/fixtures/hil-tests/suite.json");
    const AGENT: &str = "test-token";
    const PERSON: &str = "person-test-token";

    fn state(name: &str) -> AppState {
        let mut s = AppState::new(AGENT.to_string());
        s.definitions = Definitions::new(s.run_log.data_dir(), Some(checkout(&format!("test-runs-{name}"))));
        s
    }

    async fn call(s: &AppState, bearer: &str, method: &str, uri: &str, body: Option<Value>) -> (StatusCode, Value) {
        let mut b = Request::builder().method(method).uri(uri).header("authorization", format!("Bearer {bearer}"));
        let body = match body {
            Some(v) => {
                b = b.header("content-type", "application/json");
                Body::from(v.to_string())
            }
            None => Body::empty(),
        };
        let r = build_router(s.clone()).oneshot(b.body(body).unwrap()).await.unwrap();
        let status = r.status();
        let bytes = axum::body::to_bytes(r.into_body(), 1 << 20).await.unwrap();
        let text = String::from_utf8_lossy(&bytes).into_owned();
        (status, serde_json::from_str(&text).unwrap_or(Value::String(text)))
    }

    /// What the caller would send: a custom suite of the fixture test (its
    /// `requires` cleared, so it stands alone), a plan over it, the study, and
    /// the `run.json` the crate expands from them.
    struct Fixture {
        plan: String,
        suite: String,
        run: RunJson,
        studies: BTreeMap<String, HostStudy>,
    }

    async fn post(s: &AppState, kind: &str, definition: Value) -> String {
        let (st, p) = call(s, AGENT, "POST", "/definitions", Some(json!({ "kind": kind, "definition": definition }))).await;
        assert!(st == StatusCode::CREATED || st == StatusCode::OK, "{kind}: {p}");
        p["hash"].as_str().unwrap().to_string()
    }

    async fn fixture(s: &AppState, run_id: &str) -> Fixture {
        let mut test: TestDef = serde_json::from_str(TEST_JSON).unwrap();
        test.requires.clear();
        let mut manifest: SuiteManifest = serde_json::from_str(SUITE_JSON).unwrap();
        manifest.tests = vec![SuiteTest { test_id: test.test_id.clone(), file: "test.json".into(), hash: test.hash() }];
        let plan: Plan = serde_json::from_value(json!({
            "suite": { "id": manifest.id, "version": manifest.version, "commit": "c0" },
            "select": { "kind": "all" },
            "dut": { "capabilities": [ { "name": "role", "value": "source", "provenance": "declared" } ], "hooks": [] },
            "stop_on_fail": false
        }))
        .unwrap();
        post(s, "test", serde_json::to_value(&test).unwrap()).await;
        let suite = post(s, "suite", serde_json::to_value(&manifest).unwrap()).await;
        let plan_hash = post(s, "plan", serde_json::to_value(&plan).unwrap()).await;
        let studies: BTreeMap<String, HostStudy> = [(test.study.file.clone(), serde_json::from_str(STUDY_JSON).unwrap())].into();
        let tests = [test];
        let ctx = ExpandContext {
            run_id: run_id.into(),
            plan_hash: Some(plan_hash.clone()),
            standard: Standard { name: "usb-pd".into(), revision: "3.2".into() },
            identity: DutIdentity { hardware_id: None, declared_serial: "unit-1".into(), board_revision: "A".into() },
            resumes: None,
            benches_present: vec!["pd-sink-bench".into()],
            confirmations: vec![],
        };
        let exp = expand(&SuiteContent { manifest: &manifest, tests: &tests, studies: &studies }, &plan, &plan.dut, &ctx).unwrap();
        Fixture { plan: plan_hash, suite, run: exp.run, studies }
    }

    fn body(f: &Fixture) -> Value {
        json!({ "run": f.run, "plan": f.plan, "suite": f.suite, "studies": f.studies })
    }

    #[tokio::test]
    async fn a_run_core_reproduces_is_registered_and_its_card_served() {
        let s = state("served");
        let f = fixture(&s, "run-1").await;
        s.lease.take("session-a", "a test", 60).unwrap();
        let (st, prep) = call(&s, AGENT, "POST", "/test-runs", Some(body(&f))).await;
        assert_eq!(st, StatusCode::CREATED, "{prep}");
        assert_eq!(prep["lease_holder"], "session-a");
        assert_eq!(prep["duration_bound_ms"], 186_000 * 3);

        let (st, card) = call(&s, AGENT, "GET", "/test-runs/run-1/card", None).await;
        assert_eq!(st, StatusCode::OK, "{card}");
        assert!(card["nonce"].is_null() && card["expires_utc_ms"].is_null(), "the agent's card carries no nonce");
        let c = &card["card"];
        assert_eq!(c["duration_bound_ms"], 558_000);
        assert_eq!(c["benches"][0]["name"]["agent_text"], "pd-sink-bench");
        let t = &c["tests"][0];
        assert_eq!(t["origin"], "custom");
        let kinds: Vec<&str> = t["prompts"].as_array().unwrap().iter().map(|p| p["kind"].as_str().unwrap()).collect();
        assert_eq!(kinds, ["setup", "clearance", "observation"]);
        assert_eq!(t["prompts"][0]["text"]["agent_text"], "Wire the partner to port 1");
        let lines: Vec<&str> = t["energising"].as_array().unwrap().iter().map(|e| e["line"]["agent_text"].as_str().unwrap()).collect();
        assert_eq!(lines, ["OUTP ON"], "the dmm query and the bench's read-only status do not energise");
        let l = &t["limits"][0];
        assert_eq!((l["measurement"]["agent_text"].clone(), l["lower"].clone(), l["upper"].clone()), ("vbus_mean".into(), 4.75.into(), 5.25.into()));
        assert!(l.get("U").is_some() && l.get("w").is_some() && l.get("k").is_some());
        assert_eq!(t["decision_rule"]["w_multiple"], 1.0);
        let names: Vec<(&str, &str)> =
            c["unconfirmed"].as_array().unwrap().iter().map(|u| (u["kind"].as_str().unwrap(), u["name"]["agent_text"].as_str().unwrap())).collect();
        assert_eq!(names, [("suite", "usb-pd-r3.2@0.1.0"), ("capability", "role"), ("test", "vbus-5v-source-voltage")]);
        assert_eq!(c["dut"]["capabilities"][0]["value"]["agent_text"], "source", "a capability's value is the agent's text");

        // The person's token: a nonce, valid once, for this run and hash.
        let (st, mine) = call(&s, PERSON, "GET", "/test-runs/run-1/card", None).await;
        assert_eq!(st, StatusCode::OK);
        let nonce = mine["nonce"].as_str().unwrap();
        let key = go_key("run-1", mine["run_hash"].as_str().unwrap());
        assert!(s.definitions.spend_nonce(nonce, "another key", crate::study::current_utc_ms()).is_err(), "it is for this run only, and a wrong try spends it");
        let again = call(&s, PERSON, "GET", "/test-runs/run-1/card", None).await.1;
        let now = crate::study::current_utc_ms();
        assert!(s.definitions.spend_nonce(again["nonce"].as_str().unwrap(), &key, now).is_ok());
        assert!(s.definitions.spend_nonce(again["nonce"].as_str().unwrap(), &key, now).is_err(), "single use");
        assert_eq!(call(&s, AGENT, "GET", "/test-runs/nope/card", None).await.0, StatusCode::NOT_FOUND);
        assert_eq!(call(&s, AGENT, "GET", "/test-runs/a.b/card", None).await.0, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn a_run_with_one_limit_edited_is_refused_naming_it_and_a_reused_id_is_refused() {
        let s = state("refused");
        let f = fixture(&s, "run-2").await;
        let mut edited = body(&f);
        edited["run"]["tests"][0]["limits"][0]["upper"] = 5.5.into();
        let (st, why) = call(&s, AGENT, "POST", "/test-runs", Some(edited)).await;
        assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY, "{why}");
        assert!(why.as_str().unwrap().contains("tests[vbus-5v-source-voltage].limits[vbus_mean].upper"), "{why}");
        assert_eq!(call(&s, AGENT, "GET", "/test-runs/run-2/card", None).await.0, StatusCode::NOT_FOUND, "a refused run is not registered");

        assert_eq!(call(&s, AGENT, "POST", "/test-runs", Some(body(&f))).await.0, StatusCode::CREATED);
        let (st, why) = call(&s, AGENT, "POST", "/test-runs", Some(body(&f))).await;
        assert_eq!(st, StatusCode::CONFLICT, "{why}");
        // A definition Core does not keep is named, not guessed.
        let mut unknown = body(&f);
        unknown["plan"] = "0".repeat(64).into();
        unknown["run"]["run_id"] = "run-3".into();
        let (st, why) = call(&s, AGENT, "POST", "/test-runs", Some(unknown)).await;
        assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY);
        assert!(why.as_str().unwrap().contains("holds no definition"), "{why}");
        let mut bad_id = body(&f);
        bad_id["run"]["run_id"] = "../x".into();
        assert_eq!(call(&s, AGENT, "POST", "/test-runs", Some(bad_id)).await.0, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn an_unconfirmed_input_is_listed_not_refused_and_a_confirmation_clears_it() {
        let s = state("unconfirmed");
        let f = fixture(&s, "run-4").await;
        let (st, _) = call(&s, AGENT, "POST", "/test-runs", Some(body(&f))).await;
        assert_eq!(st, StatusCode::CREATED);
        let cap = post(&s, "capability", json!({ "name": "role", "value": "source", "provenance": "declared" })).await;
        let (_, c) = call(&s, PERSON, "GET", &format!("/confirmations/{cap}/card"), None).await;
        let (st, e) = call(&s, PERSON, "POST", "/confirmations", Some(json!({ "hash": cap, "nonce": c["nonce"], "surface": "ui" }))).await;
        assert_eq!(st, StatusCode::CREATED, "{e}");
        let (_, card) = call(&s, AGENT, "GET", "/test-runs/run-4/card", None).await;
        let kinds: Vec<&str> = card["card"]["unconfirmed"].as_array().unwrap().iter().map(|u| u["kind"].as_str().unwrap()).collect();
        assert_eq!(kinds, ["suite", "test"], "the capability is confirmed now");
        // A run prepared after it still reproduces: `confirmed` is Core's record, not the caller's claim.
        let mut later = body(&f);
        later["run"]["run_id"] = "run-5".into();
        let (st, why) = call(&s, AGENT, "POST", "/test-runs", Some(later)).await;
        assert_eq!(st, StatusCode::CREATED, "{why}");
    }

    /// Confirms every input the card still lists, as the person, one at a time: confirming a
    /// suite confirms the tests it lists, so the list is read again after each.
    async fn confirm_all(s: &AppState, run_id: &str) {
        for _ in 0..10 {
            let (_, card) = call(s, PERSON, "GET", &format!("/test-runs/{run_id}/card"), None).await;
            let Some(i) = card["card"]["unconfirmed"].as_array().unwrap().first().cloned() else { return };
            let hash = i["hash"].as_str().unwrap();
            if i["kind"] == "capability" {
                // The plan names it; Core keeps it only once it is posted.
                post(s, "capability", json!({ "name": "role", "value": "source", "provenance": "declared" })).await;
            }
            let (_, c) = call(s, PERSON, "GET", &format!("/confirmations/{hash}/card"), None).await;
            let (st, e) = call(s, PERSON, "POST", "/confirmations", Some(json!({ "hash": hash, "nonce": c["nonce"], "surface": "ui" }))).await;
            assert_eq!(st, StatusCode::CREATED, "{e}");
        }
        panic!("inputs stayed unconfirmed");
    }

    /// A prepared, fully confirmed run under a lease: the state and the lease's token.
    async fn ready(name: &str, run_id: &str) -> (AppState, String) {
        let s = state(name);
        let f = fixture(&s, run_id).await;
        let token = s.lease.take("session-a", "a test", 600).unwrap().token;
        assert_eq!(call(&s, AGENT, "POST", "/test-runs", Some(body(&f))).await.0, StatusCode::CREATED);
        confirm_all(&s, run_id).await;
        (s, token)
    }

    /// The go's body from a fresh card: its own nonce, the card's hash.
    async fn go_body(s: &AppState, run_id: &str) -> Value {
        let (_, card) = call(s, PERSON, "GET", &format!("/test-runs/{run_id}/card"), None).await;
        json!({ "run_id": card["run_id"], "run_hash": card["run_hash"], "nonce": card["nonce"] })
    }

    async fn reset_with(s: &AppState, token: &str) -> (StatusCode, String) {
        let r = build_router(s.clone())
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/reset")
                    .header("authorization", format!("Bearer {AGENT}"))
                    .header(crate::lease::LEASE_HEADER, token)
                    .header("content-type", "application/json")
                    .body(Body::from("{}"))
                    .unwrap(),
            )
            .await
            .unwrap();
        let st = r.status();
        (st, String::from_utf8_lossy(&axum::body::to_bytes(r.into_body(), 1 << 20).await.unwrap()).into_owned())
    }

    #[tokio::test]
    async fn the_agent_cannot_go_and_a_replayed_expired_or_mismatched_go_names_which() {
        let (s, _) = ready("go-refusals", "run-g1").await;
        let uri = "/test-runs/run-g1/go";
        let good = go_body(&s, "run-g1").await;
        assert_eq!(call(&s, AGENT, "POST", uri, Some(good.clone())).await.0, StatusCode::FORBIDDEN, "the agent's go is refused");
        assert!(s.lease.run_in_flight().is_none());

        let mut wrong_hash = good.clone();
        wrong_hash["run_hash"] = "0".repeat(64).into();
        let (st, why) = call(&s, PERSON, "POST", uri, Some(wrong_hash)).await;
        assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY);
        assert!(why.as_str().unwrap().starts_with("mismatched"), "{why}");

        let mut other_card = good.clone();
        other_card["nonce"] = s.definitions.issue_nonce("go:another-run:abc", crate::study::current_utc_ms()).into();
        let (st, why) = call(&s, PERSON, "POST", uri, Some(other_card)).await;
        assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY);
        assert!(why.as_str().unwrap().starts_with("mismatched"), "{why}");

        let mut unknown = good.clone();
        unknown["nonce"] = "ab".repeat(32).into();
        let (_, why) = call(&s, PERSON, "POST", uri, Some(unknown)).await;
        assert!(why.as_str().unwrap().starts_with("replayed"), "{why}");

        // An expired nonce: issued a long time ago.
        let hash = good["run_hash"].as_str().unwrap();
        let old = s.definitions.issue_nonce(&go_key("run-g1", hash), 1_000);
        let mut expired = good.clone();
        expired["nonce"] = old.into();
        let (_, why) = call(&s, PERSON, "POST", uri, Some(expired)).await;
        assert!(why.as_str().unwrap().starts_with("expired"), "{why}");

        // None of that started it; the good go does, once, and a replay of it is refused.
        let (st, started) = call(&s, PERSON, "POST", uri, Some(good.clone())).await;
        assert_eq!(st, StatusCode::OK, "{started}");
        assert_eq!(s.lease.run_in_flight().as_deref(), Some("run-g1"));
        let (st, why) = call(&s, PERSON, "POST", uri, Some(good)).await;
        assert_eq!(st, StatusCode::CONFLICT, "{why}");
    }

    #[tokio::test]
    async fn a_go_is_refused_while_an_input_is_unconfirmed_or_the_session_lost_the_lease() {
        let s = state("go-state");
        let f = fixture(&s, "run-g2").await;
        let token = s.lease.take("session-a", "a test", 600).unwrap().token;
        call(&s, AGENT, "POST", "/test-runs", Some(body(&f))).await;
        let go = go_body(&s, "run-g2").await;
        let (st, why) = call(&s, PERSON, "POST", "/test-runs/run-g2/go", Some(go.clone())).await;
        assert_eq!(st, StatusCode::CONFLICT);
        assert!(why.as_str().unwrap().contains("unconfirmed"), "{why}");
        confirm_all(&s, "run-g2").await;
        // The refusal did not spend the nonce, and a released lease is refused by name.
        s.lease.release(Some(&token)).unwrap();
        let (st, why) = call(&s, PERSON, "POST", "/test-runs/run-g2/go", Some(go.clone())).await;
        assert_eq!(st, StatusCode::CONFLICT);
        assert!(why.as_str().unwrap().contains("no longer holds the lease"), "{why}");
        s.lease.take("session-b", "x", 60).unwrap();
        let (_, why) = call(&s, PERSON, "POST", "/test-runs/run-g2/go", Some(go)).await;
        assert!(why.as_str().unwrap().contains("held by 'session-b'"), "{why}");
    }

    #[tokio::test]
    async fn a_run_holds_the_lease_refuses_the_session_and_gives_it_back_when_it_ends() {
        let (s, token) = ready("go-holds", "run-g3").await;
        assert_ne!(reset_with(&s, &token).await.0, StatusCode::CONFLICT);
        let go = go_body(&s, "run-g3").await;
        assert_eq!(call(&s, PERSON, "POST", "/test-runs/run-g3/go", Some(go)).await.0, StatusCode::OK);

        let (st, why) = reset_with(&s, &token).await;
        assert_eq!(st, StatusCode::CONFLICT);
        assert!(why.contains("run-g3"), "names the run: {why}");
        assert!(matches!(s.lease.release(Some(&token)), Err(crate::lease::LeaseError::Held(_))), "the session cannot release it either");
        let view = s.lease.view().unwrap();
        assert_eq!((view.holder.as_str(), view.run_id.as_deref()), ("test run run-g3", Some("run-g3")));
        // Reads of the run are open.
        assert_eq!(call(&s, AGENT, "GET", "/test-runs/run-g3/card", None).await.0, StatusCode::OK);

        assert!(end_run(&s, "run-g3", "completed", None, None));
        assert_ne!(reset_with(&s, &token).await.0, StatusCode::CONFLICT, "the old token is admitted again");
        assert!(!end_run(&s, "run-g3", "completed", None, None), "it ended once");
    }

    #[tokio::test]
    async fn a_cancel_from_either_token_ends_the_run_and_records_its_scope() {
        for (bearer, scope) in [(AGENT, "agent"), (PERSON, "person")] {
            let id = format!("run-c-{scope}");
            let (s, token) = ready(&format!("cancel-{scope}"), &id).await;
            assert_eq!(call(&s, AGENT, "POST", &format!("/test-runs/{id}/cancel"), None).await.0, StatusCode::CONFLICT, "not started yet");
            let go = go_body(&s, &id).await;
            assert_eq!(call(&s, PERSON, "POST", &format!("/test-runs/{id}/go"), Some(go)).await.0, StatusCode::OK);
            let (st, r) = call(&s, bearer, "POST", &format!("/test-runs/{id}/cancel"), None).await;
            assert_eq!(st, StatusCode::OK, "{r}");
            assert!(s.lease.run_in_flight().is_none());
            assert_ne!(reset_with(&s, &token).await.0, StatusCode::CONFLICT);
            let log = s.run_log.lines().unwrap();
            let ended = log.iter().find(|l| l["entry"]["type"] == "test_run_ended").unwrap();
            assert_eq!((ended["entry"]["status"].as_str(), ended["entry"]["scope"].as_str()), (Some("cancelled"), Some(scope)));
            let started = log.iter().find(|l| l["entry"]["type"] == "test_run_started").unwrap();
            assert_eq!((started["entry"]["scope"].as_str(), started["entry"]["session"].as_str()), (Some("person"), Some("session-a")));
            // A run is started once: a cancelled run does not start again.
            let go = go_body(&s, &id).await;
            assert_eq!(call(&s, PERSON, "POST", &format!("/test-runs/{id}/go"), Some(go)).await.0, StatusCode::CONFLICT);
        }
    }

    #[test]
    fn every_string_in_a_free_value_is_marked() {
        let hook = json!({ "shell": "reboot now" });
        assert_eq!(agent_value(&hook), json!({ "shell": { "agent_text": "reboot now" } }));
        let nested = json!([1, true, null, { "k": ["a"] }]);
        assert_eq!(agent_value(&nested), json!([1, true, null, { "k": [{ "agent_text": "a" }] }]));
    }

    #[test]
    fn a_run_id_is_a_safe_file_name() {
        assert!(is_run_id("run-01_a"));
        for bad in ["", "..", "a/b", "a.b", &"x".repeat(MAX_RUN_ID_LEN + 1)] {
            assert!(!is_run_id(bad), "{bad}");
        }
    }

    #[test]
    fn the_first_difference_names_the_limit() {
        let a = json!({ "tests": [ { "test_id": "t", "limits": [ { "measurement": "a", "upper": 1.0 }, { "measurement": "vbus_mean", "upper": 5.5 } ] } ] });
        let mut b = a.clone();
        b["tests"][0]["limits"][1]["upper"] = 5.25.into();
        let d = first_diff(&a, &b, "").unwrap();
        assert!(d.starts_with("tests[t].limits[vbus_mean].upper: sent 5.5"), "{d}");
        assert!(first_diff(&a, &a, "").is_none());
    }
}
