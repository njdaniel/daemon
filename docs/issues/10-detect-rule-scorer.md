# #10 — dmn-detect: rule scorer; `eval/run.py` recall/precision on fixtures

Labels: `size:M, model:opus, m1, lane:llm`

## Goal
Score every final Interviewer segment with the cue table in docs/SPEC.md § Question detector, emit candidates at score ≥ 2, and report recall / precision against each fixture's `questions.json`.
## Size / Model / Turns
M | Opus | 45   (from docs/PROCESS.md; labels must match)
## Scope
`crates/dmn-detect`: cue matchers, weights per `[detect].sensitivity`, pin/queue logic; `eval/run.py` (Python, stdlib only). Replay prints candidates with scores.
Out of scope: everything else, including later issues.
## Depends on
#9
## Acceptance
- [ ] Recall ≥ 0.85 and precision ≥ 0.8 on the three fixtures
- [ ] Detection latency from final segment to candidate ≤ 100 ms
- [ ] Every cue has a unit test with a positive and a negative example
- [ ] `just check`, `just test` and `just lint` green
## Verify
```
cargo test -p dmn-detect
just eval
```
