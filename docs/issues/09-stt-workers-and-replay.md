# #9 — dmn-stt: per-source worker thread, partials, hallucination filter; `dmn replay` prints labelled segments

Labels: `size:L, model:fable, m1, lane:stt`

## Goal
Run one whisper worker per source fed by a bounded channel, emit 1 s partials from an 8 s sliding window and a final pass per utterance, drop hallucinated segments, and make `dmn replay` print timestamped, labelled segments.
## Size / Model / Turns
L | Fable | 60 × 2   (from docs/PROCESS.md; labels must match)
## Scope
`crates/dmn-stt` worker; `crates/dmn-transcript` `Segment` and bus; `apps/dmn-cli` `replay` (two WAVs, `--speed` factor). `crossbeam-channel` for the utterance queue.
Out of scope: everything else, including later issues.
## Depends on
#8
## Acceptance
- [ ] `dmn replay` on the sysdesign fixture prints segments labelled Interviewer / You with timestamps
- [ ] Known hallucination phrases on silence are dropped; count reported at the end
- [ ] Final pass replaces all partials for the utterance (no duplicate text in the log)
- [ ] `just check`, `just test` and `just lint` green
## Verify
```
cargo run -p dmn-cli -- replay testdata/sysdesign/interviewer.wav testdata/sysdesign/candidate.wav --speed 4
```
