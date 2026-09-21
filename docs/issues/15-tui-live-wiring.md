# #15 — ui: live wiring, keys `p r m q`, status line, `dmn run --replay`

Labels: `size:M, model:opus, m1, lane:ui`

## Goal
Wire the TUI to the `Session` watch channel so `dmn run` updates live, handle `p` pin, `r` regenerate, `m` mute mic, `q` quit, show mic / system / model status dots, and drive it from fixtures with `dmn run --replay`.
## Size / Model / Turns
M | Opus | 45   (from docs/PROCESS.md; labels must match)
## Scope
`apps/dmn-cli` `run` subcommand and key handling; status line from pipeline health; `--replay <interviewer.wav> <candidate.wav>` at 1×.
Out of scope: everything else, including later issues.
## Depends on
#14
## Acceptance
- [ ] `dmn run --replay` shows the question and Approach within 2.0 s p50 of the scripted question end
- [ ] Pin blocks replacement and shows a "1 new" badge; regenerate uses temperature 0.7
- [ ] Errors appear only as status dots
- [ ] `just check`, `just test` and `just lint` green
## Verify
```
cargo run -p dmn-cli -- run --replay testdata/sysdesign/interviewer.wav testdata/sysdesign/candidate.wav
```
