# Dependencies

One line per crate saying why it is here. Add a line before adding a dependency.

| Crate | Used by | Why |
| --- | --- | --- |
| `clap` (derive) | `dmn` | CLI parsing for `run`, `doctor`, `devices`, `sources`, `replay`, `bench` |
| `thiserror` | all pipeline crates | Error enums per crate; no `unwrap` in pipeline code |
| `serde` (derive) | `dmn-core`, later every crate with a data type | Config structs, session JSON, LLM schema |
| `toml` | `dmn-core` | Parse and write `~/.config/dmn/config.toml` |
| `directories` | `dmn-core` | XDG config / data / cache / state paths without hand-rolled env logic |
| `tracing` | all crates | Structured events; the only logging API in the workspace |
| `tracing-subscriber` (json, env-filter) | `dmn-core` | JSON lines to the log file; `DMN_LOG` filter |
| `serde_json` | `dmn-core` (dev), later `dmn-llm` | Assert log lines are JSON; LLM request/response bodies |
| `tempfile` (dev) | tests | Isolated config / log directories so tests never touch `~` |

Planned by the spec (add the line when the issue lands): `hound`, `ringbuf`, `rubato` (#3); `pipewire` (#4); `crossbeam-channel`, `whisper-rs` (#8); `reqwest`, `tokio`, `tokio-util` (#11); `rusqlite` (#13); `ratatui`, `crossterm` (#14).
