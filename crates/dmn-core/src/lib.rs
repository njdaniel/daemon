//! Pipeline wiring. The only crate that knows every stage.
//!
//! Input: config plus an `AudioSource` per channel (PipeWire live, or WAV for `dmn replay` and tests).
//! Output: a running pipeline whose `Session` watch channel the CLI TUI or the desktop window subscribes to.
//! Owns: thread and task lifetimes for every stage; shutdown order is the reverse of start order.
