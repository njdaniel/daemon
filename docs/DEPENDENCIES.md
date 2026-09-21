# Dependencies

One line per crate saying why it is here. Add a line before adding a dependency.

| Crate | Used by | Why |
| --- | --- | --- |
| `clap` (derive) | `dmn` | CLI parsing for `run`, `doctor`, `devices`, `sources`, `replay`, `bench` |
| `thiserror` | all pipeline crates | Error enums per crate; no `unwrap` in pipeline code |

Planned by the spec (add the line when the issue lands): `serde`, `toml`, `directories`, `tracing`, `tracing-subscriber` (#2); `hound`, `ringbuf`, `rubato` (#3); `pipewire` (#4); `crossbeam-channel`, `whisper-rs` (#8); `reqwest`, `serde_json`, `tokio`, `tokio-util` (#11); `rusqlite` (#13); `ratatui`, `crossterm` (#14).
