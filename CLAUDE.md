# Daemon (`dmn`)
Rust Linux interview copilot. M1–M2 is a ratatui TUI; M3 adds a Tauri 2 window. Spec: docs/SPEC.md. Process: docs/PROCESS.md (sizes, budgets, kickoff prompt, review, merge rules). Design: docs/design/. Issues: GitHub, mirrored in docs/issues/.

## Commands (use `just`; every recipe is also runnable by hand)
- Check fast: `just check` (`cargo check --workspace --all-targets`) — run this after every edit
- Test: `just test` (`cargo test --workspace`)
- Lint: `just lint` (`cargo clippy --workspace --all-targets -- -D warnings`, `cargo fmt --all --check`)
- Build: `just build` (`cargo build --release -p dmn-cli`, plus `--features vulkan` when libvulkan-dev and glslc are installed; `just build cpu|gpu` forces one; M3 adds `cargo tauri build`)
- Replay a fixture: `cargo run -p dmn-cli -- replay testdata/sysdesign/interviewer.wav testdata/sysdesign/candidate.wav`
- Eval: `just eval` (python3 eval/run.py; prints recall/precision/latency)
- Process: `just agent <n|next>` runs an issue headlessly on branch `issue/<n>-<slug>` and opens a PR; `just review <pr>` reviews it; `just issues-push` mirrors docs/issues/ to GitHub

## System dependencies (Ubuntu 24.04 / Pop!_OS 24.04)
- Build: `sudo apt install libpipewire-0.3-dev clang libclang-dev cmake just` (cmake already present on the dev box)
- Runtime: PipeWire ≥ 1.0 running (dev box: 1.5.85, Wayland, COSMIC); `pw-record`, `pw-play`, `pw-dump` on PATH
- LLM: Ollama at http://localhost:11434. Default model `qwen3.6:35b-a3b` (MoE, ~3B active): fast phase in 0.95 s, first Approach line at 0.73 s, 172 tok/s on the 4090, measured 2026-09-21. `qwen3.6:27b` (dense) is 3× slower and misses the 2 s budget. `llama3.1:8b` (the spec baseline) matches the a3b speed but misclassified a system-design question as behavioral and gave only 3 generic lines; not recommended. Always send `think: false`. The a3b model takes 22 GB of the 24 GB VRAM, so run whisper on CPU or check `nvidia-smi` before enabling GPU STT.
- GPU (optional): RTX 4090 on the dev box; build `-p dmn-stt --features cuda` needs the CUDA toolkit (`nvcc`), not yet installed; `--features vulkan` needs `libvulkan-dev`
- M3 only: `libwebkit2gtk-4.1-dev libgtk-3-dev`

## Rules
- The PipeWire process callback allocates nothing, locks nothing, logs nothing.
- No `unwrap`/`expect` outside tests and `main`; errors are `thiserror` enums per crate.
- Never write audio to disk unless `privacy.record_audio` is true.
- The UI renders JSON fields only; enforce length limits in `crates/dmn-llm/src/limits.rs`, not in prompts.
- No new dependencies without a line in docs/DEPENDENCIES.md saying why; no `unsafe` outside `dmn-audio` and `dmn-stt`, and every `unsafe` block has a `// SAFETY:` comment.
- Every crate's `lib.rs` doc comment states its input, output and the channel it owns.
- Tests use testdata WAVs, never a microphone; CI has no PipeWire. Anything that needs PipeWire is behind `#[ignore]` and run by `just probe` on a dev machine.
- Keep the TUI's three-area layout (question / approach / transcript+status); the M3 window follows docs/design/.
- One issue per run, on branch `issue/<n>-<slug>`; PR body starts with `Closes #n` and ticks each acceptance criterion with its verify command. Never merge or force-push. If the spec and the issue disagree, the issue wins, and say so in the PR.

## Done means
- `just check`, `just test` and `just lint` pass; the issue's acceptance criteria are met and stated in the PR body.
