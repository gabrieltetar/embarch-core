use axum::{
    extract::{FromRequest, Json, Multipart, Query, Request, State},
    http::{header::CONTENT_TYPE, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{delete, get, post},
    Router,
};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Arc, Mutex as StdMutex};
use tokio::sync::Mutex;

use crate::{chip_resolve, hardware, logs, serial, study};

/// Shared state for every handler. `hw_lock` serializes access to the
/// physical probe/serial connections so a CLI call and a Claude Code call
/// can't collide on the same USB device at the same time.
///
/// `study_lock`/`study_jobs` are `/study*`'s own state (`study.rs`).
/// `study_lock` is explicitly separate from `hw_lock` — a different physical
/// connection (decision 15) — so an in-flight
/// study and a `/flash`/`/reset` call never contend on the same guard.
#[derive(Clone)]
pub struct AppState {
    pub token: String,
    pub hw_lock: Arc<Mutex<()>>,
    /// The route currently holding `hw_lock`, or `None` when it's free.
    /// Kept in a plain `std::sync::Mutex` rather than folded into `hw_lock`
    /// itself (e.g. `Arc<Mutex<Option<String>>>`) because a contending
    /// caller must be able to read *who* holds the lock while that same
    /// lock is held — reading it out of the guarded value would need the
    /// guard. `acquire_hw_lock` below is the only way this is set or
    /// cleared (decision 14, `tasks/core/013`).
    pub hw_holder: Arc<StdMutex<Option<String>>>,
    pub study_lock: study::StudyLock,
    pub study_jobs: study::JobRegistry,
    /// Live push for `GET /study/{study_id}/events` (SSE) — every
    /// `StudyEvent` `study.rs` produces goes through this one process-wide
    /// channel, same "only one study in flight" assumption `study_lock`
    /// already makes. Capacity is a small backlog, not a full history — a
    /// subscriber that falls behind gets an explicit `lagged` notice
    /// (`study::study_events_handler`) rather than silently missing events.
    pub study_events: tokio::sync::broadcast::Sender<study::StudyEvent>,
    /// The DUT's `outpost-manifest.json`, as the flash that put that image on
    /// the board delivered it (`embarch-outpost` decision 9,
    /// decision 30(c)). Empty until a `POST /flash` carries one.
    pub outpost_manifest: crate::outpost_manifest::ManifestSlot,
}

impl AppState {
    /// Constructs the `/study*`-only fields fresh — kept here so
    /// `main.rs`'s `serve` doesn't need to know any of their internals.
    pub fn new(token: String) -> Self {
        let (study_events, _rx) = tokio::sync::broadcast::channel(256);
        Self {
            token,
            outpost_manifest: crate::outpost_manifest::ManifestSlot::new(),
            hw_lock: Arc::new(Mutex::new(())),
            hw_holder: Arc::new(StdMutex::new(None)),
            study_lock: Arc::new(StdMutex::new(None)),
            study_jobs: Arc::new(StdMutex::new(HashMap::new())),
            study_events,
        }
    }
}

/// How long a caller waits for a contended `hw_lock` before being refused
/// with `503` naming the holder, rather than queueing silently and
/// indefinitely (decision 14, `tasks/core/013`). Short enough that the
/// common case — a flash or reset in the low hundreds of ms — only ever
/// costs a contending caller a brief wait, not this whole timeout.
const HW_LOCK_WAIT_MS: u64 = 500;

/// A held `hw_lock`, tagged with the route that took it. Clears
/// [`AppState::hw_holder`] on drop so the next contender (or the next
/// `acquire_hw_lock` call) sees the lock as free again — this is the only
/// place that happens, so a holder string can never outlive the guard that
/// set it.
#[derive(Debug)]
pub(crate) struct HwGuard {
    _permit: tokio::sync::OwnedMutexGuard<()>,
    holder: Arc<StdMutex<Option<String>>>,
}

impl Drop for HwGuard {
    fn drop(&mut self) {
        *self.holder.lock().unwrap() = None;
    }
}

/// Takes `hw_lock` for `route`, waiting up to [`HW_LOCK_WAIT_MS`] for a
/// concurrent holder to release it. Past that, refuses with `503` naming
/// the holder instead of the old behaviour — an unbounded silent wait on
/// the mutex, indistinguishable from Core being unresponsive, which is
/// exactly what decision 14 says a `503` exists to avoid (`tasks/core/013`).
///
/// Logs on both paths, at different levels, so a wait and a refusal are
/// distinguishable in `core.log` after the fact rather than both looking
/// like "the handler took a while."
pub(crate) async fn acquire_hw_lock(state: &AppState, route: &'static str) -> Result<HwGuard, (StatusCode, String)> {
    let lock = state.hw_lock.clone();
    match tokio::time::timeout(std::time::Duration::from_millis(HW_LOCK_WAIT_MS), lock.lock_owned()).await {
        Ok(permit) => {
            *state.hw_holder.lock().unwrap() = Some(route.to_string());
            tracing::info!("{route} took hw_lock");
            Ok(HwGuard {
                _permit: permit,
                holder: state.hw_holder.clone(),
            })
        }
        Err(_) => {
            let holder = state
                .hw_holder
                .lock()
                .unwrap()
                .clone()
                .unwrap_or_else(|| "unknown".to_string());
            let msg = format!("hw_lock held by {holder}; {route} refused after waiting {HW_LOCK_WAIT_MS}ms");
            tracing::warn!("{msg}");
            Err((StatusCode::SERVICE_UNAVAILABLE, msg))
        }
    }
}

/// Every route requires the bearer token — there is no unauthenticated
/// route left (there used to be exactly one, `GET /enroll`'s static HTML/JS
/// page; retired 2026-08-24 in favor of `embarch-ui`'s Enroll tab,
/// `embarch-ui` decision 1 — `POST /probes/enroll`
/// itself is unaffected and still lives below).
pub fn build_router(state: AppState) -> Router {
    Router::new()
        .route("/status", get(status_handler))
        .route("/flash", post(flash_handler))
        .route("/reset", post(reset_handler))
        .route("/serial-log", get(serial_log_handler))
        .route("/dev-bench/port", get(dev_bench_port_handler))
        .route("/dev-bench/hello", get(study::hello_handler))
        .route("/resolve-chip", post(resolve_chip_handler))
        .route("/probes/enroll", post(enroll_probe_handler))
        .route("/probes/enrolled", get(list_enrolled_probes_handler))
        .route("/dev-bench/link", post(set_dev_bench_link_handler))
        .route("/signals", post(declare_signal_handler).get(list_signals_handler))
        .route("/signals/{name}", delete(remove_signal_handler))
        .route("/serial-ports", get(serial_ports_handler))
        .route("/validate", post(validate_handler))
        .route("/alerts", get(alerts_handler))
        .route("/logs/recent", get(logs_recent_handler))
        .route("/study", post(study::post_study_handler))
        .route("/study/{study_id}", get(study::get_study_handler))
        .route("/study/{study_id}/events", get(study::study_events_handler))
        .route("/study/{study_id}/steps", get(study::study_steps_handler))
        .route("/study/{study_id}/streams", get(study::stream_index_handler))
        .route("/study/{study_id}/stream/{name}", get(study::stream_data_handler))
        .layer(middleware::from_fn_with_state(state.clone(), auth_middleware))
        .with_state(state)
}

/// Simple bearer-token check. This is deliberately not OAuth or anything
/// fancy — Core may end up reachable over a real network (WSL-to-Windows,
/// or a LAN if Core moves to a Pi), so "open to whoever can see the port"
/// isn't good enough even at single-engineer scale.
async fn auth_middleware(
    State(state): State<AppState>,
    req: Request,
    next: Next,
) -> Result<Response, StatusCode> {
    let expected = format!("Bearer {}", state.token);
    let ok = req
        .headers()
        .get("authorization")
        .and_then(|h| h.to_str().ok())
        .map(|h| h == expected)
        .unwrap_or(false);

    if ok {
        Ok(next.run(req).await)
    } else {
        Err(StatusCode::UNAUTHORIZED)
    }
}

pub(crate) fn internal_err<E: std::fmt::Debug>(e: E) -> (StatusCode, String) {
    let msg = format!("{e:?}");
    tracing::error!("{msg}");
    (StatusCode::INTERNAL_SERVER_ERROR, msg)
}

/// The same `not_attached`-versus-`mismatch` split `POST /validate` renders
/// as JSON (`ValidateMismatchResponse`, decision 59), for the three call
/// sites that only ever surface plain text: `flash`/`reset` (below) and
/// `study.rs`'s dev-bench handshake gate, which all run the exact same
/// `embarch_topology::hardware::validate_serial`/`validate_role` check
/// mid-attach and, before this, rendered both conditions under the same
/// `topology-mismatch: ...` lead via `{e:?}`'s debug chain. Returns the text
/// unchanged (via `internal_err`'s own `{e:?}` formatting) for any other
/// error — only a genuine `TopologyMismatch` gets a distinguishing lead.
pub(crate) fn describe_topology_error(e: anyhow::Error) -> (StatusCode, String) {
    match e.downcast_ref::<embarch_topology::hardware::TopologyMismatch>() {
        Some(m) if m.live_hardware_id.is_none() => (
            StatusCode::SERVICE_UNAVAILABLE,
            format!(
                "probe not attached for role '{}' (probe {}, chip '{}'): {}",
                m.role, m.probe_serial, m.chip, m.reason
            ),
        ),
        Some(m) => (
            StatusCode::CONFLICT,
            format!(
                "topology mismatch for role '{}' (probe {}, chip '{}'): {} — fix it at {}",
                m.role, m.probe_serial, m.chip, m.reason, m.fix_it_url
            ),
        ),
        None => internal_err(e),
    }
}

// ---- GET /status --------------------------------------------------------

#[derive(Serialize)]
struct StatusResponse {
    status: &'static str,
    probes: Vec<hardware::ProbeInfo>,
    /// The `embarch-study-designer` **host type** schema version this Core
    /// was built against (`embarch-study-designer` decision 12
    /// and its 2026-08-25 amendment). `embarch-api` compares it against its
    /// own compiled-in copy before submitting a `Study`, since `GET /status`
    /// is already that hop's connection-establishment check and there is no
    /// separate handshake call.
    ///
    /// The **host** constant specifically, not the dev-bench wire one: this
    /// hop carries `Study`/`StudyResult` *whole*, including the parts
    /// dev-bench never sees (`validations`, `requires`, `gatt`). Serving the
    /// wire number here would let a host-side-only reshape drift these two
    /// processes undetected — which is exactly the failure the split was
    /// made to prevent.
    study_designer_schema_version: u32,
    /// This Core binary's own crate version — `CARGO_PKG_VERSION`, the same
    /// string `embarch-core --version` prints, resolved at compile time
    /// (decision 13, amended 2026-09-03).
    ///
    /// **Mechanical on purpose.** The version was reachable only by running
    /// the binary, so nothing talking to Core *over HTTP* could say which
    /// build answered — and a deploy that silently did not land looks
    /// exactly like one that did (`embarch-dev-workflow.md` §4a). Reading it
    /// off `env!` rather than a hand-maintained constant is the whole point:
    /// `Cargo.toml`'s version already tracks the release tags, so this field
    /// cannot drift from the build that serves it.
    ///
    /// A consumer should **warn, not refuse**, on a difference from whatever
    /// Core version it was built or tested against — decision 13's posture on
    /// skew, unchanged. There is deliberately no separate hand-bumped
    /// `contract_version` beside this; decision 13's amendment has why.
    core_version: &'static str,
}

async fn status_handler() -> Result<Json<StatusResponse>, (StatusCode, String)> {
    let probes = tokio::task::spawn_blocking(hardware::list_probes)
        .await
        .map_err(internal_err)?
        .map_err(internal_err)?;

    Ok(Json(StatusResponse {
        status: "ok",
        probes,
        study_designer_schema_version: embarch_study_designer::HOST_TYPE_SCHEMA_VERSION,
        core_version: env!("CARGO_PKG_VERSION"),
    }))
}

// ---- POST /flash ---------------------------------------------------------

#[derive(Deserialize)]
struct FlashRequest {
    chip: String,
    firmware_path: String,
    #[serde(default = "default_format")]
    format: String,
    /// Only meaningful for `format = "bin"` (`hardware::flash`'s own doc
    /// comment). Hex (`"0x2000"`) or plain decimal — parsed the same way in
    /// both the JSON and multipart bodies, `parse_base_address` below.
    #[serde(default)]
    base_address: Option<String>,
    /// Disambiguates which attached debug probe to use when more than one
    /// is (decision 9, `hardware::open_probe`) — matched
    /// against `ProbeInfo.serial_number`. Omitted behaves as before when
    /// exactly one probe is attached; more than one with this omitted is
    /// now a named `500` rather than a silent, possibly-wrong pick.
    #[serde(default)]
    probe_serial: Option<String>,
    /// Full chip erase before writing, rather than erasing only the sectors
    /// the image covers (`hardware::flash`'s own doc comment has why that
    /// distinction matters). The equivalent of `west flash --erase`.
    /// Defaults to `false` — the previous behavior, so an existing caller
    /// that omits it is unaffected.
    #[serde(default)]
    erase: bool,
    /// The `outpost-manifest.json` this build produced, as a path *this
    /// process* can open — the JSON-body sibling of the multipart `manifest`
    /// part, exactly as `firmware_path` is `firmware`'s.
    ///
    /// On the same call as the artifact rather than on a `POST /manifests` of
    /// its own (decision 30(c), Settlement 1): the manifest and
    /// the image it describes then arrive in **one operation**, which is what
    /// makes "the study's own flash binds it" hold with no "which manifest is
    /// current" record to go stale.
    #[serde(default)]
    manifest_path: Option<String>,
}

fn default_format() -> String {
    "elf".to_string()
}

/// Parses a caller-supplied base address, hex (`0x`-prefixed) or decimal.
fn parse_base_address(s: &str) -> Result<u64, (StatusCode, String)> {
    let trimmed = s.trim();
    let parsed = trimmed
        .strip_prefix("0x")
        .or_else(|| trimmed.strip_prefix("0X"))
        .map(|hex| u64::from_str_radix(hex, 16))
        .unwrap_or_else(|| trimmed.parse::<u64>());
    parsed.map_err(|_| {
        (
            StatusCode::BAD_REQUEST,
            format!("invalid base_address '{s}' (expected hex like '0x2000' or a decimal integer)"),
        )
    })
}

#[derive(Serialize)]
struct FlashResponse {
    flashed: bool,
    chip: String,
}

/// A path Core can open directly, or a temp file holding an uploaded
/// artifact's bytes — kept alive (not dropped, which deletes it) until the
/// blocking flash call below has actually read it.
#[derive(Debug)]
struct FlashArgs {
    chip: String,
    path: PathBuf,
    format: String,
    base_address: Option<u64>,
    probe_serial: Option<String>,
    erase: bool,
    /// The manifest's bytes, when this flash carried one. `None` means this
    /// flash carried none, which **clears** whatever that chip had — see
    /// `ManifestSlot::clear_for_chip`.
    manifest_json: Option<String>,
    _uploaded: Option<tempfile::NamedTempFile>,
}

/// `/flash` accepts a JSON body (`firmware_path` — a path *this process*
/// can open directly, the same-machine assumption) or a
/// `multipart/form-data` body carrying the artifact's bytes (decision 10;
/// `embarch-api` decision 15's 2026-08-18 finding is what
/// actually gave this a caller — a `WslHost` Core running as an installed
/// Windows service has no access to the WSL2-side `\\wsl.localhost` share
/// at all, so `embarch-api` now uploads bytes for that case instead of
/// sending a path). Branches on `Content-Type` rather than two separate
/// routes, matching the one-`/flash`-endpoint contract already documented
/// in §4's endpoint table.
async fn flash_handler(
    State(state): State<AppState>,
    request: Request,
) -> Result<Json<FlashResponse>, (StatusCode, String)> {
    let is_multipart = request
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|ct| ct.starts_with("multipart/form-data"))
        .unwrap_or(false);

    let args = if is_multipart {
        let multipart = Multipart::from_request(request, &state)
            .await
            .map_err(|e| (StatusCode::BAD_REQUEST, format!("invalid multipart body: {e}")))?;
        flash_args_from_multipart(multipart).await?
    } else {
        let Json(req) = Json::<FlashRequest>::from_request(request, &state)
            .await
            .map_err(|e| (StatusCode::BAD_REQUEST, format!("invalid JSON body: {e}")))?;
        let base_address = req.base_address.as_deref().map(parse_base_address).transpose()?;
        let manifest_json = match req.manifest_path.as_deref() {
            Some(path) => Some(std::fs::read_to_string(path).map_err(|e| {
                (
                    StatusCode::BAD_REQUEST,
                    format!("failed to read manifest_path '{path}': {e}"),
                )
            })?),
            None => None,
        };
        FlashArgs {
            chip: req.chip,
            path: PathBuf::from(req.firmware_path),
            format: req.format,
            base_address,
            probe_serial: req.probe_serial,
            erase: req.erase,
            manifest_json,
            _uploaded: None,
        }
    };

    let _guard = acquire_hw_lock(&state, "POST /flash").await?;

    let chip_for_response = args.chip.clone();
    let FlashArgs {
        chip,
        path,
        format,
        base_address,
        probe_serial,
        erase,
        manifest_json,
        _uploaded,
    } = args;

    // Parsed *before* the flash, so a build-tooling problem is reported while
    // the person who ran the build is still watching rather than at render
    // time, hours later, as an unnamed trace.
    let parsed_manifest = match manifest_json.as_deref() {
        Some(json) => Some(
            crate::outpost_manifest::parse(json)
                .map_err(|e| (StatusCode::BAD_REQUEST, e))?,
        ),
        None => None,
    };

    tokio::task::spawn_blocking(move || {
        let result =
            hardware::flash(&chip, &path, &format, base_address, probe_serial.as_deref(), erase);
        drop(_uploaded); // outlives the flash call; dropped (deleted) here, not before
        result
    })
    .await
    .map_err(internal_err)?
    .map_err(describe_topology_error)?;

    // Only after the flash actually succeeded: a manifest bound to an image
    // that never reached the board would describe firmware that is not running.
    match (manifest_json, parsed_manifest) {
        (Some(json), Some(manifest)) => {
            state.outpost_manifest.store(&chip_for_response, json, manifest)
        }
        // A flash carrying no manifest replaced whatever image the stored one
        // described, so the stored one no longer describes anything on that
        // chip. Keeping it would leave Core holding a plausible, wrong answer.
        _ => state.outpost_manifest.clear_for_chip(&chip_for_response),
    }

    Ok(Json(FlashResponse {
        flashed: true,
        chip: chip_for_response,
    }))
}

fn bad_multipart_field<E: std::fmt::Display>(e: E) -> (StatusCode, String) {
    (StatusCode::BAD_REQUEST, format!("invalid multipart field: {e}"))
}

/// Fields: `chip` (required), `format` (optional, same default as the JSON
/// body), `base_address` (optional, same hex-or-decimal parsing as the JSON
/// body — only meaningful for `format = "bin"`), `probe_serial` (optional,
/// same as the JSON body), and a `firmware` file part (required) — the
/// artifact's raw bytes, written to a temp file since `hardware::flash`
/// reads from a path. An optional `manifest` text part carries the build's
/// `outpost-manifest.json` (decision 30(c)) — the multipart
/// sibling of the JSON body's `manifest_path`.
async fn flash_args_from_multipart(mut multipart: Multipart) -> Result<FlashArgs, (StatusCode, String)> {
    let mut chip: Option<String> = None;
    let mut format: Option<String> = None;
    let mut base_address_raw: Option<String> = None;
    let mut probe_serial: Option<String> = None;
    let mut erase_raw: Option<String> = None;
    let mut manifest_json: Option<String> = None;
    let mut uploaded: Option<tempfile::NamedTempFile> = None;

    while let Some(field) = multipart.next_field().await.map_err(bad_multipart_field)? {
        match field.name() {
            Some("chip") => chip = Some(field.text().await.map_err(bad_multipart_field)?),
            Some("format") => format = Some(field.text().await.map_err(bad_multipart_field)?),
            Some("base_address") => base_address_raw = Some(field.text().await.map_err(bad_multipart_field)?),
            Some("probe_serial") => probe_serial = Some(field.text().await.map_err(bad_multipart_field)?),
            Some("erase") => erase_raw = Some(field.text().await.map_err(bad_multipart_field)?),
            // The manifest rides the same request as the artifact it
            // describes (decision 30(c)), so there is no interval
            // in which Core holds one without the other.
            Some("manifest") => manifest_json = Some(field.text().await.map_err(bad_multipart_field)?),
            Some("firmware") => {
                let bytes = field.bytes().await.map_err(bad_multipart_field)?;
                let mut temp = tempfile::Builder::new()
                    .prefix("embarch-core-flash-")
                    .tempfile()
                    .map_err(|e| {
                        (StatusCode::INTERNAL_SERVER_ERROR, format!("failed to create a temp file for the uploaded firmware: {e}"))
                    })?;
                temp.write_all(&bytes).map_err(|e| {
                    (StatusCode::INTERNAL_SERVER_ERROR, format!("failed to write the uploaded firmware to a temp file: {e}"))
                })?;
                uploaded = Some(temp);
            }
            _ => {} // ignore unrecognized fields rather than erroring
        }
    }

    let chip = chip.ok_or_else(|| (StatusCode::BAD_REQUEST, "multipart body missing 'chip' field".to_string()))?;
    let format = format.unwrap_or_else(default_format);
    let base_address = base_address_raw.as_deref().map(parse_base_address).transpose()?;
    let uploaded = uploaded
        .ok_or_else(|| (StatusCode::BAD_REQUEST, "multipart body missing 'firmware' file part".to_string()))?;
    let path = uploaded.path().to_path_buf();

    Ok(FlashArgs {
        chip,
        path,
        format,
        base_address,
        probe_serial,
        manifest_json,
        // Accepts the spellings a form actually carries a boolean as; anything
        // else is a caller error rather than a silent `false`, since silently
        // *not* erasing is exactly the surprise this field exists to remove.
        erase: match erase_raw.as_deref().map(str::trim) {
            None | Some("") => false,
            Some("true") | Some("1") | Some("yes") => true,
            Some("false") | Some("0") | Some("no") => false,
            Some(other) => {
                return Err((
                    StatusCode::BAD_REQUEST,
                    format!("invalid erase '{other}' (expected true/false)"),
                ))
            }
        },
        _uploaded: Some(uploaded),
    })
}

// ---- POST /reset ----------------------------------------------------------

#[derive(Deserialize)]
struct ResetRequest {
    chip: String,
    /// Same disambiguation as `FlashRequest::probe_serial` above.
    #[serde(default)]
    probe_serial: Option<String>,
}

#[derive(Serialize)]
struct ResetResponse {
    reset: bool,
}

async fn reset_handler(
    State(state): State<AppState>,
    Json(req): Json<ResetRequest>,
) -> Result<Json<ResetResponse>, (StatusCode, String)> {
    let _guard = acquire_hw_lock(&state, "POST /reset").await?;
    let chip = req.chip;
    let probe_serial = req.probe_serial;

    tokio::task::spawn_blocking(move || hardware::reset(&chip, probe_serial.as_deref()))
        .await
        .map_err(internal_err)?
        .map_err(describe_topology_error)?;

    Ok(Json(ResetResponse { reset: true }))
}

// ---- GET /serial-log --------------------------------------------------------

#[derive(Deserialize)]
struct SerialLogQuery {
    port: String,
    #[serde(default = "default_baud")]
    baud: u32,
    #[serde(default = "default_duration_ms")]
    duration_ms: u64,
}

fn default_baud() -> u32 {
    115_200
}

fn default_duration_ms() -> u64 {
    2000
}

#[derive(Debug, Serialize)]
struct SerialLogResponse {
    port: String,
    lines: Vec<String>,
    /// `true` when the capture hit `serial::serial_log_max_bytes()` before
    /// `duration_ms` (or the source) ran out — the response is a genuine
    /// prefix, not the whole capture.
    truncated: bool,
}

async fn serial_log_handler(
    State(state): State<AppState>,
    Query(q): Query<SerialLogQuery>,
) -> Result<Json<SerialLogResponse>, (StatusCode, String)> {
    if q.duration_ms > serial::MAX_DURATION_MS {
        return Err((
            StatusCode::BAD_REQUEST,
            format!(
                "duration_ms={} exceeds the cap of {} ms",
                q.duration_ms,
                serial::MAX_DURATION_MS
            ),
        ));
    }

    let _guard = acquire_hw_lock(&state, "GET /serial-log").await?;

    let port = q.port.clone();
    let baud = q.baud;
    let duration_ms = q.duration_ms;
    let max_bytes = serial::serial_log_max_bytes();

    let result = tokio::task::spawn_blocking(move || serial::read_log(&port, baud, duration_ms, max_bytes))
        .await
        .map_err(internal_err)?
        .map_err(internal_err)?;

    Ok(Json(SerialLogResponse {
        port: q.port,
        lines: result.lines,
        truncated: result.truncated,
    }))
}

// ---- POST /resolve-chip ----------------------------------------------------

/// Zephyr SoC name → probe-rs chip target string (`chip_resolve.rs`,
/// decision 8). Pure lookup against probe-rs's own target
/// registry — no hardware touched, so this takes no `hw_lock`, same posture
/// as `/status`'s probe listing and `/dev-bench/port`.
#[derive(Deserialize)]
struct ResolveChipRequest {
    soc: String,
}

#[derive(Serialize)]
struct ResolveChipResponse {
    chip: String,
}

async fn resolve_chip_handler(
    Json(req): Json<ResolveChipRequest>,
) -> Result<Json<ResolveChipResponse>, (StatusCode, String)> {
    let soc = req.soc.clone();
    let result = tokio::task::spawn_blocking(move || chip_resolve::resolve(&soc))
        .await
        .map_err(internal_err)?;

    match result {
        Ok(chip) => Ok(Json(ResolveChipResponse { chip })),
        Err(e) => {
            let msg = e.to_string();
            tracing::info!("{msg}");
            Err((StatusCode::NOT_FOUND, msg))
        }
    }
}

// ---- POST /probes/enroll ---------------------------------------------------

/// The only sanctioned way to populate/update `embarch-topology`'s
/// enrollment storage (decision 22;
/// `embarch_topology::hardware::enroll`, formerly this crate's own
/// `board_gate::enroll`). Takes `hw_lock` like `/flash`/`/reset` — it
/// attaches to a real chip over the same physical connection those do, and
/// shouldn't be allowed to race either of them.
#[derive(Deserialize)]
struct EnrollProbeRequest {
    role: String,
    chip: String,
    /// Picks which currently-attached probe to enroll when more than one
    /// is present — `/enroll`'s own drag-and-drop UI always sends this
    /// (§3 decision 15), since it lets a human enroll two visibly-
    /// different boards without unplugging either. Omitted, `enroll`
    /// falls back to its original "exactly one attached" requirement.
    #[serde(default)]
    probe_serial: Option<String>,
}

#[derive(Serialize)]
struct EnrollProbeResponse {
    probe_serial: String,
    role: String,
    chip: String,
    /// The probe-read (JTAG) hardware ID, not the bench's self-reported one —
    /// `hardware_id`, unprefixed, is this suite's name for the probe/JTAG-read
    /// identity everywhere except `GET /dev-bench/hello` (decision 56).
    hardware_id: String,
    confirmed_at_utc_ms: u64,
}

async fn enroll_probe_handler(
    State(state): State<AppState>,
    Json(req): Json<EnrollProbeRequest>,
) -> Result<Json<EnrollProbeResponse>, (StatusCode, String)> {
    let _guard = acquire_hw_lock(&state, "POST /probes/enroll").await?;

    let role = req.role;
    let chip = req.chip;
    let probe_serial = req.probe_serial;

    let board = tokio::task::spawn_blocking(move || {
        embarch_topology::hardware::enroll(&role, &chip, probe_serial.as_deref())
    })
        .await
        .map_err(internal_err)?
        .map_err(internal_err)?;

    Ok(Json(EnrollProbeResponse {
        probe_serial: board.probe_serial,
        role: board.role,
        chip: board.chip,
        hardware_id: board.hardware_id,
        confirmed_at_utc_ms: board.confirmed_at_utc_ms,
    }))
}

// ---- GET /probes/enrolled ---------------------------------------------------

/// Every currently-enrolled board — pure read of `embarch-topology`'s own
/// storage, no hardware touched, no `hw_lock` needed (same posture as
/// `/dev-bench/port`'s enumeration below). Added alongside the `/enroll`
/// static UI page so it has something to show without a human needing to
/// run `embarch-topology list` in a separate terminal.
async fn list_enrolled_probes_handler() -> Result<Json<Vec<embarch_topology::hardware::EnrolledBoard>>, (StatusCode, String)> {
    tokio::task::spawn_blocking(embarch_topology::hardware::list_enrolled)
        .await
        .map_err(internal_err)?
        .map_err(internal_err)
        .map(Json)
}

// ---- POST /dev-bench/link ---------------------------------------------------

/// Declares dev-bench's runtime-link USB serial
/// (`embarch_topology::hardware::set_dev_bench_link_port_serial`) — a second
/// fact from its JTAG probe's own serial, needed once dev-bench's link and
/// its JTAG probe are different physical USB devices (a Silabs UART bridge
/// vs. a SEGGER probe, `embarch-topology`'s `port.rs` doc
/// comment). No probe-rs attach happens here — it's a plain enrollment-file
/// write, same class of operation as `/probes/enroll`, so it takes the same
/// `hw_lock` to avoid racing it rather than because it touches hardware
/// itself. dev-bench must already be enrolled via `/probes/enroll` first —
/// this only ever amends that existing row.
/// `interface` answers a question `serial` structurally cannot: a debug
/// probe exposing two VCOM ports gives **both** of them the same USB serial,
/// so neither this endpoint's `serial` nor the enrolled probe's own can
/// narrow that pair down to a port. The nRF54L15DK is that case — its
/// `zephyr,console` is `uart20`, wired to VCOM1 at interface 2, while
/// detection's undeclared fallback guesses the lowest interface and lands on
/// a port that accepts bytes and never answers. Either field may be sent
/// alone; sending neither is a 400 rather than a silent no-op.
#[derive(Deserialize)]
struct SetDevBenchLinkRequest {
    #[serde(default)]
    serial: Option<String>,
    #[serde(default)]
    interface: Option<u8>,
}

async fn set_dev_bench_link_handler(
    State(state): State<AppState>,
    Json(req): Json<SetDevBenchLinkRequest>,
) -> Result<StatusCode, (StatusCode, String)> {
    let _guard = acquire_hw_lock(&state, "POST /dev-bench/link").await?;
    let SetDevBenchLinkRequest { serial, interface } = req;

    if serial.is_none() && interface.is_none() {
        return Err((
            StatusCode::BAD_REQUEST,
            "POST /dev-bench/link needs at least one of 'serial' or 'interface'".to_string(),
        ));
    }

    tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
        if let Some(serial) = serial {
            embarch_topology::hardware::set_dev_bench_link_port_serial(&serial)?;
        }
        if let Some(interface) = interface {
            embarch_topology::hardware::set_dev_bench_link_port_interface(interface)?;
        }
        Ok(())
    })
    .await
    .map_err(internal_err)?
    .map_err(internal_err)?;

    Ok(StatusCode::NO_CONTENT)
}

// ---- POST /signals, GET /signals --------------------------------------------

/// Declares (or re-declares) where a named DUT signal currently goes —
/// `embarch_topology::hardware::declare_signal`, that crate's
/// decision 18 and its 2026-08-25 amendment.
///
/// Same shape and same posture as `POST /dev-bench/link` above, for the same
/// reasons: it is a plain enrollment-file write rather than a hardware
/// operation, and it takes `hw_lock` to avoid racing `/probes/enroll` and
/// `/dev-bench/link` on the same file — not because it touches a probe.
/// Idempotent by name (`declare_signal` overwrites an existing row), and
/// that overwrite *is* the migration path the decision promises: moving the
/// outpost from a `Direct` route onto dev-bench pins is one call.
///
/// **Core owns this write, and there is deliberately no `embarch-topology`
/// CLI mirror**, unlike decision 17's `set-dev-bench-link`. That subcommand
/// writes `enrollment.toml` directly and a plain-user run hits the NTFS
/// permission wall on this suite's real primary deployment — which is why
/// the endpoint has to exist at all. A second writer that does not work
/// where the suite actually runs is a surface to keep in step for no one.
/// The cost is stated rather than hidden: a bench with no Core running has
/// no terminal path to declare a signal.
async fn declare_signal_handler(
    State(state): State<AppState>,
    Json(link): Json<embarch_topology::hardware::SignalLink>,
) -> Result<StatusCode, (StatusCode, String)> {
    let _guard = acquire_hw_lock(&state, "POST /signals").await?;

    tokio::task::spawn_blocking(move || embarch_topology::hardware::declare_signal(link))
        .await
        .map_err(internal_err)?
        // A blank name is `declare_signal`'s own rejection, and it is a
        // caller error rather than a Core failure — the same distinction
        // `/dev-bench/port` draws between "not plugged in" and "detection
        // broke".
        .map_err(|e| (StatusCode::BAD_REQUEST, format!("{e:?}")))?;

    Ok(StatusCode::NO_CONTENT)
}

/// Every declared signal link — pure read of `embarch-topology`'s own
/// storage, no hardware touched, no `hw_lock` (same posture as
/// `/probes/enrolled`).
///
/// Added alongside the write because `list_signals` has never had an HTTP
/// caller at all and `embarch-ui`'s Topology tab needs to list rows
/// (`embarch-ui` decision 10, routing half).
async fn list_signals_handler(
) -> Result<Json<Vec<embarch_topology::hardware::SignalLink>>, (StatusCode, String)> {
    tokio::task::spawn_blocking(embarch_topology::hardware::list_signals)
        .await
        .map_err(internal_err)?
        .map_err(internal_err)
        .map(Json)
}

/// Un-declares a signal — `embarch_topology::hardware::remove_signal`.
///
/// Added 2026-08-26 with `embarch-ui`'s signal-route rows
/// (`embarch-ui` decision 10, routing half). Not in that decision's original
/// endpoint pair, and the reason it has to be here is the decision's own
/// consequence: **this tab is the only human surface there is**, and
/// `declare_signal` is idempotent by name, so without a removal the one
/// surface that can state a wire cannot retract one. A signal declared
/// against a bridge that was never bought would otherwise be permanent, and
/// a `Study` naming it would keep passing `POST /study`'s pre-flight while
/// resolving to a port that does not exist.
///
/// `404` when nothing was declared under that name — the same distinction
/// `remove_signal`'s own `Ok(false)` draws, surfaced rather than flattened
/// into a silent success, so a UI that thought a row existed learns it did
/// not. Takes `hw_lock` for the same reason the write above does: it edits
/// the same enrollment file.
async fn remove_signal_handler(
    State(state): State<AppState>,
    axum::extract::Path(name): axum::extract::Path<String>,
) -> Result<StatusCode, (StatusCode, String)> {
    let _guard = acquire_hw_lock(&state, "DELETE /signals/{name}").await?;

    let removed = tokio::task::spawn_blocking(move || embarch_topology::hardware::remove_signal(&name))
        .await
        .map_err(internal_err)?
        .map_err(internal_err)?;

    if removed {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err((StatusCode::NOT_FOUND, "no signal is declared under that name".to_string()))
    }
}

// ---- GET /serial-ports ------------------------------------------------------

/// Every USB serial port this machine currently enumerates, unnarrowed —
/// `embarch_topology::hardware::list_serial_ports`.
///
/// Exists for one caller: declaring a `Route::Direct` signal needs a
/// `port_serial`, and `embarch-ui` decision 10 (routing half) says that pick
/// comes from **Core's own enumeration** rather than being typed from memory.
/// It has to be Core's, not the asking process's: a serial port on the
/// machine running the UI is not a serial port on the machine running Core,
/// which is the entire reason `embarch-ui` links no hardware crate
/// (decision 5).
///
/// **Not `/dev-bench/port` with the filter off.** That endpoint answers
/// "which port is dev-bench's link" and applies the VID gate to do it; a
/// `Direct` route's USB-UART bridge is a wire's carrier and can carry any
/// VID, so gating this list would hide the port it exists to name (see
/// `embarch_topology::hardware::list_serial_ports`).
///
/// Takes no `hw_lock` and opens nothing: this reads USB descriptors the OS
/// already enumerated, same posture as `/status`'s probe listing and
/// `/dev-bench/port`. An empty list is a `200` — nothing plugged in is a real
/// answer, not a failure.
async fn serial_ports_handler(
) -> Result<Json<Vec<embarch_topology::hardware::DetectedPort>>, (StatusCode, String)> {
    tokio::task::spawn_blocking(embarch_topology::hardware::list_serial_ports)
        .await
        .map_err(internal_err)?
        .map_err(internal_err)
        .map(Json)
}

// ---- GET /dev-bench/port ----------------------------------------------------

/// Which serial port `embarch-dev-bench` is on
/// (`embarch_topology::hardware::resolve_dev_bench_port`).
///
/// Takes no `hw_lock`: this only reads USB descriptors the OS already
/// enumerated, opening nothing — same as `/status`'s probe listing.
///
/// "Not plugged in" answers `404`, not `500`: it's an expected state of the
/// bench, not a Core failure, and `embarch-api` needs to distinguish it from a
/// genuinely broken detection (an ambiguous match, or an unreadable USB bus),
/// which still comes back as `500` with the full error chain.
async fn dev_bench_port_handler(
) -> Result<Json<embarch_topology::hardware::DevBenchPort>, (StatusCode, String)> {
    let detected = tokio::task::spawn_blocking(embarch_topology::hardware::resolve_dev_bench_port)
        .await
        .map_err(internal_err)?;

    match detected {
        Ok(port) => Ok(Json(port)),
        Err(e) if e.downcast_ref::<embarch_topology::hardware::DevBenchNotFound>().is_some() => {
            let msg = format!("{e:?}");
            tracing::info!("{msg}");
            Err((StatusCode::NOT_FOUND, msg))
        }
        Err(e) => Err(internal_err(e)),
    }
}

// ---- POST /validate ---------------------------------------------------

/// Explicit, non-destructive live re-check of an already-enrolled board's
/// identity (`embarch_topology::hardware::validate_role_timed`, decision 28) — the exact same check `flash`/`reset`/the dev-bench
/// handshake already run mid-attach (decisions 8, 22), callable on its own,
/// any time, without an actual `flash`/`reset`/`run_study` call to trigger
/// it. Takes `hw_lock` like `/flash`/`/reset` — it opens the same physical
/// probe connection those do, and shouldn't be allowed to race either.
/// Calls the `_timed` variant (`embarch-topology` decision 26) so the
/// response can carry `validated_at_utc_ms` — the instant *this* call's
/// check passed — alongside the unchanged `confirmed_at_utc_ms` from the
/// enrolled record (`embarch-core` decision below).
#[derive(Deserialize)]
struct ValidateRequest {
    role: String,
}

#[derive(Serialize)]
struct ValidateOkResponse {
    ok: bool,
    role: String,
    probe_serial: String,
    chip: String,
    /// The probe-read (JTAG) hardware ID, not the bench's self-reported one —
    /// `hardware_id`, unprefixed, is this suite's name for the probe/JTAG-read
    /// identity everywhere except `GET /dev-bench/hello` (decision 56).
    hardware_id: String,
    confirmed_at_utc_ms: u64,
    /// The instant *this* live check's hardware-ID compare passed — distinct
    /// from `confirmed_at_utc_ms` above, which names *enrolment* time and
    /// does not move on a re-check (`embarch-topology` decision 26;
    /// `embarch-core` decision below). Two `/validate` calls minutes or days
    /// apart used to come back with identical `confirmed_at_utc_ms`, which a
    /// caller reading it as freshness could mistake for a plausible, wrong
    /// answer in the safe-looking direction.
    validated_at_utc_ms: u64,
}

/// Mirrors `embarch_topology::hardware::TopologyMismatch`'s fields — a
/// separate response type, rather than serializing that struct directly, so
/// this endpoint's own JSON contract doesn't silently shift if that crate's
/// internal error type ever gains/renames a field (`EnrollProbeResponse`'s
/// own precedent for the same reasoning against `EnrolledBoard`).
///
/// `kind` is the field a caller branches on — never the leading words of
/// `reason` (`embarch-core` decision 59). `"not_attached"` (`live_hardware_id`
/// is `None`: nothing was compared, so this is not a mismatch at all) and
/// `"mismatch"` (`live_hardware_id` is `Some` and differs from
/// `recorded_hardware_id`) get opposite handling downstream
/// (`../../embarch-fleet/protocol.md`'s `.claude/leg.md`: one leaves a task
/// `open`, the other alerts a human) and must be tellable apart without
/// reading to the end of the sentence. `fix_it_url` is only meaningful for
/// `"mismatch"` — the fix for a detached probe is a USB cable, not the
/// Topology tab — so it is `None` on the `"not_attached"` arm.
#[derive(Serialize)]
struct ValidateMismatchResponse {
    ok: bool,
    kind: &'static str,
    role: String,
    probe_serial: String,
    chip: String,
    recorded_hardware_id: String,
    live_hardware_id: Option<String>,
    reason: String,
    fix_it_url: Option<String>,
}

/// `live_hardware_id` is `None` exactly when nothing was compared — the
/// probe couldn't be opened at all, unplugged being the ordinary reason —
/// which is not a mismatch (`embarch-core` decision 59). Distinguished here
/// by a `kind` field and a different status, not by the wording of `reason`:
/// `503` ("try again once it's plugged in") for the absent case, `409` ("a
/// human must reconcile this") only when a live ID was actually read and
/// disagreed. `fix_it_url` — the Topology tab — only makes sense for the
/// latter; the fix for a detached probe is a USB cable.
fn classify_topology_mismatch(
    m: &embarch_topology::hardware::TopologyMismatch,
) -> (StatusCode, &'static str, Option<String>) {
    if m.live_hardware_id.is_none() {
        (StatusCode::SERVICE_UNAVAILABLE, "not_attached", None)
    } else {
        (StatusCode::CONFLICT, "mismatch", Some(m.fix_it_url.clone()))
    }
}

async fn validate_handler(
    State(state): State<AppState>,
    Json(req): Json<ValidateRequest>,
) -> Result<Response, (StatusCode, String)> {
    let _guard = acquire_hw_lock(&state, "POST /validate").await?;
    let role = req.role;

    let result = tokio::task::spawn_blocking(move || embarch_topology::hardware::validate_role_timed(&role))
        .await
        .map_err(internal_err)?;

    match result {
        Ok(validation) => {
            let board = validation.board;
            Ok((
                StatusCode::OK,
                Json(ValidateOkResponse {
                    ok: true,
                    role: board.role,
                    probe_serial: board.probe_serial,
                    chip: board.chip,
                    hardware_id: board.hardware_id,
                    confirmed_at_utc_ms: board.confirmed_at_utc_ms,
                    validated_at_utc_ms: validation.validated_at_utc_ms,
                }),
            )
                .into_response())
        }
        Err(e) => {
            // A topology mismatch is an expected, structured outcome of a
            // non-destructive check — not a Core failure — so it's a `409
            // Conflict` (matching `/study`'s own use of that status for "a
            // real, named condition the caller can act on"), with the full
            // structured fields as its JSON body, never collapsed into
            // plain-text `500` prose the way an unrelated I/O error still is
            // below.
            if let Some(m) = e.downcast_ref::<embarch_topology::hardware::TopologyMismatch>() {
                let msg = format!("{e:?}");
                tracing::info!("{msg}");
                let (status, kind, fix_it_url) = classify_topology_mismatch(m);
                return Ok((
                    status,
                    Json(ValidateMismatchResponse {
                        ok: false,
                        kind,
                        role: m.role.clone(),
                        probe_serial: m.probe_serial.clone(),
                        chip: m.chip.clone(),
                        recorded_hardware_id: m.recorded_hardware_id.clone(),
                        live_hardware_id: m.live_hardware_id.clone(),
                        reason: m.reason.clone(),
                        fix_it_url,
                    }),
                )
                    .into_response());
            }
            // No board enrolled under this role yet — an ordinary "not
            // configured" state (decision 28), not a Core
            // failure — `404`, matching `/dev-bench/port`'s own "unplugged
            // bench" posture.
            if e.downcast_ref::<embarch_topology::hardware::NotEnrolled>().is_some() {
                let msg = format!("{e:?}");
                tracing::info!("{msg}");
                return Err((StatusCode::NOT_FOUND, msg));
            }
            Err(internal_err(e))
        }
    }
}

// ---- GET /alerts --------------------------------------------------------

/// Recent topology-mismatch alerts from `embarch-topology`'s durable log
/// (`embarch_topology::hardware::recent_alerts`, decision 28) —
/// what a human (or an agent, after a `409` from `/validate` above) checks
/// to see the full mismatch history, not just the one that just happened.
/// Pure read of a local file, no hardware touched — no `hw_lock`, same
/// posture as `/probes/enrolled`/`/dev-bench/port`'s enumeration.
#[derive(Deserialize)]
struct AlertsQuery {
    #[serde(default = "default_alerts_limit")]
    limit: usize,
}

fn default_alerts_limit() -> usize {
    20
}

async fn alerts_handler(
    Query(q): Query<AlertsQuery>,
) -> Result<Json<Vec<embarch_topology::hardware::Alert>>, (StatusCode, String)> {
    let limit = q.limit;
    tokio::task::spawn_blocking(move || embarch_topology::hardware::recent_alerts(limit))
        .await
        .map_err(internal_err)?
        .map_err(internal_err)
        .map(Json)
}

// ---- GET /logs/recent ---------------------------------------------------
//
// `embarch-ui` decision 7 (the Debug tab): backlog-on-open, mediated through
// Core rather than embarch-ui ever reading a logfile directly — Core can
// run on a different machine than whatever's asking (the whole reason
// `embarch-topology` exists). Reuses `logs.rs`'s existing
// daily-rolling-logfile logic (`main.rs`'s own `Logs` CLI subcommand shares
// it too) rather than a second, size-capped mechanism this decision
// originally proposed before noticing one already existed.
//
// `GET /logs/stream`, the live-tail counterpart decision 7 also built, was
// retired (`tasks/core/021`): decision 13 in that same file structurally
// excludes an SSE source by sharing one poll/diff loop across both log
// sources, nothing replaced it, and no caller anywhere ever used it. This
// route's own `logs::FollowState` poll-follow machinery went with it;
// `read_recent`/`tail_lines` below are unaffected. `decisions/logging.md`
// decision 44 (the hold-past-`\n` rule this route needed) is retired
// alongside it.

#[derive(Deserialize)]
struct LogsRecentQuery {
    #[serde(default = "default_logs_recent_tail")]
    tail: usize,
}

fn default_logs_recent_tail() -> usize {
    200
}

#[derive(Serialize)]
struct LogsRecentResponse {
    lines: Vec<String>,
}

/// Backlog on first open — the tail of Core's current daily log file, pure
/// local read, no hardware touched. `?tail=<n>` (default 200) is the one
/// knob; no level/component filtering server-side (`embarch-core` open.md's open
/// question, resolved this way: the client can filter/color client-side
/// from the same plain lines `tracing_subscriber`'s own formatter already
/// produces — reformatting Core's actual log output into structured JSON
/// just for this would be a real change to a foundational, already-
/// deployed piece of a live service, not something this decision needs).
async fn logs_recent_handler(
    Query(q): Query<LogsRecentQuery>,
) -> Result<Json<LogsRecentResponse>, (StatusCode, String)> {
    let tail = q.tail;
    let lines = tokio::task::spawn_blocking(move || logs::read_recent(tail))
        .await
        .map_err(internal_err)?
        .map_err(internal_err)?;
    Ok(Json(LogsRecentResponse { lines }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request as HttpRequest;

    #[tokio::test]
    async fn serial_log_over_the_duration_cap_is_a_bad_request_naming_both_numbers() {
        // No port is ever opened: the cap is checked before `hw_lock` is
        // even taken, so a nonexistent port name never gets far enough to
        // matter.
        let state = AppState::new("t".to_string());
        let q = SerialLogQuery {
            port: "COM_NONEXISTENT".to_string(),
            baud: default_baud(),
            duration_ms: serial::MAX_DURATION_MS + 1,
        };
        let err = serial_log_handler(State(state), Query(q)).await.unwrap_err();
        assert_eq!(err.0, StatusCode::BAD_REQUEST);
        assert!(err.1.contains(&serial::MAX_DURATION_MS.to_string()));
        assert!(err.1.contains(&(serial::MAX_DURATION_MS + 1).to_string()));
    }

    #[tokio::test]
    async fn serial_log_at_the_duration_cap_is_unchanged() {
        // At-cap must not be rejected by the cap check itself; it fails
        // later, on the nonexistent port, which is the pre-existing
        // behavior this change must not disturb.
        let state = AppState::new("t".to_string());
        let q = SerialLogQuery {
            port: "COM_NONEXISTENT".to_string(),
            baud: default_baud(),
            duration_ms: serial::MAX_DURATION_MS,
        };
        let err = serial_log_handler(State(state), Query(q)).await.unwrap_err();
        // Not the cap's own message — it got past validation and failed
        // opening the port instead.
        assert_ne!(err.0, StatusCode::BAD_REQUEST);
    }

    // ---- decision 14: `503` naming the holder on `hw_lock` contention ------

    /// Real contention, not a mocked one: one task holds `hw_lock` (via
    /// `acquire_hw_lock` itself, the same path every handler uses) for
    /// longer than `HW_LOCK_WAIT_MS`, and a second call observes a `503`
    /// naming the first call's route as the holder. This is the test
    /// `tasks/core/013` calls for — decision 14 built, not merely typed.
    #[tokio::test]
    async fn a_second_caller_is_refused_503_naming_the_holder_under_real_contention() {
        let state = AppState::new("t".to_string());

        let holder_guard = acquire_hw_lock(&state, "POST /flash").await.expect("first caller must succeed uncontended");

        let contender_state = state.clone();
        let contender = tokio::spawn(async move { acquire_hw_lock(&contender_state, "POST /reset").await });

        // Hold past the contender's whole wait window before releasing —
        // this is what makes the contention real rather than a race that
        // might resolve either way.
        tokio::time::sleep(std::time::Duration::from_millis(HW_LOCK_WAIT_MS + 200)).await;
        drop(holder_guard);

        let err = contender.await.unwrap().expect_err("a held hw_lock must refuse, not queue silently");
        assert_eq!(err.0, StatusCode::SERVICE_UNAVAILABLE);
        assert!(
            err.1.contains("POST /flash"),
            "503 body must name the actual holder, not just say 'busy': {}",
            err.1
        );
    }

    /// The mirror case: once the holder releases, a fresh caller succeeds
    /// uncontended rather than being wedged by stale holder state left over
    /// from a previous guard — `HwGuard::drop` is what clears it.
    #[tokio::test]
    async fn hw_lock_is_free_again_once_the_holder_drops() {
        let state = AppState::new("t".to_string());

        let first = acquire_hw_lock(&state, "POST /flash").await.unwrap();
        drop(first);

        let second = acquire_hw_lock(&state, "POST /reset").await;
        assert!(second.is_ok(), "hw_lock must be free once the prior guard dropped");
    }

    /// Uncontended acquisitions never wait `HW_LOCK_WAIT_MS` — the common
    /// case this design is meant to stay cheap for.
    #[tokio::test]
    async fn an_uncontended_acquire_does_not_pay_the_wait_timeout() {
        let state = AppState::new("t".to_string());
        let started = std::time::Instant::now();
        let _guard = acquire_hw_lock(&state, "POST /flash").await.unwrap();
        assert!(started.elapsed() < std::time::Duration::from_millis(HW_LOCK_WAIT_MS));
    }

    /// Hand-built `multipart/form-data` body — no HTTP client needed, this
    /// exercises exactly what `/flash` actually parses (`embarch-api` decision 15's
    /// 2026-08-18 finding: `embarch-api` uploads bytes for a `WslHost`/
    /// `Remote` Core rather than sending a path it can't open).
    fn multipart_request(chip: &str, format: Option<&str>, firmware: &[u8]) -> Request {
        multipart_request_full(chip, format, None, firmware)
    }

    fn multipart_request_full(
        chip: &str,
        format: Option<&str>,
        base_address: Option<&str>,
        firmware: &[u8],
    ) -> Request {
        const BOUNDARY: &str = "embarch-core-test-boundary";
        let mut body = Vec::new();
        body.extend_from_slice(format!("--{BOUNDARY}\r\n").as_bytes());
        body.extend_from_slice(b"Content-Disposition: form-data; name=\"chip\"\r\n\r\n");
        body.extend_from_slice(chip.as_bytes());
        body.extend_from_slice(b"\r\n");

        if let Some(format) = format {
            body.extend_from_slice(format!("--{BOUNDARY}\r\n").as_bytes());
            body.extend_from_slice(b"Content-Disposition: form-data; name=\"format\"\r\n\r\n");
            body.extend_from_slice(format.as_bytes());
            body.extend_from_slice(b"\r\n");
        }

        if let Some(base_address) = base_address {
            body.extend_from_slice(format!("--{BOUNDARY}\r\n").as_bytes());
            body.extend_from_slice(b"Content-Disposition: form-data; name=\"base_address\"\r\n\r\n");
            body.extend_from_slice(base_address.as_bytes());
            body.extend_from_slice(b"\r\n");
        }

        body.extend_from_slice(format!("--{BOUNDARY}\r\n").as_bytes());
        body.extend_from_slice(
            b"Content-Disposition: form-data; name=\"firmware\"; filename=\"zephyr.hex\"\r\n\
              Content-Type: application/octet-stream\r\n\r\n",
        );
        body.extend_from_slice(firmware);
        body.extend_from_slice(b"\r\n");
        body.extend_from_slice(format!("--{BOUNDARY}--\r\n").as_bytes());

        HttpRequest::builder()
            .method("POST")
            .uri("/flash")
            .header(CONTENT_TYPE, format!("multipart/form-data; boundary={BOUNDARY}"))
            .body(Body::from(body))
            .unwrap()
    }

    #[tokio::test]
    async fn multipart_flash_args_round_trip_chip_format_and_firmware_bytes() {
        let request = multipart_request("nRF54L15", Some("hex"), b"fake firmware bytes");
        let multipart = Multipart::from_request(request, &()).await.unwrap();
        let args = flash_args_from_multipart(multipart).await.unwrap();

        assert_eq!(args.chip, "nRF54L15");
        assert_eq!(args.format, "hex");
        assert_eq!(
            std::fs::read(&args.path).unwrap(),
            b"fake firmware bytes"
        );
        // The temp file must still exist while `_uploaded` is alive — this
        // is the exact property the flash handler depends on (read the
        // file during the blocking flash call, only then let it drop).
        assert!(args._uploaded.is_some());
    }

    #[tokio::test]
    async fn multipart_flash_args_default_format_when_omitted() {
        let request = multipart_request("nRF54L15", None, b"bytes");
        let multipart = Multipart::from_request(request, &()).await.unwrap();
        let args = flash_args_from_multipart(multipart).await.unwrap();

        assert_eq!(args.format, "elf"); // default_format()
    }

    #[tokio::test]
    async fn multipart_flash_args_missing_chip_is_a_bad_request() {
        const BOUNDARY: &str = "b";
        let mut body = Vec::new();
        body.extend_from_slice(format!("--{BOUNDARY}\r\n").as_bytes());
        body.extend_from_slice(
            b"Content-Disposition: form-data; name=\"firmware\"; filename=\"x.hex\"\r\n\r\nbytes\r\n",
        );
        body.extend_from_slice(format!("--{BOUNDARY}--\r\n").as_bytes());
        let request = HttpRequest::builder()
            .method("POST")
            .uri("/flash")
            .header(CONTENT_TYPE, format!("multipart/form-data; boundary={BOUNDARY}"))
            .body(Body::from(body))
            .unwrap();

        let multipart = Multipart::from_request(request, &()).await.unwrap();
        let err = flash_args_from_multipart(multipart).await.unwrap_err();
        assert_eq!(err.0, StatusCode::BAD_REQUEST);
        assert!(err.1.contains("chip"));
    }

    #[tokio::test]
    async fn multipart_flash_args_missing_firmware_part_is_a_bad_request() {
        const BOUNDARY: &str = "b";
        let mut body = Vec::new();
        body.extend_from_slice(format!("--{BOUNDARY}\r\n").as_bytes());
        body.extend_from_slice(b"Content-Disposition: form-data; name=\"chip\"\r\n\r\nnRF54L15\r\n");
        body.extend_from_slice(format!("--{BOUNDARY}--\r\n").as_bytes());
        let request = HttpRequest::builder()
            .method("POST")
            .uri("/flash")
            .header(CONTENT_TYPE, format!("multipart/form-data; boundary={BOUNDARY}"))
            .body(Body::from(body))
            .unwrap();

        let multipart = Multipart::from_request(request, &()).await.unwrap();
        let err = flash_args_from_multipart(multipart).await.unwrap_err();
        assert_eq!(err.0, StatusCode::BAD_REQUEST);
        assert!(err.1.contains("firmware"));
    }

    // base_address (`embarch-dev-bench` decision 26): only meaningful for format = "bin", but parsed
    // the same way regardless of which format accompanies it — parsing is a
    // pure string→u64 concern, independent of hardware.rs's own decision to
    // ignore it for every format but Bin.

    #[tokio::test]
    async fn multipart_flash_args_base_address_hex() {
        let request = multipart_request_full("esp32c5", Some("bin"), Some("0x2000"), b"bytes");
        let multipart = Multipart::from_request(request, &()).await.unwrap();
        let args = flash_args_from_multipart(multipart).await.unwrap();
        assert_eq!(args.base_address, Some(0x2000));
    }

    #[tokio::test]
    async fn multipart_flash_args_base_address_decimal() {
        let request = multipart_request_full("esp32c5", Some("bin"), Some("8192"), b"bytes");
        let multipart = Multipart::from_request(request, &()).await.unwrap();
        let args = flash_args_from_multipart(multipart).await.unwrap();
        assert_eq!(args.base_address, Some(8192));
    }

    #[tokio::test]
    async fn multipart_flash_args_omitted_base_address_is_none() {
        let request = multipart_request_full("esp32c5", Some("bin"), None, b"bytes");
        let multipart = Multipart::from_request(request, &()).await.unwrap();
        let args = flash_args_from_multipart(multipart).await.unwrap();
        assert_eq!(args.base_address, None);
    }

    #[tokio::test]
    async fn multipart_flash_args_invalid_base_address_is_a_bad_request() {
        let request = multipart_request_full("esp32c5", Some("bin"), Some("not-a-number"), b"bytes");
        let multipart = Multipart::from_request(request, &()).await.unwrap();
        let err = flash_args_from_multipart(multipart).await.unwrap_err();
        assert_eq!(err.0, StatusCode::BAD_REQUEST);
        assert!(err.1.contains("base_address"));
    }

    #[test]
    fn parse_base_address_accepts_hex_and_decimal() {
        assert_eq!(parse_base_address("0x2000").unwrap(), 0x2000);
        assert_eq!(parse_base_address("0X2000").unwrap(), 0x2000);
        assert_eq!(parse_base_address("8192").unwrap(), 8192);
    }

    #[test]
    fn parse_base_address_rejects_garbage() {
        let err = parse_base_address("not-a-number").unwrap_err();
        assert_eq!(err.0, StatusCode::BAD_REQUEST);
        assert!(err.1.contains("not-a-number"));
    }

    // ---- build_router requires the bearer token on every route ----
    //
    // `embarch-doc/embarch-core/spec.md` states every route requires
    // `Authorization: Bearer <token>`, "no exceptions" — there used to be
    // exactly one deliberate exemption (`GET /enroll`'s static page),
    // retired 2026-08-24 (`embarch-ui` decision 1).
    //
    // That invariant used to be checked by one hand-written test per route,
    // and the two lists drifted exactly as you would expect: on 2026-09-06,
    // twelve of the twenty-six registered paths had a test and fourteen —
    // including every `/study*` route, the newest surface — had none. So the
    // list is derived from `build_router`'s own source instead
    // (`embarch-doc/embarch-core/decisions/platform.md` decision 42).
    // `every_registered_route_has_an_auth_case` fails when a
    // registered path has no row in `AUTH_CASES`, and the two sweeps below
    // drive every row through the real router with no token and with a wrong
    // one. Adding a route without adding its row is a test failure rather
    // than a silently open path.
    //
    // Nothing here touches hardware: `auth_middleware` is a `.layer` on the
    // whole router, so it rejects before axum routes the request at all and
    // no handler — probe, serial port or study — ever runs.

    use tower::ServiceExt as _;

    fn test_router() -> Router {
        build_router(AppState::new("test-token".to_string()))
    }

    /// The prefix a `build_router` registration line starts with. A `const`
    /// so that this file's own source contains the literal only here, on a
    /// line the scan below skips.
    const ROUTE_MARKER: &str = ".route(\"";

    /// One row per `(method, registered path, concrete URI)` that
    /// `build_router` wires up. The path is matched against the source; the
    /// URI is what actually gets sent, which is why the parameterised routes
    /// carry a substituted segment for `{study_id}` / `{name}`. A path with
    /// two methods gets two rows.
    const AUTH_CASES: &[(&str, &str, &str)] = &[
        ("GET", "/status", "/status"),
        ("POST", "/flash", "/flash"),
        ("POST", "/reset", "/reset"),
        ("GET", "/serial-log", "/serial-log"),
        ("GET", "/dev-bench/port", "/dev-bench/port"),
        ("GET", "/dev-bench/hello", "/dev-bench/hello"),
        ("POST", "/resolve-chip", "/resolve-chip"),
        ("POST", "/probes/enroll", "/probes/enroll"),
        ("GET", "/probes/enrolled", "/probes/enrolled"),
        ("POST", "/dev-bench/link", "/dev-bench/link"),
        ("POST", "/signals", "/signals"),
        ("GET", "/signals", "/signals"),
        ("DELETE", "/signals/{name}", "/signals/outpost"),
        ("GET", "/serial-ports", "/serial-ports"),
        ("POST", "/validate", "/validate"),
        ("GET", "/alerts", "/alerts"),
        ("GET", "/logs/recent", "/logs/recent"),
        ("POST", "/study", "/study"),
        ("GET", "/study/{study_id}", "/study/abc"),
        ("GET", "/study/{study_id}/events", "/study/abc/events"),
        ("GET", "/study/{study_id}/steps", "/study/abc/steps"),
        ("GET", "/study/{study_id}/streams", "/study/abc/streams"),
        ("GET", "/study/{study_id}/stream/{name}", "/study/abc/stream/power"),
    ];

    /// Every path `build_router` registers, read out of this file's own
    /// source. Reading the text is the only way in: axum exposes no route
    /// iterator, so a built `Router` cannot be asked what it serves.
    fn registered_route_paths() -> Vec<String> {
        include_str!("api.rs")
            .lines()
            .map(str::trim)
            .filter(|l| l.starts_with(ROUTE_MARKER))
            .map(|l| l[ROUTE_MARKER.len()..].split('"').next().unwrap().to_string())
            .collect()
    }

    /// The number of distinct `.route(` registration lines `build_router`
    /// carries — i.e. `registered_route_paths().len()`, **not**
    /// `AUTH_CASES.len()`: `/signals` is one `.route()` call chaining
    /// `.get()`/`.post()`, one line but two auth cases, so this number runs
    /// one behind that one. Hand-counted against
    /// `embarch-doc/embarch-core/interfaces.md`'s tables. **Not** derived by
    /// reading that file: this crate has no
    /// reliable relative path to it — the doc repo is a sibling checkout in
    /// the normal layout but a *different* worktree entirely under the
    /// fleet's one-branch-two-worktrees model (`embarch-fleet/protocol.md`
    /// §5), so a path that resolves for a human at a desk breaks under a
    /// worker's checkout with no signal beyond an `include_str!` compile
    /// error naming a path nobody touched (decision 46). A pinned literal,
    /// checked against the same source scan `AUTH_CASES` already relies on,
    /// catches the same drift `tasks/core/018` found without that cross-repo
    /// dependency.
    const DOCUMENTED_ROUTE_COUNT: usize = 22;

    #[test]
    fn registered_route_count_matches_the_count_documented_in_interfaces_md() {
        let registered = registered_route_paths();
        assert_eq!(
            registered.len(),
            DOCUMENTED_ROUTE_COUNT,
            "`build_router` now registers {} `.route(` lines; \
             `embarch-doc/embarch-core/interfaces.md` was last counted at \
             {DOCUMENTED_ROUTE_COUNT}. Add (or remove) the row there, then move \
             `DOCUMENTED_ROUTE_COUNT` to match in the same commit.",
            registered.len()
        );
    }

    #[test]
    fn every_registered_route_has_an_auth_case() {
        let registered = registered_route_paths();
        assert!(
            registered.len() > 20,
            "the scan for `{ROUTE_MARKER}` found {} route registrations, which is fewer \
             than `build_router` has ever had — the scan broke, not the router.",
            registered.len()
        );

        for path in &registered {
            assert!(
                AUTH_CASES.iter().any(|(_, p, _)| p == path),
                "`{path}` is registered in `build_router` but has no row in `AUTH_CASES`, \
                 so nothing asserts it requires the bearer token. Add the row — `spec.md` \
                 says every route requires it, no exceptions."
            );
        }

        for (_, path, _) in AUTH_CASES {
            assert!(
                registered.iter().any(|p| p == path),
                "`AUTH_CASES` covers `{path}`, which `build_router` no longer registers. \
                 Drop the row; a stale row is the same drift in the other direction."
            );
        }
    }

    #[tokio::test]
    async fn every_registered_route_rejects_a_missing_bearer_token() {
        for (method, path, uri) in AUTH_CASES {
            let response = test_router()
                .oneshot(
                    Request::builder()
                        .method(*method)
                        .uri(*uri)
                        .body(axum::body::Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(
                response.status(),
                StatusCode::UNAUTHORIZED,
                "{method} {path} (sent as {uri}) answered a request with no \
                 Authorization header"
            );
        }
    }

    #[tokio::test]
    async fn every_registered_route_rejects_a_wrong_bearer_token() {
        for (method, path, uri) in AUTH_CASES {
            let response = test_router()
                .oneshot(
                    Request::builder()
                        .method(*method)
                        .uri(*uri)
                        .header("authorization", "Bearer not-the-token")
                        .body(axum::body::Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(
                response.status(),
                StatusCode::UNAUTHORIZED,
                "{method} {path} (sent as {uri}) answered a request carrying the wrong \
                 bearer token"
            );
        }
    }

    /// **The other half of a mirror no crate can see both sides of.**
    ///
    /// `embarch-core-client`'s `SignalLink` is a hand-maintained mirror of
    /// `embarch_topology::hardware::SignalLink` — it has to be, since the real
    /// type is behind that crate's `hardware` feature and pulling that in
    /// would link `probe-rs`/`serialport` into a client that deliberately
    /// never does. Nothing compiles both types, so the coupling is pinned from
    /// each side against this same literal;
    /// `embarch-core-client`'s `a_declared_signal_serializes_to_the_shape_core_parses`
    /// is the other assertion. If you change one, change both.
    #[test]
    fn the_signal_link_wire_shape_is_what_clients_send() {
        const SIGNAL_LINK_JSON: &str = concat!(
            r#"{"name":"outpost","origin_role":"dut","direction":"dut-to-host","#,
            r#""route":{"kind":"direct","port_serial":"ABC123"}}"#
        );

        let link: embarch_topology::hardware::SignalLink =
            serde_json::from_str(SIGNAL_LINK_JSON).expect("a client's POST /signals body parses");
        assert_eq!(link.name, "outpost");
        assert_eq!(link.origin_role, "dut");
        assert_eq!(link.direction, embarch_topology::hardware::SignalDirection::DutToHost);
        assert_eq!(
            link.route,
            embarch_topology::hardware::Route::Direct { port_serial: "ABC123".to_string() }
        );
        assert_eq!(serde_json::to_string(&link).unwrap(), SIGNAL_LINK_JSON);
    }

    #[tokio::test]
    async fn status_succeeds_with_the_correct_bearer_token() {
        let response = test_router()
            .oneshot(
                Request::builder()
                    .uri("/status")
                    .header("authorization", "Bearer test-token")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    /// The parameterised route (decision 30) is actually
    /// wired to `study::stream_data_handler`, not falling through to axum's
    /// own not-found — which is the difference this asserts, since an
    /// unrouted path with a valid token 404s too, just with an empty body.
    #[tokio::test]
    async fn stream_data_is_routed_to_the_handler_rather_than_the_fallback() {
        let response = test_router()
            .oneshot(
                Request::builder()
                    .uri("/study/0123456789abcdef0123456789abcdef/stream/power")
                    .header("authorization", "Bearer test-token")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        let body = axum::body::to_bytes(response.into_body(), 64 * 1024).await.unwrap();
        let body = String::from_utf8_lossy(&body);
        assert!(
            body.contains("no captured streams"),
            "expected the handler's own 404, got: {body}"
        );
    }

    // ---- GET /status's serialized shape --------------------------------
    //
    // `interfaces.md`'s `/status` row is the published contract, and this
    // sub-project has shipped that row describing fields the code did not
    // emit (decision 13's own note: "the endpoint table described all three
    // fields as shipped for months when none were"). These two tests are
    // what makes that class of drift a test failure instead of a doc bug:
    // the key set is pinned exactly, so **adding** a field to the response
    // without editing the doc row fails just as loudly as removing one.

    #[test]
    fn status_response_serializes_exactly_the_documented_fields() {
        let json = serde_json::to_value(StatusResponse {
            status: "ok",
            probes: Vec::new(),
            study_designer_schema_version: 7,
            core_version: "9.9.9",
        })
        .unwrap();

        let mut keys: Vec<&str> = json
            .as_object()
            .expect("a struct serializes to an object")
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            [
                "core_version",
                "probes",
                "status",
                "study_designer_schema_version",
            ],
            "GET /status's field set changed — update embarch-doc/embarch-core/interfaces.md's \
             /status row in the same commit"
        );
        assert_eq!(json["core_version"], "9.9.9");
    }

    /// The served `core_version` is the compiled-in crate version, not a
    /// hand-maintained constant that could disagree with the binary — the
    /// property that makes the field worth trusting at all.
    #[tokio::test]
    async fn status_serves_this_binary_s_own_crate_version() {
        let response = test_router()
            .oneshot(
                Request::builder()
                    .uri("/status")
                    .header("authorization", "Bearer test-token")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), 256 * 1024).await.unwrap();
        let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(body["core_version"], env!("CARGO_PKG_VERSION"));
        assert!(
            body.get("contract_version").is_none(),
            "no hand-bumped contract_version is served (decision 13, amended \
             2026-09-03); if one is added, say so in interfaces.md"
        );
    }

    // ---- pinning `embarch_topology::hardware::{EnrolledBoard, Alert}` against
    // `embarch-api`'s hand-maintained mirror -------------------------------
    //
    // `embarch-api/crates/embarch-core-client/src/client.rs`'s
    // `EnrolledBoardResponse`/`AlertResponse` are hand-maintained mirrors of
    // these two real types (`GET /probes/enrolled` and `GET /alerts` above
    // serialise them verbatim). Only that client-side crate pinned its own
    // shape (`an_enrolled_board_round_trips_against_the_pinned_shape`,
    // `an_alert_round_trips_against_the_pinned_shape`) — nothing pinned the
    // real types against the same literal, so a field added here without a
    // matching client-side change would typecheck cleanly on both sides and
    // fail only against a live Core (`tasks/core/024`). These two tests use
    // the exact same JSON strings as those client-side tests; a copy, not a
    // shared constant, because the two crates don't share a dependency this
    // could live in without embarch-core depending on embarch-api or vice
    // versa — if the two literals below and the client-side ones you find by
    // searching for `ENROLLED_BOARD_RESPONSE_JSON`/`ALERT_RESPONSE_JSON` in
    // `embarch-api/crates/embarch-core-client/src/client.rs` ever disagree,
    // that disagreement (not just a red test here) is the finding.

    const ENROLLED_BOARD_RESPONSE_JSON: &str = concat!(
        r#"{"probe_serial":"ABC123","role":"dev-bench","chip":"nrf54l15","#,
        r#""hardware_id":"AAAA","confirmed_at_utc_ms":1725000000000,"#,
        r#""link_port_serial":"D607104","link_port_interface":2}"#
    );

    fn sample_enrolled_board() -> embarch_topology::hardware::EnrolledBoard {
        embarch_topology::hardware::EnrolledBoard {
            probe_serial: "ABC123".to_string(),
            role: "dev-bench".to_string(),
            chip: "nrf54l15".to_string(),
            hardware_id: "AAAA".to_string(),
            confirmed_at_utc_ms: 1725000000000,
            link_port_serial: Some("D607104".to_string()),
            link_port_interface: Some(2),
        }
    }

    /// Pins `embarch_topology::hardware::EnrolledBoard` against the same
    /// JSON `embarch-api`'s
    /// `an_enrolled_board_round_trips_against_the_pinned_shape` (client.rs)
    /// pins its own mirror type against — including `link_port_interface`,
    /// the field that silently dropped out of the mirror for a release
    /// (`embarch-topology` decision 20) before that test existed.
    #[test]
    fn enrolled_board_round_trips_against_the_client_s_pinned_shape() {
        assert_eq!(
            serde_json::to_string(&sample_enrolled_board()).unwrap(),
            ENROLLED_BOARD_RESPONSE_JSON
        );
        assert_eq!(
            serde_json::from_str::<embarch_topology::hardware::EnrolledBoard>(
                ENROLLED_BOARD_RESPONSE_JSON
            )
            .unwrap(),
            sample_enrolled_board()
        );
    }

    const ALERT_RESPONSE_JSON: &str = concat!(
        r#"{"id":"18f3a2-4242","occurred_at_utc_ms":1725000000000,"role":"dut","#,
        r#""probe_serial":"ABC123","chip":"nrf54l15","recorded_hardware_id":"AAAA","#,
        r#""live_hardware_id":"BBBB","reason":"hardware id mismatch"}"#
    );

    fn sample_alert() -> embarch_topology::hardware::Alert {
        embarch_topology::hardware::Alert {
            id: "18f3a2-4242".to_string(),
            occurred_at_utc_ms: 1725000000000,
            role: "dut".to_string(),
            probe_serial: "ABC123".to_string(),
            chip: "nrf54l15".to_string(),
            recorded_hardware_id: "AAAA".to_string(),
            live_hardware_id: Some("BBBB".to_string()),
            reason: "hardware id mismatch".to_string(),
        }
    }

    /// Pins `embarch_topology::hardware::Alert` against the same JSON
    /// `embarch-api`'s `an_alert_round_trips_against_the_pinned_shape`
    /// (client.rs) pins its own mirror type against.
    ///
    /// `Alert` (unlike `EnrolledBoard`) doesn't derive `PartialEq`, so the
    /// deserialize-then-compare half round-trips through `to_string` instead
    /// of a struct comparison — still asserts the same thing, that parsing
    /// `ALERT_RESPONSE_JSON` and re-serializing it reproduces the literal
    /// exactly.
    #[test]
    fn alert_round_trips_against_the_client_s_pinned_shape() {
        assert_eq!(serde_json::to_string(&sample_alert()).unwrap(), ALERT_RESPONSE_JSON);
        let parsed: embarch_topology::hardware::Alert =
            serde_json::from_str(ALERT_RESPONSE_JSON).unwrap();
        assert_eq!(serde_json::to_string(&parsed).unwrap(), ALERT_RESPONSE_JSON);
    }

    // ---- decision 59: not-attached is not a mismatch -----------------------

    fn sample_mismatch(live_hardware_id: Option<String>) -> embarch_topology::hardware::TopologyMismatch {
        embarch_topology::hardware::TopologyMismatch {
            role: "dev-bench".to_string(),
            probe_serial: "001057729826".to_string(),
            chip: "nRF54L15".to_string(),
            recorded_hardware_id: "6fcddc36cb781b71".to_string(),
            live_hardware_id,
            reason: "probe '001057729826' enrolled as role 'dev-bench' is not currently attached"
                .to_string(),
            fix_it_url: "http://127.0.0.1:4890/#topology".to_string(),
        }
    }

    /// The regression this whole task is about: a `live_hardware_id` of
    /// `None` — nothing was compared, the probe couldn't be opened — must
    /// come back as `kind: "not_attached"`, never `"mismatch"`, and without
    /// `fix_it_url` (a USB cable, not the Topology tab, is the fix).
    #[test]
    fn a_detached_probe_is_not_attached_not_a_mismatch() {
        let m = sample_mismatch(None);
        let (status, kind, fix_it_url) = classify_topology_mismatch(&m);
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(kind, "not_attached");
        assert_eq!(fix_it_url, None);
    }

    /// A live readback that disagrees with the recorded ID — decision 20's
    /// own case — keeps the `mismatch` kind, the `409`, and `fix_it_url`.
    #[test]
    fn a_wrong_live_id_is_a_mismatch() {
        let m = sample_mismatch(Some("deadbeefdeadbeef".to_string()));
        let (status, kind, fix_it_url) = classify_topology_mismatch(&m);
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(kind, "mismatch");
        assert_eq!(fix_it_url, Some("http://127.0.0.1:4890/#topology".to_string()));
    }

    /// `flash`/`reset`'s plain-text path (`describe_topology_error`) must
    /// give the two conditions different lead words too, not just `/validate`'s
    /// JSON — this is the "other call sites" half of the task.
    #[test]
    fn flash_reset_path_leads_differ_between_not_attached_and_mismatch() {
        let not_attached = anyhow::Error::new(sample_mismatch(None));
        let (status, msg) = describe_topology_error(not_attached);
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert!(msg.starts_with("probe not attached for role"), "got: {msg}");
        assert!(!msg.contains("topology mismatch"), "got: {msg}");

        let mismatch = anyhow::Error::new(sample_mismatch(Some("deadbeefdeadbeef".to_string())));
        let (status, msg) = describe_topology_error(mismatch);
        assert_eq!(status, StatusCode::CONFLICT);
        assert!(msg.starts_with("topology mismatch for role"), "got: {msg}");
    }
}
