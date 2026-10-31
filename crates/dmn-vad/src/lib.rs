//! Voice activity detection and utterance segmentation.
//!
//! Input: 20 ms frames drained from a `dmn-audio` ring buffer ([`dmn_audio::Frames`]), one
//! [`VadWorker`] thread per source.
//! Output: [`VadEvent`]s on a bounded `crossbeam-channel`: [`VadEvent::Utterance`] (closed after
//! 700 ms of trailing silence, force-split at 12 s, or flushed at end of stream),
//! [`VadEvent::Pause`] after 300 ms of silence inside an open utterance,
//! [`VadEvent::SpeechStart`] at onset, and [`VadEvent::Partial`] every second while an utterance
//! is open (the last 8 s of it), which is what `dmn-stt` re-runs to keep the transcript moving.
//! #124: [`VadWorker::spawn_gated`] puts a push-to-talk [`PttGate`] in front of the segmenter:
//! while the button is held every frame is speech, while it is up none is, and a release closes
//! the utterance at once ([`CloseReason::Released`]).
//! Owns: the per-source VAD thread, the [`Frames`](dmn_audio::Frames) consumer it drains, and the
//! dropped-event counter the status bar reads alongside the ring's overrun counter.
//!
//! Layering: this crate knows nothing about `dmn-core`'s config. [`VadConfig`] has defaults
//! from docs/SPEC.md § Transcription and question detection; `dmn-core` converts `[stt]` into it
//! when it wires the pipeline (#13).
//!
//! Shape: a [`Vad`] makes one speech / non-speech decision per frame ([`EnergyVad`], or
//! [`SileroVad`] through ONNX Runtime from #62, chosen by `[stt].vad`); a [`Segmenter`] turns
//! those decisions into utterances with frame-count timestamps (never wall clock, so replay and
//! live agree); a [`VadWorker`] runs both on its own thread. The Silero model file is fetched by
//! `dmn doctor --fetch silero` and checked by [`silero::verify_model`].

// CLAUDE.md: no unwrap/expect outside tests and main.
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

mod energy;
mod segmenter;
pub mod silero;
mod worker;

pub use energy::{EnergyVad, EnergyVadConfig, Vad, frame_dbfs};
pub use segmenter::{CloseReason, Segmenter, SegmenterConfig, Utterance, VadEvent};
pub use silero::{SileroError, SileroVad, SileroVadConfig};
pub use worker::{PttGate, PttState, VadError, VadWorker, WorkerStats, event_channel};

/// Everything a [`VadWorker`] needs. `Default` is the SPEC's numbers.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct VadConfig {
    pub energy: EnergyVadConfig,
    pub segmenter: SegmenterConfig,
}
