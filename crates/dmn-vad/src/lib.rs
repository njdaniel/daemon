//! Voice activity detection and utterance segmentation.
//!
//! Input: 20 ms frames drained from a `dmn-audio` ring buffer, one thread per source.
//! Output: `Utterance { source, start, end, samples }`, closed after 700 ms of silence or at 12 s.
//! Owns: the per-source VAD thread and the overrun counter the status bar reads.
