//! Core's hash-chained run log (decision 111): one entry per study run, device
//! and host, plus an entry for each publish and each discard of a run's
//! captures, every entry carrying the hash of the one before it.
//!
//! The log lives beside the token files (`token_store::local_data_dir`) as
//! `run-log.jsonl` and **no retention sweep, discard or restart touches it**:
//! the sweep walks `study_results/`, and a discard deletes a run's capture
//! files, never a log line. A line is `{"entry": {...}, "hash": "<sha256 hex>"}`
//! where `hash` is the SHA-256 of the entry's serialisation (`serde_json`'s map
//! is ordered by key, so it is reproducible) and the entry's `prev` is the
//! previous line's `hash`, all zeros for the first.

use anyhow::{Context, Result};
use axum::{
    extract::{Path as UrlPath, State},
    http::StatusCode,
    Json,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex as StdMutex, OnceLock};

use crate::api::AppState;
use crate::token_store::Scope;

/// The log's file name beside the token files.
pub const RUN_LOG_FILE: &str = "run-log.jsonl";
/// `prev` of the first entry.
pub const GENESIS: &str = "0000000000000000000000000000000000000000000000000000000000000000";

#[derive(Clone)]
pub struct RunLog {
    data_dir: PathBuf,
    /// The kind (`device` or `host`) of each run in flight, by study id. A run
    /// is entered into the log when it ends, once, and only if it began here.
    pending: Arc<StdMutex<HashMap<String, (&'static str, Scope)>>>,
    write: Arc<StdMutex<()>>,
}

/// What `fail_job` and `finish_job` reach: they hold no `AppState`, and `main`
/// installs the one log. Without it (a test) recording a run end is a no-op.
static INSTALLED: OnceLock<RunLog> = OnceLock::new();

pub fn install(log: RunLog) {
    let _ = INSTALLED.set(log);
}

pub fn installed() -> Option<&'static RunLog> {
    INSTALLED.get()
}

/// Study ids whose captures retention must leave alone: runs the log knows
/// that have not been published and whose captures have not been discarded.
///
/// `Err` when the log cannot be read or does not verify: the set is then
/// unknown, and the caller must not treat it as empty (decision 111).
pub fn protected_ids() -> Result<HashSet<String>> {
    installed().map(|l| l.unpublished_ids()).unwrap_or_else(|| Ok(HashSet::new()))
}

#[derive(Debug, PartialEq)]
pub enum DiscardError {
    NotFound,
    AlreadyDiscarded,
    /// The agent's token on a run only the person may discard.
    Refused(&'static str),
    Failed(String),
}

#[derive(Debug, PartialEq)]
pub struct ChainError {
    pub line: usize,
    pub reason: String,
}

fn sha256_hex(bytes: &[u8]) -> String {
    crate::signing::hex(&Sha256::digest(bytes))
}

fn now_ms() -> u64 {
    crate::study::current_utc_ms()
}

impl RunLog {
    /// A log under `data_dir`: `run-log.jsonl`, with the runs' directories in
    /// `data_dir/study_results/<id>/`.
    pub fn at(data_dir: PathBuf) -> Self {
        Self { data_dir, pending: Arc::default(), write: Arc::default() }
    }

    pub fn resolve() -> Result<Self> {
        Ok(Self::at(crate::token_store::local_data_dir()?))
    }

    /// A log in a fresh temporary directory, for `AppState::new`.
    #[cfg(test)]
    pub fn temp() -> Self {
        static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("embarch-run-log-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Self::at(dir)
    }

    fn path(&self) -> PathBuf {
        self.data_dir.join(RUN_LOG_FILE)
    }

    pub(crate) fn run_dir(&self, study_id: &str) -> PathBuf {
        self.data_dir.join("study_results").join(study_id)
    }

    /// A run has started: it will be entered when it ends, with the scope of
    /// the token that gave the go (`go_by`, decision 116).
    pub fn begin(&self, study_id: &str, kind: &'static str, go_by: Scope) {
        self.pending.lock().unwrap().insert(study_id.to_string(), (kind, go_by));
    }

    /// Every line, parsed; a missing file is an empty log.
    pub(crate) fn lines(&self) -> Result<Vec<serde_json::Value>> {
        let text = match std::fs::read_to_string(self.path()) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e).context("failed to read the run log"),
        };
        text.lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| serde_json::from_str(l).context("a run-log line is not JSON"))
            .collect()
    }

    /// Recomputes the whole chain: every hash, every `prev`, every `seq`.
    /// Returns the head (`None` for an empty log) and the length.
    pub fn verify(&self) -> Result<std::result::Result<(Option<String>, usize), ChainError>> {
        let lines = self.lines()?;
        let mut prev = GENESIS.to_string();
        for (i, line) in lines.iter().enumerate() {
            let fail = |reason: String| Ok(Err(ChainError { line: i + 1, reason }));
            let (Some(entry), Some(hash)) = (line.get("entry"), line.get("hash").and_then(|h| h.as_str())) else {
                return fail("not an {entry, hash} line".into());
            };
            if sha256_hex(&serde_json::to_vec(entry)?) != hash {
                return fail("the entry does not match its hash: it was altered".into());
            }
            if entry.get("prev").and_then(|p| p.as_str()) != Some(prev.as_str()) {
                return fail("its prev is not the previous entry's hash: an entry was removed, added or reordered".into());
            }
            if entry.get("seq").and_then(|s| s.as_u64()) != Some(i as u64) {
                return fail("its seq is not its position".into());
            }
            prev = hash.to_string();
        }
        Ok(Ok(((!lines.is_empty()).then_some(prev), lines.len())))
    }

    /// Chains `body` onto the head and appends it. Refuses to extend a chain
    /// that does not verify: a broken log stays as it is, for someone to read.
    fn append(&self, body: serde_json::Value) -> Result<serde_json::Value> {
        let _w = self.write.lock().unwrap();
        self.append_locked(body)
    }

    /// `append` for a caller already holding the write lock.
    fn append_locked(&self, mut body: serde_json::Value) -> Result<serde_json::Value> {
        let (head, len) = self.verify()?.map_err(|e| anyhow::anyhow!("the run log is broken at line {}: {}", e.line, e.reason))?;
        body["prev"] = head.unwrap_or_else(|| GENESIS.to_string()).into();
        body["seq"] = len.into();
        let hash = sha256_hex(&serde_json::to_vec(&body)?);
        let line = serde_json::json!({ "entry": body, "hash": hash });
        std::fs::create_dir_all(&self.data_dir)?;
        let mut f = std::fs::OpenOptions::new().create(true).append(true).open(self.path())?;
        f.write_all(format!("{}\n", serde_json::to_string(&line)?).as_bytes())?;
        f.sync_data()?;
        Ok(line)
    }

    /// Enters a run that has ended, if it began here: its outcome, the hash of
    /// `record.dsse.json` when it was signed, and the hash of every capture
    /// file, which a later discard leaves in place (decision 111).
    pub fn end(&self, study_id: &str, status: &str, reason: Option<&str>) {
        let Some((kind, go_by)) = self.pending.lock().unwrap().remove(study_id) else { return };
        let dir = self.run_dir(study_id);
        let record = crate::signing::sha256_of_file(&dir.join(crate::signing::RECORD_FILE)).ok();
        let captures = capture_hashes(&dir).unwrap_or_default();
        let clean = status == "completed" && steps_all_passed(&dir);
        let body = serde_json::json!({
            "type": "run", "study_id": study_id, "kind": kind, "status": status, "reason": reason,
            "clean": clean, "ended_utc_ms": now_ms(), "record_sha256": record, "captures": captures,
            "go_by": go_by.as_str(),
        });
        if let Err(e) = self.append(body) {
            // The run stays on disk and unprotected from the sweep; say so loudly.
            tracing::error!(study_id, "could not enter the run in the run log: {e:#}");
        }
    }

    /// The latest entry of `kind` for `study_id`, by line.
    pub(crate) fn latest(&self, study_id: &str, kind: &str) -> Result<Option<serde_json::Value>> {
        Ok(self
            .lines()?
            .into_iter()
            .rev()
            .find(|l| l["entry"]["type"] == kind && l["entry"]["study_id"] == study_id))
    }

    /// Runs the log knows that are neither published nor discarded. A log that
    /// cannot be read, parsed or verified is an error, never an empty set.
    pub fn unpublished_ids(&self) -> Result<HashSet<String>> {
        let mut out = HashSet::new();
        self.verify()?.map_err(|e| anyhow::anyhow!("the run log is broken at line {}: {}", e.line, e.reason))?;
        for l in self.lines()? {
            let e = &l["entry"];
            let Some(id) = e["study_id"].as_str() else { continue };
            match e["type"].as_str() {
                Some("run") => {
                    out.insert(id.to_string());
                }
                Some("published" | "discarded") => {
                    out.remove(id);
                }
                _ => {}
            }
        }
        Ok(out)
    }

    /// Records the hash of the manifest a run was published under.
    pub fn publish(&self, study_id: &str, manifest_sha256: &str, scope: Scope) -> Result<serde_json::Value, (StatusCode, String)> {
        let internal = |e: anyhow::Error| (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}"));
        if self.latest(study_id, "run").map_err(internal)?.is_none() {
            return Err((StatusCode::NOT_FOUND, format!("the run log has no run {study_id}")));
        }
        let body = serde_json::json!({
            "type": "published", "study_id": study_id, "manifest_sha256": manifest_sha256,
            "scope": scope.as_str(), "at_utc_ms": now_ms(),
        });
        self.append(body).map_err(internal)
    }

    /// Records that a note or a prose block was adopted for a run: the token's
    /// scope and the SHA-256 of the text, never the text (decision 117).
    pub fn adopt(&self, study_id: &str, kind: &str, name: &str, text: &str, scope: Scope) -> Result<serde_json::Value, (StatusCode, String)> {
        let internal = |e: anyhow::Error| (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}"));
        if self.latest(study_id, "run").map_err(internal)?.is_none() {
            return Err((StatusCode::NOT_FOUND, format!("the run log has no run {study_id}")));
        }
        let body = serde_json::json!({
            "type": "adopted", "study_id": study_id, "kind": kind, "name": name,
            "text_sha256": sha256_hex(text.as_bytes()), "scope": scope.as_str(), "at_utc_ms": now_ms(),
        });
        self.append(body).map_err(internal)
    }

    /// Appends the entry `build` makes, under the write lock, from the log as it
    /// stands: every line before the new one, and the head they end at. A run
    /// that ends meanwhile cannot slip between the head a publish records and
    /// its own entry (decision 118).
    pub(crate) fn append_with<F>(&self, build: F) -> Result<serde_json::Value, (StatusCode, String)>
    where
        F: FnOnce(&[serde_json::Value], Option<&str>) -> Result<serde_json::Value, (StatusCode, String)>,
    {
        let internal = |e: anyhow::Error| (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}"));
        let _w = self.write.lock().unwrap();
        let (head, _) = self
            .verify()
            .map_err(internal)?
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("the run log is broken at line {}: {}", e.line, e.reason)))?;
        let lines = self.lines().map_err(internal)?;
        let body = build(&lines, head.as_deref())?;
        self.append_locked(body).map_err(internal)
    }

    /// Deletes a run's capture files and records that, with the scope of the
    /// token that did it. A run that did not end clean, or that a publish
    /// rests on, is the person's to discard; the agent's token is refused and
    /// nothing is deleted. The run's entry and its capture hashes stay.
    pub fn discard(&self, study_id: &str, scope: Scope) -> std::result::Result<serde_json::Value, DiscardError> {
        let fail = |e: anyhow::Error| DiscardError::Failed(format!("{e:#}"));
        let run = self.latest(study_id, "run").map_err(fail)?.ok_or(DiscardError::NotFound)?;
        if self.latest(study_id, "discarded").map_err(fail)?.is_some() {
            return Err(DiscardError::AlreadyDiscarded);
        }
        if scope == Scope::Agent {
            if self.latest(study_id, "published").map_err(fail)?.is_some() {
                return Err(DiscardError::Refused("a published report rests on this run's captures"));
            }
            if run["entry"]["clean"] != true {
                return Err(DiscardError::Refused("this run failed, errored or was inconclusive"));
            }
        }
        let streams = self.run_dir(study_id).join("streams");
        let mut removed = 0u64;
        if streams.is_dir() {
            for e in std::fs::read_dir(&streams).map_err(|e| fail(e.into()))? {
                let e = e.map_err(|e| fail(e.into()))?;
                if e.file_type().map_err(|e| fail(e.into()))?.is_file() {
                    std::fs::remove_file(e.path()).map_err(|e| fail(e.into()))?;
                    removed += 1;
                }
            }
        }
        let body = serde_json::json!({
            "type": "discarded", "study_id": study_id, "scope": scope.as_str(),
            "files_removed": removed, "at_utc_ms": now_ms(),
        });
        self.append(body).map_err(fail)
    }
}

/// `{name, sha256}` for every regular file directly under `streams/`, sorted.
fn capture_hashes(dir: &Path) -> Result<Vec<serde_json::Value>> {
    let streams = dir.join("streams");
    let mut names = Vec::new();
    if streams.is_dir() {
        for e in std::fs::read_dir(&streams)? {
            let e = e?;
            if e.file_type()?.is_file() {
                names.push(e.file_name().to_string_lossy().into_owned());
            }
        }
    }
    names.sort();
    names
        .into_iter()
        .map(|n| Ok(serde_json::json!({ "name": n, "sha256": crate::signing::sha256_of_file(&streams.join(&n))? })))
        .collect()
}

/// Whether `events.json` exists and every step's outcome is `Pass`. Anything
/// that cannot be read is not clean, so the doubt falls to the person's token.
fn steps_all_passed(dir: &Path) -> bool {
    let Ok(bytes) = std::fs::read(dir.join("events.json")) else { return false };
    let Ok(v) = serde_json::from_slice::<serde_json::Value>(&bytes) else { return false };
    match v.get("steps").and_then(|s| s.as_array()) {
        Some(steps) => steps.iter().all(|s| s["outcome"] == "Pass"),
        None => false,
    }
}

/// What stays and what goes.
pub type Split<T> = (Vec<(T, PathBuf)>, Vec<(T, PathBuf)>);

/// Splits `dirs` (newest first) into what retention keeps: every protected
/// run, plus the newest `keep` of the rest. `keep == 0` keeps all.
pub fn retained<T>(dirs: Vec<(T, PathBuf)>, keep: usize, protected: &HashSet<String>) -> Split<T> {
    let mut kept = Vec::new();
    let mut gone = Vec::new();
    let mut unprotected = 0;
    for d in dirs {
        let is_protected = d.1.file_name().and_then(|n| n.to_str()).is_some_and(|n| protected.contains(n));
        if is_protected || keep == 0 || unprotected < keep {
            if !is_protected {
                unprotected += 1;
            }
            kept.push(d);
        } else {
            gone.push(d);
        }
    }
    (kept, gone)
}

// ---- routes -----------------------------------------------------------------

#[derive(Serialize)]
pub struct RunLogResponse {
    verified: bool,
    error: Option<String>,
    length: usize,
    head: Option<String>,
    entries: Vec<serde_json::Value>,
}

/// The whole chain, so a reader can recompute it (`verify` check 8).
// route: GET /run-log
pub async fn run_log_handler(State(state): State<AppState>) -> Result<Json<RunLogResponse>, (StatusCode, String)> {
    let internal = |e: anyhow::Error| (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}"));
    let entries = state.run_log.lines().map_err(internal)?;
    let resp = match state.run_log.verify().map_err(internal)? {
        Ok((head, length)) => RunLogResponse { verified: true, error: None, length, head, entries },
        Err(e) => RunLogResponse { verified: false, error: Some(format!("line {}: {}", e.line, e.reason)), length: entries.len(), head: None, entries },
    };
    Ok(Json(resp))
}

#[derive(Serialize)]
pub struct StudyLogResponse {
    run: serde_json::Value,
    published: Option<serde_json::Value>,
    discarded: Option<serde_json::Value>,
    /// The log's head, which a publish records beside the manifest hash.
    head: Option<String>,
    length: usize,
}

/// A run's entry, its latest publish and discard, and the log head.
// route: GET /study/{study_id}/log
pub async fn study_log_handler(State(state): State<AppState>, UrlPath(study_id): UrlPath<String>) -> Result<Json<StudyLogResponse>, (StatusCode, String)> {
    let internal = |e: anyhow::Error| (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}"));
    let log = &state.run_log;
    let run = log.latest(&study_id, "run").map_err(internal)?.ok_or((StatusCode::NOT_FOUND, format!("the run log has no run {study_id}")))?;
    let published = log.latest(&study_id, "published").map_err(internal)?;
    let discarded = log.latest(&study_id, "discarded").map_err(internal)?;
    let (head, length) = log
        .verify()
        .map_err(internal)?
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("the run log is broken at line {}: {}", e.line, e.reason)))?;
    Ok(Json(StudyLogResponse { run, published, discarded, head, length }))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PublishRequest {
    /// Lowercase hex SHA-256 of the manifest the run was published under.
    manifest_sha256: String,
}

/// Records the manifest hash a run was published under; answers its entry.
// route: POST /study/{study_id}/published
pub async fn publish_handler(
    State(state): State<AppState>,
    UrlPath(study_id): UrlPath<String>,
    scope: Scope,
    Json(req): Json<PublishRequest>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let h = &req.manifest_sha256;
    if h.len() != 64 || !h.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)) {
        return Err((StatusCode::BAD_REQUEST, "manifest_sha256 must be 64 lowercase hex characters".into()));
    }
    state.run_log.publish(&study_id, h, scope).map(Json)
}

/// Deletes a run's captures and records it; the agent's token is refused on a
/// run that did not end clean or that a publish rests on (decision 112).
// route: POST /study/{study_id}/discard
pub async fn discard_handler(State(state): State<AppState>, UrlPath(study_id): UrlPath<String>, scope: Scope) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    if state.study_jobs.lock().unwrap().get(&study_id).is_some_and(|j| j.status == "running") {
        return Err((StatusCode::CONFLICT, format!("study {study_id} is still running")));
    }
    let log = state.run_log.clone();
    let id = study_id.clone();
    let out = tokio::task::spawn_blocking(move || log.discard(&id, scope))
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    match out {
        Ok(entry) => Ok(Json(entry)),
        Err(DiscardError::NotFound) => Err((StatusCode::NOT_FOUND, format!("the run log has no run {study_id}"))),
        Err(DiscardError::AlreadyDiscarded) => Err((StatusCode::CONFLICT, format!("study {study_id}'s captures were already discarded"))),
        Err(DiscardError::Refused(why)) => Err((
            StatusCode::FORBIDDEN,
            format!(
                "refused: {why}, and only the person's token may discard its captures. Nothing was deleted. \
                 Ask the person to discard it, or to run it with the person token (decision 112)."
            ),
        )),
        Err(DiscardError::Failed(e)) => Err((StatusCode::INTERNAL_SERVER_ERROR, e)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ID: &str = "0123456789abcdef0123456789abcdef";

    fn run_with(log: &RunLog, id: &str, status: &str, outcome: &str) {
        let dir = log.run_dir(id);
        std::fs::create_dir_all(dir.join("streams")).unwrap();
        std::fs::write(dir.join("streams/tap.bin"), b"raw bytes").unwrap();
        std::fs::write(dir.join("events.json"), format!(r#"{{"steps":[{{"outcome":"{outcome}"}}]}}"#)).unwrap();
        log.begin(id, "device", Scope::Agent);
        log.end(id, status, None);
    }

    fn entries(log: &RunLog) -> Vec<serde_json::Value> {
        log.lines().unwrap().into_iter().map(|l| l["entry"].clone()).collect()
    }

    #[test]
    fn the_chain_recomputes_across_runs_publishes_and_discards_and_a_restart() {
        let dir = tempfile::tempdir().unwrap();
        let log = RunLog::at(dir.path().to_path_buf());
        assert_eq!(log.verify().unwrap(), Ok((None, 0)));
        run_with(&log, "a", "completed", "Pass");
        run_with(&log, "b", "failed", "Fail");
        log.publish("a", &"ab".repeat(32), Scope::Person).unwrap();
        log.discard("b", Scope::Person).unwrap();
        let (head, len) = log.verify().unwrap().unwrap();
        assert_eq!(len, 4);
        let e = entries(&log);
        assert_eq!(e[0]["prev"], GENESIS);
        assert_eq!(e[1]["prev"], serde_json::Value::String(sha256_hex(&serde_json::to_vec(&e[0]).unwrap())));
        assert_eq!(head.unwrap(), sha256_hex(&serde_json::to_vec(&e[3]).unwrap()));
        // A new process opening the same directory sees the same chain.
        let again = RunLog::at(dir.path().to_path_buf());
        assert_eq!(again.verify().unwrap().unwrap().1, 4);
    }

    #[test]
    fn an_altered_removed_or_reordered_entry_breaks_the_chain() {
        let dir = tempfile::tempdir().unwrap();
        let log = RunLog::at(dir.path().to_path_buf());
        for id in ["a", "b", "c"] {
            run_with(&log, id, "completed", "Pass");
        }
        let original = std::fs::read_to_string(log.path()).unwrap();
        let lines: Vec<&str> = original.lines().collect();
        let write = |ls: &[&str]| std::fs::write(log.path(), ls.join("\n") + "\n").unwrap();

        write(&lines);
        assert!(log.verify().unwrap().is_ok());

        write(&[lines[0], lines[2]]);
        assert_eq!(log.verify().unwrap().unwrap_err().line, 2, "a removed entry");

        write(&[lines[0], lines[2], lines[1]]);
        assert_eq!(log.verify().unwrap().unwrap_err().line, 2, "a reordered pair");

        let altered = lines[1].replace("\"completed\"", "\"failed\"");
        write(&[lines[0], &altered, lines[2]]);
        let err = log.verify().unwrap().unwrap_err();
        assert_eq!(err.line, 2);
        assert!(err.reason.contains("altered"), "{}", err.reason);

        // Cutting the first entry leaves a second whose prev is not genesis.
        write(&lines[1..]);
        assert_eq!(log.verify().unwrap().unwrap_err().line, 1);
    }

    #[test]
    fn a_broken_log_is_not_extended() {
        let dir = tempfile::tempdir().unwrap();
        let log = RunLog::at(dir.path().to_path_buf());
        run_with(&log, "a", "completed", "Pass");
        std::fs::write(log.path(), std::fs::read_to_string(log.path()).unwrap().replace("completed", "failed")).unwrap();
        assert!(log.append(serde_json::json!({"type": "run"})).is_err());
    }

    #[test]
    fn a_run_that_did_not_begin_here_is_not_entered_and_one_is_entered_once() {
        let dir = tempfile::tempdir().unwrap();
        let log = RunLog::at(dir.path().to_path_buf());
        log.end("never-began", "failed", None);
        assert!(entries(&log).is_empty());
        run_with(&log, "a", "completed", "Pass");
        log.end("a", "failed", None);
        assert_eq!(entries(&log).len(), 1);
    }

    #[test]
    fn the_agent_is_refused_a_failing_runs_discard_and_the_person_is_not_and_both_leave_the_entry() {
        let dir = tempfile::tempdir().unwrap();
        let log = RunLog::at(dir.path().to_path_buf());
        run_with(&log, "fail", "failed", "Fail");
        run_with(&log, "slow", "completed", "TimedOut");
        let tap = |id: &str| log.run_dir(id).join("streams/tap.bin");
        let hash = entries(&log)[0]["captures"][0]["sha256"].clone();
        assert_eq!(hash, sha256_hex(b"raw bytes"));

        for id in ["fail", "slow"] {
            assert!(matches!(log.discard(id, Scope::Agent), Err(DiscardError::Refused(_))));
            assert!(tap(id).exists(), "a refusal deletes nothing");
        }
        assert_eq!(entries(&log).len(), 2, "a refusal writes nothing");

        let done = log.discard("fail", Scope::Person).unwrap();
        assert_eq!(done["entry"]["scope"], "person");
        assert!(!tap("fail").exists());
        let e = entries(&log);
        assert_eq!(e[0]["captures"][0]["sha256"], hash, "the capture hash stays");
        assert_eq!(e.len(), 3);
        assert_eq!(log.discard("fail", Scope::Person), Err(DiscardError::AlreadyDiscarded));
        assert_eq!(log.discard("nope", Scope::Person), Err(DiscardError::NotFound));
    }

    #[test]
    fn the_agent_may_discard_a_clean_unpublished_run_but_not_a_published_one() {
        let dir = tempfile::tempdir().unwrap();
        let log = RunLog::at(dir.path().to_path_buf());
        run_with(&log, "ok", "completed", "Pass");
        run_with(&log, "pub", "completed", "Pass");
        log.publish("pub", &"cd".repeat(32), Scope::Agent).unwrap();
        assert!(matches!(log.discard("pub", Scope::Agent), Err(DiscardError::Refused(_))));
        let done = log.discard("ok", Scope::Agent).unwrap();
        assert_eq!(done["entry"]["scope"], "agent");
        assert!(log.discard("pub", Scope::Person).is_ok(), "the person may");
    }

    #[test]
    fn publish_records_the_manifest_hash_against_the_run_and_the_head_follows() {
        let dir = tempfile::tempdir().unwrap();
        let log = RunLog::at(dir.path().to_path_buf());
        run_with(&log, ID, "completed", "Pass");
        assert!(log.unpublished_ids().unwrap().contains(ID));
        let fixture = "9f".repeat(32);
        let line = log.publish(ID, &fixture, Scope::Person).unwrap();
        assert_eq!(line["entry"]["manifest_sha256"], fixture.as_str());
        assert_eq!(line["entry"]["study_id"], ID);
        assert!(!log.unpublished_ids().unwrap().contains(ID));
        assert_eq!(log.verify().unwrap().unwrap().0.unwrap(), line["hash"].as_str().unwrap());
        assert!(log.publish("ghost", &fixture, Scope::Person).is_err());
    }

    #[test]
    fn retention_keeps_every_protected_run_and_the_newest_keep_of_the_rest() {
        let d = |n: &str| (0u8, PathBuf::from(n));
        let dirs = vec![d("new"), d("old-unpublished"), d("mid"), d("older"), d("oldest")];
        let protected: HashSet<String> = ["old-unpublished".to_string()].into();
        let (kept, gone) = retained(dirs, 2, &protected);
        let names = |v: &[(u8, PathBuf)]| v.iter().map(|d| d.1.to_str().unwrap().to_string()).collect::<Vec<_>>();
        assert_eq!(names(&kept), ["new", "old-unpublished", "mid"]);
        assert_eq!(names(&gone), ["older", "oldest"]);
    }

    async fn call(state: &AppState, uri: &str, bearer: &str, body: &str) -> (StatusCode, String) {
        use tower::ServiceExt as _;
        let req = axum::http::Request::builder()
            .method("POST")
            .uri(uri)
            .header("authorization", format!("Bearer {bearer}"))
            .header("content-type", "application/json")
            .body(axum::body::Body::from(body.to_string()))
            .unwrap();
        let r = crate::api::build_router(state.clone()).oneshot(req).await.unwrap();
        let status = r.status();
        let bytes = axum::body::to_bytes(r.into_body(), 1 << 20).await.unwrap();
        (status, String::from_utf8(bytes.to_vec()).unwrap())
    }

    #[tokio::test]
    async fn over_http_the_agent_token_is_refused_403_with_a_message_and_the_person_token_succeeds() {
        let state = AppState::new("agent".to_string());
        run_with(&state.run_log, ID, "failed", "Fail");
        let uri = format!("/study/{ID}/discard");

        let (status, msg) = call(&state, &uri, "agent", "").await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert!(msg.contains("only the person's token") && msg.contains("Nothing was deleted"), "{msg}");
        assert!(state.run_log.run_dir(ID).join("streams/tap.bin").exists());

        let (status, body) = call(&state, &uri, "person-agent", "").await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(!state.run_log.run_dir(ID).join("streams/tap.bin").exists());
        assert_eq!(entries(&state.run_log)[0]["captures"][0]["name"], "tap.bin");
    }

    #[tokio::test]
    async fn the_publish_route_records_a_fixture_hash_and_the_log_route_returns_the_entry_and_head() {
        use tower::ServiceExt as _;
        let state = AppState::new("agent".to_string());
        run_with(&state.run_log, ID, "completed", "Pass");
        let fixture = "ab".repeat(32);
        let (status, _) = call(&state, &format!("/study/{ID}/published"), "person-agent", &format!(r#"{{"manifest_sha256":"{fixture}"}}"#)).await;
        assert_eq!(status, StatusCode::OK);
        let (status, _) = call(&state, &format!("/study/{ID}/published"), "agent", r#"{"manifest_sha256":"nothex"}"#).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);

        let req = axum::http::Request::builder()
            .uri(format!("/study/{ID}/log"))
            .header("authorization", "Bearer agent")
            .body(axum::body::Body::empty())
            .unwrap();
        let r = crate::api::build_router(state.clone()).oneshot(req).await.unwrap();
        assert_eq!(r.status(), StatusCode::OK);
        let v: serde_json::Value = serde_json::from_slice(&axum::body::to_bytes(r.into_body(), 1 << 20).await.unwrap()).unwrap();
        assert_eq!(v["published"]["entry"]["manifest_sha256"], fixture.as_str());
        assert_eq!(v["head"], v["published"]["hash"]);
        assert_eq!(v["length"], 2);
    }

    #[test]
    fn the_sweep_removes_a_published_run_past_keep_and_never_an_unpublished_one() {
        let root = tempfile::tempdir().unwrap();
        let mk = |id: &str, age: u64| {
            let d = root.path().join(id);
            std::fs::create_dir_all(&d).unwrap();
            let t = std::time::SystemTime::now() - std::time::Duration::from_secs(age);
            std::fs::File::open(&d).unwrap().set_modified(t).unwrap();
        };
        let (unpub, pubd, newer) = ("a".repeat(32), "b".repeat(32), "c".repeat(32));
        mk(&unpub, 300);
        mk(&pubd, 200);
        mk(&newer, 100);
        let protected: HashSet<String> = [unpub.clone()].into();
        let removed = crate::stream_store::sweep_study_results(root.path(), 1, &protected).unwrap();
        assert_eq!(removed, 1);
        assert!(root.path().join(&unpub).exists(), "an unpublished run's captures stay");
        assert!(!root.path().join(&pubd).exists());
        assert!(root.path().join(&newer).exists());
    }

    #[test]
    fn an_unreadable_or_broken_log_is_an_error_not_an_empty_protected_set() {
        let dir = tempfile::tempdir().unwrap();
        let log = RunLog::at(dir.path().to_path_buf());
        run_with(&log, "a", "completed", "Pass");
        assert!(log.unpublished_ids().unwrap().contains("a"));
        // a corrupted line
        let good = std::fs::read_to_string(log.path()).unwrap();
        std::fs::write(log.path(), format!("{good}not json\n")).unwrap();
        assert!(log.unpublished_ids().is_err());
        // a tampered but parseable line
        std::fs::write(log.path(), good.replace("completed", "failed")).unwrap();
        assert!(log.unpublished_ids().is_err());
        // a directory in the log's place
        std::fs::remove_file(log.path()).unwrap();
        std::fs::create_dir(log.path()).unwrap();
        assert!(log.unpublished_ids().is_err());
    }
}
