# #60 — stt: echo guard — drop mic text that duplicates the loopback within 300 ms

Labels: `size:S, model:sonnet, m2, lane:stt`

## Goal
When both sources are active within the same 300 ms and the mic segment's text is a near-duplicate (ratio ≥ 0.8) of the loopback segment, drop the mic copy (SPEC § Audio capture, echo guard).
## Size / Model / Turns
S | Sonnet | 30   (from docs/PROCESS.md; labels must match)
## Scope
- One module where both lanes' finals meet (`crates/dmn-transcript` or `apps/dmn-cli/src/pipeline.rs`): hold a mic final briefly, compare it with Interviewer finals that overlap it in time, drop it on a match. `[audio].echo_guard` (exists, default true) turns it off.
- Dropped copies are counted in the run summary and `dmn bench --live`, never shown.
- Motivation: live session 5 (Meet, questions read from a phone in the same room) had every interviewer question duplicated into the You lane.
- An echo fixture: mic WAV = the interviewer WAV attenuated and delayed ~100 ms, plus a few real candidate lines (generated with `tools/fixtures`).
Out of scope: everything else, including later issues.
## Depends on
#15
## Acceptance
- [ ] On the echo fixture, duplicated lines are dropped and the real candidate lines kept
- [ ] With `echo_guard = false`, nothing is dropped
- [ ] Eval on the three M1 fixtures is unchanged (`just eval`)
- [ ] `just check`, `just test` and `just lint` green
## Verify
```
cargo test -p dmn-transcript -p dmn-cli echo
just eval
```
