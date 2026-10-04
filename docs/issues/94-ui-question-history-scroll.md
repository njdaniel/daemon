# #94 — ui: step back through earlier questions and scroll an open section that does not fit

Labels: `size:M, model:opus, m2, lane:ui`

Proposed 2026-10-04, from a live mock interview (session 24). The user's words: "I want to be able to navigate past questions and scroll for long in-depth answers". Found against `main` (978d13c):

- The session keeps one question. `crates/dmn-session/src/state.rs` `reduce()` replaces `Session::live` when `Action::Question` arrives unpinned; the earlier question, its Approach and its sections leave the state. They are in the store (`questions`, `suggestions`) and nothing on screen reads them back.
- An open section's body is cut. `apps/dmn-cli/src/tui/sections.rs` `render()` truncates the body to the rows it has and ends it with `…`; a Deeper Answer of four 240-character items, a 12-line Example or a 40-line Solution cannot be read to the end on a 30-row pane.
- `apps/dmn-cli/src/run.rs` `key()` binds `p r m s w 1`–`6 q`, Esc and Ctrl+Shift+A. No arrow key, PgUp/PgDn or mouse event is handled.

Choices the request left open are marked **Decided**; say so on the issue if one should go the other way.

## Goal
During a call the user can go back to any question shown earlier in the session, with its Approach and sections as they were, and can scroll the open section when it is taller than the screen.

## Size / Model / Turns
M | Opus | 60

## Scope
`crates/dmn-session` (state, actions, reducer), `apps/dmn-cli/src/run.rs` (keys), `apps/dmn-cli/src/tui/` (question block, sections, status line), docs/SPEC.md § UI, Terminal MVP.

History:
- The session keeps every question it has shown, in order, with its focus, Approach, STAR cards, clarifications and sections as they stood when it was replaced. In memory only; the store does not change.
- **Decided**: `←` shows the previous question and `→` the next. `→` on the newest returns to live.
- **Decided**: while an earlier question is on screen it holds the screen the way a pin does: a new question is answered in the background, the status line counts it (`2 new`), and it appears when the user returns to live. The question block's title says where the user is (`QUESTION 2 of 5`).
- **Decided**: an earlier question is read-only. `r` and `w` act on the live question only and do nothing while browsing; `1`–`6` open its sections. A section whose slow phase was cancelled when the next question arrived shows what had arrived and is not requested again.

Scrolling:
- **Decided**: `↑` / `↓` scroll the open section by one row, PgUp / PgDn by the rows visible. The offset resets when the section closes or the question changes.
- The `…` at the cut becomes a position hint (`↓ 6 more`, and `↑ 3` once scrolled), so a body that fits looks exactly as today.

Out of scope: mouse input (#95), scrolling the transcript, browsing earlier sessions, longer answers (the limits in `crates/dmn-llm/src/limits.rs` stay), re-running the model for an earlier question, the M3 window.

## Depends on
#56, #88 (merged)

## Acceptance
- [ ] After three questions, `←` twice shows the first with its own focus, Approach and sections; `→` twice returns to live; reducer tests cover both ends of the list
- [ ] A question that arrives while browsing does not change the screen; the status line shows `1 new`; returning to live shows it with its Approach
- [ ] Browsing with a pinned question: the pin and its queue behave as today once back on live
- [ ] A section body taller than its area scrolls with `↑ ↓ PgUp PgDn` to its last row and stops at both ends; the hint shows rows above and below
- [ ] Snapshot tests at 100 × 30 and 140 × 30: an earlier question on screen, a scrolled section, the `n new` badge. Every existing snapshot with no history and no overflow is unchanged
- [ ] The status line's key hints name the new keys when they apply; docs/SPEC.md § UI, Terminal MVP lists them
- [ ] `just check`, `just test` and `just lint` green

## Verify
```bash
cargo test -p dmn-session
cargo test -p dmn-cli tui
cargo test -p dmn-cli run
# by hand, on a 100-column pane: after the third question press ← ← → →, then 1 and ↓ / PgDn
dmn run --replay testdata/sysdesign/interviewer.wav testdata/sysdesign/candidate.wav
```
