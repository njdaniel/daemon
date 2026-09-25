# #57 — dmn-context: session memory — one summary line per answered question; promises surface in Things I Should Mention

Labels: `size:M, model:opus, m2, lane:llm`

## Goal
Keep a rolling session summary: after every answered question the slow phase appends one line, prompts carry the last 8 lines, and "promised to return to" items appear in Things I Should Mention on later questions until mentioned (SPEC § Context and LLM, Session memory).
## Size / Model / Turns
M | Opus | 45   (from docs/PROCESS.md; labels must match)
## Scope
- `crates/dmn-llm`: a `summary` line and an optional `promise` in the slow-phase reply (limits in `limits.rs`: one line ≤ 160 chars).
- `crates/dmn-context`: the summariser keeps the session's lines; `FastRequest.summary` (exists, empty today) and the slow request carry the last 8.
- Promise tracking: open promises are added to `mention` on later questions and closed once a later You segment covers them (simple text match first; stated in the doc comment).
- `crates/dmn-session` / `dmn-store`: summary lines are actions and rows, so a replayed action log rebuilds them, and `dmn sessions export` includes them.
Out of scope: everything else, including later issues.
## Depends on
#55
## Acceptance
- [ ] After N answered questions on a fixture the session holds N summary lines; prompts carry at most the last 8 (asserted on MockProvider requests)
- [ ] A promise made in one answer appears in Things I Should Mention on the next question and disappears once a You segment mentions it (scripted MockProvider test)
- [ ] Summary lines survive `dmn sessions export` and replay from the action log
- [ ] `just check`, `just test` and `just lint` green
## Verify
```
cargo test -p dmn-context -p dmn-llm -p dmn-session
```
