# #93 — detect: a technical question that contains a small-talk word ("traffic", "connection") is no longer dropped

Labels: `size:S, model:sonnet, m2, lane:llm`

Proposed 2026-10-04, from a live mock interview (session 24, the ChatGPT desktop app asking). The interviewer said "Let's zoom into the quota problem. How would you reserve and then reconcile usage correctly under heavy concurrent traffic across regions?"; the line was transcribed correctly (segment 1726) and the screen stayed on the previous question. Found against `main` (978d13c):

- `crates/dmn-detect/src/cue.rs` `SMALL_TALK` holds single words that are also system-design vocabulary: `traffic`, `connection`, `audio`, `mic`, `microphone`, `calendar`, `invite`, `frozen`, `door`, `lunch`, `holiday`, `vacation`, `snow`. `is_small_talk` matches each as a whole word anywhere in the sentence; its doc comment says "and the sentence is not carrying a technical question around it", which the code does not check.
- `DetectConfig::default()` sets `small_talk_vetoes: true`, so one such word removes the candidate whatever it scores. The sentence above scores 6 (`?` + opener + interviewer + long) and is vetoed by `traffic`. No `[detect].sensitivity` setting gets it back.
- The drop is silent: nothing in `dmn.log`, no `questions` row, no `ignored` count (the candidate never exists).
- The same prompt sent to `qwen3.6:35b-a3b` by hand returns `is_question: true, kind: system_design` with a five-line Approach, so the detector is the only stage that fails.
- The fixtures did not catch it: no interviewer question in `testdata/` contains one of those words (only candidate answers and real small-talk lines do).

Also dropped today: "How do you handle connection pooling?", "How would you handle a traffic spike?", "Design a calendar app."

## Goal
A final that asks a technical question becomes a candidate even when it contains a word from the small-talk list; call chatter is still not answered.

## Size / Model / Turns
S | Sonnet | 30

## Scope
`crates/dmn-detect/src/cue.rs` (the `SMALL_TALK` list and `is_small_talk`), `detector.rs` only if the veto rule itself changes, and the tests of both.

**Decided** (say so on the issue if it should go the other way): the single words that are also technical vocabulary leave the veto list, and chatter that needs them is matched by phrase instead ("can you hear me", "my audio", "calendar invite", "at my door"). A chatter question that no phrase covers ("Is the traffic bad out there?") then reaches the fast phase, which already returns `kind: small_talk` and shows nothing (SPEC § Extraction); that costs one request and is preferred to dropping a real question. Words with no technical sense (`weather`, `weekend`, `coffee`) stay as they are.

Out of scope: logging or counting vetoed lines, a key that sends the last interviewer line to the model by hand (#96), changes to weights or thresholds.

## Depends on
#10, #63, #90 (all merged)

## Acceptance
- [ ] Each of these is a candidate from the Interviewer lane: the session-24 line above (candidate text is its last sentence), "How do you handle connection pooling?", "How would you handle a traffic spike?", "Design a calendar app."
- [ ] Still no candidate: "Can you hear me okay?", "How's the weather where you are?", "Did you find the calendar invite okay, the link changed at the last minute?", "Okay, hold on one second, someone's at my door.", "Sorry, my calendar double-booked me and the other call ran over."
- [ ] `is_small_talk`'s doc comment says what the code does
- [ ] `just eval` on the four fixtures: recall and precision no lower than on `main` (0.96 / 0.94 at #63); the numbers before and after are in the PR body
- [ ] `just check`, `just test` and `just lint` green

## Verify
```bash
cargo test -p dmn-detect
cargo test -p dmn-cli pipeline
just eval    # recall / precision per fixture, compared with main in the PR body
```
