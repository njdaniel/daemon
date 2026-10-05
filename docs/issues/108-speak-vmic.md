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
