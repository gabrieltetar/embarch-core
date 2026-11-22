//! Core's side of publish (decisions 116 to 119): adopting notes and prose, the
//! signed `report-data.json`, and the signed manifest, for one study run.
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
pub const MANIFEST_FILE: &str = "manifest.dsse.json";
/// The files Core writes into a revision's directory, which a reader may fetch
/// beside whatever the manifest names.
const PUBLISH_FILES: [&str; 4] = [REPORT_DATA_FILE, REPORT_DATA_ENVELOPE_FILE, SEGMENT_FILE, MANIFEST_FILE];

/// The most text one adoption takes.
const MAX_ADOPTED_BYTES: usize = 1 << 20;
const MAX_NAME_BYTES: usize = 200;

type Failure = (StatusCode, String);

fn internal(e: impl std::fmt::Display) -> Failure {
    (StatusCode::INTERNAL_SERVER_ERROR, format!("{e}"))
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    crate::signing::hex(&Sha256::digest(bytes))
}

fn is_sha256_hex(s: &str) -> bool {
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

/// The Statement of `record.dsse.json`, decoded. The envelope is Core's own
/// file; one that does not read is a `500`.
fn record_statement(dir: &std::path::Path) -> Result<serde_json::Value, Failure> {
    let env: serde_json::Value = serde_json::from_slice(&std::fs::read(dir.join(RECORD_FILE)).map_err(internal)?).map_err(internal)?;
    let payload = base64::engine::general_purpose::STANDARD.decode(env["payload"].as_str().unwrap_or("")).map_err(internal)?;
    serde_json::from_slice(&payload).map_err(internal)
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
fn attestation(person_acted: bool) -> serde_json::Value {
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
    let dir = state.run_log.run_dir(study_id);
    let events: serde_json::Value = serde_json::from_slice(&std::fs::read(dir.join("events.json")).map_err(internal)?).map_err(internal)?;
    let statement = record_statement(&dir)?;
    let counts = count_outcomes(&events);
    let adopted = adoptions(lines, study_id);
    let go_by = run["go_by"].as_str().unwrap_or("agent");
    let declarations = statement["predicate"]["observed"]["declarations"].clone();
    let person_acted = go_by == "person"
        || publisher == Scope::Person
        || adopted.iter().any(|a| a["scope"] == "person")
        || declarations["entries"].as_array().is_some_and(|e| e.iter().any(|d| d["scope"] == "person"));
    let revision = revision_of(lines, study_id);
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
        "observed": {
            "image_status": statement["predicate"]["observed"]["image_status"],
            "declarations": declarations,
        },
        "claimed": { "build": statement["predicate"]["claimed"]["build"] },
        "core": { "version": env!("CARGO_PKG_VERSION"), "keyid": state.sealer.key().keyid() },
        "record_sha256": run["record_sha256"],
    });
    Ok((data, revision))
}

fn revision_dir(state: &AppState, study_id: &str, revision: u64) -> PathBuf {
    state.run_log.run_dir(study_id).join("publish").join(format!("r{revision}"))
}

fn statement_bytes(subject: Vec<serde_json::Value>, predicate_type: &str, predicate: serde_json::Value) -> Result<Vec<u8>, Failure> {
    serde_json::to_vec(&serde_json::json!({
        "_type": STATEMENT_TYPE, "subject": subject, "predicateType": predicate_type, "predicate": predicate,
    }))
    .map_err(internal)
}

fn write_file(path: &std::path::Path, bytes: &[u8]) -> Result<(), Failure> {
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
    name: String,
    sha256: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PublishRequest {
    /// Every file embarch-api made for the zip, with its hash.
    #[serde(default)]
    files: Vec<ApiFile>,
}

/// The files of the run Core produced, by zip-relative name.
fn core_run_files(dir: &std::path::Path) -> Result<Vec<String>, Failure> {
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

/// Signs the manifest over every file Core produced and records the publish,
/// with the run log's head, in the log.
// route: POST /study/{study_id}/publish
pub async fn publish_handler(
    State(state): State<AppState>,
    UrlPath(study_id): UrlPath<String>,
    scope: Scope,
    Json(req): Json<PublishRequest>,
) -> Result<Json<serde_json::Value>, Failure> {
    let mut seen = std::collections::HashSet::new();
    for f in &req.files {
        if !safe_name(&f.name) || !is_sha256_hex(&f.sha256) {
            return Err((StatusCode::BAD_REQUEST, format!("`{}` needs a relative file name and a 64-hex sha256", f.name)));
        }
        if !seen.insert(f.name.clone()) {
            return Err((StatusCode::BAD_REQUEST, format!("`{}` is listed twice", f.name)));
        }
    }
    tokio::task::spawn_blocking(move || publish(&state, &study_id, scope, req)).await.map_err(internal)?
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
    for f in &req.files {
        if subjects.iter().any(|(n, _)| *n == f.name) || f.name == SEGMENT_FILE || f.name == MANIFEST_FILE {
            return Err((StatusCode::BAD_REQUEST, format!("`{}` is a file Core produces; embarch-api lists only its own", f.name)));
        }
    }
    let report_data_sha256 = sha256_hex(&bytes);
    let record_sha256 = data["record_sha256"].clone();

    let entry = state.run_log.append_with(|lines, head| {
        // Everything has to have held between the check above and now.
        publishable_run(lines, study_id)?;
        if revision_of(lines, study_id) != revision {
            return Err((StatusCode::CONFLICT, format!("revision {revision} was published meanwhile; ask for report-data again")));
        }
        let mut segment = Vec::new();
        for l in lines {
            segment.extend(serde_json::to_vec(l).map_err(internal)?);
            segment.push(b'\n');
        }
        write_file(&rdir.join(SEGMENT_FILE), &segment)?;
        let mut all = subjects.clone();
        all.push((SEGMENT_FILE.to_string(), sha256_hex(&segment)));
        all.sort();
        let subject: Vec<serde_json::Value> =
            all.iter().map(|(n, h)| serde_json::json!({ "name": n, "digest": { "sha256": h } })).collect();
        let supersedes = lines
            .iter()
            .rfind(|l| l["entry"]["type"] == "published" && l["entry"]["study_id"] == study_id)
            .map(|l| serde_json::json!({ "revision": revision - 1, "manifest_sha256": l["entry"]["manifest_sha256"] }));
        let at = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0);
        let mut listed: Vec<serde_json::Value> =
            req.files.iter().map(|f| serde_json::json!({ "name": f.name, "sha256": f.sha256 })).collect();
        listed.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
        let statement = statement_bytes(
            subject,
            MANIFEST_PREDICATE_TYPE,
            serde_json::json!({
                "schema": 1, "study_id": study_id, "revision": revision, "supersedes": supersedes,
                "published_by": scope.as_str(), "published_utc_ms": at,
                "core": { "version": env!("CARGO_PKG_VERSION") },
                "run_log": { "head": head, "length": lines.len() },
                "record_sha256": record_sha256, "report_data_sha256": report_data_sha256,
                "embarch_api_files": listed,
            }),
        )?;
        let envelope = state.sealer.sign_statement(&statement).map_err(internal)?;
        write_file(&rdir.join(MANIFEST_FILE), &envelope)?;
        Ok(serde_json::json!({
            "type": "published", "study_id": study_id, "manifest_sha256": sha256_hex(&envelope),
            "scope": scope.as_str(), "at_utc_ms": at, "revision": revision, "log_head": head,
            "report_data_sha256": report_data_sha256,
        }))
    })?;
    Ok(Json(serde_json::json!({ "revision": revision, "entry": entry, "keyid": state.sealer.key().keyid() })))
}

// ---- fetching what Core produced ---------------------------------------------

/// A file of a revision, by the name the zip gives it: one of Core's four
/// publish files, or any file the revision's signed manifest names. Nothing
/// else is served, so the route cannot read outside what was published.
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
        let env: serde_json::Value = std::fs::read(rdir.join(MANIFEST_FILE))
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .ok_or_else(not_found)?;
        let payload = base64::engine::general_purpose::STANDARD.decode(env["payload"].as_str().unwrap_or("")).map_err(internal)?;
        let manifest: serde_json::Value = serde_json::from_slice(&payload).map_err(internal)?;
        if !manifest["subject"].as_array().is_some_and(|s| s.iter().any(|x| x["name"] == name.as_str())) {
            return Err(not_found());
        }
        state.run_log.run_dir(&study_id).join(&name)
    };
    let bytes = tokio::fs::read(&path).await.map_err(|_| not_found())?;
    Ok(([(header::CONTENT_TYPE, "application/octet-stream")], bytes).into_response())
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
        let dir = state.run_log.run_dir(ID);
        std::fs::create_dir_all(dir.join("streams")).unwrap();
        std::fs::write(dir.join("streams/tap.bin"), b"raw bytes").unwrap();
        std::fs::write(
            dir.join("events.json"),
            format!(r#"{{"study_name":"fixture","steps":[{{"outcome":{outcome}}}],"provenance":{{"dev_bench_version":"gbench1"}}}}"#),
        )
        .unwrap();
        state.run_log.begin(ID, "device", go);
        let job = crate::signing::SealJob { sealer: state.sealer.clone(), kind: "device", build: None, submitted_by: Some(go) };
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
        for want in ["events.json", "record.dsse.json", "streams/tap.bin", "report-data.json", "report-data.dsse.json", "run-log-segment.jsonl"] {
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

    #[test]
    fn names_are_checked() {
        assert!(safe_name("report.typ") && safe_name("streams/a.bin") && safe_name("typst/pkg/lib.typ"));
        for bad in ["", "/a", "../a", "a/../b", "a\\b", "a b", "a//b", "./a"] {
            assert!(!safe_name(bad), "{bad}");
        }
    }
}
