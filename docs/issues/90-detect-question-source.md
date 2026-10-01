# #90 — detect: `[detect].question_source` picks whose questions are answered: interviewer, you or both

Labels: `size:M, model:opus, m2, lane:llm`

Written on GitHub on 2026-10-01 as "Allow questions from the user, interviewer, or both to trigger AI answers"; fleshed out the same day against the code on `main` (1c12fd9). The Problem and Proposed behavior sections are the original text. Where the original left a choice open, the choice made here is marked **Decided**; say so on the issue if one should go the other way.

## Problem
Question detection currently processes only the Interviewer stream. Users cannot ask Daemon technical questions directly through their microphone. This also prevents testing with mock-interview audio played from a phone into the computer microphone.

## Proposed behavior
Add a configurable question-source setting with three choices:
- Interviewer only (default; preserve current behavior)
- You only
- Both

Apply this selection to question detection and automatic answer generation without changing transcript speaker labels or audio routing. Microphone input remains labeled You; application playback remains Interviewer. The setting selects eligible streams, not actual voice identities.

Expose the choice in configuration and the CLI, and make the active selection visible in the TUI. When AI assist is off, no question detection or LLM requests should run regardless of the selection.

## Goal
`[detect].question_source = "interviewer" | "you" | "both"` (default `interviewer`) chooses which lane's finals the detector scores and the LLM answers; everything else about a session (labels, routing, transcript, store) is the same in all three.

## Size / Model / Turns
M | Opus | 45

## Where the Interviewer lane is hard-coded today
Five places, all of which this issue touches:

1. `crates/dmn-detect/src/detector.rs` `Detector::observe`: returns `None` unless `segment.source == Source::Interviewer`.
2. `detector.rs` `Following::matches`: "followed" means 1.5 s of silence or `next_speaker == Some(Source::You)`.
3. `apps/dmn-cli/src/pipeline.rs` `Detecting::feed`: only an Interviewer final is scored, held as `pending`, or added to a `Chain` (#63); `you` holds the You lane's spans as "the other side spoke, the question is over".
4. `crates/dmn-llm/src/prompt.rs` `user_message`: the candidate goes under `## Interviewer just said`.
5. `crates/dmn-session/src/state.rs` `last_interviewer` and `apps/dmn-cli/src/tui/question.rs`: the listening screen shows the last Interviewer line, or "Waiting for the interviewer…".

Already lane-neutral, so no change: `Candidate.source` → `Action::Candidate.speaker` (`dmn-core/src/wire.rs`), and the store links a question row to its segment by `(speaker, start)` (`dmn-core/src/recorder.rs`). The #79 gate (`Assistance::feed` and `flush` return early while off) sits above the detector, so assist-off holds for every source without new code; it needs a test, not a change.

## Scope

### Config and CLI
- `crates/dmn-core/src/config.rs`: `Detect.question_source`, an enum `interviewer | you | both`, default `interviewer`; a line in the commented template under `[detect]` beside `sensitivity`. **Decided:** it lives in `[detect]`, not `[assist]`: it is a detector input, like `sensitivity`.
- `crates/dmn-detect/src/config.rs`: `QuestionSource { Interviewer, You, Both }` with `from_name` and `allows(Source) -> bool`, a field on `DetectConfig`. Same shape as `Sensitivity`, so `dmn-detect` still does not depend on `dmn-core`.
- `apps/dmn-cli/src/main.rs`: `--question-source <interviewer|you|both>` on `dmn run` and `dmn replay`, overriding the config for that run, passed through `pipeline::Options` into `Detecting::new`.
- **Decided:** no runtime key. The source is fixed for the session, like `--language`. The on-demand hotkey from the original's follow-up stays a separate issue.

### Detector (`dmn-detect`)
- `observe` scores a final when `cfg.question_source.allows(segment.source)`.
- `Following::matches` takes the line's own source: followed means ≥ 1.5 s of silence or the next final coming from the *other* lane. For an Interviewer line this is today's rule exactly.
- **Decided:** the `Interviewer` cue (+1) stays as it is; a You line does not get it. The question-form gate and the rhetorical and small-talk vetoes apply to both lanes unchanged, which is what keeps statements on an enabled lane out. Consequence: a You line needs one more point than the same words from the Interviewer. At `balanced` (threshold 2) `?` or an opener alone still clears it; at `strict` (3) a bare "Why?" from the mic does not. No weight changes in this issue.

### Pipeline (`apps/dmn-cli/src/pipeline.rs`, `Detecting`)
- `feed` scores finals from every eligible lane. `pending` stays one slot: the next final from either lane resolves it.
- `Chain` (#63) records its lane. A final continues a chain only when it is from the same lane; "the other side spoke in between" uses the other lane's spans, so `you` becomes a per-lane span list. With `interviewer` the behaviour, and every existing `Detecting` test, is unchanged.
- In `both`, a You question asked while an Interviewer chain is open ends that chain and starts its own; the two are separate candidates, never joined.
- The echo guard already runs before `Assistance::feed`, so a mic copy of the interviewer's question (speakers, no headphones) is dropped before the detector sees it and `both` does not answer one question twice. A You final is held up to 1.5 s (`echo::HOLD`, 4 s while an Interviewer utterance is open), so a You question's candidate is that much later than an Interviewer one's. Not changed here; the PR states the measured figure.

### Prompt (`dmn-llm`)
- `FastRequest` (and the slow request built from it) carries who asked. `user_message` writes `## Interviewer just said` or `## You just said` to match; `You` is the label the transcript lines already use. Any other sentence in the fast or slow system prompt that assumes the Interviewer asked is reworded to hold for both. No length limits change (`limits.rs` untouched).

### Session and TUI
- The session knows the source (one `Action` at start, like `Action::Assist`), so rendering stays a function of session state and the snapshot fixtures can set it.
- Status line: **Decided:** a tag `asks: you` or `asks: both` beside the `es→en` tag, shown only when the source is not `interviewer` and assistance is on. The default screen is byte-identical to today, and the 80-column status line keeps its room.
- Listening screen: `last_interviewer` becomes the last final from an eligible lane; the placeholder reads "Waiting for the interviewer…", "Waiting for your question…" or "Waiting for a question…" for the three sources.
- The transcript, its speaker labels and colours are untouched. The three-area layout is untouched.

### Replay and eval
- `dmn replay`'s candidate line and its `--json` candidate event name the lane (added if absent), so a run shows which stream triggered.
- `eval/run.py` is not changed: it scores the default source against `questions.json`, which lists interviewer questions only.

### Docs
`docs/SPEC.md` § Transcription and question detection ("runs on every final Interviewer segment" becomes "every final segment of the lanes `[detect].question_source` selects", the followed-by row, the split-questions paragraph) and § Data, config and privacy; README config and flags; the config template.

## Out of scope
- Telling voices apart on one lane. With mock audio played from a phone into the mic, the interviewer's voice and yours are both `You`: in `you` or `both` your own clarifying questions ("Are we limiting per API key, per user, or per IP?") trigger answers too. That is the stated meaning of "eligible streams, not voice identities".
- A hotkey to answer your latest question on demand (the original's optional follow-up).
- Changing the source mid-session.
- Session memory (#57): "what the candidate said since the question" still collects You lines after the question, whoever asked it.
- Detector weights, sensitivity, the echo guard's timings, audio routing.

## Depends on
#63, #79 (both merged)

## Acceptance
- [ ] Default (`interviewer`, no flag, no config line): `dmn replay` output on the four English fixtures and every existing TUI snapshot are unchanged; `just eval` prints the same recall and precision as `main`
- [ ] `you`: a question on the You lane becomes a candidate and is answered; an Interviewer question on the same run produces no candidate and no LLM request
- [ ] `both`: either lane's question triggers; a You question and an Interviewer question 1 s apart are two candidates, not one continued question
- [ ] An echoed copy of an Interviewer question on the mic lane yields one candidate in `both`, not two
- [ ] In every mode the transcript lines, their speaker labels and the stored segments are identical to the default run's; a You-lane question row in the store links to the You segment
- [ ] A statement on an enabled lane ("First I'd clarify the scope.") is not a candidate; a rhetorical or small-talk line from You ("Can you hear me?") is vetoed as it is for the Interviewer
- [ ] A You question split at a pause is one continued candidate (#63's rule on the You lane)
- [ ] The prompt for a You candidate says `## You just said`; for an Interviewer candidate it is byte-identical to today's
- [ ] Assistance off with `you` and with `both`: zero candidates, zero context retrievals, zero LLM requests; on again resumes with the next eligible question
- [ ] `--question-source` on `run` and `replay` overrides `[detect].question_source`; an unknown value is a config error naming the three choices
- [ ] Status line shows `asks: you` / `asks: both`, nothing for the default, nothing while assistance is off; the listening placeholder matches the source; snapshots at 100 × 30 and 80 × 24
- [ ] docs/SPEC.md, README and the config template describe the setting
- [ ] `just check`, `just test` and `just lint` green

## Verify
```bash
cargo test -p dmn-detect
cargo test -p dmn-llm prompt
cargo test -p dmn-cli pipeline
cargo test -p dmn-cli tui
just eval    # default source: same numbers as main

# You only, on a fixture whose candidate asks two questions (turns.json: 25.2 s and 432.4 s):
cargo run -p dmn-cli -- replay --no-llm --question-source you \
  testdata/continuous/interviewer.wav testdata/continuous/candidate.wav
# expected: every candidate is from the You lane, among them "Just to check, do you mean a limit
# per calling service, or one global limit for the whole API?"; no Interviewer candidate

# The phone-into-the-mic case: the interviewer's audio on the You lane.
cargo run -p dmn-cli -- replay --no-llm --question-source you \
  testdata/sysdesign/candidate.wav testdata/sysdesign/interviewer.wav
# the PR records how many of sysdesign's scripted questions are found this way (reported, not gated)
```

## Tests to add
- config: default is `interviewer`; each value parses and round-trips; an unknown value is refused
- detector: `observe` per source for each of the three settings; `Following` for a You line (followed by an Interviewer final, by silence, by another You final)
- `Detecting`: You-only ignores Interviewer finals; both scores each lane; a chain does not cross lanes; a You question split at a pause continues
- pipeline with `MockProvider`: `you` and `both` with assistance off make no request; the echo case in `both` makes one
- prompt: the header per asker
- TUI snapshots: `asks: you`, `asks: both`, the three listening placeholders
- one fixture test on `continuous/` under `you` (behind the same gate as the existing whisper fixture tests)
