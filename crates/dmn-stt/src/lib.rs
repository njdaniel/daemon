//! Speech to text via whisper.cpp (`whisper-rs`).
//!
//! Input: `Utterance`s over a bounded channel, one worker thread and `WhisperState` per source.
//! Output: `Segment { source, start, end, text, is_final, avg_logprob }`, partials every 1 s, hallucination-filtered.
//! Owns: the whisper worker threads and the model cache under `~/.cache/dmn/models/`.
