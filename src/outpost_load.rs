//! An outpost capture's own answer: per-subject load shares and the coverage
//! line, computed once, here, over the rendered `*.trace.csv`
//! `outpost_manifest::render` already writes beside the raw capture.
//!
//! [Suite decision 4](../../../embarch-doc/suite/decisions.md) settles
//! that this computation lives in `embarch-core` rather than only in
//! `embarch-ui`'s Trace tab, because Core is the only component on both the
//! agent's path (through `embarch-api`) and the human's (through `embarch-ui`'s
//! Core client), and the only one in the release archive. What follows is
//! ported from `embarch-ui/src/trace.rs`'s `parse`/`parse_with_cap` and
//! `summarize` — the CSV-to-timeline-to-repartition arithmetic — **not**
//! `embarch-ui`'s chart geometry: windowed binning and the `TraceView`
//! payload shape stay there per `embarch-ui` decision 18, and the study-step
//! row stays there per `embarch-ui` decision 10's chart half — neither is
//! reopened by this move.
//!
//! **This is deliberately not a second decoder.** Core already owns the one
//! decode of the raw outpost frames, in `outpost_manifest.rs`, and refuses to
//! render a manifest whose `record_layout_version` disagrees with the shared
//! crate's. This module never touches the raw bytes or the manifest: it reads
//! the CSV that decode already produced, and inherits `embarch-ui` decision 10
//! (trace)'s pin — the column list is checked against
//! [`embarch_study_designer::outpost::csv_header`] and refused if it differs
//! — for the reason [reversals row 86](../../../embarch-doc/reversals/rows-73-92.md)
//! gives: a wire change that moves a column is exactly the kind of drift two
//! independent hosts can each get wrong in a different way, and refusing to
//! guess is what a second host inheriting the pin buys.
//!
//! **This is, however, a second *implementation* of the timeline-building
//! arithmetic**, until the `embarch-ui` follow-up (queued, not filed until
//! this lands) makes the browser read this answer instead of computing its
//! own. Suite decision 4 accepts that duplication as a known, temporarily
//! tolerated cost with a decision pointing at it — the alternative, a shared
//! host-side analysis crate, was considered and rejected on cost, not
//! principle. Until that follow-up lands, a change to `RecordKind`, a gap
//! record's semantics, or the five-lies rules below has to be made in both
//! this file and `embarch-ui/src/trace.rs`.

use std::collections::HashMap;

use embarch_study_designer::outpost::{self, RecordKind};
use serde::Serialize;

/// Same cap and the same reason as `embarch-ui`'s `MAX_ROWS`: a study long
/// enough to overflow this is a real thing, and refusing to guess past it —
/// counting what was dropped rather than silently shortening the timeline —
/// is the honest answer.
const MAX_ROWS: usize = 250_000;

/// How long a leading stale prefix may be before this stops reading it as
/// one. See `embarch-ui/src/trace.rs`'s `STALE_PREFIX_MAX_ROWS` for the
/// measurement this is carried from: assumed, not measured, a bound on
/// damage rather than a detector.
const STALE_PREFIX_MAX_ROWS: usize = 512;

/// A backwards step between two DUT stamps larger than this means the DUT's
/// counter restarted, not that a hook's read-then-reserve ordering inverted
/// two adjacent stamps by a few microseconds. Carried verbatim from
/// `embarch-ui/src/trace.rs`'s `STALE_PREFIX_MIN_US`.
const STALE_PREFIX_MIN_US: u64 = 10_000;

/// The `idle` record's own lane key. Zephyr traces idle entry only — see
/// `embarch-ui/src/trace.rs`'s `IDLE_LANE` for the firmware's own comment on
/// why there is no exit hook to define.
const IDLE_LANE: &str = "cpu-idle";

/// One traced subject over time — a thread, the CPU's idle state, or one
/// interrupt vector. Carries only what [`summarize`] needs; the label
/// bookkeeping, point events and browser-search indices `embarch-ui`'s own
/// `Lane` carries alongside these are chart concerns and stay there.
///
/// `pub` and `Serialize` since `core/076`: this is also the wire shape
/// `GET .../load/spans` serves, unchanged from what `summarize` already
/// consumed — no second timeline type, per decision 65
/// (`decisions/stream-index.md`).
#[derive(Debug, Clone, Serialize)]
pub struct Lane {
    pub key: String,
    pub label: String,
    pub unnamed: bool,
    pub kind: &'static str,
    pub spans: Vec<Span>,
}

/// One interval a subject was running, in [`LoadSummary::unit`]s. Same shape
/// and same four doubts as `embarch-ui/src/trace.rs`'s `Span` — see there for
/// why each exists.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Span {
    pub from: u64,
    pub to: u64,
    pub open_start: bool,
    pub open_end: bool,
    pub crosses_gap: bool,
    pub below_resolution: bool,
}

/// Records the firmware itself reported dropping, and the interval they were
/// lost somewhere inside. Full parity with `embarch-ui/src/trace.rs`'s own
/// `Gap` since `embarch-core` decision 66 (`decisions/stream-index.md`) — see
/// there for why the DUT and host clocks bound `from`/`to` so differently, and
/// for each of the fields below.
#[derive(Debug, Clone, Copy, Serialize)]
pub struct Gap {
    pub from: u64,
    pub to: u64,
    pub records_lost: u32,
    /// The firmware's own cycle span between the first and last dropped
    /// record (`OUTPOST_KIND_GAP`'s `b`). Carried, not drawn — see
    /// `trace.rs`'s own field for why converting it would introduce a
    /// rounding error this struct exists to avoid.
    pub cycle_span: u32,
    pub frame_index: u64,
    /// This gap's position in the rendered CSV's row order (post-header),
    /// not its frame — a caller wanting a specific line back needs this,
    /// `frame_index` alone is not enough where one frame carries several rows.
    pub row_index: usize,
    /// `from == to` and the extent is genuinely unknown, not zero — only
    /// reachable for the capture's first record-carrying frame, which has no
    /// earlier arrival to bound it with.
    pub unbounded_start: bool,
}

/// One traced subject's share of the capture window — the "load repartition"
/// [`embarch-ui` decision 10 (trace)](../../../embarch-doc/embarch-ui/decisions/trace-view.md)
/// defines and this module now also computes. Identical shape and identical
/// exclusion rules to `embarch-ui/src/trace.rs`'s `LoadSubject`.
#[derive(Debug, Clone, Serialize)]
pub struct LoadSubject {
    pub key: String,
    pub label: String,
    pub unnamed: bool,
    pub kind: &'static str,
    pub entries: usize,
    pub measured_spans: usize,
    pub total_extent: u64,
    pub share: f64,
    pub excluded_spans: usize,
    pub excluded_extent: u64,
    pub gap_crossing_spans: usize,
    pub open_ended_spans: usize,
    pub open_started_spans: usize,
    pub below_resolution_spans: usize,
}

/// The whole capture's load repartition, plus the coverage line: how much of
/// the window is in a state a reader must doubt before trusting the rest.
/// Identical shape to `embarch-ui/src/trace.rs`'s `LoadSummary`.
#[derive(Debug, Clone, Serialize)]
pub struct LoadSummary {
    pub unit: &'static str,
    pub window_extent: u64,
    pub gap_extent: u64,
    pub gap_fraction: f64,
    pub records_lost: u64,
    pub has_time_base: bool,
    pub thread_extent: u64,
    pub idle_record_extent: u64,
    pub isr_extent: u64,
    pub unaccounted_extent: u64,
    pub below_resolution_spans: usize,
    pub subjects: Vec<LoadSubject>,
}

/// [`LoadSummary`], plus what a caller needs to judge whether this rendered
/// CSV was read in full.
#[derive(Debug, Clone, Serialize)]
pub struct LoadAnswer {
    pub rows: usize,
    /// Rows past [`MAX_ROWS`], never silently discarded from this count even
    /// though they were dropped from the computation.
    pub rows_dropped_by_cap: usize,
    pub row_cap: usize,
    /// Rows this parser refused: a line short of nine fields, or a
    /// `frame_index` that did not parse. Same meaning as `embarch-ui`'s
    /// `TraceView::rows_unparsed`.
    pub rows_unparsed: usize,
    pub summary: LoadSummary,
}

/// The decoded per-lane timeline [`load_answer`] builds and then discards
/// once [`summarize`] reduces it — served directly by `GET
/// .../load/spans` (`embarch-core` decision 65, `decisions/stream-index.md`)
/// for a caller (`embarch-ui`'s Trace tab) that wants the spans themselves,
/// not the repartition. Built from the same [`decode_with_cap`] call
/// [`load_answer`] uses — this is not a second decode.
#[derive(Debug, Clone, Serialize)]
pub struct SpansAnswer {
    /// Same meaning as [`LoadSummary::unit`]: which clock draws the axis
    /// `from`/`to` below are stamped in.
    pub unit: &'static str,
    /// The capture window's bounds, in `unit`s — [`Span::from`]/[`Span::to`]
    /// and [`Gap::from`]/[`Gap::to`] are absolute against these, unlike
    /// [`LoadSummary::window_extent`], which only carries their difference.
    pub t_from: u64,
    pub t_to: u64,
    pub records_lost: u64,
    pub rows: usize,
    pub rows_dropped_by_cap: usize,
    pub row_cap: usize,
    pub rows_unparsed: usize,
    pub gaps: Vec<Gap>,
    pub lanes: Vec<Lane>,
}

fn kind_of(name: &str) -> Option<RecordKind> {
    (0u8..=u8::MAX).find_map(|b| match RecordKind::from_byte(b) {
        Some(k) if k.as_str() == name => Some(k),
        _ => None,
    })
}

struct Row {
    frame_index: u64,
    rx_utc_ms: Option<u64>,
    dut_us: Option<u64>,
    dut_cycles: Option<u64>,
    kind: Option<RecordKind>,
    a: u32,
    b: u32,
    name: String,
    /// Filled in once the axis unit is known: `dut_us`, `rx_utc_ms` or
    /// `frame_index`.
    t: u64,
}

/// Splits one CSV line, honouring the double-quoting `name` may carry (a
/// resolved ISR label is `handler(inner_handler)`).
fn split_row(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut field = String::new();
    let mut quoted = false;
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '"' if quoted && chars.peek() == Some(&'"') => {
                field.push('"');
                chars.next();
            }
            '"' => quoted = !quoted,
            ',' if !quoted => out.push(std::mem::take(&mut field)),
            other => field.push(other),
        }
    }
    out.push(field);
    out
}

/// Everything the axis decision and the stale-prefix search need to know
/// about the DUT's clock over a set of rows.
struct DutClockHealth {
    /// How long these rows can possibly have taken — the other clock's span,
    /// or (with no host stamps at all) the DUT's own.
    bound: u64,
    /// A step longer than `bound`: two independent clocks contradicting each
    /// other, which means the DUT's counter restarted somewhere in this set
    /// of rows.
    broken: bool,
}

/// Reads the DUT's clock over `rows`, ignoring gap records: a gap record is
/// stamped when the first dropped record was lost, not when the drain thread
/// reported it, so its stamp legitimately sits outside the run its own frame
/// carries.
fn dut_clock_health(rows: &[Row]) -> DutClockHealth {
    let mut step_max = 0u64;
    let mut prev: Option<u64> = None;
    for r in rows.iter().filter(|r| r.kind != Some(RecordKind::Gap)) {
        if let Some(us) = r.dut_us {
            if let Some(p) = prev {
                step_max = step_max.max(us.abs_diff(p));
            }
            prev = Some(us);
        }
    }
    let span = |vals: &mut dyn Iterator<Item = u64>| {
        let (lo, hi) = vals.fold((u64::MAX, 0u64), |(lo, hi), v| (lo.min(v), hi.max(v)));
        hi.saturating_sub(if lo == u64::MAX { 0 } else { lo })
    };
    let dut_span = span(&mut rows.iter().filter_map(|r| r.dut_us));
    let host_span = span(&mut rows.iter().filter_map(|r| r.rx_utc_ms)).saturating_mul(1000);
    let bound = if host_span > 0 { host_span } else { dut_span };
    DutClockHealth { bound, broken: step_max > 0 && step_max > bound }
}

/// Where the capture's own stream starts, when it opens with records from
/// before a DUT reset — the index of the first row to keep, and how far the
/// prefix's clock sat from it. Ported verbatim (algorithm, not comments) from
/// `embarch-ui/src/trace.rs`'s `stale_prefix_end`; see there for the full
/// argument this only runs on an already-refused clock.
fn stale_prefix_end(rows: &[Row]) -> Option<(usize, u64)> {
    if !dut_clock_health(rows).broken {
        return None;
    }
    let dated: Vec<usize> = rows
        .iter()
        .enumerate()
        .filter(|(_, r)| r.kind != Some(RecordKind::Gap) && r.dut_us.is_some())
        .map(|(i, _)| i)
        .collect();
    let us = |k: usize| rows[dated[k]].dut_us.unwrap_or(0);
    for k in 1..dated.len() {
        if k > STALE_PREFIX_MAX_ROWS || k > dated.len() - k || dated.len() - k < 2 {
            break;
        }
        let step = us(k).abs_diff(us(k - 1));
        if step <= STALE_PREFIX_MIN_US {
            continue;
        }
        let after = dut_clock_health(&rows[dated[k]..]);
        if step > after.bound && !after.broken {
            return Some((dated[k], step));
        }
    }
    None
}

/// One closed span. `below_resolution` is computed here, in the one place
/// that knows both ends' frames *and* which clock is drawing the axis.
fn close(from: (u64, u64, bool), to_t: u64, to_frame: u64, open_end: bool, unit: &'static str) -> Span {
    let (from_t, from_frame, open_start) = from;
    Span {
        from: from_t,
        to: to_t,
        open_start,
        open_end,
        crosses_gap: false,
        below_resolution: unit != "us" && from_frame == to_frame,
    }
}

/// Merges gap bands into a set of disjoint intervals clamped to the capture
/// window, so overlapping bands are counted once.
fn merged_gap_extent(gaps: &[Gap], from: u64, to: u64) -> u64 {
    let mut bands: Vec<(u64, u64)> = gaps
        .iter()
        .filter_map(|g| {
            let lo = g.from.max(from);
            let hi = g.to.min(to);
            (lo < hi).then_some((lo, hi))
        })
        .collect();
    bands.sort_unstable();
    let mut total = 0u64;
    let mut cur: Option<(u64, u64)> = None;
    for (lo, hi) in bands {
        match cur {
            Some((clo, chi)) if lo <= chi => cur = Some((clo, chi.max(hi))),
            Some((clo, chi)) => {
                total += chi - clo;
                cur = Some((lo, hi));
            }
            None => cur = Some((lo, hi)),
        }
    }
    if let Some((clo, chi)) = cur {
        total += chi - clo;
    }
    total
}

/// Computes the load repartition. Pure arithmetic over already-built lanes —
/// it re-derives nothing about the trace, which is why every caveat it
/// reports is one [`Span`] already carried. Ported verbatim from
/// `embarch-ui/src/trace.rs`'s `summarize`.
fn summarize(lanes: &[Lane], gaps: &[Gap], unit: &'static str, t_from: u64, t_to: u64, records_lost: u64) -> LoadSummary {
    let window_extent = t_to.saturating_sub(t_from);
    let share_of = |c: u64| if window_extent == 0 { 0.0 } else { c as f64 / window_extent as f64 };

    let mut subjects: Vec<LoadSubject> = lanes
        .iter()
        .map(|lane| {
            let mut total_extent = 0u64;
            let mut excluded_extent = 0u64;
            let (mut measured, mut excluded) = (0usize, 0usize);
            let (mut crossing, mut open_end, mut open_start, mut below_res) = (0usize, 0usize, 0usize, 0usize);
            for span in &lane.spans {
                let extent = span.to.saturating_sub(span.from);
                if span.crosses_gap {
                    crossing += 1;
                }
                if span.open_end {
                    open_end += 1;
                }
                if span.open_start {
                    open_start += 1;
                }
                if span.below_resolution {
                    below_res += 1;
                }
                if span.crosses_gap || span.open_end || span.open_start || span.below_resolution {
                    excluded += 1;
                    excluded_extent += extent;
                } else {
                    measured += 1;
                    total_extent += extent;
                }
            }
            LoadSubject {
                key: lane.key.clone(),
                label: lane.label.clone(),
                unnamed: lane.unnamed,
                kind: lane.kind,
                entries: lane.spans.len(),
                measured_spans: measured,
                total_extent,
                share: share_of(total_extent),
                excluded_spans: excluded,
                excluded_extent,
                gap_crossing_spans: crossing,
                open_ended_spans: open_end,
                open_started_spans: open_start,
                below_resolution_spans: below_res,
            }
        })
        .collect();
    subjects.sort_by(|a, b| b.total_extent.cmp(&a.total_extent).then_with(|| a.key.cmp(&b.key)));

    let thread_extent: u64 = subjects.iter().filter(|s| s.kind == "thread").map(|s| s.total_extent).sum();
    let idle_record_extent: u64 = subjects.iter().filter(|s| s.kind == "idle").map(|s| s.total_extent).sum();
    let isr_extent: u64 = subjects.iter().filter(|s| s.kind == "isr").map(|s| s.total_extent).sum();
    let below_resolution_spans: usize = subjects.iter().map(|s| s.below_resolution_spans).sum();
    let gap_extent = merged_gap_extent(gaps, t_from, t_to);

    LoadSummary {
        unit,
        window_extent,
        gap_extent,
        gap_fraction: share_of(gap_extent),
        records_lost,
        has_time_base: unit != "frame",
        thread_extent,
        idle_record_extent,
        isr_extent,
        unaccounted_extent: window_extent.saturating_sub(thread_extent),
        below_resolution_spans,
        subjects,
    }
}

/// Computes [`LoadAnswer`] from a rendered `*.trace.csv`.
pub fn load_answer(csv: &str) -> Result<LoadAnswer, String> {
    load_answer_with_cap(csv, MAX_ROWS)
}

/// [`load_answer`] with the row cap as a parameter, for tests that want to
/// exercise the cap itself without a quarter-million-row fixture.
fn load_answer_with_cap(csv: &str, cap: usize) -> Result<LoadAnswer, String> {
    let d = decode_with_cap(csv, cap)?;
    let summary = summarize(&d.lanes, &d.gaps, d.unit, d.t_from, d.t_to, d.records_lost);
    Ok(LoadAnswer {
        rows: d.rows,
        rows_dropped_by_cap: d.rows_dropped_by_cap,
        row_cap: cap,
        rows_unparsed: d.rows_unparsed,
        summary,
    })
}

/// Computes [`SpansAnswer`] from the same rendered `*.trace.csv` — the
/// decoded timeline [`load_answer`] discards after [`summarize`] reduces it,
/// served here instead.
pub fn spans_answer(csv: &str) -> Result<SpansAnswer, String> {
    spans_answer_with_cap(csv, MAX_ROWS)
}

/// [`spans_answer`] with the row cap as a parameter, mirroring
/// [`load_answer_with_cap`].
fn spans_answer_with_cap(csv: &str, cap: usize) -> Result<SpansAnswer, String> {
    let d = decode_with_cap(csv, cap)?;
    Ok(SpansAnswer {
        unit: d.unit,
        t_from: d.t_from,
        t_to: d.t_to,
        records_lost: d.records_lost,
        rows: d.rows,
        rows_dropped_by_cap: d.rows_dropped_by_cap,
        row_cap: cap,
        rows_unparsed: d.rows_unparsed,
        gaps: d.gaps,
        lanes: d.lanes,
    })
}

/// Everything [`load_answer`] and [`spans_answer`] both need: the CSV read,
/// the stale-prefix and axis-unit decisions, gap extraction and lane
/// construction — the one decode both response shapes are built from, so a
/// wire-schema change to `RecordKind` or the five-lies rules only has to be
/// made once here.
struct Decoded {
    rows: usize,
    rows_dropped_by_cap: usize,
    rows_unparsed: usize,
    unit: &'static str,
    t_from: u64,
    t_to: u64,
    records_lost: u64,
    gaps: Vec<Gap>,
    lanes: Vec<Lane>,
}

fn decode_with_cap(csv: &str, cap: usize) -> Result<Decoded, String> {
    let mut lines = csv.split('\n');
    let header = lines.next().unwrap_or_default().trim_end_matches('\r');
    if header != outpost::csv_header() {
        return Err(format!(
            "this capture's columns are {header:?}, and this build reads {:?} — refusing to guess \
             which column moved, the same check `embarch-ui` decision 10 (trace) makes against this \
             same header",
            outpost::csv_header()
        ));
    }

    let mut rows: Vec<Row> = Vec::new();
    let mut rows_dropped_by_cap = 0usize;
    let mut rows_unparsed = 0usize;
    for line in lines {
        let line = line.trim_end_matches('\r');
        if line.is_empty() {
            continue;
        }
        if rows.len() >= cap {
            rows_dropped_by_cap += 1;
            continue;
        }
        let f = split_row(line);
        if f.len() < 9 {
            rows_unparsed += 1;
            continue;
        }
        // Positional access is safe only because the header check above
        // already refused anything whose columns are not exactly
        // `outpost::csv_header()`: 0 frame_index, 1 frame_seq, 2 rx_utc_ms,
        // 3 cycles, 4 us, 5 kind, 6 a, 7 b, 8 name.
        let Ok(frame_index) = f[0].parse::<u64>() else {
            rows_unparsed += 1;
            continue;
        };
        rows.push(Row {
            frame_index,
            rx_utc_ms: if f[2].is_empty() { None } else { f[2].parse::<u64>().ok() },
            dut_us: if f[4].is_empty() {
                None
            } else {
                f[4].parse::<f64>().ok().filter(|v| v.is_finite() && *v >= 0.0).map(|v| v as u64)
            },
            dut_cycles: f[3].parse::<u64>().ok(),
            kind: kind_of(&f[5]),
            a: f[6].parse::<u32>().unwrap_or(0),
            b: f[7].parse::<u32>().unwrap_or(0),
            name: f[8].clone(),
            t: 0,
        });
    }

    // A stale leading prefix, dropped before any clock is read — see
    // `stale_prefix_end`.
    if let Some((end, _step)) = stale_prefix_end(&rows) {
        rows.drain(..end);
    }

    let unstamped_rows = rows.iter().filter(|r| r.rx_utc_ms.is_none()).count();
    let undated_rows = rows.iter().filter(|r| r.dut_us.is_none()).count();
    let dut_clock_broken = dut_clock_health(&rows).broken;

    let unit: &'static str = if rows.is_empty() {
        "frame"
    } else if undated_rows == 0 && !dut_clock_broken {
        "us"
    } else if unstamped_rows == 0 {
        "ms"
    } else {
        "frame"
    };

    for row in &mut rows {
        row.t = match unit {
            "us" => row.dut_us.unwrap_or(row.frame_index),
            "ms" => row.rx_utc_ms.unwrap_or(row.frame_index),
            _ => row.frame_index,
        };
    }

    // Frames, in file order — what a gap band reaches back through: bounded
    // by the frame *before* the one reporting it.
    let mut frame_order: Vec<(u64, u64)> = Vec::new();
    let mut frame_pos: HashMap<u64, usize> = HashMap::new();
    for r in &rows {
        if let std::collections::hash_map::Entry::Vacant(slot) = frame_pos.entry(r.frame_index) {
            slot.insert(frame_order.len());
            frame_order.push((r.frame_index, r.t));
        }
    }

    // Gaps first: a gap row's extent comes from somewhere other than its own
    // record, and taking them out is what makes the rest a stream this can
    // pair switch-ins against.
    let mut gaps: Vec<Gap> = Vec::new();
    let mut records_lost = 0u64;
    for (i, r) in rows.iter().enumerate() {
        if r.kind != Some(RecordKind::Gap) {
            continue;
        }
        records_lost += u64::from(r.a);
        let (from, to, unbounded_start) = if unit == "us" {
            let span_us = match (r.dut_cycles, r.dut_us) {
                (Some(c), Some(u)) if c > 0 => ((f64::from(r.b) * (u as f64) / (c as f64)).round()) as u64,
                _ => 0,
            };
            (r.t, r.t.saturating_add(span_us), span_us == 0 && r.b > 0)
        } else {
            let pos = frame_pos.get(&r.frame_index).copied().unwrap_or(0);
            let from = if pos == 0 { r.t } else { frame_order[pos - 1].1 };
            (from, r.t, pos == 0)
        };
        gaps.push(Gap {
            from,
            to,
            records_lost: r.a,
            cycle_span: r.b,
            frame_index: r.frame_index,
            row_index: i,
            unbounded_start,
        });
    }
    gaps.sort_by_key(|g| g.from);

    let timeline: Vec<&Row> = rows.iter().filter(|r| r.kind != Some(RecordKind::Gap)).collect();

    let t_from = timeline
        .iter()
        .map(|r| r.t)
        .min()
        .unwrap_or(0)
        .min(gaps.iter().map(|g| g.from).min().unwrap_or(u64::MAX));
    let t_to = timeline
        .iter()
        .map(|r| r.t)
        .max()
        .unwrap_or(t_from)
        .max(gaps.iter().map(|g| g.to).max().unwrap_or(0));

    // ---- lanes --------------------------------------------------------
    struct Building {
        lane: Lane,
        /// `(t, frame_index, open_start)` for each currently-open span.
        open: Vec<(u64, u64, bool)>,
    }
    let mut order: Vec<String> = Vec::new();
    let mut building: HashMap<String, Building> = HashMap::new();

    let ensure = |building: &mut HashMap<String, Building>,
                  order: &mut Vec<String>,
                  key: String,
                  label: String,
                  unnamed: bool,
                  kind: &'static str| {
        if !building.contains_key(&key) {
            order.push(key.clone());
            building.insert(
                key.clone(),
                Building { lane: Lane { key, label, unnamed, kind, spans: Vec::new() }, open: Vec::new() },
            );
        } else if let Some(b) = building.get_mut(&key) {
            // A later record may carry a name the first one did not.
            // Upgrading is safe; downgrading a name back to a pointer is not.
            if b.lane.unnamed && !unnamed {
                b.lane.label = label;
                b.lane.unnamed = false;
            }
        }
    };

    let thread_key = |a: u32| format!("0x{a:08x}");

    for r in timeline.iter() {
        let Some(kind) = r.kind else {
            // A kind this build does not know decodes as itself, and has no
            // lane — nothing here needs its point event.
            continue;
        };

        match kind {
            RecordKind::ThreadSwitchIn => {
                let key = thread_key(r.a);
                let named_here = !r.name.is_empty();
                let label = if named_here { r.name.clone() } else { key.clone() };
                ensure(&mut building, &mut order, key.clone(), label, !named_here, "thread");
                if let Some(b) = building.get_mut(&key) {
                    // A switch-in with one already open means this thread's
                    // switch-out was among the losses: close the old run
                    // where it stopped being observable rather than nesting
                    // a thread inside itself.
                    if let Some(open) = b.open.pop() {
                        b.lane.spans.push(close(open, r.t, r.frame_index, true, unit));
                    }
                    b.open.push((r.t, r.frame_index, false));
                }
                // A switch-in is also what ends idle: there is no idle-exit
                // hook to define.
                if let Some(idle) = building.get_mut(IDLE_LANE) {
                    if let Some(open) = idle.open.pop() {
                        idle.lane.spans.push(close(open, r.t, r.frame_index, false, unit));
                    }
                }
            }
            RecordKind::ThreadSwitchOut => {
                let key = thread_key(r.a);
                let named_here = !r.name.is_empty();
                let label = if named_here { r.name.clone() } else { key.clone() };
                ensure(&mut building, &mut order, key.clone(), label, !named_here, "thread");
                if let Some(b) = building.get_mut(&key) {
                    match b.open.pop() {
                        Some(open) => b.lane.spans.push(close(open, r.t, r.frame_index, false, unit)),
                        // Its switch-in was among the losses: the run is
                        // real and its start is not known.
                        None => b.lane.spans.push(Span {
                            from: r.t,
                            to: r.t,
                            open_start: true,
                            open_end: false,
                            crosses_gap: false,
                            below_resolution: false,
                        }),
                    }
                }
            }
            RecordKind::IsrEnter | RecordKind::IsrExit => {
                let unidentified = r.a == outpost::IRQ_UNKNOWN;
                let key = if unidentified { "isr-unidentified".to_string() } else { format!("irq-{}", r.a) };
                let named_here = !r.name.is_empty();
                let label = if unidentified {
                    "ISR (vector not reported)".to_string()
                } else if named_here {
                    r.name.clone()
                } else {
                    format!("IRQ {}", r.a)
                };
                ensure(&mut building, &mut order, key.clone(), label, unidentified || !named_here, "isr");
                if let Some(b) = building.get_mut(&key) {
                    if kind == RecordKind::IsrEnter {
                        b.open.push((r.t, r.frame_index, false));
                    } else {
                        match b.open.pop() {
                            Some(open) => b.lane.spans.push(close(open, r.t, r.frame_index, false, unit)),
                            None => b.lane.spans.push(Span {
                                from: r.t,
                                to: r.t,
                                open_start: true,
                                open_end: false,
                                crosses_gap: false,
                                below_resolution: false,
                            }),
                        }
                    }
                }
            }
            RecordKind::Idle => {
                ensure(&mut building, &mut order, IDLE_LANE.to_string(), "cpu idle".to_string(), false, "idle");
                if let Some(b) = building.get_mut(IDLE_LANE) {
                    if let Some(open) = b.open.pop() {
                        b.lane.spans.push(close(open, r.t, r.frame_index, true, unit));
                    }
                    b.open.push((r.t, r.frame_index, false));
                }
            }
            // Point-only records: registering the lane (so a subject with no
            // spans still appears in the repartition, exactly as it would
            // through `embarch-ui`) is all the answer needs from these.
            RecordKind::ThreadCreate | RecordKind::ThreadName => {
                let key = thread_key(r.a);
                let named_here = !r.name.is_empty();
                let label = if named_here { r.name.clone() } else { key.clone() };
                ensure(&mut building, &mut order, key, label, !named_here, "thread");
            }
            RecordKind::GpioDispatch | RecordKind::GpioCallbackDone => {
                let key = format!("gpio:0x{:08x}", r.a);
                let named_here = !r.name.is_empty();
                let label = if named_here { r.name.clone() } else { format!("0x{:08x}", r.a) };
                ensure(&mut building, &mut order, key, label, !named_here, "gpio");
            }
            RecordKind::Marker => {}
            RecordKind::Gap => unreachable!("gap rows are filtered out of `timeline`"),
        }
    }

    // Whatever is still open at the end never got a closing record: drawn
    // out to the end of the capture, flagged, so the extent is a shape and
    // not a duration.
    let mut lanes: Vec<Lane> = Vec::new();
    for key in order {
        if let Some(mut b) = building.remove(&key) {
            let open: Vec<(u64, u64, bool)> = b.open.drain(..).collect();
            for (from_t, _from_frame, open_start) in open {
                b.lane.spans.push(Span {
                    from: from_t,
                    to: t_to,
                    open_start,
                    open_end: true,
                    crosses_gap: false,
                    below_resolution: false,
                });
            }
            for span in &mut b.lane.spans {
                span.crosses_gap = gaps.iter().any(|g| span.from < g.to && g.from < span.to);
            }
            lanes.push(b.lane);
        }
    }

    Ok(Decoded { rows: rows.len(), rows_dropped_by_cap, rows_unparsed, unit, t_from, t_to, records_lost, gaps, lanes })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header() -> &'static str {
        outpost::csv_header()
    }

    /// A `Gap` with only `from`/`to` set, for tests that exercise band
    /// merging and do not care about the firmware-reported fields.
    fn test_gap(from: u64, to: u64) -> Gap {
        Gap { from, to, records_lost: 0, cycle_span: 0, frame_index: 0, row_index: 0, unbounded_start: false }
    }

    #[test]
    fn a_mismatched_column_list_is_refused_not_guessed() {
        let csv = "frame_index,kind,a,b,name\n0,thread_switch_in,1,0,\n";
        let err = load_answer(csv).unwrap_err();
        assert!(err.contains("refusing to guess"), "{err}");
    }

    #[test]
    fn an_empty_capture_after_the_header_answers_with_a_frame_axis_and_no_subjects() {
        let csv = format!("{}\n", header());
        let answer = load_answer(&csv).unwrap();
        assert_eq!(answer.rows, 0);
        assert_eq!(answer.summary.unit, "frame");
        assert!(answer.summary.subjects.is_empty());
        assert_eq!(answer.summary.window_extent, 0);
    }

    /// A single thread runs the whole window on the DUT clock: `share` must
    /// come out at 1.0, and there is nothing to exclude.
    #[test]
    fn a_single_uncontested_thread_gets_the_whole_window_as_its_share() {
        let csv = format!(
            "{}\n0,0,,0,0,thread_switch_in,1,0,worker\n1,1,,100,100,thread_switch_out,1,0,worker\n",
            header()
        );
        let answer = load_answer(&csv).unwrap();
        assert_eq!(answer.summary.unit, "us");
        assert_eq!(answer.summary.subjects.len(), 1);
        let s = &answer.summary.subjects[0];
        assert_eq!(s.label, "worker");
        assert!(!s.unnamed);
        assert_eq!(s.entries, 1);
        assert_eq!(s.total_extent, 100);
        assert!((s.share - 1.0).abs() < 1e-9);
        assert_eq!(answer.summary.gap_fraction, 0.0);
    }

    /// A gap record's reported loss must show up in `records_lost` and widen
    /// `gap_fraction` above zero — the coverage line the decision exists to
    /// carry across.
    #[test]
    fn a_reported_gap_moves_records_lost_and_the_coverage_line() {
        let csv = format!(
            "{}\n\
             0,0,,0,0,thread_switch_in,1,0,worker\n\
             1,1,,100,100,gap,5,50,\n\
             2,2,,200,200,thread_switch_out,1,0,worker\n",
            header()
        );
        let answer = load_answer(&csv).unwrap();
        assert_eq!(answer.summary.records_lost, 5);
        assert!(answer.summary.gap_fraction > 0.0);
        // The span crossing the gap is excluded from the measured total, not
        // silently folded in.
        let s = &answer.summary.subjects[0];
        assert_eq!(s.gap_crossing_spans, 1);
        assert_eq!(s.measured_spans, 0);
    }

    /// An unnamed thread's raw pointer key must never be silently upgraded
    /// to a name from nothing, and must never be downgraded once named.
    #[test]
    fn an_unnamed_thread_stays_unnamed_until_a_later_record_names_it() {
        let csv = format!(
            "{}\n\
             0,0,,0,0,thread_switch_in,66,0,\n\
             1,1,,10,10,thread_switch_out,66,0,\n\
             2,2,,20,20,thread_name,66,0,worker\n",
            header()
        );
        let answer = load_answer(&csv).unwrap();
        let s = &answer.summary.subjects[0];
        assert!(!s.unnamed, "a later thread_name record should have named this lane");
        assert_eq!(s.label, "worker");
    }

    #[test]
    fn the_row_cap_is_counted_not_silently_absorbed() {
        let mut csv = format!("{}\n", header());
        for i in 0..5u64 {
            csv.push_str(&format!("{i},{i},,{},{},thread_switch_in,1,0,w\n", i * 10, i * 10));
        }
        let answer = load_answer_with_cap(&csv, 2).unwrap();
        assert_eq!(answer.rows, 2);
        assert_eq!(answer.rows_dropped_by_cap, 3);
        assert_eq!(answer.row_cap, 2);
    }

    #[test]
    fn a_short_line_is_refused_and_counted_rather_than_silently_skipped() {
        let csv = format!("{}\n0,0,,0,0,thread_switch_in\n", header());
        let answer = load_answer(&csv).unwrap();
        assert_eq!(answer.rows, 0);
        assert_eq!(answer.rows_unparsed, 1);
    }

    // ---- three behaviours recovered from `embarch-ui`'s deleted
    // ---- `load_summary_tests` module (`git show 87d01b4^:src/trace.rs` in
    // ---- `embarch-ui`), whose only test they were before `ui/051` deleted it
    // ---- once this arithmetic was ported here. Expectations below are taken
    // ---- from that history, not re-derived from this file's own
    // ---- implementation.

    /// Ported from `load_summary_tests::overlapping_gap_bands_are_counted_as_a_union`.
    /// Overlapping bands are counted once. Summing raw widths instead is the
    /// bug that lets `gap_fraction` exceed 1.
    #[test]
    fn overlapping_gap_bands_are_counted_as_a_union() {
        let gaps = vec![test_gap(100, 200), test_gap(150, 250), test_gap(400, 450)];
        // Union is 100..250 (150) plus 400..450 (50), not 100+100+50.
        assert_eq!(merged_gap_extent(&gaps, 0, 1_000), 200);
        // And it clamps to the window rather than counting outside it.
        assert_eq!(merged_gap_extent(&gaps, 0, 120), 20);
    }

    /// Ported from `load_summary_tests::idle_is_not_counted_twice`. The
    /// double count this design exists to avoid: idle is reported both by
    /// `RecordKind::Idle` records (the `cpu-idle` lane) and by switches of
    /// whatever thread the manifest itself names `idle`. The two must stay
    /// apart, and the `cpu-idle` lane must never also appear as a `thread`
    /// subject.
    #[test]
    fn idle_is_not_counted_twice() {
        let csv = format!(
            "{}\n\
             0,0,,0,0,thread_switch_in,1,0,idle\n\
             1,1,,50,50,thread_switch_out,1,0,idle\n\
             2,2,,50,50,idle,0,0,\n\
             3,3,,150,150,thread_switch_in,2,0,worker\n\
             4,4,,200,200,thread_switch_out,2,0,worker\n",
            header()
        );
        let answer = load_answer(&csv).unwrap();
        let s = &answer.summary;
        let idle_thread = s
            .subjects
            .iter()
            .find(|x| x.kind == "thread" && x.label == "idle")
            .expect("this capture's manifest names an idle thread");
        assert!(idle_thread.total_extent > 0, "the idle thread ran and was measured");
        assert!(
            s.subjects.iter().any(|x| x.kind == "idle"),
            "the idle *record* lane exists as its own subject"
        );
        assert!(
            !s.subjects.iter().filter(|x| x.kind == "thread").any(|x| x.key == "cpu-idle"),
            "the idle record lane leaked into the thread total"
        );
        // Exactly the two threads' 50 + 50, not the `cpu-idle` lane's 100
        // folded in on top: a naive "threads plus idle" total would double
        // this to 200.
        assert_eq!(s.thread_extent, 100);
        assert_eq!(s.idle_record_extent, 100);
        assert!(s.thread_extent <= s.window_extent);
    }

    /// Ported from `load_summary_tests::subjects_are_sorted_by_measured_time`.
    /// Sorted heaviest-first, so the load repartition reads as one.
    #[test]
    fn subjects_are_sorted_by_measured_time() {
        let csv = format!(
            "{}\n\
             0,0,,0,0,thread_switch_in,1,0,a\n\
             1,1,,30,30,thread_switch_out,1,0,a\n\
             2,2,,30,30,thread_switch_in,2,0,b\n\
             3,3,,130,130,thread_switch_out,2,0,b\n\
             4,4,,130,130,thread_switch_in,3,0,c\n\
             5,5,,175,175,thread_switch_out,3,0,c\n",
            header()
        );
        let answer = load_answer(&csv).unwrap();
        let totals: Vec<u64> = answer.summary.subjects.iter().map(|s| s.total_extent).collect();
        let mut sorted = totals.clone();
        sorted.sort_unstable_by(|a, b| b.cmp(a));
        assert_eq!(totals, sorted);
        // And it is a real order, not three equal totals passing vacuously.
        assert_eq!(totals, vec![100, 45, 30]);
    }

    // ---- against the same real firmware capture `outpost_manifest.rs` tests
    // ---- itself against — not a fixture built for this file alone.

    /// The same `embarch-outpost/tests/native_sim_stream` bytes
    /// `outpost_manifest.rs`'s own `a_real_firmware_capture_decodes_and_names_itself`
    /// pins its decoder against, rendered here and fed to this module rather
    /// than to `embarch-ui`'s. This is what "consumes the rendered CSV, does
    /// not write a second decoder" means in practice: the decode is
    /// `outpost_manifest::render`'s, unmodified, and only its output crosses
    /// into `load_answer`.
    #[test]
    fn a_real_firmware_captures_load_answer_reports_a_named_us_axis_with_a_real_gap() {
        let manifest = crate::outpost_manifest::parse(include_str!(
            "../tests/fixtures/outpost-native-sim-manifest.json"
        ))
        .expect("the real manifest parses");
        let raw_bytes = include_bytes!("../tests/fixtures/outpost-native-sim.bin");

        let dir = std::env::temp_dir().join(format!(
            "embarch-core-outpost-load-test-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let raw_path = dir.join("outpost.bin");
        let out_path = dir.join("outpost.trace.csv");
        std::fs::write(&raw_path, raw_bytes).unwrap();
        let outcome = crate::outpost_manifest::render(&raw_path, &out_path, None, Some(&manifest))
            .expect("a real capture with its own manifest renders");
        let csv = std::fs::read_to_string(&out_path).unwrap();
        let _ = std::fs::remove_dir_all(&dir);

        assert_eq!(outcome.refusal, None, "this capture's own manifest must apply");
        assert!(outcome.dropped_at_source > 0, "this fixture deliberately overflows its ring");

        let answer = load_answer(&csv).expect("a real rendered trace parses");
        assert_eq!(answer.rows_unparsed, 0);
        assert_eq!(answer.rows_dropped_by_cap, 0);
        // No arrival log was supplied, so the host clock never reaches these
        // rows and the DUT's own `us` column is what draws the axis.
        assert_eq!(answer.summary.unit, "us");
        assert!(!answer.summary.subjects.is_empty(), "a real capture traces at least one subject");
        // At least one named subject: the real manifest names threads and
        // ISRs, so an all-`unnamed` result would mean the manifest silently
        // failed to apply despite `outcome.refusal` reading `None`.
        assert!(answer.summary.subjects.iter().any(|s| !s.unnamed));
        assert_eq!(
            answer.summary.records_lost, outcome.dropped_at_source,
            "the coverage line's own count must match the render's"
        );
        assert!(answer.summary.gap_extent > 0, "a capture with dropped records has a real gap band");
        assert!(answer.summary.gap_fraction > 0.0 && answer.summary.gap_fraction <= 1.0);
    }

    /// A single closed span, straight through [`spans_answer`] rather than
    /// [`load_answer`]'s reduction of it — `core/076`'s own route.
    #[test]
    fn spans_answer_serves_the_closed_span_a_single_thread_left_behind() {
        let csv = format!(
            "{}\n0,0,,0,0,thread_switch_in,1,0,worker\n1,1,,100,100,thread_switch_out,1,0,worker\n",
            header()
        );
        let answer = spans_answer(&csv).unwrap();
        assert_eq!(answer.unit, "us");
        assert_eq!(answer.t_from, 0);
        assert_eq!(answer.t_to, 100);
        assert!(answer.gaps.is_empty());
        assert_eq!(answer.lanes.len(), 1);
        let lane = &answer.lanes[0];
        assert_eq!(lane.label, "worker");
        assert!(!lane.unnamed);
        assert_eq!(lane.kind, "thread");
        assert_eq!(lane.spans, vec![Span {
            from: 0,
            to: 100,
            open_start: false,
            open_end: false,
            crosses_gap: false,
            below_resolution: false,
        }]);
    }

    /// `Gap`'s firmware-reported fields (`records_lost`, `cycle_span`,
    /// `frame_index`, `row_index`) must all come through `spans_answer`, not
    /// just `from`/`to` — decision 66's widening, checked field-for-field the
    /// way `tasks/ui/065` checked this route against `trace.rs`'s own `Gap`.
    #[test]
    fn spans_answer_carries_the_gap_records_lost_cycle_span_and_position() {
        let csv = format!(
            "{}\n\
             0,0,,0,0,thread_switch_in,1,0,worker\n\
             1,1,,100,100,gap,5,50,\n\
             2,2,,200,200,thread_switch_out,1,0,worker\n",
            header()
        );
        let answer = spans_answer(&csv).unwrap();
        assert_eq!(answer.gaps.len(), 1);
        let gap = &answer.gaps[0];
        assert_eq!(gap.records_lost, 5);
        assert_eq!(gap.cycle_span, 50);
        assert_eq!(gap.frame_index, 1);
        assert_eq!(gap.row_index, 1, "the gap's own row, not its frame");
        assert!(!gap.unbounded_start, "both ends measured off the row's own cycles/us ratio");
    }

    /// [`spans_answer`] and [`load_answer`] must agree on the same real
    /// capture, because they are two views of one [`decode_with_cap`] call,
    /// not two computations: every [`LoadSubject`] `summarize` produced must
    /// be reconstructable from the [`Lane`] `spans_answer` served for the
    /// same key — the exclusion rules ([`Span::crosses_gap`],
    /// `open_start`/`open_end`, `below_resolution`) applied to the raw spans
    /// must reduce to exactly the extent and counts `summarize` reported.
    #[test]
    fn spans_answer_and_load_answer_agree_on_the_same_real_capture() {
        let manifest = crate::outpost_manifest::parse(include_str!(
            "../tests/fixtures/outpost-native-sim-manifest.json"
        ))
        .expect("the real manifest parses");
        let raw_bytes = include_bytes!("../tests/fixtures/outpost-native-sim.bin");

        let dir = std::env::temp_dir().join(format!(
            "embarch-core-outpost-spans-test-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let raw_path = dir.join("outpost.bin");
        let out_path = dir.join("outpost.trace.csv");
        std::fs::write(&raw_path, raw_bytes).unwrap();
        crate::outpost_manifest::render(&raw_path, &out_path, None, Some(&manifest))
            .expect("a real capture with its own manifest renders");
        let csv = std::fs::read_to_string(&out_path).unwrap();
        let _ = std::fs::remove_dir_all(&dir);

        let load = load_answer(&csv).expect("a real rendered trace parses via load_answer");
        let spans = spans_answer(&csv).expect("the same rendered trace parses via spans_answer");

        assert_eq!(spans.unit, load.summary.unit);
        assert_eq!(spans.t_to.saturating_sub(spans.t_from), load.summary.window_extent);
        assert_eq!(spans.records_lost, load.summary.records_lost);
        assert_eq!(spans.rows, load.rows);
        assert_eq!(spans.rows_dropped_by_cap, load.rows_dropped_by_cap);
        assert_eq!(spans.rows_unparsed, load.rows_unparsed);
        assert_eq!(spans.lanes.len(), load.summary.subjects.len(), "same lane count on both sides");

        for subject in &load.summary.subjects {
            let lane = spans
                .lanes
                .iter()
                .find(|l| l.key == subject.key)
                .unwrap_or_else(|| panic!("spans_answer must carry a lane for key {}", subject.key));
            assert_eq!(lane.label, subject.label);
            assert_eq!(lane.unnamed, subject.unnamed);
            assert_eq!(lane.kind, subject.kind);
            assert_eq!(lane.spans.len(), subject.entries);

            let excluded = |s: &Span| s.crosses_gap || s.open_end || s.open_start || s.below_resolution;
            let measured_extent: u64 =
                lane.spans.iter().filter(|s| !excluded(s)).map(|s| s.to.saturating_sub(s.from)).sum();
            assert_eq!(
                measured_extent, subject.total_extent,
                "reducing lane '{}'s own spans by the same exclusion rule must reproduce \
                 summarize's total_extent",
                subject.key
            );
            let measured_count = lane.spans.iter().filter(|s| !excluded(s)).count();
            assert_eq!(measured_count, subject.measured_spans);
            let crossing_count = lane.spans.iter().filter(|s| s.crosses_gap).count();
            assert_eq!(crossing_count, subject.gap_crossing_spans);
        }

        // The gap bands themselves cross too, not only the per-lane counts —
        // `summarize`'s `gap_extent` is a union of these same raw bands.
        assert!(!spans.gaps.is_empty(), "the real capture's dropped records must show up as raw gap bands too");
    }
}
