//! Persistence (SQLite via `rusqlite`, bundled).
//!
//! Input: sessions, segments, questions, suggestions, chunks, summaries.
//! Output: queries for retrieval and session memory; JSONL export of a session.
//! Owns: `~/.local/share/dmn/dmn.sqlite` and its migration runner. Never stores API keys or audio.
//!
//! Issue #13 ships the migration runner, the v1 schema (sessions / segments / questions /
//! suggestions), the [`Store`] handle and the JSONL export. Chunks and summaries arrive with the
//! M2 source-ingestion issue.

// CLAUDE.md: no unwrap/expect outside tests and main.
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

mod error;
mod export;
mod migrate;
mod store;
#[cfg(test)]
mod tests;

pub use error::StoreError;
pub use migrate::SCHEMA_VERSION;
pub use store::{
    NewQuestion, NewSegment, NewSession, NewSuggestion, Phase, QuestionRow, SegmentRow, SessionRow,
    Source, Store, SuggestionRow,
};
