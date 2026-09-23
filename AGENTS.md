# Daemon (`dmn`) — Agent Guide

## Project overview

Daemon is a **Linux-native, local-first interview copilot** written in Rust. It listens to a live interview call (Google Meet, Zoom, Teams) via PipeWire, transcribes it locally with whisper.cpp, detects interviewer questions with deterministic rules, and surfaces a short answer scaffold (3–5 lines) from an LLM within a ~2 second budget. It is built for mock interviews and interviews where AI assistance is permitted: it does not join calls as a bot, type or speak on the user's behalf, or hide itself from screen sharing.

Status: **early development**. The foundation CLI works (`doctor`, `devices`, `config show`, `bench`, `replay`); the live TUI pipeline and desktop UI are under active development.

The source of truth for design is `docs/SPEC.md`; the development process contract is `docs/PROCESS.md`; dependency decisions are recorded in `docs/DEPENDENCIES.md`; `CLAUDE.md` carries the same build rules as this file.

## Technology stack

- **Rust** stable, edition 2024, minimum 1.85 (`rust-toolchain.toml` pins stable with rustfmt + clippy). One Cargo workspace (`Cargo.toml`, resolver 3), one binary named `dmn`.
- **Audio:** `pipewire` crate (official pipewire-rs 0.10), `ringbuf` SPSC rings, `hound` + `rubato` for WAV input/resampling.
- **STT:** `whisper-rs` 0.16 (whisper.cpp 1.8.3, compiled from C++ source by the build script — first builds are slow), CPU by default; `cuda` / `vulkan` / `openblas` features on `dmn-stt` and forwarded by `dmn-cli`.
- **Async:** `tokio` (HTTP, later SQLite and UI events — never on the audio path), `reqwest`, `tokio-util` CancellationToken, `crossbeam-channel` for the real-time stage channels.
- **Config/CLI:** TOML via `serde` + `toml` at XDG paths (`directories`), `clap` derive, `tracing` + `tracing-subscriber` (JSON log, `DMN_LOG` filter).
- **Python** (offline tooling only, never on the audio path): `tools/` uses `uv` (`piper-tts`, `onnxruntime`, `numpy`) to regenerate fixtures; `eval/` is the recall/precision/latency harness.
- **Planned per spec** (do not assume present): `ratatui` + `crossterm` TUI (M1–M2), Tauri 2 desktop window (M3), `rusqlite` store (#13), Silero VAD via `ort` (M2).

## Repository layout

```
apps/dmn-cli/      the `dmn` binary: doctor, devices, config, bench, replay (run/sources later)
crates/
  dmn-audio/       AudioSource trait; WavSource; PipeWire registry/discovery/capture; pw-record fallback; ring buffers
  dmn-vad/         energy VAD + utterance segmenter, one worker thread per source (Silero in M2)
  dmn-stt/         whisper-rs wrapper, per-source worker threads, partials, hallucination filter, model download
  dmn-transcript/  Segment types, transcript bus, 60 s window log
  dmn-detect/      question detector (Tier-C rules) — skeleton, lands with issue #10
  dmn-context/     resume/JD/notes ingestion + BM25 retrieval — skeleton (M2)
  dmn-llm/         Provider trait, Ollama client (NDJSON streaming), MockProvider; prompts/limits land with #12
  dmn-session/     Session state behind tokio::sync::watch — skeleton (#13)
  dmn-store/       SQLite persistence — skeleton (#13)
  dmn-core/        pipeline wiring; today holds config, XDG paths, logging shared by all stages
docs/              SPEC.md, PROCESS.md, DEPENDENCIES.md, design/, issues/ (mirrors of GitHub issues)
eval/              Python eval harness (run.py lands with issue #10)
testdata/          mock-interview fixtures (WAVs not in git; see "Testing")
tools/             offline fixture generator (uv, Piper TTS)
scripts/           agent.sh / review.sh / issues-push.sh implementing docs/PROCESS.md
```

Pipeline (per SPEC): PipeWire capture as two labelled streams (mic = You, app output = Interviewer) → VAD/segmenter → whisper workers → transcript bus → rule-based question detector → context assembler → LLM (fast phase: question/kind/focus/approach; slow phase: the rest) → Session state → UI. Every stage can be fed from WAV files, so the whole pipeline runs headless in tests and `dmn replay`.

## Build and test commands

Use `just` (every recipe is also runnable by hand):

| Command | What it runs |
| --- | --- |
| `just check` | `cargo check --workspace --all-targets` — run this after every edit |
| `just test` | `cargo test --workspace` — unit + integration tests, no PipeWire needed |
| `just lint` | `cargo clippy --workspace --all-targets -- -D warnings` and `cargo fmt --all --check` |
| `just build` | `cargo build --release -p dmn-cli` → `target/release/dmn` |
| `just probe` | `cargo test --workspace -- --ignored` — tests needing a live PipeWire server, dev machine only |
| `just replay sysdesign` | feeds fixture WAVs through the real pipeline |
| `just eval` | `python3 eval/run.py` recall/precision/latency (lands with issue #10) |
| `just fixtures-fetch` | download fixture WAVs from the `fixtures-v1` release (what CI does) |
| `just fixtures` | regenerate fixtures locally with Piper TTS (`tools/`, needs `uv`) |

System dependencies (Ubuntu 24.04 / Pop!_OS 24.04): `sudo apt install libpipewire-0.3-dev clang libclang-dev cmake just`. Runtime needs PipeWire ≥ 1.0 with `pw-record`, `pw-play`, `pw-dump` on PATH. LLM: Ollama at `http://localhost:11434`, default model `qwen3.6:35b-a3b` (always send `think: false`); the model takes ~22 GB VRAM, so whisper runs on CPU by default. GPU STT needs `-p dmn-cli --features cuda` (CUDA toolkit) or `--features vulkan` (`libvulkan-dev`).

## Code style guidelines

These are enforced rules from `CLAUDE.md` / `docs/PROCESS.md`, not preferences:

- **No `unwrap`/`expect`** outside tests and `main`; errors are `thiserror` enums, one per crate. Workspace lints set `unwrap_used`/`expect_used` to warn; test modules opt out with `#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]`.
- **The PipeWire process callback allocates nothing, locks nothing, logs nothing.** Real-time stages live on dedicated `std::thread`s; tokio is only for HTTP/UI/storage.
- **`unsafe` only in `dmn-audio` and `dmn-stt`**, and every block carries a `// SAFETY:` comment. `unsafe_code` is warn at workspace level.
- **No new dependency without a line in `docs/DEPENDENCIES.md`** saying why; add the line in the same PR.
- **Every crate's `lib.rs` doc comment states its input, output and the channel it owns.** Follow the existing format when extending a crate.
- **Never write audio to disk** unless `privacy.record_audio` is true; never store API keys or audio in the database.
- The UI renders **structured JSON fields only**; length limits are enforced in Rust (`crates/dmn-llm/src/limits.rs`, arrives with #12), never in prompts.
- `tracing` is the only logging API; the log file (`~/.local/state/dmn/dmn.log`, JSON, rotated) is the only sink during a call.
- Formatting is `rustfmt` defaults; clippy runs with `-D warnings` in CI and `just lint`.

## Testing instructions

- The default suite (`just test`, CI) uses the `testdata/` fixture WAVs and `MockProvider`, **never a microphone or live network**. CI has no PipeWire server, no Ollama, and no whisper models.
- Anything needing a live service (PipeWire, or Ollama at `localhost:11434`) or a downloaded whisper model goes behind `#[ignore]` and runs via `just probe` on a dev machine, so `just probe` needs those running.
- Fixture WAVs (~20 MB each) are not in git. Get them with `just fixtures-fetch` (release download, sha256-checked; CI caches this) or regenerate with `just fixtures`. `questions.json` / `turns.json` in each fixture directory are the eval ground truth and are checked in.
- The whisper pipeline tests (`crates/dmn-stt/tests/pipeline.rs`, ignored) need `small.en` (`dmn doctor --fetch small.en`); nothing downloads at test time in CI.
- Concurrency/pipeline tests drive stages from WAVs via `WavSource` at controlled speeds (`dmn replay --speed N` paces delivery).
- CI (`.github/workflows/ci.yml`, ubuntu-24.04): fmt check → check → clippy `-D warnings` → test → `dmn --version`; `dmn doctor` runs informational-only. The whisper.cpp C++ build and fixtures are cached.

## Development process

`docs/PROCESS.md` is the contract; `just agent`, `just review` and `just issues-push` automate it:

- Work happens as **sized GitHub issues** (S/M/L, labels `size:`, `model:`, milestone, `lane:`), one issue per change, one new crate or unfamiliar API per issue. Issue specs are mirrored in `docs/issues/`; after `just issues-push`, GitHub is the source of truth.
- One issue per run on branch `issue/<n>-<slug>`; the PR body starts with `Closes #n` and ticks each acceptance criterion with the exact verify command. **Never merge or force-push** from an agent run; a human squash-merges with title `#<n>: <issue title>`.
- If the spec and an issue disagree, **the issue wins** — and the PR says so.
- Review (`just review <pr>`) is by a different model session; every GitHub Copilot finding must be triaged VALID/FIXED/FALSE; merges require `BLOCKING: 0 findings`.
- Human checkpoints after issues #5, #10, #14, #16.
- Current state: issues #1–#9, #11 and fixture/process work have landed on `main`. Next up: #10 (detect), #12 (LLM fast phase + limits), #13 (session + store), #14–#15 (ratatui TUI), #16 (live-call tuning). `dmn run`, `dmn sources`, `eval/run.py` and `limits.rs` do not exist yet — do not reference them as working.

## Definition of done

- `just check`, `just test` and `just lint` all pass.
- The issue's acceptance criteria are met, each stated in the PR body with the command used to verify it.
- No new dependency without its `docs/DEPENDENCIES.md` line; no `unsafe` outside `dmn-audio`/`dmn-stt`; no audio on disk.

## Privacy and security considerations

- Local-first by design: audio buffers stay in memory; with `provider = "ollama"` (the default) nothing leaves the machine. Remote providers are opt-in via `[llm].provider`, and only the transcript window, retrieved chunks and summary are sent.
- API keys are read from the environment variable named in `api_key_env` at start; the key itself is never written to config, logs or the database.
- No accounts, telemetry, or update checks. `dmn sessions purge <id|all>` (planned, arrives with #13) will delete local data.
- Do not touch anything under `~/.config/dmn/` during agent runs.
