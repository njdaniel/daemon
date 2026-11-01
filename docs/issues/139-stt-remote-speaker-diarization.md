# #139 — stt: distinguish multiple remote speakers in the Them transcript

Labels: `size:L, model:fable, m4, lane:stt, backlog`

Requested 2026-10-24 during a translation-only Discord call: distinguish the different people speaking on the other side.

## Goal
When several people share the call's mixed playback stream, give their transcript turns consistent session-local labels such as Them 1 and Them 2, while the microphone remains You.

## Size / Model / Turns
L | Fable | 60, two runs planned (docs/PROCESS.md). One local diarization backend/API only. Reuse existing workers/runtime; no new capture thread or FFI. If the selected backend requires both new threading/FFI and UI work, split along that boundary before implementation as required by PROCESS §2.

## Scope
Optional local diarization of the existing Them lane, speaker metadata through transcript/session/store/export, small transcript label changes, fixture evaluation, configuration and documentation.

Today separate microphone and playback capture distinguish You from Them, but everyone in the mixed playback stream is Them. This is the M4 diarization fallback described in SPEC; the requested case is a remote group call.

- Select one local backend after a short fixture-based feasibility measurement. Record its model/version, license, setup, RAM/VRAM, attribution quality and added latency. Document any new dependency in DEPENDENCIES.
- Opt-in and off by default. Use the already captured audio in memory; do not record call audio to disk when `privacy.record_audio = false`, and do not send it to a cloud service.
- Assign stable IDs in first-seen order for a session. A speaker returning after someone else speaks or after silence keeps their ID. Do not guess people's real names.
- Keep capture source (them/you/unknown) separate from remote speaker identity, so question-source selection, echo handling and translate-my-voice keep their existing lane semantics.
- Show Them 1/Them 2 consistently in live transcript and persisted/exported turns. Existing sessions without speaker metadata still load and render as Them.
- Account for an utterance that contains a speaker change; do not attribute an entire mixed utterance to its first voice. Preserve timestamps and transcript text, and do not merge adjacent turns from different speakers.
- Uncertain, very short or overlapping speech may remain Them/unknown rather than receiving a confident invented identity. Transcription continues if the backend is unavailable or falls behind; display a concise status and document queue/timeout limits. Do not block microphone translation.
- Keep speaker profiles session-local; no cross-session voice recognition or enrollment.

Out of scope: separating/reconstructing overlapping audio, naming people, identifying a person outside the session, per-speaker translation voices, new call-platform integrations and the M3 window.

## Depends on
None beyond current main. #128 supplies neutral Them labels; #136 supplies readable saved transcripts. Transcript scrolling is an independent feature.

## Acceptance
- [ ] A checked-in/licensed two-speaker Them WAV with a reference manifest and a separate You fixture exercises A→B→A, silence, a mid-utterance change and a brief overlap.
- [ ] On the fixed non-overlapping evaluation regions, after matching anonymous IDs to reference speakers, at least 90% of scored speech duration has the correct speaker; score unknown as incorrect there, report coverage and exclude only the predeclared overlap regions.
- [ ] A returning voice retains its ID; uncertain/overlapping intervals use the documented fallback; You is never assigned a remote speaker ID.
- [ ] Speaker identity remains separate from source and survives session/store/text/JSONL export; an old session without it still loads, and cross-speaker finals are not merged.
- [ ] Default-off and backend-failure paths keep transcription and own-voice translation working; deterministic mock tests exercise timeout/backlog handling without a service or PipeWire.
- [ ] No call audio is written with recording disabled; document model/license/resources, setup, latency results and limitations.
- [ ] `just check`, `just test` and `just lint` pass.

## Verify
```bash
cargo test -p dmn-stt diarization
cargo test -p dmn-transcript
cargo test -p dmn-session
cargo test -p dmn-store
cargo test -p dmn-cli
just check
just test
just lint
```
Add a reproducible fixture-evaluation command to this issue/README when the one backend is selected; it must print attribution accuracy, coverage and added latency. Model-dependent tests may require a downloaded model explicitly; default CI uses fixtures/mocks and no microphone or PipeWire.

