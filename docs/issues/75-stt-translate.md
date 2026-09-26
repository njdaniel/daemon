# #75 — dmn-stt: multilingual option — `--translate` turns Interviewer audio in any language into English text

Labels: `size:S, model:sonnet, m3, lane:stt`

## Goal
One option makes an interview in any language whisper supports work unchanged downstream: with `--translate` (or `[stt].translate = true`) the Interviewer lane runs whisper's translate task with the language auto-detected, so the transcript, detector, prompts and TUI see English. The You lane keeps transcribing as today. `transcriber.rs` currently hard-codes `set_language(Some("en"))` and never sets translate (SPEC § Transcription).
## Size / Model / Turns
S | Sonnet | 30   (from docs/PROCESS.md; labels must match)
## Scope
- `crates/dmn-stt/src/transcriber.rs`: a per-lane `Task { Transcribe, Translate }`. `Translate` sets `set_translate(true)` and `set_language(None)`; `Transcribe` keeps `Some("en")`. The detected language (`full_lang_id` on the state) goes on `Segment` as `lang: Option<String>`; `--json` records it.
- `crates/dmn-core/src/config.rs`: `[stt].translate = false`, template comment naming the model requirement; `dmn run --translate` and `dmn replay --translate` override it.
- Guard: `translate` on with a model whose name ends in `.en` exits before capture, naming the model and the fix (`dmn doctor --fetch medium`, or `large-v3-turbo` on the #61 GPU build). English-only models cannot translate and would emit garbage silently.
- Transcript strip: Interviewer lines whose `lang` is not `en` read `Interviewer <detected>→en`, where `<detected>` is whatever language whisper detected on that utterance (`es→en`, `ru→en`, `de→en`, changing per line if the speaker switches); one format string in the existing label, no language list anywhere in the code. `dmn doctor` prints `translate: on (<model>)` when configured.
- `README.md`: a short "Non-English interviews" section: the flag, the model requirement, that whisper is strongest on Spanish, German, French, Russian and Portuguese, and that `large-v3-turbo` needs the GPU build to hold the 2 s budget.
Out of scope: non-English fixtures (a later S if the option earns them), answering in another language, locking the detected language, the vocabulary prompt, named profiles, translating the You lane.
## Depends on
#61
## Acceptance
- [ ] `dmn replay --translate --model small` (multilingual) on the English fixtures gives the same recall and precision as `just eval` within 0.02 and `lang = en` on ≥ 95 % of Interviewer finals: the plumbing works and translate mode on English input is a no-op
- [ ] `dmn run --translate` with `small.en` exits non-zero before any capture starts, with the model name and the fix in the message; `dmn doctor` shows the mode
- [ ] Without `--translate`, `just eval` and the fixture headless numbers are unchanged
- [ ] Transcript-strip snapshots show `Interviewer es→en` and `Interviewer ru→en` for segments with `lang = es` and `lang = ru`, from the same format string
- [ ] Human check, not a gate: `dmn run --translate --model large-v3-turbo --app <browser>` beside a German or Spanish YouTube interview shows English Interviewer lines and an Approach; the PR body notes the model, GPU or CPU, and the p50 from `dmn bench --live`
- [ ] `just check`, `just test` and `just lint` green
## Verify
```
cargo run -p dmn-cli -- doctor --fetch small
python3 eval/run.py --model small          # baseline with the multilingual model
cargo run -p dmn-cli -- replay --translate --model small testdata/mixed/interviewer.wav testdata/mixed/candidate.wav --json | grep -c '"lang":"en"'
cargo run -p dmn-cli -- run --translate --model small.en; echo $?   # non-zero, names the model
cargo test -p dmn-stt -p dmn-cli translate
```
