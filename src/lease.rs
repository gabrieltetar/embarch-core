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

use serde::Serialize;
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant};
use time::OffsetDateTime;

/// The request header that carries the lease token.
pub const LEASE_HEADER: &str = "x-embarch-lease";

/// Longest TTL a lease may ask for, and the default a renew without a
/// `ttl_s` reuses is the lease's own. Reasoned, not measured: the longest
/// field hold was 1.4 h (5,040 s), so four hours covers a long session
/// without letting a forgotten lease park the bench for a day.
pub const MAX_TTL_S: u64 = 4 * 60 * 60;

#[derive(Debug, Clone)]
struct Lease {
    token: String,
    holder: String,
    purpose: String,
    ttl_s: u64,
    started_at: OffsetDateTime,
    expires_at: OffsetDateTime,
    expires: Instant,
}

/// What a caller may see of a lease: everything except the token.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct LeaseView {
    pub holder: String,
    pub purpose: String,
    pub started_at: String,
    pub expires_at: String,
    pub ttl_s: u64,
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
    Held(LeaseView),
    /// Nothing is live: never taken, released, expired, or Core restarted.
    NoLease,
    Invalid(String),
}

#[derive(Clone, Default)]
pub struct LeaseSlot(Arc<StdMutex<Option<Lease>>>);

impl LeaseSlot {
    /// The live lease, clearing one that has expired.
    fn live(slot: &mut Option<Lease>) -> Option<&mut Lease> {
        if slot.as_ref().is_some_and(|l| Instant::now() >= l.expires) {
            if let Some(l) = slot.take() {
                tracing::info!("lease held by '{}' expired", l.holder);
            }
        }
        slot.as_mut()
    }

    pub fn view(&self) -> Option<LeaseView> {
        Self::live(&mut self.0.lock().unwrap()).map(|l| l.view())
    }

    pub fn take(&self, holder: &str, purpose: &str, ttl_s: u64) -> Result<Granted, LeaseError> {
        let holder = holder.trim();
        if holder.is_empty() {
            return Err(LeaseError::Invalid("holder must not be empty".into()));
        }
        let ttl_s = check_ttl(ttl_s)?;
        let mut slot = self.0.lock().unwrap();
        if let Some(l) = Self::live(&mut slot) {
            return Err(LeaseError::Held(l.view()));
        }
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
        };
        tracing::info!("lease taken by '{}' ({}), ttl {ttl_s}s", lease.holder, lease.purpose);
        let view = lease.view();
        *slot = Some(lease);
        Ok(Granted { token, lease: view })
    }

    /// Extends the live lease to `ttl_s` from now (its own TTL when `None`).
    pub fn renew(&self, token: Option<&str>, ttl_s: Option<u64>) -> Result<LeaseView, LeaseError> {
        let mut slot = self.0.lock().unwrap();
        let l = Self::live(&mut slot).ok_or(LeaseError::NoLease)?;
        if token != Some(l.token.as_str()) {
            return Err(LeaseError::Held(l.view()));
        }
        let ttl_s = check_ttl(ttl_s.unwrap_or(l.ttl_s))?;
        let d = Duration::from_secs(ttl_s);
        l.ttl_s = ttl_s;
        l.expires_at = OffsetDateTime::now_utc() + d;
        l.expires = Instant::now() + d;
        Ok(l.view())
    }

    pub fn release(&self, token: Option<&str>) -> Result<(), LeaseError> {
        let mut slot = self.0.lock().unwrap();
        let l = Self::live(&mut slot).ok_or(LeaseError::NoLease)?;
        if token != Some(l.token.as_str()) {
            return Err(LeaseError::Held(l.view()));
        }
        tracing::info!("lease released by '{}'", l.holder);
        *slot = None;
        Ok(())
    }

    /// `Ok(holder)` when the request may touch hardware: no lease is live
    /// (`None`), or it carries the live lease's token (`Some(holder)`).
    pub fn admit(&self, token: Option<&str>) -> Result<Option<String>, LeaseView> {
        let mut slot = self.0.lock().unwrap();
        match Self::live(&mut slot) {
            None => Ok(None),
            Some(l) if token == Some(l.token.as_str()) => Ok(Some(l.holder.clone())),
            Some(l) => Err(l.view()),
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
/// the bench can tell whether it must reflash. Keyed by chip. In memory: a
/// Core restart forgets it, and an absent entry means "unknown", not "none".
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

#[derive(Clone, Default)]
pub struct ImageLog(Arc<StdMutex<BTreeMap<String, FlashedImage>>>);

impl ImageLog {
    pub fn record(&self, mut image: FlashedImage) {
        image.flashed_at = rfc3339(OffsetDateTime::now_utc());
        self.0.lock().unwrap().insert(image.chip.clone(), image);
    }

    pub fn all(&self) -> Vec<FlashedImage> {
        self.0.lock().unwrap().values().cloned().collect()
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
    ("GET", "/serial-ports"),
    ("GET", "/alerts"),
    ("GET", "/logs/recent"),
    ("GET", "/studies"),
    ("GET", "/study/{study_id}"),
    ("GET", "/study/{study_id}/events"),
    ("GET", "/study/{study_id}/steps"),
    ("GET", "/study/{study_id}/streams"),
    ("GET", "/study/{study_id}/stream/{name}"),
    ("GET", "/study/{study_id}/stream/{name}/arrivals"),
    ("GET", "/study/{study_id}/stream/{name}/load"),
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
        s.0.lock().unwrap().as_mut().unwrap().expires = Instant::now();
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
}
