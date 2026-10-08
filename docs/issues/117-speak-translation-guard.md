# #117 — speak: a reply the translator invented is never spoken — a sanity check on the translation in Rust, and a prompt that does not cast the model as the candidate

Labels: `size:S, model:fable, m4, lane:llm`

Found 2026-10-08 in the second live Discord test of #113 (translator `llama3.1:8b`, target `ru`). The user's turn was "Back to English. And no input." The model answered, in English: "I'm a quick learner and I can pick up new things fast. I've been working with computers for years, so I think I'd be a good fit for this job." dmn spoke all 14 s of it into the call in the Russian voice (the TTS server's log holds both sentences as the two requests of that press; the take is kept in the owner's `~/dmn-tests/`). Re-running the same turn through the prompt gives a correct Russian line nine times in ten, and "Мне не нравится эта компания. У них очень плохая репутация…" ("I don't like this company, they have a bad reputation…") the tenth: the small model stops translating and starts talking as the candidate.

Two causes, both in this repo:
- The prompt's first rule casts the model as the candidate: "The message is what a candidate just said aloud in a job interview. The translation is spoken in their own voice to the interviewer." A fragment, a test phrase or small talk gives a small model nothing to translate and a role to play instead.
- Nothing checks the reply. `limit_translation` cuts length and strips quotes; a reply in the wrong language or four times the length of the turn goes to the TTS as it is. In a live interview an invented answer in the user's own voice is the worst outcome the feature has, far worse than speaking nothing.

## Goal
A reply that cannot be a translation of the turn is dropped with a 5 s notice and nothing is spoken; the prompt makes the derail rarer in the first place.

## Size / Model / Turns
S | Fable | 20

## Scope
`crates/dmn-llm/src/translate.rs`, `crates/dmn-llm/prompts/translate.md`, `apps/dmn-cli/src/speaker.rs` (the call site), docs/SPEC.md § Speak, README § Speak.

- **Decided**: the check lives in Rust (`translate.rs`), beside `limit_translation`, and takes the turn, the reply and the target's language code. It rejects a reply that is longer than two and a half times the turn plus forty characters (German and Russian run 1.2–1.6× English; a reply far past that was not translated), and, for a target whose script is not Latin (Cyrillic for `ru uk bg be sr mk`, Greek `el`, Hangul `ko`, kana/Han `ja`, Han `zh`, Arabic `ar fa ur`, Hebrew `he`, Devanagari `hi mr ne`, Thai `th`), a reply whose letters are not mostly in that script. For a Latin-script target no script check: English cannot be told from German cheaply, and the length rule still holds.
- A rejected reply is `LlmError::MalformedStream` with a detail that starts with `off-script`; `failure_text` shows `translate failed: off-script reply` on the status line (the existing 5 s notice) and the full reply goes to the log at `warn`, so the owner can see what the model wrote. No retry: at temperature 0 the same request gives the same reply, and the user can press `t` again after the notice.
- The prompt no longer says whose words they are or where they were said. It says the text is a fragment of a live transcript — it may be a partial sentence, small talk or a test phrase — and is to be translated literally even then; never answered, continued or spoken for. Person, tense and tone stay.
- The check is applied to every translation, `dmn run`'s `t` (and #114's automatic turns once merged) alike; `dmn speak "text"` speaks text as given and is not affected.

Out of scope: a better small translator (none other was tried), a retry with a reworded request, detecting a Latin-script reply in the wrong Latin-script language, confidence from the model.

## Depends on
#113 (merged)

## Acceptance
- [ ] The failing turn is a test: "Back to English. And no input." with the English reply above, target `ru`, is rejected (`off-script`), nothing is returned to speak, and the status text is `translate failed: off-script reply`
- [ ] A correct Russian, German and Spanish translation of an English line passes; a German reply 1.6× the English passes; a short reply ("Ja." for "Yes.") passes; a Russian reply with a Latin brand name in it passes
- [ ] A reply in Latin letters for target `ru`, and a reply four times the turn's length for target `de`, are rejected
- [ ] The prompt test (`the_prompt_names_the_target_asks_for_the_translation_only_and_carries_the_utterance`) is updated: no "candidate" or "interview" in the system prompt; "fragment" and "add nothing" in it
- [ ] By hand with `llama3.1:8b`: the ten short fragments from the live sessions ("And no input.", "Back to English.", "Hello.", "This is working.", …) translated 10× each; the PR body reports how many replies the check rejected and how many invented replies got through, before and after the prompt change
- [ ] docs/SPEC.md § Speak and README § Speak say a reply that cannot be a translation is dropped with a notice
- [ ] `just check`, `just test` and `just lint` green

## Verify
```bash
cargo test -p dmn-llm translate
cargo test -p dmn-cli speaker
# by hand: dmn run --no-assist, say "And no input.", press t ten times, read the log for `off-script`
```
