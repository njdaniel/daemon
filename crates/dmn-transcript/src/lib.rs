//! Transcript types and bus.
//!
//! Input: `Segment`s from `dmn-stt`.
//! Output: merged, punctuated segments on the transcript bus, plus the last-60-seconds window.
//! Owns: the transcript broadcast channel every downstream stage subscribes to.

// CLAUDE.md: no unwrap/expect outside tests and main.
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]
