//! Pipeline wiring. The only crate that knows every stage.
//!
//! Input: config plus an `AudioSource` per channel (PipeWire live, or WAV for `dmn replay` and tests).
//! Output: a running pipeline whose `Session` watch channel the CLI TUI or the desktop window subscribes to.
//! Owns: thread and task lifetimes for every stage; shutdown order is the reverse of start order.
//!
//! Until the pipeline lands this crate holds what every stage shares: [`config`] (typed
//! `config.toml`), [`paths`] (XDG locations) and [`logging`] (JSON log file with size rotation).

// CLAUDE.md: no unwrap/expect outside tests and main.
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

pub mod config;
pub mod logging;
pub mod paths;

pub use config::Config;
pub use paths::Paths;
