# #56 — ui: six expandable sections (keys 1–6, one open at a time) and the STAR block

Labels: `size:M, model:opus, m2, lane:ui`

## Goal
Show the slow phase's six rows under the Approach in fixed order — Deeper Answer, Follow-up Questions, Tradeoffs, Example, Things I Should Mention, Resume Context — with keys `1`–`6`, exactly one open at a time (SPEC § UI spec, AI Assistance; Terminal MVP).
## Size / Model / Turns
M | Opus | 45   (from docs/PROCESS.md; labels must match)
## Scope
- `apps/dmn-cli/src/tui/`: a sections widget; keys `1`–`6` toggle one section (opening another closes the first); rows still loading show a subtle "…" hint; rows with no content for this question are hidden; nothing auto-expands; a timed-out row shows a small `slow` tag.
- The open section is a `dmn-session` action (pure reducer, logged), like pin and the coding-solutions switch.
- Layout: the three areas stay (question / approach + sections / transcript + status); an open section takes rows from the Approach area and still fits 100 × 30 and degrades at 80 × 24. Density rule: no paragraph over 3 sentences.
- Behavioral questions: the existing STAR cards (drawn since #14) replace the Approach; the header leaves room for the matched story path that #58 fills.
- Footer shortcuts show `1`–`6` when a section can open.
- Snapshot fixtures built by the reducer, like #14's `testdata/session-*.json`.
Out of scope: everything else, including later issues.
## Depends on
#55
## Acceptance
- [ ] Snapshots at 100 × 30 and 80 × 24: sections collapsed, each of the six open, a loading row with "…", a hidden empty row, a `slow` row
- [ ] Opening a second section closes the first (key-handling test on TestBackend)
- [ ] Nothing expands without a key press (test: slow content arriving never changes which section is open)
- [ ] Human review point: reads in 2–5 s with a section open
- [ ] `just check`, `just test` and `just lint` green
## Verify
```
cargo test -p dmn-cli tui
cargo run -p dmn-cli -- run --replay testdata/sysdesign/interviewer.wav testdata/sysdesign/candidate.wav
```
