# #16 — infra: live-call tuning of VAD thresholds and detector weights, `dmn bench --live` report

Labels: `size:M, model:opus, m1, lane:infra`

## Goal
Run against a real Meet call, tune VAD thresholds and detector weights from the log, and report end-to-end latency with `dmn bench --live`.
## Size / Model / Turns
M | Opus, interactive | —   (from docs/PROCESS.md; labels must match)
## Scope
Config defaults in `dmn-vad` and `dmn-detect`; `bench --live` (reads the session log, prints p50 / p95 question-end → Approach and CPU use). README section on headphones and echo.
Out of scope: everything else, including later issues.
## Depends on
#15
## Acceptance
- [ ] 60-minute live call in a terminal beside the call without crash or ring-buffer overrun
- [ ] p50 ≤ 2.0 s, p95 ≤ 3.5 s with `small.en` on the fixtures; CPU ≤ 1 core idle-listening
- [ ] Recall ≥ 0.85, precision ≥ 0.8 still hold after tuning (`just eval`)
- [ ] `just check`, `just test` and `just lint` green
## Verify
```
cargo run -p dmn-cli -- bench --live
just eval
```
