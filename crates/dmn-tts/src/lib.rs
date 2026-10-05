//! Text-to-speech in the user's cloned voice through a local server (#107).
//!
//! Input: text plus the name of a voice reference enrolled with the server
//! ([`TtsProvider::enroll`]; the clip itself is kept under the data dir by the CLI).
//! Output: a [`PcmStream`] of mono f32 [`PcmChunk`]s at the server's sample rate.
//! Owns: one bounded crossbeam channel per synthesis, fed by a decoder thread that reads the
//! HTTP response body straight into hound — backpressure is the channel bound, and dropping
//! the [`PcmStream`] stops the download. Playback (the virtual microphone) is #108's; the
//! translation that produces the text is #109's.
//!
//! The server is a sidecar the user runs, exactly like Ollama: Chatterbox-TTS-Server
//! (Chatterbox, MIT, zero-shot voice cloning) on `[speak].tts_url`, speaking the
//! OpenAI-compatible `POST /v1/audio/speech`. [`ChatterboxProvider`] is the real client,
//! [`MockTtsProvider`] replays a generated sine WAV so nothing in tests or the CLI needs a
//! live server.
//!
//! **Trait shape.** [`TtsProvider::synthesize`] and [`TtsProvider::enroll`] are plain `fn`s
//! returning [`BoxFuture`]s, mirroring `dmn-llm`'s `Provider`: no `async fn` in the trait and
//! no macro crate, so the trait stays object-safe and a caller can hold `Box<dyn TtsProvider>`.
//! `synthesize`'s future resolves once the server has answered and the WAV header is parsed —
//! connect-refused, a non-2xx status and a malformed header are its `Err`; a decode failure
//! mid-body arrives as an `Err` item on the stream.

// CLAUDE.md: no unwrap/expect outside tests and main.
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

mod chatterbox;
mod decode;
mod mock;
mod provider;
#[cfg(test)]
mod testserver;

pub use chatterbox::ChatterboxProvider;
pub use futures_util::future::BoxFuture;
pub use mock::MockTtsProvider;
pub use provider::{PcmChunk, PcmStream, SpeakRequest, TtsError, TtsProvider};
