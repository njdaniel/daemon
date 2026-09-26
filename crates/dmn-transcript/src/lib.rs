//! Transcript types and bus.
//!
//! Input: [`Segment`]s from `dmn-stt` (one partial per second while an utterance is open, one
//! final when it closes).
//! Output: the same segments fanned out to every subscriber, and a [`TranscriptLog`] in which a
//! final has replaced its utterance's partials, forced 12 s splits read as one line, and
//! [`TranscriptLog::window`] gives the last 60 s the LLM prompt carries.
//! Owns: the [`Bus`] channel every downstream stage (detector, session, UI) subscribes to, and
//! the [`EchoGuard`] the pipeline runs the bus's segments through: a mic final that duplicates
//! the loopback within 300 ms is dropped and counted (docs/SPEC.md § Audio capture, #60).
//!
//! No dependency on `dmn-core` or `dmn-stt`: this crate is the shape of the data, not a stage.

// CLAUDE.md: no unwrap/expect outside tests and main.
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

mod bus;
mod echo;
mod log;
mod segment;

pub use bus::{Bus, DEFAULT_CAPACITY};
pub use echo::{
    EchoGuard, HOLD, MAX_HOLD, MIN_CONTAINED_WORDS, MIN_SIMILARITY, Verdict, WINDOW, containment,
    similarity,
};
pub use log::{Applied, DEFAULT_WINDOW, TranscriptLog};
pub use segment::{MERGE_GAP, Segment, continues, merge_adjacent, normalise, timestamp};
