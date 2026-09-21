//! Audio capture.
//!
//! Input: a PipeWire node (mic `Audio/Source` or an app's `Stream/Output/Audio`) or a WAV file.
//! Output: 16 kHz mono f32 samples, one labelled stream per source (You / Interviewer).
//! Owns: the PipeWire main-loop thread and one lock-free `ringbuf` producer per capture stream.
//! The process callback allocates nothing, locks nothing, logs nothing.
