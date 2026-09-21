//! Persistence (SQLite via `rusqlite`, bundled).
//!
//! Input: sessions, segments, questions, suggestions, chunks, summaries.
//! Output: queries for retrieval and session memory; JSONL export of a session.
//! Owns: `~/.local/share/dmn/dmn.sqlite` and its migration runner. Never stores API keys or audio.

// CLAUDE.md: no unwrap/expect outside tests and main.
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]
