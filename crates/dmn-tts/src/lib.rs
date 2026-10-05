//! Text-to-speech in the user's cloned voice through a local server (#107).
//!
//! Input: text plus the name of a voice enrolled with the server ([`SpeakRequest`];
//! [`TtsProvider::enroll`] registers a reference clip under a name).
//! Output: a [`PcmStream`] of mono f32 [`PcmChunk`]s, each carrying the server's sample rate.
//! Owns: one bounded crossbeam channel per synthesis (the [`PcmStream`]'s receiver), fed by a
//! decoder thread that reads the HTTP body as it arrives: backpressure is the channel bound,
//! and dropping the [`PcmStream`] stops the download. Playback (the virtual microphone) is
//! #108's; the translation that produces the text is #109's.
//!
//! The server is a sidecar the user runs, exactly like Ollama: chatterbox-tts-api (an
//! OpenAI-compatible FastAPI server around Resemble AI's Chatterbox, zero-shot voice cloning)
//! on `[speak].tts_url`. [`ChatterboxProvider`] is the real client (its module doc lists the
//! requests); [`MockTtsProvider`] replays a generated sine WAV so tests need no live server.
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
mod mock;
mod provider;
#[cfg(any(test, feature = "test-server"))]
pub mod testserver;

pub use chatterbox::ChatterboxProvider;
pub use futures_util::future::BoxFuture;
pub use mock::{MockEnrollment, MockTtsProvider};
pub use provider::{Enrolled, PcmChunk, PcmStream, SpeakRequest, TtsError, TtsProvider};
