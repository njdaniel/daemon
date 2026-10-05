# #109 — speak: a key translates your last utterance and speaks it to the interviewer

Labels: `size:M, model:opus, m4, lane:llm`

Written 2026-10-05. Third and final issue of the translate-and-speak feature (planned 2026-10-05 with #107 and #108). #107 speaks text in your cloned voice; #108 routes it to a virtual microphone the interviewer hears. This issue is the feature the user asked for: press a key, and what you just said in your own language is spoken in the interview's language, in your voice.

## Goal
With `[speak].enabled = true` and `target = "es"` (any whisper language code), pressing `s` in the TUI takes your last finalized `You` utterance, translates it to the target language with the configured LLM (default Ollama, `think: false`), synthesizes it in your enrolled voice and plays it into `dmn-voice`. The status line shows `translating…` / `speaking…` while it happens.

## Size / Model / Turns
M | Opus | 45

## Scope

### Translation (`crates/dmn-llm/src/translate.rs`, new)
- One non-streaming `chat()` call through the existing `Provider` trait: system prompt "translate the user's message verbatim into <target>; output only the translation", user message = the utterance text. Uses the session's configured provider and model; the fast-phase timeout is the budget.
- Output capped in `limits.rs` (new `TRANSLATION` limit, ~600 chars) — enforced there, not in the prompt (repo rule).
- The `You` lane keeps its original-language text: whisper runs with translation **off** for that lane when `[speak]` is enabled (it already does for `Language::English`/`Auto` per #77 — auto pins and translates *to English*, which would destroy the source text; when `[speak].enabled` and `stt.language = "auto"`, the You lane transcribes without whisper-translate). State the interaction plainly in the PR.

### Config (`crates/dmn-core/src/config.rs`)
- `[speak]` gains `target: String` (2–3 letter code, same validation as `stt.language`), no default — enabling `[speak]` without `target` is a config error naming the key.
- TEMPLATE + key-count test updated.

### Pipeline + TUI (`apps/dmn-cli`)
- Key `s` → `Command::Speak` (same path as the existing pin/regenerate/ask keys): the pipeline pulls the last finalized `You` segment from its TranscriptLog, runs translate → synthesize → play (#108's vmic). No `You` final yet: status shows `nothing to speak`.
- New actions `Action::SpeakStarted` / `SpeakDone` (with the translated text, shown under the question area as one dim line, capped by the limit) so the session reducer owns the state and snapshots can pin it.
- Status line tag `speaking` / `translating…` beside the existing `es→en` and `asks:` tags.
- While a speak is in flight, the pipeline's EchoGuard ignores anything on either lane for the playback window. In practice a virtual source does not appear on the loopback, so this is a belt-and-braces time-window guard, not new echo logic.
- Assist on/off is independent: `s` works with assistance off (it is a speech feature, not an answer feature). The key does nothing when `[speak].enabled = false`.

### Docs
- README: the `s` key, `target`, a note that translation runs on the same Ollama model so it adds ~1 s.
- docs/SPEC.md: a Speak subsection (key, flow, limits, privacy: text goes to localhost Ollama and the localhost TTS server only).
- docs/issues/README.md paragraph (the three issues) if not already written.

## Out of scope
- Automatic per-utterance speaking, queueing, barge-in, speaking the LLM's *suggested* answer instead of your own words (all later, if wanted).
- Translating the interviewer's speech for you.
- Changing `target` mid-session.

## Depends on
#107, #108 (both merged)

## Acceptance
- [ ] Replay an English candidate fixture with `[speak].enabled`, `target = "es"`, mock LLM + mock TTS: pressing `s` after a `You` final produces exactly one translate request whose body is that utterance, then one synthesize request whose text is the (mock) translation, then playback starts
- [ ] With no `You` final yet, `s` shows `nothing to speak` and makes no LLM or TTS request
- [ ] The translate request goes through the configured provider with `think: false`; the translation is capped by the new `limits.rs` limit
- [ ] Status line shows `translating…` then `speaking…`, then clears; snapshots at 100 × 30 and 80 × 24
- [ ] `s` is inert with `[speak].enabled = false`, and works with assistance off
- [ ] `[speak].enabled = true` with no `target` is a config error naming `target`
- [ ] Echo guard drops segments during the playback window (unit test, no PipeWire)
- [ ] README + SPEC updated
- [ ] `just check`, `just test` and `just lint` green

## Verify
```bash
cargo test -p dmn-llm translate
cargo test -p dmn-cli pipeline
cargo test -p dmn-cli tui          # snapshots: translating/speaking status, dim translation line
cargo test -p dmn-core config
# live on the dev box (reported in the PR, not gated):
cargo run -p dmn-cli -- run        # [speak].enabled, target = "es", dmn-voice selected in the call
# say something in English, press s, the interviewer hears it in Spanish in your voice
```

## Tests to add
- translate.rs: prompt shape, verbatim-only instruction, limit enforcement, provider errors surface as status text
- pipeline: `s` with a `You` final (mock LLM + mock TTS) → one translate, one synthesize, one play; `s` with none → nothing; disabled → nothing
- session reducer: SpeakStarted/SpeakDone transitions; status tags
- TUI snapshots: both status tags, the dim translation line, 100 × 30 and 80 × 24
- echo window: a segment inside the window is dropped, one outside is kept
