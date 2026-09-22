//! Speech to text via whisper.cpp (`whisper-rs`).
//!
//! Input: 16 kHz mono f32 samples of one closed utterance, plus the previous utterance's text as a
//! prompt for continuity. (From #9 these arrive as `Utterance`s over a bounded channel.)
//! Output: a [`Transcript`] per utterance: text, mean token log-probability and a low-confidence
//! flag (`avg_logprob < -1.0`, SPEC § STT worker).
//! Owns: one `WhisperState` per [`Transcriber`] (one per source, inference serialised per state)
//! and the ggml model files under the models directory the caller passes in (`dmn` passes
//! `~/.cache/dmn/models/`). The per-source worker thread and its channel arrive with issue #9.
//!
//! No dependency on `dmn-core`: the wiring crate converts `[stt]` config into [`SttConfig`].

// CLAUDE.md: no unwrap/expect outside tests and main.
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

pub mod model;
pub mod transcriber;
pub mod wer;

pub use model::{Model, download_model, model_path};
pub use transcriber::{Backend, SttConfig, Transcriber, Transcript};
pub use wer::word_error_rate;

/// Sample rate whisper expects; `dmn-audio` delivers exactly this.
pub const SAMPLE_RATE: usize = 16_000;

#[derive(Debug, thiserror::Error)]
pub enum SttError {
    #[error("unknown whisper model {0:?}; known: {known}", known = model::KNOWN_NAMES)]
    UnknownModel(String),
    #[error("model file {0} is missing; fix: dmn doctor --fetch <model>")]
    ModelMissing(String),
    #[error("cannot load whisper model {path}: {source}")]
    Load {
        path: String,
        #[source]
        source: whisper_rs::WhisperError,
    },
    #[error("whisper inference failed: {0}")]
    Inference(#[source] whisper_rs::WhisperError),
    #[error("download of {url} failed: {reason}")]
    Download { url: String, reason: String },
    #[error("downloaded {path} is {got} bytes, expected {expected}")]
    SizeMismatch {
        path: String,
        got: u64,
        expected: u64,
    },
    #[error("{path}: {source}")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },
}
