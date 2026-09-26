//! Session state.
//!
//! Input: [`Action`]s: transcript segments, detector candidates, fast-phase fields and outcomes,
//! slow-phase sections (#55), session-memory summary lines (#57), and user actions (pin, unpin,
//! the coding-solutions mode); `dmn-core` maps pipeline values into them.
//! Output: a single [`Session`] struct behind `tokio::sync::watch`, changed only by the pure
//! [`reduce`], so a recorded action log replays to an identical `Session` ([`Session::replay`]).
//! Owns: the `watch` channel the TUI (M1) and the Tauri bridge (M3) mirror, and the `broadcast`
//! of actions the store writer persists ([`SessionHub`]). The bridge coalesces emits to ≤ 30/s.

// CLAUDE.md: no unwrap/expect outside tests and main.
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

mod action;
mod hub;
pub mod memory;
mod state;
#[cfg(test)]
mod tests;

pub use action::{
    Action, Citation, CodeBlock, CodingSolutions, DeeperItem, Dot, Health, IgnoreReason,
    ProviderLabel, Section, SectionState, SlowEnd, SlowField, Speaker, Stage, StarCards,
    TradeoffItem,
};
pub use hub::{DEFAULT_ACTION_CAPACITY, SessionHub};
pub use memory::{Promise, SummaryLine};
pub use state::{
    Clarification, Counts, Line, Progress, Question, SectionStates, Sections, Session, Status,
    TRANSCRIPT_WINDOW_S, ignore_label, reduce,
};
