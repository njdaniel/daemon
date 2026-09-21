# #3 — dmn-audio: `AudioSource` trait, `WavSource`, `ringbuf` plumbing, resampler; unit tests on fixtures

Labels: `size:M, model:opus, m0, lane:audio`

## Goal
Define `AudioSource` and ship a `WavSource` that feeds 16 kHz mono f32 through an SPSC `ringbuf` in 20 ms frames, so every later stage can be tested from files.
## Size / Model / Turns
M | Opus | 45   (from docs/PROCESS.md; labels must match)
## Scope
`crates/dmn-audio`: `AudioSource` trait (start / stop / label / sample sender), `WavSource` (`hound`), resampler (`rubato`) for non-16 kHz input, `ringbuf` plumbing, overrun counter. Unit tests use short synthetic WAVs generated in the test (no fixtures required yet).
Out of scope: everything else, including later issues.
## Depends on
#2
## Acceptance
- [ ] A 48 kHz stereo WAV is delivered as 16 kHz mono f32 within 0.1 dB of a reference downmix
- [ ] Ring buffer overrun drops the oldest 20 ms and increments the counter; never blocks the producer
- [ ] `WavSource` can run at 1× wall-clock or as fast as possible
- [ ] `just check`, `just test` and `just lint` green
## Verify
```
cargo test -p dmn-audio
```
