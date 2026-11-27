use embarch_study_designer::host_study::{PromptAnswer, PromptKind};
use embarch_study_designer::{GattTranscriptEntry, HostList, HostStepResult, Sample, StepResult};
use serde::de::Error as _;
use serde::ser::Error as _;
use serde::{Deserialize, Serialize};

/// One live event pushed to every subscriber of `GET /study/{study_id}/events`
/// (SSE) the instant Core processes it, never buffered until the study ends.
/// Every variant carries `study_id` so a subscriber that reconnects across
/// studies can tell them apart.
///
/// **Strict on purpose.** A `kind` this build does not know fails to parse,
/// and a client turns that into its "unrecognized frame" (it keeps the raw
/// data and the reason), which is what an older client needs when a newer Core
/// adds a variant. A `#[serde(other)]` variant would instead hand every Core
/// `match` a variant Core never sends, and lose the data the frame carried.
///
/// Variant order and field order are the wire's: a test pins each variant's
/// JSON byte for byte.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind")]
pub enum StudyEvent {
    /// A step just completed: the `StepResult` Core just appended to
    /// `events.json`, with when it started and ended and the delay the study
    /// declared before it (which falls inside that window). Boxed because a
    /// result is tens of KB next to this enum's small variants.
    StepCompleted {
        study_id: String,
        step_index: u32,
        result: Box<StepResult>,
        started_utc_ms: u64,
        ended_utc_ms: u64,
        delay_before_ms: u32,
    },
    /// One batch of power/waveform samples, keyed by the tap's index and name
    /// in `Study.streams`. `core_rx_utc_ms` is Core's own receipt time for the
    /// record; a `Sample`'s own `rx_utc_ms` is dev-bench uptime on a
    /// bench-mediated tap (suite decision 3).
    SampleBatch {
        study_id: String,
        stream_id: u8,
        stream_name: String,
        samples: Vec<Sample>,
        core_rx_utc_ms: u64,
    },
    /// One GATT transcript entry. `core_rx_utc_ms` is Core's epoch clock, the
    /// value `gatt.csv`'s last column carries; `entry.rx_utc_ms` keeps its name
    /// and is dev-bench uptime. Both travel, both named as what they are.
    GattTranscript {
        study_id: String,
        step_index: u32,
        entry: Box<GattTranscriptEntry>,
        core_rx_utc_ms: u64,
    },
    /// One chunk off a `Text`-encoded tap, carried verbatim with no line
    /// framing: a chunk can split a line or a UTF-8 character, and assembling
    /// lines is the consumer's job. `Raw` taps push nothing. `rx_utc_ms` is the
    /// record's own clock and `core_rx_utc_ms` is Core's; the caller cannot
    /// tell from the event which kind of tap this is, so both are carried.
    StreamText {
        study_id: String,
        stream_id: u8,
        stream_name: String,
        step_index: u32,
        rx_utc_ms: u64,
        core_rx_utc_ms: u64,
        text: String,
    },
    /// One frame of an `OutpostTrace` tap, decoded as it arrives
    /// (`embarch-outpost` decision 10 reversed). `rows` are CSV lines in
    /// `embarch_study_designer::outpost::csv_header`'s shape. The post-hoc
    /// render stays authoritative. `header_seen: false` means the frame decoded
    /// before any header arrived, so its rows carry an empty `us` and `name`.
    OutpostRows {
        study_id: String,
        stream_id: u8,
        stream_name: String,
        frame_index: u64,
        frame_seq: u32,
        rx_utc_ms: u64,
        header_seen: bool,
        rows: Vec<String>,
    },
    /// One `chart_field` value off a `Struct`-decoded row
    /// (`embarch-study-designer` decision 52's live half), sent only when the
    /// layout declares a `chart_field`.
    StructChartValue {
        study_id: String,
        stream_id: u8,
        stream_name: String,
        field_name: String,
        value: f64,
        core_rx_utc_ms: u64,
    },
    /// A host study's step ended (`embarch-core` decision 95): the
    /// study-designer `HostStepResult`, the list it ran in, its position there
    /// and Core's `detail` word for why the outcome is what it is.
    HostStepCompleted {
        study_id: String,
        list: HostList,
        step_index: u32,
        result: Box<HostStepResult>,
        detail: String,
        started_utc_ms: u64,
        ended_utc_ms: u64,
        delay_before_ms: u32,
    },
    /// A host study's `Prompt` step is waiting for a person (decision 105).
    /// The prompt's fields sit beside `study_id`, and its `kind` travels as
    /// `prompt_kind` because `kind` is this event's own tag: two of them on one
    /// object lose the tag to any reader that keeps the last (decision 151).
    /// `GET /study/{id}` serializes the prompt itself and keeps `kind`.
    PromptPending {
        study_id: String,
        #[serde(flatten, serialize_with = "flatten_pending_prompt", deserialize_with = "unflatten_pending_prompt")]
        prompt: PendingPrompt,
    },
    /// The pending prompt ended: `answer` is `confirmed`, `cancelled`,
    /// `timed_out` or `study_cancelled`, and `answered_by` names the surface
    /// for the first two.
    PromptResolved {
        study_id: String,
        prompt_id: u32,
        answer: String,
        answered_by: Option<AnsweredBy>,
        resolved_utc_ms: u64,
    },
    /// The job's own `status`/`reason` changed: `"completed"` or `"failed"`.
    StatusChanged { study_id: String, status: String, reason: Option<String> },
}

/// The prompt a host study is waiting on, as `PromptPending` and
/// `GET /study/{id}` show it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PendingPrompt {
    /// 1-based within the study. An answer names it, so an answer meant for a
    /// prompt that has already ended can never be taken for the next one.
    pub prompt_id: u32,
    pub list: HostList,
    /// Position in `list`; for a prompt inside a `Repeat`, the `Repeat`'s position.
    pub step_index: u32,
    pub step_name: String,
    pub text: String,
    /// The bench configuration the step names, forwarded as the study wrote it
    /// (decision 107). Absent when the step names none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bench_config: Option<String>,
    /// The prompt's kind, from the Test run's `run.json` (decision 138); absent
    /// outside one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<PromptKind>,
    /// An `observation`'s declared answer: the choices, or the number and its unit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observation: Option<PromptAnswer>,
    /// The outputs Core last switched on, from the energising steps it ran
    /// (decision 138). Absent when none, and always outside a Test run.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub switched_on: Vec<SwitchedOn>,
    pub opened_utc_ms: u64,
    pub deadline_utc_ms: u64,
    /// Set only on a device study's session prompt (Start or Done, decision 127).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<SessionPromptInfo>,
}

/// One output Core last switched on, by the energising step it ran (decision 138).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SwitchedOn {
    /// The signal or instrument the step addressed.
    pub target: String,
    /// The line it sent.
    pub line: String,
    pub step: String,
}

/// Which of a session's two prompts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionPhase {
    Start,
    Done,
}

/// One run field as a showing surface needs it to draw the form.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunFieldInfo {
    pub name: String,
    pub label: String,
    pub required: bool,
}

/// What a prompt says about itself when it belongs to a session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionPromptInfo {
    pub phase: SessionPhase,
    /// The Start prompt's run fields, in declared order; empty on Done.
    #[serde(default)]
    pub run_fields: Vec<RunFieldInfo>,
}

/// Which surface relayed an answer. Recorded in the step's `answered_by`;
/// never used to decide anything, since every surface relays the same person.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AnsweredBy {
    Ui,
    Mcp,
    Cli,
}

impl AnsweredBy {
    pub fn as_str(self) -> &'static str {
        match self {
            AnsweredBy::Ui => "ui",
            AnsweredBy::Mcp => "mcp",
            AnsweredBy::Cli => "cli",
        }
    }
}

// Where a prompt's `kind` goes inside `PromptPending`, and where it comes from.
const PROMPT_KIND_KEY: &str = "prompt_kind";
const KIND_KEY: &str = "kind";

// Goes through a `serde_json::Value`, whose object keys are sorted: that is
// the order `PromptPending` has always had, and what the byte-for-byte pin
// below holds.
fn flatten_pending_prompt<S: serde::Serializer>(prompt: &PendingPrompt, ser: S) -> Result<S::Ok, S::Error> {
    let mut value = serde_json::to_value(prompt).map_err(S::Error::custom)?;
    if let Some(map) = value.as_object_mut() {
        if let Some(kind) = map.remove(KIND_KEY) {
            map.insert(PROMPT_KIND_KEY.to_string(), kind);
        }
    }
    Serialize::serialize(&value, ser)
}

fn unflatten_pending_prompt<'de, D: serde::Deserializer<'de>>(de: D) -> Result<PendingPrompt, D::Error> {
    let mut map = serde_json::Map::<String, serde_json::Value>::deserialize(de)?;
    if let Some(kind) = map.remove(PROMPT_KIND_KEY) {
        map.insert(KIND_KEY.to_string(), kind);
    }
    serde_json::from_value(serde_json::Value::Object(map)).map_err(D::Error::custom)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Each line is what Core sent before this crate existed, captured from
    // Core's own `StudyEvent` serializer. Parsing it and writing it back must
    // give the same bytes: one pin per variant.
    const PINNED: &[(&str, &str)] = &[
        ("StepCompleted", r#"{"kind":"StepCompleted","study_id":"s1","step_index":2,"result":{"step_name":"a","outcome":"Pass","captured_data":null,"gatt_services":null,"security_level":null,"protocol":null},"started_utc_ms":10,"ended_utc_ms":20,"delay_before_ms":5}"#),
        ("SampleBatch", r#"{"kind":"SampleBatch","study_id":"s1","stream_id":1,"stream_name":"rail","samples":[{"rx_utc_ms":1,"value":3.3,"unit":"Volts","channel_id":0}],"core_rx_utc_ms":99}"#),
        ("GattTranscript", r#"{"kind":"GattTranscript","study_id":"s1","step_index":3,"entry":{"rx_utc_ms":4242,"direction":"In","kind":"Notification","service_uuid":[110,64,0,1,181,163,243,147,224,169,229,14,36,220,202,158],"characteristic_uuid":[110,64,0,3,181,163,243,147,224,169,229,14,36,220,202,158],"att_status":0,"payload":[1,2]},"core_rx_utc_ms":77}"#),
        ("StreamText", r#"{"kind":"StreamText","study_id":"s1","stream_id":2,"stream_name":"con","step_index":1,"rx_utc_ms":5,"core_rx_utc_ms":6,"text":"hi\n"}"#),
        ("OutpostRows", r#"{"kind":"OutpostRows","study_id":"s1","stream_id":2,"stream_name":"tr","frame_index":7,"frame_seq":8,"rx_utc_ms":9,"header_seen":false,"rows":["a,b"]}"#),
        ("StructChartValue", r#"{"kind":"StructChartValue","study_id":"s1","stream_id":2,"stream_name":"st","field_name":"f","value":1.5,"core_rx_utc_ms":4}"#),
        ("HostStepCompleted", r#"{"kind":"HostStepCompleted","study_id":"h1","list":"Steps","step_index":1,"result":{"step_name":"x","outcome":"Pass","reply":"ok","reply_truncated":false,"answered_by":null,"elapsed_ms":3,"iteration":null},"detail":"waited","started_utc_ms":1,"ended_utc_ms":2,"delay_before_ms":0}"#),
        ("PromptPendingFull", r#"{"kind":"PromptPending","study_id":"h1","bench_config":"direct","deadline_utc_ms":2,"list":"Steps","observation":{"answers":["green","red"],"name":"led"},"opened_utc_ms":1,"prompt_id":4,"prompt_kind":"observation","session":{"phase":"start","run_fields":[{"label":"Serial","name":"serial","required":true}]},"step_index":2,"step_name":"ask","switched_on":[{"line":"OUTP ON","step":"on","target":"eload"}],"text":"plug in"}"#),
        ("PromptPendingBare", r#"{"kind":"PromptPending","study_id":"h1","deadline_utc_ms":2,"list":"Teardown","opened_utc_ms":1,"prompt_id":1,"step_index":0,"step_name":"n","text":"t"}"#),
        ("PromptResolved", r#"{"kind":"PromptResolved","study_id":"h1","prompt_id":4,"answer":"confirmed","answered_by":"mcp","resolved_utc_ms":9}"#),
        ("PromptResolvedNone", r#"{"kind":"PromptResolved","study_id":"h1","prompt_id":4,"answer":"timed_out","answered_by":null,"resolved_utc_ms":9}"#),
        ("StatusChanged", r#"{"kind":"StatusChanged","study_id":"s1","status":"failed","reason":"boom"}"#),
        ("StatusChangedNone", r#"{"kind":"StatusChanged","study_id":"s1","status":"completed","reason":null}"#),
    ];

    #[test]
    fn every_event_is_the_pinned_json() {
        for (name, json) in PINNED {
            let event: StudyEvent = serde_json::from_str(json).unwrap_or_else(|e| panic!("{name}: {e}"));
            assert_eq!(&serde_json::to_string(&event).unwrap(), json, "{name}");
        }
    }

    #[test]
    fn a_pending_prompt_reads_back_with_its_kind_and_leaves_the_tag_alone() {
        let json = PINNED.iter().find(|(n, _)| *n == "PromptPendingFull").unwrap().1;
        let StudyEvent::PromptPending { study_id, prompt } = serde_json::from_str(json).unwrap() else {
            panic!("not a PromptPending");
        };
        assert_eq!(study_id, "h1");
        assert_eq!(prompt.kind, Some(PromptKind::Observation));
        assert_eq!(prompt.switched_on.len(), 1);
        assert_eq!(prompt.session.unwrap().phase, SessionPhase::Start);
    }

    #[test]
    fn a_kind_this_build_does_not_know_fails_to_parse() {
        assert!(serde_json::from_str::<StudyEvent>(r#"{"kind":"FromTheFuture","study_id":"s1"}"#).is_err());
    }

    #[test]
    fn the_prompt_serializes_with_its_own_kind_outside_an_event() {
        let prompt: PendingPrompt = serde_json::from_str(
            r#"{"prompt_id":1,"list":"Steps","step_index":0,"step_name":"n","text":"t","kind":"clearance","opened_utc_ms":1,"deadline_utc_ms":2}"#,
        )
        .unwrap();
        assert_eq!(serde_json::to_value(&prompt).unwrap()["kind"], "clearance");
    }
}
