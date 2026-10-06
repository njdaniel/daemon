//! Speech to text via whisper.cpp (`whisper-rs`).
//!
//! Input: [`VadEvent`](dmn_vad::VadEvent)s from one source's bounded channel: closed
//! `Utterance`s and, every second, `Partial`s of the utterance still open.
//! Output: [`Segment`](dmn_transcript::Segment)s published through the callback the caller hands
//! [`SttWorker::spawn`] (in the pipeline, `dmn-transcript`'s bus): one partial per second while
//! an utterance is open, then the final that replaces them. Hallucinations and low-confidence
//! text are dropped and counted ([`filter`]). With `[stt].language` other than `en` (#77) a
//! multilingual model detects each utterance's language and its text arrives translated to
//! English; the segment carries the detected language. [`Language::Detect`] (#109) detects the
//! same way and keeps the text in the language it heard: the You lane while `[speak]` is on.
//! Owns: one worker thread and one `WhisperState` per source (inference serialised per state),
//! and the ggml model files under the models directory the caller passes in (`dmn` passes
//! `~/.cache/dmn/models/`).
//!
//! No dependency on `dmn-core`: the wiring crate converts `[stt]` config into [`SttConfig`].

// CLAUDE.md: no unwrap/expect outside tests and main.
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

pub mod filter;
pub mod gpu;
pub mod model;
pub mod transcriber;
pub mod wer;
pub mod worker;

pub use filter::{Dropped, FilterConfig, should_drop};
pub use gpu::{DeviceChoice, GpuDevice, Placement, gpu_devices};
pub use model::{Model, download_file, download_model, model_path, verify_model};
pub use transcriber::{Backend, Language, SttConfig, Transcriber, Transcript};
pub use wer::word_error_rate;
pub use worker::{Speech, SttStats, SttWorker, SttWorkerConfig};

/// Sample rate whisper expects; `dmn-audio` delivers exactly this.
pub const SAMPLE_RATE: usize = 16_000;

#[derive(Debug, thiserror::Error)]
pub enum SttError {
    #[error("unknown whisper model {0:?}; known: {known}", known = model::KNOWN_NAMES)]
    UnknownModel(String),
    #[error("model file {0} is missing; fix: dmn doctor --fetch <model>")]
    ModelMissing(String),
    #[error(
        "unknown language {0:?}; use \"en\", \"auto\" (detect and translate to English), or an ISO 639-1 code whisper knows (es, de, fr, ...)"
    )]
    UnknownLanguage(String),
    #[error(
        "whisper model {model} is English-only and cannot detect or translate; set [stt].model to small, medium or large-v3 (dmn doctor --fetch <model>), or [stt].language to \"en\""
    )]
    ModelIsEnglishOnly { model: String },
    #[error(
        "whisper model {model} was not trained to translate and would return the original language; set [stt].model to small, medium or large-v3 (dmn doctor --fetch <model>), or [stt].language to \"en\""
    )]
    ModelCannotTranslate { model: String },
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
    /// #84: the right size and the wrong bytes (seen on the dev box as single flipped bits in a
    /// 1.5 GB model, which then transcribed nothing).
    #[error(
        "{path} has sha256 {got}, not the published {expected}; fix: dmn doctor --fetch <model>"
    )]
    ChecksumMismatch {
        path: String,
        got: String,
        expected: String,
    },
    #[error("{path}: {source}")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },
}
