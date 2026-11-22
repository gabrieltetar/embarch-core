//! Core's Ed25519 key, the DSSE-wrapped in-toto Statement it signs for each
//! study run (`record.dsse.json`), and the one other thing the key signs: the
//! fixed-frame challenge (decision 110).
//!
//! Nothing here signs bytes a caller supplies. A run record is built from files
//! Core wrote and facts Core observed; the challenge signs a frame whose only
//! caller-controlled part is a hex nonce.

use anyhow::{Context, Result};
use axum::{extract::State, http::StatusCode, routing::post, Json, Router};
use base64::Engine as _;
use ed25519_dalek::{Signer as _, SigningKey};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex as StdMutex};

use crate::api::AppState;
use crate::token_store::Scope;

/// The DSSE `payloadType` of an in-toto Statement.
pub const PAYLOAD_TYPE: &str = "application/vnd.in-toto+json";
const STATEMENT_TYPE: &str = "https://in-toto.io/Statement/v1";
/// The Statement's `predicateType` (decision 110): a URN, since it names a
/// shape rather than a page.
pub const PREDICATE_TYPE: &str = "urn:embarch:predicate:study-run:v1";
/// The envelope's file name inside a study's results directory.
pub const RECORD_FILE: &str = "record.dsse.json";
/// The first line of the one frame the challenge route signs.
pub const CHALLENGE_TAG: &str = "embarch-core-auth-v1";

/// The key file, beside the token files (`token_store::local_data_dir`).
pub fn signing_key_path() -> Result<PathBuf> {
    Ok(crate::token_store::local_data_dir()?.join("signing-key"))
}

// ---- the key ----------------------------------------------------------------

/// Core's signing key and its fingerprint.
pub struct CoreKey {
    key: SigningKey,
    keyid: String,
}

impl CoreKey {
    fn from_seed(seed: [u8; 32]) -> Self {
        let key = SigningKey::from_bytes(&seed);
        let keyid = hex(&Sha256::digest(key.verifying_key().as_bytes()));
        Self { key, keyid }
    }

    /// A key that is never written down: tests, and nothing else.
    #[cfg(test)]
    pub fn ephemeral() -> Self {
        let mut seed = [0u8; 32];
        rand::fill(&mut seed);
        Self::from_seed(seed)
    }

    /// Reads the key at `path`, or generates and stores one there on first use.
    /// The file holds the 32-byte seed as 64 hex characters, like the tokens,
    /// `0600` on Linux and the `icacls` lockdown on Windows (decision 53's
    /// treatment of the token file, unvalidated on Windows for this file).
    pub fn load_or_create(path: &Path) -> Result<Self> {
        if path.exists() {
            let text = std::fs::read_to_string(path)
                .with_context(|| format!("failed to read the signing key at {}", path.display()))?;
            let bytes = unhex(text.trim())
                .filter(|b| b.len() == 32)
                .with_context(|| format!("the signing key at {} is not 64 hex characters", path.display()))?;
            let mut seed = [0u8; 32];
            seed.copy_from_slice(&bytes);
            tracing::info!(path = %path.display(), "reusing the existing signing key");
            return Ok(Self::from_seed(seed));
        }
        let mut seed = [0u8; 32];
        rand::fill(&mut seed);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("failed to create directory {}", parent.display()))?;
        }
        write_new_private(path, hex(&seed).as_bytes())
            .with_context(|| format!("failed to write the signing key at {}", path.display()))?;
        #[cfg(windows)]
        crate::token_store::restrict_token_file_permissions(path)?;
        tracing::info!(path = %path.display(), "generated a new signing key");
        Ok(Self::from_seed(seed))
    }

    /// SHA-256 of the raw public key, as 64 lowercase hex: the DSSE `keyid`
    /// and the fingerprint a client compares.
    pub fn keyid(&self) -> &str {
        &self.keyid
    }

    /// The raw 32-byte public key, as 64 lowercase hex.
    pub fn public_key_hex(&self) -> String {
        hex(self.key.verifying_key().as_bytes())
    }

    fn sign(&self, message: &[u8]) -> [u8; 64] {
        self.key.sign(message).to_bytes()
    }
}

/// Creates `path` (refusing an existing file) with owner-only permissions from
/// the start where the OS has a mode, so the seed is never briefly world-readable.
fn write_new_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write as _;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)?.write_all(bytes)
}

pub(crate) fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn unhex(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) || !s.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)) {
        return None;
    }
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok()).collect()
}

// ---- DSSE -------------------------------------------------------------------

/// The DSSE v1 pre-authentication encoding: what is signed is this, never the
/// payload alone.
fn pae(payload_type: &str, payload: &[u8]) -> Vec<u8> {
    let mut out = format!("DSSEv1 {} {} {} ", payload_type.len(), payload_type, payload.len()).into_bytes();
    out.extend_from_slice(payload);
    out
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Envelope {
    #[serde(rename = "payloadType")]
    pub payload_type: String,
    /// The Statement, standard base64.
    pub payload: String,
    pub signatures: Vec<EnvelopeSignature>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct EnvelopeSignature {
    pub keyid: String,
    /// Standard base64 of the 64-byte Ed25519 signature.
    pub sig: String,
}

fn b64(bytes: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

fn sign_envelope(key: &CoreKey, statement: &[u8]) -> Envelope {
    Envelope {
        payload_type: PAYLOAD_TYPE.to_string(),
        payload: b64(statement),
        signatures: vec![EnvelopeSignature {
            keyid: key.keyid().to_string(),
            sig: b64(&key.sign(&pae(PAYLOAD_TYPE, statement))),
        }],
    }
}

// ---- what Core was told about the build -------------------------------------

/// The build facts embarch-api carries to Core as query parameters on
/// `POST /study` and `POST /host-studies`. Core cannot check any of them, so
/// they are recorded under `claimed`, never `observed`.
#[derive(Debug, Default, Clone, Deserialize)]
#[serde(default)]
pub struct BuildClaimParams {
    pub build_commit: Option<String>,
    /// `true` or `false`.
    pub build_dirty: Option<String>,
    pub build_toolchain: Option<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct BuildClaims {
    pub commit: Option<String>,
    pub dirty: Option<bool>,
    pub toolchain: Option<String>,
}

const MAX_TOOLCHAIN_LEN: usize = 200;

impl BuildClaimParams {
    /// `Ok(None)` when the caller told Core nothing about the build.
    pub fn claims(&self) -> std::result::Result<Option<BuildClaims>, String> {
        if self.build_commit.is_none() && self.build_dirty.is_none() && self.build_toolchain.is_none() {
            return Ok(None);
        }
        if let Some(c) = &self.build_commit {
            if !(7..=64).contains(&c.len()) || unhex_loose(c).is_none() {
                return Err("build_commit is 7 to 64 lowercase hex characters".to_string());
            }
        }
        let dirty = match self.build_dirty.as_deref() {
            None => None,
            Some("true") => Some(true),
            Some("false") => Some(false),
            Some(_) => return Err("build_dirty is `true` or `false`".to_string()),
        };
        if let Some(t) = &self.build_toolchain {
            if t.len() > MAX_TOOLCHAIN_LEN || t.chars().any(char::is_control) {
                return Err(format!("build_toolchain is printable text of at most {MAX_TOOLCHAIN_LEN} bytes"));
            }
        }
        Ok(Some(BuildClaims { commit: self.build_commit.clone(), dirty, toolchain: self.build_toolchain.clone() }))
    }
}

/// Lowercase hex of any length (a commit id may be odd-length when abbreviated).
fn unhex_loose(s: &str) -> Option<()> {
    s.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)).then_some(())
}

// ---- declarations, by the scope that made them ------------------------------

/// What Core saw declared since it started, and by which token's scope. A
/// declaration made before this process started, or by a route this ledger does
/// not watch, is absent: the predicate says from when the ledger runs.
#[derive(Clone)]
pub struct Declarations(Arc<StdMutex<LedgerInner>>);

struct LedgerInner {
    since_utc_ms: u64,
    entries: BTreeMap<(String, String), (Scope, u64)>,
}

impl Declarations {
    pub fn new() -> Self {
        Self(Arc::new(StdMutex::new(LedgerInner { since_utc_ms: crate::study::current_utc_ms(), entries: BTreeMap::new() })))
    }

    /// `what` is one of `role`, `signal`, `instrument`, `bootload_ports`, `dev_bench_link`.
    pub fn record(&self, what: &str, name: &str, scope: Scope) {
        let now = crate::study::current_utc_ms();
        self.0.lock().unwrap().entries.insert((what.to_string(), name.to_string()), (scope, now));
    }

    /// The scope that made the declaration, `None` when this Core did not watch it made.
    pub fn scope_of(&self, what: &str, name: &str) -> Option<Scope> {
        self.0.lock().unwrap().entries.get(&(what.to_string(), name.to_string())).map(|(scope, _)| *scope)
    }

    pub fn forget(&self, what: &str, name: &str) {
        self.0.lock().unwrap().entries.remove(&(what.to_string(), name.to_string()));
    }

    pub fn forget_all(&self, what: &str) {
        self.0.lock().unwrap().entries.retain(|(w, _), _| w != what);
    }

    fn snapshot(&self) -> serde_json::Value {
        let inner = self.0.lock().unwrap();
        let entries: Vec<_> = inner
            .entries
            .iter()
            .map(|((what, name), (scope, at))| {
                serde_json::json!({ "what": what, "name": name, "scope": scope.as_str(), "at_utc_ms": at })
            })
            .collect();
        serde_json::json!({ "witnessed_since_utc_ms": inner.since_utc_ms, "entries": entries })
    }
}

// ---- sealing a run ----------------------------------------------------------

/// The key, the ledger, and the one operation that turns a finished run's
/// directory into a signed record.
#[derive(Clone)]
pub struct Sealer {
    key: Arc<CoreKey>,
    declarations: Declarations,
}

/// What a run hands the sealer besides its directory.
#[derive(Clone)]
pub struct SealJob {
    pub sealer: Sealer,
    /// `device` or `host`.
    pub kind: &'static str,
    pub build: Option<BuildClaims>,
    /// The scope of the token that submitted the study: what a measurement's
    /// `study_value` declaration was made with (decision 113). `None` for a host
    /// study, which declares none.
    pub submitted_by: Option<Scope>,
}

impl Sealer {
    pub fn new(key: CoreKey) -> Self {
        Self { key: Arc::new(key), declarations: Declarations::new() }
    }

    #[cfg(test)]
    pub fn ephemeral() -> Self {
        Self::new(CoreKey::ephemeral())
    }

    /// The key from the store beside the token files, made on first start.
    pub fn resolve() -> Result<Self> {
        let path = signing_key_path()?;
        let key = CoreKey::load_or_create(&path)?;
        tracing::info!(keyid = key.keyid(), "Core signs study run records with this key");
        Ok(Self::new(key))
    }

    pub fn key(&self) -> &CoreKey {
        &self.key
    }

    pub fn declarations(&self) -> &Declarations {
        &self.declarations
    }

    /// A DSSE envelope over `statement` (already serialised), as file bytes.
    pub fn sign_statement(&self, statement: &[u8]) -> Result<Vec<u8>> {
        Ok(serde_json::to_vec(&sign_envelope(&self.key, statement))?)
    }

    /// Writes `record.dsse.json` into `dir`: a DSSE envelope over an in-toto
    /// Statement whose subjects are the files the run has now. The list comes
    /// from the files present, so a file a later unit starts writing
    /// (`measurements.json`) becomes a subject with no change here.
    pub fn seal(&self, job: &SealJob, dir: &Path, study_id: &str, ended_utc_ms: u64) -> Result<PathBuf> {
        let subjects = subjects_of(dir)?;
        anyhow::ensure!(!subjects.is_empty(), "{} has no file to sign", dir.display());

        let events: Option<serde_json::Value> =
            std::fs::read(dir.join("events.json")).ok().and_then(|b| serde_json::from_slice(&b).ok());
        let provenance = events.as_ref().and_then(|e| e.get("provenance")).cloned().unwrap_or(serde_json::Value::Null);
        let behaviours = std::fs::read(dir.join("measurements.json"))
            .ok()
            .and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok())
            .and_then(|m| m.get("behaviours").cloned())
            .unwrap_or_else(|| serde_json::json!({}));

        // `serde_json`'s map is ordered by key, and every number here is an
        // integer, so the serialisation is RFC 8785 canonical (decision 110).
        let statement = serde_json::json!({
            "_type": STATEMENT_TYPE,
            "subject": subjects,
            "predicateType": PREDICATE_TYPE,
            "predicate": {
                "schema": 1,
                "study_id": study_id,
                "kind": job.kind,
                "ended_utc_ms": ended_utc_ms,
                "observed": {
                    "core": { "version": env!("CARGO_PKG_VERSION") },
                    "provenance": provenance,
                    "image_status": "unverified",
                    "declarations": self.declarations.snapshot(),
                    "behaviours": behaviours,
                },
                "claimed": { "told_by": "embarch-api", "build": job.build },
            }
        });
        let bytes = serde_json::to_vec(&statement)?;
        let envelope = serde_json::to_vec(&sign_envelope(&self.key, &bytes))?;

        let path = dir.join(RECORD_FILE);
        let tmp = dir.join(format!("{RECORD_FILE}.partial"));
        std::fs::write(&tmp, envelope).with_context(|| format!("failed to write {}", tmp.display()))?;
        std::fs::rename(&tmp, &path).with_context(|| format!("failed to finalize {}", path.display()))?;
        Ok(path)
    }
}

/// The files a run's record vouches for, by path relative to `dir` with `/`:
/// `events.json`, `measurements.json` and `study.json` when present, and every
/// regular file directly under `streams/`. Sorted, so a record is reproducible.
fn subjects_of(dir: &Path) -> Result<Vec<serde_json::Value>> {
    let mut names: Vec<String> = ["events.json", "measurements.json", "study.json"]
        .iter()
        .filter(|n| dir.join(n).is_file())
        .map(|n| n.to_string())
        .collect();
    let streams = dir.join("streams");
    if streams.is_dir() {
        for entry in std::fs::read_dir(&streams).with_context(|| format!("failed to list {}", streams.display()))? {
            let entry = entry?;
            if entry.file_type()?.is_file() {
                names.push(format!("streams/{}", entry.file_name().to_string_lossy()));
            }
        }
    }
    names.sort();
    names
        .into_iter()
        .map(|name| {
            let digest = sha256_of_file(&dir.join(&name))?;
            Ok(serde_json::json!({ "name": name, "digest": { "sha256": digest } }))
        })
        .collect()
}

pub(crate) fn sha256_of_file(path: &Path) -> Result<String> {
    let mut file = std::fs::File::open(path).with_context(|| format!("failed to open {}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = file.read(&mut buf).with_context(|| format!("failed to read {}", path.display()))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hex(&hasher.finalize()))
}

// ---- the challenge ----------------------------------------------------------

const MIN_NONCE_HEX: usize = 32;
const MAX_NONCE_HEX: usize = 128;

/// The bytes the challenge signs: the tag, Core's nonce, the caller's nonce,
/// one per line. Both nonces are validated hex, so no caller input can add a
/// line; and the tag cannot begin a DSSE pre-authentication string
/// (`DSSEv1 …`), so a signature over a frame is no signature over a record.
fn challenge_frame(core_nonce: &str, caller_nonce: &str) -> String {
    format!("{CHALLENGE_TAG}\n{core_nonce}\n{caller_nonce}")
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ChallengeRequest {
    /// 32 to 128 lowercase hex characters, the caller's own.
    nonce: String,
}

#[derive(Serialize)]
struct ChallengeResponse {
    /// The exact text that was signed.
    frame: String,
    core_nonce: String,
    keyid: String,
    public_key: String,
    /// Lowercase hex of the 64-byte Ed25519 signature over `frame`.
    signature: String,
}

/// The one route that takes no token, so a client can check which Core answers
/// before it sends one. It signs only [`challenge_frame`].
pub fn challenge_router() -> Router<AppState> {
    Router::new().route("/auth/challenge", post(challenge_handler))
}

// route: POST /auth/challenge
async fn challenge_handler(
    State(state): State<AppState>,
    Json(req): Json<ChallengeRequest>,
) -> std::result::Result<Json<ChallengeResponse>, (StatusCode, String)> {
    let n = req.nonce.len();
    if !(MIN_NONCE_HEX..=MAX_NONCE_HEX).contains(&n) || !n.is_multiple_of(2) || unhex(&req.nonce).is_none() {
        return Err((
            StatusCode::BAD_REQUEST,
            format!("nonce is {MIN_NONCE_HEX} to {MAX_NONCE_HEX} lowercase hex characters, an even count"),
        ));
    }
    let mut core_nonce = [0u8; 32];
    rand::fill(&mut core_nonce);
    let core_nonce = hex(&core_nonce);
    let frame = challenge_frame(&core_nonce, &req.nonce);
    let signature = hex(&state.sealer.key().sign(frame.as_bytes()));
    Ok(Json(ChallengeResponse {
        frame,
        core_nonce,
        keyid: state.sealer.key().keyid().to_string(),
        public_key: state.sealer.key().public_key_hex(),
        signature,
    }))
}

// ---- tests ------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use ed25519_dalek::{Signature, Verifier as _, VerifyingKey};
    use tower::ServiceExt as _;

    fn run_dir() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path();
        std::fs::write(p.join("events.json"), br#"{"study_name":"s","steps":[],"provenance":{"dev_bench_version":"1.2.3"},"streams":[]}"#).unwrap();
        std::fs::write(p.join("study.json"), b"{\"name\":\"s\"}").unwrap();
        std::fs::create_dir(p.join("streams")).unwrap();
        std::fs::write(p.join("streams/console.bin"), b"hello\n").unwrap();
        std::fs::write(p.join("streams/index.json"), b"[]").unwrap();
        dir
    }

    fn job(sealer: &Sealer, build: Option<BuildClaims>) -> SealJob {
        SealJob { sealer: sealer.clone(), kind: "device", build, submitted_by: None }
    }

    /// A DSSE check written from the DSSE v1 text alone, not through this
    /// module's `pae`: the way a third-party verifier would run it.
    fn standard_dsse_check(envelope: &Envelope, public_key_hex: &str) -> std::result::Result<Vec<u8>, String> {
        let b = base64::engine::general_purpose::STANDARD;
        let payload = b.decode(&envelope.payload).map_err(|e| e.to_string())?;
        let mut signed = Vec::new();
        signed.extend_from_slice(b"DSSEv1 ");
        signed.extend_from_slice(envelope.payload_type.len().to_string().as_bytes());
        signed.push(b' ');
        signed.extend_from_slice(envelope.payload_type.as_bytes());
        signed.push(b' ');
        signed.extend_from_slice(payload.len().to_string().as_bytes());
        signed.push(b' ');
        signed.extend_from_slice(&payload);
        let pk: [u8; 32] = unhex(public_key_hex).unwrap().try_into().unwrap();
        let vk = VerifyingKey::from_bytes(&pk).map_err(|e| e.to_string())?;
        let sig = b.decode(&envelope.signatures[0].sig).map_err(|e| e.to_string())?;
        let sig = Signature::from_slice(&sig).map_err(|e| e.to_string())?;
        vk.verify(&signed, &sig).map_err(|e| e.to_string())?;
        Ok(payload)
    }

    fn read_envelope(dir: &Path) -> Envelope {
        serde_json::from_slice(&std::fs::read(dir.join(RECORD_FILE)).unwrap()).unwrap()
    }

    #[test]
    fn a_run_record_passes_a_standard_dsse_check_and_names_every_file_by_hash() {
        let dir = run_dir();
        let sealer = Sealer::ephemeral();
        sealer.seal(&job(&sealer, None), dir.path(), "abc", 42).unwrap();
        let env = read_envelope(dir.path());
        assert_eq!(env.payload_type, PAYLOAD_TYPE);
        assert_eq!(env.signatures[0].keyid, sealer.key().keyid());
        let payload = standard_dsse_check(&env, &sealer.key().public_key_hex()).expect("verifies");
        let st: serde_json::Value = serde_json::from_slice(&payload).unwrap();
        assert_eq!(st["_type"], STATEMENT_TYPE);
        assert_eq!(st["predicateType"], PREDICATE_TYPE);
        let names: Vec<&str> = st["subject"].as_array().unwrap().iter().map(|s| s["name"].as_str().unwrap()).collect();
        assert_eq!(names, ["events.json", "streams/console.bin", "streams/index.json", "study.json"]);
        // the subject's digest is the SHA-256 of the file's bytes as written
        let console = st["subject"][1]["digest"]["sha256"].as_str().unwrap();
        assert_eq!(console, hex(&Sha256::digest(b"hello\n")));
    }

    #[test]
    fn the_record_fails_the_check_when_a_signed_byte_changes_and_a_changed_subject_no_longer_matches() {
        let dir = run_dir();
        let sealer = Sealer::ephemeral();
        sealer.seal(&job(&sealer, None), dir.path(), "abc", 42).unwrap();
        let pk = sealer.key().public_key_hex();

        // a changed payload byte breaks the signature
        let mut env = read_envelope(dir.path());
        let mut payload = base64::engine::general_purpose::STANDARD.decode(&env.payload).unwrap();
        let i = payload.len() / 2;
        payload[i] ^= 1;
        env.payload = b64(&payload);
        assert!(standard_dsse_check(&env, &pk).is_err());

        // a changed capture no longer hashes to its subject
        std::fs::write(dir.path().join("streams/console.bin"), b"hellp\n").unwrap();
        let env = read_envelope(dir.path());
        let st: serde_json::Value =
            serde_json::from_slice(&standard_dsse_check(&env, &pk).unwrap()).unwrap();
        let recorded = st["subject"][1]["digest"]["sha256"].as_str().unwrap();
        assert_ne!(recorded, sha256_of_file(&dir.path().join("streams/console.bin")).unwrap());

        // another key's check fails
        let other = CoreKey::ephemeral();
        assert!(standard_dsse_check(&read_envelope(dir.path()), &other.public_key_hex()).is_err());
    }

    #[test]
    fn measurements_json_becomes_a_subject_when_it_is_present() {
        let dir = run_dir();
        std::fs::write(dir.path().join("measurements.json"), br#"{"schema":1,"behaviours":{"rate":1},"measurements":[]}"#).unwrap();
        let sealer = Sealer::ephemeral();
        sealer.seal(&job(&sealer, None), dir.path(), "abc", 42).unwrap();
        let st: serde_json::Value = serde_json::from_slice(
            &standard_dsse_check(&read_envelope(dir.path()), &sealer.key().public_key_hex()).unwrap(),
        )
        .unwrap();
        assert!(st["subject"].as_array().unwrap().iter().any(|s| s["name"] == "measurements.json"));
        assert_eq!(st["predicate"]["observed"]["behaviours"]["rate"], 1);
    }

    #[test]
    fn what_core_was_told_is_kept_apart_from_what_it_observed() {
        let dir = run_dir();
        let sealer = Sealer::ephemeral();
        sealer.declarations().record("signal", "console", Scope::Agent);
        let params = BuildClaimParams {
            build_commit: Some("abcdef1".into()),
            build_dirty: Some("true".into()),
            build_toolchain: Some("gcc 13".into()),
        };
        sealer.seal(&job(&sealer, params.claims().unwrap()), dir.path(), "abc", 42).unwrap();
        let st: serde_json::Value = serde_json::from_slice(
            &standard_dsse_check(&read_envelope(dir.path()), &sealer.key().public_key_hex()).unwrap(),
        )
        .unwrap();
        let p = &st["predicate"];
        assert_eq!(p["claimed"]["build"]["commit"], "abcdef1");
        assert_eq!(p["claimed"]["build"]["dirty"], true);
        assert!(p["observed"].get("build").is_none(), "a claim never sits under `observed`");
        assert_eq!(p["observed"]["declarations"]["entries"][0]["scope"], "agent");
        assert_eq!(p["observed"]["provenance"]["dev_bench_version"], "1.2.3");
    }

    #[test]
    fn a_retracted_declaration_leaves_the_ledger() {
        let l = Declarations::new();
        l.record("signal", "a", Scope::Person);
        l.record("role", "dut", Scope::Agent);
        l.record("role", "client", Scope::Agent);
        l.forget("signal", "a");
        l.forget_all("role");
        assert!(l.snapshot()["entries"].as_array().unwrap().is_empty());
        l.record("instrument", "load", Scope::Person);
        assert_eq!(l.snapshot()["entries"][0]["scope"], "person");
    }

    #[test]
    fn build_claims_are_validated_and_absent_means_none() {
        assert_eq!(BuildClaimParams::default().claims().unwrap(), None);
        let bad = BuildClaimParams { build_commit: Some("XYZ".into()), ..Default::default() };
        assert!(bad.claims().is_err());
        let bad = BuildClaimParams { build_dirty: Some("maybe".into()), ..Default::default() };
        assert!(bad.claims().is_err());
    }

    #[test]
    fn the_key_is_generated_once_stored_private_and_reused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sub").join("signing-key");
        let a = CoreKey::load_or_create(&path).unwrap();
        let b = CoreKey::load_or_create(&path).unwrap();
        assert_eq!(a.keyid(), b.keyid());
        assert_eq!(std::fs::read_to_string(&path).unwrap().len(), 64);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        }
        std::fs::write(&path, "not hex").unwrap();
        assert!(CoreKey::load_or_create(&path).is_err());
    }

    // ---- the challenge ----

    fn router() -> (Router, Sealer) {
        let state = AppState::new("t".to_string());
        let sealer = state.sealer.clone();
        (crate::api::build_router(state), sealer)
    }

    async fn post_challenge(app: &Router, nonce: &str, token: Option<&str>) -> (StatusCode, serde_json::Value) {
        let mut req = Request::post("/auth/challenge").header("content-type", "application/json");
        if let Some(t) = token {
            req = req.header("authorization", format!("Bearer {t}"));
        }
        let res = app
            .clone()
            .oneshot(req.body(Body::from(serde_json::json!({ "nonce": nonce }).to_string())).unwrap())
            .await
            .unwrap();
        let status = res.status();
        let bytes = axum::body::to_bytes(res.into_body(), 1 << 20).await.unwrap();
        (status, serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null))
    }

    #[tokio::test]
    async fn the_challenge_signs_its_fixed_frame_with_no_token() {
        let (app, sealer) = router();
        let nonce = "ab".repeat(16);
        let (status, body) = post_challenge(&app, &nonce, None).await;
        assert_eq!(status, StatusCode::OK);
        let frame = body["frame"].as_str().unwrap();
        let core_nonce = body["core_nonce"].as_str().unwrap();
        assert_eq!(frame, format!("{CHALLENGE_TAG}\n{core_nonce}\n{nonce}"));
        assert_eq!(core_nonce.len(), 64);
        let vk_bytes: [u8; 32] = unhex(&sealer.key().public_key_hex()).unwrap().try_into().unwrap();
        let sig = Signature::from_slice(&unhex(body["signature"].as_str().unwrap()).unwrap()).unwrap();
        VerifyingKey::from_bytes(&vk_bytes).unwrap().verify(frame.as_bytes(), &sig).unwrap();
        assert_eq!(body["keyid"], sealer.key().keyid());

        // each answer carries a fresh Core nonce
        let (_, again) = post_challenge(&app, &nonce, None).await;
        assert_ne!(again["core_nonce"], body["core_nonce"]);
    }

    #[tokio::test]
    async fn a_caller_chosen_payload_cannot_be_signed_through_the_challenge() {
        let (app, sealer) = router();
        // text that would turn the frame into something else, or into a DSSE
        // pre-authentication string, is not hex and is refused
        for nonce in [
            "DSSEv1 28 application/vnd.in-toto+json 2 {}",
            "embarch-core-auth-v1\nabcd",
            &format!("{}\n{}", "ab".repeat(16), "cd".repeat(16)),
            "ab", // too short
            &"ab".repeat(65), // too long
            &"AB".repeat(16), // not lowercase
            &"a".repeat(33), // odd
        ] {
            let (status, _) = post_challenge(&app, nonce, None).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "nonce {nonce:?}");
        }
        // an accepted nonce: whatever is signed is the frame, which is neither a
        // DSSE pre-authentication string nor a run record's bytes
        let nonce = "0f".repeat(64);
        let (status, body) = post_challenge(&app, &nonce, None).await;
        assert_eq!(status, StatusCode::OK);
        let frame = body["frame"].as_str().unwrap();
        assert!(frame.starts_with(CHALLENGE_TAG));
        assert!(!frame.starts_with("DSSEv1 "));
        // the signature over the frame verifies over no DSSE payload
        let sig = Signature::from_slice(&unhex(body["signature"].as_str().unwrap()).unwrap()).unwrap();
        let vk_bytes: [u8; 32] = unhex(&sealer.key().public_key_hex()).unwrap().try_into().unwrap();
        let vk = VerifyingKey::from_bytes(&vk_bytes).unwrap();
        assert!(vk.verify(&pae(PAYLOAD_TYPE, frame.as_bytes()), &sig).is_err());
        assert!(vk.verify(nonce.as_bytes(), &sig).is_err());
        // an unknown key in the body is refused too
        let res = app
            .clone()
            .oneshot(
                Request::post("/auth/challenge")
                    .header("content-type", "application/json")
                    .body(Body::from(format!(r#"{{"nonce":"{}","payload":"x"}}"#, "ab".repeat(16))))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert!(res.status().is_client_error());
    }
}
