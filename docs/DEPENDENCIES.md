# Dependencies

One line per crate saying why it is here. Add a line before adding a dependency.

| Crate | Used by | Why |
| --- | --- | --- |
| `clap` (derive) | `dmn` | CLI parsing for `run`, `doctor`, `devices`, `sources`, `replay`, `bench` |
| `thiserror` | all pipeline crates | Error enums per crate; no `unwrap` in pipeline code |
| `serde` (derive) | `dmn-core`, `dmn-session`, `dmn-store`, `dmn-llm`, later every crate with a data type | Config structs, session JSON, LLM schema |
| `toml` | `dmn-core` | Parse and write `~/.config/dmn/config.toml` |
| `directories` | `dmn-core` | XDG config / data / cache / state paths without hand-rolled env logic |
| `tracing` | all crates | Structured events; the only logging API in the workspace |
| `tracing-subscriber` (json, env-filter) | `dmn-core`, `dmn-llm` (dev) | JSON lines to the log file; `DMN_LOG` filter. `dmn-llm` tests capture the log to check an ignored candidate is logged with its score |
| `serde_json` | `dmn-core`, `dmn-session` (dev), `dmn-cli`, `dmn-llm`, `dmn-store`, `dmn-vad` (dev), `dmn-stt` (dev) | Assert log lines are JSON; parse Ollama `/api/tags` in `dmn doctor`; LLM request/response bodies; JSONL session export; the fast-phase payload the session writer stores; action-log round trips in the session tests; read fixture `turns.json` / `questions.json` in the VAD and pipeline tests |
| `tempfile` (dev) | `dmn-core`, `dmn-cli`, `dmn-audio`, `dmn-store`, `dmn-context` tests | Isolated config / log / WAV / database / source-file directories so tests never touch `~` |
| `hound` | `dmn-audio` | Read fixture / replay WAVs of any rate, channel count and sample format |
| `rubato` | `dmn-audio` | FFT resampler from the file's rate to 16 kHz; PipeWire resamples live audio itself |
| `ringbuf` | `dmn-audio` | Lock-free SPSC ring from the capture thread to the VAD thread; the RT callback only pushes |
| `pipewire` (0.10, official pipewire-rs) | `dmn-audio` | Main loop, registry discovery and, from #5, capture streams with `target.object`; the `Rc` API needs no `unsafe` for discovery |
| `tokio` (rt-multi-thread, macros, time, sync) | `dmn-llm`, `dmn-cli`, `dmn-core`, `dmn-session` | Async runtime for HTTP, SQLite access and UI events; never on the audio path. `dmn replay` and `dmn run` run the fast phase on its own thread with a current-thread runtime. `dmn-session` uses only `sync` (`watch` for the state, `broadcast` for the action log); `dmn-core`'s store writer drains that broadcast on a plain thread with `blocking_recv` |
| `tokio-util` | `dmn-llm` | `CancellationToken` (re-exported by `dmn-llm`): a newer unpinned question cancels the in-flight LLM request, and `q` in `dmn run` cancels it on the way out |
| `reqwest` (json, stream; rustls by default) | `dmn-llm` | HTTP client for Ollama, OpenAI-compatible and Anthropic endpoints; streaming bodies |
| `futures-util` | `dmn-llm` | `BoxStream` / `BoxFuture` so `Provider` is object-safe without a macro crate |
| `crossbeam-channel` | `dmn-vad`, `dmn-stt`, `dmn-transcript`, `dmn-detect`, `dmn-cli` | Bounded channel from each VAD thread to its STT worker, the transcript bus's per-subscriber queue, and the detector's candidate channel to the fast phase; `try_send` lets the sender count and drop instead of blocking when a consumer stalls. `dmn-cli`'s shared pipeline (`replay`, `run`) holds the bus and candidate channel ends in a struct, so it names their types |
| `whisper-rs` (=0.16.0, `tracing_backend`, `raw-api`; bundles whisper.cpp 1.8.3 via `whisper-rs-sys` 0.15.0) | `dmn-stt` | In-process whisper.cpp inference; pinned exactly because the build compiles C++ from source (SPEC § risks). Features `cuda` / `vulkan` / `openblas` forwarded from `dmn-stt`; CPU by default. `raw-api` re-exports the `whisper-rs-sys` bindings (no new crate) so `dmn-stt::gpu` can enumerate ggml's backend devices and pick the GPU explicitly (#61) |
| `reqwest` feature `blocking` | `dmn-stt` | One-shot model download for `dmn doctor --fetch` without an async runtime; same client crate as `dmn-llm` |
| `serde_json` feature `preserve_order` (uses `indexmap`, already in the tree via `reqwest`) | `dmn-llm` | Keeps the fast-phase schema's property order. Ollama builds its grammar in that order, so `question` streams before `approach`; with the default sorted map `approach` would come first |
| `rusqlite` (bundled) | `dmn-store`, `dmn-cli` (dev) | SQLite schema + migration runner for sessions, segments, questions, suggestions, (#53) context sources and chunks, and (#54) the job-description summary cache; bundled SQLite avoids distro version drift (SPEC § Decisions). `dmn-cli`'s tests hold a write lock from a second connection to prove retrieval never waits on the session writer |
| `pdf-extract` (0.12; `lopdf` underneath) | `dmn-context` | Text layer of PDF resumes and notes for `dmn sources add` (#53). Pure Rust, no C bindings, no OCR: a scanned PDF is refused as having no text. Its parser can panic on malformed files, so `dmn-context` calls it under `catch_unwind` and reports a refusal |
| `ort` (=2.0.0-rc.10; features `std`, `download-binaries`; wraps ONNX Runtime 1.22) | `dmn-vad` | Silero VAD (#62, SPEC § Decisions M2). Pinned: rc.10 is the newest release whose MSRV (1.81) fits the workspace's 1.85 (rc.13 needs 1.88). `download-binaries` makes `ort-sys`'s build script fetch a prebuilt static ONNX Runtime from pyke's CDN, checked by hash, so no system package is needed and nothing is downloaded at run time; CI's build step already has network. Only the safe API is used: no `unsafe` in `dmn-vad` |
| `sha2` | `dmn-vad` | SHA-256 of the downloaded Silero model before ONNX Runtime parses it. Already in the tree: `ort-sys`'s build script uses it to check the runtime it downloads |
| `ratatui` (0.29, default features: `crossterm` backend) | `dmn-cli` | The terminal UI's layout, widgets and `TestBackend` snapshot tests (#14). 0.29, not 0.30: 0.30 needs Rust 1.88 and the workspace MSRV is 1.85. `crossterm` 0.28 comes with it and is used through `ratatui::crossterm`, so the two can never disagree on versions |

## Developer tooling (`tools/`, Python)

Offline only: these run when a human regenerates the fixtures. Nothing here is on the audio path, ships in a binary, or is needed to build, test or run `dmn`; `tools/uv.lock` pins them and `just fixtures` is the only entry point.

| Package | Used by | Why |
| --- | --- | --- |
| `piper-tts` | `tools/fixtures/generate.py` | Local neural TTS for the two mock-interview voices; keeps the fixtures redistributable (public-domain voices) and reproducible offline |
| `onnxruntime` | `tools/fixtures/generate.py` | Imported directly to seed Piper's in-graph noise, so two runs produce byte-identical WAVs |
| `numpy` | `tools/fixtures/{generate,verify}.py` | Resampling, level measurement and WAV assembly on the shared timeline |

Python range is `>=3.11,<3.15`: the locked `onnxruntime` publishes cp311–cp314 wheels and no source distribution, so nothing installs outside it.
