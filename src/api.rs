use axum::{
    extract::{Json, Query, Request, State},
    http::StatusCode,
    middleware::{self, Next},
    response::Response,
    routing::{get, post},
    Router,
};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::Mutex;

use crate::{hardware, serial};

/// Shared state for every handler. `hw_lock` serializes access to the
/// physical probe/serial connections so a CLI call and a Claude Code call
/// can't collide on the same USB device at the same time.
#[derive(Clone)]
pub struct AppState {
    pub token: String,
    pub hw_lock: Arc<Mutex<()>>,
}

pub fn build_router(state: AppState) -> Router {
    Router::new()
        .route("/status", get(status_handler))
        .route("/flash", post(flash_handler))
        .route("/reset", post(reset_handler))
        .route("/serial-log", get(serial_log_handler))
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

fn internal_err<E: std::fmt::Debug>(e: E) -> (StatusCode, String) {
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
}

fn default_format() -> String {
    "elf".to_string()
}

#[derive(Serialize)]
struct FlashResponse {
    flashed: bool,
    chip: String,
}

async fn flash_handler(
    State(state): State<AppState>,
    Json(req): Json<FlashRequest>,
) -> Result<Json<FlashResponse>, (StatusCode, String)> {
    let _guard = state.hw_lock.lock().await;

    let chip = req.chip.clone();
    let path = PathBuf::from(req.firmware_path);
    let format = req.format;

    tokio::task::spawn_blocking(move || hardware::flash(&chip, &path, &format))
        .await
        .map_err(internal_err)?
        .map_err(internal_err)?;

    Ok(Json(FlashResponse {
        flashed: true,
        chip: req.chip,
    }))
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
