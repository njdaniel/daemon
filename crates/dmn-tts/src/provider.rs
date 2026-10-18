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
#[derive(Debug, Clone, PartialEq)]
pub struct SpeakRequest {
    pub text: String,
    pub voice: String,
    /// A language hint (`es`, `de`, ...), sent as the request's `language`; `None` sends none.
    /// chatterbox-tts-api ignores it and speaks in the language the voice was enrolled with
    /// (see [`TtsProvider::enroll`]). The translation step that picks it is #109's.
    pub language: Option<String>,
    /// The model's sampling knobs (#113), each sent only when set.
    pub tuning: Tuning,
}

impl SpeakRequest {
    pub fn new(text: impl Into<String>, voice: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            voice: voice.into(),
            language: None,
            tuning: Tuning::default(),
        }
    }

    pub fn with_language(mut self, language: impl Into<String>) -> Self {
        self.language = Some(language.into());
        self
    }

    pub fn with_tuning(mut self, tuning: Tuning) -> Self {
        self.tuning = tuning;
        self
    }
}

/// Chatterbox's sampling knobs (#113), from `[speak]`. `None` leaves the server's own default
/// (chatterbox-tts-api: temperature 0.8, cfg_weight 0.5, exaggeration 0.5); a lower
/// temperature makes the model's end-of-text noise burst rarer.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Tuning {
    /// Sampling temperature; the server takes 0.05–5.0.
    pub temperature: Option<f64>,
    /// Classifier-free guidance weight, which sets the pace; 0.0–1.0.
    pub cfg_weight: Option<f64>,
    /// Emotion intensity; 0.25–2.0.
    pub exaggeration: Option<f64>,
}

/// What [`TtsProvider::enroll`] registered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Enrolled {
    /// The name the server stored the voice under.
    pub name: String,
    /// A voice by that name was there already and has been replaced.
    pub replaced: bool,
}

/// One voice in the server's library (#109), as [`TtsProvider::voices`] lists it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Voice {
    pub name: String,
    /// Other names the server answers to for this voice.
    pub aliases: Vec<String>,
    /// The language it was enrolled with, and so the only one it speaks; `None` when the
    /// listing does not say.
    pub language: Option<String>,
}

impl Voice {
    /// Whether `name` is this voice's name or one of its aliases.
    pub fn answers_to(&self, name: &str) -> bool {
        self.name == name || self.aliases.iter().any(|a| a == name)
    }
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

    /// The server's voice library (#109: `dmn run` checks at start that the enrolled voice
    /// speaks `[speak].target`). `None` when the server has no library to list.
    fn voices<'a>(&'a self) -> BoxFuture<'a, Result<Option<Vec<Voice>>, TtsError>>;

    /// #122: whether one enrolled voice speaks every language in [`TtsProvider::languages`]
    /// (Qwen3-TTS clones the speaker, not the language), so the start-up check does not
    /// compare a voice's enrollment language with the one it is asked to speak.
    fn cross_lingual(&self) -> bool {
        false
    }

    /// #122: the language codes the engine speaks, when it has a fixed list; `None` when it
    /// does not say (chatterbox-tts-api: each voice speaks the language it was enrolled with).
    fn languages(&self) -> Option<&'static [&'static str]> {
        None
    }
}
