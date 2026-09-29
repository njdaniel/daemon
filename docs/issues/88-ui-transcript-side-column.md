# #88 — ui: transcript in a right-hand column on wide panes; stacked layout below 120 columns

Labels: `size:S, model:sonnet, m2, lane:ui`

Proposed 2026-09-29, from running a mock interview with assistance on. In today's stacked layout the transcript is the last two lines under the Approach, one row each, cut at the front with `…`: too little to follow the conversation while the Approach is on screen. Decisions taken with the user: the transcript moves to a right-hand column of about 40 % of the width; the question and Approach stay on the left, where they are read first; and a pane narrower than about 120 columns keeps today's stacked layout, so the 100 × 30 and 80 × 24 panes the TUI is sized for do not change.

## Goal
On a pane at least 120 columns wide, the TUI shows the transcript as a right-hand column, about 40 % of the width and the full height above the status line, with the question, the Approach and the six sections on the left. Narrower panes keep today's stacked layout.

## Size / Model / Turns
S | Sonnet | 30

## Scope
`apps/dmn-cli/src/tui/`:

- `mod.rs` `render()`: when `area.width >= SIDE_MIN_COLS` (120), split `[left, right]` horizontally above the status line. The left column gets the question block, the Approach and the sections exactly as today; the right one gets the transcript; the status line stays full-width at the bottom.
- `status.rs`: the side column reuses #79's `wrapped()` (every line of the session's 60 s window, wrapped under its speaker label, newest at the bottom, the oldest rows dropped first) instead of the two-line strip.
- The assistance-off screen (#79) is unchanged: the transcript already takes the whole screen.

Out of scope: a key or config option to switch layouts, a left-hand column, the M3 window.

## Depends on
#79 (merged)

## Acceptance
- [ ] At ≥ 120 columns: question + Approach + sections on the left, the transcript column on the right (about 40 %), status line full-width; snapshot tests at 140 × 30 for the live, pinned, sections-open and behavioral fixtures
- [ ] Below 120 columns the screen is byte-identical to today: every existing 100 × 30 and 80 × 24 snapshot unchanged
- [ ] The side column wraps lines and keeps the newest words at the bottom, including an utterance taller than the column
- [ ] docs/SPEC.md § UI, Terminal MVP describes both layouts and the threshold. CLAUDE.md's three-area rule still holds (the same three areas, side by side); SPEC's copy of it is updated only if its wording changes
- [ ] `just check`, `just test` and `just lint` green

## Verify
```bash
cargo test -p dmn-cli tui
# in a terminal at least 140 columns wide (side column), then one of 100 columns (stacked):
dmn run --replay testdata/sysdesign/interviewer.wav testdata/sysdesign/candidate.wav
```
