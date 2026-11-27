use serde::{Deserialize, Serialize};

/// One traced subject's share of the capture window: the "load repartition"
/// `embarch-ui` decision 10 (trace) defines. `kind` is `thread`, `idle` or
/// `isr`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LoadSubject {
    pub key: String,
    pub label: String,
    pub unnamed: bool,
    pub kind: String,
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
/// `unit` is the time base the extents are in (`us`, `ms` or `frame`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LoadSummary {
    pub unit: String,
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

/// `GET /study/{study_id}/stream/{name}/load`'s body: [`LoadSummary`], plus
/// what a caller needs to judge whether the rendered CSV was read in full.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LoadAnswer {
    pub rows: usize,
    /// Rows past Core's row cap: counted here, though dropped from the
    /// computation.
    pub rows_dropped_by_cap: usize,
    pub row_cap: usize,
    /// Rows the parser refused: a line short of nine fields, or a
    /// `frame_index` that did not parse.
    pub rows_unparsed: usize,
    pub summary: LoadSummary,
}

#[cfg(test)]
mod tests {
    use super::*;

    const PINNED: &str = concat!(
        r#"{"rows":9,"rows_dropped_by_cap":0,"row_cap":100,"rows_unparsed":1,"summary":{"#,
        r#""unit":"us","window_extent":400,"gap_extent":10,"gap_fraction":0.025,"records_lost":2,"#,
        r#""has_time_base":true,"thread_extent":300,"idle_record_extent":50,"isr_extent":20,"#,
        r#""unaccounted_extent":30,"below_resolution_spans":1,"subjects":[{"key":"t:main","#,
        r#""label":"main","unnamed":false,"kind":"thread","entries":3,"measured_spans":2,"#,
        r#""total_extent":100,"share":0.25,"excluded_spans":1,"excluded_extent":5,"#,
        r#""gap_crossing_spans":0,"open_ended_spans":1,"open_started_spans":0,"#,
        r#""below_resolution_spans":0}]}}"#
    );

    #[test]
    fn the_load_answer_is_the_pinned_json() {
        let answer: LoadAnswer = serde_json::from_str(PINNED).unwrap();
        assert_eq!(answer.summary.subjects[0].kind, "thread");
        assert_eq!(serde_json::to_string(&answer).unwrap(), PINNED);
    }
}
