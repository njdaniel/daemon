//! LLM providers and the structured suggestion contract.
//!
//! Input: assembled context plus the candidate question.
//! Output: one JSON `Suggestion` in two phases (fast: question / kind / focus / approach; slow: the rest),
//! length-limited in `limits.rs` after parsing. Providers: Ollama (default), OpenAI-compatible, Anthropic.
//! Owns: the in-flight request `CancellationToken`; a newer unpinned question cancels the current request.
//!
//! Issue #11 ships the [`Provider`] trait, the [`ollama`] client and [`MockProvider`]. Prompts,
//! schema and `limits.rs` arrive with #12; the OpenAI-compatible and Anthropic providers with M2.
//!
//! **Trait shape.** [`Provider::chat`] is a plain `fn` returning a [`BoxFuture`] that resolves to a
//! [`DeltaStream`] (a `BoxStream` of [`Delta`]s). No `async fn` in the trait and no macro crate:
//! boxing both the future and the stream keeps the trait object-safe, so the pipeline can hold a
//! `Box<dyn Provider>` chosen from `[llm].provider` at start, and it adds no dependency.
//! Every stream is wrapped by [`guard`], which ends it with [`LlmError::Cancelled`] when the
//! request's `CancellationToken` fires and with [`LlmError::Timeout`] when the phase deadline passes.
//! Dropping the stream drops the HTTP response, which closes the connection promptly.

// CLAUDE.md: no unwrap/expect outside tests and main.
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

pub mod mock;
pub mod ndjson;
pub mod ollama;
mod provider;

pub use futures_util::future::BoxFuture;
pub use futures_util::stream::BoxStream;
pub use mock::MockProvider;
pub use ollama::OllamaProvider;
pub use provider::{
    ChatRequest, Delta, DeltaStream, LlmError, Message, Provider, Role, Usage, guard, guard_until,
};
pub use tokio_util::sync::CancellationToken;
