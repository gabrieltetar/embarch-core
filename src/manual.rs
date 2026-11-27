//! The routes of decision 147: a manual verdict, a unit's declared serial and board revision, and
//! the person's name, each an entry in the run log. The serial and the name are the person's on
//! every Core (`token_store::DECLARATION_ROUTES`); a manual verdict is taken from either token and
//! the agent's is marked `relayed`.

use axum::{
    extract::{Path as UrlPath, State},
    http::StatusCode,
    Json,
};
use serde::Deserialize;

use crate::api::AppState;
use crate::run_log::RunLog;
use crate::token_store::Scope;

type Reply = Result<Json<serde_json::Value>, (StatusCode, String)>;

/// What a manual verdict may say.
const VERDICTS: &[&str] = &["pass", "fail", "inconclusive"];
/// The longest unit key, serial, revision or name Core keeps.
const MAX_FIELD_BYTES: usize = 200;

fn internal(e: anyhow::Error) -> (StatusCode, String) {
    (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}"))
}

fn bad(msg: String) -> (StatusCode, String) {
    (StatusCode::BAD_REQUEST, msg)
}

/// File and log work off the async threads.
async fn blocking<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> Result<T, (StatusCode, String)> {
    tokio::task::spawn_blocking(f).await.map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))
}

/// A required text field: not blank, no longer than [`MAX_FIELD_BYTES`].
fn required(name: &str, v: &str) -> Result<(), (StatusCode, String)> {
    if v.trim().is_empty() || v.len() > MAX_FIELD_BYTES {
        return Err(bad(format!("{name} must be 1 to {MAX_FIELD_BYTES} bytes and not blank")));
    }
    Ok(())
}

fn refuse_agent(scope: Scope) -> Result<(), (StatusCode, String)> {
    if scope == Scope::Agent {
        return Err((StatusCode::FORBIDDEN, crate::token_store::DECLARATION_REFUSAL.into()));
    }
    Ok(())
}

impl RunLog {
    /// A verdict a person or an agent entered by hand (decision 147). `relayed` is true for the
    /// agent's token: it never counts in a requirement's verdict or in coverage.
    #[allow(clippy::too_many_arguments)]
    pub fn manual_verdict(&self, test_id: &str, definition_hash: &str, unit_key: &str, dut_image: Option<&str>, verdict: &str, reason: &str, note: Option<&str>, scope: Scope) -> anyhow::Result<serde_json::Value> {
        self.append(serde_json::json!({
            "type": "manual_verdict", "test_id": test_id, "definition_hash": definition_hash,
            "unit_key": unit_key, "dut_image": dut_image, "verdict": verdict, "reason": reason, "note": note,
            "scope": scope.as_str(), "relayed": scope == Scope::Agent, "at_utc_ms": crate::study::current_utc_ms(),
        }))
    }

    /// The serial and board revision the person declared for a unit (decision 147).
    pub fn unit_declared(&self, unit_key: &str, serial: &str, board_revision: &str) -> anyhow::Result<serde_json::Value> {
        self.append(serde_json::json!({
            "type": "unit_declared", "unit_key": unit_key, "serial": serial, "board_revision": board_revision,
            "scope": Scope::Person.as_str(), "declared": true, "at_utc_ms": crate::study::current_utc_ms(),
        }))
    }

    /// The person's name (decision 147).
    pub fn person_declared(&self, name: &str) -> anyhow::Result<serde_json::Value> {
        self.append(serde_json::json!({
            "type": "person_declared", "name": name,
            "scope": Scope::Person.as_str(), "declared": true, "at_utc_ms": crate::study::current_utc_ms(),
        }))
    }

    /// The latest entry of `kind` (and, for a unit, of that key), as `{entry, hash}`.
    fn latest_declared(&self, kind: &str, unit_key: Option<&str>) -> anyhow::Result<Option<serde_json::Value>> {
        Ok(self.lines()?.into_iter().rev().find(|l| l["entry"]["type"] == kind && unit_key.is_none_or(|k| l["entry"]["unit_key"] == k)))
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManualVerdictRequest {
    test_id: String,
    /// Lowercase hex SHA-256 of the test definition the verdict is for.
    definition_hash: String,
    unit_key: String,
    #[serde(default)]
    dut_image: Option<String>,
    /// `pass`, `fail` or `inconclusive`.
    verdict: String,
    reason: String,
    #[serde(default)]
    note: Option<String>,
}

/// Enters a manual verdict in the run log with the token's scope; the agent's is `relayed`.
// route: POST /manual-verdicts
pub async fn manual_verdict_handler(State(state): State<AppState>, scope: Scope, Json(req): Json<ManualVerdictRequest>) -> Reply {
    required("test_id", &req.test_id)?;
    required("unit_key", &req.unit_key)?;
    required("reason", &req.reason)?;
    if !crate::definitions::is_hash(&req.definition_hash) {
        return Err(bad("definition_hash must be 64 lowercase hex characters".into()));
    }
    if !VERDICTS.contains(&req.verdict.as_str()) {
        return Err(bad(format!("verdict must be one of {}", VERDICTS.join(", "))));
    }
    let log = state.run_log.clone();
    blocking(move || log.manual_verdict(&req.test_id, &req.definition_hash, &req.unit_key, req.dut_image.as_deref(), &req.verdict, &req.reason, req.note.as_deref(), scope).map(Json).map_err(internal)).await?
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UnitRequest {
    serial: String,
    board_revision: String,
}

/// Keeps a unit's declared serial and board revision; the person's token only.
// route: PUT /units/{unit_key}
pub async fn put_unit_handler(State(state): State<AppState>, UrlPath(unit_key): UrlPath<String>, scope: Scope, Json(req): Json<UnitRequest>) -> Reply {
    refuse_agent(scope)?;
    required("unit_key", &unit_key)?;
    required("serial", &req.serial)?;
    required("board_revision", &req.board_revision)?;
    let log = state.run_log.clone();
    blocking(move || log.unit_declared(&unit_key, &req.serial, &req.board_revision).map(Json).map_err(internal)).await?
}

/// The latest declaration of a unit; `404` when none.
// route: GET /units/{unit_key}
pub async fn get_unit_handler(State(state): State<AppState>, UrlPath(unit_key): UrlPath<String>) -> Reply {
    let log = state.run_log.clone();
    let key = unit_key.clone();
    match blocking(move || log.latest_declared("unit_declared", Some(&key))).await?.map_err(internal)? {
        Some(l) => Ok(Json(l)),
        None => Err((StatusCode::NOT_FOUND, format!("no serial is declared for unit {unit_key}"))),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PersonRequest {
    name: String,
}

/// Keeps the person's name; the person's token only.
// route: PUT /person
pub async fn put_person_handler(State(state): State<AppState>, scope: Scope, Json(req): Json<PersonRequest>) -> Reply {
    refuse_agent(scope)?;
    required("name", &req.name)?;
    let log = state.run_log.clone();
    blocking(move || log.person_declared(&req.name).map(Json).map_err(internal)).await?
}

/// The person's declared name; `404` when none.
// route: GET /person
pub async fn get_person_handler(State(state): State<AppState>) -> Reply {
    let log = state.run_log.clone();
    match blocking(move || log.latest_declared("person_declared", None)).await?.map_err(internal)? {
        Some(l) => Ok(Json(l)),
        None => Err((StatusCode::NOT_FOUND, "no name is declared".into())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{body::Body, http::Request};
    use tower::ServiceExt;

    const HASH: &str = "ab00ab00ab00ab00ab00ab00ab00ab00ab00ab00ab00ab00ab00ab00ab00ab00";

    async fn call(router: &axum::Router, bearer: &str, method: &str, uri: &str, body: &str) -> (StatusCode, serde_json::Value) {
        let r = router
            .clone()
            .oneshot(Request::builder().method(method).uri(uri).header("authorization", format!("Bearer {bearer}")).header("content-type", "application/json").body(Body::from(body.to_string())).unwrap())
            .await
            .unwrap();
        let status = r.status();
        let bytes = axum::body::to_bytes(r.into_body(), 1 << 20).await.unwrap();
        (status, serde_json::from_slice(&bytes).unwrap_or_else(|_| serde_json::Value::String(String::from_utf8_lossy(&bytes).into_owned())))
    }

    fn verdict_body(verdict: &str) -> String {
        format!(r#"{{"test_id":"t1","definition_hash":"{HASH}","unit_key":"DUT-1","dut_image":"holds","verdict":"{verdict}","reason":"checked by eye","note":"n"}}"#)
    }

    #[tokio::test]
    async fn a_manual_verdict_is_relayed_with_the_agents_token_and_not_with_the_persons() {
        let state = AppState::new("test-token".to_string());
        let router = crate::api::build_router(state.clone());
        let (s, a) = call(&router, "test-token", "POST", "/manual-verdicts", &verdict_body("pass")).await;
        assert_eq!(s, StatusCode::OK, "{a}");
        assert_eq!((a["entry"]["scope"].as_str(), a["entry"]["relayed"].as_bool()), (Some("agent"), Some(true)));
        let (s, p) = call(&router, "person-test-token", "POST", "/manual-verdicts", &verdict_body("fail")).await;
        assert_eq!(s, StatusCode::OK, "{p}");
        assert_eq!((p["entry"]["scope"].as_str(), p["entry"]["relayed"].as_bool(), p["entry"]["verdict"].as_str()), (Some("person"), Some(false), Some("fail")));
        assert_eq!(call(&router, "test-token", "POST", "/manual-verdicts", &verdict_body("maybe")).await.0, StatusCode::BAD_REQUEST);
        assert_eq!(call(&router, "test-token", "POST", "/manual-verdicts", &verdict_body("pass").replace(HASH, "xyz")).await.0, StatusCode::BAD_REQUEST);
        assert!(state.run_log.verify().unwrap().is_ok(), "the chain recomputes over the new entries");
    }

    #[tokio::test]
    async fn a_serial_and_a_name_are_the_persons_and_read_back() {
        for service in [true, false] {
            let mut state = AppState::new("test-token".to_string());
            state.person_only = service;
            let router = crate::api::build_router(state.clone());
            let unit = r#"{"serial":"SN-0042","board_revision":"B"}"#;
            let name = r#"{"name":"Ada"}"#;
            assert_eq!(call(&router, "test-token", "PUT", "/units/DUT-1", unit).await.0, StatusCode::FORBIDDEN, "service {service}");
            assert_eq!(call(&router, "test-token", "PUT", "/person", name).await.0, StatusCode::FORBIDDEN, "service {service}");
            assert_eq!(call(&router, "test-token", "GET", "/units/DUT-1", "").await.0, StatusCode::NOT_FOUND);
            assert_eq!(call(&router, "test-token", "GET", "/person", "").await.0, StatusCode::NOT_FOUND);
            assert_eq!(call(&router, "person-test-token", "PUT", "/units/DUT-1", unit).await.0, StatusCode::OK);
            assert_eq!(call(&router, "person-test-token", "PUT", "/person", name).await.0, StatusCode::OK);
            let (_, u) = call(&router, "test-token", "GET", "/units/DUT-1", "").await;
            assert_eq!((u["entry"]["serial"].as_str(), u["entry"]["board_revision"].as_str(), u["entry"]["declared"].as_bool()), (Some("SN-0042"), Some("B"), Some(true)));
            let (_, p) = call(&router, "test-token", "GET", "/person", "").await;
            assert_eq!((p["entry"]["name"].as_str(), p["entry"]["declared"].as_bool()), (Some("Ada"), Some(true)));
            assert_eq!(call(&router, "test-token", "GET", "/units/DUT-2", "").await.0, StatusCode::NOT_FOUND);
            // A later declaration replaces the read-back; the chain keeps both.
            call(&router, "person-test-token", "PUT", "/person", r#"{"name":"Grace"}"#).await;
            assert_eq!(call(&router, "test-token", "GET", "/person", "").await.1["entry"]["name"], "Grace");
            assert_eq!(state.run_log.verify().unwrap().unwrap().1, 3);
        }
    }
}
