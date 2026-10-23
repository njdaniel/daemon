//! Pipeline wiring. The only crate that knows every stage.
//!
//! Input: config plus an `AudioSource` per channel (PipeWire live, or WAV for `dmn replay` and tests).
//! Output: a running pipeline whose `Session` watch channel the CLI TUI or the desktop window subscribes to.
//! Owns: thread and task lifetimes for every stage; shutdown order is the reverse of start order;
//! and the session writer thread, which drains the session's action `broadcast` into SQLite.
//!
//! Until the full pipeline lands this crate holds what every stage shares: [`config`] (typed
//! `config.toml`), [`paths`] (XDG locations) and [`logging`] (JSON log file with size rotation),
//! plus, since #13, [`wire`] (pipeline values → session actions) and [`recorder`] (the session
//! hub + store writer that `dmn replay` and `dmn run` both start, session exports and purge).

// CLAUDE.md: no unwrap/expect outside tests and main.
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

pub mod config;
pub mod logging;
pub mod paths;
pub mod recorder;
pub mod wire;

pub use config::Config;
pub use paths::Paths;
