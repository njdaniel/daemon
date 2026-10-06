//! The provider-agnostic request, chunk, stream and error types, and the [`TtsProvider`] trait.

use std::path::Path;

use futures_util::future::BoxFuture;

/// Everything a synthesis or enrollment can fail with.
#[derive(Debug, thiserror::Error)]
pub enum TtsError {
    /// Nothing answered at the server's address (connection refused, DNS, timeout).
    #[error("the TTS server at {url} is not answering: {detail}")]
    Unreachable { url: String, detail: String },
    /// The server answered with a non-2xx status; `message` is its own error text.
    #[error("the TTS server answered HTTP {status}: {message}")]
    HttpStatus { status: u16, message: String },
    /// The server's voice library has no voice by this name, and it would have spoken in its
    /// default voice instead: refused, so nobody is ever given someone else's voice.
    #[error("the TTS server has no voice named {voice:?} (it has: {known}); enroll the clip again")]
    UnknownVoice { voice: String, known: String },
    /// The body is not a WAV this crate decodes, or ended early.
    #[error("cannot decode the synthesized WAV: {detail}")]
    Decode { detail: String },
    /// The connection failed while the audio was arriving.
    #[error("the TTS server's answer broke off: {detail}")]
    BodyRead { detail: String },
    #[error("cannot read voice reference {path}: {detail}")]
    VoiceRead { path: String, detail: String },
    /// A request this crate refuses to send (a voice name the server cannot store, say).
    #[error("{0}")]
    Request(String),
    #[error("TTS worker failed: {0}")]
    Worker(String),
}

/// One synthesis job. `voice` is the name the server's voice library knows the enrolled clip
/// by (the CLI derives it from `[speak].voice_ref`'s file name).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpeakRequest {
    pub text: String,
    pub voice: String,
    /// A language hint (`es`, `de`, ...), sent as the request's `language`; `None` sends none.
    /// chatterbox-tts-api ignores it and speaks in the language the voice was enrolled with
    /// (see [`TtsProvider::enroll`]). The translation step that picks it is #109's.
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

/// What [`TtsProvider::enroll`] registered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Enrolled {
    /// The name the server stored the voice under.
    pub name: String,
    /// A voice by that name was there already and has been replaced.
    pub replaced: bool,
}

/// One decoded piece of the synthesized audio: mono f32 samples at `sample_rate`.
#[derive(Debug, Clone, PartialEq)]
pub struct PcmChunk {
    pub samples: Vec<f32>,
    pub sample_rate: u32,
}

/// The synthesized audio as it arrives: the receiving end of the bounded crossbeam channel
/// this crate owns, fed by the decoder thread. `None` from [`recv`](Self::recv) ends the audio;
/// a failure mid-body arrives as one `Err` item and then the end. Dropping it stops the
/// download at the decoder's next chunk.
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

    /// The next chunk, blocking until one arrives or the audio ends.
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
    /// was read, so a refused connection, an error status, an unknown voice or a body that is
    /// not a WAV fails here, not mid-stream.
    fn synthesize<'a>(&'a self, req: SpeakRequest) -> BoxFuture<'a, Result<PcmStream, TtsError>>;

    /// Register the reference clip `wav` with the server under `name`, replacing a voice of
    /// that name. `language` is the language the voice will speak (`None`: the server's
    /// default, English). The clip itself is the caller's to keep: the CLI copies it under the
    /// data dir and records it in `[speak].voice_ref`.
    fn enroll<'a>(
        &'a self,
        name: &'a str,
        wav: &'a Path,
        language: Option<&'a str>,
    ) -> BoxFuture<'a, Result<Enrolled, TtsError>>;
}
