# #58 — dmn-context: stories with tags; behavioral questions get a matched story in the STAR block, and Swap picks another

Labels: `size:M, model:opus, m2, lane:llm`

## Goal
For behavioral questions, match a story by tag overlap with the question's themes first and BM25 second, fill the STAR block from it, name the story in the header, and let Swap pick the next match (SPEC § Context and LLM, Sources; § UI spec, AI Assistance).
## Size / Model / Turns
M | Opus | 45   (from docs/PROCESS.md; labels must match)
## Scope
- `crates/dmn-context`: story files get a tag line (`tags: conflict, ownership`) from front matter or the first line; a matcher returns ranked stories for a question.
- `crates/dmn-llm`: the behavioral fast-phase prompt gets the matched story's text; `star.story` names its path. No match → STAR from the transcript only, no story named.
- Swap: a key (pick one that does not clash with the footer's current keys) re-runs the STAR request with the next-ranked story; a `dmn-session` action.
- `testdata/context/stories/`: five or more short tagged stories, written for the fixture or public domain, covering the behavioral fixture's questions.
Out of scope: everything else, including later issues.
## Depends on
#54, #56
## Acceptance
- [ ] A behavioral fixture question yields a STAR block with a matched story, and the header shows its path (SPEC M2 done-criterion; `dmn replay` on testdata/behavioral with the stories loaded)
- [ ] Tag overlap outranks BM25 when both match (unit test)
- [ ] Swap cycles through ranked stories and back; with one story it says there is no other (TestBackend test)
- [ ] Without stories loaded, behavioral questions behave as in M1
- [ ] `just check`, `just test` and `just lint` green
## Verify
```
cargo test -p dmn-context -p dmn-llm
cargo run -p dmn-cli -- sources add testdata/context/stories --kind stories
cargo run -p dmn-cli -- replay testdata/behavioral/interviewer.wav testdata/behavioral/candidate.wav --speed 4
```
