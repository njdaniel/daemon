# #7 — dmn-vad: energy VAD + utterance segmenter, tests on fixtures

Labels: `size:M, model:opus, m1, lane:audio`

## Goal
Segment each source's stream into `Utterance`s: pause after 300 ms of silence, utterance end after 700 ms or at 12 s, using an energy + silence-gap VAD on 20 ms frames.
## Size / Model / Turns
M | Opus | 45   (from docs/PROCESS.md; labels must match)
## Scope
`crates/dmn-vad`: `Vad` trait (so Silero can replace it in M2), `EnergyVad`, `Segmenter`; thresholds from config `[stt].vad` section. Tests on `testdata/` fixtures and on synthetic tone/silence patterns.
Out of scope: everything else, including later issues.
## Depends on
#3
## Acceptance
- [ ] On the fixtures, utterance boundaries land within 200 ms of the scripted ones for ≥ 90 % of utterances
- [ ] Per-source VAD thread drains the ring buffer without falling behind at 1× (overrun counter stays 0)
- [ ] `just check`, `just test` and `just lint` green
## Verify
```
cargo test -p dmn-vad
```
