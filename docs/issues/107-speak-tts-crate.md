# #107 — speak: dmn speak synthesizes text in your cloned voice from a local TTS server

Labels: `size:M, model:opus, m4, lane:audio`

Written 2026-10-05. First of three issues for the translate-and-speak feature (planned interactively, approved the same day): on a keypress the candidate's last utterance is translated by the local LLM and spoken to the interviewer in a clone of the candidate's own voice through a virtual microphone (#108), wired into the TUI in #109. This issue is the speech crate and its CLI, with no PipeWire and no TUI.

## Goal
A new `dmn-tts` crate talks to a locally-running OpenAI-compatible TTS server (Chatterbox-TTS-Server, default `http://localhost:4123`) to synthesize text in a voice cloned from the user's enrolled reference clip; `dmn speak enroll` registers the clip and `dmn speak "text" --out file.wav` writes synthesized speech to disk. Nothing plays audio yet.

## Size / Model / Turns
M | Opus | 45

## Decisions (from the plan)
- **Engine: Chatterbox (Resemble AI), MIT license**, zero-shot voice cloning from a short reference clip, Multilingual V3 covers 23+ languages, served by Chatterbox-TTS-Server's OpenAI-compatible API. Rejected: XTTS v2 (CPML non-commercial), F5-TTS (CC-BY-NC weights), Piper/Kokoro (no cloning), cloud TTS (breaks the local-only promise). The server is a sidecar the user runs, exactly like Ollama; no Python and no ML frameworks enter the workspace.
- Whisper is not involved: it only translates *to* English (#77), and this feature needs original-language text out (that is #109's translate step, not this crate's concern).
- The crate owns one channel: a bounded crossbeam of decoded PCM chunks (f32, with the server's sample rate attached) from the HTTP stream toward its caller. Playback is #108's.

## Scope

### New crate `crates/dmn-tts`
- `lib.rs` doc comment states input (text + voice reference id), output (PCM chunk stream), and the channel it owns, per repo rules.
- `TtsProvider` trait, object-safe like `dmn-llm`'s `Provider` (`provider.rs`): `synthesize(&self, req: SpeakRequest) -> BoxFuture<Result<PcmStream, TtsError>>`; `SpeakRequest { text, voice: String, language: Option<String> }`; `PcmStream` yields `PcmChunk { samples: Vec<f32>, sample_rate: u32 }`.
- `ChatterboxProvider`: reqwest (already in the workspace via dmn-llm's remote providers) against `speak.tts_url`; POSTs the OpenAI-compatible speech request with the enrolled voice, streams the WAV body back, decodes with hound (already in the workspace) into f32 chunks.
- `MockTtsProvider` for tests, returning a generated sine WAV, mirroring `dmn-llm`'s MockProvider.
- `thiserror` error enum (`TtsError`: connect, http status, decode, …); no `unwrap`/`expect` outside tests.
- Voice enrollment: `enroll(name, wav_path)` uploads/registers the reference clip with the server; the clip itself is stored under the data dir (default `~/.local/share/dmn/voice.wav`).

### Config (`crates/dmn-core/src/config.rs`)
- New `[speak]` section: `enabled = false`, `tts_url = "http://localhost:4123"`, `voice_ref` (path to the enrolled clip), `device = "cpu"` (a hint recorded for the server setup doc; the 35B Ollama model already takes 22 of 24 GB VRAM, so GPU TTS is opt-in). `target` (the translation language) is **not** here — it lands with #109, which uses it.
- TEMPLATE gains the commented section; the `every_spec_key_is_known` count is updated.

### CLI (`apps/dmn-cli/src/main.rs`)
- `dmn speak enroll --wav <path> [--name <n>]`: validates the WAV (16 kHz+ mono preferred, ≥ 6 s), copies it to the data dir, registers it with the server, writes `voice_ref` into the config.
- `dmn speak "text" [--out file.wav]`: synthesizes and writes the WAV. Without `--out` it errors with "playback lands in #108" — this issue ships no audio output, and main stays shippable.
- Both refuse to run when `[speak].enabled = false`, naming the config line.

### Docs
- `docs/DEPENDENCIES.md`: Chatterbox-TTS-Server (why: local voice cloning, MIT, OpenAI-compatible) plus any new crate deps.
- README: a Speak section with the server setup (docker or pip), `nvidia-smi` warning, enroll + speak examples.
- `docs/SPEC.md`: one sentence in § Privacy that synthesized text goes to a localhost server only.

## Out of scope
- PipeWire output / the virtual microphone (#108).
- Translation, the `s` key, TUI status, echo-guard window (#109).
- `device = "cuda"` server tuning beyond the doc warning.
- Auto-speak per utterance, queueing, barge-in (later).

## Depends on
None (touches no code from open issues).

## Acceptance
- [ ] `dmn-tts` compiles with the trait, the Chatterbox provider and the mock; its lib.rs doc comment names input, output and channel
- [ ] `[speak]` parses with defaults, unknown keys are config errors, TEMPLATE round-trips, key-count test updated
- [ ] `dmn speak enroll --wav clip.wav` validates, stores and registers the clip and sets `voice_ref`; `dmn speak "hola" --out /tmp/x.wav` writes a valid WAV (both tested against a mock server, not a real one)
- [ ] Both subcommands refuse with a clear error when `[speak].enabled = false` or the server is unreachable
- [ ] docs/DEPENDENCIES.md, README and SPEC § Privacy updated
- [ ] `just check`, `just test` and `just lint` green

## Verify
```bash
cargo test -p dmn-tts                      # trait + mock provider + WAV decode
cargo test -p dmn-core config              # [speak] defaults, round-trip, unknown key
cargo run -p dmn-cli -- speak "hola"       # errors: speak is disabled / no server
# against a real server on the dev box (reported in the PR, not gated):
cargo run -p dmn-cli -- speak enroll --wav testdata/es/candidate-sample.wav
cargo run -p dmn-cli -- speak "Buenas tardes, gracias por la oportunidad." --out /tmp/dmn-speak.wav
pw-play /tmp/dmn-speak.wav                 # sounds like the enrolled voice
```

## Tests to add
- provider: request shape (voice, text, streaming decode) against a hyper/axum mock server; error mapping for connect-refused, 4xx, malformed WAV
- config: defaults, round-trip, unknown key refused
- CLI: enroll validation (too short, stereo, not a WAV), speak --out writes a parseable WAV, disabled-gate errors
