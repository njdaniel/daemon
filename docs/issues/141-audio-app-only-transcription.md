# #141 — audio: transcribe only the selected app

Labels: `size:M, model:opus, m4, lane:audio, backlog`

Requested 2026-10-24 during an English-only Discord transcription session in WezTerm. The user wants only app audio transcribed. They clarified that the microphone may still be opened; avoiding microphone capture is not a requirement.

## Goal
Let the user select app-only transcription, so the live and saved transcript contains the selected application's speech without the user's microphone speech.

## Size / Model / Turns
M | Opus | 45 (docs/PROCESS.md). Reuse existing capture, VAD, STT and transcript paths; no new thread architecture, FFI or dependencies.

## Scope
A saved transcription-source option in `crates/dmn-core/src/config.rs` and its template; a live CLI override in `apps/dmn-cli/src/main.rs` / `run.rs`; transcription/transcript routing in `pipeline.rs`; minimal status indication; README and SPEC.

Current behavior: `--app` chooses the Them playback stream, but microphone speech is still transcribed as You. `--question-source them` only selects detector input and does not change the transcript.

### Selection and output
- Add an explicit app-only transcription option with a saved config setting and a CLI override. Proposed spelling: `[stt].app_only = false` and `dmn run --app-only`. Existing configurations keep both transcript lanes by default.
- Example: `dmn run --no-assist --app-only --app "WEBRTC VoiceEngine" --language en`. Discord's call playback on this machine identifies itself as WEBRTC VoiceEngine; document using `dmn devices` to find the actual application name.
- App-only mode shows and persists only the selected app's finalized transcript as Them. Microphone partials/finals must not appear in the live transcript, SQLite transcript rows, or text/JSONL transcript exports. Do not just hide You lines in the TUI while continuing to save them.
- Preserve app transcript timing, translation settings, normal shutdown draining, readable exports and `--no-store`.
- Clearly indicate that transcription is app-only. Do not label a microphone disconnected or broken merely because its speech is excluded.
- Keep app targeting and its existing waiting/reconnection behavior. Any existing all-system fallback remains visibly identified; this issue does not redesign capture fallback. Document that an explicitly empty app selector requests all system playback.

### Microphone capture remains independent
- Opening or retaining the microphone is allowed. This issue does not require eliminating its capture source, rings, workers or Whisper state, nor prove that the device is unopened.
- Existing mute, voice passthrough and translate-my-voice features may continue using microphone input when enabled. Their internal processing does not add microphone speech or microphone translation markers to an app-only transcript.
- Keep the transcription selection separate from assistance enabled/disabled and detector question-source selection. With assistance off, no helper request is made. Document how explicitly selected microphone assistance or voice features interact with excluding microphone speech from the transcript.
- Default two-lane transcription and existing replay behavior remain unchanged.

Out of scope: a guarantee that the microphone is never opened, a microphone capture-disable feature, microphone-only mode, a new source-picker UI, switching sources mid-session, remote-speaker diarization, changing Whisper models, storage locations or the M3 window.

## Depends on
None beyond current main. Existing assistance-off, question-source and saved-transcript features provide the baseline. #138 (transcript scrolling), #139 (remote-speaker diarization) and #140 (D&D helper) are independent.

## Acceptance
- [ ] Config and CLI select app-only transcription, with documented precedence; defaults retain both transcript lanes.
- [ ] Deterministic tests feed both app and mic speech and show only app partials/finals in app-only mode; default mode retains both.
- [ ] SQLite transcript rows and text/JSONL transcript exports contain app turns only, including the last finalized app turn after shutdown; --no-store writes nothing.
- [ ] Retaining mic capture or using an enabled microphone voice feature does not leak mic transcript text/translation markers into the live or saved transcript.
- [ ] Status identifies app-only transcription without claiming that microphone capture is off; assistance-off makes no helper request.
- [ ] App disappearance/reappearance, existing fallback indication and ordinary two-lane/replay behavior retain their current behavior.
- [ ] README, SPEC, CLI help and config template document the option, Discord example and independence from mic capture and question-source selection.
- [ ] `just check`, `just test` and `just lint` pass.

## Verify
```bash
cargo test -p dmn-core config
cargo test -p dmn-cli pipeline
cargo test -p dmn-cli run
cargo test -p dmn-cli tui
cargo test -p dmn-store
just check
just test
just lint
```
Use existing testdata WAVs, synthetic transcript events and MockProvider; default CI needs no microphone, PipeWire or live model service.

