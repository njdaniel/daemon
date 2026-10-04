# #95 — ui: the mouse works in the TUI: the wheel scrolls the open section, a click opens a section

Labels: `size:S, model:sonnet, m2, lane:ui`

Proposed 2026-10-04, from a live mock interview (session 24). The user's words: "im not sure if it responds to mouse clicks". It does not: `apps/dmn-cli/src/run.rs` never enables mouse capture and its event loop handles `Event::Key` and `Event::Resize` only, so clicks and the wheel are left to the terminal emulator and never reach dmn as mouse events.

Choices are marked **Decided**; say so on the issue if one should go the other way.

## Goal
In `dmn run`, the mouse wheel scrolls the open section and a click on a section row opens or closes it, the same as its number key.

## Size / Model / Turns
S | Sonnet | 30

## Scope
`apps/dmn-cli/src/run.rs` (enable and restore mouse capture, `Event::Mouse`), `apps/dmn-cli/src/tui/` (hit-testing a click against the section grid the last frame drew), `crates/dmn-core/src/config.rs` (`[ui].mouse`), docs/SPEC.md § UI, Terminal MVP.

- Wheel up / down: #94's scroll, three rows per notch, when a section is open and the pointer is anywhere on the left column. Otherwise nothing.
- Left click on a section row (`1 ▸ Deeper Answer` …): the same action as its number key. Clicks anywhere else do nothing.
- **Decided**: `[ui].mouse = true` by default. Mouse capture takes text selection away from the terminal (WezTerm still selects with Shift+drag), so `mouse = false` restores today's behaviour for anyone who copies from the transcript.
- **Decided**: no click targets for history (`←` / `→` stay keys) and none on the transcript.
- Capture is released on every exit path that leaves the alternate screen (q, Ctrl+C, a panic hook, an error), so the shell is never left with the mouse captured.

Out of scope: drag, right click, hover, clickable history, the assistance-off screen (no sections to click), the M3 window.

## Depends on
#94 (merged)

## Acceptance
- [ ] A wheel event over an open, overflowing section scrolls it and stops at both ends; with no section open it changes nothing
- [ ] A left click on each of the six rows, in the one-column and two-column grids and in the #88 wide layout, toggles that section; a click outside the grid changes nothing
- [ ] `[ui].mouse = false`: no capture is enabled and no mouse event is read; `dmn config show` prints the key
- [ ] After q, Ctrl+C and a forced error the terminal's own selection and wheel work again
- [ ] docs/SPEC.md § UI, Terminal MVP and the README's key list mention the mouse and the config key
- [ ] `just check`, `just test` and `just lint` green

## Verify
```bash
cargo test -p dmn-cli run
cargo test -p dmn-cli tui
cargo test -p dmn-core config
# by hand: open section 1 with a click, scroll it with the wheel, quit, then select text in the shell
dmn run --replay testdata/sysdesign/interviewer.wav testdata/sysdesign/candidate.wav
```
