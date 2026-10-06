# #108 — speak: the cloned voice plays into a PipeWire virtual microphone the call hears

Labels: `size:L, model:fable, m4, lane:audio`

Written 2026-10-05. Second issue of the translate-and-speak feature (planned 2026-10-05 with #107 and #109). #107 gave `dmn speak` a voice on disk; this issue puts that voice on a microphone the interview call can select.

## Goal
`dmn speak "text"` (no `--out`) plays the synthesized speech into a PipeWire **virtual source** named `dmn-voice`; a browser/Meet/Zoom call that uses `dmn-voice` as its microphone transmits it to the interviewer. The speaker's own headphones/speakers stay silent.

## Size / Model / Turns
L | Fable | 60, two runs planned

## Scope

### `crates/dmn-audio/src/vmic.rs` (new module)
- A PipeWire stream created with `media.class = Audio/Source` (virtual) and `node.name = "dmn-voice"`, so conference apps list it as a microphone. Follows the existing PipeWire code in dmn-audio for context/thread-loop setup; `unsafe` stays inside dmn-audio with a `// SAFETY:` comment per block (repo rule).
- Fed from the PCM channel #107's crate hands over: a bounded ring buffer the process callback reads. **The callback allocates nothing, locks nothing, logs nothing** (repo rule) — underrun emits silence, never blocks the graph.
- Resample the TTS output (Chatterbox emits 24 kHz) to the graph rate (48 kHz on the dev box) in the feeding thread, never in the callback. A simple linear resampler inside dmn-audio is fine for speech; no new resampling crate without a DEPENDENCIES.md line.
- The node exists only while speaking (connect on `speak`, disconnect after), so the virtual mic does not linger in the user's audio settings between runs. If keeping it alive for the whole `dmn run` session proves more reliable for call setup, that is acceptable — record the choice in the PR.
- Mono f32 throughout, matching the existing lanes.

### CLI (`apps/dmn-cli/src/main.rs`)
- `dmn speak "text"` without `--out` synthesizes (#107) and plays into `dmn-voice`; with `--out` it keeps writing the WAV (unchanged).
- Playback errors (no PipeWire, node connect failed) are reported, not panicked.

### Docs
- README Speak section: select `dmn-voice` as the microphone in the call; how to verify with `pw-dump` / the desktop's audio panel.
- docs/SPEC.md § Audio: one row for the virtual source.

## Found while building #107 (added 2026-10-06, before this issue starts)
- **Speed is the problem to design around.** The real server (travisvn/chatterbox-tts-api, CPU, the only way it runs on the dev box today: Docker has no NVIDIA runtime and the LLM holds ~22 of 24 GB VRAM) produced 3.8 s of Spanish in 11.2 s, and on its streaming route the first audio arrived after 11.4 s. A sentence reaches the call 11–15 s after it is asked for. This issue should (a) play from the streaming route (`POST /v1/audio/speech/stream`, already in `dmn-tts` as `.streaming()`) so playback starts as soon as the first chunk decodes rather than after the whole WAV, (b) report in the PR the time from `speak` to first audible sample, and (c) note in README how to run the server on the GPU (its `docker-compose.gpu.yml`) for anyone with VRAM to spare. Making it near real time is not this issue's job; measuring it is.
- **The server's WAV is 32-bit float, 24 kHz, mono, with `fact` and `PEAK` chunks**, and the streaming route writes a `0xFFFFFFFF` data length. #107's decoder handles all of that; feed the vmic from `PcmChunk`s, never re-parse the WAV here.
- **An unknown voice name is spoken in the default voice with a 200.** #107 checks `GET /v1/voices` before every synthesis; keep that path, do not bypass it.
- **The spec principle must be reworded first.** README line 7 and the SPEC's principles say Daemon does not "type or speak on the user's behalf". This issue is the first to put Daemon's audio into a call, so the PR rewords both to what is now true: Daemon never speaks unprompted and never speaks words the user did not say or ask for; with `[speak]` on, it speaks the user's own words (or their translation, #109) in the user's own cloned voice, only when the user presses the key. Say so in the PR.

## Out of scope
- Translation, the `s` key, TUI (#109).
- Playing to speakers as well (monitoring) — later if wanted.
- Auto-ducking the real mic while speaking (#109's echo window covers the app's own hearing; hardware ducking is out).
- Keeping `dmn-voice` alive across daemon restarts as a system service.

## Depends on
#107 (merged)

## Acceptance
- [ ] `dmn speak "Buenas tardes."` on the dev box creates `dmn-voice`, the call transmits the cloned voice, and the node disappears or idles silently afterwards (verified by the commands below; PipeWire tests are `#[ignore]` + `just probe` per repo rules, CI has no PipeWire)
- [ ] The process callback is allocation/lock/log free; underrun yields silence, not a stall
- [ ] 24 kHz TTS output plays at the right speed and pitch at 48 kHz graph rate
- [ ] `dmn speak --out` still writes the identical WAV as #107 (no regression)
- [ ] Without PipeWire the error names the problem; nothing panics
- [ ] README and SPEC updated
- [ ] `just check`, `just test` and `just lint` green

## Verify
```bash
just check && just test && just lint
just probe                                  # dev box: PipeWire-gated tests
# live on the dev box (reported in the PR, not gated):
cargo run -p dmn-cli -- speak "Gracias por la oportunidad." &
pw-record --target dmn-voice /tmp/vmic.wav  # record the virtual mic's output
pw-play /tmp/vmic.wav                       # intelligible, right speed, cloned voice
pw-dump | grep -c dmn-voice                 # node present while speaking
```

## Tests to add
- ring-buffer: underrun emits silence; overrun drops oldest with a counter (pure Rust, no PipeWire)
- resampler: 24 kHz → 48 kHz sine keeps its frequency; length doubles within tolerance
- `#[ignore]`d PipeWire test: speak a known WAV, record from `dmn-voice` with pw-record, assert RMS above silence and duration within 10 %
