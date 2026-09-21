//! LLM providers and the structured suggestion contract.
//!
//! Input: assembled context plus the candidate question.
//! Output: one JSON `Suggestion` in two phases (fast: question / kind / focus / approach; slow: the rest),
//! length-limited in `limits.rs` after parsing. Providers: Ollama (default), OpenAI-compatible, Anthropic.
//! Owns: the in-flight request `CancellationToken`; a newer unpinned question cancels the current request.
