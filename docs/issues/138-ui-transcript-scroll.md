# #138 — ui: scroll transcript history during a live session and return to live

Labels: `size:M, model:opus, m4, lane:ui, backlog`

Requested 2026-10-24 during a translation-only Discord call in WezTerm: the user wanted to scroll up and review earlier speech while the call continued.

## Goal
Scroll through the current session's transcript with the keyboard or mouse, then return to the newest speech without interrupting capture, transcription or translation.

## Size / Model / Turns
M | Opus | 45 (docs/PROCESS.md)

## Scope
`crates/dmn-session` transcript/view state, `apps/dmn-cli/src/run.rs` input, `apps/dmn-cli/src/tui/` rendering and status; read-only history access through the existing store if needed; README and SPEC Terminal MVP.

Current behavior: the view follows the newest wrapped lines, `Session::prune_transcript` retains only 60 seconds, and arrow/page/wheel scrolling applies only to an open helper section. #94 explicitly excluded transcript scrolling.

- Make finalized transcript history available from the beginning of the current run, including lines older than 60 seconds. Keep the detector/LLM's recent-context window separate. Define a bounded in-memory cache and page persisted history outside the render loop; document a bounded history limit for `--no-store` rather than silently growing memory without limit.
- In assistance-off mode, Up/Down scroll a row, PgUp/PgDn a page, and End returns to live. In assistance-on mode provide explicit transcript focus, visibly marked, so those keys preserve existing section scrolling when a section has focus.
- With mouse capture enabled, wheel events over the transcript scroll it; section scrolling still works over the section area. Respect `[ui].mouse = false`.
- Anchor the reader to the same content while new finals, partial updates or translation lines arrive. Show a visible history/live indicator and a count of new finalized turns while browsing.
- Reaching the bottom or pressing End resumes following live speech. Resizing preserves the viewed position as closely as wrapping permits.
- Preserve speaker labels, timestamps, translation text and mode markers. Support transcript-only, stacked and wide layouts.

Out of scope: browsing previous sessions, transcript editing/search, speaker diarization and the M3 window. No LLM request is needed to scroll.

## Depends on
None beyond current main. Related merged features: #79 (assistance-off layout), #88 (wide transcript), #94 (section scrolling), #95 (mouse), #136 (saved transcripts).

## Acceptance
- [ ] A deterministic session spanning over 60 seconds can scroll back to its first finalized turn within the documented retention policy; scrolling stops at both ends.
- [ ] Keyboard navigation, End and mouse-wheel hit testing work without changing question history or section scrolling.
- [ ] New turns, partial replacements and spoken translations do not move an anchored historical view; the new-turn count and return-to-live behavior are correct.
- [ ] History remains available with assistance off; `--no-store` respects its documented memory limit without writing a session to disk.
- [ ] Snapshot tests at 80×24, 100×30 and 140×30 cover browsing, new-turn hints, wrapping/resizing and returning to live.
- [ ] README and SPEC document controls and history retention.
- [ ] `just check`, `just test` and `just lint` pass.

## Verify
```bash
cargo test -p dmn-session
cargo test -p dmn-cli tui
cargo test -p dmn-cli run
cargo test -p dmn-store
just check
just test
just lint
```
Use synthetic session events and existing replay WAVs, not a microphone. Tests must assert that scrolling does not pause the pipeline or issue model requests.

