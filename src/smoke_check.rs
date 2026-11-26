//! The smoke check before a bench's first test (decision 143): the bench is declared, every board's
//! identity validates, each bench board answers its hello line, each instrument answers `*IDN?`,
//! and the images read back. [`check_bench`] is the policy over a [`BenchAccess`], so every failure
//! is tested over a fake; the real access is `CoreEnv`'s in `test_executor.rs`.
//!
//! A failing check names its piece and fails the bench: the executor skips the bench's tests as an
//! `error` with reason `smoke_check`. Every check runs, so the findings name every failing piece.

use embarch_study_designer::hil_expand::RunBench;
use serde::Serialize;

use crate::readback::ImageStatus;

/// The note a run carries when the DUT validated but its hardware ID could not be read, so the
/// unit cannot be kept the same from bench to bench (decision 143).
pub const UNIT_IDENTITY_NOT_READ: &str = "unit_identity_not_read";

/// What the bench's tests reach, read from their studies and the topology.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BenchNeeds {
    /// Signal names the studies' `Exchange` steps name, and each bench board's part id.
    pub signals: Vec<String>,
    /// Enrollment roles the declared signals leave from, other than the DUT's.
    pub roles: Vec<String>,
    /// Instrument names the studies' `Instrument` steps name.
    pub instruments: Vec<String>,
}

/// What a check reaches, so the policy runs without a board.
pub trait BenchAccess {
    /// Whether a signal is declared; `Err` when the table could not be read.
    fn signal_declared(&mut self, name: &str) -> Result<bool, String>;
    /// Whether an enrollment role is declared.
    fn role_declared(&mut self, role: &str) -> Result<bool, String>;
    /// Whether an instrument is declared.
    fn instrument_declared(&mut self, name: &str) -> Result<bool, String>;
    /// `validate` for the DUT: its hardware ID, empty when it could not be read.
    fn dut_identity(&mut self) -> Result<String, String>;
    /// `validate` for a bench board, by its part id: its hardware ID.
    fn board_identity(&mut self, part: &str) -> Result<String, String>;
    /// The board's reply to its hello line; `Err` when it was silent.
    fn hello(&mut self, part: &str, hello: &str) -> Result<String, String>;
    /// The instrument's `*IDN?` reply; `Err` when it was silent.
    fn idn(&mut self, instrument: &str) -> Result<String, String>;
    /// The image readback for the DUT (`None`) or a bench board (its part id).
    fn image(&mut self, part: Option<&str>) -> ImageStatus;
}

/// One board's identity as validate read it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct IdentityFinding {
    pub piece: String,
    pub hardware_id: String,
}

/// One instrument's `*IDN?` reply.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct IdnFinding {
    pub instrument: String,
    pub reply: String,
}

/// One board's image status.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ImageFinding {
    pub piece: String,
    #[serde(flatten)]
    pub status: ImageStatus,
}

/// Everything one bench's check found, kept on the run's progress and outcome.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct SmokeFindings {
    pub bench: String,
    pub identities: Vec<IdentityFinding>,
    /// The bench boards that answered their hello line, with the reply.
    pub hellos: Vec<IdnFinding>,
    pub instruments: Vec<IdnFinding>,
    pub images: Vec<ImageFinding>,
    /// The DUT's hardware ID: the unit the run keeps the same across benches.
    pub unit: Option<String>,
    /// Facts that are not failures, such as [`UNIT_IDENTITY_NOT_READ`].
    pub notes: Vec<String>,
    /// Each failing piece, `piece: why`. Empty when the bench passed.
    pub failures: Vec<String>,
}

impl SmokeFindings {
    pub fn new(bench: &str) -> Self {
        SmokeFindings { bench: bench.to_string(), ..Default::default() }
    }

    /// What the error of a skipped test says: the failing pieces.
    pub fn failure_text(&self) -> String {
        format!("the smoke check of bench '{}' failed: {}", self.bench, self.failures.join("; "))
    }
}

fn push_unique(v: &mut Vec<String>, s: &str) {
    if !v.iter().any(|x| x == s) {
        v.push(s.to_string());
    }
}

/// Runs every check of `bench` over `access`. A signal, role or instrument that is not declared
/// is a failing piece and its own probe is not tried.
pub fn check_bench(access: &mut dyn BenchAccess, bench: &RunBench, needs: &BenchNeeds) -> SmokeFindings {
    let mut f = SmokeFindings::new(&bench.name);
    let mut declared_signals: Vec<String> = Vec::new();
    for s in &needs.signals {
        match access.signal_declared(s) {
            Ok(true) => push_unique(&mut declared_signals, s),
            Ok(false) => f.failures.push(format!("signal '{s}': not declared")),
            Err(e) => f.failures.push(format!("signal '{s}': {e}")),
        }
    }
    for r in &needs.roles {
        match access.role_declared(r) {
            Ok(true) => {}
            Ok(false) => f.failures.push(format!("role '{r}': not declared")),
            Err(e) => f.failures.push(format!("role '{r}': {e}")),
        }
    }
    let mut declared_instruments: Vec<String> = Vec::new();
    for i in &needs.instruments {
        match access.instrument_declared(i) {
            Ok(true) => push_unique(&mut declared_instruments, i),
            Ok(false) => f.failures.push(format!("instrument '{i}': not declared")),
            Err(e) => f.failures.push(format!("instrument '{i}': {e}")),
        }
    }

    match access.dut_identity() {
        Ok(id) if id.is_empty() => {
            f.notes.push(UNIT_IDENTITY_NOT_READ.to_string());
            f.identities.push(IdentityFinding { piece: "the DUT".into(), hardware_id: String::new() });
        }
        Ok(id) => {
            f.unit = Some(id.clone());
            f.identities.push(IdentityFinding { piece: "the DUT".into(), hardware_id: id });
        }
        Err(e) => f.failures.push(format!("the DUT: validate failed: {e}")),
    }
    match access.image(None) {
        ImageStatus::Differs { address, length } => {
            f.failures.push(format!("the DUT: its image differs from what Core flashed (first at {address:#x}, {length} bytes)"));
            f.images.push(ImageFinding { piece: "the DUT".into(), status: ImageStatus::Differs { address, length } });
        }
        other => f.images.push(ImageFinding { piece: "the DUT".into(), status: other }),
    }

    for b in &bench.boards {
        let piece = format!("bench board '{}'", b.part);
        if !declared_signals.contains(&b.part) {
            // Its own signal is the failing piece, already named above.
            continue;
        }
        match access.board_identity(&b.part) {
            Ok(id) => f.identities.push(IdentityFinding { piece: piece.clone(), hardware_id: id }),
            Err(e) => f.failures.push(format!("{piece}: validate failed: {e}")),
        }
        match access.hello(&b.part, &b.hello) {
            Ok(reply) => f.hellos.push(IdnFinding { instrument: b.part.clone(), reply }),
            Err(e) => f.failures.push(format!("{piece}: no answer to its hello line: {e}")),
        }
        // A bench board's `unreleased`, `unknown` and `unverified` are recorded, never a failure
        // (decision 143).
        f.images.push(ImageFinding { piece, status: access.image(Some(&b.part)) });
    }

    for i in &declared_instruments {
        match access.idn(i) {
            Ok(reply) => f.instruments.push(IdnFinding { instrument: i.clone(), reply }),
            Err(e) => f.failures.push(format!("instrument '{i}': no answer to *IDN?: {e}")),
        }
    }
    f
}

/// A scripted bench access, shared with the executor's tests.
#[cfg(test)]
pub(crate) mod testing {
    use super::*;

    #[derive(Default, Clone)]
    pub struct Fake {
        pub missing_signal: Option<&'static str>,
        pub dut_id: Option<Result<String, String>>,
        pub silent_hello: bool,
        pub silent_idn: bool,
        pub dut_image: Option<ImageStatus>,
        pub board_image: Option<ImageStatus>,
    }

    impl BenchAccess for Fake {
        fn signal_declared(&mut self, name: &str) -> Result<bool, String> {
            Ok(self.missing_signal != Some(name))
        }
        fn role_declared(&mut self, _: &str) -> Result<bool, String> {
            Ok(true)
        }
        fn instrument_declared(&mut self, _: &str) -> Result<bool, String> {
            Ok(true)
        }
        fn dut_identity(&mut self) -> Result<String, String> {
            self.dut_id.clone().unwrap_or(Ok("DUT-1".into()))
        }
        fn board_identity(&mut self, part: &str) -> Result<String, String> {
            Ok(format!("{part}-id"))
        }
        fn hello(&mut self, _: &str, hello: &str) -> Result<String, String> {
            if self.silent_hello { Err("silent".into()) } else { Ok(format!("{hello}!")) }
        }
        fn idn(&mut self, i: &str) -> Result<String, String> {
            if self.silent_idn { Err("silent".into()) } else { Ok(format!("ACME,{i},S1,1.0")) }
        }
        fn image(&mut self, part: Option<&str>) -> ImageStatus {
            match part {
                None => self.dut_image.clone().unwrap_or(ImageStatus::Holds),
                Some(_) => self.board_image.clone().unwrap_or(ImageStatus::Unknown),
            }
        }
    }

    pub fn needs() -> BenchNeeds {
        BenchNeeds { signals: vec!["pd".into()], roles: vec![], instruments: vec!["dmm".into()] }
    }
}

#[cfg(test)]
mod tests {
    use super::testing::{needs, Fake};
    use super::*;
    use embarch_study_designer::hil_expand::RunBoard;

    fn bench() -> RunBench {
        RunBench { name: "A".into(), tests: vec![], boards: vec![RunBoard { part: "pd".into(), idle: "idle".into(), hello: "hello".into() }] }
    }

    fn failing(mut f: Fake) -> Vec<String> {
        check_bench(&mut f, &bench(), &needs()).failures
    }

    #[test]
    fn a_passing_check_records_every_identity_reply_and_image_status() {
        let mut f = Fake::default();
        let got = check_bench(&mut f, &bench(), &needs());
        assert!(got.failures.is_empty(), "{:?}", got.failures);
        assert_eq!(got.unit.as_deref(), Some("DUT-1"));
        assert_eq!(got.identities.iter().map(|i| (i.piece.as_str(), i.hardware_id.as_str())).collect::<Vec<_>>(), [("the DUT", "DUT-1"), ("bench board 'pd'", "pd-id")]);
        assert_eq!(got.hellos[0].reply, "hello!");
        assert_eq!(got.instruments, [IdnFinding { instrument: "dmm".into(), reply: "ACME,dmm,S1,1.0".into() }]);
        assert_eq!(got.images.iter().map(|i| i.status.clone()).collect::<Vec<_>>(), [ImageStatus::Holds, ImageStatus::Unknown]);
    }

    #[test]
    fn each_failure_names_its_piece() {
        assert_eq!(failing(Fake { missing_signal: Some("pd"), ..Default::default() }), ["signal 'pd': not declared"]);
        assert_eq!(failing(Fake { dut_id: Some(Err("mismatch".into())), ..Default::default() }), ["the DUT: validate failed: mismatch"]);
        let hello = failing(Fake { silent_hello: true, ..Default::default() });
        assert!(hello[0].starts_with("bench board 'pd': no answer to its hello line"), "{hello:?}");
        let idn = failing(Fake { silent_idn: true, ..Default::default() });
        assert!(idn[0].starts_with("instrument 'dmm': no answer to *IDN?"), "{idn:?}");
        let image = failing(Fake { dut_image: Some(ImageStatus::Differs { address: 0x1000, length: 16 }), ..Default::default() });
        assert!(image[0].starts_with("the DUT: its image differs"), "{image:?}");
    }

    #[test]
    fn a_claim_and_a_bench_boards_unreleased_image_are_recorded_not_failed() {
        let f = Fake { dut_image: Some(ImageStatus::Claim { reason: "x".into() }), board_image: Some(ImageStatus::Unreleased { scope: "person".into() }), ..Default::default() };
        assert!(failing(f).is_empty());
    }

    #[test]
    fn a_dut_whose_id_cannot_be_read_carries_unit_identity_not_read() {
        let mut f = Fake { dut_id: Some(Ok(String::new())), ..Default::default() };
        let got = check_bench(&mut f, &bench(), &needs());
        assert_eq!((got.unit, got.notes, got.failures.is_empty()), (None, vec![UNIT_IDENTITY_NOT_READ.to_string()], true));
    }
}
