//! The per-pass line-count view of a finished host study (`core/116`, `embarch-study-designer`
//! decision 86).
//!
//! **A count is shown, never compared.** Nothing here has an expected value or a verdict. A
//! [`LineCount`] is counted over the **raw bytes of the tap's file on disk**, split at newlines
//! after the run, so a line that arrived in two reads is one line, and a corrected pattern is
//! counted again over a finished run without touching the bench.
//!
//! **Where a line sits in time.** A tap's `<name>.arrival.csv` has one row per record, and a line
//! split across reads is two rows, the first holding its first byte. A line's time is therefore
//! the stamp of the row that contains its first byte. Pass markers are on the same clock
//! ([`crate::host_study::StudyClock`]), so the two compare directly.
//!
//! **Buckets.** For each `Repeat` that ran, a line stamped inside its span is attributed to a
//! pass by [`embarch_study_designer::pass_at`]; a line outside every `Repeat`'s span (before
//! pass 1, after the last pass, or between two repeats) is `outside_passes`.

use embarch_study_designer::{pass_at, HostList, IterationMarker, LineCount};
use serde::{Deserialize, Serialize};

/// The time span a `Repeat` ran for, on the tap clock. Pass starts are the markers; this says
/// where the last pass ends.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepeatSpan {
    pub list: HostList,
    pub repeat_index: u32,
    pub name: String,
    pub started_utc_ms: u64,
    pub ended_utc_ms: u64,
}

/// One pass's count.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PassCount {
    pub pass: u32,
    /// The `values` entry for a `values` repeat; `None` for `count`.
    pub value: Option<String>,
    pub count: u32,
}

/// One `Repeat`'s passes for one count.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RepeatCounts {
    pub list: HostList,
    pub repeat_index: u32,
    pub name: String,
    pub passes: Vec<PassCount>,
}

/// One [`LineCount`] rendered: over the whole tap, per pass of each `Repeat`, and outside any.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CountView {
    pub name: String,
    pub tap: String,
    pub pattern: String,
    /// Lines of the tap that match, over the whole run.
    pub total: u32,
    /// Lines in the tap, matching or not.
    pub tap_lines: u32,
    pub repeats: Vec<RepeatCounts>,
    /// Matching lines stamped outside every `Repeat`'s span.
    pub outside_passes: u32,
}

/// One tap's bytes and arrival rows, as read from disk.
pub struct TapInput<'a> {
    pub raw: &'a [u8],
    /// `(byte_offset, core_rx_utc_ms)` per row of the arrival sidecar, offsets into `raw`.
    pub arrivals: &'a [(u64, u64)],
}

/// Splits `raw` at newlines: `(start offset, text)` per line, `\r` before `\n` dropped, a final
/// unterminated line kept. Bytes that are not UTF-8 are replaced, never dropped.
pub fn split_lines(raw: &[u8]) -> Vec<(u64, String)> {
    let mut out = Vec::new();
    let mut start = 0usize;
    while start < raw.len() {
        let end = raw[start..].iter().position(|b| *b == b'\n').map(|p| start + p);
        let body = &raw[start..end.unwrap_or(raw.len())];
        let body = body.strip_suffix(b"\r").unwrap_or(body);
        out.push((start as u64, String::from_utf8_lossy(body).into_owned()));
        match end {
            Some(e) => start = e + 1,
            None => break,
        }
    }
    out
}

/// The stamp of the arrival row holding `offset`: the last row starting at or before it.
fn stamp_of(arrivals: &[(u64, u64)], offset: u64) -> Option<u64> {
    let i = arrivals.partition_point(|(o, _)| *o <= offset);
    i.checked_sub(1).map(|i| arrivals[i].1)
}

/// Renders every count in `counts`. `tap_of` yields the named tap's bytes, `None` for a tap that
/// captured nothing (every count is then zero). `Err` names a count whose pattern does not
/// compile, or a tap with bytes and no arrival rows to place them.
pub fn render<'a>(
    counts: &[LineCount],
    tap_of: &dyn Fn(&str) -> Option<TapInput<'a>>,
    markers: &[IterationMarker],
    spans: &[RepeatSpan],
) -> Result<Vec<CountView>, String> {
    let mut views = Vec::with_capacity(counts.len());
    for lc in counts {
        let input = tap_of(&lc.tap);
        let lines = input.as_ref().map(|t| split_lines(t.raw)).unwrap_or_default();
        if !lines.is_empty() && input.as_ref().is_some_and(|t| t.arrivals.is_empty()) {
            return Err(format!("tap '{}' has bytes but no arrival rows, so its lines cannot be placed in a pass", lc.tap));
        }
        let matcher_err = |e: String| format!("count '{}': pattern does not compile: {e}", lc.name);
        // Each line's match, once: `count` compiles the pattern once per call.
        let hit: Vec<bool> = lines
            .iter()
            .map(|(_, l)| lc.count(std::iter::once(l.as_str())).map(|n| n > 0))
            .collect::<Result<_, _>>()
            .map_err(matcher_err)?;
        let arrivals = input.as_ref().map(|t| t.arrivals).unwrap_or(&[]);
        let stamps: Vec<Option<u64>> = lines.iter().map(|(o, _)| stamp_of(arrivals, *o)).collect();

        let mut repeats: Vec<RepeatCounts> = spans
            .iter()
            .map(|s| {
                let mut passes: Vec<PassCount> = markers
                    .iter()
                    .filter(|m| m.list == s.list && m.repeat_index == s.repeat_index)
                    .map(|m| PassCount { pass: m.pass, value: m.value.clone(), count: 0 })
                    .collect();
                passes.sort_by_key(|p| p.pass);
                RepeatCounts { list: s.list, repeat_index: s.repeat_index, name: s.name.clone(), passes }
            })
            .collect();
        let (mut total, mut outside) = (0u32, 0u32);
        for (i, matched) in hit.iter().enumerate() {
            if !matched {
                continue;
            }
            total += 1;
            let t = stamps[i];
            let home = t.and_then(|t| {
                spans.iter().position(|s| s.started_utc_ms <= t && t <= s.ended_utc_ms).and_then(|k| {
                    let s = &spans[k];
                    pass_at(markers, s.list, s.repeat_index, t).map(|p| (k, p))
                })
            });
            match home {
                Some((k, pass)) => {
                    if let Some(pc) = repeats[k].passes.iter_mut().find(|p| p.pass == pass) {
                        pc.count += 1;
                    }
                }
                None => outside += 1,
            }
        }
        views.push(CountView {
            name: lc.name.clone(),
            tap: lc.tap.clone(),
            pattern: lc.pattern.clone(),
            total,
            tap_lines: lines.len() as u32,
            repeats,
            outside_passes: outside,
        });
    }
    Ok(views)
}

#[cfg(test)]
mod tests {
    use super::*;
    use embarch_study_designer::PatternKind;

    fn lc(name: &str, tap: &str, pat: &str) -> LineCount {
        LineCount { name: name.into(), tap: tap.into(), pattern: pat.into(), kind: PatternKind::Substring }
    }

    #[test]
    fn lines_split_at_newlines_not_at_records() {
        let l = split_lines(b"a1\r\nb2\n\npart");
        assert_eq!(l, vec![(0, "a1".into()), (4, "b2".into()), (7, "".into()), (8, "part".into())]);
        assert!(split_lines(b"").is_empty());
    }

    #[test]
    fn a_line_spanning_two_records_is_one_line_stamped_by_its_first_byte() {
        // "pd: hard reset" arrived as two rows (0..6 at t=100, 6..15 at t=900).
        let raw = b"pd: ha" as &[u8];
        let raw2 = [raw, b"rd reset\n"].concat();
        let arrivals = [(0u64, 100u64), (6, 900)];
        let markers = [
            IterationMarker { list: HostList::Steps, repeat_index: 0, pass: 0, value: None, started_utc_ms: 50 },
            IterationMarker { list: HostList::Steps, repeat_index: 0, pass: 1, value: None, started_utc_ms: 500 },
        ];
        let spans = [RepeatSpan { list: HostList::Steps, repeat_index: 0, name: "r".into(), started_utc_ms: 50, ended_utc_ms: 1000 }];
        let tap = |_: &str| Some(TapInput { raw: &raw2, arrivals: &arrivals });
        let v = render(&[lc("hr", "t", "hard reset")], &tap, &markers, &spans).unwrap();
        assert_eq!(v[0].total, 1);
        assert_eq!(v[0].tap_lines, 1);
        // Pass 0 (first byte at 100 < 500), not pass 1 (where the second record landed).
        assert_eq!(v[0].repeats[0].passes.iter().map(|p| p.count).collect::<Vec<_>>(), [1, 0]);
        assert_eq!(v[0].outside_passes, 0);
    }

    #[test]
    fn lines_before_the_first_pass_and_after_the_last_are_outside() {
        let raw = b"x\nx\nx\nx\n";
        let arrivals = [(0u64, 10u64), (2, 120), (4, 250), (6, 900)];
        let markers = [IterationMarker { list: HostList::Steps, repeat_index: 1, pass: 0, value: Some("5".into()), started_utc_ms: 100 }];
        let spans = [RepeatSpan { list: HostList::Steps, repeat_index: 1, name: "r".into(), started_utc_ms: 100, ended_utc_ms: 300 }];
        let tap = |_: &str| Some(TapInput { raw, arrivals: &arrivals });
        let v = render(&[lc("x", "t", "x")], &tap, &markers, &spans).unwrap();
        assert_eq!((v[0].total, v[0].outside_passes), (4, 2));
        assert_eq!(v[0].repeats[0].passes, vec![PassCount { pass: 0, value: Some("5".into()), count: 2 }]);
    }

    #[test]
    fn an_empty_tap_counts_zero_and_a_bad_regex_is_named() {
        let tap = |_: &str| None;
        let v = render(&[lc("x", "t", "x")], &tap, &[], &[]).unwrap();
        assert_eq!((v[0].total, v[0].tap_lines), (0, 0));
        let mut bad = lc("bad", "t", "(");
        bad.kind = PatternKind::Regex;
        let raw = b"a\n";
        let arr = [(0u64, 1u64)];
        let tap = |_: &str| Some(TapInput { raw, arrivals: &arr });
        assert!(render(&[bad], &tap, &[], &[]).unwrap_err().contains("bad"));
    }
}
