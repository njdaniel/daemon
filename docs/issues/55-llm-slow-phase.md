# #55 — dmn-llm: slow phase fills deeper, follow-ups, tradeoffs, example, mention and resume context within 20 s

Labels: `size:M, model:opus, m2, lane:llm`

## Goal
After the fast phase answers a candidate, run a second request that returns the rest of the SPEC § Prompt contract schema, with the 20 s timeout and Rust-side limits (SPEC § Context and LLM).
## Size / Model / Turns
M | Opus | 45   (from docs/PROCESS.md; labels must match)
## Scope
- `crates/dmn-llm`: `slow.rs` beside `fast.rs`; slow-phase prompt sections in `prompts/`; the schema for `deeper`, `followups`, `tradeoffs`, `example`, `mention`, `resume_context` (the fields already exist on `Suggestion` in `schema.rs`); limits in `limits.rs` (every list ≤ 5, `deeper` ≤ 4 × ≤ 240 chars).
- Coding questions honour `[ui].coding_solutions` (#50): `hints` → insight, data structure, edge cases, complexity, no complete code; `full` → the same plus a `solution` field (code, ≤ ~40 lines, capped in `limits.rs`), still collapsed until opened.
- `resume_context` comes from the chunks the request carries (#54), cited by source; the model never invents employers or projects.
- Scheduling in `apps/dmn-cli/src/suggest.rs` / `dmn-core`: the slow phase starts when the fast phase finishes for the same candidate, never delays the next fast phase, and is cancelled with it when a newer unpinned question supersedes it. `dmn-session` gets actions for each slow field and a per-section status (loading / done / failed / empty), replayable from the session log; `dmn-store` stores them.
- On timeout the previous section content stays and the section is marked `slow` (SPEC).
- `dmn replay` prints the slow fields per candidate (`--json` emits them).
Out of scope: everything else, including later issues.
## Depends on
#50, #54
## Acceptance
- [ ] All six sections' fields fill within 20 s for every answered question on the three fixtures (`dmn replay --json` timings; MockProvider in unit tests)
- [ ] Over-long slow output is truncated at a sentence boundary and flagged, never shown as a wall (tests in `limits.rs`)
- [ ] A coding question in `hints` mode never carries complete code; in `full` mode it carries `solution` ≤ 40 lines
- [ ] A superseded question's slow phase is cancelled, and a running slow phase never delays the next fast phase (test with a delayed MockProvider)
- [ ] Fixture p50 question-end → Approach still ≤ 2.0 s at 1×
- [ ] `just check`, `just test` and `just lint` green
## Verify
```
cargo test -p dmn-llm -p dmn-session
cargo run -p dmn-cli -- replay testdata/sysdesign/interviewer.wav testdata/sysdesign/candidate.wav --speed 4   # prints deeper / followups / tradeoffs per question
```
