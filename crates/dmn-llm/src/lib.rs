//! LLM providers and the structured suggestion contract.
//!
//! Input: assembled context plus the candidate question.
//! Output: one JSON `Suggestion` in two phases (fast: question / kind / focus / approach; slow: the rest),
//! length-limited in `limits.rs` after parsing. Providers: Ollama (default), OpenAI-compatible, Anthropic.
//! Owns: the in-flight request `CancellationToken`; a newer unpinned question cancels the current request.
//!
//! Issue #11 ships the [`Provider`] trait, the [`ollama`] client and [`MockProvider`]. #12 adds the
//! fast phase: per-kind prompts ([`prompt`]), the [`schema`], [`limits`], and [`run_fast_phase`],
//! which parses the streamed reply incrementally ([`stream_json`]) and hands each [`FastEvent`]
//! (question first, then approach lines one by one) to a caller-supplied callback; the session
//! crate owns what happens to them. #54 adds [`requirements`]: the one-off job-description
//! summary into ≤ 8 bullets that the fast phase's prompt then carries, with the retrieved chunks,
//! in [`FastRequest`]. #55 adds the [`slow`] phase: after the fast phase has answered, a second
//! request fills the six expandable sections, each published ([`SlowField`]) as its field
//! completes, cut in Rust, within a 20 s deadline; its caller cancels it whenever a new question
//! needs the model. #59 adds the remote providers: [`openai`] (OpenAI-compatible chat
//! completions — OpenAI, OpenRouter, vLLM, LM Studio) and [`anthropic`] (tool use with the
//! schema), both streaming SSE ([`sse`]) into the same incremental parser, and both taking the
//! API key only as a header from the env var `[llm].api_key_env` names — never logged, never in
//! a `Debug` impl.
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

pub mod anthropic;
pub mod fast;
pub mod limits;
pub mod mock;
pub mod ndjson;
pub mod ollama;
pub mod openai;
pub mod prompt;
mod provider;
pub mod requirements;
pub mod schema;
pub mod slow;
pub mod sse;
pub mod stream_json;
#[cfg(test)]
mod testserver;

pub use anthropic::AnthropicProvider;
pub use fast::{FastEvent, FastOutcome, IgnoreReason, run_fast_phase};
pub use futures_util::future::BoxFuture;
pub use futures_util::stream::BoxStream;
pub use mock::MockProvider;
pub use ollama::{OllamaProvider, OllamaSettings};
pub use openai::OpenAiProvider;
pub use prompt::{Asker, CurrentQuestion, FastRequest, StoryText};
pub use provider::{
    ChatRequest, Delta, DeltaStream, LlmError, Message, Provider, Role, Usage, guard, guard_until,
};
pub use requirements::{Requirements, summarise_job_description};
pub use schema::{Kind, Suggestion};
pub use slow::{
    CodingMode, PreviousAnswer, SlowField, SlowFields, SlowOutcome, SlowRequest, run_slow_phase,
};
pub use tokio_util::sync::CancellationToken;
