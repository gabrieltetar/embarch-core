//! A host step's declared number (core decision 131): read out of the reply at step time, recorded
//! beside the raw reply in `events.json`, and written as `measurements.json` when the study ends.
//!
//! The parser is `embarch-study-designer`'s [`parse_step_number`], so `verify` re-parsing the
//! recorded reply cannot disagree. Who measured comes from what the step addressed, never from the
//! study: this module owns that, and the file's shape; `measurements.rs` owns the rest.

use std::path::{Path, PathBuf};

use anyhow::Result;
use embarch_study_designer::reducers::Incomplete;
use embarch_study_designer::host_study::{parse_step_number, StepParse};
use embarch_study_designer::IterationRef;
use serde::Serialize;
use serde_json::{json, Value};

use crate::measurements::{incomplete_str, num, SignalOrigin};
use crate::signing::Declarations;
use embarch_topology::hardware::Instrument;
use crate::token_store::Scope;

/// What a step addressed: the one thing that says who measured.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Addressed {
    Signal(String),
    Instrument(String),
}

/// One step's parse, as the step's record in `events.json` carries it (`parsed`), plus what the
/// measurement needs and the record does not repeat.
#[derive(Debug, Clone, Serialize)]
pub struct Parsed {
    pub name: String,
    /// `None` is `null`: the pattern found no number.
    pub value: Option<f64>,
    /// Empty when complete (then omitted): `truncated_reply`, `parse_failed`.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub incomplete: Vec<Incomplete>,
    #[serde(skip)]
    pub pattern: String,
    #[serde(skip)]
    pub unit: String,
    #[serde(skip)]
    pub addressed: Addressed,
}

/// Parses the reply the step recorded. A cut reply is marked `truncated_reply` whatever the
/// parser finds in it; a number in a cut reply may itself be cut, so the mark is the verdict
/// rule's to act on, not this module's to hide.
pub fn read(parse: &StepParse, reply: &str, reply_truncated: bool, addressed: Addressed) -> Parsed {
    let mut incomplete = Vec::new();
    if reply_truncated {
        incomplete.push(Incomplete::TruncatedReply);
    }
    let value = match parse_step_number(reply, parse) {
        Ok(v) => Some(v),
        Err(e) => {
            incomplete.push(e);
            None
        }
    };
    Parsed { name: parse.name.clone(), value, incomplete, pattern: parse.pattern.clone(), unit: parse.unit.clone(), addressed }
}

/// A parse as the run collected it: where it came from in the record.
pub struct Collected {
    pub step: String,
    pub iteration: Option<IterationRef>,
    pub at_utc_ms: u64,
    pub parsed: Parsed,
}

/// An observation prompt's answer (core decision 138), as the step's record carries it
/// (`observed`) and as `measurements.json` writes it.
#[derive(Debug, Clone, Serialize)]
pub struct Observed {
    pub name: String,
    pub value: crate::host_study::ObservedValue,
    /// The declared unit of a number; `None` for a choice.
    #[serde(skip)]
    pub unit: Option<String>,
}

/// An observation as the run collected it.
pub struct CollectedObservation {
    pub step: String,
    pub iteration: Option<IterationRef>,
    pub at_utc_ms: u64,
    pub observed: Observed,
}

/// A person's answer as a measurement: `by: person`, `method {kind: "prompt", prompt_kind:
/// "observation"}`, declared by the person's own token (only that token can answer one).
fn observation(c: &CollectedObservation) -> Value {
    let o = &c.observed;
    let mut source = json!({"file": "events.json", "step": c.step});
    if let Some(it) = &c.iteration {
        source["iteration"] = json!(it);
    }
    json!({
        "name": o.name,
        "value": serde_json::to_value(&o.value).unwrap_or(Value::Null),
        "unit": o.unit,
        "at_utc_ms": c.at_utc_ms,
        "source": source,
        "by": "person",
        "instrument": Value::Null,
        "bench_image": Value::Null,
        "method": {"kind": "prompt", "prompt_kind": "observation"},
        "resolution": Value::Null,
        "uncertainty": Value::Null,
        "u_source": "undeclared",
        "declared_by": [{"what": "prompt", "name": c.step, "scope": Scope::Person.as_str()}],
        "incomplete": Vec::<&str>::new(),
    })
}

/// Writes `measurements.json` from a host run's parses and observations, in the order they ran
/// (parses first); `Ok(None)` when it had none.
pub fn write(
    dir: &Path,
    run_id: &str,
    collected: &[Collected],
    observed: &[CollectedObservation],
    signal_origin: &dyn Fn(&str) -> Option<SignalOrigin>,
    instrument_of: &dyn Fn(&str) -> Option<Instrument>,
    declarations: &Declarations,
) -> Result<Option<PathBuf>> {
    if collected.is_empty() && observed.is_empty() {
        return Ok(None);
    }
    let doc = json!({
        "schema": 1,
        "run_id": run_id,
        // A parse is no reducer: it carries no behaviour version.
        "behaviours": {},
        "measurements": collected.iter().map(|c| measurement(c, signal_origin, instrument_of, declarations)).chain(observed.iter().map(observation)).collect::<Vec<_>>(),
    });
    crate::measurements::write_file(dir, &doc).map(Some)
}

/// `by`, `instrument`, `bench_image`, from the signal's origin or the instrument named.
fn producer(addressed: &Addressed, signal_origin: &dyn Fn(&str) -> Option<SignalOrigin>) -> (&'static str, Value, Value) {
    let board = |name: String| ("bench", json!({"name": name, "kind": "board", "identity": null}), json!("unverified"));
    match addressed {
        // A bench instrument is no board: no image status.
        Addressed::Instrument(name) => ("bench", json!({"name": name, "kind": "instrument", "identity": null}), Value::Null),
        Addressed::Signal(name) => match signal_origin(name) {
            Some(SignalOrigin::Role(role)) if role != embarch_topology::hardware::DUT_ROLE => board(role),
            Some(SignalOrigin::Board(serial)) => board(serial),
            // The DUT role, or a signal no longer declared: nobody vouches for it as the bench's.
            _ => ("dut", Value::Null, Value::Null),
        },
    }
}

/// The metrology entry that speaks for a parse: the one whose unit the parse reads in, when
/// exactly one does. Two entries in one unit (two quantities) leave no honest pick, so none.
fn metrology_for<'a>(inst: &'a Instrument, unit: &str) -> Option<&'a embarch_topology::hardware::instrument::Metrology> {
    let mut hits = inst.metrology.iter().filter(|m| m.unit == unit);
    let first = hits.next()?;
    hits.next().is_none().then_some(first)
}

/// `instrument.calibration` as a measurement carries it: the declared block, with the serial
/// marked unchecked until the smoke check's `*IDN?` reply can be compared (EMB-368).
fn calibration_json(c: &embarch_topology::hardware::instrument::Calibration) -> Value {
    let mut v = json!({"serial": c.serial, "certificate_ref": c.certificate_ref, "due": c.due, "serial_checked": false});
    if let Some(h) = &c.certificate_sha256 {
        v["certificate_sha256"] = json!(h);
    }
    v
}

fn measurement(
    c: &Collected,
    signal_origin: &dyn Fn(&str) -> Option<SignalOrigin>,
    instrument_of: &dyn Fn(&str) -> Option<Instrument>,
    declarations: &Declarations,
) -> Value {
    let p = &c.parsed;
    let (by, instrument, bench_image) = producer(&p.addressed, signal_origin);
    let mut source = json!({"file": "events.json", "step": c.step});
    if let Some(it) = &c.iteration {
        source["iteration"] = json!(it);
    }
    let scope = |what: &str, name: &str| declarations.scope_of(what, name).unwrap_or(Scope::Agent).as_str();
    let mut declared_by = Vec::new();
    if let Addressed::Signal(signal) = &p.addressed {
        declared_by.push(json!({"what": "signal", "name": signal, "scope": scope("signal", signal)}));
        if by == "dut" {
            let role = embarch_topology::hardware::DUT_ROLE;
            declared_by.push(json!({"what": "role", "name": role, "scope": scope("role", role)}));
        }
    }
    let (mut resolution, mut uncertainty, mut u_source) = (Value::Null, Value::Null, "undeclared");
    let mut instrument = instrument;
    if let Addressed::Instrument(name) = &p.addressed {
        declared_by.push(json!({"what": "instrument", "name": name, "scope": scope("instrument", name)}));
        if let Some(inst) = instrument_of(name) {
            if let Some(m) = metrology_for(&inst, &p.unit) {
                resolution = num(m.resolution);
                let basis = serde_json::to_value(m.basis).unwrap_or(Value::Null);
                uncertainty = json!({"U": num(m.u_expanded), "k": num(m.k), "basis": basis});
                u_source = "source";
            }
            if let Some(cal) = &inst.calibration {
                instrument["calibration"] = calibration_json(cal);
            }
        }
    }
    json!({
        "name": p.name,
        "value": p.value.map_or(Value::Null, num),
        "unit": p.unit,
        "at_utc_ms": c.at_utc_ms,
        "source": source,
        "by": by,
        "instrument": instrument,
        "bench_image": bench_image,
        "method": {"kind": "parse", "pattern": p.pattern},
        // A bench instrument's value takes its metrology block (decision 140); a parse declares
        // none itself, so a DUT's value stays undeclared.
        "resolution": resolution,
        "uncertainty": uncertainty,
        "u_source": u_source,
        "declared_by": declared_by,
        "incomplete": p.incomplete.iter().map(|i| incomplete_str(*i)).collect::<Vec<_>>(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vbus() -> StepParse {
        StepParse { name: "vbus".into(), pattern: r"vbus: (\S+) mV".into(), unit: "mV".into() }
    }

    #[test]
    fn a_number_is_read_and_a_missing_one_is_parse_failed() {
        let ok = read(&vbus(), "pd: vbus: 5012 mV", false, Addressed::Signal("pd".into()));
        assert_eq!(ok.value, Some(5012.0));
        assert!(ok.incomplete.is_empty());
        let none = read(&vbus(), "pd: error", false, Addressed::Signal("pd".into()));
        assert_eq!(none.value, None);
        assert_eq!(none.incomplete, [Incomplete::ParseFailed]);
    }

    #[test]
    fn a_cut_reply_is_marked_and_still_parsed() {
        let cut = read(&vbus(), "vbus: 5012 mV", true, Addressed::Signal("pd".into()));
        assert_eq!(cut.value, Some(5012.0));
        assert_eq!(cut.incomplete, [Incomplete::TruncatedReply]);
        let both = read(&vbus(), "vbus: 50", true, Addressed::Signal("pd".into()));
        assert_eq!(both.incomplete, [Incomplete::TruncatedReply, Incomplete::ParseFailed]);
    }

    fn dmm(cal: bool) -> Instrument {
        use embarch_topology::hardware::instrument::{Calibration, MeasuringRange, Metrology, MetrologyBasis};
        let mut i: Instrument = serde_json::from_value(json!({"name": "dmm", "port": {"vid": 1, "pid": 2}, "baud": 9600})).unwrap();
        i.metrology = vec![Metrology {
            quantity: "voltage".into(),
            unit: "mV".into(),
            resolution: 0.1,
            u_expanded: 2.0,
            k: 2.0,
            basis: MetrologyBasis::Certificate,
            range: MeasuringRange { min: 0.0, max: 1000.0 },
        }];
        if cal {
            i.calibration = Some(Calibration { serial: "S1".into(), certificate_ref: "C-9".into(), due: "2026-01-01".into(), certificate_sha256: None });
        }
        i
    }

    fn bench_value(inst: Option<Instrument>, decls: &Declarations) -> Value {
        let c = Collected {
            step: "s".into(),
            iteration: None,
            at_utc_ms: 5,
            parsed: read(&vbus(), "vbus: 5 mV", false, Addressed::Instrument("dmm".into())),
        };
        measurement(&c, &|_| None, &|_| inst.clone(), decls)
    }

    #[test]
    fn an_instruments_metrology_and_calibration_reach_its_value() {
        let decls = Declarations::new();
        decls.record("instrument", "dmm", Scope::Person);
        let m = bench_value(Some(dmm(true)), &decls);
        assert_eq!(m["resolution"], 0.1);
        assert_eq!(m["uncertainty"], json!({"U": 2, "k": 2, "basis": "certificate"}));
        assert_eq!(m["u_source"], "source");
        assert_eq!(m["instrument"]["calibration"]["due"], "2026-01-01");
        assert_eq!(m["instrument"]["calibration"]["serial_checked"], false);
        assert_eq!(m["declared_by"], json!([{"what": "instrument", "name": "dmm", "scope": "person"}]));
    }

    #[test]
    fn an_instrument_without_metrology_or_calibration_stays_undeclared() {
        let mut bare = dmm(false);
        bare.metrology.clear();
        let m = bench_value(Some(bare), &Declarations::new());
        assert_eq!((m["u_source"].as_str(), m["uncertainty"].is_null()), (Some("undeclared"), true));
        assert!(m["instrument"].get("calibration").is_none());
        assert_eq!(m["declared_by"][0]["scope"], "agent");
    }

    #[test]
    fn the_record_carries_the_name_and_value_and_the_incomplete_only_when_there_is_one() {
        let v = serde_json::to_value(read(&vbus(), "vbus: 5 mV", false, Addressed::Signal("pd".into()))).unwrap();
        assert_eq!(v, json!({"name": "vbus", "value": 5.0}));
        let v = serde_json::to_value(read(&vbus(), "x", false, Addressed::Signal("pd".into()))).unwrap();
        assert_eq!(v, json!({"name": "vbus", "value": null, "incomplete": ["parse_failed"]}));
    }
}
