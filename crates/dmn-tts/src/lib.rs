//! Text-to-speech in the user's cloned voice through a local server (#107).
//!
//! Input: text plus the name of a voice enrolled with the server ([`SpeakRequest`];
//! [`TtsProvider::enroll`] registers a reference clip under a name).
//! Output: a [`PcmStream`] of mono f32 [`PcmChunk`]s, each carrying the server's sample rate,
//! asked for one sentence at a time and cut at the first long silence after each sentence's
//! speech, so the model's end-of-text noise never reaches the caller (#113, `tail.rs`);
//! and the server's voice library ([`TtsProvider::voices`], #109), each [`Voice`] with the
//! language it was enrolled with.
//! Owns: one bounded crossbeam channel per synthesis (the [`PcmStream`]'s receiver), fed by a
//! decoder thread that reads the HTTP body as it arrives: backpressure is the channel bound,
//! and dropping the [`PcmStream`] stops the download. Playback (the virtual microphone) is
//! #108's; the translation that produces the text is #109's.
//!
//! The server is a sidecar the user runs, exactly like Ollama, on `[speak].tts_url`; which one
//! is `[speak].engine` (#122). [`ChatterboxProvider`] speaks to chatterbox-tts-api (an
//! OpenAI-compatible FastAPI server around Resemble AI's Chatterbox, zero-shot voice cloning,
//! one language per voice); [`Qwen3Provider`] to Qwen3-TTS on vLLM-Omni (`sidecars/qwen3-tts/`:
//! streamed, first audio in under 0.1 s, one voice for every language it speaks). Each module
//! doc lists its server's requests; `http.rs` holds what they share. [`MockTtsProvider`]
//! replays a generated sine WAV so tests need no live server. #130: [`Qwen3Designer`] renders
//! a sample in a voice described in words (Qwen3-TTS VoiceDesign, `design.rs`), which is then
//! enrolled like any clip ([`TtsProvider::enroll_as`], [`Consent::Designed`]).
//!
//! **Trait shape.** [`TtsProvider::synthesize`] and [`TtsProvider::enroll`] are plain `fn`s
//! returning [`BoxFuture`]s, mirroring `dmn-llm`'s `Provider`: no `async fn` in the trait and
//! no macro crate, so the trait stays object-safe and a caller can hold `Box<dyn TtsProvider>`.
//! `synthesize`'s future resolves once the server has answered and the WAV header is parsed:
//! an unreachable server, a non-2xx status, an unknown voice and a body that is not a WAV are
//! its `Err`; a failure mid-body arrives as an `Err` item on the stream.

// CLAUDE.md: no unwrap/expect outside tests and main.
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

mod chatterbox;
mod decode;
mod design;
mod http;
mod mock;
mod provider;
mod qwen3;
mod sentences;
mod tail;
#[cfg(any(test, feature = "test-server"))]
pub mod testserver;

pub use chatterbox::ChatterboxProvider;
pub use design::{DesignRequest, Designed, QWEN3_DESIGN_MODEL, Qwen3Designer};
pub use futures_util::future::BoxFuture;
pub use mock::{MockEnrollment, MockTtsProvider};
pub use provider::{
    Consent, Enrolled, PcmChunk, PcmStream, SpeakRequest, TtsError, TtsProvider, Tuning, Voice,
};
pub use qwen3::{QWEN3_LANGUAGES, QWEN3_MODEL, Qwen3Provider, qwen3_language};
