//! Context assembly: source ingestion, retrieval and session memory.
//!
//! Input: resume / job description / notes / stories files (PDF, Markdown, plain text) given to
//! `dmn sources add`; later (#54) the extracted question.
//! Output: the documents' text cut into overlapping chunks of about 300 tokens, stored through
//! `dmn-store` (#53); later the retrieved chunks (≤ 1,500 tokens, BM25), the job-description
//! requirement bullets (#54) and the last 8 summary lines (#57).
//! Owns: the `sources` and `chunks` tables, written through `dmn-store`. No channel yet:
//! ingestion is a synchronous CLI step; the session summariser task arrives with #57.
//!
//! Token counts are an approximation: whitespace-separated words × 1.3, rounded up. That is close
//! enough for English prose to size chunks and cap prompts, and needs no tokenizer model.

// CLAUDE.md: no unwrap/expect outside tests and main.
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

mod chunk;
mod error;
mod extract;
mod ingest;
mod kind;

pub use chunk::{CHUNK_TOKENS, Chunk, OVERLAP_TOKENS, approx_tokens, chunk};
pub use error::ContextError;
pub use extract::{Format, extract};
pub use ingest::{Added, Ingested, Refused, ingest, remove};
pub use kind::SourceKind;
