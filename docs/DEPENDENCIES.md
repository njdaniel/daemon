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
| `serde_json` | `dmn-core` (dev), `dmn-cli`, `dmn-llm`, `dmn-vad` (dev) | Assert log lines are JSON; parse Ollama `/api/tags` in `dmn doctor`; LLM request/response bodies; read fixture `turns.json` in VAD accuracy tests |
| `tempfile` (dev) | `dmn-core`, `dmn-cli`, `dmn-audio` tests | Isolated config / log / WAV directories so tests never touch `~` |
| `hound` | `dmn-audio` | Read fixture / replay WAVs of any rate, channel count and sample format |
| `rubato` | `dmn-audio` | FFT resampler from the file's rate to 16 kHz; PipeWire resamples live audio itself |
| `ringbuf` | `dmn-audio` | Lock-free SPSC ring from the capture thread to the VAD thread; the RT callback only pushes |
| `pipewire` (0.10, official pipewire-rs) | `dmn-audio` | Main loop, registry discovery and, from #5, capture streams with `target.object`; the `Rc` API needs no `unsafe` for discovery |
| `tokio` (rt-multi-thread, macros, time, sync) | `dmn-llm`, later `dmn-core` | Async runtime for HTTP, SQLite access and UI events; never on the audio path |
| `tokio-util` | `dmn-llm` | `CancellationToken`: a newer unpinned question cancels the in-flight LLM request |
| `reqwest` (json, stream; rustls by default) | `dmn-llm` | HTTP client for Ollama, OpenAI-compatible and Anthropic endpoints; streaming bodies |
| `futures-util` | `dmn-llm` | `BoxStream` / `BoxFuture` so `Provider` is object-safe without a macro crate |
| `crossbeam-channel` | `dmn-vad`, later `dmn-stt` | Bounded channel from each VAD thread to the STT worker; `try_send` lets the drain count and drop instead of blocking when the consumer stalls |

Planned by the spec (add the line when the issue lands): `whisper-rs` (#8); `rusqlite` (#13); `ratatui`, `crossterm` (#14).
