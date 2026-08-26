//! Core's bridge between `embarch-api`'s HTTP `/study*` surface and
//! `embarch-dev-bench` firmware's serial link (`dev_bench_link.rs`).
//!
//! `embarch-study-designer/design.md` §5.1 (the `POST /study` async job
//! model) and §5.2 (`events.json`/`data.csv`/`waveform.csv` layout) are the
//! finalized design this module implements. Section references in doc
//! comments below point back into that document.

use axum::extract::{Json, Path, Query, State};
use axum::http::{header::CONTENT_TYPE, StatusCode};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use futures_util::StreamExt as _;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::convert::Infallible;
use std::io::Write as _;
use std::path::{Path as FsPath, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant};
use tokio::sync::broadcast;
use tokio_stream::wrappers::{errors::BroadcastStreamRecvError, BroadcastStream};

use embarch_study_designer::{
    limits::{MAX_FIRMWARE_VERSION_LEN, MAX_VERSION_OVERRIDES},
    dev_bench_log_tap, requirement_satisfied, samples_in, steps_crc, streams_crc,
    validate_taps, DevBenchMessage, GattTranscriptEntry, Provenance, Requirements, Sample,
    StepResult, StreamEncoding, StreamRecord, StreamRef, StreamSource, StreamTap, Study,
    VersionOverride, VersionSource, VersionSubject,
    DEV_BENCH_WIRE_SCHEMA_VERSION,
};

use crate::api::{internal_err, AppState};
use crate::dev_bench_link::DevBenchLink;
use crate::stream_store::{self, StreamStore};
use crate::token_store;

/// Host-side watchdog grace margin, on top of a step's own `timeout_ms`
/// (`embarch-study-designer/design.md` §3 decision 16's amendment, §7). §7
/// documents this exact constant as an unsized placeholder, not a validated
/// value — 2000ms is what's used until real dev-bench timing narrows it.
const WATCHDOG_GRACE_MS: u64 = 2_000;

/// How long Core waits for `HelloAck` after sending `Hello` before giving up
/// on the handshake. Core's own choice (the design doc doesn't specify one),
/// generous enough to cover a slow dev-bench boot.
const HANDSHAKE_TIMEOUT_MS: u64 = 5_000;

/// In-memory job registry (`AppState::study_jobs`, design.md §5.1). No
/// expiry/cleanup by design — entries live until Core restarts.
pub type JobRegistry = Arc<StdMutex<HashMap<String, StudyJob>>>;

/// One-study-at-a-time lock (`AppState::study_lock`), explicitly separate
/// from `hw_lock` — a different physical connection
/// (`embarch-core/design.md` §3 decision 15). `Some(study_id)` while a study
/// is in flight.
pub type StudyLock = Arc<StdMutex<Option<String>>>;

/// Live-progress state kept in the job registry. **Never holds a full
/// `StudyResult`** — measured at ~1.3 MB purely from `embarch-study-
/// designer`'s no_std worst-case capacity fields (`heapless::Vec<StepResult,
/// 64>` × `StepResult`'s own `gatt_activity` capacity), a value that's
/// genuinely unsafe to clone by value on a normal thread stack, which is
/// exactly what every `GET /study/{study_id}` call used to do
/// (`embarch-study-designer/design.md` §7's stack-overflow finding). A
/// completed study's actual result lives only in `events.json` on disk
/// (written incrementally by [`EventsJsonWriter`] as each step arrives, not
/// assembled from this struct) — [`get_study_handler`] reads it back from
/// there, the same pattern `power_data_handler`/`waveform_data_handler`
/// already use for `data.csv`/`waveform.csv`.
#[derive(Debug, Clone)]
pub struct StudyJob {
    /// `"running" | "completed" | "failed"` (`"pending"` is never actually
    /// used: an entry is only ever created once the dev-bench handshake has
    /// already succeeded and `StudyStart` has already been sent, so by the
    /// time a caller can observe it, the study is genuinely running).
    pub status: String,
    pub current_step: Option<u32>,
    pub total_steps: Option<u32>,
    pub reason: Option<String>,
}

/// What `GET /study/{study_id}` actually answers over HTTP — same shape
/// callers already depended on (`status`/`current_step`/`total_steps`/
/// `result`/`reason`), just no longer backed by a resident `StudyResult`:
/// `result` is filled in by reading `events.json` off disk at request time,
/// only once `status` is `"completed"`.
#[derive(Serialize)]
pub struct StudyJobResponse {
    pub status: String,
    pub current_step: Option<u32>,
    pub total_steps: Option<u32>,
    pub result: Option<serde_json::Value>,
    pub reason: Option<String>,
}

#[derive(Serialize)]
pub struct StudyAcceptedResponse {
    study_id: String,
    status: &'static str,
}

/// One live event pushed to every subscriber of `GET /study/{study_id}/
/// events` (SSE) the instant Core processes it off the dev-bench link —
/// never buffered until the study finishes. Mirrors `embarch-topology`'s own
/// durable-log-plus-live-push shape (`embarch-topology/design.md` §3
/// decision 12: write it to disk *and* push it live if anyone's watching)
/// applied here to study progress instead of a topology mismatch. Every
/// variant carries `study_id` so a subscriber that reconnects across studies
/// can tell them apart, even though today only one study is ever in flight
/// at a time (`StudyLock`).
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind")]
pub enum StudyEvent {
    /// A step just completed — the same `StepResult` [`EventsJsonWriter`]
    /// just appended to `events.json`, pushed live rather than only
    /// discoverable by polling `GET /study/{study_id}` afterward. A single
    /// `StepResult` is at most tens of KB (bounded by `limits::
    /// MAX_PAYLOAD_LEN`/`MAX_GATT_ACTIVITY_RECORDS`) — nowhere near the
    /// ~1.3 MB whole-study worst case, but still `clippy::
    /// large_enum_variant`-large next to this enum's other, small variants,
    /// so it's boxed rather than inline: one heap allocation per step
    /// (`MAX_STEPS_PER_STUDY` = 64 at most per study) is a non-issue: the
    /// point of boxing here is keeping `StudyEvent` itself small to move
    /// around and clone, not avoiding a stack overflow the way the fields
    /// this whole rework removed from `StudyJob` did.
    StepCompleted { study_id: String, step_index: u32, result: Box<StepResult> },
    /// One batch of power/waveform samples, pushed the instant Core decodes
    /// it off the wire — the same samples [`write_sample`] is appending to
    /// `data.csv`/`waveform.csv` in the same pass, not held back until the
    /// step or study finishes.
    ///
    /// Keyed by the tap that produced them (`stream_id` is its index in
    /// `Study.streams`, `stream_name` its declared name) rather than by the
    /// retired `StreamChannel` — `embarch-study-designer/design.md` §3
    /// decision 39.
    SampleBatch { study_id: String, stream_id: u8, stream_name: String, samples: Vec<Sample> },
    /// One GATT transcript entry, pushed the instant Core decodes it off the
    /// wire — the same entry [`write_transcript_entry`] is appending to
    /// `gatt.csv` in the same pass (`embarch-study-designer/design.md` §3
    /// decision 36). Boxed for the same reason `StepCompleted` is: a
    /// `MAX_PAYLOAD_LEN` payload would otherwise set this whole enum's size.
    GattTranscript { study_id: String, step_index: u32, entry: Box<GattTranscriptEntry> },
    /// The job's own `status`/`reason` changed — `"completed"` or `"failed"`.
    StatusChanged { study_id: String, status: String, reason: Option<String> },
}

// ---- pure validation (no HTTP, no hardware — unit-testable directly) ------

/// design.md §5.1: both of a study's seals must match what their own halves
/// recompute to, and its declared taps must satisfy §4.8's own pre-flight
/// rules. Factored out from the handler so it's testable with no HTTP
/// plumbing — the same posture `embarch_topology::hardware`'s own
/// port-selection logic takes.
///
/// The per-`PostHocValidation` step-index/tap-name checks this used to run
/// are gone with post-hoc validation itself.
fn validate_study(study: &Study) -> Result<(), String> {
    // design.md §3 decision 39/§4.8's own pre-flight rules — id-is-index,
    // no blank/duplicate/reserved name, no step range that could never open.
    // Computed by the crate, not restated here, so Core holds no second copy
    // of the rules to drift from.
    validate_taps(&study.streams, study.steps.len() as u32).map_err(|e| e.to_string())?;

    // `embarch-study-designer/design.md` §3 decision 40: both requirements
    // are mandatory and `"any"` is an explicit legal value, so a *blank* one
    // is the nobody-thought-about-it case and is rejected here. An omitted
    // one never reaches this function at all — `Study.requires` has no serde
    // default, so it fails to deserialize.
    //
    // Comparing them against what is actually on the bench is a different
    // thing and is not done here (Milestone 7 Phase B's version gate).
    study.requires.validate().map_err(|e| e.to_string())?;

    let recomputed = steps_crc(&study.steps)
        .map_err(|_| "failed to recompute steps_crc (a step's encoding is unexpectedly large)".to_string())?;
    if recomputed != study.steps_crc {
        return Err(format!(
            "steps_crc mismatch: submitted study.steps_crc is {}, but recomputing over study.steps gives {recomputed} — \
             the submitted steps don't match their own checksum",
            study.steps_crc
        ));
    }

    // The sibling seal (`embarch-study-designer/design.md` §3 decision 39's
    // 2026-08-25 amendment), checked independently of `steps_crc` above so a
    // failure says *which* half is corrupt — the whole reason there are two
    // seals rather than one widened one.
    let recomputed = streams_crc(&study.streams).map_err(|_| {
        "failed to recompute streams_crc (a tap's encoding is unexpectedly large)".to_string()
    })?;
    if recomputed != study.streams_crc {
        return Err(format!(
            "streams_crc mismatch: submitted study.streams_crc is {}, but recomputing over study.streams gives {recomputed} — \
             the submitted taps don't match their own checksum",
            study.streams_crc
        ));
    }

    Ok(())
}

// ---- the version gate (design.md §3 decision 31) --------------------------

/// **What Core can verify, it verifies; what it cannot, it must not pretend
/// to** — `embarch-core/design.md` §3 decision 31, the Core half of
/// `embarch-study-designer/design.md` §3 decision 40.
///
/// `requires.dev_bench_version` is the half Core genuinely *checks*:
/// dev-bench self-reports its build over `HelloAck`, so this compares a
/// declared requirement against a string the bench actually said, and a
/// mismatch is a `409` naming both. The comparison rule itself
/// ([`requirement_satisfied`]) lives in `embarch-study-designer` so Core
/// holds no second copy of it.
///
/// **`send_study_start` is a parameter rather than a call**, so the
/// ordering this decision turns on — *no step ever runs* on a mismatch — is
/// a property a test can assert directly, rather than something a reader
/// has to trust from the shape of the surrounding handler. `StudyStart` is
/// the only message that can make dev-bench execute anything, so "the
/// closure was never called" and "no step ran" are the same statement.
///
/// `requires.firmware_version` is checkable **only when this run's caller
/// says it flashed the DUT** (`run.flashed_firmware_version`,
/// `embarch-api/design.md` §3 decision 40). There is no readback path from a
/// DUT — Core flashes through a debug probe and gets nothing back — so
/// absent that, the requirement is recorded as `Declared` and not compared
/// against anything. With it, the DUT half of the gate fires for the first
/// time: `embarch-api` is the only process that sequenced both the flash and
/// the submit, so it is the only one that can supply the string.
///
/// **An override is recorded, never silently honoured** (decision 31,
/// `embarch-study-designer/design.md` §3 decision 40). On success this
/// returns every requirement it waved through, which
/// [`provenance_for`] writes into the result — a run allowed past a
/// requirement must not be indistinguishable from one that satisfied it.
fn gate_then_start(
    requires: &Requirements,
    reported_dev_bench_version: &str,
    run: &StudyRunParams,
    send_study_start: impl FnOnce() -> Result<(), String>,
) -> Result<heapless::Vec<VersionOverride, MAX_VERSION_OVERRIDES>, (StatusCode, String)> {
    let mut overrides = heapless::Vec::new();

    let mut check = |subject: VersionSubject, required: &str, actual: &str| {
        if requirement_satisfied(required, actual) {
            return Ok(());
        }
        if !run.allow_version_mismatch() {
            return Err((
                StatusCode::CONFLICT,
                mismatch_message(subject, required, actual),
            ));
        }
        // The push cannot fail: `MAX_VERSION_OVERRIDES` is the arity of
        // `Requirements` itself, and each subject is checked at most once.
        let _ = overrides.push(VersionOverride {
            subject,
            required: clamp_version(required),
            actual: clamp_version(actual),
        });
        tracing::warn!(
            field = subject.field_name(),
            required,
            actual,
            "version requirement waved through by an explicit override; recording it in the result"
        );
        Ok(())
    };

    check(
        VersionSubject::DevBench,
        requires.dev_bench_version.as_str(),
        reported_dev_bench_version,
    )?;

    if let Some(flashed) = run.flashed_firmware_version.as_deref() {
        check(VersionSubject::Firmware, requires.firmware_version.as_str(), flashed)?;
    }

    send_study_start().map_err(|msg| (StatusCode::BAD_GATEWAY, msg))?;
    Ok(overrides)
}

/// The `409` body for a version mismatch, naming **both** strings and saying
/// no step ran — the same shape `doctor` check 13 fails in
/// (`embarch-study-designer/design.md` §3 decision 40).
fn mismatch_message(subject: VersionSubject, required: &str, actual: &str) -> String {
    let (what, remedy) = match subject {
        VersionSubject::DevBench => (
            "dev-bench",
            "Reflash the bench (`run_study --reflash dev-bench`)",
        ),
        VersionSubject::Firmware => (
            "DUT firmware",
            "Reflash the DUT from the revision the study wants",
        ),
    };
    format!(
        "{what} version mismatch: this study requires requires.{} = '{required}', but this run \
         has '{actual}' — no step was run. {remedy}, author the study against the build you \
         have (`any` if it genuinely doesn't matter), or re-submit with \
         `allow_version_mismatch=1`, which proceeds and records the override in the result.",
        subject.field_name()
    )
}

/// The two out-of-band run parameters `POST /study` accepts as query
/// parameters (design.md §3 decision 31's amendment,
/// `embarch-api/design.md` §3 decision 40).
///
/// **Query parameters rather than fields on the `Study` body**, because
/// `embarch-study-designer/design.md` §3 decision 40 settles that reflash is
/// "a run parameter, not a study field": a saved study that reflashed a
/// board every time you re-read its results is the thing that decision
/// exists to prevent. A parameter of the *request* is literally that. It
/// also leaves the `Study` body byte-identical, so `steps_crc`/`streams_crc`
/// and every `Study` fixture on disk are untouched — a wrapping request body
/// would have changed the shape every existing client posts.
///
/// Query rather than a header, for the same reason the override is recorded
/// rather than honoured silently: a query parameter shows up in Core's own
/// request log and in a `curl` an engineer types by hand. A header does not.
#[derive(Debug, Default, Clone, Deserialize)]
#[serde(default)]
pub struct StudyRunParams {
    /// Proceed past a version requirement this run does not satisfy, and
    /// record the override in `StudyResult.provenance.overrides`. Accepts
    /// `1`/`true`; anything else (including absent) is off.
    pub allow_version_mismatch: Option<String>,
    /// What the caller says it just flashed onto the DUT. Its presence is
    /// what makes `firmware_source: FlashedThisRun` honest — and what makes
    /// `requires.firmware_version` checkable at all. Absent (the normal
    /// case, and every case where nothing was flashed) the DUT's version
    /// stays `Declared`.
    ///
    /// One parameter carrying both facts, not a boolean plus a string: "I
    /// flashed something" without saying what is exactly the
    /// assertion-without-content this whole area exists to remove.
    pub flashed_firmware_version: Option<String>,
}

impl StudyRunParams {
    fn allow_version_mismatch(&self) -> bool {
        matches!(
            self.allow_version_mismatch.as_deref(),
            Some("1") | Some("true")
        )
    }
}

/// What this run actually executed against, and **how each version was
/// established** (design.md §3 decision 31, `embarch-study-designer/design.md`
/// §4.5).
///
/// dev-bench's is [`VersionSource::ReportedByDevBench`] — Core read it off
/// the live link. The DUT's is [`VersionSource::Declared`] unless the caller
/// supplied `flashed_firmware_version`, in which case it is
/// [`VersionSource::FlashedThisRun`] and the recorded string is what the
/// caller says it put there rather than what the study asked for.
///
/// **Core still never flashes as part of a study** — decision 31's
/// no-build-system boundary is unchanged. `FlashedThisRun` is reachable here
/// only because `embarch-api` sequences check → build → flash → `POST
/// /study` and tells Core so out of band; Core keeps no persisted "last
/// thing I flashed" record of its own, which is the staleness pattern
/// `embarch-topology/design.md` §3 decision 3 forbids and which decision
/// 30(c) already declined once. `ReportedByOutpost` still needs an outpost
/// header record no firmware emits yet.
///
/// **Rendering a `Declared` version identically to a verified one is the
/// exact mislabelling decision 31 was written to prevent**, so the source
/// field says which it is rather than the value being presented on its own —
/// and `overrides` says which requirements, if any, this run was allowed
/// past rather than satisfied.
fn provenance_for(
    study: &Study,
    reported_dev_bench_version: &str,
    run: &StudyRunParams,
    overrides: heapless::Vec<VersionOverride, MAX_VERSION_OVERRIDES>,
) -> Provenance {
    let (firmware_version, firmware_source) = match run.flashed_firmware_version.as_deref() {
        Some(flashed) => (clamp_version(flashed), VersionSource::FlashedThisRun),
        None => (study.requires.firmware_version.clone(), VersionSource::Declared),
    };
    Provenance {
        dev_bench_version: clamp_version(reported_dev_bench_version),
        firmware_version,
        dev_bench_source: VersionSource::ReportedByDevBench,
        firmware_source,
        overrides,
    }
}

fn clamp_version(v: &str) -> heapless::String<MAX_FIRMWARE_VERSION_LEN> {
    heapless::String::try_from(v).unwrap_or_else(|_| {
        tracing::warn!("dev-bench reported a firmware_version longer than {MAX_FIRMWARE_VERSION_LEN} bytes; recording it empty rather than truncated");
        heapless::String::new()
    })
}

fn generate_study_id() -> String {
    let mut bytes = [0u8; 16];
    rand::fill(&mut bytes);
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn current_utc_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn study_results_root() -> anyhow::Result<PathBuf> {
    Ok(token_store::local_data_dir()?.join("study_results"))
}

fn study_results_dir(study_id: &str) -> anyhow::Result<PathBuf> {
    Ok(study_results_root()?.join(study_id))
}

/// Applies `f` to the registry entry for `study_id`, if it's still there.
/// Missing is silently ignored rather than panicking — losing the race
/// against a hypothetical future cleanup pass shouldn't crash a background
/// study task.
fn update_job<F: FnOnce(&mut StudyJob)>(jobs: &JobRegistry, study_id: &str, f: F) {
    let mut guard = jobs.lock().unwrap();
    if let Some(job) = guard.get_mut(study_id) {
        f(job);
    }
}

fn fail_job(jobs: &JobRegistry, events_tx: &broadcast::Sender<StudyEvent>, study_id: &str, reason: String) {
    tracing::error!(study_id, %reason, "study failed");
    update_job(jobs, study_id, |job| {
        job.status = "failed".to_string();
        job.reason = Some(reason.clone());
    });
    // No subscribers is the common case (nobody's watching `/events` right
    // now) — `send` erroring just means that, not a real failure, so the
    // result is intentionally discarded, same posture `embarch-topology`'s
    // own live-push takes (design.md §3 decision 12).
    let _ = events_tx.send(StudyEvent::StatusChanged {
        study_id: study_id.to_string(),
        status: "failed".to_string(),
        reason: Some(reason),
    });
}

/// The deadline for the next `DevBenchMessage` to arrive, given the index of
/// the next `StepResult` still outstanding — `study.rs`'s host-side watchdog
/// (design.md §3 decision 16's amendment). `next_expected` in range uses that
/// step's own `delay_before_ms + timeout_ms`; once every step has reported in,
/// the last step's `timeout_ms` alone is reused as the wait for the terminal
/// `StudyDone`. Pure and `now`-parameterized so the deadline math is
/// unit-testable without a clock or a real study run.
///
/// **`delay_before_ms` is part of the window (design.md §3 decision 33).** This
/// function used to ignore it, which was a live defect rather than a rounding
/// error: dev-bench honours the field by `k_sleep`ing it *before* running the
/// step (`main.c`'s dispatch loop), so a study authoring
/// `delay_before_ms >= timeout_ms + WATCHDOG_GRACE_MS` failed against a bench
/// that was working perfectly. That is squarely the intended path —
/// `delay_before_ms` exists so a stimulus's timing is authorable
/// (`embarch-study-designer/design.md` §3 decision 42) and multi-second delays
/// are the point of it. `timeout_ms` means "how long this step may take", never
/// "how long until I hear back"; the delay is the other term.
///
/// The terminal-`StudyDone` branch deliberately does **not** add a delay: by
/// the time every `StepResult` has arrived, the last step's own delay has
/// already elapsed, so adding it again would widen that wait for nothing.
fn next_deadline(study: &Study, next_expected: usize, now: Instant) -> Instant {
    let (delay_ms, timeout_ms) = match study.steps.get(next_expected) {
        // A step still outstanding: dev-bench sleeps its delay, then runs it.
        Some(step) => (step.delay_before_ms as u64, step.timeout_ms as u64),
        // Every step reported in; only `StudyDone` is left. The last step's
        // delay is already spent, so only its timeout carries over.
        None => (0, study.steps.last().map(|s| s.timeout_ms as u64).unwrap_or(0)),
    };
    now + Duration::from_millis(delay_ms)
        + Duration::from_millis(timeout_ms)
        + Duration::from_millis(WATCHDOG_GRACE_MS)
}

// ---- dev-bench link: open + Hello/HelloAck handshake ----------------------

/// `HelloAck`'s fields, decoupled from the wire type's `heapless::String` so
/// callers outside this module (namely [`hello_handler`]) don't need to know
/// about `embarch_study_designer`'s capacity-bounded string type — this is
/// also what `GET /dev-bench/hello` serializes verbatim for
/// `embarch-umbrella`'s doctor check 13 (`embarch-dev-bench/design.md` §3
/// decision 25).
#[derive(Debug, Clone, Serialize)]
pub struct HelloAckInfo {
    pub schema_version: u32,
    pub compatible: bool,
    pub firmware_version: String,
    /// What the bench says its own chip ID is (`embarch-study-designer`
    /// schema v10, §3 decision 35) — empty when its build cannot answer.
    pub hardware_id: String,
    /// How that compares to the identity the JTAG side just verified, as a
    /// stable string (`match`/`mismatch`/`not-reported`/`undeclared`).
    /// Reported rather than only enforced, because `undeclared` is the
    /// current answer for every chip and a human needs to see both strings
    /// side by side to make it anything else — see
    /// `embarch_topology::hardware::compare_self_reported`.
    pub link_identity: String,
    /// The JTAG-read identity this was compared against, so one call to
    /// `GET /dev-bench/hello` shows both halves.
    pub probe_hardware_id: String,
}

/// How long Core keeps reading after `HelloAck` before deciding dev-bench has
/// nothing more to say right now ([`drain_post_ack_log_lines`]).
///
/// Larger than [`DevBenchLink`]'s own 200ms per-read timeout on purpose, so
/// the window is a real one rather than one read that happens to block for its
/// whole duration.
const POST_ACK_DRAIN_MS: u64 = 250;

/// Reads whatever `LogLine`s dev-bench sends immediately after `HelloAck`, into
/// the debug file, before this function's caller does anything else with the
/// link.
///
/// **Found live, and this is the point of the whole feature.** dev-bench
/// installs its log sink right after putting `HelloAck` on the wire and
/// immediately flushes the boot records it held until then
/// (`embarch-dev-bench/app/src/dev_bench_log.c`) — that flush *is* how a
/// reboot becomes visible from Core's side. But `GET /dev-bench/hello` (the
/// doctor's check 13) returns the moment it has the ack and drops the link, so
/// on a freshly reset bench the first handshake wrote the boot record onto a
/// wire nobody was reading, and every later handshake had nothing left to
/// flush. The boot record was reliably produced and reliably lost, which is
/// worse than not having it: the file looked like the bench had never said
/// anything about coming up.
///
/// Safe to consume here because nothing else can legitimately be in flight:
/// Core has not sent `StudyStart` yet, so a `LogLine` is the only message
/// dev-bench has any reason to send at this point. Anything else is reported
/// rather than silently eaten, since consuming it is unavoidable once `recv`
/// has decoded it.
fn drain_post_ack_log_lines(link: &mut DevBenchLink) {
    let deadline = Instant::now() + Duration::from_millis(POST_ACK_DRAIN_MS);
    loop {
        match link.recv(deadline) {
            Ok(Some(DevBenchMessage::LogLine { text })) => {
                crate::dev_bench_log::append(None, text.as_str());
                tracing::info!("dev-bench (at handshake): {text}");
            }
            // The ordinary exit: the window closed with nothing more waiting.
            Ok(None) => break,
            Ok(Some(other)) => {
                tracing::warn!(
                    "dev-bench sent {other:?} between HelloAck and StudyStart, which nothing                      in the protocol calls for; it has been consumed"
                );
                break;
            }
            Err(e) => {
                tracing::warn!("error draining dev-bench's post-handshake log lines: {e:?}");
                break;
            }
        }
    }
}

/// Opens `port_name`, sends `Hello`, and waits for `HelloAck`. Runs entirely
/// inside `spawn_blocking` (all serial I/O is blocking) — `Err` carries a
/// human-readable message describing what failed, not a status code, since
/// this is called before we know whether that maps to `502`, `504`, etc.
async fn open_and_handshake(
    port_name: String,
    enrolled: &embarch_topology::hardware::EnrolledBoard,
) -> Result<(DevBenchLink, HelloAckInfo), String> {
    let probe_hardware_id = enrolled.hardware_id.clone();
    let chip = enrolled.chip.clone();
    tokio::task::spawn_blocking(move || {
        let port_for_note = port_name.clone();
        let mut link = DevBenchLink::open(&port_name).map_err(|e| format!("{e:?}"))?;

        link.send(&DevBenchMessage::Hello {
            schema_version: DEV_BENCH_WIRE_SCHEMA_VERSION,
            host_utc_ms: current_utc_ms(),
        })
        .map_err(|e| format!("failed to send Hello to dev-bench: {e:?}"))?;

        let deadline = Instant::now() + Duration::from_millis(HANDSHAKE_TIMEOUT_MS);
        // A loop, not one `recv`, because of §3 decision 37: dev-bench's log
        // backend now runs from boot and writes on its own schedule, so a
        // `LogLine` can legitimately arrive ahead of the ack on a bench that
        // has already handshaked once and is logging live. Before this, any
        // such line failed the handshake with "expected HelloAck, got
        // LogLine" — i.e. turning the firmware's logging on would have broken
        // every study, and it would have looked like a protocol bug.
        //
        // Every skipped line is still recorded (this is exactly the window
        // where a bench explains why it just rebooted), and the deadline is
        // the same one, so a bench that only ever logs still times out rather
        // than looping forever.
        let ack = loop {
            match link.recv(deadline) {
                Ok(Some(DevBenchMessage::LogLine { text })) => {
                    crate::dev_bench_log::append(None, text.as_str());
                    tracing::info!("dev-bench (pre-handshake): {text}");
                    continue;
                }
                other => break other,
            }
        };
        match ack {
            Ok(Some(DevBenchMessage::HelloAck {
                schema_version,
                compatible,
                firmware_version,
                hardware_id,
            })) => {
                let identity = embarch_topology::hardware::compare_self_reported(
                    &chip,
                    &probe_hardware_id,
                    &hardware_id,
                );
                tracing::info!(
                    dev_bench_schema_version = schema_version,
                    core_schema_version = DEV_BENCH_WIRE_SCHEMA_VERSION,
                    %firmware_version,
                    compatible,
                    bench_hardware_id = %hardware_id,
                    %probe_hardware_id,
                    link_identity = describe_identity(identity),
                    "dev-bench Hello/HelloAck handshake complete"
                );
                if !compatible {
                    return Err(format!(
                        "dev-bench firmware (schema version {schema_version}, firmware_version '{firmware_version}') \
                         is not compatible with Core's schema version {DEV_BENCH_WIRE_SCHEMA_VERSION}"
                    ));
                }
                // §3 decision 35's gate. Only a *declared* disagreement
                // refuses the link: `Undeclared`/`NotReported` mean the
                // question could not be asked, and refusing every healthy
                // bench because Core cannot yet relate two encodings would
                // be strictly worse than the gap this closes.
                if identity == embarch_topology::hardware::SelfReportedIdentity::Mismatch {
                    return Err(format!(
                        "dev-bench topology mismatch: the board on the serial link reports chip ID \
                         '{hardware_id}', but the enrolled probe just verified '{probe_hardware_id}' \
                         over JTAG — these are different boards"
                    ));
                }
                let info = HelloAckInfo {
                    schema_version,
                    compatible,
                    firmware_version: firmware_version.to_string(),
                    hardware_id: hardware_id.to_string(),
                    link_identity: describe_identity(identity).to_string(),
                    probe_hardware_id: probe_hardware_id.clone(),
                };
                // The debug file's own boundary marker (§3 decision 37).
                // Without it, a bench that reset between two studies produces
                // two runs of boot lines with nothing saying where one link
                // ended and the next began.
                crate::dev_bench_log::note(
                    None,
                    &format!(
                        "link opened on {port_for_note} (firmware {firmware_version}, wire schema v{schema_version})"
                    ),
                );
                drain_post_ack_log_lines(&mut link);
                Ok((link, info))
            }
            Ok(Some(other)) => Err(format!("expected HelloAck from dev-bench, got {other:?} instead")),
            Ok(None) => Err(format!(
                "timed out after {HANDSHAKE_TIMEOUT_MS}ms waiting for HelloAck from dev-bench"
            )),
            Err(e) => Err(format!("error waiting for HelloAck from dev-bench: {e:?}")),
        }
    })
    .await
    .map_err(|e| format!("dev-bench handshake task panicked: {e:?}"))?
}

/// The `design.md` §3 decision 22 board-identity gate, applied to
/// dev-bench's own connection — this path never calls `hardware::open_probe`
/// at all (it's a plain serial port, not a probe-rs debug session), so it
/// can't reuse `hardware.rs`'s own probe-selection logic the way
/// `hardware::flash`/`reset` do.
///
/// **Keyed by role, not by the link's own USB serial number, since
/// 2026-08-21** (`embarch-dev-bench/design.md` decision 26's update):
/// originally keyed on the USB serial the link port itself reported, back
/// when that port was the ESP32-C5's native USB-Serial/JTAG peripheral —
/// which enumerates as both a serial port *and* a probe-rs debug probe over
/// the same physical connection, so its own serial was a valid enrollment
/// key. That stopped holding once dev-bench's runtime link moved to a
/// second, dedicated USB-UART bridge chip: a plain UART bridge has no
/// JTAG/SWD capability, so it can never be a `POST /probes/enroll` candidate
/// and its serial can never be enrolled.
/// `embarch_topology::hardware::validate_role` (formerly this crate's own
/// `board_gate::enforce_for_role`) looks up dev-bench's already-enrolled
/// entry by role instead and re-verifies identity over its still-attached
/// native USB/JTAG connection — the wire carrying `DevBenchMessage` traffic
/// and the wire identity gets confirmed over no longer need to be the same
/// one, as long as both reach the same physical chip (see that function's
/// own doc comment for what this still doesn't close). Runs inside
/// `spawn_blocking`: the gate attaches to real hardware and reads memory,
/// exactly the blocking probe-rs calls every other hardware-touching
/// handler in this crate already wraps.
async fn enforce_dev_bench_gate() -> Result<embarch_topology::hardware::EnrolledBoard, String> {
    // Returns the enrolled board rather than just `Ok(())` since §3 decision
    // 35: the JTAG-verified identity it confirms is exactly what the
    // `HelloAck` comparison needs, and re-reading it would be a second
    // answer to a question already answered.
    tokio::task::spawn_blocking(|| {
        embarch_topology::hardware::validate_role(embarch_topology::hardware::DEV_BENCH_ROLE)
    })
    .await
    .map_err(|e| format!("board-identity gate task panicked: {e:?}"))?
    .map_err(|e| format!("{e:?}"))
}

/// Stable strings for [`embarch_topology::hardware::SelfReportedIdentity`],
/// so a log line and `GET /dev-bench/hello`'s JSON say the same word.
fn describe_identity(identity: embarch_topology::hardware::SelfReportedIdentity) -> &'static str {
    use embarch_topology::hardware::SelfReportedIdentity as S;
    match identity {
        S::Match => "match",
        S::Mismatch => "mismatch",
        S::NotReported => "not-reported",
        S::Undeclared => "undeclared",
    }
}

// ---- GET /dev-bench/hello ---------------------------------------------------

/// Opens the dev-bench link just long enough to run the `Hello`/`HelloAck`
/// handshake and report `firmware_version`, then closes it — no `Study` is
/// sent. This is the "existing `GET /dev-bench/port`-adjacent connection"
/// `embarch-umbrella/design.md` §3 decision 19 names as check 13's data
/// source: `doctor` compares the `firmware_version` this returns against the
/// local `embarch-dev-bench` checkout's own `git describe`.
///
/// Rejects with `409` if a study is already in flight — the dev-bench link
/// is a single serial port, and this call would otherwise race
/// [`post_study_handler`]'s own open of it.
pub async fn hello_handler(
    State(state): State<AppState>,
) -> Result<Json<HelloAckInfo>, (StatusCode, String)> {
    {
        let guard = state.study_lock.lock().unwrap();
        if let Some(in_flight) = guard.as_ref() {
            return Err((
                StatusCode::CONFLICT,
                format!("a study is already in flight ({in_flight}) — dev-bench link is busy"),
            ));
        }
    }

    let port = match tokio::task::spawn_blocking(embarch_topology::hardware::resolve_dev_bench_port).await {
        Ok(Ok(port)) => port,
        Ok(Err(e)) if e.downcast_ref::<embarch_topology::hardware::DevBenchNotFound>().is_some() => {
            return Err((StatusCode::NOT_FOUND, format!("{e:?}")));
        }
        Ok(Err(e)) => return Err(internal_err(e)),
        Err(e) => return Err(internal_err(e)),
    };

    let enrolled = enforce_dev_bench_gate()
        .await
        .map_err(|msg| (StatusCode::BAD_GATEWAY, msg))?;

    let (_link, info) = open_and_handshake(port.port_name, &enrolled)
        .await
        .map_err(|msg| (StatusCode::BAD_GATEWAY, msg))?;
    // `_link` drops here, closing the serial port — this call never holds
    // the link open past the handshake.

    Ok(Json(info))
}

// ---- POST /study -----------------------------------------------------------

/// design.md §5.1: validate, take the study lock, open dev-bench and hand it
/// the study, then return immediately and let a background task own the rest
/// of the study's lifetime.
pub async fn post_study_handler(
    State(state): State<AppState>,
    Query(run): Query<StudyRunParams>,
    Json(study): Json<Study>,
) -> Result<(StatusCode, Json<StudyAcceptedResponse>), (StatusCode, String)> {
    validate_study(&study).map_err(|msg| (StatusCode::BAD_REQUEST, msg))?;

    // A `StreamSource::Signal` tap names a signal `embarch-topology` has to
    // have a declared route for before Core can open anything
    // (`embarch-topology/design.md` §3 decision 18) — a wire between two
    // headers is invisible to software and can only ever be stated. Checked
    // here rather than at the moment the tap opens, because the failure
    // otherwise lands mid-study as a silently-empty capture, which is the
    // exact failure mode `validate_taps`' own step-range rules exist to
    // prevent. Not part of `validate_study`: that function is pure, and this
    // reads enrollment state off disk.
    if let Err(msg) = check_signal_taps_are_declared(&study).await {
        return Err((StatusCode::BAD_REQUEST, msg));
    }

    let study_id = generate_study_id();

    {
        let mut guard = state.study_lock.lock().unwrap();
        if let Some(in_flight) = guard.as_ref() {
            return Err((
                StatusCode::CONFLICT,
                format!("a study is already in flight: {in_flight}"),
            ));
        }
        *guard = Some(study_id.clone());
    }

    // From here on, any early return must release the lock we just took.
    let release_lock = || {
        *state.study_lock.lock().unwrap() = None;
    };

    let port = match tokio::task::spawn_blocking(embarch_topology::hardware::resolve_dev_bench_port).await {
        Ok(Ok(port)) => port,
        Ok(Err(e)) if e.downcast_ref::<embarch_topology::hardware::DevBenchNotFound>().is_some() => {
            release_lock();
            return Err((StatusCode::NOT_FOUND, format!("{e:?}")));
        }
        Ok(Err(e)) => {
            release_lock();
            return Err(internal_err(e));
        }
        Err(e) => {
            release_lock();
            return Err(internal_err(e));
        }
    };

    let enrolled = match enforce_dev_bench_gate().await {
        Ok(board) => board,
        Err(msg) => {
            release_lock();
            return Err((StatusCode::BAD_GATEWAY, msg));
        }
    };

    let (link, hello) = match open_and_handshake(port.port_name, &enrolled).await {
        Ok(pair) => pair,
        Err(msg) => {
            release_lock();
            return Err((StatusCode::BAD_GATEWAY, msg));
        }
    };

    let steps = study.steps.clone();
    let steps_crc_value = study.steps_crc;
    let streams = study.streams.clone();
    let streams_crc_value = study.streams_crc;
    let requires = study.requires.clone();
    let reported = hello.firmware_version.clone();
    let run_for_gate = run.clone();

    // Decision 31's gate and the `StudyStart` send, in that order and in one
    // blocking hop: the gate runs *before* dev-bench is told to do anything,
    // so a version mismatch leaves a bench that never started a step.
    let started = tokio::task::spawn_blocking(move || {
        let mut link = link;
        let outcome = gate_then_start(&requires, &reported, &run_for_gate, || {
            // `streams` rides along; `validations` and `requires`
            // deliberately do not (`embarch-study-designer/design.md` §3
            // decisions 17, 39, 40) — dev-bench has to know which taps to
            // open and which `id` each answers to, and has nothing to do
            // with either of the other two.
            link.send(&DevBenchMessage::StudyStart {
                steps,
                steps_crc: steps_crc_value,
                streams,
                streams_crc: streams_crc_value,
            })
            .map_err(|e| format!("failed to send StudyStart to dev-bench: {e:?}"))
        });
        (outcome, link)
    })
    .await;

    let (link, overrides) = match started {
        Ok((Ok(overrides), link)) => (link, overrides),
        Ok((Err(rejection), _link)) => {
            release_lock();
            return Err(rejection);
        }
        Err(e) => {
            release_lock();
            return Err(internal_err(e));
        }
    };

    // What this run actually ran against — the bench's version captured off
    // the live link, the DUT's from whatever the caller says it flashed, and
    // whatever the gate above was told to wave through (decision 31).
    // Assembled after the gate rather than before it, because `overrides` is
    // the gate's own output and a provenance built ahead of it could only
    // ever have claimed the requirements were met.
    let provenance = provenance_for(&study, &hello.firmware_version, &run, overrides);

    let total_steps = study.steps.len() as u32;
    {
        let mut jobs = state.study_jobs.lock().unwrap();
        jobs.insert(
            study_id.clone(),
            StudyJob {
                status: "running".to_string(),
                current_step: None,
                total_steps: Some(total_steps),
                reason: None,
            },
        );
    }

    let jobs = state.study_jobs.clone();
    let study_lock = state.study_lock.clone();
    let events_tx = state.study_events.clone();
    let manifest_slot = state.outpost_manifest.clone();
    let study_id_for_task = study_id.clone();

    tokio::spawn(async move {
        let jobs_for_panic = jobs.clone();
        let events_tx_for_panic = events_tx.clone();
        let study_id_for_panic = study_id_for_task.clone();

        let outcome = tokio::task::spawn_blocking(move || {
            run_study_to_completion(
                link,
                study,
                study_id_for_task,
                jobs,
                events_tx,
                provenance,
                manifest_slot,
            )
        })
        .await;

        if let Err(join_err) = outcome {
            fail_job(
                &jobs_for_panic,
                &events_tx_for_panic,
                &study_id_for_panic,
                format!("background study task panicked: {join_err:?}"),
            );
        }

        *study_lock.lock().unwrap() = None;
    });

    Ok((StatusCode::ACCEPTED, Json(StudyAcceptedResponse { study_id, status: "accepted" })))
}

// ---- background task: run to completion, receiving results ---------------

/// Everything a capture needs, in one place that a signal-tap reader thread
/// can share with the main dev-bench loop.
///
/// The `Mutex` exists for exactly one reason: a `StreamSource::Signal` tap
/// with a `Route::Direct` route reads a **third physical serial connection**
/// on its own thread (design.md §3 decision 30(a)), so two producers can
/// reach the same [`StreamStore`]. It takes neither `hw_lock` nor
/// `study_lock` — it is a read-only listener on a wire, and blocking a
/// `/flash` on it would invent contention that does not exist.
struct Capture {
    study: Study,
    /// Every tap this study actually has a file for: the ones it declared,
    /// plus the synthesized reserved `dev-bench` log tap appended last
    /// (`embarch-study-designer`'s `dev_bench_log_tap`).
    ///
    /// **Indexed by `StreamTap.id`, and that stays true for the reserved one
    /// too** — its id is `study.streams.len()`, so appending it puts it at
    /// its own index. This is the list `tap_for` and `StreamStore` both key
    /// off; `study.streams` alone would have no entry for the log tap and
    /// every log line would route to an undeclared id.
    taps: Vec<StreamTap>,
    study_id: String,
    events_tx: broadcast::Sender<StudyEvent>,
    store: StdMutex<StreamStore>,
    /// Which step is open, for the `step_name` column every rendered row
    /// carries. Read by signal threads, written by the main loop.
    current_step: AtomicU32,
}

/// Owns the dev-bench link for the rest of the study's lifetime: receives
/// `DevBenchMessage`s until `StudyDone` arrives or the watchdog fires,
/// updating the job registry, `streams/` and `events.json` as it goes
/// (design.md §5.1 steps 8+, §5.2, and `embarch-core/design.md` §3 decisions
/// 30/31). Entirely blocking — called from `spawn_blocking` by
/// [`post_study_handler`].
fn run_study_to_completion(
    mut link: DevBenchLink,
    study: Study,
    study_id: String,
    jobs: JobRegistry,
    events_tx: broadcast::Sender<StudyEvent>,
    provenance: Provenance,
    manifest_slot: crate::outpost_manifest::ManifestSlot,
) {
    let results_dir = match study_results_dir(&study_id) {
        Ok(dir) => dir,
        Err(e) => {
            fail_job(&jobs, &events_tx, &study_id, format!("failed to resolve the study results directory: {e:?}"));
            return;
        }
    };
    if let Err(e) = std::fs::create_dir_all(&results_dir) {
        fail_job(
            &jobs,
            &events_tx,
            &study_id,
            format!("failed to create results directory {}: {e:?}", results_dir.display()),
        );
        return;
    }

    // Retention across runs (`embarch-core/design.md` §3 decision 30): run
    // once per submitted study, immediately after this study's own directory
    // exists, so `keep` is exact and counts the run in progress rather than
    // being one out. A sweep that fails is logged and ignored — losing disk
    // hygiene is not a reason to lose a capture.
    if let Ok(root) = study_results_root() {
        if let Err(e) = stream_store::sweep_study_results(&root, stream_store::study_results_keep()) {
            tracing::warn!("retention sweep of {} failed: {e:?}", root.display());
        }
    }

    // The study's own copy of whatever manifest the DUT's last flash bound,
    // taken **now** rather than at render time: the binding's lifetime is the
    // flash that created it, and a later flash during a long study would
    // otherwise swap the answer out from under a capture already in progress.
    // A study with no outpost tap takes no copy — there would be nothing to
    // decode against it.
    let study_manifest = if study
        .streams
        .iter()
        .any(|t| matches!(t.encoding, StreamEncoding::OutpostTrace))
    {
        let stored = manifest_slot.current();
        if let Some(stored) = stored.as_ref() {
            if let Err(e) = crate::outpost_manifest::write_study_copy(&results_dir, stored) {
                // The manifest not reaching disk costs the *names* in a trace
                // whose bytes are captured regardless, so it is a warning
                // rather than a failed study.
                tracing::warn!("failed to write this study's outpost manifest copy: {e:?}");
            }
        } else {
            tracing::info!(
                study_id,
                "this study declares an outpost tap and no manifest is bound; its trace will be \
                 decoded but not named"
            );
        }
        stored
    } else {
        None
    };

    // Declared taps plus the reserved `dev-bench` log tap, which no study
    // may declare (`validate_taps` rejects the name) and every study gets.
    // Built once, here, so the store's files and `tap_for`'s lookups can
    // never disagree about which ids exist.
    let mut taps: Vec<StreamTap> = study.streams.iter().cloned().collect();
    taps.push(dev_bench_log_tap(&study.streams));

    // `streams/` and its index, before a single byte has arrived (decision
    // 30(b)). One file per tap, named by the tap.
    let store = match StreamStore::create(&results_dir, &taps, stream_store::stream_max_bytes()) {
        Ok(store) => store,
        Err(e) => {
            fail_job(&jobs, &events_tx, &study_id, format!("failed to create the study's streams/ directory: {e:?}"));
            return;
        }
    };

    // Streams `events.json` to disk as each `StepResult` arrives, rather
    // than accumulating them and assembling a `StudyResult` only at the
    // end — see `EventsJsonWriter`'s own doc comment for why that
    // accumulate-then-build step is exactly what used to overflow the
    // stack (`embarch-study-designer/design.md` §7).
    let mut writer = match EventsJsonWriter::start(&results_dir, study.name.as_str(), &provenance) {
        Ok(w) => w,
        Err(e) => {
            fail_job(&jobs, &events_tx, &study_id, format!("failed to open events.json for writing: {e:?}"));
            return;
        }
    };

    let capture = Arc::new(Capture {
        study,
        taps,
        study_id: study_id.clone(),
        events_tx: events_tx.clone(),
        store: StdMutex::new(store),
        current_step: AtomicU32::new(0),
    });

    // Which taps dev-bench currently has open, by `StreamTap.id`. Purely
    // for reporting a chunk that arrives outside its own open window —
    // routing a chunk needs only the tap's declared encoding, which is in
    // the submitted `Study` and can't drift.
    let mut open_taps: std::collections::HashSet<u8> = std::collections::HashSet::new();

    // The reserved log tap's own handle. Derived the same way both ends
    // derive it — one past the last declared index — and read from
    // `capture.taps`' last entry rather than recomputed, so there is exactly
    // one place this number comes from.
    let reserved_log_tap_id = capture.taps[capture.taps.len() - 1].id;
    // Core's own taps: a `Route::Direct` signal on a serial port Core opens
    // itself, bypassing dev-bench entirely (decision 30(a)).
    let mut signal_taps: HashMap<u8, SignalTapReader> = HashMap::new();
    let mut next_expected: usize = 0;
    let mut deadline = next_deadline(&capture.study, next_expected, Instant::now());

    if let Err(msg) = sync_signal_taps(&capture, &mut signal_taps, 0) {
        stop_signal_taps(&mut signal_taps);
        finish_streams(&capture, &mut writer);
        render_outpost_traces(&results_dir, &capture.taps, study_manifest.as_ref());
        fail_job(&jobs, &events_tx, &study_id, msg);
        return;
    }

    let outcome = loop {
        match link.recv(deadline) {
            Ok(Some(DevBenchMessage::StepResult { step_index, result })) => {
                if let Err(e) = writer.write_step(&result) {
                    break Err(format!("failed to write step {step_index}'s result to events.json: {e:?}"));
                }
                let _ = events_tx.send(StudyEvent::StepCompleted {
                    study_id: study_id.clone(),
                    step_index,
                    result: Box::new(result),
                });
                next_expected = step_index as usize + 1;
                capture.current_step.store(next_expected as u32, Ordering::Relaxed);
                update_job(&jobs, &study_id, |job| job.current_step = Some(step_index));
                // A signal tap's `StreamScope` is a step range like any
                // other tap's, so Core opens and closes its port on the same
                // boundaries dev-bench uses for the taps it mediates.
                if let Err(msg) = sync_signal_taps(&capture, &mut signal_taps, next_expected as u32) {
                    break Err(msg);
                }
                deadline = next_deadline(&capture.study, next_expected, Instant::now());
            }
            Ok(Some(DevBenchMessage::StudyDone { completed })) => {
                tracing::info!(study_id, completed, "study run finished (StudyDone)");
                break Ok(());
            }
            Ok(Some(DevBenchMessage::StreamOpen { id })) => match tap_for(&capture.taps, id) {
                Some(tap) => {
                    tracing::info!(study_id, id, name = tap.name.as_str(), "stream tap opened");
                    open_taps.insert(id);
                }
                None => tracing::warn!(
                    study_id,
                    id,
                    "dev-bench opened a stream id this study never declared; ignoring it"
                ),
            },
            Ok(Some(DevBenchMessage::StreamClose { id, dropped })) => {
                open_taps.remove(&id);
                let name = tap_for(&capture.taps, id).map(|t| t.name.as_str()).unwrap_or("<undeclared>");
                if dropped > 0 {
                    // A capture that lost data must say so rather than be
                    // read as complete — `embarch-study-designer/design.md`
                    // §4.8's whole reason for carrying `dropped` on close.
                    // Recorded on the `StreamRef`, not just logged: a log
                    // line is not something a result carries with it.
                    capture.store.lock().unwrap().mark_lost_at_source(id);
                    tracing::warn!(
                        study_id,
                        id,
                        name,
                        dropped,
                        "stream tap closed having DROPPED records; this capture is not complete"
                    );
                } else {
                    tracing::info!(study_id, id, name, "stream tap closed");
                }
            }
            Ok(Some(DevBenchMessage::StreamChunkBatch { id, records })) => {
                let Some(tap) = tap_for(&capture.taps, id).cloned() else {
                    tracing::warn!(
                        study_id,
                        id,
                        "received stream bytes for an id this study never declared; dropping them"
                    );
                    continue;
                };
                if !open_taps.contains(&id) {
                    tracing::warn!(
                        study_id,
                        id,
                        name = tap.name.as_str(),
                        "received stream bytes outside this tap's own open window; keeping them"
                    );
                }
                for record in records.iter() {
                    write_stream_record(&capture, &tap, record);
                }
            }
            Ok(Some(DevBenchMessage::LogLine { text })) => {
                // `info!`, not `debug!`, for the ordinary case. dev-bench
                // never chatters on this channel -- every LogLine is the
                // firmware deliberately choosing to tell the host something
                // it can't express as a step `Outcome` (a truncated
                // transcript, a link RX overrun, the per-advertiser detail
                // behind a failed BLE name match). At `debug` all of that
                // was invisible against a service running at the default
                // level, which is how a scan diagnostic that was being sent
                // correctly looked like it wasn't being sent at all.
                //
                // Still `warn!` for a truncated transcript specifically: a
                // capture silently claiming to be exhaustive when it isn't
                // is exactly what decision 36 exists to prevent.
                //
                // **Amended by §3 decision 37.** The reasoning above was
                // written when every `LogLine` was a deliberate diagnostic,
                // which is what made `info` the right level for all of them.
                // Now the same channel also carries dev-bench's whole
                // `CONFIG_LOG` output, and mirroring a firmware `<dbg>` line
                // into `core.log` at `info` would drown Core's own account of
                // the run in the bench's. So a line that carries its own
                // level marker is mirrored at *that* level, and an unmarked
                // line — every one this comment was originally about — keeps
                // `info` exactly as before. Nothing is lost either way: the
                // debug file below takes every line at full detail
                // regardless of level.
                crate::dev_bench_log::append(Some(&study_id), text.as_str());
                match crate::dev_bench_log::classify(text.as_str()) {
                    _ if text.contains("NOT exhaustive") => {
                        tracing::warn!(study_id, "dev-bench: {text}")
                    }
                    crate::dev_bench_log::LineLevel::Error => {
                        tracing::error!(study_id, "dev-bench: {text}")
                    }
                    crate::dev_bench_log::LineLevel::Warn => {
                        tracing::warn!(study_id, "dev-bench: {text}")
                    }
                    crate::dev_bench_log::LineLevel::Debug => {
                        tracing::debug!(study_id, "dev-bench: {text}")
                    }
                    crate::dev_bench_log::LineLevel::Info
                    | crate::dev_bench_log::LineLevel::Unmarked => {
                        tracing::info!(study_id, "dev-bench: {text}")
                    }
                }

                // ...and into the study's own results, on the reserved
                // `dev-bench` tap (`embarch-study-designer/design.md` §4.8).
                // This is the asymmetry that tap exists to close: until now
                // a LogLine reached Core's rolling log and *nothing else*,
                // so the firmware's own account of a run was the one part of
                // it that didn't survive in the run's directory.
                //
                // Core stamps arrival here rather than dev-bench stamping
                // departure, which is the honest reading of `rx_utc_ms` for
                // a record dev-bench never framed as one — it is when this
                // line was received, and it is on the same clock every other
                // Core-mediated record uses.
                if let Some(tap) = tap_for(&capture.taps, reserved_log_tap_id).cloned() {
                    let mut line = text.as_str().to_string();
                    line.push('\n');
                    let bytes = match heapless::Vec::from_slice(line.as_bytes()) {
                        Ok(bytes) => bytes,
                        // Longer than MAX_STREAM_CHUNK_BYTES. A LogLine is
                        // capped well below that by MAX_LOG_LINE_LEN, so
                        // this is unreachable rather than merely unlikely —
                        // dropped rather than truncated, because a silently
                        // shortened log line is the sort of thing that gets
                        // read as the whole message.
                        Err(()) => {
                            tracing::warn!(
                                study_id,
                                "a dev-bench log line didn't fit a stream record; \
                                 it is in Core's log only"
                            );
                            continue;
                        }
                    };
                    write_stream_record(
                        &capture,
                        &tap,
                        &StreamRecord { rx_utc_ms: current_utc_ms(), bytes },
                    );
                }
            }
            Ok(Some(other)) => {
                // Hello/HelloAck/StudyStart are Core->dev-bench (or
                // handshake-only) messages; dev-bench shouldn't send them
                // back once a study is running. Not fatal on its own.
                tracing::warn!(study_id, "unexpected message from dev-bench mid-study: {other:?}");
            }
            Ok(None) => {
                break Err(format!(
                    "step timed out — no message received from dev-bench before the deadline \
                     (waiting on step index {next_expected})"
                ));
            }
            Err(e) => break Err(format!("dev-bench connection error: {e:?}")),
        }
    };

    // The debug file's closing boundary (§3 decision 37), written before the
    // outcome is even decided: what makes that file readable is knowing where
    // one link's lifetime ended, and that is true whether the study passed,
    // failed, or timed out. `outcome` is borrowed, not consumed — the match
    // below still owns it.
    let closing = match &outcome {
        Ok(_) => "study finished, link closing".to_string(),
        Err(msg) => format!("study FAILED ({msg}), link closing"),
    };
    crate::dev_bench_log::note(Some(&study_id), &closing);

    // Whatever happened, Core's own ports close and the capture's `streams`
    // are sealed into the writer before the job's status is decided.
    stop_signal_taps(&mut signal_taps);
    finish_streams(&capture, &mut writer);
    // On the failure path too: a study that stopped early still captured
    // whatever ran before it did, and a trace of the run that went wrong is
    // the one most worth reading.
    render_outpost_traces(&results_dir, &capture.taps, study_manifest.as_ref());

    match outcome {
        Ok(()) => finish_job(&jobs, &events_tx, &study_id, writer),
        Err(reason) => fail_job(&jobs, &events_tx, &study_id, reason),
    }
}

/// Hands the writer one [`StreamRef`] per declared tap. Called on every exit
/// path, including a failure: a failed study's `.partial` file is a
/// diagnostic artifact, and a diagnostic that omits what the capture
/// actually produced is worth less than one that includes it.
fn finish_streams(capture: &Capture, writer: &mut EventsJsonWriter) {
    writer.set_streams(capture.store.lock().unwrap().refs());
}

/// Decodes every `OutpostTrace` tap's captured bytes into a `*.trace.csv`,
/// once the capture is closed (`embarch-outpost/design.md` §3 decision 10 —
/// post-hoc, no live feed).
///
/// **A missing or mismatched manifest costs the names, never the capture.**
/// The rows are written either way: a timeline of numeric thread pointers and
/// vector numbers is a real answer, and `index.json`'s `note` says why it has
/// no names, so an unnamed trace is never mistaken for a named one. What is
/// refused is applying the *wrong* manifest, which would produce a trace that
/// is entirely readable and entirely wrong.
fn render_outpost_traces(
    results_dir: &FsPath,
    taps: &[StreamTap],
    stored: Option<&crate::outpost_manifest::StoredManifest>,
) {
    use crate::outpost_manifest;

    let streams_dir = results_dir.join(stream_store::STREAMS_DIR);
    let Ok(Some(index)) = stream_store::read_index(&streams_dir) else {
        return;
    };

    let mut results: HashMap<String, (String, String)> = HashMap::new();

    for tap in taps {
        if !matches!(tap.encoding, StreamEncoding::OutpostTrace) {
            continue;
        }
        let Some(entry) = index.find(tap.name.as_str()) else {
            continue;
        };
        let raw_path = streams_dir.join(&entry.raw_file);
        let rendered_name = format!(
            "{}.trace.csv",
            entry.raw_file.strip_suffix(".bin").unwrap_or(&entry.raw_file)
        );
        let out_path = streams_dir.join(&rendered_name);

        match outpost_manifest::render(&raw_path, &out_path, stored.map(|s| &s.manifest)) {
            Ok(outcome) => {
                let note = match &outcome.refusal {
                    Some(why) => format!(
                        "decoded but NOT named: {}. The raw capture is intact; applying a \
                         manifest that does not describe this firmware would relabel every \
                         marker and thread.",
                        why.describe()
                    ),
                    None => String::new(),
                };
                tracing::info!(
                    name = tap.name.as_str(),
                    frames = outcome.frames,
                    bad_frames = outcome.bad_frames,
                    lost_frames = outcome.lost_frames,
                    records = outcome.records,
                    dropped_at_source = outcome.dropped_at_source,
                    named = outcome.refusal.is_none(),
                    "rendered an outpost trace"
                );
                results.insert(tap.name.as_str().to_string(), (rendered_name, note));
            }
            Err(e) => {
                tracing::error!(
                    name = tap.name.as_str(),
                    "failed to render an outpost trace; its raw bytes are kept: {e:?}"
                );
                results.insert(
                    tap.name.as_str().to_string(),
                    (String::new(), format!("rendering failed: {e}")),
                );
            }
        }
    }

    if results.is_empty() {
        return;
    }
    if let Err(e) = stream_store::update_index(&streams_dir, |entry| {
        if let Some((rendered, note)) = results.get(&entry.name) {
            if !rendered.is_empty() {
                entry.rendered_file = Some(rendered.clone());
            }
            if !note.is_empty() {
                entry.note = Some(note.clone());
            }
        }
    }) {
        tracing::warn!("failed to record the outpost rendering in streams/index.json: {e:?}");
    }
}

// ---- Core's own taps: a Route::Direct signal on a third serial port -------

/// One `StreamSource::Signal` tap Core reads itself (design.md §3 decision
/// 30(a)): a USB-UART bridge with a DUT pin on it and nothing else — **a
/// port that belongs to a wire, not to a device.**
struct SignalTapReader {
    stop: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
    port_name: String,
}

impl SignalTapReader {
    fn stop(mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

/// `POST /study` pre-flight for every `StreamSource::Signal` tap: the signal
/// has to have a declared route before Core can open anything.
///
/// This is the **first caller `resolve_signal_port`/`find_signal` have ever
/// had** — `embarch-topology/design.md` §5 recorded that surface as unwired
/// infrastructure, and this is what wires it.
async fn check_signal_taps_are_declared(study: &Study) -> Result<(), String> {
    let names: Vec<String> = study
        .streams
        .iter()
        .filter_map(|tap| match &tap.source {
            StreamSource::Signal { name } => Some(name.as_str().to_string()),
            _ => None,
        })
        .collect();
    if names.is_empty() {
        return Ok(());
    }

    tokio::task::spawn_blocking(move || {
        for name in &names {
            match embarch_topology::hardware::find_signal(name) {
                Ok(Some(_)) => {}
                Ok(None) => {
                    return Err(format!(
                        "this study taps signal '{name}', which has no declared route — \
                         declare it first with POST /signals. A wire between two headers is \
                         invisible to software and can only ever be stated \
                         (embarch-topology/design.md §3 decision 18)."
                    ))
                }
                Err(e) => {
                    return Err(format!(
                        "couldn't read the declared route for signal '{name}': {e:?}"
                    ))
                }
            }
        }
        Ok(())
    })
    .await
    .map_err(|e| format!("signal-route pre-flight task panicked: {e:?}"))?
}

/// Opens and closes Core's own signal ports on the same `StreamScope`
/// boundaries dev-bench uses for the taps it mediates.
///
/// A `Route::ViaDevBench` signal deliberately gets **no port here**: its
/// bytes arrive relayed over dev-bench's existing link, on the ordinary
/// `StreamChunkBatch` path, and opening a second carrier for it would be
/// asserting a wire that does not exist.
fn sync_signal_taps(
    capture: &Arc<Capture>,
    open: &mut HashMap<u8, SignalTapReader>,
    step_index: u32,
) -> Result<(), String> {
    for tap in capture.study.streams.iter() {
        let StreamSource::Signal { name } = &tap.source else { continue };
        let wanted = signal_tap_is_wanted(tap, step_index);
        let is_open = open.contains_key(&tap.id);

        if wanted && !is_open {
            match start_signal_tap(capture, tap, name.as_str()) {
                Ok(Some(reader)) => {
                    tracing::info!(
                        study_id = capture.study_id,
                        id = tap.id,
                        name = tap.name.as_str(),
                        port = reader.port_name,
                        "opened a signal tap's own serial port (bypassing dev-bench)"
                    );
                    open.insert(tap.id, reader);
                }
                // A `ViaDevBench` route: nothing for Core to open, and its
                // bytes are already arriving over the dev-bench link.
                Ok(None) => {}
                Err(msg) => return Err(msg),
            }
        } else if !wanted && is_open {
            if let Some(reader) = open.remove(&tap.id) {
                tracing::info!(
                    study_id = capture.study_id,
                    id = tap.id,
                    name = tap.name.as_str(),
                    "closed a signal tap's own serial port (its scope ended)"
                );
                reader.stop();
            }
        }
    }
    Ok(())
}

/// Whether Core should have this signal tap's port open while `step_index`
/// runs — a tap's declared [`StreamScope`], nothing else. Split out from
/// [`sync_signal_taps`] purely so the open/close boundary is testable
/// without a serial port: getting it wrong produces a capture that is
/// silently short at one end, which is the failure this whole decision keeps
/// insisting has to be loud.
fn signal_tap_is_wanted(tap: &StreamTap, step_index: u32) -> bool {
    tap.scope.covers(step_index)
}

fn stop_signal_taps(open: &mut HashMap<u8, SignalTapReader>) {
    for (_, reader) in open.drain() {
        reader.stop();
    }
}

/// Resolves a signal to its live carrier and starts reading it.
///
/// **A tap that cannot be opened fails the study**, rather than running on
/// to produce a result whose declared capture is silently empty. Loud beats
/// plausible — the same posture decision 30(c) takes toward a trace it
/// cannot decode.
fn start_signal_tap(
    capture: &Arc<Capture>,
    tap: &StreamTap,
    signal_name: &str,
) -> Result<Option<SignalTapReader>, String> {
    let link = match embarch_topology::hardware::find_signal(signal_name) {
        Ok(Some(link)) => link,
        Ok(None) => {
            return Err(format!(
                "signal '{signal_name}' has no declared route (declare it with POST /signals)"
            ))
        }
        Err(e) => return Err(format!("couldn't read the declared route for '{signal_name}': {e:?}")),
    };

    if matches!(link.route, embarch_topology::hardware::Route::ViaDevBench { .. }) {
        tracing::info!(
            study_id = capture.study_id,
            name = tap.name.as_str(),
            "signal '{signal_name}' is routed via dev-bench; its bytes arrive relayed over the \
             existing link, so Core opens no port of its own"
        );
        return Ok(None);
    }

    let port = embarch_topology::hardware::resolve_signal_port(signal_name)
        .map_err(|e| format!("couldn't resolve a carrier for signal '{signal_name}': {e:?}"))?;

    let baud = stream_store::signal_baud();
    let serial = serialport::new(&port.port_name, baud)
        .timeout(Duration::from_millis(200))
        .open()
        .map_err(|e| {
            format!("failed to open signal '{signal_name}' on {} at {baud} baud: {e:?}", port.port_name)
        })?;

    let stop = Arc::new(AtomicBool::new(false));
    let handle = {
        let capture = Arc::clone(capture);
        let tap = tap.clone();
        let stop = Arc::clone(&stop);
        std::thread::Builder::new()
            .name(format!("signal-tap-{}", tap.id))
            .spawn(move || read_signal_tap(capture, tap, serial, stop))
            .map_err(|e| format!("failed to start a reader thread for signal '{signal_name}': {e:?}"))?
    };

    Ok(Some(SignalTapReader { stop, handle: Some(handle), port_name: port.port_name }))
}

/// Reads a signal port until the tap's scope ends or the study does, and
/// writes what arrives **verbatim** (decision 30(a)).
///
/// Bytes are chunked into `StreamRecord`s of at most
/// `limits::MAX_STREAM_CHUNK_BYTES`, arrival-stamped by Core, and pushed
/// through the identical [`write_stream_record`] path a dev-bench-mediated
/// tap's records take — so a `Signal` tap and a relayed one render the same
/// way, which is what lets the same saved study survive the bench being
/// rewired.
fn read_signal_tap(
    capture: Arc<Capture>,
    tap: StreamTap,
    mut port: Box<dyn serialport::SerialPort>,
    stop: Arc<AtomicBool>,
) {
    use std::io::Read as _;
    const CHUNK: usize = embarch_study_designer::limits::MAX_STREAM_CHUNK_BYTES;

    let mut buf = [0u8; CHUNK];
    while !stop.load(Ordering::Relaxed) {
        match port.read(&mut buf) {
            Ok(0) => {}
            Ok(n) => {
                let record = StreamRecord {
                    // Core is the node that received these bytes, so Core
                    // stamps them (`embarch-study-designer/design.md` §4.8).
                    rx_utc_ms: current_utc_ms(),
                    bytes: heapless::Vec::from_slice(&buf[..n]).unwrap_or_default(),
                };
                write_stream_record(&capture, &tap, &record);
            }
            // A bounded per-read timeout with nothing on the wire is the
            // normal state of a debug UART, not an error.
            Err(e) if e.kind() == std::io::ErrorKind::TimedOut => {}
            Err(e) => {
                tracing::warn!(
                    study_id = capture.study_id,
                    name = tap.name.as_str(),
                    "signal tap read failed, stopping this tap: {e:?}"
                );
                return;
            }
        }
    }
}

/// The declared tap `id` refers to, or `None` if the study never declared it.
/// `id` is the tap's own index in `Study.streams`
/// (`embarch-study-designer/design.md` §4.8), enforced by that crate's
/// `validate_taps` at submission, so this is a bounds-checked index rather
/// than a search.
fn tap_for(taps: &[StreamTap], id: u8) -> Option<&StreamTap> {
    taps.get(usize::from(id)).filter(|tap| tap.id == id)
}

/// Writes one arrival-stamped record to its tap's files under `streams/`.
///
/// **The raw bytes go down first, always, before any decode is attempted**
/// (`embarch-core/design.md` §3 decision 30(b)). A decode that fails then
/// costs a rendering, not a capture — the run is recoverable, which is the
/// whole difference between a bad afternoon and a lost one.
///
/// What a payload *means* comes only from the tap's declared
/// [`StreamEncoding`] (`embarch-study-designer/design.md` §3 decision 39,
/// §4.8) — never from the bytes. There is no sniff and no fallback here:
/// `Raw` renders nothing because nobody declared anything to render it as.
fn write_stream_record(capture: &Capture, tap: &StreamTap, record: &StreamRecord) {
    // Raw first. Unconditionally, for every encoding.
    capture.store.lock().unwrap().write_raw(tap.id, &record.bytes);

    let current_step = capture.current_step.load(Ordering::Relaxed);

    match tap.encoding {
        StreamEncoding::Samples { layout, unit, channel_id } => {
            let sample_hz = match tap.source {
                StreamSource::PowerFrontEnd { sample_hz } => Some(sample_hz),
                _ => None,
            };
            let samples: Vec<Sample> =
                samples_in(record, layout, unit, channel_id, sample_hz).collect();
            for sample in &samples {
                write_sample(capture, tap.id, current_step, *sample);
            }
            let _ = capture.events_tx.send(StudyEvent::SampleBatch {
                study_id: capture.study_id.clone(),
                stream_id: tap.id,
                stream_name: tap.name.as_str().to_string(),
                samples,
            });
        }
        StreamEncoding::GattTranscript => {
            // The record's bytes are one postcard-encoded
            // `GattTranscriptEntry`. `step_index` is whichever step is open
            // when it arrives, which is what decision 36 defined that column
            // to mean; the generic record carries no step of its own.
            match postcard::from_bytes::<GattTranscriptEntry>(&record.bytes) {
                Ok(entry) => {
                    write_transcript_entry(capture, tap.id, current_step, &entry);
                    let _ = capture.events_tx.send(StudyEvent::GattTranscript {
                        study_id: capture.study_id.clone(),
                        step_index: current_step,
                        entry: Box::new(entry),
                    });
                }
                // The bytes are already on disk — this costs the row, not
                // the capture.
                Err(e) => tracing::warn!(
                    study_id = capture.study_id,
                    name = tap.name.as_str(),
                    "a record on a GattTranscript-encoded tap didn't decode as an entry; \
                     its raw bytes are kept: {e:?}"
                ),
            }
        }
        StreamEncoding::Text => {
            // The decode is the identity, so the raw file already *is* the
            // rendering — it is named `.txt` rather than `.bin` to say so.
            // Writing the same bytes twice would double the disk cost of the
            // one encoding whose render adds nothing.
        }
        StreamEncoding::OutpostTrace => {
            // Rendered **post-hoc, from the complete raw file**, not here.
            // `embarch-outpost/design.md` §3 decision 10 settled that a trace
            // is recorded for a study's duration and drawn afterwards, and
            // decoding at the end is also what lets a header frame that
            // arrived late name every record before it. See
            // `render_outpost_traces`, which runs once the capture is closed.
        }
        StreamEncoding::Raw => {
            // Nothing declared, so nothing rendered. `Raw` is the honest
            // default for a payload nobody gave a meaning.
        }
    }
}

/// Appends one decoded `Sample` to its tap's rendered CSV, labelled with
/// whichever step was open when its record arrived, plus `core_rx_utc_ms`
/// (Core's own receipt-time wall clock, decision 30) as an extra column
/// beyond what `Sample::to_csv_row` already renders. The row shape itself
/// lives entirely in `embarch-study-designer` and is unchanged by both the
/// tap reshape and the move to `streams/` — only the path changed.
fn write_sample(capture: &Capture, tap_id: u8, step_index: u32, sample: Sample) {
    // An out-of-range step index labels the row with an empty step name
    // rather than dropping a real sample — the same trade
    // `write_transcript_entry` already makes for a transcript entry.
    let step_name = capture
        .study
        .steps
        .get(step_index as usize)
        .map(|s| s.name.as_str())
        .unwrap_or("");
    let Some(row) = sample.to_csv_row(step_name) else {
        tracing::warn!(
            "step name '{step_name}' doesn't fit alongside the rest of a CSV row; dropping this \
             sample's row (its raw bytes are already on disk)"
        );
        return;
    };

    capture
        .store
        .lock()
        .unwrap()
        .write_rendered_row(tap_id, &format!("{row},{}", current_utc_ms()));
}

/// Appends one GATT transcript entry to its tap's rendered CSV
/// (`embarch-study-designer/design.md` §3 decision 36, §4.3b), the same way
/// [`write_sample`] appends samples: incrementally, as each entry arrives,
/// so a capture survives a Core crash that writes the study itself off as
/// `"failed"`.
///
/// `step_index` is whichever step was open when the record carrying this
/// entry arrived (`embarch-study-designer/design.md` §3 decision 36's own
/// definition of that column) — the generic stream record replacing the
/// retired `GattTranscriptRecord` carries no step of its own. An
/// out-of-range `step_index` still gets written, with an empty `step_name`,
/// rather than dropped: the entry is real GATT traffic that happened, and
/// losing it because Core couldn't label it would be the worse trade.
fn write_transcript_entry(capture: &Capture, tap_id: u8, step_index: u32, entry: &GattTranscriptEntry) {
    let step_name = capture
        .study
        .steps
        .get(step_index as usize)
        .map(|s| s.name.as_str())
        .unwrap_or("");

    let Some(row) = entry.to_csv_row(step_index, step_name) else {
        tracing::warn!(
            "a GATT transcript entry for step '{step_name}' doesn't fit in one CSV row; dropping \
             its row (its raw bytes are already on disk)"
        );
        return;
    };

    // `core_rx_utc_ms` appended by Core, not by the crate's own renderer —
    // Core's receipt time, not part of the wire type (decision 30), same
    // split `write_sample` uses. It is also the only wall-clock timestamp on
    // the row today: `rx_utc_ms` is dev-bench uptime until the clock-resync
    // gap (design.md §7) closes.
    capture
        .store
        .lock()
        .unwrap()
        .write_rendered_row(tap_id, &format!("{row},{}", current_utc_ms()));
}

/// Streams `events.json` to disk one `StepResult` at a time, as each
/// arrives — the file is never assembled from a fully-materialized
/// `StudyResult` held in memory. `embarch-study-designer/design.md` §7
/// measured that type at **~1.3 MB**, purely from its `no_std` worst-case-
/// capacity fields (`heapless::Vec<StepResult, 64>`, each `StepResult`
/// itself carrying up to 32 `GattActivityRecord`s at up to 512 bytes each) —
/// safe to serialize element-by-element (each `StepResult` alone is at most
/// tens of KB), genuinely unsafe to construct whole on a normal thread
/// stack, which is exactly what building one and handing it to
/// `serde_json::to_writer` used to do, and exactly what overflowed
/// `study::tests::finish_job_marks_completed_and_stores_the_result_even_when_stopped_early`
/// (and, in production, every `GET /study/{study_id}` call that cloned a
/// completed job out of the registry).
///
/// `provenance` and `streams` are the two `StudyResult` fields that landed
/// in the type at Milestone 7 Phase A and that **this file had never carried
/// at all** — `finish` wrote `steps` and a hardcoded `"validations":[]` and
/// stopped. Both are small and bounded (`Provenance` is four short strings;
/// `streams` is at most `MAX_STREAMS_PER_STUDY` entries), so both are
/// serialized directly, and neither breaks the streaming discipline the rest
/// of this type exists to keep.
struct EventsJsonWriter {
    file: std::io::BufWriter<std::fs::File>,
    partial_path: PathBuf,
    final_path: PathBuf,
    wrote_any_step: bool,
    provenance: Provenance,
    streams: Vec<StreamRef>,
}

impl EventsJsonWriter {
    fn start(results_dir: &FsPath, study_name: &str, provenance: &Provenance) -> anyhow::Result<Self> {
        use anyhow::Context;

        let partial_path = results_dir.join("events.json.partial");
        let final_path = results_dir.join("events.json");
        let mut file = std::io::BufWriter::new(
            std::fs::File::create(&partial_path)
                .with_context(|| format!("failed to create {}", partial_path.display()))?,
        );
        write!(file, "{{\"study_name\":{},\"steps\":[", serde_json::to_string(study_name)?)
            .with_context(|| format!("failed to write the opening of {}", partial_path.display()))?;
        Ok(Self {
            file,
            partial_path,
            final_path,
            wrote_any_step: false,
            provenance: provenance.clone(),
            streams: Vec::new(),
        })
    }

    fn write_step(&mut self, result: &StepResult) -> anyhow::Result<()> {
        if self.wrote_any_step {
            write!(self.file, ",")?;
        }
        serde_json::to_writer(&mut self.file, result)?;
        self.wrote_any_step = true;
        Ok(())
    }

    /// One [`StreamRef`] per declared tap, handed over once the capture is
    /// finished. Set rather than appended because a tap's `bytes_written`
    /// and `truncated` are only final once its files are.
    fn set_streams(&mut self, streams: Vec<StreamRef>) {
        self.streams = streams;
    }

    /// Closes the `steps` array, then writes `provenance` and `streams`, and
    /// atomically renames `.partial` to the real `events.json`.
    ///
    /// No longer writes a `"validations":[]` key. It was always literally
    /// that — hardcoded empty, because Core never evaluated a single
    /// validation in its life — which is a good part of why the whole notion
    /// was removed rather than finished.
    fn finish(mut self) -> anyhow::Result<()> {
        use anyhow::Context;

        write!(self.file, "],\"provenance\":")?;
        serde_json::to_writer(&mut self.file, &self.provenance)?;
        write!(self.file, ",\"streams\":[")?;
        for (i, stream_ref) in self.streams.iter().enumerate() {
            if i > 0 {
                write!(self.file, ",")?;
            }
            serde_json::to_writer(&mut self.file, stream_ref)?;
        }
        write!(self.file, "]}}")?;
        self.file.flush().with_context(|| format!("failed to flush {}", self.partial_path.display()))?;
        drop(self.file);
        std::fs::rename(&self.partial_path, &self.final_path).with_context(|| {
            format!("failed to finalize {} as {}", self.partial_path.display(), self.final_path.display())
        })?;
        Ok(())
    }
}

/// `StudyDone`'s happy path (design.md §5.1 step 8): finalize `events.json`
/// (every step in it was already streamed to disk as it arrived — this only
/// closes the file out) and mark the job `"completed"`. `completed: false`
/// (a study that stopped early on a failing step with `continue_on_fail:
/// false`) is still a normal protocol completion, not a watchdog/connection
/// failure — `"failed"` status is reserved for those.
fn finish_job(jobs: &JobRegistry, events_tx: &broadcast::Sender<StudyEvent>, study_id: &str, writer: EventsJsonWriter) {
    if let Err(e) = writer.finish() {
        // Every step already made it to the `.partial` file — only the
        // finalize-and-rename step failed, so this is reported as a failure
        // rather than silently claiming "completed" over a result that
        // never got its final name.
        fail_job(jobs, events_tx, study_id, format!("failed to finalize events.json: {e:?}"));
        return;
    }

    update_job(jobs, study_id, |job| job.status = "completed".to_string());
    let _ = events_tx.send(StudyEvent::StatusChanged {
        study_id: study_id.to_string(),
        status: "completed".to_string(),
        reason: None,
    });
}

// ---- GET /study/{study_id} --------------------------------------------------

pub async fn get_study_handler(
    State(state): State<AppState>,
    Path(study_id): Path<String>,
) -> Result<Json<StudyJobResponse>, (StatusCode, String)> {
    let job = {
        let jobs = state.study_jobs.lock().unwrap();
        match jobs.get(&study_id) {
            Some(job) => job.clone(),
            None => {
                return Err((
                    StatusCode::NOT_FOUND,
                    format!("no study job found for study_id '{study_id}' (never existed, or Core has restarted since)"),
                ))
            }
        }
    };

    // Only a `"completed"` study has a finished `events.json` to read back
    // — `result` reads it from disk rather than from a resident copy (see
    // `StudyJob`'s own doc comment for why one is never kept in memory). A
    // read failure here degrades to `result: None` rather than failing the
    // whole request — the job's own status/reason are still real and worth
    // returning even if the file is somehow missing or unreadable.
    let result = if job.status == "completed" {
        match read_events_json(&study_id).await {
            Ok(value) => Some(value),
            Err(e) => {
                tracing::error!("study {study_id} is completed but events.json couldn't be read back: {e:?}");
                None
            }
        }
    } else {
        None
    };

    Ok(Json(StudyJobResponse {
        status: job.status,
        current_step: job.current_step,
        total_steps: job.total_steps,
        result,
        reason: job.reason,
    }))
}

async fn read_events_json(study_id: &str) -> anyhow::Result<serde_json::Value> {
    let dir = study_results_dir(study_id)?;
    let bytes = tokio::fs::read(dir.join("events.json")).await?;
    Ok(serde_json::from_slice(&bytes)?)
}

// ---- GET /study/{study_id}/events (SSE, live push) --------------------------

/// Live-push companion to polling `GET /study/{study_id}`: every
/// [`StudyEvent`] is forwarded to a connected client the instant Core
/// broadcasts it — a step completing, a sample batch arriving, or the
/// study's own status changing — rather than requiring the client to poll
/// and hope it didn't miss something in between. Mirrors `embarch-
/// topology`'s own live-push shape (design.md §3 decision 12) applied here
/// to study progress. Only one study is ever in flight at a time
/// (`StudyLock`), so a single process-wide broadcast channel (`AppState::
/// study_events`) is enough — no per-study subscription bookkeeping.
///
/// A slow subscriber that falls behind the channel's buffer gets an
/// `event: lagged` frame naming how many messages it missed, rather than
/// silently resuming as if nothing was lost.
///
/// The channel itself is process-wide (`AppState::study_events`), but this
/// handler filters to events matching the `study_id` in the URL — so
/// watching one study's `/events` never surfaces a *different* study's
/// traffic even though, today, `StudyLock` never actually lets two run
/// concurrently to make that observable.
pub async fn study_events_handler(
    State(state): State<AppState>,
    Path(study_id): Path<String>,
) -> Sse<impl tokio_stream::Stream<Item = Result<Event, Infallible>>> {
    let rx = state.study_events.subscribe();
    let stream = BroadcastStream::new(rx).filter_map(move |msg| {
        let study_id = study_id.clone();
        async move {
            match msg {
                Ok(event) if event_study_id(&event) == study_id => Some(Ok::<_, Infallible>(
                    Event::default()
                        .json_data(&event)
                        .unwrap_or_else(|e| Event::default().event("encode-error").data(format!("{e:?}"))),
                )),
                // A different study's event, or this subscriber lagged
                // behind the channel's buffer — the latter is reported
                // rather than silently skipped.
                Ok(_) => None,
                Err(BroadcastStreamRecvError::Lagged(n)) => {
                    Some(Ok(Event::default().event("lagged").data(n.to_string())))
                }
            }
        }
    });
    Sse::new(stream).keep_alive(KeepAlive::default())
}

fn event_study_id(event: &StudyEvent) -> &str {
    match event {
        StudyEvent::StepCompleted { study_id, .. }
        | StudyEvent::SampleBatch { study_id, .. }
        | StudyEvent::GattTranscript { study_id, .. }
        | StudyEvent::StatusChanged { study_id, .. } => study_id,
    }
}

// ---- GET /study/{study_id}/streams -----------------------------------------

/// One declared tap, as `GET /study/{study_id}/streams` reports it.
///
/// A dedicated response type rather than serializing [`stream_store::StreamIndex`]
/// itself — the same reasoning `EnrollProbeResponse` states against
/// serializing `EnrolledBoard`: the on-disk index is Core's own bookkeeping
/// (it carries file names inside a private results directory and a version
/// number nothing outside Core reads), and this endpoint's contract should
/// not shift the day that file gains a field.
///
/// `rendered` is a boolean rather than the file name for the same reason:
/// a caller never opens the file, it asks `GET /study/{id}/stream/{name}`,
/// and what it needs to know is whether that call will hand back a decoded
/// rendering or the raw bytes.
#[derive(Debug, Serialize)]
pub struct StreamIndexEntryResponse {
    pub id: u8,
    pub name: String,
    pub encoding: StreamEncoding,
    pub alias: Option<String>,
    /// Whether a decoded rendering exists, i.e. whether
    /// `GET /study/{id}/stream/{name}` (without `?raw=1`) serves something
    /// other than the raw bytes.
    pub rendered: bool,
    /// Why this tap's rendering is missing, incomplete, or **unnamed** — set
    /// only when there is something to say.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct StreamIndexResponse {
    pub streams: Vec<StreamIndexEntryResponse>,
}

/// What a study's taps captured, and — the reason this route exists — **why a
/// trace has no names when it has none**.
///
/// Added 2026-08-26 for `embarch-ui`'s Trace view (`embarch-ui/design.md` §3
/// decision 10). Until it existed, `streams/index.json`'s `note` had no HTTP
/// caller at all: `GET /study/{id}` returns `StreamRef { name, bytes_written,
/// truncated }`, which has no room for it, and
/// `GET /study/{id}/stream/{name}` serves the rendered CSV either way — so
/// over HTTP **a refused trace and a named one were indistinguishable**,
/// which is exactly the confusion decision 10's "an unnamed trace is never
/// mistaken for a named one" exists to prevent.
///
/// A new route rather than a field on `StreamRef`, deliberately: `StreamRef`
/// lives in `embarch-study-designer` and rides inside `StudyResult`, so
/// growing it is a host schema bump for a fact that is Core-side bookkeeping
/// and is not produced by, or meaningful to, dev-bench. The cheapest shape
/// that carries the answer is the one that does not move a shared type.
///
/// Reads purely off disk, like `stream_data_handler` — a study whose job
/// registry entry is gone (Core restarted) still answers, because the results
/// directory is the durable record.
pub async fn stream_index_handler(
    Path(study_id): Path<String>,
) -> Result<Json<StreamIndexResponse>, (StatusCode, String)> {
    let streams_dir = streams_dir_for(&study_id)?;
    let index = read_stream_index(&streams_dir)?.ok_or_else(|| {
        (
            StatusCode::NOT_FOUND,
            format!(
                "study '{study_id}' has no captured streams (it may predate streams/, or never \
                 have started)"
            ),
        )
    })?;

    Ok(Json(stream_index_response(index)))
}

/// [`stream_index_handler`]'s whole body, minus the disk read — split out so
/// the one thing worth pinning is testable without a writable results root:
/// that `note` survives into the response, and that `rendered` says what
/// `GET /study/{id}/stream/{name}` will actually serve.
fn stream_index_response(index: stream_store::StreamIndex) -> StreamIndexResponse {
    StreamIndexResponse {
        streams: index
            .streams
            .into_iter()
            .map(|e| StreamIndexEntryResponse {
                id: e.id,
                name: e.name,
                encoding: e.encoding,
                alias: e.alias,
                rendered: e.rendered_file.is_some(),
                note: e.note,
            })
            .collect(),
    }
}

// ---- GET /study/{study_id}/stream/{name}, and the three routes it replaces --

/// `?raw=1` serves the tap's byte-for-byte capture instead of its rendering.
///
/// A tap with no rendering (`Raw`, `Text`, `OutpostTrace`) serves its raw
/// file either way — the flag picks between two files when there are two,
/// and is not a request for a different *kind* of answer.
#[derive(Debug, Deserialize)]
pub struct StreamQuery {
    #[serde(default)]
    raw: Option<String>,
}

impl StreamQuery {
    fn wants_raw(&self) -> bool {
        matches!(self.raw.as_deref(), Some("1") | Some("true") | Some(""))
    }
}

/// One tap's capture, by the name the `Study` declared it under
/// (`embarch-core/design.md` §3 decision 30(b), §4).
///
/// Served as **bytes**, for the same reason the three fixed routes it
/// replaces were: Core and `embarch-api` are not guaranteed to share a
/// filesystem (§7's artifact-transfer gap), so handing back a path would
/// hand back something the caller cannot open.
///
/// A name that isn't in this study's `streams/index.json` is a `404`, which
/// is also what makes a tap name incapable of naming a file outside the
/// streams directory: only names the index already carries resolve at all.
pub async fn stream_data_handler(
    Path((study_id, name)): Path<(String, String)>,
    Query(query): Query<StreamQuery>,
) -> Result<Response, (StatusCode, String)> {
    let streams_dir = streams_dir_for(&study_id)?;
    let index = read_stream_index(&streams_dir)?.ok_or_else(|| {
        (
            StatusCode::NOT_FOUND,
            format!("study '{study_id}' has no captured streams (it may predate streams/, or never have started)"),
        )
    })?;

    let entry = index.find(&name).ok_or_else(|| {
        let declared: Vec<&str> = index.streams.iter().map(|e| e.name.as_str()).collect();
        (
            StatusCode::NOT_FOUND,
            format!(
                "study '{study_id}' declares no stream tap named '{name}' — it declared: {}",
                if declared.is_empty() { "(none)".to_string() } else { declared.join(", ") }
            ),
        )
    })?;

    serve_capture(&streams_dir, entry, query.wants_raw(), &format!("stream '{name}'")).await
}

// The three fixed routes `GET /study/{id}/stream/{name}` replaces, kept as
// aliases for one release rather than breaking `embarch-api`'s existing
// `study_power_data`/`study_waveform_data`/`study_gatt_data` tools mid-flight
// (`embarch-core/design.md` §3 decision 30). Each resolves through the
// study's own index to whichever tap answers that alias — which is the whole
// reason the index exists, since a handler reading results back off disk has
// no `Study` in hand to ask.

pub async fn power_data_handler(Path(study_id): Path<String>) -> Result<Response, (StatusCode, String)> {
    serve_alias(&study_id, "power", "data.csv", "power data").await
}

pub async fn waveform_data_handler(Path(study_id): Path<String>) -> Result<Response, (StatusCode, String)> {
    serve_alias(&study_id, "waveform", "waveform.csv", "waveform data").await
}

/// `embarch-study-designer/design.md` §3 decision 36: the study's whole GATT
/// transcript, every entry across every step, uncapped — as opposed to
/// `GET /study/{id}`'s per-step `gatt_activity`, which is a bounded inline
/// summary.
pub async fn gatt_data_handler(Path(study_id): Path<String>) -> Result<Response, (StatusCode, String)> {
    serve_alias(&study_id, "gatt", "gatt.csv", "GATT transcript").await
}

/// Resolves one of the three retired fixed paths.
///
/// `legacy_file` is the pre-`streams/` path the same data used to live at,
/// and is tried when a study has no index at all: results captured before
/// this release are still on disk, and an alias that 404'd on them would
/// break the very tools these aliases exist to keep working.
async fn serve_alias(
    study_id: &str,
    alias: &str,
    legacy_file: &str,
    kind: &str,
) -> Result<Response, (StatusCode, String)> {
    let streams_dir = streams_dir_for(study_id)?;

    if let Some(index) = read_stream_index(&streams_dir)? {
        return match index.find_alias(alias) {
            Some(entry) => serve_capture(&streams_dir, entry, false, kind).await,
            None => Err((StatusCode::NOT_FOUND, format!("no {kind} captured for this study"))),
        };
    }

    // Pre-`streams/` results.
    let legacy = study_results_dir(study_id).map_err(internal_err)?.join(legacy_file);
    match tokio::fs::read(&legacy).await {
        Ok(bytes) => Ok(([(CONTENT_TYPE, "text/csv")], bytes).into_response()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            Err((StatusCode::NOT_FOUND, format!("no {kind} captured for this study")))
        }
        Err(e) => Err(internal_err(e)),
    }
}

fn streams_dir_for(study_id: &str) -> Result<PathBuf, (StatusCode, String)> {
    Ok(study_results_dir(study_id).map_err(internal_err)?.join(stream_store::STREAMS_DIR))
}

fn read_stream_index(streams_dir: &FsPath) -> Result<Option<stream_store::StreamIndex>, (StatusCode, String)> {
    stream_store::read_index(streams_dir).map_err(internal_err)
}

async fn serve_capture(
    streams_dir: &FsPath,
    entry: &stream_store::StreamIndexEntry,
    raw: bool,
    kind: &str,
) -> Result<Response, (StatusCode, String)> {
    let file = match (raw, entry.rendered_file.as_deref()) {
        (false, Some(rendered)) => rendered,
        _ => entry.raw_file.as_str(),
    }
    .to_string();

    let dir = streams_dir.to_path_buf();
    let file_for_read = file.clone();
    let is_csv = file.ends_with(".csv");
    let read = tokio::task::spawn_blocking(move || {
        stream_store::read_capture(&dir, &file_for_read, is_csv)
    })
    .await
    .map_err(internal_err)?
    .map_err(internal_err)?;

    match read {
        Some(bytes) => {
            Ok(([(CONTENT_TYPE, stream_store::content_type_for(&file))], bytes).into_response())
        }
        None => Err((StatusCode::NOT_FOUND, format!("no {kind} captured for this study"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use embarch_study_designer::RESERVED_DEV_BENCH_STREAM_NAME;
    use embarch_study_designer::{
        Action, BleRole,
    };
    use heapless::Vec as HVec;

    fn step(name: &str, timeout_ms: u32) -> embarch_study_designer::Step {
        embarch_study_designer::Step {
            name: heapless::String::try_from(name).unwrap(),
            action: Action::BleConnect { role: BleRole::Central, target_address: None , target_name: None },
            timeout_ms,
            continue_on_fail: false,
            delay_before_ms: 0,
        }
    }

    /// A `Capture` over `dir`, so the writers can be exercised with no
    /// hardware, no HTTP, and no dev-bench link — the same posture the rest
    /// of this module's tests already take.
    fn test_capture(dir: &FsPath, study: Study) -> Capture {
        // Same construction the real path uses, reserved log tap included —
        // a fixture that skipped it would not exercise the id the log tap
        // actually routes on.
        let mut taps: Vec<StreamTap> = study.streams.iter().cloned().collect();
        taps.push(dev_bench_log_tap(&study.streams));
        let store = StreamStore::create(dir, &taps, 0).unwrap();
        let (events_tx, _rx) = broadcast::channel(64);
        Capture {
            study,
            taps,
            study_id: "test-study-id".to_string(),
            events_tx,
            store: StdMutex::new(store),
            current_step: AtomicU32::new(0),
        }
    }

    fn tap(id: u8, name: &str, source: StreamSource, encoding: StreamEncoding) -> StreamTap {
        StreamTap {
            id,
            name: heapless::String::try_from(name).unwrap(),
            source,
            encoding,
            scope: embarch_study_designer::StreamScope::WholeStudy,
        }
    }

    /// `study_with_steps`, plus declared taps (and the seal recomputed over
    /// them, so the study is still submittable).
    fn study_with_taps(timeouts: &[u32], taps: &[StreamTap]) -> Study {
        let mut study = study_with_steps(timeouts);
        let mut streams: HVec<StreamTap, { embarch_study_designer::limits::MAX_STREAMS_PER_STUDY }> =
            HVec::new();
        for tap in taps {
            streams.push(tap.clone()).unwrap();
        }
        study.streams_crc = streams_crc(&streams).unwrap();
        study.streams = streams;
        study
    }

    fn study_with_steps(timeouts: &[u32]) -> Study {
        let mut steps = embarch_study_designer::bounded::StepList::new();
        for (i, t) in timeouts.iter().enumerate() {
            steps.push(step(&format!("step-{i}"), *t)).unwrap();
        }
        let steps_crc_value = steps_crc(&steps).unwrap();
        let streams: HVec<StreamTap, { embarch_study_designer::limits::MAX_STREAMS_PER_STUDY }> =
            HVec::new();
        let streams_crc_value = streams_crc(&streams).unwrap();
        Study {
            name: heapless::String::try_from("test-study").unwrap(),
            requires: embarch_study_designer::Requirements::any(),
            steps,
            streams,
            steps_crc: steps_crc_value,
            streams_crc: streams_crc_value,
        }
    }

    // ---- write_transcript_entry (design.md §3 decision 36) ----

    fn transcript_entry(payload: &[u8]) -> GattTranscriptEntry {
        use embarch_study_designer::{GattDirection, GattEventKind, Uuid};
        GattTranscriptEntry {
            rx_utc_ms: 4_242,
            direction: GattDirection::In,
            kind: GattEventKind::Notification,
            service_uuid: Uuid::parse("6e400001-b5a3-f393-e0a9-e50e24dcca9e"),
            characteristic_uuid: Uuid::parse("6e400003-b5a3-f393-e0a9-e50e24dcca9e"),
            att_status: 0,
            payload: heapless::Vec::from_slice(payload).unwrap(),
        }
    }

    #[test]
    fn write_transcript_entry_appends_rows_under_one_header() {
        let dir = tempfile::tempdir().unwrap();
        let study = study_with_taps(
            &[1_000, 2_000],
            &[tap(0, "gatt", StreamSource::GattTranscript, StreamEncoding::GattTranscript)],
        );
        let capture = test_capture(dir.path(), study);

        write_transcript_entry(&capture, 0, 0, &transcript_entry(b"ok\r\n"));
        write_transcript_entry(&capture, 0, 1, &transcript_entry(b"hi"));

        // The row shape is unchanged by the move to `streams/` — only the
        // path is (`embarch-core/design.md` §3 decision 30(b)).
        let csv = std::fs::read_to_string(dir.path().join("streams").join("gatt.csv")).unwrap();
        let lines: Vec<&str> = csv.lines().collect();
        assert_eq!(lines.len(), 3, "expected a header plus two rows, got: {csv}");

        // Header is the crate's, plus the one column Core itself appends
        // (decision 30) — Core holds no other column knowledge.
        assert_eq!(
            lines[0],
            format!("{},core_rx_utc_ms", GattTranscriptEntry::csv_header())
        );
        // Every row must have exactly as many columns as the header, or
        // every consumer misreads everything past the mismatch.
        let cols = lines[0].split(',').count();
        for row in &lines[1..] {
            assert_eq!(row.split(',').count(), cols, "column count mismatch in: {row}");
        }

        // The step name is denormalized in from the Study, and the payload
        // is readable as text without decoding hex by hand — the whole
        // reason `payload_ascii` exists.
        assert!(lines[1].contains(",step-0,"), "{}", lines[1]);
        assert!(lines[1].contains("6f6b0d0a"), "{}", lines[1]);
        // `,ok..,` not `ends_with(",ok..")` — Core appends its own
        // core_rx_utc_ms column after the crate's last one.
        assert!(lines[1].contains(",ok..,"), "{}", lines[1]);
        assert!(lines[2].contains(",step-1,"), "{}", lines[2]);
    }

    #[test]
    fn write_transcript_entry_keeps_an_entry_whose_step_index_is_out_of_range() {
        // Real GATT traffic Core can't label is still real GATT traffic:
        // written with an empty step_name rather than dropped.
        let dir = tempfile::tempdir().unwrap();
        let study = study_with_taps(
            &[1_000],
            &[tap(0, "gatt", StreamSource::GattTranscript, StreamEncoding::GattTranscript)],
        );
        let capture = test_capture(dir.path(), study);

        write_transcript_entry(&capture, 0, 99, &transcript_entry(b"x"));

        let csv = std::fs::read_to_string(dir.path().join("streams").join("gatt.csv")).unwrap();
        let row = csv.lines().nth(1).expect("a row was written");
        assert!(row.starts_with("4242,99,,in,notification,"), "{row}");
        assert_eq!(
            row.split(',').count(),
            csv.lines().next().unwrap().split(',').count()
        );
    }

    // ---- validate_study ----

    #[test]
    fn validate_study_accepts_a_well_formed_study() {
        let study = study_with_steps(&[1_000, 2_000]);
        assert!(validate_study(&study).is_ok());
    }

    #[test]
    fn validate_study_rejects_a_steps_crc_mismatch() {
        let mut study = study_with_steps(&[1_000]);
        study.steps_crc = study.steps_crc.wrapping_add(1);
        let err = validate_study(&study).unwrap_err();
        assert!(err.contains("steps_crc mismatch"), "{err}");
    }




    /// The two seals are checked independently, so a corrupt tap list is
    /// reported as a `streams_crc` failure and leaves `steps_crc`'s verdict
    /// alone — which is the property having two of them exists for.
    #[test]
    fn validate_study_rejects_a_streams_crc_mismatch_by_name() {
        use embarch_study_designer::{StreamEncoding, StreamScope, StreamSource};

        let mut study = study_with_steps(&[1_000]);
        study
            .streams
            .push(StreamTap {
                id: 0,
                name: heapless::String::try_from("outpost").unwrap(),
                source: StreamSource::Signal {
                    name: heapless::String::try_from("outpost").unwrap(),
                },
                encoding: StreamEncoding::Raw,
                scope: StreamScope::WholeStudy,
            })
            .unwrap();
        // Deliberately left at the empty-list seal the helper produced.
        assert_eq!(study.streams_crc, 0);
        let err = validate_study(&study).unwrap_err();
        assert!(err.contains("streams_crc mismatch"), "{err}");
        assert!(!err.contains("steps_crc mismatch"), "{err}");
    }


    // ---- next_deadline (watchdog math) ----

    #[test]
    fn next_deadline_uses_the_upcoming_steps_own_timeout_plus_grace() {
        let study = study_with_steps(&[1_000, 5_000]);
        let now = Instant::now();
        let deadline = next_deadline(&study, 1, now);
        assert_eq!(deadline, now + Duration::from_millis(5_000 + WATCHDOG_GRACE_MS));
    }

    #[test]
    fn next_deadline_uses_the_first_steps_timeout_at_the_start() {
        let study = study_with_steps(&[3_000, 5_000]);
        let now = Instant::now();
        let deadline = next_deadline(&study, 0, now);
        assert_eq!(deadline, now + Duration::from_millis(3_000 + WATCHDOG_GRACE_MS));
    }

    #[test]
    fn next_deadline_falls_back_to_the_last_steps_timeout_after_all_steps_report_in() {
        let study = study_with_steps(&[1_000, 5_000]);
        let now = Instant::now();
        // next_expected == steps.len(): every StepResult has arrived, we're
        // only waiting on the terminal StudyDone now.
        let deadline = next_deadline(&study, 2, now);
        assert_eq!(deadline, now + Duration::from_millis(5_000 + WATCHDOG_GRACE_MS));
    }

    /// The regression for design.md §3 decision 33. Against the old math
    /// (`timeout_ms + GRACE`, delay ignored) this study's window was 3s while
    /// dev-bench would not even *start* the step for 30s — a guaranteed
    /// spurious lapse against a bench doing exactly what it was told.
    #[test]
    fn next_deadline_includes_the_upcoming_steps_delay_before_ms() {
        let mut study = study_with_steps(&[1_000]);
        study.steps[0].delay_before_ms = 30_000;
        let now = Instant::now();
        let deadline = next_deadline(&study, 0, now);
        assert_eq!(deadline, now + Duration::from_millis(30_000 + 1_000 + WATCHDOG_GRACE_MS));
        // The defect this replaces, stated as the thing that must not be true:
        // a bench sleeping its authored delay must not outlive the window.
        assert!(deadline > now + Duration::from_millis(30_000));
    }

    #[test]
    fn next_deadline_ignores_the_last_steps_delay_once_only_study_done_is_left() {
        // Every StepResult has arrived, so the last step's delay is already
        // spent — re-adding it would widen the StudyDone wait for nothing.
        let mut study = study_with_steps(&[1_000, 5_000]);
        study.steps[1].delay_before_ms = 30_000;
        let now = Instant::now();
        let deadline = next_deadline(&study, 2, now);
        assert_eq!(deadline, now + Duration::from_millis(5_000 + WATCHDOG_GRACE_MS));
    }

    /// The per-step delay must be read from the step actually being waited on,
    /// not from the first step or from any aggregate.
    #[test]
    fn next_deadline_reads_the_delay_of_the_outstanding_step_only() {
        let mut study = study_with_steps(&[1_000, 2_000]);
        study.steps[0].delay_before_ms = 9_000;
        study.steps[1].delay_before_ms = 500;
        let now = Instant::now();
        assert_eq!(
            next_deadline(&study, 1, now),
            now + Duration::from_millis(500 + 2_000 + WATCHDOG_GRACE_MS)
        );
    }

    // ---- job registry state transitions ----

    fn empty_registry() -> JobRegistry {
        Arc::new(StdMutex::new(HashMap::new()))
    }

    fn running_job(total_steps: u32) -> StudyJob {
        StudyJob { status: "running".to_string(), current_step: None, total_steps: Some(total_steps), reason: None }
    }

    #[test]
    fn update_job_mutates_an_existing_entry() {
        let jobs = empty_registry();
        jobs.lock().unwrap().insert("abc".to_string(), running_job(3));

        update_job(&jobs, "abc", |job| job.current_step = Some(1));

        let job = jobs.lock().unwrap().get("abc").unwrap().clone();
        assert_eq!(job.current_step, Some(1));
        assert_eq!(job.status, "running");
    }

    #[test]
    fn update_job_on_a_missing_id_is_a_silent_no_op() {
        let jobs = empty_registry();
        update_job(&jobs, "missing", |job| job.current_step = Some(1));
        assert!(jobs.lock().unwrap().get("missing").is_none());
    }

    fn test_step_result(name: &str) -> StepResult {
        StepResult {
            step_name: heapless::String::try_from(name).unwrap(),
            outcome: embarch_study_designer::Outcome::Pass,
            captured_data: None,
            // embarch-study-designer/design.md §3 decisions 31/32 — new
            // fields this test fixture doesn't need to populate.
            gatt_services: None,
            gatt_activity: None,
            // Decision 44's `security_level`, likewise: this fixture has no
            // link, and `None` is what a step with no connection reports.
            security_level: None,
        }
    }

    #[test]
    fn fail_job_sets_status_and_reason() {
        let jobs = empty_registry();
        jobs.lock().unwrap().insert("abc".to_string(), running_job(3));
        let (events_tx, _rx) = broadcast::channel(16);

        fail_job(&jobs, &events_tx, "abc", "dev-bench connection error: dropped".to_string());

        let job = jobs.lock().unwrap().get("abc").unwrap().clone();
        assert_eq!(job.status, "failed");
        assert_eq!(job.reason.as_deref(), Some("dev-bench connection error: dropped"));
    }

    #[test]
    fn fail_job_broadcasts_a_status_changed_event() {
        let jobs = empty_registry();
        jobs.lock().unwrap().insert("abc".to_string(), running_job(3));
        let (events_tx, mut rx) = broadcast::channel(16);

        fail_job(&jobs, &events_tx, "abc", "boom".to_string());

        match rx.try_recv().unwrap() {
            StudyEvent::StatusChanged { study_id, status, reason } => {
                assert_eq!(study_id, "abc");
                assert_eq!(status, "failed");
                assert_eq!(reason.as_deref(), Some("boom"));
            }
            other => panic!("expected StatusChanged, got {other:?}"),
        }
    }

    /// Regression test for the real stack overflow this streaming rework
    /// fixes (`embarch-study-designer/design.md` §7): the old version of
    /// this test built a full `StudyResult` (~1.3 MB, all no_std worst-case
    /// capacity) and cloned a `StudyJob` holding one — both routinely
    /// overflowed a normal thread stack. `EventsJsonWriter` never
    /// constructs that type at all now; this only ever touches individual
    /// `StepResult`s (tens of KB at most) and the small `StudyJob`/
    /// `StudyEvent` types.
    #[test]
    fn finish_job_marks_completed_and_streams_the_result_to_disk() {
        let jobs = empty_registry();
        jobs.lock().unwrap().insert("abc".to_string(), running_job(2));
        let (events_tx, mut rx) = broadcast::channel(16);
        let dir = std::env::temp_dir().join(format!("embarch-study-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();

        let mut writer = EventsJsonWriter::start(&dir, "test-study", &provenance_for(&study_with_steps(&[1_000]), "gbench1", &no_run_params(), Default::default())).unwrap();
        writer.write_step(&test_step_result("step-0")).unwrap();

        finish_job(&jobs, &events_tx, "abc", writer);

        let job = jobs.lock().unwrap().get("abc").unwrap().clone();
        assert_eq!(job.status, "completed");

        // events.json is real, finalized (not left as `.partial`), and has
        // the exact shape callers expect — proof the manual streamed-JSON
        // construction produces the same thing the old whole-struct
        // `serde_json::to_writer` call did. Checked via `serde_json::Value`,
        // deliberately never `embarch_study_designer::StudyResult` itself:
        // deserializing *into* that type reconstructs its full no_std
        // worst-case capacity the same way constructing one used to —
        // proven by this exact assertion overflowing the stack the first
        // time this test was written, before being changed to this. It's
        // the type itself that's unsafe to materialize host-side, not just
        // the one code path that used to build it — `get_study_handler`
        // deliberately reads `events.json` back as a `Value` for the same
        // reason.
        assert!(dir.join("events.json").exists());
        assert!(!dir.join("events.json.partial").exists());
        let bytes = std::fs::read(dir.join("events.json")).unwrap();
        let result: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(result["study_name"], "test-study");
        assert_eq!(result["steps"].as_array().unwrap().len(), 1);
        assert_eq!(result["steps"][0]["step_name"], "step-0");
        // `validations` is deliberately *absent*, not empty: post-hoc
        // validation is gone, and this key used to be a hardcoded `[]` Core
        // wrote without ever having evaluated anything into it.
        assert!(result.get("validations").is_none());
        // `provenance` and `streams` were added to `StudyResult` at Phase A
        // and `events.json` had never carried either (decision 31). It does
        // now, and the DUT's version says it was only ever declared.
        assert_eq!(result["provenance"]["dev_bench_version"], "gbench1");
        assert_eq!(result["provenance"]["dev_bench_source"], "ReportedByDevBench");
        assert_eq!(result["provenance"]["firmware_source"], "Declared");
        assert!(result["streams"].as_array().unwrap().is_empty());

        match rx.try_recv().unwrap() {
            StudyEvent::StatusChanged { study_id, status, .. } => {
                assert_eq!(study_id, "abc");
                assert_eq!(status, "completed");
            }
            other => panic!("expected StatusChanged, got {other:?}"),
        }

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn events_json_carries_one_stream_ref_per_declared_tap_including_truncation() {
        // `StudyResult.streams` is what makes a short capture legible as a
        // short one. A tap that produced nothing is present with
        // `bytes_written: 0` rather than absent — a missing entry and an
        // empty one are different facts.
        let dir = tempfile::tempdir().unwrap();
        let study = study_with_taps(
            &[1_000],
            &[
                power_tap(),
                tap(1, "quiet", StreamSource::DevBenchLog, StreamEncoding::Raw),
            ],
        );
        let capture = test_capture(dir.path(), study);

        let record = StreamRecord {
            rx_utc_ms: 5,
            bytes: heapless::Vec::from_slice(&3.3f32.to_le_bytes()).unwrap(),
        };
        write_stream_record(&capture, &capture.study.streams[0].clone(), &record);
        // dev-bench reported dropped records on the second tap.
        capture.store.lock().unwrap().mark_lost_at_source(1);

        let mut writer =
            EventsJsonWriter::start(dir.path(), "s", &provenance_for(&capture.study, "gbench1", &no_run_params(), Default::default()))
                .unwrap();
        finish_streams(&capture, &mut writer);
        writer.finish().unwrap();

        let bytes = std::fs::read(dir.path().join("events.json")).unwrap();
        let result: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        let streams = result["streams"].as_array().unwrap();
        // Three, not two: the two declared taps plus the reserved
        // `dev-bench` log tap every study now gets, last and at its own id.
        assert_eq!(streams.len(), 3);
        assert_eq!(streams[0]["name"], "power");
        assert_eq!(streams[0]["bytes_written"], 4);
        assert_eq!(streams[0]["truncated"], false);
        assert_eq!(streams[1]["name"], "quiet");
        assert_eq!(streams[1]["bytes_written"], 0);
        assert_eq!(streams[1]["truncated"], true);
        // Present and reported even when dev-bench said nothing at all —
        // a tap that produced nothing reports `bytes_written: 0` rather than
        // going missing, same as any other.
        assert_eq!(streams[2]["name"], RESERVED_DEV_BENCH_STREAM_NAME);
        assert_eq!(streams[2]["bytes_written"], 0);
        assert_eq!(streams[2]["truncated"], false);
    }

    #[test]
    fn a_dev_bench_log_line_lands_in_the_studys_own_results_not_only_cores_log() {
        // The asymmetry the reserved tap exists to close. Before it, a
        // LogLine reached Core's rolling log and nothing else, so the
        // firmware's own account of a run was the one part of that run which
        // did not survive in the run's directory.
        let dir = tempfile::tempdir().unwrap();
        let study = study_with_steps(&[1_000]);
        let capture = test_capture(dir.path(), study);

        let reserved = capture.taps.last().unwrap().clone();
        assert_eq!(reserved.name.as_str(), RESERVED_DEV_BENCH_STREAM_NAME);
        // Its id is one past the last declared index — the rule both ends
        // derive independently, asserted here rather than assumed.
        assert_eq!(usize::from(reserved.id), capture.study.streams.len());
        assert_eq!(tap_for(&capture.taps, reserved.id).map(|t| t.name.as_str()), Some(RESERVED_DEV_BENCH_STREAM_NAME));

        let mut line = "link RX overrun".to_string();
        line.push('\n');
        write_stream_record(
            &capture,
            &reserved,
            &StreamRecord { rx_utc_ms: 7, bytes: heapless::Vec::from_slice(line.as_bytes()).unwrap() },
        );

        // A Text tap's raw file *is* its rendering (no second copy), so the
        // bytes are readable straight out of streams/.
        let written = std::fs::read_dir(dir.path().join("streams"))
            .unwrap()
            .filter_map(|e| e.ok())
            .find(|e| e.file_name().to_string_lossy().starts_with(RESERVED_DEV_BENCH_STREAM_NAME))
            .expect("the reserved tap has a file under streams/");
        let body = std::fs::read_to_string(written.path()).unwrap();
        assert!(body.contains("link RX overrun"), "got {body:?}");
    }

    #[test]
    fn events_json_writer_leaves_only_the_partial_file_until_finished() {
        let dir = std::env::temp_dir().join(format!("embarch-study-writer-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let mut writer = EventsJsonWriter::start(&dir, "s", &provenance_for(&study_with_steps(&[1_000]), "gbench1", &no_run_params(), Default::default())).unwrap();
        writer.write_step(&test_step_result("a")).unwrap();
        writer.write_step(&test_step_result("b")).unwrap();
        assert!(dir.join("events.json.partial").exists());
        assert!(!dir.join("events.json").exists());

        writer.finish().unwrap();
        assert!(!dir.join("events.json.partial").exists());

        // Deliberately checked as a `Value`, not deserialized into
        // `embarch_study_designer::StudyResult` — see the sibling test's
        // comment for why that specific assertion is exactly the overflow
        // this rework fixes, not just a style choice.
        let bytes = std::fs::read(dir.join("events.json")).unwrap();
        let result: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(result["steps"].as_array().unwrap().len(), 2);
        assert_eq!(result["steps"][0]["step_name"], "a");
        assert_eq!(result["steps"][1]["step_name"], "b");

        std::fs::remove_dir_all(&dir).ok();
    }

    // ---- write_sample: CSV row shape, including Core's own core_rx_utc_ms ----

    fn power_tap() -> StreamTap {
        tap(
            0,
            "power",
            StreamSource::PowerFrontEnd { sample_hz: 1_000 },
            StreamEncoding::Samples {
                layout: embarch_study_designer::SampleLayout::F32Le,
                unit: embarch_study_designer::Unit::Volts,
                channel_id: 0,
            },
        )
    }

    #[test]
    fn write_sample_appends_core_rx_utc_ms_and_writes_the_header_once() {
        let dir = tempfile::tempdir().unwrap();
        let study = study_with_taps(&[1_000], &[power_tap()]);
        let capture = test_capture(dir.path(), study);

        let sample = Sample {
            rx_utc_ms: 1_753_000_000_123,
            value: 3.3,
            unit: embarch_study_designer::Unit::Volts,
            channel_id: 0,
        };

        write_sample(&capture, 0, 0, sample);
        write_sample(&capture, 0, 0, sample);

        // Named by the tap, under `streams/` — the row shape is exactly what
        // `data.csv` carried before decision 30 moved the path.
        let contents =
            std::fs::read_to_string(dir.path().join("streams").join("power.csv")).unwrap();
        let mut lines = contents.lines();
        assert_eq!(lines.next().unwrap(), "rx_utc_ms,step_name,value,unit,channel_id,core_rx_utc_ms");
        let first_row = lines.next().unwrap();
        assert!(first_row.starts_with("1753000000123,step-0,3.3,volts,0,"));
        assert_eq!(lines.clone().count(), 1); // exactly one more data row
    }

    #[test]
    fn write_sample_for_an_out_of_range_step_is_written_with_an_empty_step_name() {
        // A real sample that Core simply can't label is still real data.
        // Dropping it because the label is missing is the worse trade —
        // the same one `write_transcript_entry` already makes.
        let dir = tempfile::tempdir().unwrap();
        let study = study_with_taps(&[1_000], &[power_tap()]);
        let capture = test_capture(dir.path(), study);

        let sample = Sample { rx_utc_ms: 1, value: 1.0, unit: embarch_study_designer::Unit::Raw, channel_id: 0 };

        write_sample(&capture, 0, 99, sample);

        let contents =
            std::fs::read_to_string(dir.path().join("streams").join("power.csv")).unwrap();
        assert!(contents.lines().nth(1).unwrap().starts_with("1,,1,raw,0,"));
    }

    // ---- decision 30(b): raw bytes always land before any decode ----------

    #[test]
    fn a_record_whose_decode_fails_still_costs_only_the_row_and_never_the_capture() {
        // The load-bearing sentence of decision 30(b): a tap writes its raw
        // bytes *first*, so a decode that fails leaves the run recoverable.
        // These bytes are not a `GattTranscriptEntry` and never will be.
        let dir = tempfile::tempdir().unwrap();
        let study = study_with_taps(
            &[1_000],
            &[tap(0, "gatt", StreamSource::GattTranscript, StreamEncoding::GattTranscript)],
        );
        let capture = test_capture(dir.path(), study);

        let record = StreamRecord {
            rx_utc_ms: 7,
            bytes: heapless::Vec::from_slice(&[0xff; 24]).unwrap(),
        };
        write_stream_record(&capture, &capture.study.streams[0].clone(), &record);

        let streams = dir.path().join("streams");
        assert_eq!(std::fs::read(streams.join("gatt.bin")).unwrap(), vec![0xffu8; 24]);
        // No row was rendered, and the rendered file was never even created.
        assert!(!streams.join("gatt.csv").exists());
        // The bytes are reported as captured, and nothing claims they were
        // lost — they weren't.
        let refs = capture.store.lock().unwrap().refs();
        assert_eq!(refs[0].bytes_written, 24);
        assert!(!refs[0].truncated);
    }

    #[test]
    fn a_raw_tap_writes_its_bytes_and_renders_nothing() {
        // No sniff, no heuristic, no "looks like text" fallback: `Raw` is
        // the honest default for a payload nobody declared a meaning for
        // (`embarch-study-designer/design.md` §3 decision 35).
        let dir = tempfile::tempdir().unwrap();
        let study = study_with_taps(
            &[1_000],
            &[tap(
                0,
                "trace",
                StreamSource::Signal { name: heapless::String::try_from("outpost").unwrap() },
                StreamEncoding::Raw,
            )],
        );
        let capture = test_capture(dir.path(), study);

        let record =
            StreamRecord { rx_utc_ms: 1, bytes: heapless::Vec::from_slice(b"hello").unwrap() };
        write_stream_record(&capture, &capture.study.streams[0].clone(), &record);

        let streams = dir.path().join("streams");
        assert_eq!(std::fs::read(streams.join("trace.bin")).unwrap(), b"hello");
        assert!(!streams.join("trace.csv").exists());
        assert!(!streams.join("trace.txt").exists());
    }

    #[test]
    fn a_record_arriving_during_step_1_is_labelled_step_1_not_step_0() {
        // `embarch-study-designer/design.md` §4.8 defines this column as
        // "whichever step the host has open when the record arrives" —
        // which is the step now *running*, not the one that just finished.
        // Phase A's adaptation used the latter, and the two agree only
        // before the first `StepResult`, which is exactly why the off-by-one
        // went unnoticed: the retired `GattTranscriptRecord` carried its own
        // `step_index` from dev-bench, so this only became the host's number
        // to compute at Phase A. Pinned at both writers.
        let dir = tempfile::tempdir().unwrap();
        let study = study_with_taps(
            &[1_000, 2_000],
            &[
                power_tap(),
                tap(1, "gatt", StreamSource::GattTranscript, StreamEncoding::GattTranscript),
            ],
        );
        let capture = test_capture(dir.path(), study);

        // dev-bench has just reported step 0's result, so step 1 is open.
        capture.current_step.store(1, Ordering::Relaxed);
        let step = capture.current_step.load(Ordering::Relaxed);

        let sample = Sample {
            rx_utc_ms: 1,
            value: 1.0,
            unit: embarch_study_designer::Unit::Volts,
            channel_id: 0,
        };
        write_sample(&capture, 0, step, sample);
        write_transcript_entry(&capture, 1, step, &transcript_entry(b"x"));

        let streams = dir.path().join("streams");
        let power = std::fs::read_to_string(streams.join("power.csv")).unwrap();
        assert!(
            power.lines().nth(1).unwrap().contains(",step-1,"),
            "expected the open step, got: {}",
            power.lines().nth(1).unwrap()
        );
        let gatt = std::fs::read_to_string(streams.join("gatt.csv")).unwrap();
        let row = gatt.lines().nth(1).unwrap();
        assert!(row.starts_with("4242,1,step-1,"), "expected the open step, got: {row}");
    }

    // ---- decision 30(a): Core's own signal-tap port -----------------------

    #[test]
    fn a_signal_taps_port_is_open_exactly_across_its_declared_scope() {
        // The whole-study tap the outpost uses is open from step 0 onward;
        // a windowed one opens and closes on its own inclusive boundaries,
        // the same ones dev-bench uses for the taps it mediates.
        let signal = StreamSource::Signal { name: heapless::String::try_from("outpost").unwrap() };
        let whole = tap(0, "trace", signal.clone(), StreamEncoding::Raw);
        let mut windowed = tap(1, "burst", signal, StreamEncoding::Raw);
        windowed.scope = embarch_study_designer::StreamScope::Steps { from: 1, to: 2 };

        assert!(signal_tap_is_wanted(&whole, 0));
        assert!(signal_tap_is_wanted(&whole, 7));

        assert!(!signal_tap_is_wanted(&windowed, 0));
        assert!(signal_tap_is_wanted(&windowed, 1));
        assert!(signal_tap_is_wanted(&windowed, 2), "the range is inclusive at both ends");
        assert!(!signal_tap_is_wanted(&windowed, 3));
    }

    // ---- decision 31: the POST /study version gate ------------------------

    fn requires(dev_bench: &str) -> Requirements {
        Requirements {
            dev_bench_version: heapless::String::try_from(dev_bench).unwrap(),
            firmware_version: heapless::String::try_from("any").unwrap(),
        }
    }

    fn requires_pair(dev_bench: &str, firmware: &str) -> Requirements {
        Requirements {
            dev_bench_version: heapless::String::try_from(dev_bench).unwrap(),
            firmware_version: heapless::String::try_from(firmware).unwrap(),
        }
    }

    /// No override, nothing flashed — the shape every pre-item-2 caller had.
    fn no_run_params() -> StudyRunParams {
        StudyRunParams::default()
    }

    fn allow_mismatch() -> StudyRunParams {
        StudyRunParams { allow_version_mismatch: Some("1".to_string()), ..Default::default() }
    }

    fn flashed(version: &str) -> StudyRunParams {
        StudyRunParams {
            flashed_firmware_version: Some(version.to_string()),
            ..Default::default()
        }
    }

    #[test]
    fn a_dev_bench_version_mismatch_is_a_409_and_no_step_ever_runs() {
        // The assertion that matters is the second one. `StudyStart` is the
        // only message that can make dev-bench execute anything, so a gate
        // that returned 409 *after* sending it would still have run steps —
        // which is exactly what decision 31 forbids, and exactly what a test
        // asserting only the status code would miss.
        let mut sent = false;
        let outcome = gate_then_start(&requires("g1a2b3c"), "gdeadbee", &no_run_params(), || {
            sent = true;
            Ok(())
        });

        let (status, msg) = outcome.expect_err("a mismatch must be rejected");
        assert_eq!(status, StatusCode::CONFLICT);
        assert!(!sent, "StudyStart must not be sent on a version mismatch — no step may run");
        // Both strings named, so the operator can see which way to fix it.
        assert!(msg.contains("g1a2b3c"), "{msg}");
        assert!(msg.contains("gdeadbee"), "{msg}");
    }

    #[test]
    fn an_exact_version_match_starts_the_study() {
        let mut sent = false;
        gate_then_start(&requires("g1a2b3c"), "g1a2b3c", &no_run_params(), || {
            sent = true;
            Ok(())
        })
        .expect("an exact match satisfies the requirement");
        assert!(sent);
    }

    #[test]
    fn any_is_a_value_that_satisfies_every_bench() {
        let mut sent = false;
        gate_then_start(&requires("any"), "whatever-the-bench-happens-to-be", &no_run_params(), || {
            sent = true;
            Ok(())
        })
        .expect("`any` is an explicit, legal way to say the build doesn't matter");
        assert!(sent);
    }

    #[test]
    fn a_send_failure_is_a_bad_gateway_not_a_version_rejection() {
        let (status, msg) = gate_then_start(&requires("any"), "x", &no_run_params(), || Err("serial died".to_string()))
            .expect_err("a failed send is still a failure");
        assert_eq!(status, StatusCode::BAD_GATEWAY);
        assert_eq!(msg, "serial died");
    }

    #[test]
    fn provenance_never_presents_a_declared_dut_version_as_a_verified_one() {
        // Decision 31's asymmetry, made structural: dev-bench self-reports,
        // so its source is `ReportedByDevBench`. There is no readback path
        // from a DUT, and Core does not flash one as part of a study, so the
        // firmware version is `Declared` — an assertion nobody checked, and
        // it says so.
        let study = study_with_steps(&[1_000]);
        let provenance = provenance_for(&study, "gfeedface", &no_run_params(), Default::default());
        assert_eq!(provenance.dev_bench_version.as_str(), "gfeedface");
        assert_eq!(provenance.dev_bench_source, VersionSource::ReportedByDevBench);
        assert_eq!(provenance.firmware_source, VersionSource::Declared);
        assert!(provenance.dev_bench_source.is_verified());
        assert!(!provenance.firmware_source.is_verified());
    }

    #[test]
    fn an_override_proceeds_and_the_result_records_what_was_waved_through() {
        // `embarch-study-designer/design.md` §3 decision 40: the override is
        // "recorded in the result rather than silently honoured". The
        // assertion that matters is the third one — a run that proceeded past
        // a requirement must not be indistinguishable from one that met it.
        let mut sent = false;
        let overrides = gate_then_start(&requires("g1a2b3c"), "gdeadbee", &allow_mismatch(), || {
            sent = true;
            Ok(())
        })
        .expect("an explicit override proceeds");
        assert!(sent, "the study runs — that is what the override is for");

        let provenance = provenance_for(
            &study_with_steps(&[1_000]),
            "gdeadbee",
            &allow_mismatch(),
            overrides,
        );
        assert!(provenance.was_overridden());
        let recorded = provenance
            .override_for(VersionSubject::DevBench)
            .expect("the dev-bench requirement is the one that was waved through");
        assert_eq!(recorded.required.as_str(), "g1a2b3c");
        assert_eq!(recorded.actual.as_str(), "gdeadbee");
    }

    #[test]
    fn a_satisfied_gate_records_no_override_even_when_one_was_allowed() {
        // `--allow-version-mismatch` is permission, not an assertion that
        // anything mismatched. A run that passed the gate on its own merits
        // must not be marked as having been waved through.
        let overrides =
            gate_then_start(&requires("g1a2b3c"), "g1a2b3c", &allow_mismatch(), || Ok(()))
                .expect("an exact match satisfies the requirement");
        let provenance =
            provenance_for(&study_with_steps(&[1_000]), "g1a2b3c", &allow_mismatch(), overrides);
        assert!(!provenance.was_overridden());
    }

    #[test]
    fn the_dut_requirement_is_only_checked_when_this_run_says_it_flashed_one() {
        // Core has no readback path from a DUT (decision 31). Absent a
        // caller that flashed it, `requires.firmware_version` is compared
        // against nothing and recorded as `Declared` — so a study demanding a
        // specific DUT build still starts, exactly as it did before item 2.
        let mut study = study_with_steps(&[1_000]);
        study.requires = requires_pair("any", "g-dut-aaaa");
        let mut sent = false;
        let overrides = gate_then_start(
            &requires_pair("any", "g-dut-aaaa"),
            "gbench1",
            &no_run_params(),
            || {
                sent = true;
                Ok(())
            },
        )
        .expect("an unverifiable DUT requirement is not a rejection");
        assert!(sent);
        assert!(overrides.is_empty());

        let provenance = provenance_for(&study, "gbench1", &no_run_params(), overrides);
        assert_eq!(provenance.firmware_source, VersionSource::Declared);
        assert_eq!(provenance.firmware_version.as_str(), "g-dut-aaaa");
        assert!(!provenance.firmware_source.is_verified());
    }

    #[test]
    fn a_flashed_dut_version_makes_the_firmware_gate_fire_and_the_source_verified() {
        // The whole point of `embarch-api` supplying this: the DUT half of
        // the gate is unreachable from Core on its own (decision 31's
        // implementation note), so this is the first path on which a wrong
        // DUT build can be rejected at all.
        let study = study_with_steps(&[1_000]);

        let mut sent = false;
        let (status, msg) = gate_then_start(
            &requires_pair("any", "g-dut-aaaa"),
            "gbench1",
            &flashed("g-dut-bbbb"),
            || {
                sent = true;
                Ok(())
            },
        )
        .expect_err("a DUT build the study did not ask for is a rejection");
        assert_eq!(status, StatusCode::CONFLICT);
        assert!(!sent, "no step may run on a firmware mismatch either");
        assert!(msg.contains("g-dut-aaaa"), "{msg}");
        assert!(msg.contains("g-dut-bbbb"), "{msg}");

        // The matching case records what was actually put on the board, not
        // what the study asked for — the two are the same string here, and
        // the *source* is what makes the difference legible.
        let overrides = gate_then_start(
            &requires_pair("any", "g-dut-aaaa"),
            "gbench1",
            &flashed("g-dut-aaaa"),
            || Ok(()),
        )
        .expect("the build this run flashed is the one the study wanted");
        let provenance =
            provenance_for(&study, "gbench1", &flashed("g-dut-aaaa"), overrides);
        assert_eq!(provenance.firmware_source, VersionSource::FlashedThisRun);
        assert_eq!(provenance.firmware_version.as_str(), "g-dut-aaaa");
        assert!(provenance.firmware_source.is_verified());
    }

    #[test]
    fn a_flashed_dut_version_wins_over_the_declared_one_in_the_result() {
        // `requires.firmware_version: any` is a legitimate study that still
        // flashed something real. The result has to say *what* ran, not
        // "any" — which is the gap decision 40 was opened by.
        let study = study_with_steps(&[1_000]);
        let overrides =
            gate_then_start(&requires_pair("any", "any"), "gbench1", &flashed("g-dut-cccc"), || {
                Ok(())
            })
            .expect("`any` is satisfied by whatever was flashed");
        let provenance = provenance_for(&study, "gbench1", &flashed("g-dut-cccc"), overrides);
        assert_eq!(provenance.firmware_version.as_str(), "g-dut-cccc");
        assert_eq!(provenance.firmware_source, VersionSource::FlashedThisRun);
    }

    #[test]
    fn both_requirements_can_be_waved_through_in_one_run() {
        let study = study_with_steps(&[1_000]);
        let run = StudyRunParams {
            allow_version_mismatch: Some("1".to_string()),
            flashed_firmware_version: Some("g-dut-bbbb".to_string()),
        };
        let overrides =
            gate_then_start(&requires_pair("g1a2b3c", "g-dut-aaaa"), "gdeadbee", &run, || Ok(()))
                .expect("both are explicitly allowed");
        assert_eq!(overrides.len(), 2);
        let provenance = provenance_for(&study, "gdeadbee", &run, overrides);
        assert!(provenance.override_for(VersionSubject::DevBench).is_some());
        assert!(provenance.override_for(VersionSubject::Firmware).is_some());
    }

    #[test]
    fn only_an_explicit_query_value_turns_the_override_on() {
        // A typo'd or absent parameter must not read as permission to
        // proceed past a version gate.
        for raw in [None, Some(""), Some("0"), Some("false"), Some("yes"), Some("TRUE")] {
            let run = StudyRunParams {
                allow_version_mismatch: raw.map(str::to_string),
                ..Default::default()
            };
            assert!(
                gate_then_start(&requires("g1a2b3c"), "gdeadbee", &run, || Ok(())).is_err(),
                "allow_version_mismatch={raw:?} must not wave a mismatch through"
            );
        }
        for raw in ["1", "true"] {
            let run = StudyRunParams {
                allow_version_mismatch: Some(raw.to_string()),
                ..Default::default()
            };
            assert!(gate_then_start(&requires("g1a2b3c"), "gdeadbee", &run, || Ok(())).is_ok());
        }
    }

    // ---- serving a capture back: GET /study/{id}/stream/{name} -------------

    #[tokio::test]
    async fn a_stream_is_served_rendered_by_default_and_raw_on_request() {
        let dir = tempfile::tempdir().unwrap();
        let study = study_with_taps(&[1_000], &[power_tap()]);
        let capture = test_capture(dir.path(), study);
        let record = StreamRecord {
            rx_utc_ms: 5,
            bytes: heapless::Vec::from_slice(&3.3f32.to_le_bytes()).unwrap(),
        };
        write_stream_record(&capture, &capture.study.streams[0].clone(), &record);

        let streams_dir = dir.path().join("streams");
        let index = stream_store::read_index(&streams_dir).unwrap().unwrap();
        let entry = index.find("power").expect("the tap resolves by its declared name");

        let rendered = serve_capture(&streams_dir, entry, false, "power").await.unwrap();
        assert_eq!(rendered.status(), StatusCode::OK);
        assert_eq!(rendered.headers().get(CONTENT_TYPE).unwrap(), "text/csv");

        let raw = serve_capture(&streams_dir, entry, true, "power").await.unwrap();
        assert_eq!(raw.headers().get(CONTENT_TYPE).unwrap(), "application/octet-stream");

        // And the three retired routes still resolve, through the same index.
        assert_eq!(index.find_alias("power").map(|e| e.name.as_str()), Some("power"));
        assert!(index.find_alias("gatt").is_none());
        assert!(index.find("no-such-tap").is_none());
    }

    /// **The test the aliases exist for** (`embarch-core/design.md` §3
    /// decision 30, `embarch-api/design.md` §3 decision 39): each of the
    /// three retired fixed routes has to keep answering with *exactly* what
    /// its replacement answers with, for one release, or an agent
    /// mid-conversation gets silently different data from the same call it
    /// was already making.
    ///
    /// Asserting the bodies are byte-identical is the point — two paths that
    /// each merely return "some CSV" would pass a test that checked only
    /// status codes, and diverge in content the first time one of them picks
    /// a different entry out of the index.
    #[tokio::test]
    async fn each_alias_returns_byte_for_byte_what_its_replacement_returns() {
        let dir = tempfile::tempdir().unwrap();
        let gatt = tap(1, "gatt-transcript", StreamSource::GattTranscript, StreamEncoding::GattTranscript);
        let waveform = tap(
            2,
            "sensor-waveform",
            StreamSource::GattNotify {
                service_uuid: embarch_study_designer::Uuid::parse(
                    "6e400001-b5a3-f393-e0a9-e50e24dcca9e",
                )
                .unwrap(),
                characteristic_uuid: embarch_study_designer::Uuid::parse(
                    "6e400003-b5a3-f393-e0a9-e50e24dcca9e",
                )
                .unwrap(),
            },
            StreamEncoding::Samples {
                layout: embarch_study_designer::SampleLayout::F32Le,
                unit: embarch_study_designer::Unit::Volts,
                channel_id: 1,
            },
        );
        let study = study_with_taps(&[1_000], &[power_tap(), gatt, waveform]);
        let capture = test_capture(dir.path(), study);

        // Give all three taps something to serve.
        for tap in capture.study.streams.clone().iter() {
            let bytes: heapless::Vec<u8, { embarch_study_designer::limits::MAX_STREAM_CHUNK_BYTES }> =
                match tap.encoding {
                    StreamEncoding::GattTranscript => {
                        let entry = transcript_entry(b"\x01\x02");
                        let mut buf = [0u8; 256];
                        let encoded = postcard::to_slice(&entry, &mut buf).unwrap();
                        heapless::Vec::from_slice(encoded).unwrap()
                    }
                    _ => heapless::Vec::from_slice(&3.3f32.to_le_bytes()).unwrap(),
                };
            write_stream_record(&capture, tap, &StreamRecord { rx_utc_ms: 7, bytes });
        }

        let streams_dir = dir.path().join("streams");
        let index = stream_store::read_index(&streams_dir).unwrap().unwrap();

        for (alias, tap_name) in [
            ("power", "power"),
            ("gatt", "gatt-transcript"),
            ("waveform", "sensor-waveform"),
        ] {
            let via_alias = index
                .find_alias(alias)
                .unwrap_or_else(|| panic!("alias '{alias}' must resolve"));
            let via_name = index
                .find(tap_name)
                .unwrap_or_else(|| panic!("tap '{tap_name}' must resolve by its declared name"));
            assert_eq!(
                via_alias.name, via_name.name,
                "alias '{alias}' and tap '{tap_name}' must be the same tap"
            );

            let alias_body = body_bytes(serve_capture(&streams_dir, via_alias, false, alias).await.unwrap()).await;
            let stream_body =
                body_bytes(serve_capture(&streams_dir, via_name, false, tap_name).await.unwrap()).await;
            assert_eq!(
                alias_body, stream_body,
                "alias '{alias}' and study_stream_data('{tap_name}') must return identical bytes"
            );
            assert!(!alias_body.is_empty(), "alias '{alias}' returned nothing");
        }
    }

    async fn body_bytes(response: Response) -> Vec<u8> {
        axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap()
            .to_vec()
    }

    #[tokio::test]
    async fn a_declared_tap_that_captured_nothing_is_a_404_rather_than_an_empty_body() {
        let dir = tempfile::tempdir().unwrap();
        let study = study_with_taps(&[1_000], &[power_tap()]);
        let _capture = test_capture(dir.path(), study);

        let streams_dir = dir.path().join("streams");
        let index = stream_store::read_index(&streams_dir).unwrap().unwrap();
        let entry = index.find("power").unwrap();

        let (status, _) = serve_capture(&streams_dir, entry, false, "power data")
            .await
            .expect_err("nothing was captured");
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    // ---- dev-bench link identity (§3 decision 35) ----

    #[test]
    fn only_a_declared_disagreement_refuses_the_link() {
        // The gate in `open_and_handshake` refuses on exactly one variant.
        // Pinned as a test because the tempting version of this — "refuse
        // unless it matched" — refuses every healthy bench on the suite's
        // only board, since no chip has a declared relation yet.
        use embarch_topology::hardware::SelfReportedIdentity as S;
        let refuses = |identity: S| identity == S::Mismatch;

        assert!(refuses(S::Mismatch));
        assert!(!refuses(S::Match));
        assert!(!refuses(S::NotReported), "a bench that cannot answer is not a wrong bench");
        assert!(
            !refuses(S::Undeclared),
            "Core not knowing how to relate two encodings is not evidence of a wrong board"
        );
    }

    #[test]
    fn identity_words_are_stable_and_distinct() {
        // These reach `GET /dev-bench/hello`'s JSON and a log line, and are
        // what a human greps for while working out the pairing that turns
        // `undeclared` into a real relation — so they are API, not prose.
        use embarch_topology::hardware::SelfReportedIdentity as S;
        let all = [S::Match, S::Mismatch, S::NotReported, S::Undeclared];
        let words: Vec<&str> = all.iter().map(|i| describe_identity(*i)).collect();
        assert_eq!(words, vec!["match", "mismatch", "not-reported", "undeclared"]);

        let mut unique = words.clone();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(unique.len(), all.len());
    }

    // ---- generate_study_id ----

    #[test]
    fn generate_study_id_is_hex_and_not_trivially_colliding() {
        let a = generate_study_id();
        let b = generate_study_id();
        assert_eq!(a.len(), 32);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, b);
    }

    /// The reason `GET /study/{id}/streams` exists at all: over HTTP, a
    /// refused trace has to be distinguishable from a named one. Nothing else
    /// on Core's surface carries that fact.
    #[test]
    fn the_stream_index_response_carries_why_a_trace_has_no_names() {
        let index = stream_store::StreamIndex {
            version: 1,
            streams: vec![
                stream_store::StreamIndexEntry {
                    id: 0,
                    name: "outpost".to_string(),
                    raw_file: "outpost.bin".to_string(),
                    rendered_file: Some("outpost.trace.csv".to_string()),
                    encoding: StreamEncoding::OutpostTrace,
                    alias: None,
                    note: Some("decoded but NOT named: manifest build_id \"a\" != firmware build_id \"b\"".to_string()),
                },
                stream_store::StreamIndexEntry {
                    id: 1,
                    name: "power".to_string(),
                    raw_file: "power.bin".to_string(),
                    rendered_file: Some("power.csv".to_string()),
                    encoding: StreamEncoding::Raw,
                    alias: Some("power".to_string()),
                    note: None,
                },
            ],
        };

        let response = stream_index_response(index);
        let trace = &response.streams[0];
        assert_eq!(trace.name, "outpost");
        assert!(trace.rendered, "a rendered trace must report that it rendered");
        assert!(
            trace.note.as_deref().is_some_and(|n| n.contains("NOT named")),
            "the refusal reason must reach an HTTP caller, or a UI cannot tell an unnamed trace \
             from a named one"
        );
        // The unrefused tap says nothing, rather than saying "fine" — an
        // absent note is what "nothing to report" looks like.
        assert_eq!(response.streams[1].note, None);
        assert_eq!(response.streams[1].alias.as_deref(), Some("power"));
    }

    /// A tap whose encoding has no rendering must not claim one, or a caller
    /// will present raw bytes as a decoded answer.
    #[test]
    fn a_tap_with_no_rendering_says_so() {
        let index = stream_store::StreamIndex {
            version: 1,
            streams: vec![stream_store::StreamIndexEntry {
                id: 0,
                name: "raw".to_string(),
                raw_file: "raw.bin".to_string(),
                rendered_file: None,
                encoding: StreamEncoding::Raw,
                alias: None,
                note: None,
            }],
        };
        assert!(!stream_index_response(index).streams[0].rendered);
    }
}
