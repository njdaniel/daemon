# #96 — ui: a key sends what the interviewer last said to the model when the detector missed the question

Labels: `size:M, model:opus, m2, lane:ui`

Proposed 2026-10-04, from the same mock interview as #93. The detector dropped a real question and the user had no way to get it answered: `r` regenerates the question already on screen, and no key reaches a line the detector did not turn into a candidate. #93 fixes that one cause; this is the way out for every other miss (a question with no question form, a rule nobody has found yet). Found against `main` (978d13c):

- `apps/dmn-cli/src/pipeline.rs` `Command` has `Regenerate`, `Swap` and `Assist`. A candidate is only ever made by `Detecting::feed` → `Detector::observe`; `emit()` then dispatches it to the hub and calls `Suggester::submit_after`.
- When the fast phase answers `is_question: false` or `small_talk`, the TUI shows nothing: `Action::Ignored` only raises `Counts::ignored`. That is right for a detector candidate and wrong for a key press, which needs an answer either way.

Choices are marked **Decided**; say so on the issue if one should go the other way.

## Goal
One key press makes the last thing the interviewer said the question: it goes to the fast phase as a candidate without passing the detector, and the screen updates as for any detected question.

## Size / Model / Turns
M | Opus | 60

## Scope
`apps/dmn-cli/src/run.rs` (the key), `apps/dmn-cli/src/pipeline.rs` (a `Command::Ask`, building the candidate from the transcript log), `crates/dmn-session` (the feedback state), `apps/dmn-cli/src/tui/status.rs`, docs/SPEC.md § UI, Terminal MVP and § Question detector.

- **Decided**: the key is Enter.
- **Decided**: the candidate's text is the asking lane's last turn: its most recent finals with no final from the other lane between them, newest last, going back at most `CONTINUE_SPAN` (30 s). The fast phase already reduces "what the Interviewer just said" to one question, so a turn that opens with remarks and ends with the question needs no rule here. The asking lane follows `[detect].question_source` (#90); with `both` it is whichever lane spoke last.
- The candidate takes the normal path after that: `wire::candidate`, the 60 s transcript window, `submit_after` with what was answered before it (#57), the slow phase, the store. Under a pin it queues like any other.
- **Decided**: Enter does nothing when that turn already produced the live question (that is `r`), when the lane has said nothing yet, and while assistance is off (#79), like `r` and `w`.
- **Decided**: when the model answers `is_question: false` or `small_talk` to a key press, the status line says `not a question` for 5 s and the screen keeps what it had. A detector candidate that is ignored stays silent as today.
- The status line's key hints gain `⏎ ask` while assistance is on.

Out of scope: typing or editing a question, choosing an older line, a hand-off from the mic lane when `question_source` is `interviewer`, logging what the detector vetoes, the M3 window.

## Depends on
#12, #57, #79, #90 (all merged)

## Acceptance
- [ ] With the session-24 lines as the transcript and no candidate from them, Enter shows "How would you reserve and then reconcile usage correctly under heavy concurrent traffic across regions?" with its Approach (MockProvider); the store has the question and its suggestion
- [ ] A turn of three interviewer finals within 30 s is sent as one candidate in spoken order; a You final between them cuts the turn there
- [ ] Enter while pinned queues the question and the pin badge counts it; Enter twice on the same turn sends one request; Enter with assistance off or before anyone has spoken sends none
- [ ] A reply of `is_question: false` to a key press shows `not a question` on the status line for 5 s; the same reply to a detector candidate shows nothing (snapshot tests for both)
- [ ] `question_source = "you"`: Enter sends the mic lane's last turn
- [ ] docs/SPEC.md and the README's key list name the key
- [ ] `just check`, `just test` and `just lint` green

## Verify
```bash
cargo test -p dmn-cli pipeline
cargo test -p dmn-cli run
cargo test -p dmn-cli tui
cargo test -p dmn-session
```
