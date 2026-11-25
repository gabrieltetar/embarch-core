//! Core's copy of the HIL definitions and what is confirmed about them
//! (decision 133): a test, suite manifest, plan, bench, capability or hook
//! mapping, kept by the hash of its definition (`embarch-study-designer`'s
//! `hil_test::definition_hash`), the premade suites read from the checkout
//! `EMBARCH_SUITES_DIR` names, and the single-use nonces a confirmation card
//! is submitted with.
//!
//! A definition is `definitions/<sha256>.json` in the store, written once: the
//! same bytes posted again are the same file. Confirmations are entries in the
//! run log (`RunLog::confirm`), so this module keeps no state about them; it
//! only asks the log which hashes are confirmed.

use anyhow::{Context, Result};
use embarch_study_designer::hil_check::{self, By, Measured, Metrology, StudyKind};
use embarch_study_designer::hil_test::{
    definition_hash, derived_diff, Capability, HookMapping, Plan, SuiteBench, SuiteManifest, TestDef,
};
use embarch_study_designer::host_study::HostStudy;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex as StdMutex};

/// The environment variable naming the suite checkout (plan choice 11).
pub const SUITES_DIR_VAR: &str = "EMBARCH_SUITES_DIR";
/// A suite's manifest, wherever it sits under the checkout.
pub const SUITE_FILE: &str = "suite.json";
/// How deep under the checkout a `suite.json` is looked for. Chosen, not
/// measured: the suites repo's layout is not fixed yet.
const SUITE_SEARCH_DEPTH: usize = 3;
/// How long a card's nonce lives. The go's nonce is ten minutes (plan choice 29
/// for Test); this reuses it. Chosen, not measured.
pub const NONCE_TTL_MS: u64 = 10 * 60 * 1000;
/// The folder under the store.
const DEFINITIONS_DIR: &str = "definitions";

/// What a posted definition is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    Test,
    Suite,
    Plan,
    Bench,
    Capability,
    Hook,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Test => "test",
            Kind::Suite => "suite",
            Kind::Plan => "plan",
            Kind::Bench => "bench",
            Kind::Capability => "capability",
            Kind::Hook => "hook",
        }
    }
}

/// `POST /definitions`' body. `study` and `bench` go together and, with
/// `measured`, run `hil_check::check_test` on a test; without them a test gets
/// the shape check alone.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DefinitionRequest {
    pub kind: Kind,
    pub definition: serde_json::Value,
    #[serde(default)]
    pub study: Option<HostStudy>,
    #[serde(default)]
    pub bench: Option<SuiteBench>,
    #[serde(default)]
    pub measured: BTreeMap<String, MeasuredIn>,
}

/// Who measured a value, as a caller writes it.
#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ByIn {
    Bench,
    Dut,
    Person,
}

/// `hil_check::Measured` has no serde of its own; this is its wire form.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MeasuredIn {
    pub by: ByIn,
    #[serde(default)]
    pub source: String,
    #[serde(default)]
    pub range_min: Option<f64>,
    #[serde(default)]
    pub range_max: Option<f64>,
    #[serde(default)]
    pub resolution: Option<f64>,
    #[serde(default)]
    pub u: Option<f64>,
}

impl MeasuredIn {
    fn into_measured(self) -> Measured {
        Measured {
            by: match self.by {
                ByIn::Bench => By::Bench,
                ByIn::Dut => By::Dut,
                ByIn::Person => By::Person,
            },
            source: self.source,
            metrology: Metrology { range_min: self.range_min, range_max: self.range_max, resolution: self.resolution, u: self.u },
        }
    }
}

/// A refused definition: the status, and the crate's reason one per line.
#[derive(Debug, PartialEq)]
pub struct Refused(pub String);

/// A definition that parsed, checked and was hashed.
pub struct Checked {
    pub kind: Kind,
    pub hash: String,
    /// The definition as the crate serialises it.
    pub value: serde_json::Value,
    /// `shape` or `definition` (`check_test` ran).
    pub checked: &'static str,
}

fn parse<T: serde::de::DeserializeOwned>(v: serde_json::Value, what: &str) -> Result<T, Refused> {
    serde_json::from_value(v).map_err(|e| Refused(format!("not a {what}: {e}")))
}

fn serialise<T: Serialize>(t: &T) -> serde_json::Value {
    serde_json::to_value(t).unwrap_or(serde_json::Value::Null)
}

/// Parses, checks and hashes a definition. The checks are the crate's own
/// (`TestDef::check`, `SuiteManifest::check`, `Plan::check`, and
/// `hil_check::check_test` where the study and the bench came too), so Core and
/// embarch-api refuse the same definition for the same reason.
pub fn check(req: DefinitionRequest) -> Result<Checked, Refused> {
    let DefinitionRequest { kind, definition, study, bench, measured } = req;
    let shape = |r: Result<(), embarch_study_designer::hil_test::ShapeError>| r.map_err(|e| Refused(format!("{e:?}")));
    let mut checked = "shape";
    let (hash, value) = match kind {
        Kind::Test => {
            let t: TestDef = parse(definition, "test")?;
            shape(t.check())?;
            match (study, bench) {
                (Some(h), Some(b)) => {
                    let measured = measured.into_iter().map(|(k, v)| (k, v.into_measured())).collect();
                    hil_check::check_test(&t, StudyKind::Host(&h), &b, &measured)
                        .map_err(|rs| Refused(rs.iter().map(|r| r.to_string()).collect::<Vec<_>>().join("\n")))?;
                    checked = "definition";
                }
                (None, None) => {}
                _ => return Err(Refused("`study` and `bench` go together: the definition-time checks need both".into())),
            }
            (definition_hash(&t), serialise(&t))
        }
        Kind::Suite => {
            let s: SuiteManifest = parse(definition, "suite manifest")?;
            shape(s.check())?;
            (definition_hash(&s), serialise(&s))
        }
        Kind::Plan => {
            let p: Plan = parse(definition, "plan")?;
            shape(p.check())?;
            (definition_hash(&p), serialise(&p))
        }
        Kind::Bench => {
            let b: SuiteBench = parse(definition, "bench")?;
            if !b.description.is_object() {
                return Err(Refused(format!("bench `{}`: its description is not a JSON object", b.name)));
            }
            (definition_hash(&b), serialise(&b))
        }
        Kind::Capability => {
            let c: Capability = parse(definition, "capability")?;
            (definition_hash(&c), serialise(&c))
        }
        Kind::Hook => {
            let h: HookMapping = parse(definition, "hook mapping")?;
            (definition_hash(&h), serialise(&h))
        }
    };
    Ok(Checked { kind, hash, value, checked })
}

/// Whether `s` is a SHA-256 as the crate spells it: 64 lowercase hex.
pub fn is_hash(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// A suite version Core holds, from the checkout.
#[derive(Debug, Clone, Serialize)]
pub struct SuiteEntry {
    pub id: String,
    pub version: String,
    /// `SuiteManifest::hash`: what a confirmation of a suite Core does not hold binds.
    pub manifest_sha256: String,
    pub tests: Vec<SuiteTestEntry>,
    /// What could not be indexed: a test file that is missing or does not hash
    /// to the manifest's line. Such a test is not premade.
    pub problems: Vec<String>,
    #[serde(skip)]
    pub dir: PathBuf,
}

#[derive(Debug, Clone, Serialize)]
pub struct SuiteTestEntry {
    pub test_id: String,
    /// The test's `definition_hash`, recomputed from the file.
    pub sha256: String,
}

/// The suites under `root`: every `suite.json` up to [`SUITE_SEARCH_DEPTH`]
/// folders down, sorted by id and version. Each test file must parse and hash
/// to the manifest's line to count; a manifest that does not parse is skipped
/// and named in the returned problems.
pub fn scan_suites(root: &Path) -> (Vec<SuiteEntry>, Vec<String>) {
    let mut found = Vec::new();
    let mut problems = Vec::new();
    find_manifests(root, SUITE_SEARCH_DEPTH, &mut found);
    let mut out = Vec::new();
    for dir in found {
        let path = dir.join(SUITE_FILE);
        let manifest: SuiteManifest = match std::fs::read(&path).map_err(|e| e.to_string()).and_then(|b| serde_json::from_slice(&b).map_err(|e| e.to_string())) {
            Ok(m) => m,
            Err(e) => {
                problems.push(format!("{}: {e}", path.display()));
                continue;
            }
        };
        if let Err(e) = manifest.check() {
            problems.push(format!("{}: {e:?}", path.display()));
            continue;
        }
        let mut tests = Vec::new();
        let mut tp = Vec::new();
        for t in &manifest.tests {
            match read_test(&dir, &t.file) {
                Err(e) => tp.push(format!("{}: {e}", t.test_id)),
                Ok(def) => {
                    let sha = def.hash();
                    if sha == t.hash {
                        tests.push(SuiteTestEntry { test_id: t.test_id.clone(), sha256: sha });
                    } else {
                        tp.push(format!("{}: {} hashes to {sha}, the manifest says {}", t.test_id, t.file, t.hash));
                    }
                }
            }
        }
        out.push(SuiteEntry { id: manifest.id.clone(), version: manifest.version.clone(), manifest_sha256: manifest.hash(), tests, problems: tp, dir });
    }
    out.sort_by(|a, b| (&a.id, &a.version).cmp(&(&b.id, &b.version)));
    (out, problems)
}

fn find_manifests(dir: &Path, depth: usize, out: &mut Vec<PathBuf>) {
    if dir.join(SUITE_FILE).is_file() {
        out.push(dir.to_path_buf());
    }
    if depth == 0 {
        return;
    }
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    let mut subs: Vec<PathBuf> = rd.filter_map(|e| e.ok()).filter(|e| e.file_type().is_ok_and(|t| t.is_dir())).map(|e| e.path()).collect();
    subs.sort();
    for s in subs {
        find_manifests(&s, depth - 1, out);
    }
}

fn read_test(dir: &Path, file: &str) -> Result<TestDef, String> {
    let rel = Path::new(file);
    if rel.components().any(|c| !matches!(c, Component::Normal(_))) {
        return Err(format!("{file} is not a path inside the suite"));
    }
    let bytes = std::fs::read(dir.join(rel)).map_err(|e| format!("{file}: {e}"))?;
    serde_json::from_slice(&bytes).map_err(|e| format!("{file}: {e}"))
}

/// A hash a person may be asked to confirm, and why.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Standing {
    /// Identical to a test of a suite version Core holds: needs none.
    Premade,
    /// A bench or a plan: confirmed by the go, not here.
    NotRequired,
    /// A suite version Core holds.
    Held,
    Confirmed,
    /// Unconfirmed itself, but listed by a confirmed suite manifest Core does not hold: confirmed as that unit.
    ConfirmedWithSuite,
    Unconfirmed,
}

/// Core's definitions, its premade suites and the cards' nonces.
#[derive(Clone)]
pub struct Definitions {
    dir: PathBuf,
    suites_dir: Option<PathBuf>,
    nonces: Arc<StdMutex<HashMap<String, (String, u64)>>>,
}

impl Definitions {
    /// Definitions under `<store>/definitions`, premade suites from `suites_dir`.
    pub fn new(store: &Path, suites_dir: Option<PathBuf>) -> Self {
        Self { dir: store.join(DEFINITIONS_DIR), suites_dir, nonces: Arc::default() }
    }

    /// As [`new`](Self::new), the suite checkout being the one `EMBARCH_SUITES_DIR` names.
    pub fn from_env(store: &Path) -> Self {
        Self::new(store, std::env::var_os(SUITES_DIR_VAR).filter(|v| !v.is_empty()).map(PathBuf::from))
    }

    fn path(&self, hash: &str) -> PathBuf {
        self.dir.join(format!("{hash}.json"))
    }

    /// Keeps a checked definition; `true` when it was new. The file is the
    /// `{kind, definition}` envelope, written whole and renamed into place.
    pub fn keep(&self, c: &Checked) -> Result<bool> {
        let path = self.path(&c.hash);
        if path.is_file() {
            return Ok(false);
        }
        std::fs::create_dir_all(&self.dir).context("failed to create the definitions folder")?;
        let tmp = self.dir.join(format!("{}.tmp", c.hash));
        let body = serde_json::to_vec(&serde_json::json!({ "kind": c.kind.as_str(), "definition": c.value }))?;
        std::fs::write(&tmp, body)?;
        std::fs::rename(&tmp, &path)?;
        Ok(true)
    }

    /// A kept definition's `{kind, definition}`, or `None`. `hash` must be a [`is_hash`].
    pub fn get(&self, hash: &str) -> Result<Option<serde_json::Value>> {
        if !is_hash(hash) {
            return Ok(None);
        }
        match std::fs::read(self.path(hash)) {
            Ok(b) => Ok(Some(serde_json::from_slice(&b).context("a kept definition is not JSON")?)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e).context("failed to read a definition"),
        }
    }

    /// Every kept definition's hash and kind.
    pub fn all(&self) -> Result<Vec<(String, String)>> {
        let mut out = Vec::new();
        let Ok(rd) = std::fs::read_dir(&self.dir) else { return Ok(out) };
        for e in rd {
            let p = e?.path();
            let Some(hash) = p.file_stem().and_then(|s| s.to_str()).filter(|s| is_hash(s) && p.extension().is_some_and(|x| x == "json")) else { continue };
            if let Some(v) = self.get(hash)? {
                out.push((hash.to_string(), v["kind"].as_str().unwrap_or("").to_string()));
            }
        }
        out.sort();
        Ok(out)
    }

    /// The suites in the checkout, read now (a contributor's edit is seen at once), and what could not be read.
    pub fn suites(&self) -> (Vec<SuiteEntry>, Vec<String>) {
        match &self.suites_dir {
            Some(d) => scan_suites(d),
            None => (Vec::new(), Vec::new()),
        }
    }

    /// Where a kept definition stands: premade tests and held suites need no
    /// confirmation, benches and plans are the go's, the rest wait for `confirmed`.
    pub fn standing(&self, kind: &str, hash: &str, confirmed: &HashSet<String>) -> Standing {
        let (suites, _) = self.suites();
        match kind {
            "bench" | "plan" => Standing::NotRequired,
            "test" if suites.iter().any(|s| s.tests.iter().any(|t| t.sha256 == hash)) => Standing::Premade,
            "suite" if suites.iter().any(|s| s.manifest_sha256 == hash) => Standing::Held,
            _ if confirmed.contains(hash) => Standing::Confirmed,
            "test" if self.listed_by_confirmed_suite(hash, confirmed) => Standing::ConfirmedWithSuite,
            _ => Standing::Unconfirmed,
        }
    }

    fn listed_by_confirmed_suite(&self, test_hash: &str, confirmed: &HashSet<String>) -> bool {
        self.all().unwrap_or_default().iter().filter(|(h, k)| k == "suite" && confirmed.contains(h)).any(|(h, _)| {
            let Ok(Some(v)) = self.get(h) else { return false };
            serde_json::from_value::<SuiteManifest>(v["definition"].clone()).is_ok_and(|m| m.tests.iter().any(|t| t.hash == test_hash))
        })
    }

    /// Whether a suite checkout is named at all.
    pub fn suites_configured(&self) -> bool {
        self.suites_dir.is_some()
    }

    /// The premade test a copy names in `derived_from` (`<test id>@<suite version>`), read from the checkout.
    pub fn premade_original(&self, derived_from: &str) -> Option<TestDef> {
        let (id, version) = derived_from.split_once('@')?;
        let (suites, _) = self.suites();
        let suite = suites.iter().find(|s| s.version == version && s.tests.iter().any(|t| t.test_id == id))?;
        let manifest: SuiteManifest = serde_json::from_slice(&std::fs::read(suite.dir.join(SUITE_FILE)).ok()?).ok()?;
        let file = &manifest.tests.iter().find(|t| t.test_id == id)?.file;
        read_test(&suite.dir, file).ok()
    }

    /// The manifest of the premade suite `id` at `version`, read from the checkout.
    pub fn premade_suite(&self, id: &str, version: &str) -> Option<SuiteManifest> {
        let (suites, _) = self.suites();
        let suite = suites.iter().find(|s| s.id == id && s.version == version)?;
        serde_json::from_slice(&std::fs::read(suite.dir.join(SUITE_FILE)).ok()?).ok()
    }

    /// The premade test whose definition hash is `hash`, from any suite of the checkout.
    pub fn premade_test(&self, hash: &str) -> Option<TestDef> {
        let (suites, _) = self.suites();
        let suite = suites.iter().find(|s| s.tests.iter().any(|t| t.sha256 == hash))?;
        let manifest: SuiteManifest = serde_json::from_slice(&std::fs::read(suite.dir.join(SUITE_FILE)).ok()?).ok()?;
        let line = manifest.tests.iter().find(|t| t.hash == hash)?;
        read_test(&suite.dir, &line.file).ok()
    }

    /// The data of a card: the definition, and for a copy its diff against the premade test it names.
    pub fn card(&self, hash: &str, stored: &serde_json::Value) -> serde_json::Value {
        let mut card = serde_json::json!({ "hash": hash, "kind": stored["kind"], "definition": stored["definition"] });
        if stored["kind"] == "test" {
            if let Ok(copy) = serde_json::from_value::<TestDef>(stored["definition"].clone()) {
                if let Some(from) = copy.derived_from.as_deref() {
                    match self.premade_original(from) {
                        Some(original) => card["diff"] = serialise(&derived_diff(&copy, &original)),
                        None => card["diff_unavailable"] = format!("Core holds no premade test {from}").into(),
                    }
                }
            }
        }
        card
    }

    /// Issues a single-use nonce for `hash`'s card, void after [`NONCE_TTL_MS`]; expired ones are dropped here.
    pub fn issue_nonce(&self, hash: &str, now_ms: u64) -> String {
        let mut bytes = [0u8; 32];
        rand::fill(&mut bytes);
        let nonce: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
        let mut map = self.nonces.lock().unwrap();
        map.retain(|_, (_, exp)| *exp > now_ms);
        map.insert(nonce.clone(), (hash.to_string(), now_ms + NONCE_TTL_MS));
        nonce
    }

    /// Spends `nonce`, whatever it was for: it is gone after this call. `Ok`
    /// only when it was issued for `hash` and has not expired.
    pub fn spend_nonce(&self, nonce: &str, hash: &str, now_ms: u64) -> Result<(), &'static str> {
        match self.nonces.lock().unwrap().remove(nonce) {
            None => Err("the nonce is unknown, already used, or from before Core restarted"),
            Some((_, exp)) if exp <= now_ms => Err("the nonce has expired"),
            Some((h, _)) if h != hash => Err("the nonce was issued for another card"),
            Some(_) => Ok(()),
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    const TEST_JSON: &str = include_str!("../tests/fixtures/hil-tests/test.json");
    const SUITE_JSON: &str = include_str!("../tests/fixtures/hil-tests/suite.json");
    const STUDY_JSON: &str = include_str!("../tests/fixtures/hil-tests/study.json");

    pub(crate) fn test_req(def: &str) -> DefinitionRequest {
        DefinitionRequest {
            kind: Kind::Test,
            definition: serde_json::from_str(def).unwrap(),
            study: None,
            bench: None,
            measured: BTreeMap::new(),
        }
    }

    fn temp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("embarch-defs-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn a_definition_posted_twice_is_kept_once_by_its_hash() {
        let d = Definitions::new(&temp("twice"), None);
        let c = check(test_req(TEST_JSON)).unwrap();
        assert!(is_hash(&c.hash));
        assert!(d.keep(&c).unwrap());
        // Same definition, keys in another order: the same hash.
        let mut v: serde_json::Value = serde_json::from_str(TEST_JSON).unwrap();
        let reordered: serde_json::Map<String, serde_json::Value> = v.as_object_mut().unwrap().iter().rev().map(|(k, v)| (k.clone(), v.clone())).collect();
        let again = check(DefinitionRequest { definition: reordered.into(), ..test_req("{}") }).unwrap();
        assert_eq!(again.hash, c.hash);
        assert!(!d.keep(&again).unwrap());
        assert_eq!(d.all().unwrap().len(), 1);
        assert_eq!(d.get(&c.hash).unwrap().unwrap()["kind"], "test");
    }

    #[test]
    fn an_invalid_definition_is_refused_with_the_crates_reason() {
        let mut v: serde_json::Value = serde_json::from_str(TEST_JSON).unwrap();
        v["run_count"] = 0.into();
        let Refused(why) = check(DefinitionRequest { definition: v, ..test_req("{}") }).map(|_| ()).unwrap_err();
        assert!(why.contains("RunCount"), "{why}");
        let Refused(why) = check(test_req(r#"{"nonsense":1}"#)).map(|_| ()).unwrap_err();
        assert!(why.starts_with("not a test"), "{why}");
    }

    #[test]
    fn a_test_with_its_study_and_bench_gets_the_definition_time_checks() {
        let suite: SuiteManifest = serde_json::from_str(SUITE_JSON).unwrap();
        let study: HostStudy = serde_json::from_str(STUDY_JSON).unwrap();
        let measured: BTreeMap<String, MeasuredIn> = [
            ("vbus_mean", r#"{"by":"bench","source":"dmm","u":0.01,"resolution":0.001,"range_min":0,"range_max":20}"#),
            ("vbus_ripple", r#"{"by":"bench","source":"dmm","u":0.005,"resolution":0.0001,"range_min":0,"range_max":0.5}"#),
            ("led_state", r#"{"by":"person","source":"person"}"#),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), serde_json::from_str(v).unwrap()))
        .collect();
        let bench = suite.benches[0].clone();
        let ok = DefinitionRequest { study: Some(study.clone()), bench: Some(bench.clone()), measured: measured.clone(), ..test_req(TEST_JSON) };
        assert_eq!(check(ok).map(|c| c.checked), Ok("definition"));
        // A bench with no DUT: the crate refuses, and says which test and field.
        let mut no_dut = bench;
        no_dut.description = serde_json::json!({ "parts": [] });
        let bad = DefinitionRequest { study: Some(study), bench: Some(no_dut), measured, ..test_req(TEST_JSON) };
        let Refused(why) = check(bad).map(|_| ()).unwrap_err();
        assert!(why.contains("holds no DUT"), "{why}");
        // One without the other is a request error.
        let half = DefinitionRequest { bench: Some(suite.benches[0].clone()), ..test_req(TEST_JSON) };
        assert!(check(half).map(|_| ()).unwrap_err().0.contains("go together"));
    }

    impl PartialEq for Checked {
        fn eq(&self, o: &Self) -> bool {
            self.hash == o.hash
        }
    }
    impl std::fmt::Debug for Checked {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "Checked({})", self.hash)
        }
    }

    /// A one-suite checkout whose manifest lists the fixture test under its true hash.
    pub(crate) fn checkout(name: &str) -> PathBuf {
        let root = temp(name);
        let dir = root.join("usb-pd").join("1.0.0");
        std::fs::create_dir_all(&dir).unwrap();
        let t: TestDef = serde_json::from_str(TEST_JSON).unwrap();
        std::fs::write(dir.join("test.json"), TEST_JSON).unwrap();
        let mut m: SuiteManifest = serde_json::from_str(SUITE_JSON).unwrap();
        m.id = "usb-pd".into();
        m.version = "1.0.0".into();
        m.tests = vec![embarch_study_designer::hil_test::SuiteTest { test_id: t.test_id.clone(), file: "test.json".into(), hash: t.hash() }];
        std::fs::write(dir.join(SUITE_FILE), serde_json::to_vec(&m).unwrap()).unwrap();
        root
    }

    #[test]
    fn a_premade_test_needs_no_confirmation_and_one_byte_changed_needs_one() {
        let store = temp("premade-store");
        let d = Definitions::new(&store, Some(checkout("premade")));
        let none = HashSet::new();
        let (suites, problems) = d.suites();
        assert!(problems.is_empty() && suites.len() == 1 && suites[0].problems.is_empty(), "{suites:?} {problems:?}");
        assert_eq!((suites[0].id.as_str(), suites[0].version.as_str()), ("usb-pd", "1.0.0"));
        let same = check(test_req(TEST_JSON)).unwrap();
        assert_eq!(d.standing("test", &same.hash, &none), Standing::Premade);
        let mut v: serde_json::Value = serde_json::from_str(TEST_JSON).unwrap();
        v["title"] = format!("{}!", v["title"].as_str().unwrap()).into();
        let edited = check(DefinitionRequest { definition: v, ..test_req("{}") }).unwrap();
        assert_ne!(edited.hash, same.hash);
        assert_eq!(d.standing("test", &edited.hash, &none), Standing::Unconfirmed);
        assert_eq!(d.standing("test", &edited.hash, &HashSet::from([edited.hash.clone()])), Standing::Confirmed);
        // With no checkout nothing is premade.
        assert_eq!(Definitions::new(&store, None).standing("test", &same.hash, &none), Standing::Unconfirmed);
    }

    #[test]
    fn a_test_file_that_does_not_hash_to_its_manifest_line_is_not_premade() {
        let root = checkout("drift");
        let dir = root.join("usb-pd").join("1.0.0");
        std::fs::write(dir.join("test.json"), TEST_JSON.replace("\"run_count\": 3", "\"run_count\": 4")).unwrap();
        let (suites, _) = scan_suites(&root);
        assert!(suites[0].tests.is_empty(), "{suites:?}");
        assert!(suites[0].problems[0].contains("manifest says"), "{suites:?}");
    }

    #[test]
    fn a_nonce_is_single_use_for_one_card_and_expires() {
        let d = Definitions::new(&temp("nonce"), None);
        let h = "a".repeat(64);
        let n = d.issue_nonce(&h, 1000);
        assert!(d.spend_nonce(&n, &h, 2000).is_ok());
        assert!(d.spend_nonce(&n, &h, 2000).unwrap_err().contains("already used"));
        let n = d.issue_nonce(&h, 1000);
        assert!(d.spend_nonce(&n, &"b".repeat(64), 2000).unwrap_err().contains("another card"));
        assert!(d.spend_nonce(&n, &h, 2000).is_err(), "a wrong-card attempt spends it");
        let n = d.issue_nonce(&h, 1000);
        assert!(d.spend_nonce(&n, &h, 1000 + NONCE_TTL_MS).unwrap_err().contains("expired"));
    }

    #[test]
    fn a_manifest_test_path_outside_the_suite_is_not_read() {
        assert!(read_test(Path::new("/tmp"), "../etc/passwd").unwrap_err().contains("not a path inside"));
        assert!(read_test(Path::new("/tmp"), "/etc/passwd").unwrap_err().contains("not a path inside"));
    }
}
