//! The bench lease (decision 93): one session holds the whole bench across
//! calls, and every other caller of a hardware route is refused by name
//! until the holder releases it or its TTL runs out.
//!
//! Core serialised single requests (`hw_lock`) and nothing longer; a
//! session's real unit of work is a sequence (flash, reset, a dozen
//! exchanges, a register read), and between any two of those another
//! session could flash its own image. The lease is that sequence's lock.
//!
//! In memory only, evaluated lazily: there is no timer, so an expired lease
//! is simply not live the next time anything asks, and a crashed holder
//! cannot block the bench past its TTL. A Core restart drops it too, which
//! frees the bench (the safe direction) and makes the holder's next renew a
//! `404`.
//!
//! A lease also ends after [`IDLE_RELEASE`] with no hardware call (decision
//! 123), whatever client renews it.

use serde::Serialize;
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant};
use time::OffsetDateTime;

use crate::study::StudyLock;

/// The request header that carries the lease token.
pub const LEASE_HEADER: &str = "x-embarch-lease";

/// Longest TTL a lease may ask for, and the default a renew without a
/// `ttl_s` reuses is the lease's own. Reasoned, not measured: the longest
/// field hold was 1.4 h (5,040 s), so four hours covers a long session
/// without letting a forgotten lease park the bench for a day.
pub const MAX_TTL_S: u64 = 4 * 60 * 60;

/// A lease with no hardware call for this long is released, however it is
/// renewed (decision 123). 30 minutes, the owner's call; not measured.
pub const IDLE_RELEASE: Duration = Duration::from_secs(30 * 60);

/// What a renew after an idle release is refused with.
pub const IDLE_RELEASE_REASON: &str = "released after 30 min with no hardware call";

#[derive(Debug, Clone)]
struct Lease {
    token: String,
    holder: String,
    purpose: String,
    ttl_s: u64,
    started_at: OffsetDateTime,
    expires_at: OffsetDateTime,
    expires: Instant,
    /// The idle clock's zero: the start, the last hardware call, or the end
    /// of the last study run, whichever is latest.
    idle_since: Instant,
    /// Wall time of the last hardware call; `None` until one is made.
    last_call_at: Option<OffsetDateTime>,
}

/// What a caller may see of a lease: everything except the token.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct LeaseView {
    pub holder: String,
    pub purpose: String,
    pub started_at: String,
    pub expires_at: String,
    pub ttl_s: u64,
    /// The last hardware call admitted with the token, or `None` before one.
    pub last_hardware_call_at: Option<String>,
}

impl LeaseView {
    /// The `409` body for a caller that does not hold the lease.
    pub fn refusal(&self) -> String {
        format!(
            "bench leased by '{}' for '{}' (started {}, expires {}); send that lease's token in the \
             {LEASE_HEADER} header, or wait for it to expire",
            self.holder, self.purpose, self.started_at, self.expires_at
        )
    }
}

fn rfc3339(t: OffsetDateTime) -> String {
    t.format(&time::format_description::well_known::Rfc3339).unwrap_or_default()
}

impl Lease {
    fn view(&self) -> LeaseView {
        LeaseView {
            holder: self.holder.clone(),
            purpose: self.purpose.clone(),
            started_at: rfc3339(self.started_at),
            expires_at: rfc3339(self.expires_at),
            ttl_s: self.ttl_s,
            last_hardware_call_at: self.last_call_at.map(rfc3339),
        }
    }
}

/// `POST /lease`'s answer: the view plus the token, shown once.
#[derive(Debug, Serialize)]
pub struct Granted {
    pub token: String,
    #[serde(flatten)]
    pub lease: LeaseView,
}

#[derive(Debug, PartialEq, Eq)]
pub enum LeaseError {
    /// Another lease is live (create), or the token is not the live one.
    Held(Box<LeaseView>),
    /// Nothing is live: never taken, released, expired, or Core restarted.
    NoLease,
    /// Nothing is live because the last lease, held by this holder, sat
    /// [`IDLE_RELEASE`] with no hardware call.
    IdleReleased(String),
    Invalid(String),
}

#[derive(Default)]
struct Inner {
    lease: Option<Lease>,
    /// Holder of the last lease if it ended idle; cleared by the next take
    /// or a release.
    idle_released: Option<String>,
}

#[derive(Clone, Default)]
pub struct LeaseSlot {
    inner: Arc<StdMutex<Inner>>,
    /// `AppState::study_lock`: a lease is not idle while it is held.
    study_lock: Option<StudyLock>,
}

impl LeaseSlot {
    /// A slot that treats a held `study_lock` as activity (decision 123).
    pub fn watching(study_lock: StudyLock) -> Self {
        Self { inner: Arc::default(), study_lock: Some(study_lock) }
    }

    fn study_in_flight(&self) -> bool {
        self.study_lock.as_ref().is_some_and(|l| l.lock().unwrap().is_some())
    }

    /// The live lease, clearing one that has expired or sat idle.
    fn live<'a>(&self, inner: &'a mut Inner) -> Option<&'a mut Lease> {
        let now = Instant::now();
        let expired = inner.lease.as_ref().is_some_and(|l| now >= l.expires);
        let idle = !expired
            && inner.lease.as_ref().is_some_and(|l| now >= l.idle_since + IDLE_RELEASE)
            && !self.study_in_flight();
        if expired || idle {
            if let Some(l) = inner.lease.take() {
                if idle {
                    tracing::info!("lease held by '{}' released: {IDLE_RELEASE_REASON}", l.holder);
                    inner.idle_released = Some(l.holder);
                } else {
                    tracing::info!("lease held by '{}' expired", l.holder);
                }
            }
        }
        inner.lease.as_mut()
    }

    pub fn view(&self) -> Option<LeaseView> {
        self.live(&mut self.inner.lock().unwrap()).map(|l| l.view())
    }

    /// Why nothing is live: [`LeaseError::IdleReleased`] naming the holder
    /// when the last lease ended idle, else [`LeaseError::NoLease`].
    pub fn absent(&self) -> LeaseError {
        let mut inner = self.inner.lock().unwrap();
        if self.live(&mut inner).is_some() {
            return LeaseError::NoLease;
        }
        self.idle_or_none(&inner)
    }

    /// A study run ended: the idle clock restarts from now (decision 123).
    /// Called before the study lock is cleared, so no check sees it free
    /// with a stale clock.
    pub fn study_ended(&self) {
        if let Some(l) = self.inner.lock().unwrap().lease.as_mut() {
            l.idle_since = Instant::now();
        }
    }

    pub fn take(&self, holder: &str, purpose: &str, ttl_s: u64) -> Result<Granted, LeaseError> {
        let holder = holder.trim();
        if holder.is_empty() {
            return Err(LeaseError::Invalid("holder must not be empty".into()));
        }
        let ttl_s = check_ttl(ttl_s)?;
        let mut slot = self.inner.lock().unwrap();
        if let Some(l) = self.live(&mut slot) {
            return Err(LeaseError::Held(Box::new(l.view())));
        }
        slot.idle_released = None;
        let mut bytes = [0u8; 16];
        rand::fill(&mut bytes);
        let token: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
        let now = OffsetDateTime::now_utc();
        let d = Duration::from_secs(ttl_s);
        let lease = Lease {
            token: token.clone(),
            holder: holder.to_string(),
            purpose: purpose.trim().to_string(),
            ttl_s,
            started_at: now,
            expires_at: now + d,
            expires: Instant::now() + d,
            idle_since: Instant::now(),
            last_call_at: None,
        };
        tracing::info!("lease taken by '{}' ({}), ttl {ttl_s}s", lease.holder, lease.purpose);
        let view = lease.view();
        slot.lease = Some(lease);
        Ok(Granted { token, lease: view })
    }

    /// Extends the live lease to `ttl_s` from now (its own TTL when `None`).
    pub fn renew(&self, token: Option<&str>, ttl_s: Option<u64>) -> Result<LeaseView, LeaseError> {
        let mut slot = self.inner.lock().unwrap();
        if self.live(&mut slot).is_none() {
            return Err(self.idle_or_none(&slot));
        }
        let l = slot.lease.as_mut().unwrap();
        if token != Some(l.token.as_str()) {
            return Err(LeaseError::Held(Box::new(l.view())));
        }
        let ttl_s = check_ttl(ttl_s.unwrap_or(l.ttl_s))?;
        let d = Duration::from_secs(ttl_s);
        l.ttl_s = ttl_s;
        l.expires_at = OffsetDateTime::now_utc() + d;
        l.expires = Instant::now() + d;
        Ok(l.view())
    }

    pub fn release(&self, token: Option<&str>) -> Result<(), LeaseError> {
        let mut slot = self.inner.lock().unwrap();
        if self.live(&mut slot).is_none() {
            return Err(self.idle_or_none(&slot));
        }
        let l = slot.lease.as_ref().unwrap();
        if token != Some(l.token.as_str()) {
            return Err(LeaseError::Held(Box::new(l.view())));
        }
        tracing::info!("lease released by '{}'", l.holder);
        slot.lease = None;
        slot.idle_released = None;
        Ok(())
    }

    fn idle_or_none(&self, inner: &Inner) -> LeaseError {
        inner.idle_released.clone().map_or(LeaseError::NoLease, LeaseError::IdleReleased)
    }

    /// `Ok(holder)` when the request may touch hardware: no lease is live
    /// (`None`), or it carries the live lease's token (`Some(holder)`),
    /// which also stamps the lease's last hardware call and restarts its
    /// idle clock. A refused call stamps nothing.
    pub fn admit(&self, token: Option<&str>) -> Result<Option<String>, Box<LeaseView>> {
        let mut slot = self.inner.lock().unwrap();
        match self.live(&mut slot) {
            None => Ok(None),
            Some(l) if token == Some(l.token.as_str()) => {
                l.idle_since = Instant::now();
                l.last_call_at = Some(OffsetDateTime::now_utc());
                Ok(Some(l.holder.clone()))
            }
            Some(l) => Err(Box::new(l.view())),
        }
    }
}

fn check_ttl(ttl_s: u64) -> Result<u64, LeaseError> {
    if ttl_s == 0 || ttl_s > MAX_TTL_S {
        return Err(LeaseError::Invalid(format!("ttl_s must be 1..={MAX_TTL_S}, got {ttl_s}")));
    }
    Ok(ttl_s)
}

/// The last image Core put on a board (decision 93), so a session that takes
/// the bench can tell whether it must reflash. Keyed by chip. Kept in
/// `flashed-images.jsonl` (`ImageLog`), so a restart keeps it; an absent entry
/// means "unknown", not "none".
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct FlashedImage {
    pub chip: String,
    pub probe_serial: Option<String>,
    /// The path the caller named, or `(uploaded)` / `(bootload image)` when
    /// Core only ever held the bytes.
    pub path: String,
    /// `None` when the file could not be read back for hashing.
    pub sha256: Option<String>,
    pub flashed_at: String,
    /// The lease holder when it landed, or `None` with no lease live.
    pub lease_holder: Option<String>,
}

/// What Core knows about an image beyond [`FlashedImage`]'s status shape: the
/// load segments the bytes write, so a later readback has something to compare.
/// `None` when the format has no segment reading here (uf2, idf, a bootload
/// image whose load address Core does not know) or the bytes would not parse.
#[derive(Debug, Clone, Default)]
pub struct ImageDetail {
    pub role: Option<String>,
    pub identity: Option<String>,
    pub segments: Option<Vec<embarch_study_designer::hil_image::Segment>>,
}

/// The segments and identity of `bytes` flashed as `format` (`base` is a raw
/// binary's load address). All `None` for a format `image_segments` does not
/// read, a missing file, or bytes that do not parse (logged).
pub fn image_detail(bytes: Option<&[u8]>, format: &str, base: Option<u64>) -> ImageDetail {
    use embarch_study_designer::hil_image::{identity, image_segments, ImageFormat};
    let fmt = match format.to_ascii_lowercase().as_str() {
        "hex" | "ihex" => ImageFormat::Hex,
        "elf" => ImageFormat::Elf,
        "bin" => ImageFormat::Bin,
        _ => return ImageDetail::default(),
    };
    let Some(bytes) = bytes else { return ImageDetail::default() };
    match image_segments(bytes, fmt, base) {
        Ok(segments) => ImageDetail { role: None, identity: Some(identity(&segments)), segments: Some(segments) },
        Err(e) => {
            tracing::warn!("flashed image: no segments recorded: {e}");
            ImageDetail::default()
        }
    }
}

/// One line of `flashed-images.jsonl` (decision 93): [`FlashedImage`]'s own
/// fields (`flashed_at` is the same instant as `at_utc_ms`), the board's
/// `role`, the segments, the token scope that flashed, and the call-log entry
/// of that flash. `call_log_seq` is `None` when the call log could not enter it.
#[derive(Debug, Clone, Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct FlashedEntry {
    pub chip: String,
    pub probe_serial: Option<String>,
    pub role: Option<String>,
    pub path: String,
    pub sha256: Option<String>,
    pub flashed_at: String,
    pub lease_holder: Option<String>,
    pub identity: Option<String>,
    pub segments: Option<Vec<embarch_study_designer::hil_image::Segment>>,
    pub scope: String,
    pub at_utc_ms: u64,
    pub call_log_seq: Option<u64>,
}

impl FlashedEntry {
    fn status(&self) -> FlashedImage {
        FlashedImage {
            chip: self.chip.clone(),
            probe_serial: self.probe_serial.clone(),
            path: self.path.clone(),
            sha256: self.sha256.clone(),
            flashed_at: self.flashed_at.clone(),
            lease_holder: self.lease_holder.clone(),
        }
    }
}

/// The file the log appends to, beside the run log.
pub const FLASHED_IMAGES_FILE: &str = "flashed-images.jsonl";

#[derive(Default)]
struct ImageLogInner {
    dir: Option<std::path::PathBuf>,
    map: StdMutex<BTreeMap<String, FlashedEntry>>,
}

#[derive(Clone, Default)]
pub struct ImageLog(Arc<ImageLogInner>);

impl ImageLog {
    /// A log backed by `dir/flashed-images.jsonl`, reading back the latest
    /// entry per chip. A line that does not parse is skipped, not fatal.
    pub fn at(dir: &std::path::Path) -> Self {
        let mut map = BTreeMap::new();
        if let Ok(text) = std::fs::read_to_string(dir.join(FLASHED_IMAGES_FILE)) {
            for line in text.lines().filter(|l| !l.trim().is_empty()) {
                match serde_json::from_str::<FlashedEntry>(line) {
                    Ok(e) => {
                        map.insert(e.chip.clone(), e);
                    }
                    Err(e) => tracing::warn!("{FLASHED_IMAGES_FILE}: skipping a line that does not parse: {e}"),
                }
            }
        }
        Self(Arc::new(ImageLogInner { dir: Some(dir.to_path_buf()), map: StdMutex::new(map) }))
    }

    /// Remembers an image in memory only; [`commit`](Self::commit) writes it.
    #[cfg(test)]
    pub fn record(&self, image: FlashedImage) {
        self.record_detail(image, ImageDetail::default());
    }

    pub fn record_detail(&self, image: FlashedImage, detail: ImageDetail) {
        let at = OffsetDateTime::now_utc();
        let entry = FlashedEntry {
            chip: image.chip,
            probe_serial: image.probe_serial,
            role: detail.role,
            path: image.path,
            sha256: image.sha256,
            flashed_at: rfc3339(at),
            lease_holder: image.lease_holder,
            identity: detail.identity,
            segments: detail.segments,
            scope: "unknown".to_string(),
            at_utc_ms: (at.unix_timestamp_nanos() / 1_000_000) as u64,
            call_log_seq: None,
        };
        self.0.map.lock().unwrap().insert(entry.chip.clone(), entry);
    }

    /// Stamps the chip's latest entry with the scope of the call that flashed it
    /// and that call's call-log entry, then appends it to the file. A write
    /// that fails is logged: the flash already happened.
    pub fn commit(&self, chip: &str, scope: &str, call_log_seq: Option<u64>) {
        let entry = {
            let mut map = self.0.map.lock().unwrap();
            let Some(e) = map.get_mut(chip) else { return };
            e.scope = scope.to_string();
            e.call_log_seq = call_log_seq;
            e.clone()
        };
        let Some(dir) = &self.0.dir else { return };
        let write = || -> std::io::Result<()> {
            use std::io::Write as _;
            std::fs::create_dir_all(dir)?;
            let mut f = std::fs::OpenOptions::new().create(true).append(true).open(dir.join(FLASHED_IMAGES_FILE))?;
            writeln!(f, "{}", serde_json::to_string(&entry).map_err(std::io::Error::other)?)
        };
        if let Err(e) = write() {
            tracing::error!("could not append to {FLASHED_IMAGES_FILE}: {e}");
        }
    }

    pub fn all(&self) -> Vec<FlashedImage> {
        self.0.map.lock().unwrap().values().map(FlashedEntry::status).collect()
    }

    /// The latest entry per chip, with its segments. Tests only until the readback lands.
    #[cfg(test)]
    pub fn entries(&self) -> Vec<FlashedEntry> {
        self.0.map.lock().unwrap().values().cloned().collect()
    }
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(bytes).iter().map(|b| format!("{b:02x}")).collect()
}

/// Every route a live lease gates: a request without the token is refused
/// `409`. Decision 93 has the reason for each edge case. Paired with
/// [`LEASE_OPEN`]; a test requires every `AUTH_CASES` row to be in exactly
/// one of the two, so a new route cannot be added without deciding.
pub const LEASE_GATED: &[(&str, &str)] = &[
    ("POST", "/flash"),
    ("POST", "/reset"),
    ("POST", "/bootload"),
    ("PUT", "/bootload/ports"),
    ("DELETE", "/bootload/ports"),
    ("GET", "/serial-log"),
    ("GET", "/dev-bench/hello"),
    ("POST", "/probes/enroll"),
    ("DELETE", "/probes/enrolled/{role}"),
    ("PUT", "/probes/enrolled/{role}/board"),
    ("DELETE", "/probes/by-serial/{serial}"),
    ("POST", "/dev-bench/link"),
    ("POST", "/signals"),
    ("DELETE", "/signals/{name}"),
    ("POST", "/signals/{name}/exchange"),
    ("POST", "/signals/{name}/capture"),
    ("DELETE", "/signals/{name}/capture"),
    ("PUT", "/instruments"),
    ("DELETE", "/instruments/{name}"),
    ("POST", "/instruments/{name}/exchange"),
    ("POST", "/live/mem-read"),
    ("POST", "/live/core-state"),
    ("POST", "/validate"),
    ("POST", "/study"),
    ("POST", "/host-studies"),
    ("POST", "/study/{study_id}/cancel"),
];

/// Routes a lease never gates: reads that touch no board, the lease's own
/// routes (they check the token themselves), and reading a capture or a
/// study's record back.
#[cfg(test)]
pub const LEASE_OPEN: &[(&str, &str)] = &[
    ("GET", "/status"),
    ("GET", "/bootload/ports"),
    ("GET", "/dev-bench/port"),
    ("POST", "/resolve-chip"),
    ("GET", "/probes/enrolled"),
    ("GET", "/signals"),
    ("GET", "/signals/{name}/capture"),
    ("GET", "/instruments"),
    ("GET", "/serial-ports"),
    ("GET", "/alerts"),
    ("GET", "/logs/recent"),
    ("GET", "/studies"),
    ("GET", "/study/{study_id}"),
    ("GET", "/study/{study_id}/events"),
    ("GET", "/study/{study_id}/steps"),
    ("GET", "/study/{study_id}/streams"),
    // A file already on disk, written as the study ended: no port (decision 121).
    ("GET", "/study/{study_id}/measurements"),
    ("GET", "/study/{study_id}/stream/{name}"),
    ("GET", "/study/{study_id}/stream/{name}/arrivals"),
    ("GET", "/study/{study_id}/stream/{name}/load"),
    // Counts lines of files already on disk: no port, no hardware (core/116).
    // The run log and what it governs touch files and the log, never a port (decision 111).
    ("GET", "/run-log"),
    // Reads Core's own call log, never a port (decision 122).
    ("GET", "/call-log"),
    ("GET", "/study/{study_id}/log"),
    ("POST", "/study/{study_id}/published"),
    ("POST", "/study/{study_id}/discard"),
    ("POST", "/study/{study_id}/adopt"),
    ("POST", "/study/{study_id}/report-data"),
    ("POST", "/study/{study_id}/publish"),
    ("PUT", "/study/{study_id}/restore/{*name}"),
    ("GET", "/study/{study_id}/publish/{revision}/files/{*name}"),
    ("GET", "/study/{study_id}/line-counts"),
    ("POST", "/study/{study_id}/line-counts"),
    // A person's answer to a prompt, from whichever surface is in front of them; the prompt id,
    // not the lease, keeps a stale answer off a later prompt (decision 105).
    ("POST", "/study/{study_id}/prompt"),
    ("POST", "/lease"),
    ("GET", "/lease"),
    ("PUT", "/lease"),
    ("DELETE", "/lease"),
];

pub fn is_gated(method: &str, matched_path: &str) -> bool {
    LEASE_GATED.iter().any(|(m, p)| *m == method && *p == matched_path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn second_take_is_refused_naming_the_first() {
        let s = LeaseSlot::default();
        s.take("a", "flash and run", 60).unwrap();
        match s.take("b", "x", 60) {
            Err(LeaseError::Held(v)) => {
                assert_eq!(v.holder, "a");
                assert_eq!(v.purpose, "flash and run");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn admit_needs_the_token_only_while_live() {
        let s = LeaseSlot::default();
        assert_eq!(s.admit(None), Ok(None));
        let g = s.take("a", "p", 60).unwrap();
        assert!(s.admit(None).is_err());
        assert!(s.admit(Some("nope")).is_err());
        assert_eq!(s.admit(Some(&g.token)), Ok(Some("a".to_string())));
        s.release(Some(&g.token)).unwrap();
        assert_eq!(s.admit(None), Ok(None));
    }

    #[test]
    fn expiry_frees_the_bench_without_anyone_asking() {
        let s = LeaseSlot::default();
        s.take("a", "p", 1).unwrap();
        // Force expiry rather than sleeping.
        s.inner.lock().unwrap().lease.as_mut().unwrap().expires = Instant::now();
        assert!(s.view().is_none());
        assert_eq!(s.admit(None), Ok(None));
        assert!(s.take("b", "p", 60).is_ok());
    }

    #[test]
    fn renew_and_release_need_the_token() {
        let s = LeaseSlot::default();
        let g = s.take("a", "p", 60).unwrap();
        assert!(matches!(s.renew(None, None), Err(LeaseError::Held(_))));
        assert!(matches!(s.release(Some("x")), Err(LeaseError::Held(_))));
        let v = s.renew(Some(&g.token), Some(120)).unwrap();
        assert_eq!(v.ttl_s, 120);
        s.release(Some(&g.token)).unwrap();
        assert_eq!(s.renew(Some(&g.token), None), Err(LeaseError::NoLease));
    }

    fn age(s: &LeaseSlot, by: Duration) {
        let mut inner = s.inner.lock().unwrap();
        let l = inner.lease.as_mut().unwrap();
        l.idle_since -= by;
    }

    #[test]
    fn renewing_every_minute_without_a_hardware_call_ends_at_30_minutes() {
        let s = LeaseSlot::default();
        let g = s.take("a", "p", 600).unwrap();
        for _ in 0..29 {
            age(&s, Duration::from_secs(60));
            s.renew(Some(&g.token), Some(600)).unwrap();
        }
        assert!(s.view().is_some());
        age(&s, Duration::from_secs(60));
        assert!(s.view().is_none());
        assert_eq!(s.admit(None), Ok(None));
    }

    #[test]
    fn a_token_call_restarts_the_clock_and_a_wrong_token_does_not() {
        let s = LeaseSlot::default();
        s.take("a", "p", 7200).unwrap();
        assert_eq!(s.view().unwrap().last_hardware_call_at, None);
        age(&s, IDLE_RELEASE - Duration::from_secs(10));
        assert!(s.admit(Some("wrong")).is_err());
        assert_eq!(s.view().unwrap().last_hardware_call_at, None);
        age(&s, Duration::from_secs(20));
        assert!(s.view().is_none(), "a refused call must not restart the clock");

        let g2 = s.take("a", "p", 7200).unwrap();
        age(&s, IDLE_RELEASE - Duration::from_secs(10));
        s.admit(Some(&g2.token)).unwrap();
        age(&s, IDLE_RELEASE - Duration::from_secs(10));
        let v = s.view().expect("the call restarted the clock");
        assert!(v.last_hardware_call_at.is_some());
    }

    #[test]
    fn a_study_in_flight_keeps_the_lease_and_the_clock_restarts_when_it_ends() {
        let study_lock: StudyLock = Arc::new(StdMutex::new(Some("study-1".into())));
        let s = LeaseSlot::watching(study_lock.clone());
        s.take("a", "p", 7200).unwrap();
        age(&s, IDLE_RELEASE * 2);
        assert!(s.view().is_some(), "kept while the run holds the study lock");
        s.study_ended();
        *study_lock.lock().unwrap() = None;
        age(&s, IDLE_RELEASE - Duration::from_secs(10));
        assert!(s.view().is_some());
        age(&s, Duration::from_secs(20));
        assert!(s.view().is_none());
    }

    #[test]
    fn a_renew_after_an_idle_release_names_it_and_the_holder() {
        let s = LeaseSlot::default();
        let g = s.take("alice", "p", 7200).unwrap();
        age(&s, IDLE_RELEASE);
        assert_eq!(s.renew(Some(&g.token), None), Err(LeaseError::IdleReleased("alice".into())));
        assert_eq!(s.absent(), LeaseError::IdleReleased("alice".into()));
        // A new take clears the reason.
        s.take("bob", "p", 60).unwrap();
        assert_eq!(s.absent(), LeaseError::NoLease);
    }

    #[test]
    fn ttl_and_holder_are_validated() {
        let s = LeaseSlot::default();
        assert!(matches!(s.take("a", "p", 0), Err(LeaseError::Invalid(_))));
        assert!(matches!(s.take("a", "p", MAX_TTL_S + 1), Err(LeaseError::Invalid(_))));
        assert!(matches!(s.take("  ", "p", 5), Err(LeaseError::Invalid(_))));
    }

    #[test]
    fn image_log_keeps_the_latest_per_chip() {
        let log = ImageLog::default();
        let img = |sha: &str| FlashedImage {
            chip: "c".into(),
            probe_serial: None,
            path: "p".into(),
            sha256: Some(sha.into()),
            flashed_at: String::new(),
            lease_holder: None,
        };
        log.record(img("1"));
        log.record(img("2"));
        let all = log.all();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].sha256.as_deref(), Some("2"));
        assert!(!all[0].flashed_at.is_empty());
    }

    /// A fixture hex: 4 bytes at 0x1000, 2 more at 0x2000.
    fn fixture_hex() -> Vec<u8> {
        // Built, not hand-summed: a checksum is the two's complement of the line's sum.
        fn line(addr: u16, ty: u8, data: &[u8]) -> String {
            let mut b = vec![data.len() as u8, (addr >> 8) as u8, addr as u8, ty];
            b.extend_from_slice(data);
            let sum = b.iter().fold(0u8, |a, x| a.wrapping_add(*x));
            format!(":{}{:02X}\n", b.iter().map(|x| format!("{x:02X}")).collect::<String>(), 0u8.wrapping_sub(sum))
        }
        [line(0x1000, 0, &[1, 2, 3, 4]), line(0x2000, 0, &[9, 8]), line(0, 1, &[])].concat().into_bytes()
    }

    fn flashed(chip: &str, sha: &str) -> FlashedImage {
        FlashedImage {
            chip: chip.into(),
            probe_serial: Some("0010".into()),
            path: "p".into(),
            sha256: Some(sha.into()),
            flashed_at: String::new(),
            lease_holder: Some("me".into()),
        }
    }

    #[test]
    fn a_flashed_hex_is_recorded_with_its_segments_identity_and_scope_and_survives_a_restart() {
        let dir = tempfile::tempdir().unwrap();
        let log = ImageLog::at(dir.path());
        let detail = image_detail(Some(&fixture_hex()), "hex", None);
        let segs = detail.segments.clone().unwrap();
        assert_eq!(segs.len(), 2);
        assert_eq!((segs[0].address, segs[0].length), (0x1000, 4));
        assert!(detail.identity.is_some());
        log.record_detail(flashed("nRF54L15", "aa"), ImageDetail { role: Some("dut".into()), ..detail.clone() });
        log.commit("nRF54L15", "agent", Some(7));

        let again = ImageLog::at(dir.path());
        let e = &again.entries()[0];
        assert_eq!((e.chip.as_str(), e.role.as_deref(), e.scope.as_str(), e.call_log_seq), ("nRF54L15", Some("dut"), "agent", Some(7)));
        assert_eq!(e.segments, detail.segments);
        assert_eq!(e.identity, detail.identity);
        assert_eq!(e.probe_serial.as_deref(), Some("0010"));
        assert!(e.at_utc_ms > 0);
        assert_eq!(again.all(), log.all());
    }

    #[test]
    fn a_reopened_log_keeps_the_latest_entry_per_chip_and_skips_a_bad_line() {
        let dir = tempfile::tempdir().unwrap();
        let log = ImageLog::at(dir.path());
        log.record(flashed("c", "1"));
        log.commit("c", "agent", Some(1));
        log.record(flashed("c", "2"));
        log.commit("c", "person", Some(2));
        let path = dir.path().join(FLASHED_IMAGES_FILE);
        let mut text = std::fs::read_to_string(&path).unwrap();
        text.push_str("not json\n");
        std::fs::write(&path, text).unwrap();
        let again = ImageLog::at(dir.path());
        assert_eq!(again.all().len(), 1);
        assert_eq!(again.all()[0].sha256.as_deref(), Some("2"));
        assert_eq!(again.entries()[0].scope, "person");
    }

    #[test]
    fn an_unreadable_format_has_no_segments() {
        assert!(image_detail(Some(b"x"), "uf2", None).segments.is_none());
        assert!(image_detail(None, "hex", None).segments.is_none());
        assert!(image_detail(Some(b"zzz"), "hex", None).segments.is_none());
    }

    #[test]
    fn status_shape_is_the_six_original_fields() {
        let log = ImageLog::default();
        log.record_detail(flashed("c", "1"), image_detail(Some(&fixture_hex()), "hex", None));
        let v = serde_json::to_value(&log.all()[0]).unwrap();
        let mut keys: Vec<_> = v.as_object().unwrap().keys().cloned().collect();
        keys.sort();
        assert_eq!(keys, ["chip", "flashed_at", "lease_holder", "path", "probe_serial", "sha256"]);
    }
}
