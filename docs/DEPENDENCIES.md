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

## Developer tooling (`tools/`, Python)

Offline only: these run when a human regenerates the fixtures. Nothing here is on the audio path, ships in a binary, or is needed to build, test or run `dmn`; `tools/uv.lock` pins them and `just fixtures` is the only entry point.

| Package | Used by | Why |
| --- | --- | --- |
| `piper-tts` | `tools/fixtures/generate.py` | Local neural TTS for the two mock-interview voices; keeps the fixtures redistributable (public-domain voices) and reproducible offline |
| `onnxruntime` | `tools/fixtures/generate.py` | Imported directly to seed Piper's in-graph noise, so two runs produce byte-identical WAVs |
| `numpy` | `tools/fixtures/{generate,verify}.py` | Resampling, level measurement and WAV assembly on the shared timeline |

Python floor is 3.11: the locked `onnxruntime` publishes cp311–cp314 wheels and no source distribution.
