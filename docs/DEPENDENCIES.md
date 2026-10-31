# Dependencies

One line per crate saying why it is here. Add a line before adding a dependency.

| Crate | Used by | Why |
| --- | --- | --- |
| `clap` (derive) | `dmn` | CLI parsing for `run`, `doctor`, `devices`, `sources`, `replay`, `bench` |
| `thiserror` | all pipeline crates, `dmn-tts` | Error enums per crate; no `unwrap` in pipeline code |
| `serde` (derive) | `dmn-core`, `dmn-session`, `dmn-store`, `dmn-llm`, later every crate with a data type | Config structs, session JSON, LLM schema |
| `toml` | `dmn-core` | Parse and write `~/.config/dmn/config.toml` |
| `directories` | `dmn-core` | XDG config / data / cache / state paths without hand-rolled env logic |
| `tracing` | all crates | Structured events; the only logging API in the workspace |
| `tracing-subscriber` (json, env-filter) | `dmn-core`, `dmn-llm` (dev) | JSON lines to the log file; `DMN_LOG` filter. `dmn-llm` tests capture the log to check an ignored candidate is logged with its score |
| `serde_json` | `dmn-core`, `dmn-session` (dev), `dmn-cli`, `dmn-llm`, `dmn-store`, `dmn-vad` (dev), `dmn-stt` (dev) | Assert log lines are JSON; parse Ollama `/api/tags` in `dmn doctor`; LLM request/response bodies; JSONL session export; the fast-phase payload the session writer stores; action-log round trips in the session tests; read fixture `turns.json` / `questions.json` in the VAD and pipeline tests |
| `tempfile` (dev) | `dmn-core`, `dmn-cli`, `dmn-audio`, `dmn-store`, `dmn-context`, `dmn-tts` tests | Isolated config / log / WAV / database / source-file directories so tests never touch `~` |
| `hound` | `dmn-audio`, `dmn-tts`, `dmn-cli` | Read fixture / replay WAVs of any rate, channel count and sample format. `dmn-tts` writes its mock's sine WAV with it (it decodes the server's WAV body itself: hound takes the sample count from the `data` length, which a streamed WAV leaves at `0xFFFFFFFF`); `dmn-cli` checks an enrollment clip and writes `dmn speak --out` (#107) |
| `rubato` | `dmn-audio` | FFT resampler from the file's rate to 16 kHz; PipeWire resamples live audio itself |
| `ringbuf` | `dmn-audio` | Lock-free SPSC ring from the capture thread to the VAD thread; the RT callback only pushes. The virtual microphone (#108) uses a second one the other way round: the writer's thread pushes speech, the RT callback only pops |
| `pipewire` (0.10, official pipewire-rs; feature `v0_3_49`) | `dmn-audio` | Main loop, registry discovery and, from #5, capture streams with `target.object`; the `Rc` API needs no `unsafe` for discovery. #108 adds the `dmn-voice` playback stream and turns on the `v0_3_49` feature for `Buffer::requested` (how many frames the graph wants per cycle): a version gate on the installed libpipewire, not a new crate |
| `tokio` (rt-multi-thread, macros, time, sync) | `dmn-llm`, `dmn-cli`, `dmn-core`, `dmn-session`, `dmn-tts` | Async runtime for HTTP, SQLite access and UI events; never on the audio path. `dmn replay` and `dmn run` run the fast phase on its own thread with a current-thread runtime. `dmn-session` uses only `sync` (`watch` for the state, `broadcast` for the action log); `dmn-core`'s store writer drains that broadcast on a plain thread with `blocking_recv` |
| `tokio-util` | `dmn-llm` | `CancellationToken` (re-exported by `dmn-llm`): a newer unpinned question cancels the in-flight LLM request, and `q` in `dmn run` cancels it on the way out |
| `reqwest` (json, stream; rustls by default) | `dmn-llm` | HTTP client for Ollama, OpenAI-compatible and Anthropic endpoints; streaming bodies |
| `futures-util` | `dmn-llm`, `dmn-tts` | `BoxStream` / `BoxFuture` so `Provider` and `TtsProvider` are object-safe without a macro crate |
| `crossbeam-channel` | `dmn-vad`, `dmn-stt`, `dmn-transcript`, `dmn-detect`, `dmn-cli`, `dmn-tts` | Bounded channel from each VAD thread to its STT worker, `dmn-tts`'s PCM chunks from its decoder thread to the caller (#107), the transcript bus's per-subscriber queue, and the detector's candidate channel to the fast phase; `try_send` lets the sender count and drop instead of blocking when a consumer stalls. `dmn-cli`'s shared pipeline (`replay`, `run`) holds the bus and candidate channel ends in a struct, so it names their types |
| `whisper-rs` (=0.16.0, `tracing_backend`, `raw-api`; bundles whisper.cpp 1.8.3 via `whisper-rs-sys` 0.15.0) | `dmn-stt` | In-process whisper.cpp inference; pinned exactly because the build compiles C++ from source (SPEC § risks). Features `cuda` / `vulkan` / `openblas` forwarded from `dmn-stt`; CPU by default. `raw-api` re-exports the `whisper-rs-sys` bindings (no new crate) so `dmn-stt::gpu` can enumerate ggml's backend devices and pick the GPU explicitly (#61) |
| `reqwest` feature `blocking` | `dmn-stt`, `dmn-tts` | One-shot model download for `dmn doctor --fetch` without an async runtime; same client crate as `dmn-llm`. `dmn-tts` (#107) reads the TTS server's WAV body on its own decoder thread through the blocking client, so a slow consumer blocks the read (backpressure) and no runtime is needed to drain it. Its multipart enrollment form is built by hand rather than with reqwest's `multipart` feature, which would add `mime_guess` and `unicase` |
| `serde_json` feature `preserve_order` (uses `indexmap`, already in the tree via `reqwest`) | `dmn-llm` | Keeps the fast-phase schema's property order. Ollama builds its grammar in that order, so `question` streams before `approach`; with the default sorted map `approach` would come first |
| `rusqlite` (bundled) | `dmn-store`, `dmn-cli` (dev) | SQLite schema + migration runner for sessions, segments, questions, suggestions, (#53) context sources and chunks, and (#54) the job-description summary cache; bundled SQLite avoids distro version drift (SPEC § Decisions). `dmn-cli`'s tests hold a write lock from a second connection to prove retrieval never waits on the session writer |
| `pdf-extract` (0.12; `lopdf` underneath) | `dmn-context` | Text layer of PDF resumes and notes for `dmn sources add` (#53). Pure Rust, no C bindings, no OCR: a scanned PDF is refused as having no text. Its parser can panic on malformed files, so `dmn-context` calls it under `catch_unwind` and reports a refusal |
| `ort` (=2.0.0-rc.10; features `std`, `download-binaries`; wraps ONNX Runtime 1.22) | `dmn-vad` | Silero VAD (#62, SPEC § Decisions M2). Pinned: rc.10 is the newest release whose MSRV (1.81) fits the workspace's 1.85 (rc.13 needs 1.88). `download-binaries` makes `ort-sys`'s build script fetch a prebuilt static ONNX Runtime from pyke's CDN, checked by hash, so no system package is needed and nothing is downloaded at run time; CI's build step already has network. Only the safe API is used: no `unsafe` in `dmn-vad` |
| `sha2` | `dmn-vad`, `dmn-stt` | SHA-256 of the downloaded Silero model before ONNX Runtime parses it; since #84 also of every whisper model, on `--fetch` (while it streams) and in `dmn doctor`, because a model with a few wrong bytes loads fine and transcribes nothing. Already in the tree: `ort-sys`'s build script uses it to check the runtime it downloads |
| `ratatui` (0.29, default features: `crossterm` backend) | `dmn-cli` | The terminal UI's layout, widgets and `TestBackend` snapshot tests (#14). 0.29, not 0.30: 0.30 needs Rust 1.88 and the workspace MSRV is 1.85. `crossterm` 0.28 comes with it and is used through `ratatui::crossterm`, so the two can never disagree on versions |
| `libc` (0.2, already in the tree through `tokio`, `pipewire` and others) | `dmn-cli` | Only the `O_NONBLOCK` constant, so the push-to-talk device (#124) opens non-blocking and its reader thread can stop when push-to-talk is turned off, rather than block on a read until the next key event. No `libc` function is called, so no `unsafe` |

## Sidecar servers (run by the user, never linked)

Programs Daemon talks to over HTTP on this machine, which the user installs and starts like Ollama. Nothing of them is vendored, linked or shipped; they are not in `Cargo.lock`.

| Server | Used by | Why |
| --- | --- | --- |
| [chatterbox-tts-api](https://github.com/travisvn/chatterbox-tts-api) (server: AGPL-3.0) around Resemble AI's [Chatterbox](https://github.com/resemble-ai/chatterbox) (model and library: MIT) | `dmn-tts`, `dmn speak` (#107) | Zero-shot voice cloning from a short clip of the user, 22 languages in the multilingual model, behind an OpenAI-compatible `POST /v1/audio/speech` plus a voice library (`/v1/voices`) to enroll a clip once; runs on the CPU, so it need not share the GPU with the LLM. Rejected in #107: XTTS v2 (non-commercial CPML), F5-TTS (CC-BY-NC weights), Piper and Kokoro (no cloning), cloud TTS (breaks local-only). Its AGPL covers the server program the user runs; Daemon only sends it HTTP requests. `[speak].tts_url` must be a loopback address |
| [Qwen3-TTS](https://github.com/QwenLM/Qwen3-TTS) 1.7B Base (Alibaba, model: Apache-2.0) served by [vLLM-Omni](https://github.com/vllm-project/vllm-omni) 0.30.0 (Apache-2.0), the `vllm/vllm-omni:v0.30.0` image | `dmn-tts`, `[speak].engine = "qwen3"` (#122) | The faster voice engine: streamed speech with first audio ~0.1 s after the request against 1–2 s for Chatterbox on the same 4090, 12/12 test lines correct in zh/ru/de, and one clip speaks all ten of its languages. Chosen by measurement in #122 over CosyVoice 3 on vLLM-Omni (0.55 s, 3–4/12 correct) and Qwen3-TTS through its own `qwen-tts` package (no streaming, 1.6–3.7 s); free and local, where ElevenLabs (paid, cloud) was set aside. vLLM-Omni already serves it OpenAI-style with a voice library, so there is no wrapper: `sidecars/qwen3-tts/compose.yaml` is only the Docker setup (127.0.0.1, GPU memory per stage, `VLLM_USE_FLASHINFER_SAMPLER=0` because FlashInfer JIT-compiles with `nvcc`). NVIDIA GPU only. `[speak].tts_url` must be a loopback address |

## Developer tooling (`tools/`, Python)

Offline only: these run when a human regenerates the fixtures. Nothing here is on the audio path, ships in a binary, or is needed to build, test or run `dmn`; `tools/uv.lock` pins them and `just fixtures` is the only entry point.

| Package | Used by | Why |
| --- | --- | --- |
| `piper-tts` | `tools/fixtures/generate.py` | Local neural TTS for the two mock-interview voices; keeps the fixtures redistributable (public-domain voices) and reproducible offline |
| `onnxruntime` | `tools/fixtures/generate.py` | Imported directly to seed Piper's in-graph noise, so two runs produce byte-identical WAVs |
| `numpy` | `tools/fixtures/{generate,verify}.py` | Resampling, level measurement and WAV assembly on the shared timeline |

Python range is `>=3.11,<3.15`: the locked `onnxruntime` publishes cp311–cp314 wheels and no source distribution, so nothing installs outside it.
