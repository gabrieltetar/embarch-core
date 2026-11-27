//! The JSON `embarch-core` serves that a client also reads, defined once.
//!
//! Core builds these types and serializes them; a client (`embarch-core-client`)
//! deserializes the same ones, so a field added here is one edit and cannot
//! drift between the two. Nothing in this crate is Core's logic: no router, no
//! file, no clock. `embarch-core/decisions/wire.md` has the rule for what may
//! live here and what it may depend on.

mod hello;
mod lease;
mod outpost_load;
mod study_event;

pub use hello::HelloAckInfo;
pub use lease::LEASE_GATED;
pub use outpost_load::{LoadAnswer, LoadSubject, LoadSummary};
pub use study_event::{
    AnsweredBy, PendingPrompt, RunFieldInfo, SessionPhase, SessionPromptInfo, StudyEvent, SwitchedOn,
};
