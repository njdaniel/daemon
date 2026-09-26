//! Persistence (SQLite via `rusqlite`, bundled).
//!
//! Input: sessions, segments, questions, suggestions, chunks, summaries.
//! Output: queries for retrieval and session memory; JSONL export of a session.
//! Owns: `~/.local/share/dmn/dmn.sqlite` and its migration runner. Never stores API keys or audio.
//!
//! Issue #13 ships the migration runner, the v1 schema (sessions / segments / questions /
//! suggestions), the [`Store`] handle and the JSONL export. Schema v2 (#16) adds per-question
//! timing, CPU samples and the overrun count for `dmn bench --live`. Schema v4 (#53) adds the
//! context `sources` and their `chunks`; v5 (#54) caches the job-description summary by content
//! hash; v6 (#57) keeps the session memory: a summary line per answered question and the
//! promises made in them. [`Store::open_read_only`] lets a
//! `--no-store` run read the context without writing anything.

// CLAUDE.md: no unwrap/expect outside tests and main.
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

mod error;
mod export;
mod migrate;
mod sources;
mod store;
mod summaries;
#[cfg(test)]
mod tests;

pub use error::StoreError;
pub use migrate::SCHEMA_VERSION;
pub use sources::{
    ChunkRow, NewChunk, NewSourceDoc, SourceChunk, SourceDocRow, SourceWrite, SourcesSignature,
};
pub use store::{
    CpuSampleRow, Mark, NewCpuSample, NewQuestion, NewSegment, NewSession, NewSuggestion, Phase,
    QuestionRow, SegmentRow, SessionRow, Source, Store, SuggestionRow,
};
pub use summaries::{NewSummary, SummaryRow};
