use axum::{
    extract::{FromRequest, Json, Multipart, Query, Request, State},
    http::{header::CONTENT_TYPE, StatusCode},
    middleware::{self, Next},
    response::Response,
    routing::{get, post},
    Router,
};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Arc, Mutex as StdMutex};
use tokio::sync::Mutex;

use crate::{chip_resolve, dev_bench, hardware, serial, study};

/// Shared state for every handler. `hw_lock` serializes access to the
/// physical probe/serial connections so a CLI call and a Claude Code call
/// can't collide on the same USB device at the same time.
///
/// `study_lock`/`study_jobs` are `/study*`'s own state (`study.rs`).
/// `study_lock` is explicitly separate from `hw_lock` — a different physical
/// connection (`embarch-core/design.md` §3 decision 15) — so an in-flight
/// study and a `/flash`/`/reset` call never contend on the same guard.
#[derive(Clone)]
pub struct AppState {
    pub token: String,
    pub hw_lock: Arc<Mutex<()>>,
    pub study_lock: study::StudyLock,
    pub study_jobs: study::JobRegistry,
}

impl AppState {
    /// Constructs the two `/study*`-only fields fresh — kept here so
    /// `main.rs`'s `serve` doesn't need to know either type's internals.
    pub fn new(token: String) -> Self {
        Self {
            token,
            hw_lock: Arc::new(Mutex::new(())),
            study_lock: Arc::new(StdMutex::new(None)),
            study_jobs: Arc::new(StdMutex::new(HashMap::new())),
        }
    }
}

pub fn build_router(state: AppState) -> Router {
    Router::new()
        .route("/status", get(status_handler))
        .route("/flash", post(flash_handler))
        .route("/reset", post(reset_handler))
        .route("/serial-log", get(serial_log_handler))
        .route("/dev-bench/port", get(dev_bench_port_handler))
        .route("/dev-bench/hello", get(study::hello_handler))
        .route("/resolve-chip", post(resolve_chip_handler))
        .route("/study", post(study::post_study_handler))
        .route("/study/{study_id}", get(study::get_study_handler))
        .route("/study/{study_id}/power-data", get(study::power_data_handler))
        .route("/study/{study_id}/waveform-data", get(study::waveform_data_handler))
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

// ---- GET /status --------------------------------------------------------

#[derive(Serialize)]
struct StatusResponse {
    status: &'static str,
    probes: Vec<hardware::ProbeInfo>,
}

async fn status_handler() -> Result<Json<StatusResponse>, (StatusCode, String)> {
    let probes = tokio::task::spawn_blocking(hardware::list_probes)
        .await
        .map_err(internal_err)?
        .map_err(internal_err)?;

    Ok(Json(StatusResponse {
        status: "ok",
        probes,
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
    _uploaded: Option<tempfile::NamedTempFile>,
}

/// `/flash` accepts a JSON body (`firmware_path` — a path *this process*
/// can open directly, the same-machine assumption) or a
/// `multipart/form-data` body carrying the artifact's bytes (`design.md`
/// §9 decision 10; `embarch-api/design.md` §9's 2026-08-18 finding is what
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
        FlashArgs {
            chip: req.chip,
            path: PathBuf::from(req.firmware_path),
            format: req.format,
            base_address,
            _uploaded: None,
        }
    };

    let _guard = state.hw_lock.lock().await;

    let chip_for_response = args.chip.clone();
    let FlashArgs {
        chip,
        path,
        format,
        base_address,
        _uploaded,
    } = args;

    tokio::task::spawn_blocking(move || {
        let result = hardware::flash(&chip, &path, &format, base_address);
        drop(_uploaded); // outlives the flash call; dropped (deleted) here, not before
        result
    })
    .await
    .map_err(internal_err)?
    .map_err(internal_err)?;

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
/// body — only meaningful for `format = "bin"`), and a `firmware` file part
/// (required) — the artifact's raw bytes, written to a temp file since
/// `hardware::flash` reads from a path.
async fn flash_args_from_multipart(mut multipart: Multipart) -> Result<FlashArgs, (StatusCode, String)> {
    let mut chip: Option<String> = None;
    let mut format: Option<String> = None;
    let mut base_address_raw: Option<String> = None;
    let mut uploaded: Option<tempfile::NamedTempFile> = None;

    while let Some(field) = multipart.next_field().await.map_err(bad_multipart_field)? {
        match field.name() {
            Some("chip") => chip = Some(field.text().await.map_err(bad_multipart_field)?),
            Some("format") => format = Some(field.text().await.map_err(bad_multipart_field)?),
            Some("base_address") => base_address_raw = Some(field.text().await.map_err(bad_multipart_field)?),
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
        _uploaded: Some(uploaded),
    })
}

// ---- POST /reset ----------------------------------------------------------

#[derive(Deserialize)]
struct ResetRequest {
    chip: String,
}

#[derive(Serialize)]
struct ResetResponse {
    reset: bool,
}

async fn reset_handler(
    State(state): State<AppState>,
    Json(req): Json<ResetRequest>,
) -> Result<Json<ResetResponse>, (StatusCode, String)> {
    let _guard = state.hw_lock.lock().await;
    let chip = req.chip;

    tokio::task::spawn_blocking(move || hardware::reset(&chip))
        .await
        .map_err(internal_err)?
        .map_err(internal_err)?;

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

#[derive(Serialize)]
struct SerialLogResponse {
    port: String,
    lines: Vec<String>,
}

async fn serial_log_handler(
    State(state): State<AppState>,
    Query(q): Query<SerialLogQuery>,
) -> Result<Json<SerialLogResponse>, (StatusCode, String)> {
    let _guard = state.hw_lock.lock().await;

    let port = q.port.clone();
    let baud = q.baud;
    let duration_ms = q.duration_ms;

    let lines = tokio::task::spawn_blocking(move || serial::read_log(&port, baud, duration_ms))
        .await
        .map_err(internal_err)?
        .map_err(internal_err)?;

    Ok(Json(SerialLogResponse {
        port: q.port,
        lines,
    }))
}

// ---- POST /resolve-chip ----------------------------------------------------

/// Zephyr SoC name → probe-rs chip target string (`chip_resolve.rs`,
/// `design.md` §3 decision 8). Pure lookup against probe-rs's own target
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

// ---- GET /dev-bench/port ----------------------------------------------------

/// Which serial port `embarch-dev-bench` is on (`dev_bench.rs`).
///
/// Takes no `hw_lock`: this only reads USB descriptors the OS already
/// enumerated, opening nothing — same as `/status`'s probe listing.
///
/// "Not plugged in" answers `404`, not `500`: it's an expected state of the
/// bench, not a Core failure, and `embarch-api` needs to distinguish it from a
/// genuinely broken detection (an ambiguous match, or an unreadable USB bus),
/// which still comes back as `500` with the full error chain.
async fn dev_bench_port_handler() -> Result<Json<dev_bench::DevBenchPort>, (StatusCode, String)> {
    let detected = tokio::task::spawn_blocking(dev_bench::detect)
        .await
        .map_err(internal_err)?;

    match detected {
        Ok(port) => Ok(Json(port)),
        Err(e) if e.downcast_ref::<dev_bench::NotFound>().is_some() => {
            let msg = format!("{e:?}");
            tracing::info!("{msg}");
            Err((StatusCode::NOT_FOUND, msg))
        }
        Err(e) => Err(internal_err(e)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request as HttpRequest;

    /// Hand-built `multipart/form-data` body — no HTTP client needed, this
    /// exercises exactly what `/flash` actually parses (`design.md` §9's
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

    // base_address (embarch-dev-bench/design.md's ESP JTAG decision, reversing
    // that doc's decision 13): only meaningful for format = "bin", but parsed
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
}
