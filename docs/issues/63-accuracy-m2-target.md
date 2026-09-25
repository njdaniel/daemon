# #63 — detect: recall ≥ 0.9 and precision ≥ 0.9 on the fixtures, including a continuous-speech fixture

Labels: `size:M, model:opus, m2, lane:llm`

## Goal
Raise detection to the M2 target — recall ≥ 0.9 and precision ≥ 0.9 — from recall 0.97 / precision 0.83 after #16's `vad_close_ms = 500`, and prove it on a harder fixture (SPEC M2 done-criteria).
## Size / Model / Turns
M | Opus | 45   (from docs/PROCESS.md; labels must match)
## Scope
- A fourth fixture, `testdata/continuous/`, via `tools/fixtures`: two-person continuous speech, questions split at mid-sentence pauses, a candidate asking a clarifying question, filler ("so, uh, how would you…"), and a reverse-questions close; with `questions.json` / `turns.json` like the others.
- Precision work: #16's remaining false positives are fragments from pauses ("a user's history?"). Options: join a fragment to the previous Interviewer final within a short gap, detector weights in `crates/dmn-detect`, and choosing `[stt].vad` (energy vs Silero from #62) and `vad_close_ms` from measurements.
- Echo guard (#60) on, so You-lane echoes do not count.
- `eval/run.py` reports the fourth fixture and the M2 target line.
Out of scope: everything else, including later issues.
## Depends on
#60, #62
## Acceptance
- [ ] `just eval`: recall ≥ 0.9 and precision ≥ 0.9 overall on all four fixtures
- [ ] Fixture p50 question-end → Approach ≤ 2.0 s and p95 ≤ 3.5 s at 1× still hold
- [ ] Every changed default (VAD, close time, weights) is recorded with the numbers that chose it
- [ ] `just check`, `just test` and `just lint` green
## Verify
```
just eval
cargo run -p dmn-cli -- run --replay testdata/continuous/interviewer.wav testdata/continuous/candidate.wav --headless
```
