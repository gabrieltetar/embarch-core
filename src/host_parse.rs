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

/// Writes `measurements.json` from a host run's parses; `Ok(None)` when it had none.
pub fn write(
    dir: &Path,
    run_id: &str,
    collected: &[Collected],
    signal_origin: &dyn Fn(&str) -> Option<SignalOrigin>,
    declarations: &Declarations,
) -> Result<Option<PathBuf>> {
    if collected.is_empty() {
        return Ok(None);
    }
    let doc = json!({
        "schema": 1,
        "run_id": run_id,
        // A parse is no reducer: it carries no behaviour version.
        "behaviours": {},
        "measurements": collected.iter().map(|c| measurement(c, signal_origin, declarations)).collect::<Vec<_>>(),
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

fn measurement(c: &Collected, signal_origin: &dyn Fn(&str) -> Option<SignalOrigin>, declarations: &Declarations) -> Value {
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
        "resolution": null,
        // A parse declares none, and an instrument's metrology does not reach it yet
        // (EMB-358/EMB-360): undeclared, for a DUT's value and a bench's alike.
        "uncertainty": null,
        "u_source": "undeclared",
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

    #[test]
    fn the_record_carries_the_name_and_value_and_the_incomplete_only_when_there_is_one() {
        let v = serde_json::to_value(read(&vbus(), "vbus: 5 mV", false, Addressed::Signal("pd".into()))).unwrap();
        assert_eq!(v, json!({"name": "vbus", "value": 5.0}));
        let v = serde_json::to_value(read(&vbus(), "x", false, Addressed::Signal("pd".into()))).unwrap();
        assert_eq!(v, json!({"name": "vbus", "value": null, "incomplete": ["parse_failed"]}));
    }
}
