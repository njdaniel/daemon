# #144 — speak: a live voice-changer mode — your speech turned into the chosen voice while you talk (RVC)

Labels: `size:L, model:opus, m4, lane:audio, backlog`

Proposed 2026-11-02 while the owner tried #130's re-voice mode with a designed `girl` voice. Re-voice is speech → text → speech: whisper needs the whole sentence, so the voice only starts after a pause, about a second behind each sentence. The owner asked "why the voice would be after i stop vs just when im talking?" and asked for an issue for a real-time mode. Real-time voice conversion (RVC, Retrieval-based Voice Conversion) changes the sound of your speech chunk by chunk, about 0.1–0.3 s behind, keeping your timing, pauses and intonation, with no transcription in the path.

## Goal
A fourth voice mode, `live`: while it is on, the microphone goes into `dmn-voice` through an RVC model of the chosen voice, continuously, while you talk. The transcript still shows what you said. Re-voice and translate are unchanged.

## Size / Model / Turns
L | Opus | 90 (docs/PROCESS.md). A new streaming audio path (mic → converter → `dmn-voice`), a sidecar like `dmn-qwen3-tts`, a training command, and the mode wiring. If one run cannot finish it, split as in "If one run cannot finish it" below.

## Background (check before relying on it)
- RVC needs a model per target voice: a `.pth` (and usually an `.index` for timbre retrieval) trained from roughly 10+ minutes of the target's speech. Training on the 4090 is minutes to an hour depending on epochs.
- Inference takes your audio, extracts content features (HuBERT/ContentVec) and pitch (RMVPE is the usual choice), and resynthesises in the target voice. A pitch shift in semitones is applied to your pitch (+12 is the common male → female setting).
- Open-source implementations, all to be checked for licence, Linux/Docker support and a streaming (chunked) API: RVC-Project's Retrieval-based-Voice-Conversion-WebUI, w-okada/voice-changer (built for real-time; has a server), Applio. Pick by measured latency and quality on the dev box, not by reputation.
- Known weaknesses: artefacts and a robotic edge on fast speech, breaths and laughs; male → female conversion can slip back toward the speaker's own timbre; quality depends heavily on the training data.

## Scope
`apps/dmn-cli/src/speaker.rs`, `pipeline.rs`, `run.rs`, `speak.rs`, `doctor.rs`; `crates/dmn-audio` (the mic → converter → `dmn-voice` path); `crates/dmn-session/src/action.rs` and `state.rs` (the mode); `crates/dmn-core/src/config.rs` and its template; a sidecar under `sidecars/rvc/`; README, SPEC, docs/DEPENDENCIES.md.

### 1. Probe first (its result goes in the PR body)
Before wiring anything into dmn, measure on the dev box with one voice: mouth-to-output latency at 2–3 chunk sizes, GPU memory beside whisper medium on the GPU + Qwen3-TTS (`dmn-qwen3-tts`) + the translator, and quality by ear (the owner listens). Pick the implementation and chunk size from that. If no implementation gets under ~400 ms with acceptable quality, stop and report instead of building the rest.

### 2. The sidecar
- `dmn-rvc` in Docker like `dmn-qwen3-tts` (`sidecars/rvc/`), GPU, on `http://localhost:4126`, streaming over a WebSocket (or what the chosen implementation offers): PCM chunks in, converted PCM chunks out, a voice and pitch shift per session.
- Models live in `~/.local/share/dmn/rvc/<voice>/` (mounted read-only); the sidecar lists them.
- `[speak].rvc_url` (default `http://localhost:4126`), `[speak].rvc_pitch` (semitones, default 0; per voice if the picker needs it: decide in the PR).

### 3. Making a model for a voice (`dmn speak rvc train <voice>`)
- For a voice in the #130 library (designed or enrolled): generate the training set with the TTS itself (`dmn speak --voice <voice>` over a built-in set of varied sentences, ~10–15 min of speech), then train in the sidecar's image. Progress on the terminal; Ctrl+C stops the container and leaves no half-written model (as `dmn speak design` does).
- `dmn speak rvc import <voice> <model.pth> [<file.index>]` for a model the user already has.
- Consent: training from a #130 voice keeps that voice's consent tag (`dmn-designed` or the enrolment's). Importing asks the user to confirm they have the right to use the voice, as `enroll` does. No downloading of voice models from the internet.
- Training data and models are not audio recordings of the user; they are written regardless of `privacy.record_audio`. Nothing of the user's own speech is written to disk unless `privacy.record_audio` is true.

### 4. The live mode
- `VoiceMode::Live` beside `Passthrough`, `Translate`, `Revoice`; `[speak].mode = "live"` starts in it. `l` cycles own voice → translate → re-voice → live, and live is skipped when the chosen voice has no RVC model (the status line says why once). With #124 merged, push-to-talk is unchanged and does not apply to live (the owner does not want to hold a button for a voice change).
- The `o` picker marks which voices have an RVC model. Status tag `live: <voice>`.
- Audio path: the mic frames that would go to pass-through go instead to the converter, and its output goes into `dmn-voice`. The PipeWire callback allocates, locks and logs nothing: frames cross into a worker thread through a preallocated ring, and back the same way.
- The worker keeps the sidecar connection and a small jitter buffer. If the converter falls behind by more than a set amount (e.g. 1 s), the status line says so and the mode drops back to own voice rather than lagging the call. If the sidecar is unreachable at the switch, the mode does not change and the status line says why.
- Whisper still transcribes the raw mic, so the transcript and the session file are unchanged. The echo guard sees `dmn-voice` output as today.
- Log `live_latency_ms` (mic frame in → converted frame out of the worker) every few seconds.

### 5. Doctor and docs
- `dmn doctor`: the sidecar is reachable and lists models; the chosen voice has one when `mode = "live"`.
- README: what live mode is, how it differs from re-voice (instant but less clean; no translation), how to train or import a model, the GPU memory it takes. SPEC voice-mode paragraphs and key table. DEPENDENCIES.md for anything new on the Rust side (e.g. a WebSocket client).

Out of scope: translation in live mode, voice changing of the other person, training from recordings of real people other than the user, and a non-Docker install of the sidecar.

## If one run cannot finish it
(a) probe + sidecar + `import` + live mode with an imported model; (b) `dmn speak rvc train`. Each leaves `main` working; (b) needs (a).

## Depends on
#130 (PR #131): the voice library, `[speak].voice`, the `o` picker and re-voice in the `l` cycle.

## Acceptance
- [ ] Probe numbers (latency per chunk size, GPU memory, the implementation chosen and why) in the PR body
- [ ] `[speak].mode = "live"`, `rvc_url`, `rvc_pitch` parse and round-trip; `l` cycles the four modes and skips live without a model (`cargo test -p dmn-cli live`)
- [ ] With a mock converter, mic frames in live mode reach `dmn-voice` converted and none reach it raw; leaving live mode restores the previous gate; a converter that falls behind drops the mode to own voice with a status message (`cargo test -p dmn-cli live`)
- [ ] Nothing allocates, locks or logs in the PipeWire callback (review of the diff; the ring is preallocated)
- [ ] `dmn speak rvc import` and `train` write `~/.local/share/dmn/rvc/<voice>/`; Ctrl+C during training leaves no partial model and no running container
- [ ] `dmn doctor` reports an unreachable sidecar and a missing model for the chosen voice, each with its fix
- [ ] One live check on the dev box, by the owner: train or import a model for a voice, switch to live, talk in a Discord call or Mic Test; `live_latency_ms` from the log and the owner's verdict in the PR body
- [ ] README, SPEC, config template and DEPENDENCIES.md updated
- [ ] `just check`, `just test` and `just lint` pass

## Verify
```bash
cargo test -p dmn-core config
cargo test -p dmn-cli live
cargo test -p dmn-cli doctor
just check
just test
just lint
# live: dmn speak rvc train girl; dmn run --no-assist --app ""; l until "live: girl"; talk
```
CI has no GPU and no PipeWire; the sidecar and audio paths are mocked in tests and checked live on the dev box.
