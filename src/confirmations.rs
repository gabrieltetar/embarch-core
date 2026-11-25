//! The routes of decision 133: `POST /definitions`, `GET /definitions/{hash}`,
//! `GET /suites`, `GET /confirmations/pending`, a card's data with its nonce,
//! and the confirmation itself. The logic is in `definitions.rs` and the run
//! log; these are the HTTP seams. The card and the confirmation are person-only
//! on every Core (`token_store::CONFIRMATION_ROUTES`).

use axum::{
    extract::{Path as UrlPath, State},
    http::StatusCode,
    Json,
};
use serde::{Deserialize, Serialize};

use crate::api::AppState;
use crate::definitions::{self, DefinitionRequest, Standing};
use crate::token_store::Scope;

type Reply = Result<(StatusCode, Json<serde_json::Value>), (StatusCode, String)>;

fn internal(e: anyhow::Error) -> (StatusCode, String) {
    (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}"))
}

/// File and log work off the async threads; the scan of a suite checkout can be long.
async fn blocking<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> Result<T, (StatusCode, String)> {
    tokio::task::spawn_blocking(f).await.map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))
}

/// A short name for a listing: the test id, the suite id and version, the capability or hook name.
fn name_of(kind: &str, def: &serde_json::Value) -> String {
    let s = |k: &str| def[k].as_str().unwrap_or("").to_string();
    match kind {
        "test" => s("test_id"),
        "suite" => format!("{}@{}", s("id"), s("version")),
        "capability" => s("name"),
        "hook" => s("hook"),
        _ => String::new(),
    }
}

/// Checks, hashes and keeps a definition; `201` when new, `200` when Core already held it.
// route: POST /definitions
pub async fn post_definition_handler(State(state): State<AppState>, Json(req): Json<DefinitionRequest>) -> Reply {
    let checked = definitions::check(req).map_err(|e| (StatusCode::UNPROCESSABLE_ENTITY, e.0))?;
    let (defs, log) = (state.definitions.clone(), state.run_log.clone());
    blocking(move || -> Result<_, (StatusCode, String)> {
        let new = defs.keep(&checked).map_err(internal)?;
        let confirmed = log.confirmed_hashes().map_err(internal)?;
        let standing = defs.standing(checked.kind.as_str(), &checked.hash, &confirmed);
        let status = if new { StatusCode::CREATED } else { StatusCode::OK };
        Ok((status, Json(serde_json::json!({
            "hash": checked.hash, "kind": checked.kind.as_str(), "kept": new,
            "checked": checked.checked, "confirmation": standing,
        }))))
    })
    .await?
}

/// A kept definition and where it stands.
// route: GET /definitions/{hash}
pub async fn get_definition_handler(State(state): State<AppState>, UrlPath(hash): UrlPath<String>) -> Reply {
    let (defs, log) = (state.definitions.clone(), state.run_log.clone());
    blocking(move || -> Result<_, (StatusCode, String)> {
        let stored = defs.get(&hash).map_err(internal)?.ok_or((StatusCode::NOT_FOUND, format!("Core holds no definition {hash}")))?;
        let confirmed = log.confirmed_hashes().map_err(internal)?;
        let standing = defs.standing(stored["kind"].as_str().unwrap_or(""), &hash, &confirmed);
        Ok((StatusCode::OK, Json(serde_json::json!({
            "hash": hash, "kind": stored["kind"], "definition": stored["definition"], "confirmation": standing,
        }))))
    })
    .await?
}

/// The premade suites Core reads from `EMBARCH_SUITES_DIR`.
// route: GET /suites
pub async fn suites_handler(State(state): State<AppState>) -> Reply {
    let defs = state.definitions.clone();
    blocking(move || {
        let (suites, problems) = defs.suites();
        (StatusCode::OK, Json(serde_json::json!({ "configured": defs.suites_configured(), "suites": suites, "problems": problems })))
    })
    .await
}

#[derive(Serialize)]
struct PendingItem {
    hash: String,
    kind: String,
    name: String,
}

/// What waits for a confirmation: every kept definition that is neither premade
/// nor confirmed. No card data and no nonce: either token may list.
// route: GET /confirmations/pending
pub async fn pending_handler(State(state): State<AppState>) -> Reply {
    let (defs, log) = (state.definitions.clone(), state.run_log.clone());
    blocking(move || -> Result<_, (StatusCode, String)> {
        let confirmed = log.confirmed_hashes().map_err(internal)?;
        let mut pending = Vec::new();
        for (hash, kind) in defs.all().map_err(internal)? {
            if defs.standing(&kind, &hash, &confirmed) == Standing::Unconfirmed {
                let def = defs.get(&hash).map_err(internal)?.map(|v| v["definition"].clone()).unwrap_or_default();
                pending.push(PendingItem { name: name_of(&kind, &def), hash, kind });
            }
        }
        Ok((StatusCode::OK, Json(serde_json::json!({ "pending": pending }))))
    })
    .await?
}

/// A card's data and, for a definition still waiting, a nonce for this
/// rendering: single use, ten minutes. The person's token only, on every Core.
// route: GET /confirmations/{hash}/card
pub async fn card_handler(State(state): State<AppState>, UrlPath(hash): UrlPath<String>, scope: Scope) -> Reply {
    if scope != Scope::Person {
        return Err((StatusCode::FORBIDDEN, crate::token_store::CONFIRMATION_REFUSAL.into()));
    }
    let (defs, log) = (state.definitions.clone(), state.run_log.clone());
    blocking(move || -> Result<_, (StatusCode, String)> {
        let stored = defs.get(&hash).map_err(internal)?.ok_or((StatusCode::NOT_FOUND, format!("Core holds no definition {hash}")))?;
        let confirmed = log.confirmed_hashes().map_err(internal)?;
        let standing = defs.standing(stored["kind"].as_str().unwrap_or(""), &hash, &confirmed);
        let now = crate::study::current_utc_ms();
        let (nonce, expires) = if standing == Standing::Unconfirmed {
            (Some(defs.issue_nonce(&hash, now)), Some(now + definitions::NONCE_TTL_MS))
        } else {
            (None, None)
        };
        Ok((StatusCode::OK, Json(serde_json::json!({
            "card": defs.card(&hash, &stored), "confirmation": standing, "nonce": nonce, "expires_utc_ms": expires,
        }))))
    })
    .await?
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfirmRequest {
    hash: String,
    nonce: String,
    /// A label for where the person acted (`ui`, `cli`); recorded, never trusted.
    #[serde(default)]
    surface: Option<String>,
}

/// Binds the hash of exactly what the card showed, with the nonce issued for
/// it, as an entry in the run log. The nonce is spent by the attempt.
// route: POST /confirmations
pub async fn confirm_handler(State(state): State<AppState>, scope: Scope, Json(req): Json<ConfirmRequest>) -> Reply {
    if scope != Scope::Person {
        return Err((StatusCode::FORBIDDEN, crate::token_store::CONFIRMATION_REFUSAL.into()));
    }
    if !definitions::is_hash(&req.hash) {
        return Err((StatusCode::BAD_REQUEST, "hash must be 64 lowercase hex characters".into()));
    }
    let (defs, log) = (state.definitions.clone(), state.run_log.clone());
    blocking(move || -> Result<_, (StatusCode, String)> {
        defs.spend_nonce(&req.nonce, &req.hash, crate::study::current_utc_ms()).map_err(|why| (StatusCode::FORBIDDEN, why.to_string()))?;
        let stored = defs.get(&req.hash).map_err(internal)?.ok_or((StatusCode::NOT_FOUND, format!("Core holds no definition {}", req.hash)))?;
        let kind = stored["kind"].as_str().unwrap_or("").to_string();
        let confirmed = log.confirmed_hashes().map_err(internal)?;
        match defs.standing(&kind, &req.hash, &confirmed) {
            Standing::Unconfirmed => {}
            other => return Err((StatusCode::CONFLICT, format!("this definition needs no confirmation: it stands as {}", serde_json::to_string(&other).unwrap_or_default().trim_matches('"')))),
        }
        // A suite Core does not hold is always the case here: a held one stands as `held`.
        let holds = (kind == "suite").then_some(false);
        let entry = log.confirm(&kind, &req.hash, scope, req.surface.as_deref().unwrap_or("unstated"), holds).map_err(internal)?;
        Ok((StatusCode::CREATED, Json(entry)))
    })
    .await?
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::build_router;
    use crate::definitions::tests::checkout;
    use axum::{body::Body, http::Request};
    use tower::ServiceExt as _;

    const TEST_JSON: &str = include_str!("../tests/fixtures/hil-tests/test.json");
    const AGENT: &str = "test-token";
    const PERSON: &str = "person-test-token";

    fn state(suites: Option<std::path::PathBuf>) -> AppState {
        let mut s = AppState::new(AGENT.to_string());
        s.definitions = definitions::Definitions::new(s.run_log.data_dir(), suites);
        s
    }

    async fn call(s: &AppState, bearer: &str, method: &str, uri: &str, body: Option<serde_json::Value>) -> (StatusCode, serde_json::Value) {
        let mut b = Request::builder().method(method).uri(uri).header("authorization", format!("Bearer {bearer}"));
        let body = match body {
            Some(v) => {
                b = b.header("content-type", "application/json");
                Body::from(v.to_string())
            }
            None => Body::empty(),
        };
        let r = build_router(s.clone()).oneshot(b.body(body).unwrap()).await.unwrap();
        let status = r.status();
        let bytes = axum::body::to_bytes(r.into_body(), 1 << 20).await.unwrap();
        let text = String::from_utf8_lossy(&bytes).into_owned();
        (status, serde_json::from_str(&text).unwrap_or(serde_json::Value::String(text)))
    }

    fn def(title: &str) -> serde_json::Value {
        let mut v: serde_json::Value = serde_json::from_str(TEST_JSON).unwrap();
        v["title"] = title.into();
        serde_json::json!({ "kind": "test", "definition": v })
    }

    #[tokio::test]
    async fn a_definition_posted_twice_is_kept_once_and_an_invalid_one_is_refused() {
        let s = state(None);
        let (st, a) = call(&s, AGENT, "POST", "/definitions", Some(def("t"))).await;
        assert_eq!(st, StatusCode::CREATED, "{a}");
        assert_eq!((a["kept"].clone(), a["confirmation"].clone()), (true.into(), "unconfirmed".into()));
        let (st, b) = call(&s, PERSON, "POST", "/definitions", Some(def("t"))).await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!((b["hash"].clone(), b["kept"].clone()), (a["hash"].clone(), false.into()));
        let hash = a["hash"].as_str().unwrap();
        let (st, got) = call(&s, AGENT, "GET", &format!("/definitions/{hash}"), None).await;
        assert_eq!((st, got["kind"].clone()), (StatusCode::OK, "test".into()));
        assert_eq!(call(&s, AGENT, "GET", &format!("/definitions/{}", "0".repeat(64)), None).await.0, StatusCode::NOT_FOUND);
        assert_eq!(call(&s, AGENT, "GET", "/definitions/..%2Fx", None).await.0, StatusCode::NOT_FOUND);

        let mut bad: serde_json::Value = serde_json::from_str(TEST_JSON).unwrap();
        bad["run_count"] = 0.into();
        let (st, why) = call(&s, AGENT, "POST", "/definitions", Some(serde_json::json!({ "kind": "test", "definition": bad }))).await;
        assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY);
        assert!(why.as_str().unwrap().contains("RunCount"), "{why}");
        assert_eq!(s.definitions.all().unwrap().len(), 1, "a refused one is not kept");
    }

    #[tokio::test]
    async fn the_agent_cannot_obtain_a_nonce_or_confirm_on_any_core_and_the_person_confirms_once() {
        for service in [false, true] {
            let mut s = state(None);
            s.person_only = service;
            let (_, posted) = call(&s, AGENT, "POST", "/definitions", Some(def("edited"))).await;
            let hash = posted["hash"].as_str().unwrap().to_string();

            let (st, pend) = call(&s, AGENT, "GET", "/confirmations/pending", None).await;
            assert_eq!(st, StatusCode::OK);
            assert_eq!(pend["pending"][0]["hash"], hash.as_str());
            assert!(pend["pending"][0].get("nonce").is_none(), "the listing carries no nonce");

            // The agent: no card, no nonce, no confirmation, even on a development Core.
            let (st, body) = call(&s, AGENT, "GET", &format!("/confirmations/{hash}/card"), None).await;
            assert_eq!(st, StatusCode::FORBIDDEN, "{body}");
            let (st, _) = call(&s, AGENT, "POST", "/confirmations", Some(serde_json::json!({ "hash": hash, "nonce": "x" }))).await;
            assert_eq!(st, StatusCode::FORBIDDEN);
            assert!(s.run_log.confirmed_hashes().unwrap().is_empty());

            // The person: a card with a nonce, then a confirmation once.
            let (st, card) = call(&s, PERSON, "GET", &format!("/confirmations/{hash}/card"), None).await;
            assert_eq!(st, StatusCode::OK, "{card}");
            assert_eq!(card["card"]["definition"]["title"], "edited");
            let nonce = card["nonce"].as_str().unwrap().to_string();
            let ask = serde_json::json!({ "hash": hash, "nonce": nonce, "surface": "ui" });
            let (st, entry) = call(&s, PERSON, "POST", "/confirmations", Some(ask.clone())).await;
            assert_eq!(st, StatusCode::CREATED, "{entry}");
            assert_eq!(entry["entry"]["type"], "confirmed");
            assert_eq!(
                (entry["entry"]["sha256"].clone(), entry["entry"]["scope"].clone(), entry["entry"]["surface"].clone(), entry["entry"]["kind"].clone()),
                (hash.clone().into(), "person".into(), "ui".into(), "test".into())
            );
            let (st, _) = call(&s, PERSON, "POST", "/confirmations", Some(ask)).await;
            assert_eq!(st, StatusCode::FORBIDDEN, "the same nonce again is refused");
            let (_, log) = call(&s, PERSON, "GET", "/run-log", None).await;
            assert_eq!((log["verified"].clone(), log["length"].clone()), (true.into(), 1.into()));

            // Confirmed now; and an edit is a new hash that reads unconfirmed.
            let (_, got) = call(&s, AGENT, "GET", &format!("/definitions/{hash}"), None).await;
            assert_eq!(got["confirmation"], "confirmed");
            let (_, edited) = call(&s, AGENT, "POST", "/definitions", Some(def("edited again"))).await;
            assert_ne!(edited["hash"], posted["hash"]);
            assert_eq!(edited["confirmation"], "unconfirmed");
            let (_, pend) = call(&s, AGENT, "GET", "/confirmations/pending", None).await;
            assert_eq!(pend["pending"].as_array().unwrap().len(), 1);
            // A confirmed one gets a card and no nonce.
            let (_, card) = call(&s, PERSON, "GET", &format!("/confirmations/{hash}/card"), None).await;
            assert!(card["nonce"].is_null());
        }
    }

    #[tokio::test]
    async fn a_nonce_is_for_one_card() {
        let s = state(None);
        let (_, a) = call(&s, AGENT, "POST", "/definitions", Some(def("a"))).await;
        let (_, b) = call(&s, AGENT, "POST", "/definitions", Some(def("b"))).await;
        let (_, card) = call(&s, PERSON, "GET", &format!("/confirmations/{}/card", a["hash"].as_str().unwrap()), None).await;
        let (st, _) = call(&s, PERSON, "POST", "/confirmations", Some(serde_json::json!({ "hash": b["hash"], "nonce": card["nonce"] }))).await;
        assert_eq!(st, StatusCode::FORBIDDEN);
        assert!(s.run_log.confirmed_hashes().unwrap().is_empty());
        let (st, _) = call(&s, PERSON, "POST", "/confirmations", Some(serde_json::json!({ "hash": "zz", "nonce": "n" }))).await;
        assert_eq!(st, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn a_premade_test_needs_none_and_one_byte_changed_needs_one() {
        let s = state(Some(checkout("api-premade")));
        let (_, suites) = call(&s, AGENT, "GET", "/suites", None).await;
        assert_eq!((suites["configured"].clone(), suites["suites"][0]["id"].clone()), (true.into(), "usb-pd".into()));
        let (_, same) = call(&s, AGENT, "POST", "/definitions", Some(def("Source holds 5 V"))).await;
        let original: serde_json::Value = serde_json::from_str::<serde_json::Value>(TEST_JSON).unwrap()["title"].clone();
        let (_, copy) = call(&s, AGENT, "POST", "/definitions", Some(def(original.as_str().unwrap()))).await;
        assert_eq!(copy["confirmation"], "premade", "identical to the checkout's copy");
        assert_eq!(same["confirmation"], "unconfirmed", "a changed title is a changed definition");
        let (_, pend) = call(&s, AGENT, "GET", "/confirmations/pending", None).await;
        assert_eq!(pend["pending"].as_array().unwrap().len(), 1);
        assert_eq!(pend["pending"][0]["hash"], same["hash"]);
        // The premade one is refused a confirmation as needless, after its nonce is spent.
        let (_, card) = call(&s, PERSON, "GET", &format!("/confirmations/{}/card", copy["hash"].as_str().unwrap()), None).await;
        assert!(card["nonce"].is_null());
    }

    #[tokio::test]
    async fn a_suite_core_does_not_hold_is_confirmed_once_as_a_unit() {
        let s = state(Some(checkout("api-unheld")));
        let t: embarch_study_designer::hil_test::TestDef = serde_json::from_str(TEST_JSON).unwrap();
        let mut edited = t.clone();
        edited.title = "pinned elsewhere".into();
        let mut manifest: embarch_study_designer::hil_test::SuiteManifest =
            serde_json::from_str(include_str!("../tests/fixtures/hil-tests/suite.json")).unwrap();
        manifest.id = "usb-pd".into();
        manifest.version = "0.9.0".into();
        manifest.tests = vec![embarch_study_designer::hil_test::SuiteTest { test_id: edited.test_id.clone(), file: "test.json".into(), hash: edited.hash() }];
        let (st, suite) = call(&s, AGENT, "POST", "/definitions", Some(serde_json::json!({ "kind": "suite", "definition": manifest }))).await;
        assert_eq!((st, suite["confirmation"].clone()), (StatusCode::CREATED, "unconfirmed".into()), "{suite}");
        let (_, test) = call(&s, AGENT, "POST", "/definitions", Some(serde_json::json!({ "kind": "test", "definition": edited }))).await;
        assert_eq!(test["confirmation"], "unconfirmed");
        let hash = suite["hash"].as_str().unwrap();
        let (_, card) = call(&s, PERSON, "GET", &format!("/confirmations/{hash}/card"), None).await;
        let (st, entry) = call(&s, PERSON, "POST", "/confirmations", Some(serde_json::json!({ "hash": hash, "nonce": card["nonce"] }))).await;
        assert_eq!(st, StatusCode::CREATED, "{entry}");
        assert_eq!((entry["entry"]["kind"].clone(), entry["entry"]["core_holds"].clone()), ("suite".into(), false.into()));
        let (_, test) = call(&s, AGENT, "GET", &format!("/definitions/{}", test["hash"].as_str().unwrap()), None).await;
        assert_eq!(test["confirmation"], "confirmed_with_suite");
        let (_, pend) = call(&s, AGENT, "GET", "/confirmations/pending", None).await;
        assert!(pend["pending"].as_array().unwrap().is_empty(), "{pend}");
    }
}
