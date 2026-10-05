//! The provider-agnostic request, chunk, stream and error types, and the [`TtsProvider`] trait.

use std::path::Path;

use futures_util::future::BoxFuture;

/// Everything a synthesis or enrollment can fail with.
#[derive(Debug, thiserror::Error)]
pub enum TtsError {
    #[error("TTS server unreachable at {url}: {detail}")]
    Unreachable { url: String, detail: String },
    #[error("TTS server returned HTTP {status}: {body}")]
    HttpStatus { status: u16, body: String },
    #[error("cannot decode the synthesized WAV: {detail}")]
    Decode { detail: String },
    #[error("cannot read voice reference {path}: {detail}")]
    VoiceRead { path: String, detail: String },
    #[error("cannot build the request: {0}")]
    Request(String),
    #[error("TTS worker failed: {0}")]
    Worker(String),
}

/// One synthesis job. `voice` is the name the server registered the enrolled clip under
/// (the CLI derives it from `[speak].voice_ref`'s file name).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpeakRequest {
    pub text: String,
    pub voice: String,
    /// A language hint for the server (`es`, `de`, ...); `None` lets it decide. The
    /// translation step that picks it is #109's.
    pub language: Option<String>,
}

impl SpeakRequest {
    pub fn new(text: impl Into<String>, voice: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            voice: voice.into(),
            language: None,
        }
    }

    pub fn with_language(mut self, language: impl Into<String>) -> Self {
        self.language = Some(language.into());
        self
    }
}

/// One decoded piece of the synthesized audio: mono f32 samples at `sample_rate`.
#[derive(Debug, Clone, PartialEq)]
pub struct PcmChunk {
    pub samples: Vec<f32>,
    pub sample_rate: u32,
}

/// The synthesized audio as it arrives, owned by this crate: the receiving end of the bounded
/// channel the decoder thread feeds. `None` from [`recv`](Self::recv) ends the audio; a decode
/// failure mid-body arrives as one `Err` item and then the end.
#[derive(Debug)]
pub struct PcmStream {
    rx: crossbeam_channel::Receiver<Result<PcmChunk, TtsError>>,
    sample_rate: u32,
}

impl PcmStream {
    pub(crate) fn new(
        rx: crossbeam_channel::Receiver<Result<PcmChunk, TtsError>>,
        sample_rate: u32,
    ) -> Self {
        Self { rx, sample_rate }
    }

    /// The rate the server's WAV header declared; every chunk carries the same.
    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    /// The next chunk, blocking until one arrives or the audio ends. When the caller is gone
    /// (this value dropped) the decoder thread's next send fails and it exits.
    pub fn recv(&self) -> Option<Result<PcmChunk, TtsError>> {
        self.rx.recv().ok()
    }
}

impl Iterator for PcmStream {
    type Item = Result<PcmChunk, TtsError>;

    fn next(&mut self) -> Option<Self::Item> {
        self.recv()
    }
}

/// A TTS backend. Object-safe like `dmn-llm`'s `Provider`; see the crate doc for the shape.
pub trait TtsProvider: Send + Sync {
    /// Stable name for logs and errors ("chatterbox", "mock").
    fn name(&self) -> &'static str;

    /// Start speaking `req`. The future resolves once the server answered and the WAV header
    /// was read, so a refused connection or an error status fails here, not mid-stream.
    fn synthesize<'a>(&'a self, req: SpeakRequest) -> BoxFuture<'a, Result<PcmStream, TtsError>>;

    /// Register the reference clip `wav` with the server under `name`; returns the name the
    /// server registered it as. The clip itself is the caller's to keep (the CLI copies it
    /// under the data dir and records it in `[speak].voice_ref`).
    fn enroll<'a>(&'a self, name: &'a str, wav: &'a Path) -> BoxFuture<'a, Result<String, TtsError>>;
}
