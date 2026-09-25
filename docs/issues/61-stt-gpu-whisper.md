# #61 — stt: GPU whisper (Vulkan or CUDA) beside the Ollama model, measured, with a clean CPU fallback

Labels: `size:L, model:fable, m2, lane:stt`

## Goal
Run whisper on the RTX 4090 through whisper-rs's `vulkan` or `cuda` feature, measure whether `small.en` fits beside Ollama's model and what it does to the final-pass time, and fall back to CPU cleanly when the GPU is unavailable or out of memory.
## Size / Model / Turns
L | Fable | 60   (from docs/PROCESS.md; labels must match)
## Scope
- `crates/dmn-stt` (features `cuda` / `vulkan` already forwarded) and `apps/dmn-cli` features; a `[stt].device = "cpu" | "gpu" | "auto"` setting (default `cpu` until measured).
- **User prerequisite:** the dev box has neither the CUDA toolkit (`nvcc`) nor `libvulkan-dev`; the user installs one before the run (CLAUDE.md, System dependencies). Try Vulkan first if it avoids the CUDA toolkit.
- VRAM: Ollama's `qwen3.6:35b-a3b` holds ~22 of 24 GB. Measure `small.en` (and `base.en`) on the GPU beside it with `nvidia-smi`; if it does not fit, record that and keep CPU the default.
- Fallback: a GPU load failure or out-of-memory → a warning, then CPU, never a crash; the status line says which device transcribes.
- Measure with `dmn bench` and `dmn run --replay … --headless` at 1× on the three fixtures, plus continuous speech (live session 6 on CPU: p50 2.19 s, speech end → final ≈ 1.0 s).
- CI stays CPU-only; GPU tests are `#[ignore]` and run by `just probe`.
Out of scope: everything else, including later issues.
## Depends on
#16
## Acceptance
- [ ] `dmn bench` reports the real-time factor on GPU and CPU; `dmn doctor` shows the GPU backend
- [ ] Fixture p50 / p95 at 1× with GPU whisper recorded in the PR; the default device is chosen from those numbers
- [ ] No GPU, or running out of VRAM, falls back to CPU with a warning (tested by forcing the failure)
- [ ] The SPEC's target-hardware open question is updated with the measurements
- [ ] `just check`, `just test` and `just lint` green
## Verify
```
cargo test -p dmn-stt
cargo run --release -p dmn-cli --features vulkan -- bench     # dev box, after installing libvulkan-dev
cargo run --release -p dmn-cli --features vulkan -- run --replay testdata/sysdesign/interviewer.wav testdata/sysdesign/candidate.wav --headless
```
