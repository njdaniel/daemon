# #14 — ui: static ratatui layout rendered from a saved `Session` JSON fixture; snapshot tests

Labels: `size:S, model:sonnet, m1, lane:ui`

## Goal
Draw the three areas (question + focus, numbered Approach, last two transcript lines + status) for a 100 × 30 pane from a saved `Session` fixture, with snapshot tests.
## Size / Model / Turns
S | Sonnet | 30   (from docs/PROCESS.md; labels must match)
## Scope
`apps/dmn-cli/src/tui/`: one widget per area, `ratatui` + `crossterm`; `testdata/session-*.json` fixtures; snapshot tests via `ratatui`'s `TestBackend`. Density rules: no paragraph over 3 sentences, one section open at a time, nothing auto-expands.
Out of scope: everything else, including later issues.
## Depends on
#13
## Acceptance
- [ ] Layout matches the SPEC's terminal MVP description at 100 × 30 and degrades at 80 × 24
- [ ] Snapshot tests cover: listening, live question, pinned, loading (previous Approach dimmed)
- [ ] Human review point: reads in 2–5 s
- [ ] `just check`, `just test` and `just lint` green
## Verify
```
cargo test -p dmn-cli tui
```
