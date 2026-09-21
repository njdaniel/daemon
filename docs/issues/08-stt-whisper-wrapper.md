# #8 — dmn-stt: `whisper-rs` wrapper, model lookup/download, `dmn bench` real-time factor

Labels: `size:M, model:opus, m1, lane:stt`

## Goal
Wrap `whisper-rs` with the `FullParams` from docs/SPEC.md § STT worker, resolve models under `~/.cache/dmn/models/`, download with `dmn doctor --fetch <model>`, and measure real-time factor with `dmn bench`.
## Size / Model / Turns
M | Opus | 45   (from docs/PROCESS.md; labels must match)
## Scope
`crates/dmn-stt`: `Transcriber` (one `WhisperState`), model registry, feature flags `cuda` / `vulkan` / `openblas`; `apps/dmn-cli` `bench` and `doctor --fetch`. CI builds CPU-only.
Out of scope: everything else, including later issues.
## Depends on
#6, #7
## Acceptance
- [ ] `dmn bench` transcribes a fixture utterance and prints real-time factor and the backend in use
- [ ] `doctor --fetch small.en` downloads to the cache and verifies size
- [ ] Segments with `avg_logprob < -1.0` are marked low-confidence
- [ ] `just check`, `just test` and `just lint` green
## Verify
```
cargo run -p dmn-cli -- doctor --fetch small.en
cargo run -p dmn-cli -- bench
```
