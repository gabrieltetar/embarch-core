//! Core's side of publish (decisions 116 to 120): adopting notes and prose,
//! the signed `report-data.json`, and the signed manifest, for one study run,
//! and a swept run's files taken back.
//!
//! Nothing here takes a value from embarch-api except the list of the files
//! embarch-api made, which the manifest records as embarch-api's and never
//! vouches for. `report-data.json` is written from the run log, the signed
//! record and `events.json`, which Core holds.

use axum::{
    extract::{Path as UrlPath, State},
    http::{header, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use base64::Engine as _;
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::PathBuf;

use crate::api::AppState;
use crate::signing::{sha256_of_file, RECORD_FILE};
use crate::token_store::Scope;

/// What the manifest Statement says it is.
pub const MANIFEST_PREDICATE_TYPE: &str = "urn:embarch:predicate:publish-manifest:v1";
/// What the `report-data.json` Statement says it is.
pub const REPORT_DATA_PREDICATE_TYPE: &str = "urn:embarch:predicate:report-data:v1";
const STATEMENT_TYPE: &str = "https://in-toto.io/Statement/v1";

pub const REPORT_DATA_FILE: &str = "report-data.json";
pub const REPORT_DATA_ENVELOPE_FILE: &str = "report-data.dsse.json";
pub const SEGMENT_FILE: &str = "run-log-segment.jsonl";
pub const CALL_LOG_SEGMENT_FILE: &str = "call-log-segment.jsonl";
pub const MANIFEST_FILE: &str = "manifest.dsse.json";
/// The files Core writes into a revision's directory, which a reader may fetch
/// beside whatever the manifest names.
pub(crate) const PUBLISH_FILES: [&str; 5] =
    [REPORT_DATA_FILE, REPORT_DATA_ENVELOPE_FILE, SEGMENT_FILE, CALL_LOG_SEGMENT_FILE, MANIFEST_FILE];

/// The most text one adoption takes.
const MAX_ADOPTED_BYTES: usize = 1 << 20;
const MAX_NAME_BYTES: usize = 200;

pub(crate) type Failure = (StatusCode, String);

/// The stable `code` of the measurements route's `422` (core decision 141).
const NO_MEASUREMENTS_CODE: &str = "no_measurements";

pub(crate) fn internal(e: impl std::fmt::Display) -> Failure {
    (StatusCode::INTERNAL_SERVER_ERROR, format!("{e}"))
}

pub(crate) fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    crate::signing::hex(&Sha256::digest(bytes))
}

pub(crate) fn is_sha256_hex(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// A zip-relative name: printable ASCII, no `..`, no leading `/`, no `\`.
fn safe_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_NAME_BYTES
        && !name.starts_with('/')
        && !name.contains('\\')
        && name.bytes().all(|b| (0x21..0x7f).contains(&b))
        && name.split('/').all(|seg| !seg.is_empty() && seg != "." && seg != "..")
}

/// How a note or prose block is labelled for the token that entered it.
fn attribution(kind: &str, scope: &str) -> &'static str {
    match (kind, scope) {
        ("note", "agent") => "relayed by the agent",
        ("note", _) => "the person's",
        (_, "agent") => "entered with the agent's token",
        _ => "entered with the person's token",
    }
}

// ---- adopting notes and prose ------------------------------------------------

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AdoptRequest {
    /// `note` or `prose`.
    kind: String,
    /// The file the text becomes in the zip.
    name: String,
    text: String,
}

/// Records the scope of the calling token and the SHA-256 of a note or a prose
/// block for a run. Core keeps the hash, not the text.
// route: POST /study/{study_id}/adopt
pub async fn adopt_handler(
    State(state): State<AppState>,
    UrlPath(study_id): UrlPath<String>,
    scope: Scope,
    Json(req): Json<AdoptRequest>,
) -> Result<Json<serde_json::Value>, Failure> {
    if req.kind != "note" && req.kind != "prose" {
        return Err((StatusCode::BAD_REQUEST, "kind must be `note` or `prose`".into()));
    }
    if !safe_name(&req.name) {
        return Err((StatusCode::BAD_REQUEST, "name must be a relative file name of printable characters, 200 bytes at most".into()));
    }
    if req.text.len() > MAX_ADOPTED_BYTES {
        return Err((StatusCode::PAYLOAD_TOO_LARGE, format!("text is over {MAX_ADOPTED_BYTES} bytes")));
    }
    let log = state.run_log.clone();
    tokio::task::spawn_blocking(move || log.adopt(&study_id, &req.kind, &req.name, &req.text, scope))
        .await
        .map_err(internal)?
        .map(Json)
}

// ---- a run the retention sweep took (decision 120) ---------------------------

/// Where a handed-back manifest waits, inside the run's directory.
const RESTORED_DIR: &str = "restored";
/// The most one handed-back file takes.
const MAX_RESTORED_BYTES: usize = 1 << 30;

/// The Statements of the manifests handed back for this run, by revision.
fn staged_manifests(dir: &std::path::Path) -> Result<Vec<serde_json::Value>, Failure> {
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir(dir.join(RESTORED_DIR)) else { return Ok(out) };
    for e in rd {
        let bytes = std::fs::read(e.map_err(internal)?.path()).map_err(internal)?;
        let env: serde_json::Value = serde_json::from_slice(&bytes).map_err(internal)?;
        let payload = base64::engine::general_purpose::STANDARD.decode(env["payload"].as_str().unwrap_or("")).map_err(internal)?;
        out.push(serde_json::from_slice(&payload).map_err(internal)?);
    }
    out.sort_by_key(|st| st["predicate"]["revision"].as_u64().unwrap_or(0));
    Ok(out)
}

/// The run's Core files must be there, or have all been handed back: a swept
/// run is `409` naming what to restore, never a half-built manifest.
fn require_run_files(dir: &std::path::Path) -> Result<(), Failure> {
    let staged = staged_manifests(dir)?;
    let Some(latest) = staged.last() else {
        return if dir.join("events.json").is_file() {
            Ok(())
        } else {
            Err((
                StatusCode::CONFLICT,
                "the run's files are gone (the retention sweep took them): hand back the earlier revision's manifest, then its Core files, with PUT /study/{id}/restore/{name}".into(),
            ))
        };
    };
    let missing: Vec<&str> = latest["subject"]
        .as_array()
        .map(|a| a.as_slice())
        .unwrap_or(&[])
        .iter()
        .filter_map(|s| s["name"].as_str())
        .filter(|n| !PUBLISH_FILES.contains(n) && !dir.join(n).is_file())
        .collect();
    if missing.is_empty() {
        Ok(())
    } else {
        Err((StatusCode::CONFLICT, format!("the earlier manifest names files not yet handed back: {}", missing.join(", "))))
    }
}

fn refused(msg: String) -> Failure {
    (StatusCode::UNPROCESSABLE_ENTITY, msg)
}

/// Takes one file of the earlier revision back into the run's directory. The
/// manifest first, which Core accepts only if the run log recorded its hash for
/// this run; then each Core file, which Core accepts only if a manifest so
/// accepted names it as a subject with the same digest. Nothing else is taken.
// route: PUT /study/{study_id}/restore/{*name}
pub async fn restore_handler(
    State(state): State<AppState>,
    UrlPath((study_id, name)): UrlPath<(String, String)>,
    _scope: Scope,
    request: axum::extract::Request,
) -> Result<Json<serde_json::Value>, Failure> {
    // Read here, not through the `Bytes` extractor, whose 2 MiB default is too
    // small for a capture; the cap is ours.
    let body = axum::body::to_bytes(request.into_body(), MAX_RESTORED_BYTES)
        .await
        .map_err(|_| (StatusCode::PAYLOAD_TOO_LARGE, format!("a restored file is over {MAX_RESTORED_BYTES} bytes")))?;
    if !safe_name(&name) {
        return Err((StatusCode::BAD_REQUEST, "name must be a relative file name of printable characters, 200 bytes at most".into()));
    }
    tokio::task::spawn_blocking(move || {
        let lines = state.run_log.lines().map_err(internal)?;
        publishable_run(&lines, &study_id)?;
        let dir = state.run_log.run_dir(&study_id);
        let sha = sha256_hex(&body);
        if name == MANIFEST_FILE {
            let logged = lines.iter().any(|l| {
                l["entry"]["type"] == "published" && l["entry"]["study_id"] == study_id.as_str() && l["entry"]["manifest_sha256"] == sha.as_str()
            });
            if !logged {
                return Err(refused(format!("the run log never recorded a manifest with hash {sha} for run {study_id}")));
            }
            write_file(&dir.join(RESTORED_DIR).join(format!("{sha}.json")), &body)?;
            return Ok(Json(serde_json::json!({ "restored": name, "sha256": sha })));
        }
        if PUBLISH_FILES.contains(&name.as_str()) {
            return Err(refused(format!("`{name}` is written by a revision, not restored")));
        }
        let staged = staged_manifests(&dir)?;
        if staged.is_empty() {
            return Err((StatusCode::CONFLICT, "hand back the earlier revision's manifest.dsse.json first".into()));
        }
        let digest = staged.iter().find_map(|st| {
            st["subject"].as_array()?.iter().find(|s| s["name"] == name.as_str()).map(|s| s["digest"]["sha256"].clone())
        });
        match digest {
            None => Err(refused(format!("`{name}` is not a subject of a manifest Core signed and logged for run {study_id}"))),
            Some(d) if d != sha.as_str() => Err(refused(format!("`{name}` does not match the digest its manifest names"))),
            Some(_) => {
                write_file(&dir.join(&name), &body)?;
                Ok(Json(serde_json::json!({ "restored": name, "sha256": sha })))
            }
        }
    })
    .await
    .map_err(internal)?
}

// ---- report-data.json --------------------------------------------------------

/// The study's run entry, which must be signed and whose captures must still be
/// there to hash.
fn publishable_run(lines: &[serde_json::Value], study_id: &str) -> Result<serde_json::Value, Failure> {
    let of = |kind: &str| lines.iter().rev().find(|l| l["entry"]["type"] == kind && l["entry"]["study_id"] == study_id);
    let run = of("run").ok_or((StatusCode::NOT_FOUND, format!("the run log has no run {study_id}")))?;
    if of("discarded").is_some() {
        return Err((StatusCode::CONFLICT, format!("study {study_id}'s captures were discarded, so there is nothing to publish")));
    }
    if run["entry"]["record_sha256"].is_null() {
        return Err((StatusCode::CONFLICT, format!("study {study_id} has no signed record (it did not complete), so Core will not publish it")));
    }
    Ok(run["entry"].clone())
}

fn revision_of(lines: &[serde_json::Value], study_id: &str) -> u64 {
    lines.iter().filter(|l| l["entry"]["type"] == "published" && l["entry"]["study_id"] == study_id).count() as u64 + 1
}

/// The latest adoption of each `(kind, name)`, ordered by kind then name.
fn adoptions(lines: &[serde_json::Value], study_id: &str) -> Vec<serde_json::Value> {
    let mut latest: BTreeMap<(String, String), serde_json::Value> = BTreeMap::new();
    for l in lines {
        let e = &l["entry"];
        if e["type"] == "adopted" && e["study_id"] == study_id {
            let key = (e["kind"].as_str().unwrap_or("").to_string(), e["name"].as_str().unwrap_or("").to_string());
            let scope = e["scope"].as_str().unwrap_or("agent");
            latest.insert(
                key,
                serde_json::json!({
                    "kind": e["kind"], "name": e["name"], "text_sha256": e["text_sha256"], "scope": e["scope"],
                    "attribution": attribution(e["kind"].as_str().unwrap_or(""), scope), "at_utc_ms": e["at_utc_ms"],
                }),
            );
        }
    }
    latest.into_values().collect()
}

/// The Statement of a DSSE envelope Core wrote, decoded. The envelope is Core's own
/// file; one that does not read is a `500`.
pub(crate) fn statement_of(path: &std::path::Path) -> Result<serde_json::Value, Failure> {
    let env: serde_json::Value = serde_json::from_slice(&std::fs::read(path).map_err(internal)?).map_err(internal)?;
    let payload = base64::engine::general_purpose::STANDARD.decode(env["payload"].as_str().unwrap_or("")).map_err(internal)?;
    serde_json::from_slice(&payload).map_err(internal)
}

/// The call-log claim of a record's `observed`, as `report-data.json` carries it: the head, the
/// range and the entries in it. `None` when the record names no range.
pub(crate) fn call_log_claim(observed: &serde_json::Value) -> Option<serde_json::Value> {
    let range = &observed["call_log"];
    if !range.is_object() {
        return None;
    }
    let count = match (range["first_seq"].as_u64(), range["last_seq"].as_u64()) {
        (Some(first), Some(last)) => last + 1 - first,
        _ => 0,
    };
    Some(serde_json::json!({ "head": range["head"], "first_seq": range["first_seq"], "last_seq": range["last_seq"], "count": count }))
}

/// What an `events.json` step's outcome counts as.
fn count_outcomes(events: &serde_json::Value) -> serde_json::Value {
    let (mut pass, mut fail, mut timed_out, mut other) = (0u64, 0u64, 0u64, 0u64);
    for s in events["steps"].as_array().map(|a| a.as_slice()).unwrap_or(&[]) {
        match &s["outcome"] {
            o if o == "Pass" => pass += 1,
            o if o == "TimedOut" => timed_out += 1,
            o if o.get("Fail").is_some() => fail += 1,
            _ => other += 1,
        }
    }
    serde_json::json!({ "total": pass + fail + timed_out + other, "pass": pass, "fail": fail, "timed_out": timed_out, "other": other })
}

/// What Core can say of who acted, as one label of the closed list in
/// `notes/hil/06` that a milestone 1 Core can reach. Every Core is unattested
/// in milestone 1, so a person's act is never "protected" (decision 119).
pub(crate) fn attestation(person_acted: bool) -> serde_json::Value {
    let label = if person_acted { "person's acts not protected" } else { "agent-scoped only" };
    serde_json::json!({
        "label": label, "core_attested": false,
        "reason": "milestone 1: this Core's key is readable by the agent, so a person's token proves nothing the agent could not have taken",
    })
}

/// `report-data.json` for the next revision of a study, from what Core holds.
/// Deterministic for a given log and files: it carries no clock and no log
/// head, so publish can check that what it signs is what was asked for.
fn report_data(
    state: &AppState,
    lines: &[serde_json::Value],
    study_id: &str,
    publisher: Scope,
) -> Result<(serde_json::Value, u64), Failure> {
    let run = publishable_run(lines, study_id)?;
    // A study a Test run amended is bound by that run's entry, not by `run`'s (decision 146).
    let record_sha256 = state.run_log.bound_record_sha256(study_id).map_err(internal)?;
    let dir = state.run_log.run_dir(study_id);
    require_run_files(&dir)?;
    let events: serde_json::Value = serde_json::from_slice(&std::fs::read(dir.join("events.json")).map_err(internal)?).map_err(internal)?;
    let statement = statement_of(&dir.join(RECORD_FILE))?;
    let counts = count_outcomes(&events);
    let adopted = adoptions(lines, study_id);
    let go_by = run["go_by"].as_str().unwrap_or("agent");
    let declarations = statement["predicate"]["observed"]["declarations"].clone();
    let person_acted = go_by == "person"
        || publisher == Scope::Person
        || adopted.iter().any(|a| a["scope"] == "person")
        || declarations["entries"].as_array().is_some_and(|e| e.iter().any(|d| d["scope"] == "person"));
    let revision = revision_of(lines, study_id);
    let mut observed = serde_json::json!({
        "image_status": statement["predicate"]["observed"]["image_status"],
        "declarations": declarations,
    });
    // The record's call-log claim, present exactly when the record carries one.
    if let Some(claim) = call_log_claim(&statement["predicate"]["observed"]) {
        observed["call_log"] = claim;
    }
    let data = serde_json::json!({
        "schema": 1,
        "study_id": study_id,
        "kind": run["kind"],
        "revision": revision,
        "run_status": if run["clean"] == true { "completed" } else { "step_failed" },
        "steps": counts,
        "subject": { "study_name": events["study_name"], "provenance": events["provenance"] },
        "attestation": attestation(person_acted),
        "go_by": go_by,
        "published_by": publisher.as_str(),
        "adopted": adopted,
        "observed": observed,
        "claimed": { "build": statement["predicate"]["claimed"]["build"] },
        "core": { "version": env!("CARGO_PKG_VERSION"), "keyid": state.sealer.key().keyid() },
        "record_sha256": record_sha256,
    });
    Ok((data, revision))
}

pub(crate) fn revision_dir(state: &AppState, study_id: &str, revision: u64) -> PathBuf {
    state.run_log.run_dir(study_id).join("publish").join(format!("r{revision}"))
}

pub(crate) fn statement_bytes(subject: Vec<serde_json::Value>, predicate_type: &str, predicate: serde_json::Value) -> Result<Vec<u8>, Failure> {
    serde_json::to_vec(&serde_json::json!({
        "_type": STATEMENT_TYPE, "subject": subject, "predicateType": predicate_type, "predicate": predicate,
    }))
    .map_err(internal)
}

pub(crate) fn write_file(path: &std::path::Path, bytes: &[u8]) -> Result<(), Failure> {
    std::fs::create_dir_all(path.parent().expect("a file has a parent")).map_err(internal)?;
    let tmp = path.with_extension("partial");
    std::fs::write(&tmp, bytes).map_err(internal)?;
    std::fs::rename(&tmp, path).map_err(internal)
}

/// Writes and signs `report-data.json` for the run's next revision, and answers
/// it. Asking again replaces it; nothing is recorded in the log until `publish`.
// route: POST /study/{study_id}/report-data
pub async fn report_data_handler(State(state): State<AppState>, UrlPath(study_id): UrlPath<String>, scope: Scope) -> Result<Json<serde_json::Value>, Failure> {
    tokio::task::spawn_blocking(move || {
        let lines = state.run_log.lines().map_err(internal)?;
        let (data, revision) = report_data(&state, &lines, &study_id, scope)?;
        let bytes = serde_json::to_vec(&data).map_err(internal)?;
        let sha = sha256_hex(&bytes);
        let statement = statement_bytes(
            vec![serde_json::json!({ "name": REPORT_DATA_FILE, "digest": { "sha256": sha } })],
            REPORT_DATA_PREDICATE_TYPE,
            data.clone(),
        )?;
        let dir = revision_dir(&state, &study_id, revision);
        write_file(&dir.join(REPORT_DATA_FILE), &bytes)?;
        write_file(&dir.join(REPORT_DATA_ENVELOPE_FILE), &state.sealer.sign_statement(&statement).map_err(internal)?)?;
        Ok(Json(serde_json::json!({
            "revision": revision, "report_data_sha256": sha, "keyid": state.sealer.key().keyid(), "report_data": data,
        })))
    })
    .await
    .map_err(internal)?
}

// ---- the manifest and the publish --------------------------------------------

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApiFile {
    pub(crate) name: String,
    pub(crate) sha256: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PublishRequest {
    /// Every file embarch-api made for the zip, with its hash.
    #[serde(default)]
    pub(crate) files: Vec<ApiFile>,
}

/// The files of the run Core produced, by zip-relative name.
pub(crate) fn core_run_files(dir: &std::path::Path) -> Result<Vec<String>, Failure> {
    let mut names: Vec<String> = ["events.json", "measurements.json", "study.json", RECORD_FILE]
        .iter()
        .filter(|n| dir.join(n).is_file())
        .map(|n| n.to_string())
        .collect();
    let streams = dir.join("streams");
    if streams.is_dir() {
        for e in std::fs::read_dir(&streams).map_err(internal)? {
            let e = e.map_err(internal)?;
            if e.file_type().map_err(internal)?.is_file() {
                names.push(format!("streams/{}", e.file_name().to_string_lossy()));
            }
        }
    }
    Ok(names)
}

/// The run's slice of the call log (decision 117), from the range its signed
/// record names. `None` for a run sealed with no call-log range (one recorded
/// before the call log, or whose log could not be read at the time).
fn call_log_segment(state: &AppState, dir: &std::path::Path) -> Result<Option<Vec<u8>>, Failure> {
    call_log_segment_of(state, &statement_of(&dir.join(RECORD_FILE))?)
}

/// The same, from the Statement of any record that names a range in `observed.call_log`: a
/// study's or a Test run's (decision 148).
pub(crate) fn call_log_segment_of(state: &AppState, statement: &serde_json::Value) -> Result<Option<Vec<u8>>, Failure> {
    let range = &statement["predicate"]["observed"]["call_log"];
    if range.is_null() {
        return Ok(None);
    }
    let text = |k: &str| range[k].as_str().map(str::to_string).ok_or_else(|| internal(format!("the record's call_log has no {k}")));
    let (prev, head) = (text("prev")?, text("head")?);
    // `length` is the log's length when the run ended; an empty range names no seq, so it starts there.
    let end_len = range["length"].as_u64().ok_or_else(|| internal("the record's call_log has no length"))?;
    let start_len = range["first_seq"].as_u64().unwrap_or(end_len);
    let start = crate::call_log::Mark { head: prev, len: start_len };
    let end = crate::call_log::Mark { head, len: end_len };
    state.call_log.segment_bytes(&start, &end).map(Some).map_err(|e| (StatusCode::CONFLICT, format!("{e:#}")))
}

/// Signs the manifest over every file Core produced and records the publish,
/// with the run log's head, in the log.
// route: POST /study/{study_id}/publish
pub async fn publish_handler(
    State(state): State<AppState>,
    UrlPath(study_id): UrlPath<String>,
    scope: Scope,
    Json(req): Json<PublishRequest>,
) -> Result<Json<serde_json::Value>, Failure> {
    check_files(&req.files)?;
    tokio::task::spawn_blocking(move || publish(&state, &study_id, scope, req)).await.map_err(internal)?
}

/// embarch-api's files as a publish request lists them: each a safe name and a hash, none twice.
pub(crate) fn check_files(files: &[ApiFile]) -> Result<(), Failure> {
    let mut seen = std::collections::HashSet::new();
    for f in files {
        if !safe_name(&f.name) || !is_sha256_hex(&f.sha256) {
            return Err((StatusCode::BAD_REQUEST, format!("`{}` needs a relative file name and a 64-hex sha256", f.name)));
        }
        if !seen.insert(f.name.clone()) {
            return Err((StatusCode::BAD_REQUEST, format!("`{}` is listed twice", f.name)));
        }
    }
    Ok(())
}

/// A name embarch-api lists must not be one Core produces.
pub(crate) fn check_api_names(files: &[ApiFile], subjects: &[(String, String)]) -> Result<(), Failure> {
    for f in files {
        if subjects.iter().any(|(n, _)| *n == f.name) || f.name == SEGMENT_FILE || f.name == CALL_LOG_SEGMENT_FILE || f.name == MANIFEST_FILE {
            return Err((StatusCode::BAD_REQUEST, format!("`{}` is a file Core produces; embarch-api lists only its own", f.name)));
        }
    }
    Ok(())
}

/// What a manifest is signed over, for a study run or a Test run (decision 148).
pub(crate) struct Manifest<'a> {
    /// `study_id` or `run_id`: the key the manifest's predicate and the `published` entry name the run by.
    pub id_key: &'static str,
    pub id: &'a str,
    pub revision: u64,
    pub rdir: &'a std::path::Path,
    /// Every file Core produced for the zip, by zip-relative name and hash.
    pub subjects: &'a [(String, String)],
    pub call_segment: Option<&'a [u8]>,
    pub files: &'a [ApiFile],
    pub scope: Scope,
    pub record_sha256: &'a serde_json::Value,
    pub report_data_sha256: &'a str,
}

/// Writes the run log's segment and the call log's, signs the manifest and answers the
/// `published` entry to append. Called inside `RunLog::append_with`'s lock with the lines and head
/// it hands over, so the head named is the entry's own `prev` (decision 118).
pub(crate) fn sign_manifest(
    state: &AppState,
    m: &Manifest,
    lines: &[serde_json::Value],
    head: Option<&str>,
    supersedes: Option<serde_json::Value>,
) -> Result<serde_json::Value, Failure> {
    let mut segment = Vec::new();
    for l in lines {
        segment.extend(serde_json::to_vec(l).map_err(internal)?);
        segment.push(b'\n');
    }
    write_file(&m.rdir.join(SEGMENT_FILE), &segment)?;
    let mut all = m.subjects.to_vec();
    all.push((SEGMENT_FILE.to_string(), sha256_hex(&segment)));
    if let Some(c) = m.call_segment {
        write_file(&m.rdir.join(CALL_LOG_SEGMENT_FILE), c)?;
        all.push((CALL_LOG_SEGMENT_FILE.to_string(), sha256_hex(c)));
    }
    all.sort();
    let subject: Vec<serde_json::Value> = all.iter().map(|(n, h)| serde_json::json!({ "name": n, "digest": { "sha256": h } })).collect();
    let at = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0);
    let mut listed: Vec<serde_json::Value> = m.files.iter().map(|f| serde_json::json!({ "name": f.name, "sha256": f.sha256 })).collect();
    listed.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
    let mut predicate = serde_json::json!({
        "schema": 1, "revision": m.revision, "supersedes": supersedes,
        "published_by": m.scope.as_str(), "published_utc_ms": at,
        "core": { "version": env!("CARGO_PKG_VERSION") },
        "run_log": { "head": head, "length": lines.len() },
        "record_sha256": m.record_sha256, "report_data_sha256": m.report_data_sha256,
        "embarch_api_files": listed,
    });
    predicate[m.id_key] = m.id.into();
    let statement = statement_bytes(subject, MANIFEST_PREDICATE_TYPE, predicate)?;
    let envelope = state.sealer.sign_statement(&statement).map_err(internal)?;
    write_file(&m.rdir.join(MANIFEST_FILE), &envelope)?;
    let mut entry = serde_json::json!({
        "type": "published", "manifest_sha256": sha256_hex(&envelope),
        "scope": m.scope.as_str(), "at_utc_ms": at, "revision": m.revision, "log_head": head,
        "report_data_sha256": m.report_data_sha256,
    });
    entry[m.id_key] = m.id.into();
    Ok(entry)
}

fn publish(state: &AppState, study_id: &str, scope: Scope, req: PublishRequest) -> Result<Json<serde_json::Value>, Failure> {
    let first = state.run_log.lines().map_err(internal)?;
    let (data, revision) = report_data(state, &first, study_id, scope)?;
    let rdir = revision_dir(state, study_id, revision);
    let on_disk = std::fs::read(rdir.join(REPORT_DATA_FILE)).map_err(|_| {
        (StatusCode::CONFLICT, "ask for report-data first: POST /study/{id}/report-data, then build the zip's files from it".to_string())
    })?;
    let bytes = serde_json::to_vec(&data).map_err(internal)?;
    if on_disk != bytes {
        return Err((
            StatusCode::CONFLICT,
            "report-data.json no longer matches what Core would write now (a note or prose block was adopted since, or the publishing token's scope differs from the one that asked); ask for report-data again".into(),
        ));
    }

    // Hash the run's files before taking the log lock; they do not move.
    let dir = state.run_log.run_dir(study_id);
    let mut subjects: Vec<(String, String)> = Vec::new();
    for name in core_run_files(&dir)? {
        subjects.push((name.clone(), sha256_of_file(&dir.join(&name)).map_err(internal)?));
    }
    for name in [REPORT_DATA_FILE, REPORT_DATA_ENVELOPE_FILE] {
        subjects.push((name.to_string(), sha256_of_file(&rdir.join(name)).map_err(internal)?));
    }
    check_api_names(&req.files, &subjects)?;
    let call_segment = call_log_segment(state, &dir)?;
    let report_data_sha256 = sha256_hex(&bytes);
    let record_sha256 = data["record_sha256"].clone();

    let entry = state.run_log.append_with(|lines, head| {
        // Everything has to have held between the check above and now.
        publishable_run(lines, study_id)?;
        if revision_of(lines, study_id) != revision {
            return Err((StatusCode::CONFLICT, format!("revision {revision} was published meanwhile; ask for report-data again")));
        }
        let supersedes = lines
            .iter()
            .rfind(|l| l["entry"]["type"] == "published" && l["entry"]["study_id"] == study_id)
            .map(|l| serde_json::json!({ "revision": revision - 1, "manifest_sha256": l["entry"]["manifest_sha256"] }));
        let m = Manifest {
            id_key: "study_id", id: study_id, revision, rdir: &rdir, subjects: &subjects, call_segment: call_segment.as_deref(),
            files: &req.files, scope, record_sha256: &record_sha256, report_data_sha256: &report_data_sha256,
        };
        sign_manifest(state, &m, lines, head, supersedes)
    })?;
    Ok(Json(serde_json::json!({ "revision": revision, "entry": entry, "keyid": state.sealer.key().keyid() })))
}

// ---- fetching what Core produced ---------------------------------------------

/// A file of a revision, by the name the zip gives it: one of Core's four
/// publish files, or any file the revision's signed manifest names; at the
/// pending revision (opened by `report-data`, no manifest yet), the run's own
/// Core files. Nothing else is served, so the route cannot read outside the run.
// route: GET /study/{study_id}/publish/{revision}/files/{*name}
pub async fn file_handler(
    State(state): State<AppState>,
    UrlPath((study_id, revision, name)): UrlPath<(String, u64, String)>,
) -> Result<Response, Failure> {
    let not_found = || (StatusCode::NOT_FOUND, format!("revision {revision} of {study_id} has no file {name}"));
    let rdir = revision_dir(&state, &study_id, revision);
    let path = if PUBLISH_FILES.contains(&name.as_str()) {
        rdir.join(&name)
    } else {
        let run_dir = state.run_log.run_dir(&study_id);
        let manifest = std::fs::read(rdir.join(MANIFEST_FILE)).ok();
        let named = match manifest {
            Some(bytes) => {
                let env: serde_json::Value = serde_json::from_slice(&bytes).map_err(|_| not_found())?;
                let payload = base64::engine::general_purpose::STANDARD.decode(env["payload"].as_str().unwrap_or("")).map_err(internal)?;
                let manifest: serde_json::Value = serde_json::from_slice(&payload).map_err(internal)?;
                manifest["subject"].as_array().is_some_and(|s| s.iter().any(|x| x["name"] == name.as_str()))
            }
            // No manifest yet: only the pending revision, once `report-data` has
            // opened it, serves the run's own files, the ones the manifest will sign.
            None => {
                let (sid, rd) = (study_id.clone(), rdir.join(REPORT_DATA_FILE));
                let st = state.clone();
                let pending = tokio::task::spawn_blocking(move || -> Result<bool, Failure> {
                    let lines = st.run_log.lines().map_err(internal)?;
                    Ok(revision == revision_of(&lines, &sid) && rd.is_file())
                })
                .await
                .map_err(internal)??;
                pending && core_run_files(&run_dir)?.contains(&name)
            }
        };
        if !named {
            return Err(not_found());
        }
        run_dir.join(&name)
    };
    let bytes = tokio::fs::read(&path).await.map_err(|_| not_found())?;
    Ok(([(header::CONTENT_TYPE, "application/octet-stream")], bytes).into_response())
}

/// A finished run's own `measurements.json`, byte for byte as Core wrote it as
/// the study ended, with no publish and no revision (decision 121). The same
/// run directory the publish routes read; nothing is recomputed. Three distinct
/// refusals: `404` no such run, `409` the study has not ended, `422` no file was written (the
/// study declared no measurements, failed, or is a host study with no `parse`; the body is JSON
/// `{error, code: "no_measurements"}`), and `410` the run's files are gone. A host study's file is
/// served like a device study's: it sits in the same run directory.
// route: GET /study/{study_id}/measurements
pub async fn measurements_handler(State(state): State<AppState>, UrlPath(study_id): UrlPath<String>) -> Result<Response, Failure> {
    if state.study_jobs.lock().unwrap().get(&study_id).is_some_and(|j| j.status == "running") {
        return Err((StatusCode::CONFLICT, format!("study {study_id} is still running: its measurements are written as it ends")));
    }
    let st = state.clone();
    let id = study_id.clone();
    let out = tokio::task::spawn_blocking(move || -> Result<Result<Vec<u8>, String>, Failure> {
        let lines = st.run_log.lines().map_err(internal)?;
        let entered = lines.iter().any(|l| l["entry"]["type"] == "run" && l["entry"]["study_id"] == id.as_str());
        if !entered {
            return Err((StatusCode::NOT_FOUND, format!("the run log has no run {id}")));
        }
        let dir = st.run_log.run_dir(&id);
        match std::fs::read(dir.join(crate::measurements::FILE)) {
            Ok(bytes) => Ok(Ok(bytes)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                if dir.join("events.json").is_file() {
                    Ok(Err(format!("study {id} has no {}: it declared no measurements, did not complete, or is a host study with no parse step", crate::measurements::FILE)))
                } else {
                    Err((StatusCode::GONE, format!("study {id}'s files are gone (discarded or swept by retention)")))
                }
            }
            Err(e) => Err(internal(e)),
        }
    })
    .await
    .map_err(internal)??;
    match out {
        Ok(bytes) => Ok(([(header::CONTENT_TYPE, "application/json")], bytes).into_response()),
        Err(message) => Ok((StatusCode::UNPROCESSABLE_ENTITY, Json(serde_json::json!({ "error": message, "code": NO_MEASUREMENTS_CODE }))).into_response()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{Signature, Verifier, VerifyingKey};
    use tower::ServiceExt as _;

    const ID: &str = "0123456789abcdef0123456789abcdef";

    /// A finished run as the log and the signer leave it: captures, events, a
    /// record, and the run's entry.
    fn finished_run(state: &AppState, status: &str, outcome: &str, go: Scope) {
        finished_run_with_calls(state, status, outcome, go, 0, true);
    }

    /// As `finished_run`, with `calls` entries made in the call log while it ran;
    /// `noted` false is a run sealed before the call log existed.
    fn finished_run_with_calls(state: &AppState, status: &str, outcome: &str, go: Scope, calls: u64, noted: bool) {
        let dir = state.run_log.run_dir(ID);
        std::fs::create_dir_all(dir.join("streams")).unwrap();
        std::fs::write(dir.join("streams/tap.bin"), b"raw bytes").unwrap();
        std::fs::write(
            dir.join("events.json"),
            format!(r#"{{"study_name":"fixture","steps":[{{"outcome":{outcome}}}],"provenance":{{"dev_bench_version":"gbench1"}}}}"#),
        )
        .unwrap();
        state.run_log.begin(ID, "device", go);
        let before = crate::call_log::RunCalls::begin(&state.call_log).filter(|_| noted);
        for i in 0..calls {
            state.call_log.record(serde_json::json!({"type": "call", "at_utc_ms": i, "method": "POST", "route": "/x"}), None).unwrap();
        }
        let job = crate::signing::SealJob { sealer: state.sealer.clone(), kind: "device", build: None, submitted_by: Some(go), calls: before };
        state.sealer.seal(&job, &dir, ID, 1).unwrap();
        state.run_log.end(ID, status, None);
    }

    async fn send(state: &AppState, method: &str, uri: &str, bearer: &str, body: &str) -> (StatusCode, Vec<u8>) {
        let req = axum::http::Request::builder()
            .method(method)
            .uri(uri)
            .header("authorization", format!("Bearer {bearer}"))
            .header("content-type", "application/json")
            .body(axum::body::Body::from(body.to_string()))
            .unwrap();
        let r = crate::api::build_router(state.clone()).oneshot(req).await.unwrap();
        let status = r.status();
        (status, axum::body::to_bytes(r.into_body(), 1 << 24).await.unwrap().to_vec())
    }

    fn json(b: &[u8]) -> serde_json::Value {
        serde_json::from_slice(b).unwrap_or_else(|_| panic!("{}", String::from_utf8_lossy(b)))
    }

    /// A standard DSSE v1 check from the spec's text alone, against the key.
    fn dsse_payload(envelope: &[u8], public_key_hex: &str) -> serde_json::Value {
        let env: serde_json::Value = serde_json::from_slice(envelope).unwrap();
        let payload = base64::engine::general_purpose::STANDARD.decode(env["payload"].as_str().unwrap()).unwrap();
        let pt = env["payloadType"].as_str().unwrap();
        let mut pae = format!("DSSEv1 {} {} {} ", pt.len(), pt, payload.len()).into_bytes();
        pae.extend_from_slice(&payload);
        let sig = base64::engine::general_purpose::STANDARD.decode(env["signatures"][0]["sig"].as_str().unwrap()).unwrap();
        let key: [u8; 32] = (0..64).step_by(2).map(|i| u8::from_str_radix(&public_key_hex[i..i + 2], 16).unwrap()).collect::<Vec<_>>().try_into().unwrap();
        VerifyingKey::from_bytes(&key).unwrap().verify(&pae, &Signature::from_slice(&sig).unwrap()).expect("the envelope verifies");
        serde_json::from_slice(&payload).unwrap()
    }

    #[tokio::test]
    async fn publishing_a_run_yields_a_signed_manifest_and_signed_report_data() {
        let state = AppState::new("agent".to_string());
        finished_run(&state, "completed", r#""Pass""#, Scope::Agent);
        let pk = state.sealer.key().public_key_hex();

        let (s, b) = send(&state, "POST", &format!("/study/{ID}/report-data"), "agent", "").await;
        assert_eq!(s, StatusCode::OK, "{}", String::from_utf8_lossy(&b));
        let rd = json(&b);
        assert_eq!(rd["revision"], 1);
        let pdf = "cd".repeat(32);
        let (s, b) = send(&state, "POST", &format!("/study/{ID}/publish"), "agent", &format!(r#"{{"files":[{{"name":"report.pdf","sha256":"{pdf}"}}]}}"#)).await;
        assert_eq!(s, StatusCode::OK, "{}", String::from_utf8_lossy(&b));
        let published = json(&b);

        // The manifest verifies under Core's key and names Core's files by hash.
        let (_, env) = send(&state, "GET", &format!("/study/{ID}/publish/1/files/manifest.dsse.json"), "agent", "").await;
        assert_eq!(sha256_hex(&env), published["entry"]["entry"]["manifest_sha256"].as_str().unwrap());
        let st = dsse_payload(&env, &pk);
        assert_eq!(st["predicateType"], MANIFEST_PREDICATE_TYPE);
        let names: Vec<&str> = st["subject"].as_array().unwrap().iter().map(|s| s["name"].as_str().unwrap()).collect();
        for want in ["events.json", "record.dsse.json", "streams/tap.bin", "report-data.json", "report-data.dsse.json", "run-log-segment.jsonl", "call-log-segment.jsonl"] {
            assert!(names.contains(&want), "{want} in {names:?}");
        }
        assert!(!names.contains(&"report.pdf"), "embarch-api's file is not Core's subject");
        assert_eq!(st["predicate"]["embarch_api_files"][0]["name"], "report.pdf");
        for s in st["subject"].as_array().unwrap() {
            let (code, body) = send(&state, "GET", &format!("/study/{ID}/publish/1/files/{}", s["name"].as_str().unwrap()), "agent", "").await;
            assert_eq!(code, StatusCode::OK);
            assert_eq!(sha256_hex(&body), s["digest"]["sha256"].as_str().unwrap(), "{}", s["name"]);
        }

        // The signed report-data is the file's content.
        let (_, rd_env) = send(&state, "GET", &format!("/study/{ID}/publish/1/files/report-data.dsse.json"), "agent", "").await;
        let rst = dsse_payload(&rd_env, &pk);
        let (_, rd_file) = send(&state, "GET", &format!("/study/{ID}/publish/1/files/report-data.json"), "agent", "").await;
        assert_eq!(rst["predicate"], json(&rd_file));
        assert_eq!(rst["subject"][0]["digest"]["sha256"], sha256_hex(&rd_file).as_str());

        // A byte changed in the manifest no longer verifies.
        let mut e: serde_json::Value = serde_json::from_slice(&env).unwrap();
        let mut payload = base64::engine::general_purpose::STANDARD.decode(e["payload"].as_str().unwrap()).unwrap();
        payload[10] ^= 1;
        e["payload"] = base64::engine::general_purpose::STANDARD.encode(payload).into();
        assert!(std::panic::catch_unwind(|| dsse_payload(&serde_json::to_vec(&e).unwrap(), &pk)).is_err());
    }

    /// The call log's range as the signed record names it, and the segment the
    /// publish wrote, recomputed from the segment alone.
    async fn call_log_of(state: &AppState) -> (serde_json::Value, Vec<u8>) {
        let pk = state.sealer.key().public_key_hex();
        let record = std::fs::read(state.run_log.run_dir(ID).join("record.dsse.json")).unwrap();
        let range = dsse_payload(&record, &pk)["predicate"]["observed"]["call_log"].clone();
        send(state, "POST", &format!("/study/{ID}/report-data"), "agent", "").await;
        let (s, b) = send(state, "POST", &format!("/study/{ID}/publish"), "agent", r#"{"files":[]}"#).await;
        assert_eq!(s, StatusCode::OK, "{}", String::from_utf8_lossy(&b));
        let (s, seg) = send(state, "GET", &format!("/study/{ID}/publish/1/files/call-log-segment.jsonl"), "agent", "").await;
        assert_eq!(s, StatusCode::OK);
        (range, seg)
    }

    #[tokio::test]
    async fn a_runs_record_names_the_call_log_range_and_the_publish_carries_the_segment() {
        let state = AppState::new("agent".to_string());
        // One call before the run, which is not in its range.
        state.call_log.record(serde_json::json!({"type": "call", "at_utc_ms": 0}), None).unwrap();
        finished_run_with_calls(&state, "completed", r#""Pass""#, Scope::Agent, 3, true);
        let (range, seg) = call_log_of(&state).await;
        assert_eq!((range["first_seq"].as_u64(), range["last_seq"].as_u64(), range["length"].as_u64()), (Some(1), Some(3), Some(4)));

        // The segment's entries recompute from the named entry to the recorded head.
        let text = String::from_utf8(seg.clone()).unwrap();
        let mut lines = text.lines().map(|l| serde_json::from_str::<serde_json::Value>(l).unwrap());
        let header = lines.next().unwrap()["call_log_segment"].clone();
        assert_eq!(header["prev"], range["prev"]);
        assert_eq!(header["head"], range["head"]);
        let mut prev = range["prev"].as_str().unwrap().to_string();
        let mut n = 0;
        for l in lines {
            assert_eq!(l["entry"]["prev"], prev.as_str());
            assert_eq!(l["hash"], sha256_hex(&serde_json::to_vec(&l["entry"]).unwrap()).as_str());
            prev = l["hash"].as_str().unwrap().to_string();
            n += 1;
        }
        assert_eq!(n, 3);
        assert_eq!(prev, range["head"].as_str().unwrap());

        // report-data.json carries the same claim, with the count.
        let (_, rd) = send(&state, "GET", &format!("/study/{ID}/publish/1/files/report-data.json"), "agent", "").await;
        let rd: serde_json::Value = serde_json::from_slice(&rd).unwrap();
        assert_eq!(rd["observed"]["call_log"], serde_json::json!({"head": range["head"], "first_seq": 1, "last_seq": 3, "count": 3}));

        // The signed manifest lists it by hash.
        let pk = state.sealer.key().public_key_hex();
        let (_, env) = send(&state, "GET", &format!("/study/{ID}/publish/1/files/manifest.dsse.json"), "agent", "").await;
        let st = dsse_payload(&env, &pk);
        let listed = st["subject"].as_array().unwrap().iter().find(|s| s["name"] == "call-log-segment.jsonl").unwrap();
        assert_eq!(listed["digest"]["sha256"], sha256_hex(&seg).as_str());
    }

    #[tokio::test]
    async fn a_run_with_no_call_in_its_range_publishes_an_empty_segment_that_names_the_head() {
        let state = AppState::new("agent".to_string());
        state.call_log.record(serde_json::json!({"type": "call", "at_utc_ms": 0}), None).unwrap();
        let head = state.call_log.mark(false).unwrap().head;
        finished_run_with_calls(&state, "completed", r#""Pass""#, Scope::Agent, 0, true);
        let (range, seg) = call_log_of(&state).await;
        assert!(range["first_seq"].is_null() && range["last_seq"].is_null());
        assert_eq!((range["head"].as_str(), range["prev"].as_str()), (Some(head.as_str()), Some(head.as_str())));
        let text = String::from_utf8(seg).unwrap();
        assert_eq!(text.lines().count(), 1, "the header alone");
        let h: serde_json::Value = serde_json::from_str(text.lines().next().unwrap()).unwrap();
        assert_eq!(h["call_log_segment"]["head"], head.as_str());
        let (_, rd) = send(&state, "GET", &format!("/study/{ID}/publish/1/files/report-data.json"), "agent", "").await;
        let rd: serde_json::Value = serde_json::from_slice(&rd).unwrap();
        assert_eq!(
            rd["observed"]["call_log"],
            serde_json::json!({"head": head.as_str(), "first_seq": null, "last_seq": null, "count": 0})
        );
    }

    #[tokio::test]
    async fn a_run_sealed_without_a_call_log_range_publishes_no_segment() {
        let state = AppState::new("agent".to_string());
        finished_run_with_calls(&state, "completed", r#""Pass""#, Scope::Agent, 0, false);
        let record = std::fs::read(state.run_log.run_dir(ID).join("record.dsse.json")).unwrap();
        assert!(dsse_payload(&record, &state.sealer.key().public_key_hex())["predicate"]["observed"].get("call_log").is_none());
        send(&state, "POST", &format!("/study/{ID}/report-data"), "agent", "").await;
        let (s, _) = send(&state, "POST", &format!("/study/{ID}/publish"), "agent", r#"{"files":[]}"#).await;
        assert_eq!(s, StatusCode::OK);
        let (_, rd) = send(&state, "GET", &format!("/study/{ID}/publish/1/files/report-data.json"), "agent", "").await;
        let rd: serde_json::Value = serde_json::from_slice(&rd).unwrap();
        assert!(rd["observed"].get("call_log").is_none());
        let (s, _) = send(&state, "GET", &format!("/study/{ID}/publish/1/files/call-log-segment.jsonl"), "agent", "").await;
        assert_eq!(s, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn the_publish_records_the_log_head_beside_the_manifest_hash_and_the_segment_ends_before_it() {
        let state = AppState::new("agent".to_string());
        finished_run(&state, "completed", r#""Pass""#, Scope::Agent);
        send(&state, "POST", &format!("/study/{ID}/report-data"), "agent", "").await;
        let head_before = state.run_log.verify().unwrap().unwrap().0.unwrap();
        let (s, b) = send(&state, "POST", &format!("/study/{ID}/publish"), "agent", r#"{"files":[]}"#).await;
        assert_eq!(s, StatusCode::OK);
        let entry = json(&b)["entry"].clone();
        assert_eq!(entry["entry"]["log_head"], head_before.as_str());
        assert_eq!(entry["entry"]["prev"], head_before.as_str());
        let (_, seg) = send(&state, "GET", &format!("/study/{ID}/publish/1/files/run-log-segment.jsonl"), "agent", "").await;
        let lines: Vec<&str> = std::str::from_utf8(&seg).unwrap().lines().collect();
        assert_eq!(lines.len(), 1);
        assert_eq!(json(lines[0].as_bytes())["hash"], head_before.as_str());
        // The log stays a valid chain, and the study's log route shows the publish.
        assert!(state.run_log.verify().unwrap().is_ok());
        let (_, b) = send(&state, "GET", &format!("/study/{ID}/log"), "agent", "").await;
        assert_eq!(json(&b)["published"]["entry"]["log_head"], head_before.as_str());
    }

    #[tokio::test]
    async fn a_note_is_relayed_by_the_agent_with_the_agents_token_and_the_persons_with_the_persons() {
        let state = AppState::new("agent".to_string());
        finished_run(&state, "completed", r#""Pass""#, Scope::Agent);
        let text = "bench B, cable 2";
        let body = format!(r#"{{"kind":"note","name":"notes.md","text":"{text}"}}"#);
        let (s, b) = send(&state, "POST", &format!("/study/{ID}/adopt"), "agent", &body).await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(json(&b)["entry"]["scope"], "agent");
        assert_eq!(json(&b)["entry"]["text_sha256"], sha256_hex(text.as_bytes()).as_str());
        let (_, b) = send(&state, "POST", &format!("/study/{ID}/report-data"), "agent", "").await;
        assert_eq!(json(&b)["report_data"]["adopted"][0]["attribution"], "relayed by the agent");

        // The person's token (the test state's person token is `person-agent`).
        let body = format!(r#"{{"kind":"note","name":"notes.md","text":"{text} B"}}"#);
        let (_, b) = send(&state, "POST", &format!("/study/{ID}/adopt"), "person-agent", &body).await;
        assert_eq!(json(&b)["entry"]["scope"], "person");
        let (_, b) = send(&state, "POST", &format!("/study/{ID}/report-data"), "person-agent", "").await;
        let rd = json(&b)["report_data"].clone();
        assert_eq!(rd["adopted"][0]["attribution"], "the person's");
        assert_eq!(rd["adopted"][0]["text_sha256"], sha256_hex(format!("{text} B").as_bytes()).as_str());
        assert_eq!(rd["attestation"]["label"], "person's acts not protected");

        // Malformed adoptions and an unknown run are refused.
        let (s, _) = send(&state, "POST", &format!("/study/{ID}/adopt"), "agent", r#"{"kind":"memo","name":"n","text":"x"}"#).await;
        assert_eq!(s, StatusCode::BAD_REQUEST);
        let (s, _) = send(&state, "POST", &format!("/study/{ID}/adopt"), "agent", r#"{"kind":"note","name":"../x","text":"x"}"#).await;
        assert_eq!(s, StatusCode::BAD_REQUEST);
        let (s, _) = send(&state, "POST", "/study/nosuchrun/adopt", "agent", r#"{"kind":"note","name":"n","text":"x"}"#).await;
        assert_eq!(s, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn a_studys_report_data_carries_a_run_status_and_no_tests_requirements_or_verdicts() {
        let state = AppState::new("agent".to_string());
        finished_run(&state, "completed", r#"{"Fail":{"reason":"x"}}"#, Scope::Agent);
        let (_, b) = send(&state, "POST", &format!("/study/{ID}/report-data"), "agent", "").await;
        let rd = json(&b)["report_data"].clone();
        assert_eq!(rd["run_status"], "step_failed");
        assert_eq!(rd["steps"]["fail"], 1);
        assert_eq!(rd["attestation"]["label"], "agent-scoped only");
        assert_eq!(rd["go_by"], "agent");
        let text = serde_json::to_string(&rd).unwrap();
        for key in ["tests", "requirements", "verdicts"] {
            assert!(rd.get(key).is_none(), "{key}");
            assert!(!text.contains(&format!("\"{key}\"")), "{key} appears nested");
        }
    }

    #[tokio::test]
    async fn publish_refuses_what_it_cannot_stand_behind() {
        let state = AppState::new("agent".to_string());
        // No such run.
        let (s, _) = send(&state, "POST", "/study/nosuchrun/report-data", "agent", "").await;
        assert_eq!(s, StatusCode::NOT_FOUND);
        finished_run(&state, "completed", r#""Pass""#, Scope::Agent);
        // Publish before report-data, and with a changed adoption since.
        let (s, _) = send(&state, "POST", &format!("/study/{ID}/publish"), "agent", r#"{"files":[]}"#).await;
        assert_eq!(s, StatusCode::CONFLICT);
        send(&state, "POST", &format!("/study/{ID}/report-data"), "agent", "").await;
        send(&state, "POST", &format!("/study/{ID}/adopt"), "agent", r#"{"kind":"note","name":"n.md","text":"late"}"#).await;
        let (s, _) = send(&state, "POST", &format!("/study/{ID}/publish"), "agent", r#"{"files":[]}"#).await;
        assert_eq!(s, StatusCode::CONFLICT);
        send(&state, "POST", &format!("/study/{ID}/report-data"), "agent", "").await;
        // embarch-api may not list a file Core produces, nor a malformed one.
        let (s, _) = send(&state, "POST", &format!("/study/{ID}/publish"), "agent", &format!(r#"{{"files":[{{"name":"events.json","sha256":"{}"}}]}}"#, "ab".repeat(32))).await;
        assert_eq!(s, StatusCode::BAD_REQUEST);
        let (s, _) = send(&state, "POST", &format!("/study/{ID}/publish"), "agent", r#"{"files":[{"name":"a.pdf","sha256":"nothex"}]}"#).await;
        assert_eq!(s, StatusCode::BAD_REQUEST);
        // A different token's scope than the one that asked for report-data.
        let (s, _) = send(&state, "POST", &format!("/study/{ID}/publish"), "person-agent", r#"{"files":[]}"#).await;
        assert_eq!(s, StatusCode::CONFLICT);
        // A discarded run has nothing to publish.
        state.run_log.discard(ID, Scope::Person).unwrap();
        let (s, _) = send(&state, "POST", &format!("/study/{ID}/report-data"), "agent", "").await;
        assert_eq!(s, StatusCode::CONFLICT);
    }

    #[tokio::test]
    async fn a_second_publish_is_revision_two_naming_the_first() {
        let state = AppState::new("agent".to_string());
        finished_run(&state, "completed", r#""Pass""#, Scope::Agent);
        send(&state, "POST", &format!("/study/{ID}/report-data"), "agent", "").await;
        let (_, b) = send(&state, "POST", &format!("/study/{ID}/publish"), "agent", r#"{"files":[]}"#).await;
        let first = json(&b)["entry"]["entry"]["manifest_sha256"].clone();
        let (_, b) = send(&state, "POST", &format!("/study/{ID}/report-data"), "agent", "").await;
        assert_eq!(json(&b)["revision"], 2);
        let (s, _) = send(&state, "POST", &format!("/study/{ID}/publish"), "agent", r#"{"files":[]}"#).await;
        assert_eq!(s, StatusCode::OK);
        let (_, env) = send(&state, "GET", &format!("/study/{ID}/publish/2/files/manifest.dsse.json"), "agent", "").await;
        let st = dsse_payload(&env, &state.sealer.key().public_key_hex());
        assert_eq!(st["predicate"]["supersedes"]["manifest_sha256"], first);
        assert_eq!(st["predicate"]["supersedes"]["revision"], 1);
        // The first revision's files are still there.
        let (s, _) = send(&state, "GET", &format!("/study/{ID}/publish/1/files/manifest.dsse.json"), "agent", "").await;
        assert_eq!(s, StatusCode::OK);
        // A file the manifest does not name is not served.
        let (s, _) = send(&state, "GET", &format!("/study/{ID}/publish/1/files/study.json"), "agent", "").await;
        assert_eq!(s, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn the_pending_revision_serves_the_runs_own_files_and_nothing_else() {
        let state = AppState::new("agent".to_string());
        finished_run(&state, "completed", r#""Pass""#, Scope::Agent);
        let url = |r: u64, n: &str| format!("/study/{ID}/publish/{r}/files/{n}");
        // Before report-data opens the revision, nothing of the run is served.
        let (s, _) = send(&state, "GET", &url(1, "events.json"), "agent", "").await;
        assert_eq!(s, StatusCode::NOT_FOUND);
        send(&state, "POST", &format!("/study/{ID}/report-data"), "agent", "").await;
        let mut served = Vec::new();
        for n in ["events.json", "record.dsse.json", "streams/tap.bin"] {
            let (s, b) = send(&state, "GET", &url(1, n), "agent", "").await;
            assert_eq!(s, StatusCode::OK, "{n}");
            served.push((n, sha256_hex(&b)));
        }
        // Not a Core file of the run, and not the pending revision.
        for n in ["../run-log.jsonl", "run-log.jsonl", "nothing.json", "streams", "manifest.dsse.json"] {
            let (s, _) = send(&state, "GET", &url(1, n), "agent", "").await;
            assert_eq!(s, StatusCode::NOT_FOUND, "{n}");
        }
        for r in [0, 2] {
            let (s, _) = send(&state, "GET", &url(r, "events.json"), "agent", "").await;
            assert_eq!(s, StatusCode::NOT_FOUND, "revision {r}");
        }
        // The bytes are the ones the manifest then signs.
        send(&state, "POST", &format!("/study/{ID}/publish"), "agent", r#"{"files":[]}"#).await;
        let (_, env) = send(&state, "GET", &url(1, "manifest.dsse.json"), "agent", "").await;
        let st = dsse_payload(&env, &state.sealer.key().public_key_hex());
        for (n, sha) in served {
            let subject = st["subject"].as_array().unwrap().iter().find(|x| x["name"] == n).unwrap();
            assert_eq!(subject["digest"]["sha256"], sha.as_str(), "{n}");
        }
        // Revision 1 is published, so revision 2 is pending but not yet opened.
        let (s, _) = send(&state, "GET", &url(2, "events.json"), "agent", "").await;
        assert_eq!(s, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn measurements_are_served_as_written_with_distinct_refusals() {
        let state = AppState::new("agent".to_string());
        let uri = format!("/study/{ID}/measurements");
        // Unknown study.
        let (s, b) = send(&state, "GET", &uri, "agent", "").await;
        assert_eq!(s, StatusCode::NOT_FOUND);
        assert!(String::from_utf8_lossy(&b).contains("no run"));
        // A run that ends without declaring measurements wrote no file.
        finished_run(&state, "completed", r#""Pass""#, Scope::Agent);
        let (s, b) = send(&state, "GET", &uri, "agent", "").await;
        assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY);
        let body: serde_json::Value = serde_json::from_slice(&b).unwrap();
        assert_eq!(body["code"], "no_measurements");
        let msg = body["error"].as_str().unwrap();
        assert!(msg.contains("declared no measurements") && msg.contains("host study with no parse step"), "{msg}");
        assert!(!msg.contains("cannot have"), "{msg}");
        // With the file Core wrote, the bytes come back unchanged, no publish run.
        let written = br#"{"schema":1,  "behaviours":{"x":[1,2]}}"#;
        std::fs::write(state.run_log.run_dir(ID).join(crate::measurements::FILE), written).unwrap();
        let (s, b) = send(&state, "GET", &uri, "agent", "").await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(b, written);
        // A host study's file (decision 131: a parse per pass of a `Repeat`) is served the same way.
        let host = br#"{"schema":1,"measurements":[{"name":"v","source":{"file":"events.json","step":"read","iteration":2}}]}"#;
        std::fs::write(state.run_log.run_dir(ID).join(crate::measurements::FILE), host).unwrap();
        let (s, b) = send(&state, "GET", &uri, "agent", "").await;
        assert_eq!((s, b.as_slice()), (StatusCode::OK, host.as_slice()));
        // A study still running refuses, even with a file on disk.
        state.study_jobs.lock().unwrap().insert(ID.to_string(), crate::study::StudyJob { status: "running".into(), current_step: None, total_steps: None, reason: None });
        let (s, b) = send(&state, "GET", &uri, "agent", "").await;
        assert_eq!(s, StatusCode::CONFLICT);
        assert!(String::from_utf8_lossy(&b).contains("still running"));
    }

    /// What a caller keeps of a published revision: the manifest and every
    /// Core run file it names, fetched through the file route.
    async fn keep_revision(state: &AppState, revision: u64) -> Vec<(String, Vec<u8>)> {
        let (_, env) = send(state, "GET", &format!("/study/{ID}/publish/{revision}/files/manifest.dsse.json"), "agent", "").await;
        let st = dsse_payload(&env, &state.sealer.key().public_key_hex());
        let mut kept = vec![(MANIFEST_FILE.to_string(), env)];
        for s in st["subject"].as_array().unwrap() {
            let n = s["name"].as_str().unwrap();
            if PUBLISH_FILES.contains(&n) {
                continue;
            }
            let (_, b) = send(state, "GET", &format!("/study/{ID}/publish/{revision}/files/{n}"), "agent", "").await;
            kept.push((n.to_string(), b));
        }
        kept
    }

    async fn put(state: &AppState, name: &str, bytes: &[u8]) -> StatusCode {
        let req = axum::http::Request::builder()
            .method("PUT")
            .uri(format!("/study/{ID}/restore/{name}"))
            .header("authorization", "Bearer agent")
            .body(axum::body::Body::from(bytes.to_vec()))
            .unwrap();
        crate::api::build_router(state.clone()).oneshot(req).await.unwrap().status()
    }

    async fn published_then_swept() -> (AppState, Vec<(String, Vec<u8>)>) {
        let state = AppState::new("agent".to_string());
        finished_run(&state, "completed", r#""Pass""#, Scope::Agent);
        send(&state, "POST", &format!("/study/{ID}/report-data"), "agent", "").await;
        let (s, _) = send(&state, "POST", &format!("/study/{ID}/publish"), "agent", r#"{"files":[]}"#).await;
        assert_eq!(s, StatusCode::OK);
        let kept = keep_revision(&state, 1).await;
        std::fs::remove_dir_all(state.run_log.run_dir(ID)).unwrap();
        (state, kept)
    }

    #[tokio::test]
    async fn a_run_swept_after_its_publish_republishes_from_the_earlier_zips_files() {
        let (state, kept) = published_then_swept().await;
        let first = sha256_hex(&kept[0].1);
        // Swept and nothing handed back: a 409 that says what to do, not a 500.
        let (s, b) = send(&state, "POST", &format!("/study/{ID}/report-data"), "agent", "").await;
        assert_eq!(s, StatusCode::CONFLICT, "{}", String::from_utf8_lossy(&b));
        for (n, bytes) in &kept {
            assert_eq!(put(&state, n, bytes).await, StatusCode::OK, "{n}");
            // Partway through, report-data names what is missing instead of signing a short manifest.
            if n == MANIFEST_FILE {
                let (s, _) = send(&state, "POST", &format!("/study/{ID}/report-data"), "agent", "").await;
                assert_eq!(s, StatusCode::CONFLICT);
            }
        }
        let (s, b) = send(&state, "POST", &format!("/study/{ID}/report-data"), "agent", "").await;
        assert_eq!(s, StatusCode::OK, "{}", String::from_utf8_lossy(&b));
        assert_eq!(json(&b)["revision"], 2);
        let (s, b) = send(&state, "POST", &format!("/study/{ID}/publish"), "agent", r#"{"files":[]}"#).await;
        assert_eq!(s, StatusCode::OK, "{}", String::from_utf8_lossy(&b));
        let (_, env) = send(&state, "GET", &format!("/study/{ID}/publish/2/files/manifest.dsse.json"), "agent", "").await;
        let st = dsse_payload(&env, &state.sealer.key().public_key_hex());
        assert_eq!(st["predicate"]["revision"], 2);
        assert_eq!(st["predicate"]["supersedes"]["revision"], 1);
        assert_eq!(st["predicate"]["supersedes"]["manifest_sha256"], first.as_str());
        let names: Vec<&str> = st["subject"].as_array().unwrap().iter().map(|s| s["name"].as_str().unwrap()).collect();
        for want in ["events.json", "record.dsse.json", "streams/tap.bin", "report-data.json"] {
            assert!(names.contains(&want), "{want} in {names:?}");
        }
    }

    #[tokio::test]
    async fn a_restore_refuses_a_manifest_the_log_never_recorded_a_stray_file_and_wrong_bytes() {
        let (state, kept) = published_then_swept().await;
        // A file before any manifest was accepted.
        assert_eq!(put(&state, "events.json", &kept[1].1).await, StatusCode::CONFLICT);
        // A manifest whose hash the run log never recorded for this run.
        let mut forged = kept[0].1.clone();
        let last = forged.len() - 3;
        forged[last] ^= 1;
        assert_eq!(put(&state, MANIFEST_FILE, &forged).await, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(put(&state, MANIFEST_FILE, &kept[0].1).await, StatusCode::OK);
        // A file that is not one of the manifest's subjects, and a revision output.
        assert_eq!(put(&state, "extra.bin", b"x").await, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(put(&state, "report-data.json", b"{}").await, StatusCode::UNPROCESSABLE_ENTITY);
        // A subject whose bytes do not match its digest.
        assert_eq!(put(&state, "streams/tap.bin", b"other bytes").await, StatusCode::UNPROCESSABLE_ENTITY);
        assert!(!state.run_log.run_dir(ID).join("streams/tap.bin").exists());
        assert_eq!(put(&state, "streams/tap.bin", b"raw bytes").await, StatusCode::OK);
        // A run the log does not know is refused before anything is written.
        let req = axum::http::Request::builder()
            .method("PUT")
            .uri("/study/ffffffffffffffffffffffffffffffff/restore/manifest.dsse.json")
            .header("authorization", "Bearer agent")
            .body(axum::body::Body::from(kept[0].1.clone()))
            .unwrap();
        let r = crate::api::build_router(state.clone()).oneshot(req).await.unwrap();
        assert_eq!(r.status(), StatusCode::NOT_FOUND);
    }

    #[test]
    fn names_are_checked() {
        assert!(safe_name("report.typ") && safe_name("streams/a.bin") && safe_name("typst/pkg/lib.typ"));
        for bad in ["", "/a", "../a", "a/../b", "a\\b", "a b", "a//b", "./a"] {
            assert!(!safe_name(bad), "{bad}");
        }
    }
}
