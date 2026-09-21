//! Audio capture.
//!
//! Input: a PipeWire node (mic `Audio/Source` or an app's `Stream/Output/Audio`) or a WAV file.
//! Output: 16 kHz mono f32 samples, one labelled stream per source (You / Interviewer).
//! Owns: the PipeWire main-loop thread and one lock-free `ringbuf` producer per capture stream.
//! The process callback allocates nothing, locks nothing, logs nothing.
//!
//! Shape (issue #3): an [`AudioSource`] is started with a [`FrameSink`] and pushes 20 ms frames
//! ([`FRAME_LEN`] samples at [`SAMPLE_RATE`]) into an SPSC ring; the consumer drains them through
//! [`Frames`] on its own thread. [`WavSource`] is the file-backed implementation every later
//! stage is tested against; `PipeWireSource` arrives with issues #4 and #5.

// CLAUDE.md: no unwrap/expect outside tests and main.
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

mod frames;
mod source;
mod wav;

pub use frames::{FrameSink, Frames, frame_channel};
pub use source::{AudioError, AudioSource, Source};
pub use wav::{Speed, WavSource};

/// Sample rate every source delivers, in Hz.
pub const SAMPLE_RATE: u32 = 16_000;
/// Frame length in milliseconds.
pub const FRAME_MS: u32 = 20;
/// Samples per frame: 16 kHz × 20 ms.
pub const FRAME_LEN: usize = (SAMPLE_RATE * FRAME_MS / 1000) as usize;
/// Ring capacity handed to a source by default: 2 s of audio, so an overrun means the consumer
/// stalled for 2 s, not that it was briefly late.
pub const DEFAULT_CAPACITY_FRAMES: usize = 100;
