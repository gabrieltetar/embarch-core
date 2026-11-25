//! `measurements.json`: the derived measurements a study declares, computed over
//! its own captures as it ends (decision 113, `interfaces/measurements.md`).
//!
//! The shared crate owns the reducers and the declaration shape
//! (`embarch-study-designer` decisions 92 to 94, 99 and 100); this module owns
//! what only Core knows: which clock the points are on, who produced a tap's
//! bytes, what the bench declared, which token made each declaration, and
//! whether a capture is short.
//!
//! **A study that declares no measurement gets no file.** The record's
//! `behaviours` is then `{}` as before, and nothing here runs.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use embarch_study_designer::reducers::{Basis, Incomplete, Input, Marker, Measurement, Point, Registry, USource, Window};
use embarch_study_designer::streams::{StreamEncoding, StreamRef, StreamSource, StreamTap};
use embarch_study_designer::{MeasurementDecl, Study};
use serde_json::{json, Value};

use crate::signing::Declarations;
use crate::stream_store::{self, StreamIndexEntry};
use crate::token_store::Scope;

/// The file's name in a run's directory.
pub const FILE: &str = "measurements.json";

/// `measurements.json`'s own `schema`.
const SCHEMA: u64 = 1;

/// The coverage factor a declared `U` is taken at when the study names none
/// (the reducers' default).
const DEFAULT_K: f64 = embarch_study_designer::reducers::DEFAULT_K;

/// A declared sample rate and the count agree within this relative tolerance,
/// plus [`RATE_EDGE_SAMPLES`] samples' worth of it (decision 113).
const RATE_TOLERANCE_BASE_REL: f64 = 0.05;

/// The window's two ends are stamped by whole reads, so each can be off by one
/// sample: the tolerance widens by `2 / (n - 1)`.
const RATE_EDGE_SAMPLES: f64 = 2.0;

/// Who produced a signal's bytes, as the topology declares it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SignalOrigin {
    /// An enrollment role, `dut` or another.
    Role(String),
    /// A bench board, by its probe serial.
    Board(String),
}

/// What a finished study hands this module.
pub struct Ended<'a> {
    pub study: &'a Study,
    pub results_dir: &'a Path,
    /// One per declared tap, in declaration order, with each record check folded in.
    pub refs: &'a [StreamRef],
    pub run_id: &'a str,
    pub submitted_by: Scope,
    pub ended_utc_ms: u64,
    pub declarations: &'a Declarations,
    /// Resolves a signal tap's name to where it leaves; `None` when undeclared.
    pub signal_origin: &'a dyn Fn(&str) -> Option<SignalOrigin>,
}

/// Computes and writes `measurements.json`; `Ok(None)` when the study declares none.
pub fn write(ended: &Ended) -> Result<Option<PathBuf>> {
    if ended.study.measurements.is_empty() {
        return Ok(None);
    }
    let doc = compute(ended)?;
    let path = ended.results_dir.join(FILE);
    let tmp = ended.results_dir.join(format!("{FILE}.partial"));
    std::fs::write(&tmp, serde_json::to_vec(&doc)?).with_context(|| format!("failed to write {}", tmp.display()))?;
    std::fs::rename(&tmp, &path).with_context(|| format!("failed to finalize {}", path.display()))?;
    Ok(Some(path))
}

/// One integer where the value is one, because RFC 8785 prints `5`, not `5.0`.
fn num(x: f64) -> Value {
    if x.is_finite() && x.fract() == 0.0 && x.abs() < 1e15 {
        Value::from(x as i64)
    } else {
        serde_json::Number::from_f64(x).map_or(Value::Null, Value::Number)
    }
}

fn opt_num(x: Option<f64>) -> Value {
    x.map_or(Value::Null, num)
}

/// Who measured, from the tap that carries the bytes and never from the study.
enum Producer {
    Dut { role: &'static str },
    DutSignal { signal: String, role: String },
    Bench { name: String, signal: Option<String> },
}

fn producer(tap: &StreamTap, ended: &Ended) -> Producer {
    match &tap.source {
        StreamSource::GattNotify { .. } => Producer::Dut { role: embarch_topology::hardware::DUT_ROLE },
        StreamSource::GattTranscript
        | StreamSource::DevBenchLog
        | StreamSource::PowerFrontEnd { .. }
        | StreamSource::AnalogInput { .. } => {
            Producer::Bench { name: "dev-bench".to_string(), signal: None }
        }
        StreamSource::Signal { name } => match (ended.signal_origin)(name.as_str()) {
            Some(SignalOrigin::Role(role)) if role == embarch_topology::hardware::DUT_ROLE => {
                Producer::DutSignal { signal: name.as_str().to_string(), role }
            }
            Some(SignalOrigin::Role(role)) => Producer::Bench { name: role, signal: Some(name.as_str().to_string()) },
            Some(SignalOrigin::Board(serial)) => Producer::Bench { name: serial, signal: Some(name.as_str().to_string()) },
            // Not declared any more: nobody vouches for it as the bench's.
            None => Producer::DutSignal { signal: name.as_str().to_string(), role: String::new() },
        },
    }
}

/// What one tap's capture gives the reducers.
struct Series {
    points: Vec<Point>,
    /// Marker candidates: a `Text` tap's lines with Core's receipt stamp.
    lines: Vec<(u64, String)>,
    source: Value,
    gap: bool,
}

fn compute(ended: &Ended) -> Result<Value> {
    let streams_dir = ended.results_dir.join(stream_store::STREAMS_DIR);
    let index = stream_store::read_index(&streams_dir)?.context("the run has no streams/index.json")?;
    let registry = Registry::builtin();

    // Every `Text` tap's lines: where a marker can come from.
    let mut text_lines: Vec<(u64, String)> = Vec::new();
    for tap in ended.study.streams.iter().filter(|t| matches!(t.encoding, StreamEncoding::Text)) {
        if let Some(entry) = index.streams.iter().find(|e| e.id == tap.id) {
            text_lines.extend(read_text(&streams_dir, entry).lines);
        }
    }

    let mut behaviours: BTreeMap<String, u32> = BTreeMap::new();
    let mut out: Vec<Value> = Vec::new();
    for decl in ended.study.measurements.iter() {
        let tap = ended.study.streams.iter().find(|t| t.id == decl.tap).context("a declaration names a tap the study lacks")?;
        let entry = index.streams.iter().find(|e| e.id == tap.id).context("the index lacks a declared tap")?;
        let stream_ref = ended.refs.get(usize::from(tap.id));

        let series = read_series(&streams_dir, tap, entry, decl, ended.study);
        let prod = producer(tap, ended);
        let is_dut = !matches!(prod, Producer::Bench { .. });

        let unit = unit_of(tap, decl);
        let honoured = decl.declared.as_ref().filter(|_| is_dut);
        let k = honoured.and_then(|d| d.k).unwrap_or(DEFAULT_K);
        let declared_u = honoured.and_then(|d| d.u_expanded);

        let (from, to) = match (series.points.first(), series.points.last()) {
            (Some(a), Some(b)) => (a.t_ms, b.t_ms),
            _ => (ended.ended_utc_ms, ended.ended_utc_ms),
        };
        let wanted = |m: &Option<heapless::String<{ embarch_study_designer::limits::MAX_MARKER_NAME_LEN }>>| m.as_ref().map(|s| s.as_str().to_string());
        let (from_marker, to_marker) = (wanted(&decl.window.from_marker), wanted(&decl.window.to_marker));
        let markers: Vec<Marker> = [&from_marker, &to_marker]
            .into_iter()
            .flatten()
            .filter_map(|name| first_marker(&text_lines, name))
            .collect();

        let mut incomplete = Vec::new();
        if stream_ref.is_some_and(|r| r.truncated) {
            incomplete.push(Incomplete::TruncatedCapture);
        }
        if series.gap || stream_ref.and_then(|r| r.records.as_ref()).is_some_and(|r| !r.all_verified()) {
            incomplete.push(Incomplete::RecordGap);
        }

        let input = Input {
            points: series.points.clone(),
            markers,
            unit,
            u: declared_u.map(|u| u / k),
            clock_u_ms: None,
            k,
            incomplete,
            window: Window {
                from_utc_ms: Some(from),
                to_utc_ms: Some(to),
                percentile: decl.window.percentile,
                from_marker,
                to_marker,
            },
        };
        let m = registry.reduce_latest(decl.reducer.as_str(), &input).map_err(|e| anyhow::anyhow!("{e:?}"))?;
        behaviours.insert(decl.reducer.as_str().to_string(), m.method.behaviour);

        let mut declared_by = Vec::new();
        let scope = |what: &str, name: &str| ended.declarations.scope_of(what, name).unwrap_or(Scope::Agent).as_str();
        match &prod {
            Producer::Dut { role } => declared_by.push(json!({"what": "role", "name": role, "scope": scope("role", role)})),
            Producer::DutSignal { signal, role } => {
                declared_by.push(json!({"what": "signal", "name": signal, "scope": scope("signal", signal)}));
                if !role.is_empty() {
                    declared_by.push(json!({"what": "role", "name": role, "scope": scope("role", role)}));
                }
            }
            Producer::Bench { signal: Some(signal), .. } => {
                declared_by.push(json!({"what": "signal", "name": signal, "scope": scope("signal", signal)}));
            }
            Producer::Bench { .. } => {}
        }
        let rate_declared = decl.declared.as_ref().and_then(|d| d.sample_rate_hz);
        if honoured.is_some_and(|d| d.resolution.is_some() || d.u_expanded.is_some()) || rate_declared.is_some() {
            declared_by.push(json!({"what": "study_value", "name": decl.name.as_str(), "scope": ended.submitted_by.as_str()}));
        }

        out.push(measurement_json(decl, &m, &series, &prod, honoured.map(|d| (d.resolution, declared_u, k)), rate_declared, to, declared_by));
    }

    Ok(json!({
        "schema": SCHEMA,
        "run_id": ended.run_id,
        "behaviours": behaviours,
        "measurements": out,
    }))
}

fn unit_of(tap: &StreamTap, decl: &MeasurementDecl) -> String {
    if let Some(u) = &decl.unit {
        return u.as_str().to_string();
    }
    match tap.encoding {
        StreamEncoding::Samples { unit: embarch_study_designer::Unit::Milliamps, .. } => "mA",
        StreamEncoding::Samples { unit: embarch_study_designer::Unit::Volts, .. } => "V",
        StreamEncoding::Samples { unit: embarch_study_designer::Unit::Milliwatts, .. } => "mW",
        _ => "1",
    }
    .to_string()
}

#[allow(clippy::too_many_arguments)]
fn measurement_json(
    decl: &MeasurementDecl,
    m: &Measurement,
    series: &Series,
    prod: &Producer,
    honoured: Option<(Option<f64>, Option<f64>, f64)>,
    rate_declared: Option<f64>,
    at_utc_ms: u64,
    declared_by: Vec<Value>,
) -> Value {
    let (by, instrument, bench_image) = match prod {
        Producer::Bench { name, .. } => (
            "bench",
            json!({"name": name, "kind": "board", "identity": null}),
            json!("unverified"),
        ),
        _ => ("dut", Value::Null, Value::Null),
    };

    let study_u = honoured.and_then(|(_, u, _)| u).is_some() && m.uncertainty.is_some();
    let u_source = if study_u {
        "study"
    } else {
        match m.u_source {
            USource::Derived => "derived",
            USource::Undeclared => "undeclared",
            USource::None => "none",
        }
    };
    let uncertainty = m.uncertainty.as_ref().map_or(Value::Null, |u| {
        let basis = if study_u {
            "declared"
        } else {
            match u.basis {
                Basis::Datasheet => "datasheet",
                Basis::Calibration => "calibration",
                Basis::Characterisation => "characterisation",
                Basis::Declared => "declared",
                Basis::Derived => "derived",
            }
        };
        let mut v = json!({"U": num(u.u_expanded), "k": num(u.k), "basis": basis});
        if let Some(a) = u.type_a {
            v["type_a"] = num(a);
        }
        v
    });

    let mut window = serde_json::Map::new();
    let w = &m.method.window;
    if let Some(x) = w.from_utc_ms {
        window.insert("from_utc_ms".into(), json!(x));
    }
    if let Some(x) = w.to_utc_ms {
        window.insert("to_utc_ms".into(), json!(x));
    }
    if let Some(x) = w.percentile {
        window.insert("percentile".into(), num(x));
    }
    if let Some(x) = &w.from_marker {
        window.insert("from_marker".into(), json!(x));
    }
    if let Some(x) = &w.to_marker {
        window.insert("to_marker".into(), json!(x));
    }

    let incomplete: Vec<&str> = m
        .incomplete
        .iter()
        .map(|i| match i {
            Incomplete::TruncatedCapture => "truncated_capture",
            Incomplete::TruncatedReply => "truncated_reply",
            Incomplete::RecordGap => "record_gap",
            Incomplete::ParseFailed => "parse_failed",
        })
        .collect();

    let mut v = json!({
        "name": decl.name.as_str(),
        "value": opt_num(m.value),
        "unit": m.unit,
        "at_utc_ms": at_utc_ms,
        "source": series.source,
        "by": by,
        "instrument": instrument,
        "bench_image": bench_image,
        "method": {
            "kind": "reducer",
            "name": m.method.name,
            "behaviour": m.method.behaviour,
            "window": Value::Object(window),
        },
        "resolution": opt_num(honoured.and_then(|(r, _, _)| r)),
        "uncertainty": uncertainty,
        "u_source": u_source,
        "declared_by": declared_by,
        "incomplete": incomplete,
    });
    if let Some(declared_hz) = rate_declared {
        v["rate_check"] = rate_check(declared_hz, &series.points);
    }
    v
}

/// `rate_check` (decision 113): the count is `(n - 1) / T` over the first and
/// last stamp, which is unbiased for a window bounded by two samples, and it
/// agrees when it is within `0.05 + 2 / (n - 1)` of the declared rate.
fn rate_check(declared_hz: f64, points: &[Point]) -> Value {
    let n = points.len();
    let window_ms = match (points.first(), points.last()) {
        (Some(a), Some(b)) => b.t_ms.saturating_sub(a.t_ms),
        _ => 0,
    };
    if n < 2 || window_ms == 0 {
        return json!({
            "declared_hz": num(declared_hz), "counted_hz": 0, "window_ms": window_ms,
            "samples": n, "tolerance_rel": num(RATE_TOLERANCE_BASE_REL), "agree": false,
        });
    }
    let counted = (n - 1) as f64 / (window_ms as f64 / 1000.0);
    let tolerance = RATE_TOLERANCE_BASE_REL + RATE_EDGE_SAMPLES / (n - 1) as f64;
    let agree = ((counted - declared_hz) / declared_hz).abs() <= tolerance;
    json!({
        "declared_hz": num(declared_hz), "counted_hz": num(counted), "window_ms": window_ms,
        "samples": n, "tolerance_rel": num(tolerance), "agree": agree,
    })
}

/// The earliest line, over every `Text` tap, whose first word is `name`.
/// `u_ms` is undeclared: no bench board declares a stamp's uncertainty.
fn first_marker(lines: &[(u64, String)], name: &str) -> Option<Marker> {
    lines
        .iter()
        .filter(|(_, text)| text.split_whitespace().next() == Some(name))
        .map(|(t, _)| *t)
        .min()
        .map(|t_ms| Marker { name: name.to_string(), t_ms, u_ms: None })
}

fn read_series(streams_dir: &Path, tap: &StreamTap, entry: &StreamIndexEntry, decl: &MeasurementDecl, study: &Study) -> Series {
    match tap.encoding {
        StreamEncoding::Samples { .. } => read_csv(streams_dir, entry, Some("value")),
        StreamEncoding::Struct { decoder } => {
            let field = decl.field.as_ref().map(|f| f.as_str().to_string());
            let _ = (decoder, study);
            read_csv(streams_dir, entry, field.as_deref())
        }
        StreamEncoding::GattTranscript => read_csv(streams_dir, entry, None),
        StreamEncoding::Text => read_text(streams_dir, entry),
        // No decoded rows to count: a capture of raw bytes or an outpost trace.
        StreamEncoding::Raw | StreamEncoding::OutpostTrace => {
            let len = stream_store::capture_len(streams_dir, &entry.raw_file).unwrap_or(0);
            Series { points: Vec::new(), lines: Vec::new(), source: json!({"file": format!("streams/{}", entry.raw_file), "bytes": [0, len]}), gap: false }
        }
    }
}

/// A rendered CSV's rows as points, on Core's receipt stamp (the last column,
/// `core_rx_utc_ms`). `column` names the value; `None` counts rows (value 0).
fn read_csv(streams_dir: &Path, entry: &StreamIndexEntry, column: Option<&str>) -> Series {
    let file = entry.rendered_file.clone().unwrap_or_else(|| entry.raw_file.clone());
    let source = |rows: usize, bytes: usize| {
        if rows == 0 {
            json!({"file": format!("streams/{file}"), "bytes": [0, bytes]})
        } else {
            json!({"file": format!("streams/{file}"), "rows": [0, rows - 1]})
        }
    };
    let bytes = stream_store::read_capture(streams_dir, &file, true).ok().flatten().unwrap_or_default();
    let text = String::from_utf8_lossy(&bytes);
    let mut lines = text.lines();
    let header: Vec<&str> = lines.next().unwrap_or("").split(',').collect();
    let value_at = column.and_then(|c| {
        // A struct's own columns follow `rep_index`, so a field that shares a
        // fixed column's name is never read from there.
        let from = header.iter().position(|h| *h == "rep_index").map_or(0, |i| i + 1);
        header.iter().skip(from).position(|h| *h == c).map(|i| i + from)
    });
    let mut points = Vec::new();
    let mut rows = 0;
    let mut gap = false;
    for line in lines.filter(|l| !l.is_empty()) {
        rows += 1;
        let cells: Vec<&str> = line.split(',').collect();
        let Some(t_ms) = cells.last().and_then(|c| c.parse::<u64>().ok()) else {
            gap = true;
            continue;
        };
        let value = match value_at {
            Some(i) => cells.get(i).and_then(|c| c.parse::<f64>().ok()).unwrap_or(f64::NAN),
            None => 0.0,
        };
        points.push(Point { t_ms, value });
    }
    Series { points, lines: Vec::new(), source: source(rows, bytes.len()), gap }
}

/// A `Text` tap's lines, each stamped with the arrival row covering its first
/// byte. Lines Core has no stamp for are left out (a gap).
fn read_text(streams_dir: &Path, entry: &StreamIndexEntry) -> Series {
    let raw = stream_store::read_capture(streams_dir, &entry.raw_file, false).ok().flatten().unwrap_or_default();
    let source = json!({"file": format!("streams/{}", entry.raw_file), "bytes": [0, raw.len()]});
    let Some(arrival_file) = &entry.arrival_file else {
        return Series { points: Vec::new(), lines: Vec::new(), source, gap: !raw.is_empty() };
    };
    let arrivals = stream_store::read_capture(streams_dir, arrival_file, true).ok().flatten().unwrap_or_default();
    let (arrivals, _) = stream_store::rebase_byte_arrivals(&arrivals, raw.len() as u64);
    let stamps: Vec<(u64, u64)> = String::from_utf8_lossy(&arrivals)
        .lines()
        .skip(1)
        .filter_map(|l| {
            let mut it = l.splitn(3, ',');
            Some((it.next()?.parse().ok()?, it.next()?.parse().ok()?))
        })
        .collect();

    let mut lines = Vec::new();
    let mut gap = false;
    let mut start = 0usize;
    for piece in raw.split_inclusive(|b| *b == b'\n') {
        let text = String::from_utf8_lossy(piece).trim_end_matches(['\r', '\n']).to_string();
        let at = start as u64;
        start += piece.len();
        if text.is_empty() {
            continue;
        }
        // The latest row starting at or before the line.
        match stamps.iter().rev().find(|(off, _)| *off <= at) {
            Some((_, t)) => lines.push((*t, text)),
            None => gap = true,
        }
    }
    let points = lines.iter().map(|(t, _)| Point { t_ms: *t, value: 0.0 }).collect();
    Series { points, lines, source, gap }
}

#[cfg(test)]
mod tests {
    use super::*;
    use embarch_study_designer::bounded::Bounded;
    use embarch_study_designer::decoder::{ScalarType, StructField, StructLayout};
    use embarch_study_designer::study::{DevBenchLogLevel, Requirements};
    use embarch_study_designer::{DeclaredWindow, StreamScope, StudyDeclared, Unit};
    use embarch_study_designer::streams::SampleLayout;

    fn s<const N: usize>(text: &str) -> heapless::String<N> {
        heapless::String::try_from(text).unwrap()
    }

    fn tap(id: u8, name: &str, source: StreamSource, encoding: StreamEncoding) -> StreamTap {
        StreamTap { id, name: s(name), source, encoding, scope: StreamScope::WholeStudy }
    }

    fn decl(name: &str, reducer: &str, tap: u8) -> MeasurementDecl {
        MeasurementDecl {
            name: s(name),
            reducer: s(reducer),
            tap,
            field: None,
            unit: None,
            window: DeclaredWindow::default(),
            declared: None,
        }
    }

    /// Tap 0: samples in mA from the dev-bench power front end; tap 1: a DUT
    /// console on signal `console`; tap 2: a struct of the DUT's own over GATT.
    fn study(decls: Vec<MeasurementDecl>) -> Study {
        let mut streams = heapless::Vec::new();
        streams
            .push(tap(0, "current", StreamSource::PowerFrontEnd { sample_hz: 10 }, StreamEncoding::Samples { layout: SampleLayout::F32Le, unit: Unit::Milliamps, channel_id: 0 }))
            .unwrap();
        streams.push(tap(1, "console", StreamSource::Signal { name: s("console") }, StreamEncoding::Text)).unwrap();
        streams
            .push(tap(2, "temp", StreamSource::GattNotify { service_uuid: embarch_study_designer::Uuid::parse("6e400001-b5a3-f393-e0a9-e50e24dcca9e").unwrap(), characteristic_uuid: embarch_study_designer::Uuid::parse("6e400003-b5a3-f393-e0a9-e50e24dcca9e").unwrap() }, StreamEncoding::Struct { decoder: 0 }))
            .unwrap();
        let mut header = heapless::Vec::new();
        header.push(StructField { name: s("celsius"), ty: ScalarType::U8 }).unwrap();
        let mut decoders = Bounded::new();
        decoders.push(StructLayout { name: s("reading"), header, repeat: heapless::Vec::new(), chart_field: None }).unwrap();
        let mut measurements = Bounded::new();
        for d in decls {
            measurements.push(d).unwrap();
        }
        Study {
            name: s("m"),
            requires: Requirements::any(),
            steps: Bounded::new(),
            streams,
            steps_crc: 0,
            streams_crc: 0,
            protocols: Default::default(),
            protocols_crc: 0,
            dev_bench_log_level: DevBenchLogLevel::default(),
            decoders,
            record_checks: Default::default(),
            session: None,
            measurements,
        }
    }

    /// A results directory as a finished run leaves it, one fixture per tap.
    fn results() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let streams = dir.path().join("streams");
        std::fs::create_dir_all(&streams).unwrap();
        let entry = |id: u8, name: &str, raw: &str, rendered: Option<&str>, arrival: Option<&str>, enc: &StreamEncoding| {
            json!({"id": id, "name": name, "raw_file": raw, "rendered_file": rendered, "arrival_file": arrival, "encoding": enc})
        };
        let index = json!({"version": 1, "streams": [
            entry(0, "current", "current.bin", Some("current.csv"), None, &StreamEncoding::Samples { layout: SampleLayout::F32Le, unit: Unit::Milliamps, channel_id: 0 }),
            entry(1, "console", "console.txt", None, Some("console.arrival.csv"), &StreamEncoding::Text),
            entry(2, "temp", "temp.bin", Some("temp.csv"), None, &StreamEncoding::Struct { decoder: 0 }),
        ]});
        std::fs::write(streams.join("index.json"), serde_json::to_vec(&index).unwrap()).unwrap();
        // Five samples, 100 ms apart: 10 Hz.
        let mut csv = String::from("rx_utc_ms,step_name,value,unit,channel_id,core_rx_utc_ms\n");
        for (i, v) in [10.0, 12.0, 11.0, 13.0, 14.0].iter().enumerate() {
            csv += &format!("{i},run,{v},mA,0,{}\n", 1_000 + 100 * i);
        }
        std::fs::write(streams.join("current.csv"), csv).unwrap();
        std::fs::write(streams.join("current.bin"), b"x").unwrap();
        std::fs::write(streams.join("console.txt"), "boot\nready now\nnoise\ndone\n").unwrap();
        std::fs::write(streams.join("console.arrival.csv"), "byte_offset,core_rx_utc_ms,bytes\n0,1000,5\n5,1150,10\n15,1300,11\n").unwrap();
        let mut t = String::from("rx_utc_ms,step_index,step_name,rep_index,celsius,payload_hex,decode_note,core_rx_utc_ms\n");
        for (i, v) in [20, 22].iter().enumerate() {
            t += &format!("{i},0,run,0,{v},00,,{}\n", 1_000 + 500 * i);
        }
        std::fs::write(streams.join("temp.csv"), t).unwrap();
        dir
    }

    fn refs(truncated: bool) -> Vec<StreamRef> {
        ["current", "console", "temp"]
            .iter()
            .enumerate()
            .map(|(i, n)| StreamRef { name: s(n), bytes_written: 1, truncated: truncated && i == 0, records: None })
            .collect()
    }

    fn run(study: &Study, dir: &Path, refs: &[StreamRef], decls: &Declarations) -> Value {
        let origin = |name: &str| (name == "console").then(|| SignalOrigin::Role("dut".to_string()));
        let ended = Ended { study, results_dir: dir, refs, run_id: "r1", submitted_by: Scope::Person, ended_utc_ms: 9_000, declarations: decls, signal_origin: &origin };
        write(&ended).unwrap().unwrap();
        serde_json::from_slice(&std::fs::read(dir.join(FILE)).unwrap()).unwrap()
    }

    #[test]
    fn a_recorded_capture_yields_the_expected_file() {
        let dir = results();
        let mut mean = decl("avg", "mean", 0);
        mean.unit = None;
        let mut interval = decl("boot_to_done", "interval", 1);
        interval.window.from_marker = Some(s("boot"));
        interval.window.to_marker = Some(s("done"));
        let st = study(vec![decl("n", "count", 0), mean, interval]);
        let decls = Declarations::new();
        decls.record("signal", "console", Scope::Agent);
        let doc = run(&st, dir.path(), &refs(false), &decls);

        assert_eq!(doc["schema"], 1);
        assert_eq!(doc["run_id"], "r1");
        assert_eq!(doc["behaviours"], json!({"count": 1, "interval": 1, "mean": 1}));
        let m = doc["measurements"].as_array().unwrap();
        assert_eq!(m.iter().map(|x| x["name"].as_str().unwrap()).collect::<Vec<_>>(), ["n", "avg", "boot_to_done"]);

        // count: an integer, no uncertainty, the bench's.
        assert_eq!(m[0]["value"], 5);
        assert_eq!(m[0]["unit"], "1");
        assert_eq!(m[0]["by"], "bench");
        assert_eq!(m[0]["bench_image"], "unverified");
        assert_eq!(m[0]["instrument"], json!({"name": "dev-bench", "kind": "board", "identity": null}));
        assert_eq!(m[0]["u_source"], "none");
        assert_eq!(m[0]["source"], json!({"file": "streams/current.csv", "rows": [0, 4]}));
        assert_eq!(m[0]["at_utc_ms"], 1_400);

        // mean of 10, 12, 11, 13, 14.
        assert_eq!(m[1]["value"], 12);
        assert_eq!(m[1]["unit"], "mA");
        assert_eq!(m[1]["method"]["window"], json!({"from_utc_ms": 1000, "to_utc_ms": 1400}));
        assert_eq!(m[1]["incomplete"], json!([]));
        assert_eq!(m[1]["declared_by"], json!([]));

        // interval: the first line of each name, stamped by the read that carried it.
        assert_eq!(m[2]["value"], 1_300 - 1_000);
        assert_eq!(m[2]["unit"], "ms");
        assert_eq!(m[2]["by"], "dut");
        assert_eq!(m[2]["instrument"], Value::Null);
        assert_eq!(m[2]["bench_image"], Value::Null);
        assert_eq!(m[2]["declared_by"], json!([
            {"what": "signal", "name": "console", "scope": "agent"},
            {"what": "role", "name": "dut", "scope": "agent"}
        ]));
        assert_eq!(m[2]["source"], json!({"file": "streams/console.txt", "bytes": [0, 26]}));
    }

    #[test]
    fn a_value_from_an_uncharacterised_bench_board_reads_u_source_undeclared() {
        let dir = results();
        // The study declares a `U` for a bench value; a study cannot mark the bench's word.
        let mut mean = decl("avg", "mean", 0);
        mean.declared = Some(StudyDeclared { resolution: Some(0.1), u_expanded: Some(0.5), k: None, sample_rate_hz: None });
        let st = study(vec![mean]);
        let doc = run(&st, dir.path(), &refs(false), &Declarations::new());
        let m = &doc["measurements"][0];
        assert_eq!(m["by"], "bench");
        assert_eq!(m["u_source"], "undeclared");
        assert_eq!(m["uncertainty"], Value::Null);
        assert_eq!(m["resolution"], Value::Null);
        assert_eq!(m["declared_by"], json!([]));
    }

    #[test]
    fn a_dut_value_takes_the_study_declared_u_and_says_so() {
        let dir = results();
        let mut max = decl("peak", "max", 2);
        max.field = Some(s("celsius"));
        max.unit = Some(s("V"));
        max.declared = Some(StudyDeclared { resolution: Some(1.0), u_expanded: Some(2.0), k: Some(2.0), sample_rate_hz: None });
        let st = study(vec![max]);
        let doc = run(&st, dir.path(), &refs(false), &Declarations::new());
        let m = &doc["measurements"][0];
        assert_eq!(m["by"], "dut");
        assert_eq!(m["value"], 22);
        assert_eq!(m["u_source"], "study");
        assert_eq!(m["uncertainty"], json!({"U": 2, "k": 2, "basis": "declared"}));
        assert_eq!(m["resolution"], 1);
        // Never watched: read as the agent's.
        assert_eq!(m["declared_by"], json!([
            {"what": "role", "name": "dut", "scope": "agent"},
            {"what": "study_value", "name": "peak", "scope": "person"}
        ]));
    }

    #[test]
    fn a_declared_rate_is_checked_against_the_count() {
        let dir = results();
        let mut rate = decl("r", "rate", 0);
        rate.declared = Some(StudyDeclared { sample_rate_hz: Some(10.0), ..Default::default() });
        let mut wrong = decl("w", "count", 0);
        wrong.declared = Some(StudyDeclared { sample_rate_hz: Some(100.0), ..Default::default() });
        let st = study(vec![rate, wrong]);
        let doc = run(&st, dir.path(), &refs(false), &Declarations::new());
        let ok = &doc["measurements"][0]["rate_check"];
        assert_eq!(ok["declared_hz"], 10);
        assert_eq!(ok["counted_hz"], 10);
        assert_eq!(ok["window_ms"], 400);
        assert_eq!(ok["samples"], 5);
        assert_eq!(ok["agree"], true);
        assert_eq!(doc["measurements"][1]["rate_check"]["agree"], false);
        assert_eq!(doc["measurements"][0]["declared_by"], json!([{"what": "study_value", "name": "r", "scope": "person"}]));
    }

    #[test]
    fn a_truncated_capture_and_a_missing_marker_are_said() {
        let dir = results();
        let mut at = decl("at", "value_at_marker", 0);
        at.unit = None;
        at.window.from_marker = Some(s("never_printed"));
        let st = study(vec![decl("n", "count", 0), at]);
        let doc = run(&st, dir.path(), &refs(true), &Declarations::new());
        assert_eq!(doc["measurements"][0]["incomplete"], json!(["truncated_capture"]));
        assert_eq!(doc["measurements"][1]["value"], Value::Null);
        assert_eq!(doc["measurements"][1]["incomplete"], json!(["truncated_capture", "parse_failed"]));
    }

    #[test]
    fn a_study_with_no_declared_measurement_writes_no_file() {
        let dir = results();
        let st = study(vec![]);
        let decls = Declarations::new();
        let origin = |_: &str| None;
        let r = refs(false);
        let ended = Ended { study: &st, results_dir: dir.path(), refs: &r, run_id: "r", submitted_by: Scope::Agent, ended_utc_ms: 1, declarations: &decls, signal_origin: &origin };
        assert!(write(&ended).unwrap().is_none());
        assert!(!dir.path().join(FILE).exists());
    }

    #[test]
    fn a_whole_number_is_an_integer_so_the_file_is_canonical() {
        assert_eq!(num(5.0).to_string(), "5");
        assert_eq!(num(49.8).to_string(), "49.8");
        assert_eq!(opt_num(None), Value::Null);
    }
}
