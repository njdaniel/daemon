# #62 — vad: Silero VAD via `ort` behind the `Vad` trait

Labels: `size:L, model:fable, m2, lane:audio`

## Goal
Add Silero VAD via `ort` (ONNX Runtime) behind the existing `Vad` trait, selectable with `[stt].vad = "silero"` (SPEC § Decisions, M2 row).
## Size / Model / Turns
L | Fable | 60   (from docs/PROCESS.md; labels must match)
## Scope
- `crates/dmn-vad`: `silero.rs` implementing `Vad` (`is_speech`, `reset`) on the 20 ms frames (Silero takes 512-sample windows at 16 kHz, so buffer accordingly); model lookup and `dmn doctor --fetch silero`, like the whisper models.
- `ort` with a DEPENDENCIES.md line. No `unsafe` outside `dmn-audio` / `dmn-stt` (CLAUDE.md): if `ort` needs any in `dmn-vad`, say where and why and stop for a decision.
- `[stt].vad = "energy"` stays the default until #63 measures both.
- The VAD already runs on its own thread (`VadWorker`); Silero's per-frame inference time is measured and must stay far inside 20 ms.
- Fixture tests: voiced-turn accuracy against `turns.json` (the #7 metric) for both VADs.
Out of scope: everything else, including later issues.
## Depends on
#7, #16
## Acceptance
- [ ] Silero's voiced-turn accuracy on the three fixtures ≥ the energy VAD's (numbers in the PR)
- [ ] Per-frame inference ≤ 2 ms on the dev box (measured and stated)
- [ ] `[stt].vad = "silero"` switches it; a missing model file is a clear `dmn doctor` failure, not a crash
- [ ] `just eval` with Silero still meets the M1 targets
- [ ] `just check`, `just test` and `just lint` green
## Verify
```
cargo test -p dmn-vad
cargo run -p dmn-cli -- doctor --fetch silero
python3 eval/run.py   # with [stt].vad = "silero" 
```
