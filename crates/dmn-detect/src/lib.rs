//! Question detector (Tier-C rules).
//!
//! Input: every final [`Segment`](dmn_transcript::Segment) from the transcript bus whose lane
//! [`QuestionSource`] selects (`[detect].question_source`; the Them lane by default, #90).
//! Output: scored [`Candidate`]s (the question sentence, its score and the cues behind it) for the
//! LLM fast phase, plus [`PinState`], the rule that a pinned question is never replaced.
//! Owns: the bounded candidate channel from [`candidate_channel`] into `dmn-llm`.
//!
//! Detection is a pure function of the line in front of it, so the SPEC's 100 ms budget is never
//! in question: scoring one line is a few microseconds, and the detector never waits for what
//! comes next (see [`Following`]). No dependency on `dmn-core`: `dmn-core` wires the stages.

// CLAUDE.md: no unwrap/expect outside tests and main.
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

mod config;
mod cue;
mod detector;
mod pin;
mod sentence;

use crossbeam_channel::{Receiver, Sender};

pub use config::{DetectConfig, QuestionSource, Sensitivity};
pub use cue::Cue;
pub use detector::{Candidate, Detector, FOLLOW_SILENCE, Following};
pub use pin::{Offered, PinState};
pub use sentence::{Sentence, split as split_sentences, words};

/// Default depth of the candidate channel. Questions arrive minutes apart; anything beyond a
/// handful means the consumer has stopped.
pub const DEFAULT_CAPACITY: usize = 32;

/// The bounded channel the detector publishes candidates on.
pub fn candidate_channel(capacity: usize) -> (Sender<Candidate>, Receiver<Candidate>) {
    crossbeam_channel::bounded(capacity.max(1))
}
