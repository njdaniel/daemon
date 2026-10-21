# #132 — audio: a live lane's clock keeps up with the wall clock while its app is silent, so the other side's lines stay on screen

Labels: `bug, size:M, model:opus, m4, lane:audio`

Reported 2026-10-21 in a live test (`dmn run --no-assist`, `[audio].app_stream = "Google Chrome"`, a YouTube video): the owner said "when i playing youtube, the input was going on top.. and then didnt stay". YouTube's partial lines showed, then each final vanished the moment it arrived; only the owner's own lines stayed.

What happened (session 43 in the store): every Them final was transcribed and stored, but with times about 1,000 s behind the mic's. Segment 4357 (`them`, 2040–2044 s) arrived after segment 4356 (`you`, 3041–3043 s). The transcript keeps the last 60 s measured from the newest line's end (`TRANSCRIPT_WINDOW_S`, `Session::prune_transcript`), so a line that arrives 1,000 s "old" is pruned on arrival.

Cause: each lane's time is the count of 20 ms frames its VAD worker has seen (`Segmenter::n`, `crates/dmn-vad/src/segmenter.rs`). The mic sends frames all the time. A capture linked to an app's playback stream gets frames only while that stream runs: Chrome closes or corks its stream when the video is paused or nothing plays (a call app does the same when muted or between calls), and the capture is not yet linked at all if the app has not played since `dmn run` started. The lane's clock stops for every such gap, and the gaps add up. Everything that compares times across lanes is wrong after the first gap: the transcript window, the echo guard (#60, which matches a mic final to a Them final by time), Enter's "last turn" (#96), the fast phase's last 60 s, and the stored segment times.

## Goal
On a live run, both lanes' segment times are seconds since the same instant (the pipeline's `audio_zero`, which `AudioClock` already uses), whatever the app stream does. When a lane has had no frame for a while and its frame clock is behind the wall clock, its clock jumps forward to the wall clock, as if the gap had been silence. A replay is unchanged: a WAV lane's time is its frame count, as now.

## Size / Model / Turns
M | Opus | 60

## Scope
- `crates/dmn-vad/src/segmenter.rs`: `Segmenter::skip_to(at, out)` advances the stream to `at` as silence. An open utterance hears silence frames until it closes (or `at` is reached), so it ends exactly as it would on real silence; then the position jumps, and the pre-roll is emptied (it would be audio from before the gap).
- `crates/dmn-vad/src/worker.rs`: a `LaneClock` (shared, set once with the instant audio time 0 is) given to a live lane's worker. With the ring empty, the clock set, no frame for `STALL` (0.5 s, longer than the largest PipeWire quantum a playback stream uses), and the frame clock more than `STALL` behind the wall clock, the worker calls `skip_to(wall)`. Frames that keep arriving never trigger it, so a lane that runs continuously (the mic) keeps its sample-exact time. `WorkerStats` gains the gap time skipped.
- `apps/dmn-cli/src/pipeline.rs`: live lanes get one shared `LaneClock`, started at `audio_zero`; file lanes get none.
- SPEC § Audio capture on PipeWire: one paragraph on lane time.

Out of scope: a different transcript window (once both lanes share a clock the 60 s window is right); the Them lane's partial lines.

## Depends on
none

## Acceptance
- [ ] `skip_to` closes an open utterance as silence would and moves the position; a later onset is stamped at the new position (`cargo test -p dmn-vad skip`)
- [ ] A live worker whose ring stays empty catches up to the wall clock, and the next utterance starts at wall time, not at the frame count (`cargo test -p dmn-vad wall_clock`)
- [ ] A worker whose frames keep arriving is not moved, and a worker without a clock (replay) never skips (`cargo test -p dmn-vad wall_clock`)
- [ ] `dmn run` gives both live lanes the same clock, started at `audio_zero`; replay lanes get none
- [ ] `just check`, `just test` and `just lint` green

## Verify
```bash
cargo test -p dmn-vad skip
cargo test -p dmn-vad wall_clock
cargo test -p dmn-cli pipeline
```
