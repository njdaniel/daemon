# #12 — dmn-llm: fast-phase prompt + schema + `limits.rs`; replay prints question + approach JSON

Labels: `size:M, model:opus, m1, lane:llm`

## Goal
Build the fast-phase prompt per `kind`, request the schema in docs/SPEC.md § Prompt contract, parse into `Suggestion`, and enforce field limits in Rust.
## Size / Model / Turns
M | Opus | 45   (from docs/PROCESS.md; labels must match)
## Scope
`crates/dmn-llm`: `prompts/`, `schema.rs`, `limits.rs` (approach 3–5 × ≤ 90 chars, focus ≤ 120, lists ≤ 5, deeper ≤ 4 × ≤ 240; truncate at sentence boundary and log); fast-phase timeout 4 s. Parse the streamed JSON incrementally so `question` and each `approach` line are published to the session as they arrive (deviation from SPEC § Threading rules, which says suggestions arrive as whole objects: on the dev 4090 with qwen3.6:27b the question is ready at 1.3 s but the whole object only at 3.0 s, measured 2026-09-21). Replay prints the JSON for each candidate.
Out of scope: everything else, including later issues.
## Depends on
#10, #11
## Acceptance
- [ ] `question` reaches the session before the approach finishes streaming; approach lines appear one by one
- [ ] Over-long model output is truncated and flagged, never shown as a wall
- [ ] `is_question=false` or `kind=small_talk` changes nothing on screen and is logged with the score
- [ ] Clarification follow-ups attach to the current question instead of replacing it
- [ ] `just check`, `just test` and `just lint` green
## Verify
```
cargo run -p dmn-cli -- replay testdata/sysdesign/interviewer.wav testdata/sysdesign/candidate.wav --speed 4   # prints question + approach JSON
```
