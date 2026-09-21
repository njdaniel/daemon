use std::path::PathBuf;

use crate::frames::FrameSink;

/// Who a stream belongs to. Attribution is a property of the source (docs/SPEC.md § Speaker
/// attribution): mic → `You`, app loopback → `Interviewer`, single-stream fallback → `Unknown`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Source {
    You,
    Interviewer,
    Unknown,
}

impl Source {
    /// The label the transcript shows.
    pub fn label(self) -> &'static str {
        match self {
            Source::You => "You",
            Source::Interviewer => "Interviewer",
            Source::Unknown => "Unknown",
        }
    }
}

impl std::fmt::Display for Source {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.label())
    }
}

#[derive(Debug, thiserror::Error)]
pub enum AudioError {
    #[error("cannot open {path}: {source}")]
    Open {
        path: PathBuf,
        #[source]
        source: hound::Error,
    },
    #[error("{path}: unsupported WAV format ({detail})")]
    Unsupported { path: PathBuf, detail: String },
    #[error("read error in {path}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: hound::Error,
    },
    #[error("resampler: {0}")]
    Resampler(String),
    #[error("source already started")]
    AlreadyStarted,
    #[error("source thread panicked")]
    ThreadPanicked,
}

/// A labelled producer of 16 kHz mono frames. `start` hands the source its ring producer and
/// returns immediately; the source does its work on a thread it owns until `stop` or end of
/// input. Errors after `start` are reported by `stop`.
pub trait AudioSource: Send {
    /// Which side of the conversation this source carries.
    fn source(&self) -> Source;

    /// Begin delivering frames into `sink`. Calling it twice is an error.
    fn start(&mut self, sink: FrameSink) -> Result<(), AudioError>;

    /// Signal the source to stop and wait for its thread. Idempotent.
    fn stop(&mut self) -> Result<(), AudioError>;

    /// True once the source has no more audio to deliver (end of file; a live source never
    /// finishes on its own).
    fn is_finished(&self) -> bool;
}
