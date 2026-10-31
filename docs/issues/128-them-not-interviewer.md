# #128 — the other person is "Them", not "Interviewer" — the transcript, config, CLI and store stop assuming every call is an interview

Labels: `size:M, model:opus, m4, lane:infra, backlog`

Proposed 2026-10-18 by the owner. Since #79 (assistance off) and the speak features (#107–#124), dmn is used on ordinary calls: translated Discord and Telegram conversations where nobody is interviewing anyone. The transcript still labels the other side "Interviewer". The owner chose **Them** (it pairs with "You", is short in the label column, and fits interviews, calls and translated chats alike) and a full rename with the old names still accepted, so existing configs, scripts and databases keep working.

## Goal
Everywhere dmn names the other person's lane, it says `Them` / `them`: the transcript labels, status and placeholder text, the LLM prompts' speaker label, `[detect].question_source`, `--question-source`, the session JSON, the store and the code. Every place that reads a name (config, CLI, session JSON, the store) still accepts `interviewer` and treats it as `them`.

## Size / Model / Turns
M | Opus | 45

## Scope
- **Code.** Rename the variant and its identifiers and comments: `dmn_audio::Source::Interviewer`, `dmn_session::Speaker::Interviewer`, `dmn_store::Source::Interviewer`, `dmn_core::config::QuestionSource::Interviewer`, `dmn_llm::Asker::Interviewer`, `QuestionSourceArg::Interviewer` in `apps/dmn-cli/src/main.rs`, and the lane names in `dmn-transcript` (echo guard, segment render), `dmn-detect`, `dmn-core` (wire, recorder, config) and `apps/dmn-cli` (pipeline, run, replay, bench, probe, tui). Comments that mean the person in a job interview (the SPEC's latency budget, the interview prompts' framing) can keep the word; comments that mean the lane say Them.
- **TUI.** The transcript label `Interviewer` becomes `Them`, same colour (cyan). The narrower label gives the text more room; the snapshots are regenerated and checked by eye, not just accepted. The LISTENING placeholder for the default source becomes "Waiting for their question…". The `asks:` status tag shows `them` when the source is not the default (it never shows for the default, as now).
- **Transcript text.** `Segment::render` and every other text rendering write `Them:` (`[00:12.4] Them: how would you design`, `Them (es→en): …`).
- **Config.** `[detect].question_source = "them" | "you" | "both"`, default `them`; `"interviewer"` still parses as `them` (serde alias) and is never written. The template line and SPEC example say `them`.
- **CLI.** `--question-source them|you|both`; `interviewer` is accepted as a hidden alias. `replay` and `run --replay` name their positional arguments `<THEM> <YOU>` (`value_names`) and the help says "the other person's audio" / "your audio". The fixture files keep their names (below).
- **Session JSON.** `Speaker` serialises as `"them"` and deserialises `"interviewer"` too. The `testdata/session-*.json` fixtures are rewritten to `"them"`; one test keeps an `"interviewer"` file to prove the alias.
- **Store.** Migration v9 rewrites `segments.source = 'interviewer'` to `'them'` and changes the column's `CHECK` to `('them', 'you', 'unknown')`. SQLite cannot alter a `CHECK`, so this is a table rebuild; `questions.segment_id` references `segments(id) ON DELETE CASCADE` with `foreign_keys = ON`, so the rebuild must not drop the old table with foreign keys enforced (that would delete every question). `Source::parse` still maps `"interviewer"` to `Them` for any row it meets.
- **Prompts.** Transcript lines fed to the model say `Them:`, and the `## Interviewer just said` header becomes `## Them just said` (`Asker::label`). `base.md`, `slow.md` and `clarification.md` refer to that section by its new name and say once that, in an interview, Them is the interviewer, so the model's view of the interview does not change. `limits.rs`'s header-stripping test follows. `just eval` before and after: recall and precision may not drop.
- **Docs.** SPEC (§ Audio capture's lane labels, the pipeline diagram's `source=interviewer`, `question_source`), README, `docs/design/*.dc.html` label text, `CLAUDE.md` if it names the label. The product is still an interview copilot; "the interviewer's question" in the SPEC's goal and latency budget stays.
- **#121.** `crates/dmn-audio` is also published as njdaniel/dmn-audio (Conch pins it by commit). The PR comments on #121 that the in-tree crate renamed `Source::Interviewer` to `Source::Them`, so the extracted crate makes the same change before the switch. Nothing in the other repository is changed by this issue.

Out of scope: the fixture file names `testdata/*/interviewer.wav` / `candidate.wav`, the fixture scripts and `turns.json` (they come from release tarballs via `just fixtures-fetch` and are interviews), `eval/` and `tools/fixtures/` wording about interviewer turns, closed issue files in `docs/issues/`, njdaniel/dmn-audio itself, and the "interview copilot" product description.

## Depends on
#90 (question source), #79 (assist off), both merged

## Acceptance
- [ ] No `Interviewer` / `interviewer` identifier or label is left in `crates/` or `apps/` except the aliases, the v9 migration and the tests that prove them (`grep -rniw interviewer crates apps --include=*.rs` lists only those, plus comments about job interviews)
- [ ] The TUI transcript shows `Them`; snapshots regenerated (`cargo test -p dmn-cli tui`)
- [ ] `question_source = "interviewer"` and `--question-source interviewer` both behave as `them`; the template and SPEC example say `them` (`cargo test -p dmn-core config`, `cargo test -p dmn-cli`)
- [ ] A session JSON with `"speaker": "interviewer"` loads as Them (`cargo test -p dmn-session`)
- [ ] A v8 database with interviewer segments, questions and suggestions migrates to v9 with every row kept and `source = 'them'`; a fresh database rejects `'interviewer'` on insert through the `CHECK` but `Source::parse("interviewer")` still reads it (`cargo test -p dmn-store`)
- [ ] The prompts say `## Them just said`; `just eval` recall and precision are no lower than on `main` (numbers before and after in the PR)
- [ ] `dmn replay testdata/sysdesign/interviewer.wav testdata/sysdesign/candidate.wav` labels the other lane `Them`
- [ ] #121 has a comment naming the dmn-audio rename
- [ ] `just check`, `just test` and `just lint` green

## Verify
```bash
grep -rniw interviewer crates apps --include=*.rs     # only aliases, migration, their tests, job-interview comments
cargo test -p dmn-store migrate
cargo test -p dmn-core config
cargo test -p dmn-session
cargo test -p dmn-cli tui
cargo run -p dmn-cli -- replay testdata/sysdesign/interviewer.wav testdata/sysdesign/candidate.wav | grep -c 'Them'   # > 0
just eval                                                 # compare with main
```
