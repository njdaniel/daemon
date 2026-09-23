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
| `tracing-subscriber` (json, env-filter) | `dmn-core`, `dmn-llm` (dev) | JSON lines to the log file; `DMN_LOG` filter. `dmn-llm` tests capture the log to check an ignored candidate is logged with its score |
| `serde_json` | `dmn-core` (dev), `dmn-cli`, `dmn-llm`, `dmn-vad` (dev), `dmn-stt` (dev) | Assert log lines are JSON; parse Ollama `/api/tags` in `dmn doctor`; LLM request/response bodies; read fixture `turns.json` / `questions.json` in the VAD and pipeline tests |
| `tempfile` (dev) | `dmn-core`, `dmn-cli`, `dmn-audio` tests | Isolated config / log / WAV directories so tests never touch `~` |
| `hound` | `dmn-audio` | Read fixture / replay WAVs of any rate, channel count and sample format |
| `rubato` | `dmn-audio` | FFT resampler from the file's rate to 16 kHz; PipeWire resamples live audio itself |
| `ringbuf` | `dmn-audio` | Lock-free SPSC ring from the capture thread to the VAD thread; the RT callback only pushes |
| `pipewire` (0.10, official pipewire-rs) | `dmn-audio` | Main loop, registry discovery and, from #5, capture streams with `target.object`; the `Rc` API needs no `unsafe` for discovery |
| `tokio` (rt-multi-thread, macros, time, sync) | `dmn-llm`, later `dmn-core` | Async runtime for HTTP, SQLite access and UI events; never on the audio path |
| `tokio-util` | `dmn-llm` | `CancellationToken`: a newer unpinned question cancels the in-flight LLM request |
| `reqwest` (json, stream; rustls by default) | `dmn-llm` | HTTP client for Ollama, OpenAI-compatible and Anthropic endpoints; streaming bodies |
| `futures-util` | `dmn-llm` | `BoxStream` / `BoxFuture` so `Provider` is object-safe without a macro crate |
| `crossbeam-channel` | `dmn-vad`, `dmn-stt`, `dmn-transcript`, `dmn-detect` | Bounded channel from each VAD thread to its STT worker, the transcript bus's per-subscriber queue, and the detector's candidate channel to the fast phase; `try_send` lets the sender count and drop instead of blocking when a consumer stalls |
| `whisper-rs` (=0.16.0, `tracing_backend`; bundles whisper.cpp 1.8.3 via `whisper-rs-sys` 0.15.0) | `dmn-stt` | In-process whisper.cpp inference; pinned exactly because the build compiles C++ from source (SPEC § risks). Features `cuda` / `vulkan` / `openblas` forwarded from `dmn-stt`; CPU by default |
| `reqwest` feature `blocking` | `dmn-stt` | One-shot model download for `dmn doctor --fetch` without an async runtime; same client crate as `dmn-llm` |
| `serde_json` feature `preserve_order` (uses `indexmap`, already in the tree via `reqwest`) | `dmn-llm` | Keeps the fast-phase schema's property order. Ollama builds its grammar in that order, so `question` streams before `approach`; with the default sorted map `approach` would come first |

Planned by the spec (add the line when the issue lands): `rusqlite` (#13); `ratatui`, `crossterm` (#14).

## Developer tooling (`tools/`, Python)

Offline only: these run when a human regenerates the fixtures. Nothing here is on the audio path, ships in a binary, or is needed to build, test or run `dmn`; `tools/uv.lock` pins them and `just fixtures` is the only entry point.

| Package | Used by | Why |
| --- | --- | --- |
| `piper-tts` | `tools/fixtures/generate.py` | Local neural TTS for the two mock-interview voices; keeps the fixtures redistributable (public-domain voices) and reproducible offline |
| `onnxruntime` | `tools/fixtures/generate.py` | Imported directly to seed Piper's in-graph noise, so two runs produce byte-identical WAVs |
| `numpy` | `tools/fixtures/{generate,verify}.py` | Resampling, level measurement and WAV assembly on the shared timeline |

Python range is `>=3.11,<3.15`: the locked `onnxruntime` publishes cp311–cp314 wheels and no source distribution, so nothing installs outside it.
