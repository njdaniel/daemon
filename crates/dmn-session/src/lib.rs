//! Session state.
//!
//! Input: [`Action`]s: transcript segments, detector candidates, fast-phase fields and outcomes,
//! and user actions (pin, unpin, the coding-solutions mode); `dmn-core` maps pipeline values
//! into them.
//! Output: a single [`Session`] struct behind `tokio::sync::watch`, changed only by the pure
//! [`reduce`], so a recorded action log replays to an identical `Session` ([`Session::replay`]).
//! Owns: the `watch` channel the TUI (M1) and the Tauri bridge (M3) mirror, and the `broadcast`
//! of actions the store writer persists ([`SessionHub`]). The bridge coalesces emits to ≤ 30/s.

// CLAUDE.md: no unwrap/expect outside tests and main.
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

mod action;
mod hub;
mod state;
#[cfg(test)]
mod tests;

pub use action::{Action, CodingSolutions, Dot, Health, IgnoreReason, Speaker, Stage, StarCards};
pub use hub::{DEFAULT_ACTION_CAPACITY, SessionHub};
pub use state::{
    Clarification, Counts, Line, Progress, Question, Session, Status, TRANSCRIPT_WINDOW_S,
    ignore_label, reduce,
};
