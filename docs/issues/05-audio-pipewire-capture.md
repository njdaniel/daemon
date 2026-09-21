# #5 — dmn-audio: `PipeWireSource` capture stream with `target.object`, sink-monitor fallback, `dmn doctor --probe`

Labels: `size:L, model:fable, m0, lane:audio`

## Goal
Capture one source (mic node, or one app's output node via `target.object`) as 16 kHz mono f32 with the properties in docs/SPEC.md § Audio capture on PipeWire, falling back to the default sink monitor with a visible badge.
## Size / Model / Turns
L | Fable | 60 × 2   (from docs/PROCESS.md; labels must match)
## Scope
`crates/dmn-audio`: `PipeWireSource` implementing `AudioSource`; stream params (F32LE, 16 kHz, mono, 10 ms quantum); `AUTOCONNECT | MAP_BUFFERS | RT_PROCESS`; disconnected / reconnect state on global-removed and re-added by `application.name`; `apps/dmn-cli` `doctor --probe` (plays a tone via `pw-play`, captures it back, reports pass/fail). Emergency `pw-record` subprocess source behind the same trait.
Out of scope: everything else, including later issues.
## Depends on
#4
## Acceptance
- [ ] `doctor --probe` captures the tone through a `Stream/Output/Audio` target on the dev machine's PipeWire
- [ ] Process callback has no allocation, locks or logging (reviewed; comment at the top of the callback)
- [ ] App stream disappearing sets the source to disconnected and the mic keeps running; reappearing reconnects
- [ ] `just check`, `just test` and `just lint` green
## Verify
```
cargo run -p dmn-cli -- doctor --probe
just probe
```
