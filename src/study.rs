//! Core's bridge between `embarch-api`'s HTTP `/study*` surface and
//! `embarch-dev-bench` firmware's serial link (`dev_bench_link.rs`).
//!
//! `embarch-study-designer/design.md` §5.1 (the `POST /study` async job
//! model) and §5.2 (`events.json`/`data.csv`/`waveform.csv` layout) are the
//! finalized design this module implements. Section references in doc
//! comments below point back into that document.

use axum::extract::{Json, Path, State};
use axum::http::{header::CONTENT_TYPE, StatusCode};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use futures_util::StreamExt as _;
use serde::Serialize;
use std::collections::HashMap;
use std::convert::Infallible;
use std::io::Write as _;
use std::path::{Path as FsPath, PathBuf};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant};
use tokio::sync::broadcast;
use tokio_stream::wrappers::{errors::BroadcastStreamRecvError, BroadcastStream};

use embarch_study_designer::{
    steps_crc, DevBenchMessage, Sample, StepResult, StreamChannel, Study, STUDY_DESIGNER_SCHEMA_VERSION,
};

use crate::api::{internal_err, AppState};
use crate::dev_bench_link::DevBenchLink;
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
    SampleBatch { study_id: String, step_index: u32, channel: StreamChannel, samples: Vec<Sample> },
    /// The job's own `status`/`reason` changed — `"completed"` or `"failed"`.
    StatusChanged { study_id: String, status: String, reason: Option<String> },
}

// ---- pure validation (no HTTP, no hardware — unit-testable directly) ------

/// design.md §5.1 steps 2-3: every `PostHocValidation.source.step_index` must
/// be in range, and `steps_crc` must match what `study.steps` recomputes to.
/// Factored out from the handler so it's testable with no HTTP plumbing —
/// the same posture `embarch_topology::hardware`'s own port-selection logic
/// takes.
fn validate_study(study: &Study) -> Result<(), String> {
    for validation in study.validations.iter() {
        if validation.source.step_index as usize >= study.steps.len() {
            return Err(format!(
                "validations[].source.step_index {} is out of range for a study with {} step(s)",
                validation.source.step_index,
                study.steps.len()
            ));
        }
    }

    let recomputed = steps_crc(&study.steps)
        .map_err(|_| "failed to recompute steps_crc (a step's encoding is unexpectedly large)".to_string())?;
    if recomputed != study.steps_crc {
        return Err(format!(
            "steps_crc mismatch: submitted study.steps_crc is {}, but recomputing over study.steps gives {recomputed} — \
             the submitted steps don't match their own checksum",
            study.steps_crc
        ));
    }

    Ok(())
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

fn study_results_dir(study_id: &str) -> anyhow::Result<PathBuf> {
    Ok(token_store::local_data_dir()?.join("study_results").join(study_id))
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
/// step's own `timeout_ms`; once every step has reported in, the last step's
/// `timeout_ms` is reused as the wait for the terminal `StudyDone`. Pure and
/// `now`-parameterized so the deadline math is unit-testable without a clock
/// or a real study run.
fn next_deadline(study: &Study, next_expected: usize, now: Instant) -> Instant {
    let step = study.steps.get(next_expected).or_else(|| study.steps.last());
    let timeout_ms = step.map(|s| s.timeout_ms as u64).unwrap_or(0);
    now + Duration::from_millis(timeout_ms) + Duration::from_millis(WATCHDOG_GRACE_MS)
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
}

/// Opens `port_name`, sends `Hello`, and waits for `HelloAck`. Runs entirely
/// inside `spawn_blocking` (all serial I/O is blocking) — `Err` carries a
/// human-readable message describing what failed, not a status code, since
/// this is called before we know whether that maps to `502`, `504`, etc.
async fn open_and_handshake(port_name: String) -> Result<(DevBenchLink, HelloAckInfo), String> {
    tokio::task::spawn_blocking(move || {
        let mut link = DevBenchLink::open(&port_name).map_err(|e| format!("{e:?}"))?;

        link.send(&DevBenchMessage::Hello {
            schema_version: STUDY_DESIGNER_SCHEMA_VERSION,
            host_utc_ms: current_utc_ms(),
        })
        .map_err(|e| format!("failed to send Hello to dev-bench: {e:?}"))?;

        let deadline = Instant::now() + Duration::from_millis(HANDSHAKE_TIMEOUT_MS);
        match link.recv(deadline) {
            Ok(Some(DevBenchMessage::HelloAck { schema_version, compatible, firmware_version })) => {
                tracing::info!(
                    dev_bench_schema_version = schema_version,
                    core_schema_version = STUDY_DESIGNER_SCHEMA_VERSION,
                    %firmware_version,
                    compatible,
                    "dev-bench Hello/HelloAck handshake complete"
                );
                if !compatible {
                    return Err(format!(
                        "dev-bench firmware (schema version {schema_version}, firmware_version '{firmware_version}') \
                         is not compatible with Core's schema version {STUDY_DESIGNER_SCHEMA_VERSION}"
                    ));
                }
                let info = HelloAckInfo {
                    schema_version,
                    compatible,
                    firmware_version: firmware_version.to_string(),
                };
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
async fn enforce_dev_bench_gate() -> Result<(), String> {
    tokio::task::spawn_blocking(|| {
        embarch_topology::hardware::validate_role(embarch_topology::hardware::DEV_BENCH_ROLE)
    })
    .await
    .map_err(|e| format!("board-identity gate task panicked: {e:?}"))?
    .map_err(|e| format!("{e:?}"))?;

    Ok(())
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

    enforce_dev_bench_gate()
        .await
        .map_err(|msg| (StatusCode::BAD_GATEWAY, msg))?;

    let (_link, info) = open_and_handshake(port.port_name)
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
    Json(study): Json<Study>,
) -> Result<(StatusCode, Json<StudyAcceptedResponse>), (StatusCode, String)> {
    validate_study(&study).map_err(|msg| (StatusCode::BAD_REQUEST, msg))?;

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

    if let Err(msg) = enforce_dev_bench_gate().await {
        release_lock();
        return Err((StatusCode::BAD_GATEWAY, msg));
    }

    let mut link = match open_and_handshake(port.port_name).await {
        Ok((link, _info)) => link,
        Err(msg) => {
            release_lock();
            return Err((StatusCode::BAD_GATEWAY, msg));
        }
    };

    let steps = study.steps.clone();
    let steps_crc_value = study.steps_crc;
    let link = match tokio::task::spawn_blocking(move || {
        link.send(&DevBenchMessage::StudyStart { steps, steps_crc: steps_crc_value })
            .map(|_| link)
    })
    .await
    {
        Ok(Ok(link)) => link,
        Ok(Err(e)) => {
            release_lock();
            return Err((StatusCode::BAD_GATEWAY, format!("failed to send StudyStart to dev-bench: {e:?}")));
        }
        Err(e) => {
            release_lock();
            return Err(internal_err(e));
        }
    };

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
    let study_id_for_task = study_id.clone();

    tokio::spawn(async move {
        let jobs_for_panic = jobs.clone();
        let events_tx_for_panic = events_tx.clone();
        let study_id_for_panic = study_id_for_task.clone();

        let outcome = tokio::task::spawn_blocking(move || {
            run_study_to_completion(link, study, study_id_for_task, jobs, events_tx)
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

/// Owns the dev-bench link for the rest of the study's lifetime: receives
/// `DevBenchMessage`s until `StudyDone` arrives or the watchdog fires,
/// updating the job registry and `data.csv`/`waveform.csv`/`events.json` as
/// it goes (design.md §5.1 steps 8+, §5.2). Entirely blocking — called from
/// `spawn_blocking` by [`post_study_handler`].
fn run_study_to_completion(
    mut link: DevBenchLink,
    study: Study,
    study_id: String,
    jobs: JobRegistry,
    events_tx: broadcast::Sender<StudyEvent>,
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

    // Streams `events.json` to disk as each `StepResult` arrives, rather
    // than accumulating them and assembling a `StudyResult` only at the
    // end — see `EventsJsonWriter`'s own doc comment for why that
    // accumulate-then-build step is exactly what used to overflow the
    // stack (`embarch-study-designer/design.md` §7).
    let mut writer = match EventsJsonWriter::start(&results_dir, study.name.as_str()) {
        Ok(w) => w,
        Err(e) => {
            fail_job(&jobs, &events_tx, &study_id, format!("failed to open events.json for writing: {e:?}"));
            return;
        }
    };

    let mut current_stream: Option<(u32, StreamChannel)> = None;
    let mut next_expected: usize = 0;
    let mut deadline = next_deadline(&study, next_expected, Instant::now());

    loop {
        match link.recv(deadline) {
            Ok(Some(DevBenchMessage::StepResult { step_index, result })) => {
                if let Err(e) = writer.write_step(&result) {
                    fail_job(
                        &jobs,
                        &events_tx,
                        &study_id,
                        format!("failed to write step {step_index}'s result to events.json: {e:?}"),
                    );
                    return;
                }
                let _ = events_tx.send(StudyEvent::StepCompleted {
                    study_id: study_id.clone(),
                    step_index,
                    result: Box::new(result),
                });
                next_expected = step_index as usize + 1;
                update_job(&jobs, &study_id, |job| job.current_step = Some(step_index));
                deadline = next_deadline(&study, next_expected, Instant::now());
            }
            Ok(Some(DevBenchMessage::StudyDone { completed })) => {
                tracing::info!(study_id, completed, "study run finished (StudyDone)");
                finish_job(&jobs, &events_tx, &study_id, writer);
                return;
            }
            Ok(Some(DevBenchMessage::StreamStart { step_index, channel })) => {
                current_stream = Some((step_index, channel));
            }
            Ok(Some(DevBenchMessage::StreamEnd { step_index, channel })) => {
                if current_stream == Some((step_index, channel)) {
                    current_stream = None;
                }
            }
            Ok(Some(DevBenchMessage::StreamChunk { sample })) => {
                write_sample(&results_dir, &study, current_stream, sample);
                broadcast_sample_batch(&events_tx, &study_id, current_stream, &[sample]);
            }
            Ok(Some(DevBenchMessage::StreamChunkBatch {
                base_utc_ms,
                sample_interval_ms,
                unit,
                channel_id,
                values,
            })) => {
                let mut samples = Vec::with_capacity(values.len());
                for (i, value) in values.iter().enumerate() {
                    let sample = Sample {
                        rx_utc_ms: base_utc_ms + (i as u64) * (sample_interval_ms as u64),
                        value: *value,
                        unit,
                        channel_id,
                    };
                    write_sample(&results_dir, &study, current_stream, sample);
                    samples.push(sample);
                }
                broadcast_sample_batch(&events_tx, &study_id, current_stream, &samples);
            }
            Ok(Some(DevBenchMessage::LogLine { text })) => {
                tracing::debug!(study_id, "dev-bench: {text}");
            }
            Ok(Some(other)) => {
                // Hello/HelloAck/StudyStart are Core->dev-bench (or
                // handshake-only) messages; dev-bench shouldn't send them
                // back once a study is running. Not fatal on its own.
                tracing::warn!(study_id, "unexpected message from dev-bench mid-study: {other:?}");
            }
            Ok(None) => {
                fail_job(
                    &jobs,
                    &events_tx,
                    &study_id,
                    format!(
                        "step timed out — no message received from dev-bench before the deadline \
                         (waiting on step index {next_expected})"
                    ),
                );
                return;
            }
            Err(e) => {
                fail_job(&jobs, &events_tx, &study_id, format!("dev-bench connection error: {e:?}"));
                return;
            }
        }
    }
}

/// Broadcasts one batch of samples the instant they're decoded off the wire
/// — a no-op (not an error) if `current_stream` is `None`, matching
/// `write_sample`'s own "drop it, log it" posture for a sample with no open
/// `StreamStart`.
fn broadcast_sample_batch(
    events_tx: &broadcast::Sender<StudyEvent>,
    study_id: &str,
    current_stream: Option<(u32, StreamChannel)>,
    samples: &[Sample],
) {
    let Some((step_index, channel)) = current_stream else { return };
    let _ = events_tx.send(StudyEvent::SampleBatch {
        study_id: study_id.to_string(),
        step_index,
        channel,
        samples: samples.to_vec(),
    });
}

/// Decodes one `Sample` onto whichever of `data.csv` (`Power`) or
/// `waveform.csv` (`SensorWaveform`) its currently-open stream belongs to,
/// appending `core_rx_utc_ms` (Core's own receipt-time wall clock, decision
/// 30) as an extra column beyond what `Sample::to_csv_row` already renders.
fn write_sample(results_dir: &FsPath, study: &Study, current_stream: Option<(u32, StreamChannel)>, sample: Sample) {
    let Some((step_index, channel)) = current_stream else {
        tracing::warn!("received a stream sample with no open StreamStart; dropping it");
        return;
    };
    let Some(step) = study.steps.get(step_index as usize) else {
        tracing::warn!("received a stream sample for out-of-range step_index {step_index}; dropping it");
        return;
    };
    let Some(row) = sample.to_csv_row(step.name.as_str()) else {
        tracing::warn!(
            "step name '{}' doesn't fit alongside the rest of a CSV row; dropping this sample",
            step.name
        );
        return;
    };

    let filename = match channel {
        StreamChannel::Power => "data.csv",
        StreamChannel::SensorWaveform => "waveform.csv",
    };
    let path = results_dir.join(filename);
    let is_new = !path.exists();

    match std::fs::OpenOptions::new().create(true).append(true).open(&path) {
        Ok(mut file) => {
            if is_new {
                if let Err(e) = writeln!(file, "{},core_rx_utc_ms", Sample::csv_header()) {
                    tracing::error!("failed to write header to {}: {e:?}", path.display());
                }
            }
            if let Err(e) = writeln!(file, "{row},{}", current_utc_ms()) {
                tracing::error!("failed to append a row to {}: {e:?}", path.display());
            }
        }
        Err(e) => tracing::error!("failed to open {} for append: {e:?}", path.display()),
    }
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
/// Writes to a `.partial` file, only renamed to the real `events.json` on a
/// genuine [`EventsJsonWriter::finish`] — an aborted/failed study leaves the
/// `.partial` file behind as a diagnostic artifact rather than a completed
/// result, the same posture `data.csv`/`waveform.csv` already have for a
/// crash mid-study (`embarch-study-designer/design.md` §3 decision 16).
struct EventsJsonWriter {
    file: std::io::BufWriter<std::fs::File>,
    partial_path: PathBuf,
    final_path: PathBuf,
    wrote_any_step: bool,
}

impl EventsJsonWriter {
    fn start(results_dir: &FsPath, study_name: &str) -> anyhow::Result<Self> {
        use anyhow::Context;

        let partial_path = results_dir.join("events.json.partial");
        let final_path = results_dir.join("events.json");
        let mut file = std::io::BufWriter::new(
            std::fs::File::create(&partial_path)
                .with_context(|| format!("failed to create {}", partial_path.display()))?,
        );
        write!(file, "{{\"study_name\":{},\"steps\":[", serde_json::to_string(study_name)?)
            .with_context(|| format!("failed to write the opening of {}", partial_path.display()))?;
        Ok(Self { file, partial_path, final_path, wrote_any_step: false })
    }

    fn write_step(&mut self, result: &StepResult) -> anyhow::Result<()> {
        if self.wrote_any_step {
            write!(self.file, ",")?;
        }
        serde_json::to_writer(&mut self.file, result)?;
        self.wrote_any_step = true;
        Ok(())
    }

    /// Closes the `steps` array, writes empty `validations` — post-hoc
    /// validation (design.md §3 decision 19, the `core-validation` feature's
    /// `SignalCheck` machinery) is a real feature this crate enables but
    /// doesn't call yet, unchanged from before this streaming rework — and
    /// atomically renames `.partial` to the real `events.json`.
    fn finish(mut self) -> anyhow::Result<()> {
        use anyhow::Context;

        write!(self.file, "],\"validations\":[]}}")?;
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
        | StudyEvent::StatusChanged { study_id, .. } => study_id,
    }
}

// ---- GET /study/{study_id}/power-data, /waveform-data ----------------------

pub async fn power_data_handler(Path(study_id): Path<String>) -> Result<Response, (StatusCode, String)> {
    serve_study_csv(&study_id, "data.csv", "power data").await
}

pub async fn waveform_data_handler(Path(study_id): Path<String>) -> Result<Response, (StatusCode, String)> {
    serve_study_csv(&study_id, "waveform.csv", "waveform data").await
}

async fn serve_study_csv(study_id: &str, filename: &str, kind: &str) -> Result<Response, (StatusCode, String)> {
    let dir = study_results_dir(study_id).map_err(internal_err)?;
    let path = dir.join(filename);

    match tokio::fs::read(&path).await {
        Ok(bytes) => Ok(([(CONTENT_TYPE, "text/csv")], bytes).into_response()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err((
            StatusCode::NOT_FOUND,
            format!("no {kind} captured for this study"),
        )),
        Err(e) => Err(internal_err(e)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use embarch_study_designer::{
        limits::MAX_STEPS_PER_STUDY, Action, BleRole, DataChannel, ExpectedValue, PostHocCheck, PostHocValidation,
        ValidationSource,
    };
    use heapless::Vec as HVec;

    fn step(name: &str, timeout_ms: u32) -> embarch_study_designer::Step {
        embarch_study_designer::Step {
            name: heapless::String::try_from(name).unwrap(),
            action: Action::BleConnect { role: BleRole::Central, target_address: None },
            timeout_ms,
            power_sample: None,
            continue_on_fail: false,
        }
    }

    fn study_with_steps(timeouts: &[u32]) -> Study {
        let mut steps: HVec<embarch_study_designer::Step, MAX_STEPS_PER_STUDY> = HVec::new();
        for (i, t) in timeouts.iter().enumerate() {
            steps.push(step(&format!("step-{i}"), *t)).unwrap();
        }
        let steps_crc_value = steps_crc(&steps).unwrap();
        Study {
            name: heapless::String::try_from("test-study").unwrap(),
            steps,
            validations: HVec::new(),
            steps_crc: steps_crc_value,
        }
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

    #[test]
    fn validate_study_rejects_an_out_of_range_validation_step_index() {
        let mut study = study_with_steps(&[1_000]);
        study
            .validations
            .push(PostHocValidation {
                source: ValidationSource { step_index: 5, channel: DataChannel::CapturedData },
                check: PostHocCheck::Simple(ExpectedValue::InRange { min: 0.0, max: 1.0 }),
            })
            .unwrap();
        let err = validate_study(&study).unwrap_err();
        assert!(err.contains("out of range"), "{err}");
    }

    #[test]
    fn validate_study_accepts_an_in_range_validation_step_index() {
        let mut study = study_with_steps(&[1_000, 2_000]);
        study
            .validations
            .push(PostHocValidation {
                source: ValidationSource { step_index: 1, channel: DataChannel::CapturedData },
                check: PostHocCheck::Simple(ExpectedValue::InRange { min: 0.0, max: 1.0 }),
            })
            .unwrap();
        assert!(validate_study(&study).is_ok());
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
            power_samples_ref: None,
            waveform_ref: None,
            // embarch-study-designer/design.md §3 decisions 31/32 — new
            // fields this test fixture doesn't need to populate.
            gatt_services: None,
            gatt_activity: None,
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

        let mut writer = EventsJsonWriter::start(&dir, "test-study").unwrap();
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
        assert!(result["validations"].as_array().unwrap().is_empty());

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
    fn events_json_writer_leaves_only_the_partial_file_until_finished() {
        let dir = std::env::temp_dir().join(format!("embarch-study-writer-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let mut writer = EventsJsonWriter::start(&dir, "s").unwrap();
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

    #[test]
    fn write_sample_appends_core_rx_utc_ms_and_writes_the_header_once() {
        let dir = std::env::temp_dir().join(format!("embarch-study-csv-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let study = study_with_steps(&[1_000]);
        let sample = Sample {
            rx_utc_ms: 1_753_000_000_123,
            value: 3.3,
            unit: embarch_study_designer::Unit::Volts,
            channel_id: 0,
        };

        write_sample(&dir, &study, Some((0, StreamChannel::Power)), sample);
        write_sample(&dir, &study, Some((0, StreamChannel::Power)), sample);

        let contents = std::fs::read_to_string(dir.join("data.csv")).unwrap();
        let mut lines = contents.lines();
        assert_eq!(lines.next().unwrap(), "rx_utc_ms,step_name,value,unit,channel_id,core_rx_utc_ms");
        let first_row = lines.next().unwrap();
        assert!(first_row.starts_with("1753000000123,step-0,3.3,volts,0,"));
        assert_eq!(lines.clone().count(), 1); // exactly one more data row

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn write_sample_with_no_open_stream_is_dropped_not_panicked() {
        let dir = std::env::temp_dir().join(format!("embarch-study-csv-test-nostream-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let study = study_with_steps(&[1_000]);
        let sample = Sample { rx_utc_ms: 1, value: 1.0, unit: embarch_study_designer::Unit::Raw, channel_id: 0 };

        write_sample(&dir, &study, None, sample);

        assert!(!dir.join("data.csv").exists());
        std::fs::remove_dir_all(&dir).ok();
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
}
