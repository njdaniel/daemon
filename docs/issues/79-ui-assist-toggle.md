# #79 — ui: toggle interview assistance; transcribe/translate mode skips detector and LLM

Labels: `size:M, model:opus, m2, lane:ui`

## Goal

Let Daemon switch between a lightweight transcription/translation mode and the full interview-assistance pipeline during a session.

When assistance is off, Daemon should act only as a live transcriber/translator:

```text
audio → VAD → Whisper → transcript
```

For multilingual audio, Whisper's translate task continues to produce the English transcript without an LLM.

When assistance is on, the existing downstream pipeline runs:

```text
transcript → question detector → context retrieval → LLM → Approach / STAR / slow sections
```

## Size / Model / Turns

M | Opus | 45

## Scope

### Config

Add an assistance switch with today's behavior as the default:

```toml
[assist]
enabled = true
```

The exact config location may be adjusted to match the existing config structure, but the concept should remain separate from STT/transcription settings.

### Runtime toggle

Add a TUI key to enable/disable assistance without restarting Daemon.

Suggested key:

- `a` — Assist on/off

The status/footer should make the current state obvious, for example:

```text
assist off · stt gpu · es→en
```

### Assist off behavior

When assistance is disabled:

- VAD continues running.
- STT continues running.
- Multilingual Whisper translation continues running when configured.
- Speaker-labelled transcript continues updating.
- The question detector does not run for new transcript segments.
- Context retrieval does not run.
- No fast-phase or slow-phase LLM request is made.
- No Approach, STAR block, detected-question card, or slow sections are shown.
- The transcript expands to use the space released by the assistance UI.

This should be a real pipeline mode, not merely hiding already-generated assistance.

### Assist on behavior

When assistance is enabled:

- new transcript segments resume flowing through the existing detector → context → LLM path;
- the normal question / Approach / STAR / slow-section UI returns;
- existing STT/transcript state is preserved.

Turning assistance on does not need to retroactively process the entire transcript. Processing may resume with the next eligible interviewer segment/question.

### Session state

The current assist-enabled state should be represented in live session/UI state so the TUI renders deterministically.

Transcript, language detection, and stored segments remain unchanged regardless of assist mode.

Persisting the toggle across sessions is optional unless the existing config/session model makes it natural.

### Docs

Update:

- `docs/SPEC.md` pipeline/UI section
- README usage section
- config template / config docs
- TUI key documentation

Describe interview assistance as an optional layer over Daemon's transcription/translation pipeline.

## Out of scope

- disabling STT
- changing VAD behavior
- changing multilingual Whisper translation
- changing detector accuracy
- translating to arbitrary target languages
- GUI / Tauri work
- meeting summaries or note generation

## Acceptance

- [ ] Existing default behavior is unchanged with assistance enabled
- [ ] Pressing `a` toggles assistance without restarting Daemon (decided 2026-09-29: the key is Ctrl+Shift+A, sent as Ctrl+A by most terminals, so a stray `a` cannot switch it; comment on #79)
- [ ] With assistance off, live/replay transcript continues updating normally
- [ ] With multilingual STT enabled, translation continues while assistance is off
- [ ] With assistance off, new transcript segments do not run through question detection
- [ ] With assistance off, there are zero context-retrieval or LLM requests for new segments
- [ ] Question / Approach / STAR / slow-section UI is absent while assistance is off
- [ ] Transcript uses the released TUI space while assistance is off
- [ ] Re-enabling assistance resumes the existing pipeline for subsequent questions
- [ ] `just check`, `just test` and `just lint` are green

## Verify

```bash
cargo run -p dmn-cli -- run \
  --replay testdata/sysdesign/interviewer.wav \
  testdata/sysdesign/candidate.wav
```

Toggle assistance with `a` and verify that transcript updates continuously while detector/provider instrumentation remains idle when assist is off.

For multilingual mode:

```bash
cargo run -p dmn-cli -- run \
  --replay testdata/es/interviewer.wav \
  testdata/es/candidate.wav \
  --language auto --model small
```

Verify that the `es→en` transcript continues while assistance is disabled.

## Tests to add

- config defaults assistance to enabled
- TUI key handling toggles assistance
- snapshot: normal assist-on layout
- snapshot: assist-off transcript-first layout
- pipeline test: assist off produces no detector candidates
- pipeline test: assist off produces no provider requests
- pipeline test: assist on again resumes detection for later segments
- multilingual replay test: translated transcript still flows with assist off

## Additions 2026-09-28 (from testing #77; also a comment on the GitHub issue)

Decisions taken today: a plain off switch (no mode preset), a transcript-first screen while assistance is off, and freeing the GPU by asking Ollama to unload at start.

**Start-up switch beside the key.** `dmn run --no-assist` and `[assist].enabled = false` start a session with assistance off. The pipeline already has this path: `Options.no_llm` skips the suggester and the model warm-up, and `dmn run` just never sets it. The `a` key stays as specified.

**Why it matters for the GPU.** With the LLM unloaded the 4090 has about 24 GB free. Measured on the Spanish fixture and short French, German and Portuguese clips:

| whisper model | CPU per utterance | GPU per utterance | Spanish questions found |
| --- | --- | --- | --- |
| `small` | 0.8 s | 0.07 s | 12 of 13 |
| `medium` | 2.6 s | 0.13 s | 13 of 13 |

`medium` needs about 2.7 GB per lane, so both lanes fit once the LLM is gone, and it does not fit beside the local LLM (about 1.2 GB free). `large-v3-turbo` does not translate at all (fixed in PR #78).

**Unload Ollama at start when assistance is off.** Ollama keeps its model for 30 minutes after the last request, so dmn's switch alone does not free the card. When a session starts with assistance off and the provider is `ollama`, send one request with `keep_alive: 0` for the configured model before whisper loads; a failure is a warning, not an error. Remote providers need nothing.

**The runtime toggle cannot move whisper.** Its device is chosen when the model loads. So:
- Started with assistance off: whisper takes the GPU. Pressing `a` later loads the LLM into what is left, which is slow if whisper holds several GB. The status line should say so (`assist on · llm partly on cpu`) rather than fail.
- Started with assistance on: whisper is wherever it fitted beside the LLM. Pressing `a` stops the requests but whisper stays put, and the LLM is not unloaded mid-session.

**Suggested extra acceptance criteria**
- [ ] `dmn run --no-assist` starts with assistance off and makes no LLM request, warm-up included
- [ ] With assistance off at start and provider `ollama`, the configured model is unloaded before whisper loads, and both lanes report the GPU when it has room (`stt gpu`)
- [ ] Turning assistance on mid-session with whisper on the GPU works and the status line names any LLM fallback

One rule to amend when this lands: CLAUDE.md says to keep the TUI's three-area layout. The transcript-first screen is a second layout for assistance off, so that line needs the exception.
