//! Session state.
//!
//! Input: transcript segments, candidates, suggestions and user actions (pin, regenerate, mute, toggle).
//! Output: a single `Session` struct behind `tokio::sync::watch`, reduced deterministically; emits coalesced to <= 30/s.
//! Owns: the `watch` channel the TUI (M1) and the Tauri bridge (M3) mirror.
