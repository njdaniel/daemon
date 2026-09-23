//! Everything that can change a [`Session`](crate::Session), as plain serialisable data.
//!
//! Actions carry their own facts: times are in the action (never read from a clock by the
//! reducer) and ids are assigned by whoever dispatches (candidate ids by the pipeline, in order).
//! That is what makes a recorded action log replay to an identical `Session`, and what lets the
//! store writer turn each action into rows without asking anyone.
//!
//! The types here are this crate's own, not `dmn-transcript`'s, `dmn-detect`'s or `dmn-llm`'s:
//! `dmn-core` maps pipeline values into actions, so this crate depends on none of them.

use serde::{Deserialize, Serialize};

/// Which capture stream a line came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Speaker {
    Interviewer,
    You,
    Unknown,
}

/// A status dot (SPEC § Transcript & status): mic, system audio, model.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Dot {
    Mic,
    System,
    Model,
}

/// The state of one status dot. `Error` carries the tooltip text.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase", tag = "state", content = "detail")]
pub enum Health {
    #[default]
    Unknown,
    Ok,
    Muted,
    Error(String),
}

/// STAR cards for a behavioral question (SPEC § AI Assistance).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StarCards {
    pub situation: String,
    pub task: String,
    pub action: String,
    pub result: String,
    pub story: Option<String>,
}

/// Why the fast phase put nothing on screen for a candidate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IgnoreReason {
    NotAQuestion,
    SmallTalk,
}

/// One change to the session. Serialised with a `type` tag, one JSON object per action, so an
/// action log is JSON Lines.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Action {
    /// A session begins. `at` is unix epoch seconds.
    Started { label: String, at: i64 },
    /// A transcript segment from the bus: a partial of an open utterance, or its final. Times are
    /// seconds from the session start. An empty final closes a dropped utterance.
    Segment {
        speaker: Speaker,
        start: f64,
        end: f64,
        text: String,
        is_final: bool,
        avg_logprob: f64,
    },
    /// The detector proposed the final interviewer segment that starts at `start` as a question.
    /// `id` is assigned by the pipeline and tags every fast-phase action for this candidate.
    Candidate {
        id: u64,
        speaker: Speaker,
        start: f64,
        end: f64,
        text: String,
        score: i32,
    },
    /// Fast phase: the extracted question, first field of a shown reply.
    Question {
        candidate: u64,
        text: String,
        kind: String,
    },
    /// Fast phase: a refinement of the question on screen.
    Clarification { candidate: u64, text: String },
    /// Fast phase: the focus line.
    Focus { candidate: u64, text: String },
    /// Fast phase: approach line `index` (0-based, contiguous), already length-limited.
    ApproachLine {
        candidate: u64,
        index: usize,
        text: String,
    },
    /// Fast phase: STAR cards for a behavioral question.
    Star { candidate: u64, cards: StarCards },
    /// The fast phase finished and showed something. `payload` is the prompt-contract JSON of
    /// exactly what was shown, for the store.
    Answered {
        candidate: u64,
        clarification: bool,
        payload: String,
        at: i64,
    },
    /// The fast phase decided the candidate is not to be shown.
    Ignored {
        candidate: u64,
        reason: IgnoreReason,
        score: i32,
    },
    /// The fast phase failed (timeout, unreachable, malformed). Anything already shown stays.
    Failed { candidate: u64, error: String },
    /// `p`: pin the live question; new questions queue behind it.
    Pin,
    /// Unpin: the newest queued question (if any) becomes live.
    Unpin,
    /// A status dot changed.
    Status { dot: Dot, health: Health },
    /// The session ends. `at` is unix epoch seconds.
    Ended { at: i64 },
}
