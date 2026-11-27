//! Core's hash-chained call log (decision 122): one entry for every call to a
//! route in [`crate::lease::LEASE_GATED`], a call the lease refuses included,
//! in the run log's line shape (`run_log.rs`, decision 111) in
//! `call-log.jsonl` beside `run-log.jsonl`. No sweep, discard or restart
//! touches it.
//!
//! It is **Core's calls only**: hardware reached without Core (the topology
//! CLI's own hardware feature, a probe tool run by hand) is not in it, and a
//! host or dev-bench study's own steps stay in its `events.json`.
//!
//! The layer ([`call_log_middleware`]) sits outside `lease_middleware` and
//! inside `auth_middleware`, so a refusal is seen and a new gated route is
//! logged with no handler change. A handler that resolves a board says which
//! through [`board_note`].
//!
//! Five reads fold ([`FOLDED_READS`]): the first of a run of identical reads
//! is chained at once, the repeats are counted in memory and written as one
//! `fold` entry when a different call arrives, the lease is released, or Core
//! stops cleanly. A chained entry is never rewritten.
//!
//! **Fail closed (decision 158).** The log is written through a [`Store`]. Core is in the
//! *cannot-log* state from an append that fails, or from free space below the reserved tail
//! ([`CallLog::gate`]); the next append that succeeds with room left clears it. In it every gated
//! route but a cancel answers `503`, and a study in flight starts no new step.

use anyhow::{Context, Result};
use axum::{
    body::Body,
    extract::{FromRequestParts, MatchedPath, Query, RawPathParams, Request, State},
    http::{header::CONTENT_TYPE, HeaderValue, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
    Json,
};
use serde::{Deserialize, Serialize};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex as StdMutex};

use crate::api::AppState;
use crate::run_log::{sha256_hex, verify_lines, GENESIS};
use crate::token_store::Scope;

/// The log's file name beside the token files.
pub const CALL_LOG_FILE: &str = "call-log.jsonl";
/// A JSON request body this small is kept in the entry beside its hash; a
/// larger one, or a body that is not JSON (a firmware upload), is its hash
/// alone. Chosen, not measured: every JSON request body of today's routes is
/// well under it.
pub const BODY_CAP_BYTES: usize = 4096;
/// Entries `GET /call-log` returns in one answer.
pub const MAX_RANGE: usize = 1000;
/// The most request or answer body the layer will buffer to hash it. The
/// largest body Core takes is a firmware image; the flash handler already
/// holds one whole in memory.
const MAX_BODY_BYTES: usize = 256 * 1024 * 1024;

/// The reads that fold (decision 122); every other gated route is an action.
pub const FOLDED_READS: &[(&str, &str)] = &[
    ("POST", "/live/mem-read"),
    ("POST", "/live/core-state"),
    ("GET", "/serial-log"),
    ("POST", "/validate"),
    ("GET", "/dev-bench/hello"),
];

/// What a `start` entry's `previous` says of the run before it (decision 156).
pub const PREVIOUS_CLEAN: &str = "clean stop";
pub const PREVIOUS_NOT_CLEAN: &str = "not a clean stop";
pub const PREVIOUS_ANEW: &str = "log started anew";
/// A start entry's `last_images`: the image log lives in memory, so after a start no board's last flasher is known.
pub const LAST_IMAGES_UNKNOWN: &str = "unknown";

/// The bytes one reserved-tail entry is budgeted (decision 158). Assumed, not measured: a `safe_off`
/// entry holds one declaration's line, reply and error, well under it.
pub const ENTRY_RESERVE_BYTES: u64 = 8192;
/// The one gated route that is served in the cannot-log state: a cancel always runs.
const CANCEL_ROUTE: (&str, &str) = ("POST", "/study/{study_id}/cancel");

/// Where the log's lines are written and how much room is left, a seam so a test makes a write
/// fail or the disk full on demand.
pub trait Store: Send + Sync {
    /// Appends one line to the file at `path` and makes it durable.
    fn append(&self, path: &Path, line: &[u8]) -> std::io::Result<()>;
    /// Bytes free for Core in the folder `dir`, or `None` when that cannot be read.
    fn free_bytes(&self, dir: &Path) -> Option<u64>;
}

/// The real file and the real disk.
pub struct FileStore;

impl Store for FileStore {
    fn append(&self, path: &Path, line: &[u8]) -> std::io::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let mut f = std::fs::OpenOptions::new().create(true).append(true).open(path)?;
        f.write_all(line)?;
        f.sync_data()
    }

    #[cfg(unix)]
    fn free_bytes(&self, dir: &Path) -> Option<u64> {
        use std::os::unix::ffi::OsStrExt;
        let c = std::ffi::CString::new(dir.as_os_str().as_bytes()).ok()?;
        let mut v: libc::statvfs = unsafe { std::mem::zeroed() };
        // SAFETY: `c` is a NUL-terminated path and `v` a zeroed `statvfs` the call fills.
        (unsafe { libc::statvfs(c.as_ptr(), &mut v) } == 0).then(|| (v.f_bavail as u64).saturating_mul(v.f_frsize as u64))
    }

    #[cfg(windows)]
    fn free_bytes(&self, dir: &Path) -> Option<u64> {
        use std::os::windows::ffi::OsStrExt;
        let wide: Vec<u16> = dir.as_os_str().encode_wide().chain(std::iter::once(0)).collect();
        let mut free: u64 = 0;
        // SAFETY: `wide` is NUL-terminated and `free` is a valid out pointer; the other two are optional.
        let ok = unsafe { windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW(wide.as_ptr(), &mut free, std::ptr::null_mut(), std::ptr::null_mut()) };
        (ok != 0).then_some(free)
    }

    #[cfg(not(any(unix, windows)))]
    fn free_bytes(&self, _dir: &Path) -> Option<u64> {
        None
    }
}

/// Why Core cannot log, and whether it is the space check (no entry is tried) or a failed write
/// (the next entry is the probe).
#[derive(Debug, Clone, PartialEq, Eq)]
struct Cannot {
    reason: String,
    space: bool,
}

/// What [`CallLog::gate`] says of a new gated call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Gate {
    Open,
    /// Core cannot log: `probe` is whether the refusal's own entry is tried, to find out if writes work again.
    Refused { reason: String, probe: bool },
}

/// What the graceful stop did before the safe-off, for the `stop` entry.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StopNote {
    /// The id of the study or Test run in flight that was cancelled, if one was.
    pub cancelled_study: Option<String>,
    /// Whether that study had ended (its teardown run) before the safe-off. `true` when none was in flight.
    pub study_ended: bool,
    /// Whether `hw_lock` was still held when the wait for it ran out: the safe-off ran regardless.
    pub hw_lock_held: bool,
}

/// The response headers a gated route's answer carries.
pub const SEQ_HEADER: &str = "x-embarch-call-seq";
pub const HASH_HEADER: &str = "x-embarch-call-hash";
/// Present on a folded repeat: which repeat of the first entry this is (1 for the first repeat).
pub const REPEAT_HEADER: &str = "x-embarch-call-repeat";

#[derive(Debug, Clone, Default)]
struct BoardInfo {
    role: Option<String>,
    probe_serial: Option<String>,
    chip: Option<String>,
}

/// A handle a handler fills with the board it resolved. Detached (a no-op)
/// outside a logged call, so a handler can always call it.
#[derive(Debug, Clone, Default)]
pub struct BoardNote(Arc<StdMutex<Option<BoardInfo>>>, Arc<StdMutex<Option<String>>>);

impl BoardNote {
    /// Names the board this call reached; a later call fills in what it knows
    /// and keeps what an earlier one set.
    pub fn set(&self, role: Option<&str>, probe_serial: Option<&str>, chip: Option<&str>) {
        let mut g = self.0.lock().unwrap();
        let b = g.get_or_insert_with(BoardInfo::default);
        let own = |v: Option<&str>| v.filter(|s| !s.is_empty()).map(str::to_string);
        b.role = own(role).or(b.role.take());
        b.probe_serial = own(probe_serial).or(b.probe_serial.take());
        b.chip = own(chip).or(b.chip.take());
    }

    /// Says this call put a new image on `chip`: once the call has its call-log
    /// entry, the middleware writes that image's `flashed-images.jsonl` line
    /// with the entry's `seq`.
    pub fn flashed(&self, chip: &str) {
        *self.1.lock().unwrap() = Some(chip.to_string());
    }

    fn take(&self) -> Option<BoardInfo> {
        self.0.lock().unwrap().take()
    }
}

tokio::task_local! {
    static NOTE: BoardNote;
}

/// The note of the call being logged, for a handler to name its board in. Clone it
/// into a `spawn_blocking` closure: a task-local does not cross into one.
pub fn board_note() -> BoardNote {
    NOTE.try_with(|n| n.clone()).unwrap_or_default()
}

struct Fold {
    key: String,
    first_seq: u64,
    first_hash: String,
    repeats: u64,
    first_repeat_ms: u64,
    last_repeat_ms: u64,
    /// What the fold entry repeats of the first entry, for a reader.
    summary: serde_json::Value,
}

struct Inner {
    /// The head and length, read from the file once and kept: a call appends
    /// without re-reading a log that grows with every call.
    chain: Option<(String, u64)>,
    fold: Option<Fold>,
    /// Set while Core cannot log (decision 158); the next append that succeeds with room left clears it.
    cannot: Option<Cannot>,
    /// The reserved tail as last computed, in bytes.
    reserve: u64,
}

/// Where a call landed, for the response headers.
#[derive(Debug, PartialEq, Eq)]
pub struct Stamp {
    pub seq: u64,
    pub hash: String,
    pub repeat: Option<u64>,
}

/// The call log's head (`GENESIS` when empty) and length at one moment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mark {
    pub head: String,
    pub len: u64,
}

/// A run's note of the call log at its start (decision 111): the seal takes the
/// end note and names the entries made between them.
#[derive(Clone)]
pub struct RunCalls {
    pub log: CallLog,
    pub start: Mark,
}

impl RunCalls {
    /// `None` when the log cannot be read: the run is then sealed without a call-log range.
    pub fn begin(log: &CallLog) -> Option<Self> {
        match log.mark(false) {
            Ok(start) => Some(Self { log: log.clone(), start }),
            Err(e) => {
                tracing::error!("could not note the call log's head at the run's start: {e:#}");
                None
            }
        }
    }

    /// The predicate's `observed.call_log`, and the end note.
    pub fn at_end(&self) -> Option<(serde_json::Value, Mark)> {
        let end = match self.log.mark(true) {
            Ok(m) => m,
            Err(e) => {
                tracing::error!("could not note the call log's head at the run's end: {e:#}");
                return None;
            }
        };
        let any = end.len > self.start.len;
        let v = serde_json::json!({
            "prev": self.start.head, "head": end.head, "length": end.len,
            "first_seq": any.then_some(self.start.len), "last_seq": any.then(|| end.len - 1),
        });
        Some((v, end))
    }
}

#[derive(Clone)]
pub struct CallLog {
    path: PathBuf,
    inner: Arc<StdMutex<Inner>>,
    store: Arc<dyn Store>,
    /// How big the reserved tail is, from the declarations; unset, it is a `start` and a `stop` entry.
    reserve_of: Arc<StdMutex<Option<ReserveFn>>>,
}

type ReserveFn = Arc<dyn Fn() -> u64 + Send + Sync>;

impl CallLog {
    /// A call log in `data_dir`.
    pub fn at(data_dir: PathBuf) -> Self {
        Self::with_store(data_dir, Arc::new(FileStore))
    }

    /// A call log in `data_dir` written through `store`.
    pub fn with_store(data_dir: PathBuf, store: Arc<dyn Store>) -> Self {
        let inner = Inner { chain: None, fold: None, cannot: None, reserve: 2 * ENTRY_RESERVE_BYTES };
        Self { path: data_dir.join(CALL_LOG_FILE), inner: Arc::new(StdMutex::new(inner)), store, reserve_of: Arc::new(StdMutex::new(None)) }
    }

    /// Says how big the reserved tail is: room for one entry per declared output's safe-off plus a
    /// `start` and a `stop` (decision 158). Read again at each [`CallLog::gate`].
    pub fn set_reserve(&self, f: impl Fn() -> u64 + Send + Sync + 'static) {
        *self.reserve_of.lock().unwrap() = Some(Arc::new(f));
    }

    fn reserve_now(&self) -> u64 {
        let f = self.reserve_of.lock().unwrap().clone();
        f.map_or(2 * ENTRY_RESERVE_BYTES, |f| f())
    }

    fn low_space(&self, tail: u64) -> Option<Cannot> {
        let free = self.store.free_bytes(self.path.parent()?)?;
        (free < tail).then(|| Cannot { reason: format!("free space {free} B is below the reserved tail {tail} B"), space: true })
    }

    /// Whether a new gated call may be served (decision 158). Open unless the last append failed
    /// or free space is below the reserved tail; the space check is made afresh each time, so it
    /// clears when room returns.
    pub fn gate(&self) -> Gate {
        let tail = self.reserve_now();
        let mut inner = self.inner.lock().unwrap();
        inner.reserve = tail;
        if let Some(c) = inner.cannot.clone().filter(|c| !c.space) {
            return Gate::Refused { reason: c.reason, probe: true };
        }
        inner.cannot = self.low_space(tail);
        match &inner.cannot {
            None => Gate::Open,
            Some(c) => Gate::Refused { reason: c.reason.clone(), probe: false },
        }
    }

    /// Why Core cannot log now, or `None`: what a study consults before each step.
    pub fn cannot_log(&self) -> Option<String> {
        match self.gate() {
            Gate::Open => None,
            Gate::Refused { reason, .. } => Some(reason),
        }
    }

    /// The folder the log's file is in, for a test that reopens it as a restarted Core would.
    #[cfg(test)]
    pub fn path_dir(&self) -> PathBuf {
        self.path.parent().unwrap().to_path_buf()
    }

    pub(crate) fn lines(&self) -> Result<Vec<serde_json::Value>> {
        let text = match std::fs::read_to_string(&self.path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e).context("failed to read the call log"),
        };
        text.lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| serde_json::from_str(l).context("a call-log line is not JSON"))
            .collect()
    }

    /// Recomputes the whole chain: the head (`None` when empty) and the length.
    pub fn verify(&self) -> Result<std::result::Result<(Option<String>, usize), crate::run_log::ChainError>> {
        verify_lines(&self.lines()?)
    }

    /// The head (`GENESIS` when empty) and length, read from the file on first use.
    fn chain_of(&self, inner: &mut Inner) -> Result<(String, u64)> {
        if inner.chain.is_none() {
            let (head, len) = self
                .verify()?
                .map_err(|e| anyhow::anyhow!("the call log is broken at line {}: {}", e.line, e.reason))?;
            inner.chain = Some((head.unwrap_or_else(|| GENESIS.to_string()), len as u64));
        }
        Ok(inner.chain.clone().unwrap())
    }

    /// The head and length now. With `flush`, a pending fold is written first, so
    /// the repeats counted so far fall before the mark.
    pub fn mark(&self, flush: bool) -> Result<Mark> {
        let mut inner = self.inner.lock().unwrap();
        if flush {
            self.flush_locked(&mut inner)?;
        }
        let (head, len) = self.chain_of(&mut inner)?;
        Ok(Mark { head, len })
    }

    /// The entries with `seq` in `start.len..end.len`, as `call-log-segment.jsonl`
    /// bytes (decision 117): a header line naming the entry before the range by
    /// its hash and the head at the range's end, then the entries. It is
    /// refused unless the entries chain from `start.head` to `end.head`.
    pub fn segment_bytes(&self, start: &Mark, end: &Mark) -> Result<Vec<u8>> {
        let lines = {
            let _inner = self.inner.lock().unwrap();
            self.lines()?
        };
        anyhow::ensure!(end.len as usize <= lines.len() && start.len <= end.len, "the call log no longer holds the run's range");
        let range = &lines[start.len as usize..end.len as usize];
        let mut prev = start.head.clone();
        for (i, l) in range.iter().enumerate() {
            let e = &l["entry"];
            anyhow::ensure!(
                e["prev"] == prev.as_str()
                    && e["seq"] == start.len + i as u64
                    && l["hash"].as_str() == Some(sha256_hex(&serde_json::to_vec(e)?).as_str()),
                "the call log's entry {} does not chain from the run's start",
                start.len + i as u64
            );
            prev = l["hash"].as_str().unwrap_or_default().to_string();
        }
        anyhow::ensure!(prev == end.head, "the call log's entries do not reach the head the run recorded");
        let header = serde_json::json!({ "call_log_segment": {
            "prev": start.head, "head": end.head,
            "first_seq": (!range.is_empty()).then_some(start.len),
            "last_seq": (!range.is_empty()).then(|| end.len - 1),
        }});
        let mut out = serde_json::to_vec(&header)?;
        out.push(b'\n');
        for l in range {
            out.extend(serde_json::to_vec(l)?);
            out.push(b'\n');
        }
        Ok(out)
    }

    /// Chains `body` onto the head and appends it, with the lock held. A log
    /// that does not verify is not extended: it stays as it is, to be read.
    fn append(&self, inner: &mut Inner, body: serde_json::Value) -> Result<(u64, String)> {
        match self.write_entry(inner, body) {
            Ok(r) => {
                inner.cannot = self.low_space(inner.reserve);
                Ok(r)
            }
            Err(e) => {
                inner.cannot = Some(Cannot { reason: format!("{e:#}"), space: false });
                Err(e)
            }
        }
    }

    fn write_entry(&self, inner: &mut Inner, mut body: serde_json::Value) -> Result<(u64, String)> {
        let (head, len) = self.chain_of(inner)?;
        body["prev"] = head.into();
        body["seq"] = len.into();
        let hash = sha256_hex(&serde_json::to_vec(&body)?);
        let line = serde_json::json!({ "entry": body, "hash": hash });
        self.store.append(&self.path, format!("{}\n", serde_json::to_string(&line)?).as_bytes())?;
        inner.chain = Some((hash.clone(), len + 1));
        Ok((len, hash))
    }

    fn flush_locked(&self, inner: &mut Inner) -> Result<()> {
        let Some(f) = inner.fold.take() else { return Ok(()) };
        if f.repeats == 0 {
            return Ok(());
        }
        let mut body = serde_json::json!({
            "type": "fold", "first_seq": f.first_seq, "count": f.repeats,
            "first_utc_ms": f.first_repeat_ms, "last_utc_ms": f.last_repeat_ms,
        });
        for (k, v) in f.summary.as_object().into_iter().flatten() {
            body[k] = v.clone();
        }
        self.append(inner, body).map(|_| ())
    }

    /// Writes the pending fold, if any: the lease ended, or Core is stopping.
    pub fn flush(&self) {
        let mut inner = self.inner.lock().unwrap();
        if let Err(e) = self.flush_locked(&mut inner) {
            tracing::error!("could not write the call log's fold entry: {e:#}");
        }
    }

    /// Enters the `start` entry (decision 156): how the log's last run ended, the mode, what
    /// the safe-off did and that `last_images` is unknown. The previous run ended cleanly
    /// exactly when the log's last entry is a `stop`; anything else, a fold or a call a kill cut
    /// off included, is [`PREVIOUS_NOT_CLEAN`].
    pub fn record_start(&self, mode: &str, safe_off: &[crate::safe_off::SafeOffResult]) -> Result<u64> {
        let mut inner = self.inner.lock().unwrap();
        let lines = self.lines()?;
        let entry_of = |l: &serde_json::Value| l["entry"].clone();
        let last_stop = lines.iter().rev().map(entry_of).find(|e| e["type"] == "stop");
        let previous = match lines.last().map(entry_of) {
            None => PREVIOUS_ANEW,
            Some(e) if e["type"] == "stop" => PREVIOUS_CLEAN,
            Some(_) => PREVIOUS_NOT_CLEAN,
        };
        let body = serde_json::json!({
            "type": "start", "at_utc_ms": now_ms(), "mode": mode,
            "previous": previous,
            "previous_stop_utc_ms": last_stop.map(|e| e["at_utc_ms"].clone()),
            "last_images": LAST_IMAGES_UNKNOWN,
            "safe_off": safe_off,
        });
        self.append(&mut inner, body).map(|(seq, _)| seq)
    }

    /// Enters the `stop` entry (decision 156) after the pending fold: the studies cancelled,
    /// whether they ended in time, and what the safe-off did.
    pub fn record_stop(&self, mode: &str, stop: &StopNote, safe_off: &[crate::safe_off::SafeOffResult]) -> Result<u64> {
        let mut inner = self.inner.lock().unwrap();
        self.flush_locked(&mut inner)?;
        let body = serde_json::json!({
            "type": "stop", "at_utc_ms": now_ms(), "mode": mode,
            "cancelled_study": stop.cancelled_study, "study_ended": stop.study_ended,
            "hw_lock_held": stop.hw_lock_held, "safe_off": safe_off,
        });
        self.append(&mut inner, body).map(|(seq, _)| seq)
    }

    /// Enters the safe-off Core ran in place of a study's teardown, one `safe_off` entry per result,
    /// into the reserved tail (decision 158): it does not ask [`CallLog::gate`]. `Err` names the
    /// first entry that could not be written; the caller says so to the tracing log.
    pub fn record_safe_off(&self, why: &str, results: &[crate::safe_off::SafeOffResult]) -> Result<usize> {
        let mut inner = self.inner.lock().unwrap();
        for r in results {
            let body = serde_json::json!({ "type": "safe_off", "at_utc_ms": now_ms(), "why": why, "result": r });
            self.append(&mut inner, body)?;
        }
        Ok(results.len())
    }

    /// Enters one call. `entry` carries everything but `prev` and `seq`; `key`
    /// is `Some` for a fold-eligible read and names what makes two reads identical.
    pub(crate) fn record(&self, entry: serde_json::Value, key: Option<String>) -> Result<Stamp> {
        let mut inner = self.inner.lock().unwrap();
        let now = entry["at_utc_ms"].as_u64().unwrap_or(0);
        if let (Some(k), Some(f)) = (&key, inner.fold.as_mut()) {
            if &f.key == k {
                f.repeats += 1;
                if f.repeats == 1 {
                    f.first_repeat_ms = now;
                }
                f.last_repeat_ms = now;
                return Ok(Stamp { seq: f.first_seq, hash: f.first_hash.clone(), repeat: Some(f.repeats) });
            }
        }
        self.flush_locked(&mut inner)?;
        let summary = serde_json::json!({
            "method": entry["method"], "route": entry["route"], "scope": entry["scope"],
            "status": entry["status"], "answer_sha256": entry["answer_sha256"],
        });
        let (seq, hash) = self.append(&mut inner, entry)?;
        inner.fold = key.map(|key| Fold {
            key,
            first_seq: seq,
            first_hash: hash.clone(),
            repeats: 0,
            first_repeat_ms: 0,
            last_repeat_ms: 0,
            summary,
        });
        Ok(Stamp { seq, hash, repeat: None })
    }
}

fn now_ms() -> u64 {
    crate::study::current_utc_ms()
}

fn is_json(parts: &axum::http::request::Parts) -> bool {
    parts
        .headers
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|ct| ct.starts_with("application/json"))
}

/// The `503` of a refused call, naming the call log and why (decision 158).
fn cannot_log_response(reason: &str) -> Response {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        format!("Core cannot write its call log ({reason}), so it refuses hardware calls until it can; a study's cancel and Core's own safe-off still run"),
    )
        .into_response()
}

fn too_large() -> Response {
    (StatusCode::PAYLOAD_TOO_LARGE, "request body too large for Core to log").into_response()
}

/// Logs every call to a gated route (decision 122): see the module doc.
pub async fn call_log_middleware(State(state): State<AppState>, req: Request, next: Next) -> Response {
    let (mut parts, body) = req.into_parts();
    let route = parts.extensions.get::<MatchedPath>().map(|p| p.as_str().to_string());
    let method = parts.method.as_str().to_string();
    let Some(route) = route.filter(|r| crate::lease::is_gated(&method, r)) else {
        return next.run(Request::from_parts(parts, body)).await;
    };

    let started_ms = now_ms();
    let params: serde_json::Map<String, serde_json::Value> = match RawPathParams::from_request_parts(&mut parts, &state).await {
        Ok(p) => p.iter().map(|(k, v)| (k.to_string(), serde_json::Value::String(v.to_string()))).collect(),
        Err(_) => Default::default(),
    };
    let scope = parts.extensions.get::<Scope>().map(|s| s.as_str()).unwrap_or("unknown");
    let lease = state.lease.view().map(|l| serde_json::json!({ "holder": l.holder, "purpose": l.purpose }));

    let Ok(request_bytes) = axum::body::to_bytes(body, MAX_BODY_BYTES).await else {
        return too_large();
    };
    let body_json = (is_json(&parts) && !request_bytes.is_empty() && request_bytes.len() <= BODY_CAP_BYTES)
        .then(|| serde_json::from_slice::<serde_json::Value>(&request_bytes).ok())
        .flatten();
    let body_sha256 = sha256_hex(&request_bytes);

    // Fail closed (decision 158): a cancel always runs; every other gated call is refused while Core cannot log.
    let refused = if (method.as_str(), route.as_str()) == CANCEL_ROUTE { Gate::Open } else { state.call_log.gate() };
    let note = BoardNote::default();
    let req = Request::from_parts(parts, Body::from(request_bytes));
    let response = match &refused {
        Gate::Open => NOTE.scope(note.clone(), next.run(req)).await,
        Gate::Refused { reason, .. } => cannot_log_response(reason),
    };

    let (mut rparts, rbody) = response.into_parts();
    let (answer_sha256, answer) = match axum::body::to_bytes(rbody, MAX_BODY_BYTES).await {
        Ok(b) => (Some(sha256_hex(&b)), b),
        Err(e) => {
            tracing::error!("call log: could not read the answer to hash it: {e}");
            (None, Default::default())
        }
    };
    let status = rparts.status.as_u16();

    let flashed = note.1.lock().unwrap().take();
    let board = note.take().map(|b| {
        let image = b.chip.as_deref().and_then(|c| state.images.all().into_iter().find(|i| i.chip == c)).and_then(|i| i.sha256);
        serde_json::json!({
            "role": b.role, "probe_serial": b.probe_serial, "chip": b.chip,
            "image_sha256": image.map(serde_json::Value::String).unwrap_or_else(|| "unknown".into()),
        })
    });

    let key = FOLDED_READS.iter().any(|(m, p)| *m == method && *p == route).then(|| {
        sha256_hex(
            serde_json::json!([method, route, params, body_sha256, scope, lease, status, answer_sha256])
                .to_string()
                .as_bytes(),
        )
    });
    let entry = serde_json::json!({
        "type": "call", "at_utc_ms": started_ms, "method": method, "route": route, "params": params,
        "body_sha256": body_sha256, "body": body_json, "scope": scope, "lease": lease,
        "status": status, "answer_sha256": answer_sha256, "board": board,
    });
    // A refusal for want of space is not entered: the reserved tail is for the safe-off.
    if matches!(refused, Gate::Refused { probe: false, .. }) {
        return Response::from_parts(rparts, Body::from(answer));
    }
    let recorded = state.call_log.record(entry, key);
    if let Some(chip) = flashed {
        state.images.commit(&chip, scope, recorded.as_ref().ok().map(|s| s.seq));
    }
    match recorded {
        Ok(stamp) => {
            let h = &mut rparts.headers;
            if let (Ok(s), Ok(x)) = (HeaderValue::from_str(&stamp.seq.to_string()), HeaderValue::from_str(&stamp.hash)) {
                h.insert(SEQ_HEADER, s);
                h.insert(HASH_HEADER, x);
            }
            if let Some(r) = stamp.repeat {
                h.insert(REPEAT_HEADER, HeaderValue::from(r));
            }
        }
        // The call has been served (a cancel, or a handler that ran before the write failed); Core is now
        // in the cannot-log state and refuses the next one (decision 158).
        Err(e) => tracing::error!("could not enter the call in the call log: {e:#}"),
    }
    Response::from_parts(rparts, Body::from(answer))
}

#[derive(Deserialize)]
pub struct RangeQuery {
    #[serde(default)]
    from: Option<usize>,
    #[serde(default)]
    to: Option<usize>,
}

#[derive(Serialize)]
pub struct CallLogResponse {
    verified: bool,
    error: Option<String>,
    length: usize,
    head: Option<String>,
    from: usize,
    to: usize,
    /// More entries lie in the asked range than one answer returns ([`MAX_RANGE`]).
    truncated: bool,
    entries: Vec<serde_json::Value>,
    /// Repeats counted in memory and not yet chained: `{first_seq, repeats}`.
    pending_fold: Option<serde_json::Value>,
}

/// A range of entries, `from` inclusive to `to` exclusive (the whole log by
/// default, at most [`MAX_RANGE`]), and the head. Touches no hardware.
// route: GET /call-log
pub async fn call_log_handler(
    State(state): State<AppState>,
    Query(q): Query<RangeQuery>,
) -> Result<Json<CallLogResponse>, (StatusCode, String)> {
    let internal = |e: anyhow::Error| (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}"));
    let log = &state.call_log;
    // Under the lock, so an append cannot land half-written between the reads.
    let inner = log.inner.lock().unwrap();
    let lines = log.lines().map_err(internal)?;
    let pending_fold = inner
        .fold
        .as_ref()
        .filter(|f| f.repeats > 0)
        .map(|f| serde_json::json!({ "first_seq": f.first_seq, "repeats": f.repeats }));
    drop(inner);
    let (verified, error, head) = match verify_lines(&lines).map_err(internal)? {
        Ok((head, _)) => (true, None, head),
        Err(e) => (false, Some(format!("line {}: {}", e.line, e.reason)), None),
    };
    let length = lines.len();
    let from = q.from.unwrap_or(0).min(length);
    let to = q.to.unwrap_or(length).clamp(from, length);
    let truncated = to - from > MAX_RANGE;
    let to = if truncated { from + MAX_RANGE } else { to };
    Ok(Json(CallLogResponse { verified, error, length, head, from, to, truncated, entries: lines[from..to].to_vec(), pending_fold }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::build_router;
    use axum::Router;
    use tower::ServiceExt as _;

    const TOKEN: &str = "test-token";

    fn state() -> AppState {
        AppState::new(TOKEN.to_string())
    }

    async fn call(router: &Router, bearer: &str, method: &str, uri: &str, lease: Option<&str>, body: Option<&str>) -> (StatusCode, axum::http::HeaderMap, String) {
        let mut b = Request::builder().method(method).uri(uri).header("authorization", format!("Bearer {bearer}"));
        if let Some(t) = lease {
            b = b.header(crate::lease::LEASE_HEADER, t);
        }
        if body.is_some() {
            b = b.header("content-type", "application/json");
        }
        let r = router.clone().oneshot(b.body(Body::from(body.unwrap_or("").to_string())).unwrap()).await.unwrap();
        let (p, bd) = r.into_parts();
        let bytes = axum::body::to_bytes(bd, 1 << 20).await.unwrap();
        (p.status, p.headers, String::from_utf8_lossy(&bytes).into_owned())
    }

    fn uri_of(path: &str) -> String {
        path.replace("{role}", "dut").replace("{serial}", "abc").replace("{name}", "console").replace("{study_id}", "abc")
    }

    fn entries(s: &AppState) -> Vec<serde_json::Value> {
        s.call_log.lines().unwrap().into_iter().map(|l| l["entry"].clone()).collect()
    }

    /// Takes the lease as someone else, so every gated call below is refused
    /// before its handler runs: nothing here reaches a board.
    async fn lease_held_elsewhere(router: &Router) {
        let (st, _, _) = call(router, TOKEN, "POST", "/lease", None, Some(r#"{"holder":"other","purpose":"p","ttl_s":60}"#)).await;
        assert_eq!(st, StatusCode::CREATED);
    }

    #[tokio::test]
    async fn every_gated_route_is_logged_once_per_call_refusals_included_with_its_scope() {
        let s = state();
        let router = build_router(s.clone());
        lease_held_elsewhere(&router).await;
        for (n, (method, path)) in crate::lease::LEASE_GATED.iter().enumerate() {
            // Alternate the two tokens: the entry names the scope of the token that called.
            let (bearer, scope) = if n % 2 == 0 { (TOKEN.to_string(), "agent") } else { (format!("person-{TOKEN}"), "person") };
            let (st, h, _) = call(&router, &bearer, method, &uri_of(path), None, Some("{}")).await;
            assert_eq!(st, StatusCode::CONFLICT, "{method} {path}");
            let e = &entries(&s);
            assert_eq!(e.len(), n + 1, "{method} {path} must add exactly one entry");
            assert_eq!(e[n]["route"], *path);
            assert_eq!(e[n]["method"], *method);
            assert_eq!(e[n]["scope"], scope);
            assert_eq!(e[n]["status"], 409);
            assert_eq!(e[n]["lease"]["holder"], "other");
            assert_eq!(h[SEQ_HEADER].to_str().unwrap(), n.to_string());
        }
        assert!(s.call_log.verify().unwrap().is_ok());
    }

    #[tokio::test]
    async fn an_admitted_call_is_logged_with_its_body_and_answer_hash() {
        let s = state();
        let router = build_router(s.clone());
        let (st, h, text) = call(&router, TOKEN, "POST", "/reset", None, Some("{}")).await;
        assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY);
        let e = &entries(&s)[0];
        assert_eq!(e["body"], serde_json::json!({}));
        assert_eq!(e["body_sha256"], sha256_hex(b"{}"));
        assert_eq!(e["answer_sha256"], sha256_hex(text.as_bytes()));
        assert!(e["lease"].is_null());
        assert_eq!(h[HASH_HEADER].to_str().unwrap(), s.call_log.lines().unwrap()[0]["hash"]);
    }

    #[tokio::test]
    async fn a_path_parameter_is_recorded() {
        let s = state();
        let router = build_router(s.clone());
        lease_held_elsewhere(&router).await;
        call(&router, TOKEN, "POST", "/signals/console/exchange", None, Some("{}")).await;
        assert_eq!(entries(&s)[0]["params"], serde_json::json!({ "name": "console" }));
    }

    #[tokio::test]
    async fn ungated_calls_are_not_logged() {
        let s = state();
        let router = build_router(s.clone());
        call(&router, TOKEN, "GET", "/lease", None, None).await;
        call(&router, TOKEN, "GET", "/call-log", None, None).await;
        assert!(entries(&s).is_empty());
    }

    #[tokio::test]
    async fn five_identical_reads_chain_one_entry_and_fold_four() {
        let s = state();
        let router = build_router(s.clone());
        let mut seen = Vec::new();
        for _ in 0..5 {
            let (_, h, _) = call(&router, TOKEN, "POST", "/live/mem-read", None, Some("{}")).await;
            seen.push((h[SEQ_HEADER].to_str().unwrap().to_string(), h[HASH_HEADER].to_str().unwrap().to_string(), h.get(REPEAT_HEADER).map(|v| v.to_str().unwrap().to_string())));
        }
        assert_eq!(entries(&s).len(), 1, "the repeats are counted in memory, not chained");
        assert!(seen.iter().all(|(sq, hs, _)| *sq == seen[0].0 && *hs == seen[0].1), "a repeat names the first entry");
        assert_eq!(seen[0].2, None);
        assert_eq!(seen[4].2.as_deref(), Some("4"));
        let (_, _, text) = call(&router, TOKEN, "GET", "/call-log", None, None).await;
        assert_eq!(serde_json::from_str::<serde_json::Value>(&text).unwrap()["pending_fold"]["repeats"], 4);
        s.call_log.flush();
        let e = entries(&s);
        assert_eq!(e.len(), 2);
        assert_eq!(e[1]["type"], "fold");
        assert_eq!(e[1]["count"], 4);
        assert_eq!(e[1]["first_seq"], 0);
        assert!(s.call_log.verify().unwrap().is_ok());
    }

    #[tokio::test]
    async fn a_different_call_between_two_reads_gives_no_fold() {
        let s = state();
        let router = build_router(s.clone());
        call(&router, TOKEN, "POST", "/live/mem-read", None, Some("{}")).await;
        call(&router, TOKEN, "POST", "/reset", None, Some("{}")).await;
        call(&router, TOKEN, "POST", "/live/mem-read", None, Some("{}")).await;
        s.call_log.flush();
        let e = entries(&s);
        assert_eq!(e.len(), 3);
        assert!(e.iter().all(|x| x["type"] == "call"));
    }

    #[tokio::test]
    async fn the_other_scope_does_not_fold_into_a_read() {
        let s = state();
        let router = build_router(s.clone());
        call(&router, TOKEN, "POST", "/live/mem-read", None, Some("{}")).await;
        call(&router, &format!("person-{TOKEN}"), "POST", "/live/mem-read", None, Some("{}")).await;
        assert_eq!(entries(&s).len(), 2);
    }

    #[tokio::test]
    async fn an_action_never_folds() {
        let s = state();
        let router = build_router(s.clone());
        for _ in 0..3 {
            call(&router, TOKEN, "POST", "/reset", None, Some("{}")).await;
        }
        assert_eq!(entries(&s).len(), 3);
    }

    #[tokio::test]
    async fn the_call_log_route_returns_the_entry_a_call_named_and_the_head() {
        let s = state();
        let router = build_router(s.clone());
        call(&router, TOKEN, "POST", "/reset", None, Some("{}")).await;
        let (_, h, _) = call(&router, TOKEN, "POST", "/reset", None, Some("{}")).await;
        let seq: usize = h[SEQ_HEADER].to_str().unwrap().parse().unwrap();
        let (st, _, text) = call(&router, TOKEN, "GET", &format!("/call-log?from={seq}&to={}", seq + 1), None, None).await;
        assert_eq!(st, StatusCode::OK);
        let v: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(v["verified"], true);
        assert_eq!(v["entries"].as_array().unwrap().len(), 1);
        assert_eq!(v["entries"][0]["hash"], h[HASH_HEADER].to_str().unwrap());
        assert_eq!(v["head"], h[HASH_HEADER].to_str().unwrap());
        assert_eq!(v["length"], 2);
    }

    fn rewrite(s: &AppState, f: impl FnOnce(&mut Vec<String>)) {
        let text = std::fs::read_to_string(&s.call_log.path).unwrap();
        let mut lines: Vec<String> = text.lines().map(str::to_string).collect();
        f(&mut lines);
        std::fs::write(&s.call_log.path, lines.join("\n") + "\n").unwrap();
    }

    async fn three_calls() -> AppState {
        let s = state();
        let router = build_router(s.clone());
        for _ in 0..3 {
            call(&router, TOKEN, "POST", "/reset", None, Some("{}")).await;
        }
        s
    }

    #[tokio::test]
    async fn the_chain_recomputes_and_an_edit_a_removal_or_a_reorder_is_reported() {
        let s = three_calls().await;
        let (head, len) = s.call_log.verify().unwrap().unwrap();
        assert_eq!(len, 3);
        assert_eq!(s.call_log.lines().unwrap()[0]["entry"]["prev"], GENESIS);
        assert_eq!(head.unwrap(), s.call_log.lines().unwrap()[2]["hash"]);

        rewrite(&s, |l| l[1] = l[1].replace("\"status\":422", "\"status\":200"));
        assert_eq!(s.call_log.verify().unwrap().unwrap_err().line, 2);

        let s = three_calls().await;
        rewrite(&s, |l| {
            l.remove(1);
        });
        assert_eq!(s.call_log.verify().unwrap().unwrap_err().line, 2);

        let s = three_calls().await;
        rewrite(&s, |l| l.swap(1, 2));
        assert_eq!(s.call_log.verify().unwrap().unwrap_err().line, 2);
    }

    #[tokio::test]
    async fn a_broken_log_is_not_extended() {
        let s = three_calls().await;
        let fresh = CallLog::at(s.call_log.path.parent().unwrap().to_path_buf());
        rewrite(&s, |l| l[0] = l[0].replace("\"status\":422", "\"status\":200"));
        let mut inner = fresh.inner.lock().unwrap();
        assert!(fresh.append(&mut inner, serde_json::json!({ "type": "call" })).is_err());
    }

    #[tokio::test]
    async fn a_restart_continues_the_chain() {
        let s = three_calls().await;
        let reopened = CallLog::at(s.call_log.path.parent().unwrap().to_path_buf());
        reopened.record(serde_json::json!({ "type": "call", "at_utc_ms": 1 }), None).unwrap();
        assert_eq!(reopened.verify().unwrap().unwrap().1, 4);
    }

    #[tokio::test]
    async fn a_handler_names_the_board_and_the_entry_carries_its_image_hash() {
        let s = state();
        s.images.record(crate::lease::FlashedImage {
            chip: "nRF54L15".into(),
            probe_serial: None,
            path: "p".into(),
            sha256: Some("abc123".into()),
            flashed_at: String::new(),
            lease_holder: None,
        });
        // A stand-in handler on a gated path: nothing reaches a board.
        let router = Router::new()
            .route("/reset", axum::routing::post(|| async {
                board_note().set(Some("dut"), Some("0010"), Some("nRF54L15"));
                "ok"
            }))
            .layer(axum::middleware::from_fn_with_state(s.clone(), call_log_middleware))
            .layer(axum::middleware::from_fn(|mut req: Request, next: Next| async move {
                req.extensions_mut().insert(Scope::Agent);
                next.run(req).await
            }));
        let r = router.clone().oneshot(Request::builder().method("POST").uri("/reset").body(Body::empty()).unwrap()).await.unwrap();
        assert_eq!(r.status(), StatusCode::OK);
        let b = &entries(&s)[0]["board"];
        assert_eq!(b["role"], "dut");
        assert_eq!(b["image_sha256"], "abc123");

        s.images.record(crate::lease::FlashedImage {
            chip: "nRF54L15".into(),
            probe_serial: None,
            path: "p".into(),
            sha256: None,
            flashed_at: String::new(),
            lease_holder: None,
        });
        router.oneshot(Request::builder().method("POST").uri("/reset").body(Body::empty()).unwrap()).await.unwrap();
        assert_eq!(entries(&s)[1]["board"]["image_sha256"], "unknown");
    }

    #[tokio::test]
    async fn a_flash_is_written_to_flashed_images_with_its_call_log_seq_and_scope() {
        let s = state();
        let images = s.images.clone();
        let router = Router::new()
            .route("/flash", axum::routing::post(move || {
                let images = images.clone();
                async move {
                    images.record_detail(
                        crate::lease::FlashedImage {
                            chip: "nRF54L15".into(),
                            probe_serial: None,
                            path: "p".into(),
                            sha256: Some("abc".into()),
                            flashed_at: String::new(),
                            lease_holder: None,
                        },
                        Default::default(),
                    );
                    board_note().flashed("nRF54L15");
                    "ok"
                }
            }))
            .layer(axum::middleware::from_fn_with_state(s.clone(), call_log_middleware))
            .layer(axum::middleware::from_fn(|mut req: Request, next: Next| async move {
                req.extensions_mut().insert(Scope::Agent);
                next.run(req).await
            }));
        let r = router.oneshot(Request::builder().method("POST").uri("/flash").body(Body::empty()).unwrap()).await.unwrap();
        let seq: u64 = r.headers()[SEQ_HEADER].to_str().unwrap().parse().unwrap();
        let reopened = crate::lease::ImageLog::at(s.call_log.path.parent().unwrap());
        let e = &reopened.entries()[0];
        assert_eq!((e.scope.as_str(), e.call_log_seq), ("agent", Some(seq)));
    }

    #[test]
    fn a_note_outside_a_logged_call_is_a_no_op() {
        board_note().set(Some("dut"), None, None);
    }

    #[tokio::test]
    async fn a_body_over_the_cap_is_kept_as_its_hash_alone() {
        let s = state();
        let router = build_router(s.clone());
        let big = format!("{{\"pad\":\"{}\"}}", "x".repeat(BODY_CAP_BYTES));
        call(&router, TOKEN, "POST", "/reset", None, Some(&big)).await;
        let e = &entries(&s)[0];
        assert!(e["body"].is_null());
        assert_eq!(e["body_sha256"], sha256_hex(big.as_bytes()));
    }

    /// A store whose writes fail, and whose free space is set, on demand.
    struct Flaky {
        fail: std::sync::atomic::AtomicBool,
        free: std::sync::atomic::AtomicU64,
    }

    impl Store for Flaky {
        fn append(&self, path: &Path, line: &[u8]) -> std::io::Result<()> {
            if self.fail.load(std::sync::atomic::Ordering::SeqCst) {
                return Err(std::io::Error::other("disk is read-only"));
            }
            FileStore.append(path, line)
        }
        fn free_bytes(&self, _: &Path) -> Option<u64> {
            Some(self.free.load(std::sync::atomic::Ordering::SeqCst))
        }
    }

    fn flaky() -> (AppState, Arc<Flaky>) {
        use std::sync::atomic::{AtomicBool, AtomicU64};
        let s = state();
        let store = Arc::new(Flaky { fail: AtomicBool::new(false), free: AtomicU64::new(u64::MAX) });
        let log = CallLog::with_store(s.call_log.path_dir(), store.clone());
        (AppState { call_log: log, ..s }, store)
    }

    const NOT_CANCEL: &str = "/reset";
    const CANCEL: &str = "/study/abc/cancel";

    #[tokio::test]
    async fn a_failed_write_refuses_every_gated_route_but_a_cancel_and_the_next_good_write_reopens() {
        use std::sync::atomic::Ordering::SeqCst;
        let (s, store) = flaky();
        let router = build_router(s.clone());
        lease_held_elsewhere(&router).await;
        let (st, _, _) = call(&router, TOKEN, "POST", NOT_CANCEL, None, Some("{}")).await;
        assert_eq!(st, StatusCode::CONFLICT);
        store.fail.store(true, SeqCst);
        // This call is served (its handler has run) and its entry fails: Core now cannot log.
        let (st, _, _) = call(&router, TOKEN, "POST", NOT_CANCEL, None, Some("{}")).await;
        assert_eq!(st, StatusCode::CONFLICT);
        assert!(s.call_log.cannot_log().is_some());
        for (method, path) in crate::lease::LEASE_GATED {
            if (*method, *path) == CANCEL_ROUTE {
                continue;
            }
            let (st, _, text) = call(&router, TOKEN, method, &uri_of(path), None, Some("{}")).await;
            assert_eq!(st, StatusCode::SERVICE_UNAVAILABLE, "{method} {path}");
            assert!(text.contains("call log") && text.contains("disk is read-only"), "{text}");
        }
        // A cancel is not refused: it reaches the lease layer, which answers as it always does.
        let (st, _, _) = call(&router, TOKEN, "POST", CANCEL, None, Some("{}")).await;
        assert_eq!(st, StatusCode::CONFLICT);
        assert_eq!(entries(&s).len(), 1, "nothing was written while it failed");
        // The writer recovers: the next call's refusal is the probe that is logged, and the one after is served.
        store.fail.store(false, SeqCst);
        let (st, _, _) = call(&router, TOKEN, "POST", NOT_CANCEL, None, Some("{}")).await;
        assert_eq!(st, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(entries(&s).last().unwrap()["status"], 503);
        let (st, h, _) = call(&router, TOKEN, "POST", NOT_CANCEL, None, Some("{}")).await;
        assert_eq!(st, StatusCode::CONFLICT);
        assert!(h.contains_key(SEQ_HEADER), "served and logged");
        assert!(s.call_log.verify().unwrap().is_ok());
    }

    #[tokio::test]
    async fn a_cancel_that_cannot_be_logged_is_still_served() {
        use std::sync::atomic::Ordering::SeqCst;
        let (s, store) = flaky();
        let router = build_router(s.clone());
        lease_held_elsewhere(&router).await;
        store.fail.store(true, SeqCst);
        let (st, h, _) = call(&router, TOKEN, "POST", CANCEL, None, Some("{}")).await;
        assert_eq!(st, StatusCode::CONFLICT);
        assert!(!h.contains_key(SEQ_HEADER));
    }

    #[tokio::test]
    async fn free_space_below_the_reserved_tail_refuses_without_entering_and_the_safe_off_still_lands() {
        use std::sync::atomic::Ordering::SeqCst;
        let (s, store) = flaky();
        s.call_log.set_reserve(|| 10_000);
        let router = build_router(s.clone());
        lease_held_elsewhere(&router).await;
        store.free.store(9_999, SeqCst);
        let (st, _, text) = call(&router, TOKEN, "POST", NOT_CANCEL, None, Some("{}")).await;
        assert_eq!(st, StatusCode::SERVICE_UNAVAILABLE);
        assert!(text.contains("call log") && text.contains("9999 B"), "{text}");
        assert!(entries(&s).is_empty(), "a refusal for want of space is not entered");
        let (st, _, _) = call(&router, TOKEN, "POST", CANCEL, None, Some("{}")).await;
        assert_eq!(st, StatusCode::CONFLICT, "a cancel is served");
        let result = crate::safe_off::SafeOffResult { kind: "signal", name: "load".into(), line: Some("off".into()), result: crate::safe_off::SENT, reply: None, error: None };
        assert_eq!(s.call_log.record_safe_off("test", &[result.clone(), result]).unwrap(), 2);
        let types: Vec<_> = entries(&s).iter().map(|e| e["type"].as_str().unwrap().to_string()).collect();
        assert_eq!(types, ["call", "safe_off", "safe_off"]);
        // Room returns: the next call is served and logged.
        store.free.store(1_000_000, SeqCst);
        let (st, h, _) = call(&router, TOKEN, "POST", NOT_CANCEL, None, Some("{}")).await;
        assert_eq!(st, StatusCode::CONFLICT);
        assert!(h.contains_key(SEQ_HEADER));
        assert!(s.call_log.verify().unwrap().is_ok());
    }

    #[test]
    fn a_spent_tail_fails_the_safe_off_entry_and_says_so_to_the_caller() {
        use std::sync::atomic::Ordering::SeqCst;
        let (s, store) = flaky();
        store.fail.store(true, SeqCst);
        let result = crate::safe_off::SafeOffResult { kind: "signal", name: "load".into(), line: None, result: crate::safe_off::NONE_DECLARED, reply: None, error: None };
        assert!(s.call_log.record_safe_off("test", &[result]).is_err());
    }
}
