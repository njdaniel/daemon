//! Context assembly: source ingestion, retrieval and session memory.
//!
//! Input: resume / job description / notes / stories files (PDF, Markdown, plain text) given to
//! `dmn sources add`; per question (#54), the question text as a retrieval query; per answered
//! question (#57), when it was asked and what it probed.
//! Output: the documents' text cut into overlapping chunks of about 300 tokens, stored through
//! `dmn-store` (#53); a [`Library`] (#54) loaded from those chunks that returns the best chunks
//! for a query (BM25, top 6, ≤ 1,500 tokens) and holds the job description's text and content
//! hash for its one-off summary (the summary itself is an LLM call, made by the caller through
//! `dmn-llm` and cached in `dmn-store`); the clock and fallback text of a session summary line
//! ([`memory`], #57: the line itself comes from the slow phase and lives in the session).
//! Owns: the `sources` and `chunks` tables, written through `dmn-store`. No channel: ingestion
//! is a synchronous CLI step, and retrieval and summary lines are synchronous calls on the
//! suggestion thread.
//!
//! Token counts are an approximation: whitespace-separated words × 1.3, rounded up. That is close
//! enough for English prose to size chunks and cap prompts, and needs no tokenizer model.

// CLAUDE.md: no unwrap/expect outside tests and main.
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

pub mod bm25;
mod chunk;
mod error;
mod extract;
mod ingest;
mod kind;
mod library;
pub mod memory;

pub use chunk::{CHUNK_TOKENS, Chunk, OVERLAP_TOKENS, approx_tokens, chunk, join};
pub use error::ContextError;
pub use extract::{Format, extract};
pub use ingest::{Added, Ingested, Refused, ingest, remove};
pub use kind::SourceKind;
pub use library::{
    JobDescription, Library, Passage, RETRIEVE_TOKENS, RETRIEVE_TOP, Retrieved, content_hash,
};
