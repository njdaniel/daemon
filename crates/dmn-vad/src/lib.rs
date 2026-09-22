//! Voice activity detection and utterance segmentation.
//!
//! Input: 20 ms frames drained from a `dmn-audio` ring buffer ([`dmn_audio::Frames`]), one
//! [`VadWorker`] thread per source.
//! Output: [`VadEvent`]s on a bounded `crossbeam-channel`: [`VadEvent::Utterance`] (closed after
//! 700 ms of trailing silence, force-split at 12 s, or flushed at end of stream),
//! [`VadEvent::Pause`] after 300 ms of silence inside an open utterance, and
//! [`VadEvent::SpeechStart`] at onset.
//! Owns: the per-source VAD thread, the [`Frames`](dmn_audio::Frames) consumer it drains, and the
//! dropped-event counter the status bar reads alongside the ring's overrun counter.
//!
//! Layering: this crate knows nothing about `dmn-core`'s config. [`VadConfig`] has defaults
//! from docs/SPEC.md § Transcription and question detection; `dmn-core` converts `[stt]` into it
//! when it wires the pipeline (#13).
//!
//! Shape: a [`Vad`] makes one speech / non-speech decision per frame ([`EnergyVad`] now, Silero
//! in M2 behind the same trait); a [`Segmenter`] turns those decisions into utterances with
//! frame-count timestamps (never wall clock, so replay and live agree); a [`VadWorker`] runs both
//! on its own thread.

// CLAUDE.md: no unwrap/expect outside tests and main.
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

mod energy;
mod segmenter;
mod worker;

pub use energy::{EnergyVad, EnergyVadConfig, Vad, frame_dbfs};
pub use segmenter::{CloseReason, Segmenter, SegmenterConfig, Utterance, VadEvent};
pub use worker::{VadError, VadWorker, WorkerStats, event_channel};

/// Everything a [`VadWorker`] needs. `Default` is the SPEC's numbers.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct VadConfig {
    pub energy: EnergyVadConfig,
    pub segmenter: SegmenterConfig,
}
