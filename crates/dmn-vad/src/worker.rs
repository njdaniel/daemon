//! `VadWorker`: one thread per source that drains the ring, runs the VAD and the segmenter, and
//! sends events on a bounded channel without ever stalling the drain.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender, TrySendError};
use dmn_audio::{FRAME_LEN, Frames, Source};

use crate::segmenter::{Segmenter, SegmenterConfig, VadEvent};
use crate::{EnergyVad, Vad, VadConfig};

/// How long the worker sleeps when the ring is empty. A quarter of a frame: the ring holds 2 s,
/// so this never risks an overrun, and it keeps an idle source near 0 % CPU.
const IDLE_SLEEP: Duration = Duration::from_millis(5);

/// #132: how long a live lane may go without a frame before its clock is moved up to the wall
/// clock. Longer than the largest quantum a playback stream runs at (8192 frames at 48 kHz is
/// 0.17 s), so a lane whose frames keep arriving is never moved; and the lane must also be this
/// far behind, so a lane a few frames late is left alone.
pub const STALL: Duration = Duration::from_millis(500);

#[derive(Debug, thiserror::Error)]
pub enum VadError {
    #[error("cannot spawn the VAD thread: {0}")]
    Spawn(String),
    #[error("the VAD thread panicked")]
    Panicked,
}

/// Counters a worker reports when it ends; the live values are readable while it runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct WorkerStats {
    pub frames: u64,
    pub utterances: u64,
    /// Events dropped because the channel was full (the consumer stalled).
    pub dropped_events: u64,
    /// Frames the audio producer dropped because this worker fell behind.
    pub overruns: u64,
    /// #132: stream time a live lane skipped as silence because no frames came (its app's
    /// stream paused or closed), in milliseconds.
    pub skipped_ms: u64,
}

#[derive(Default)]
struct Counters {
    frames: AtomicU64,
    utterances: AtomicU64,
    dropped: AtomicU64,
    overruns: AtomicU64,
    skipped_ms: AtomicU64,
}

impl Counters {
    fn snapshot(&self) -> WorkerStats {
        WorkerStats {
            frames: self.frames.load(Ordering::Relaxed),
            utterances: self.utterances.load(Ordering::Relaxed),
            dropped_events: self.dropped.load(Ordering::Relaxed),
            overruns: self.overruns.load(Ordering::Relaxed),
            skipped_ms: self.skipped_ms.load(Ordering::Relaxed),
        }
    }
}

/// Bounded event channel sized for a worker. 64 events is minutes of speech; if the consumer
/// stalls longer than that, events are counted and dropped rather than blocking the drain.
pub fn event_channel(capacity: usize) -> (Sender<VadEvent>, Receiver<VadEvent>) {
    crossbeam_channel::bounded(capacity)
}

/// #132: the instant a live session's audio time 0 is, shared by its lanes and set once, when
/// both sources start. A lane's time is its frame count, but a capture linked to an app's
/// playback stream gets no frames while that stream is paused or closed; with a started clock
/// the worker counts such a gap as silence, so every lane stays on the same timeline as the
/// wall clock. Unset (or no clock: a replay), a lane's time is its frame count alone.
#[derive(Debug, Clone, Default)]
pub struct LaneClock(Arc<OnceLock<Instant>>);

impl LaneClock {
    pub fn new() -> Self {
        Self::default()
    }

    /// Audio time 0 is `zero`. Only the first call counts.
    pub fn start(&self, zero: Instant) {
        let _ = self.0.set(zero);
    }

    pub fn zero(&self) -> Option<Instant> {
        self.0.get().copied()
    }
}

/// Where a live lane's frame clock should be at `now`: the wall time since `zero`, when the lane
/// has had no frame for [`STALL`] and is more than [`STALL`] behind it.
fn catch_up(
    zero: Instant,
    last_frame: Option<Instant>,
    now: Instant,
    position: Duration,
) -> Option<Duration> {
    let quiet_since = last_frame.map_or(zero, |t| t.max(zero));
    let wall = now.saturating_duration_since(zero);
    (now.saturating_duration_since(quiet_since) >= STALL && wall > position + STALL).then_some(wall)
}

pub struct VadWorker {
    source: Source,
    stop: Arc<AtomicBool>,
    finish: Arc<AtomicBool>,
    counters: Arc<Counters>,
    handle: Option<JoinHandle<()>>,
}

impl VadWorker {
    /// Start the default energy VAD on `frames`, sending events to `tx`.
    pub fn spawn(frames: Frames, cfg: &VadConfig, tx: Sender<VadEvent>) -> Result<Self, VadError> {
        Self::spawn_with(
            frames,
            Box::new(EnergyVad::new(cfg.energy.clone())),
            &cfg.segmenter,
            tx,
        )
    }

    /// Start with any [`Vad`] implementation (Silero in M2, fakes in tests).
    pub fn spawn_with(
        frames: Frames,
        vad: Box<dyn Vad>,
        seg_cfg: &SegmenterConfig,
        tx: Sender<VadEvent>,
    ) -> Result<Self, VadError> {
        Self::spawn_inner(frames, vad, seg_cfg, tx, None)
    }

    /// #132: [`VadWorker::spawn_with`] for a live source: once `clock` is started, a gap with no
    /// frames counts as silence, so this lane's times stay on the wall clock.
    pub fn spawn_live(
        frames: Frames,
        vad: Box<dyn Vad>,
        seg_cfg: &SegmenterConfig,
        tx: Sender<VadEvent>,
        clock: LaneClock,
    ) -> Result<Self, VadError> {
        Self::spawn_inner(frames, vad, seg_cfg, tx, Some(clock))
    }

    fn spawn_inner(
        mut frames: Frames,
        mut vad: Box<dyn Vad>,
        seg_cfg: &SegmenterConfig,
        tx: Sender<VadEvent>,
        clock: Option<LaneClock>,
    ) -> Result<Self, VadError> {
        let source = frames.source();
        let stop = Arc::new(AtomicBool::new(false));
        let finish = Arc::new(AtomicBool::new(false));
        let counters = Arc::new(Counters::default());
        let mut seg = Segmenter::new(source, seg_cfg);

        let (t_stop, t_finish, t_counters) = (
            Arc::clone(&stop),
            Arc::clone(&finish),
            Arc::clone(&counters),
        );
        let handle = std::thread::Builder::new()
            .name(format!("dmn-vad-{}", source.label().to_lowercase()))
            .spawn(move || {
                let mut frame = [0.0f32; FRAME_LEN];
                let mut events = Vec::new();
                let mut last_frame = None;
                loop {
                    if t_stop.load(Ordering::Acquire) {
                        break;
                    }
                    if frames.pop_frame(&mut frame) {
                        let speech = vad.is_speech(&frame);
                        seg.push(&frame, speech, &mut events);
                        if clock.is_some() {
                            last_frame = Some(Instant::now());
                        }
                        t_counters.frames.fetch_add(1, Ordering::Relaxed);
                        t_counters
                            .overruns
                            .store(frames.overruns(), Ordering::Relaxed);
                        if !send_all(&tx, &mut events, &t_counters) {
                            return;
                        }
                        continue;
                    }
                    // Ring empty. `finish` means the producer is done: drain once more, then
                    // flush the open utterance and exit.
                    if t_finish.load(Ordering::Acquire) && frames.queued_frames() == 0 {
                        break;
                    }
                    let behind = clock.as_ref().and_then(LaneClock::zero).and_then(|zero| {
                        catch_up(zero, last_frame, Instant::now(), seg.position())
                    });
                    if let Some(wall) = behind {
                        let from = seg.position();
                        seg.skip_to(wall, &mut events);
                        let skipped = seg.position().saturating_sub(from);
                        tracing::info!(
                            lane = source.label(),
                            from_s = from.as_secs_f64(),
                            skipped_s = skipped.as_secs_f64(),
                            "no audio: lane clock moved up to the wall clock"
                        );
                        t_counters.skipped_ms.fetch_add(
                            u64::try_from(skipped.as_millis()).unwrap_or(u64::MAX),
                            Ordering::Relaxed,
                        );
                        if !send_all(&tx, &mut events, &t_counters) {
                            return;
                        }
                        continue;
                    }
                    std::thread::sleep(IDLE_SLEEP);
                }
                seg.flush(&mut events);
                t_counters
                    .overruns
                    .store(frames.overruns(), Ordering::Relaxed);
                send_all(&tx, &mut events, &t_counters);
            })
            .map_err(|e| VadError::Spawn(e.to_string()))?;

        Ok(Self {
            source,
            stop,
            finish,
            counters,
            handle: Some(handle),
        })
    }

    pub fn source(&self) -> Source {
        self.source
    }

    /// Live counters.
    pub fn stats(&self) -> WorkerStats {
        self.counters.snapshot()
    }

    /// The producer has finished (end of file): drain what is queued, flush the open
    /// utterance, and join.
    pub fn finish(mut self) -> Result<WorkerStats, VadError> {
        self.finish.store(true, Ordering::Release);
        self.join()
    }

    /// Stop now: flush the open utterance as `EndOfStream` and join, leaving queued frames.
    pub fn stop(mut self) -> Result<WorkerStats, VadError> {
        self.stop.store(true, Ordering::Release);
        self.join()
    }

    fn join(&mut self) -> Result<WorkerStats, VadError> {
        if let Some(h) = self.handle.take() {
            h.join().map_err(|_| VadError::Panicked)?;
        }
        Ok(self.counters.snapshot())
    }
}

impl Drop for VadWorker {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

/// Send queued events without blocking. Returns false when the receiver is gone.
fn send_all(tx: &Sender<VadEvent>, events: &mut Vec<VadEvent>, c: &Counters) -> bool {
    for ev in events.drain(..) {
        let is_utt = matches!(ev, VadEvent::Utterance(_));
        match tx.try_send(ev) {
            Ok(()) => {
                if is_utt {
                    c.utterances.fetch_add(1, Ordering::Relaxed);
                }
            }
            Err(TrySendError::Full(_)) => {
                c.dropped.fetch_add(1, Ordering::Relaxed);
            }
            Err(TrySendError::Disconnected(_)) => return false,
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use dmn_audio::frame_channel;

    fn loud() -> [f32; FRAME_LEN] {
        [0.1; FRAME_LEN]
    }

    #[test]
    fn worker_segments_frames_and_flushes_on_finish() {
        let (mut sink, frames) = frame_channel(Source::You, 400);
        for _ in 0..25 {
            sink.push_frame(&[0.0005; FRAME_LEN]);
        }
        for _ in 0..50 {
            sink.push_frame(&loud());
        }
        let (tx, rx) = event_channel(64);
        let w = VadWorker::spawn(frames, &VadConfig::default(), tx).unwrap();
        let stats = w.finish().unwrap();
        let utts: Vec<_> = rx
            .try_iter()
            .filter_map(|e| match e {
                VadEvent::Utterance(u) => Some(u),
                _ => None,
            })
            .collect();
        assert_eq!(utts.len(), 1);
        assert_eq!(utts[0].source, Source::You);
        assert_eq!(utts[0].start, Duration::from_millis(500));
        assert_eq!(utts[0].closed_by, crate::CloseReason::EndOfStream);
        assert_eq!(stats.frames, 75);
        assert_eq!(stats.utterances, 1);
        assert_eq!(stats.overruns, 0);
    }

    #[test]
    fn a_full_channel_drops_and_counts_events_without_stalling_the_drain() {
        let (mut sink, frames) = frame_channel(Source::Them, 2000);
        // Ten 0.5 s utterances separated by 1 s of quiet: 30 events (start, pause, utterance).
        for _ in 0..10 {
            for _ in 0..25 {
                sink.push_frame(&loud());
            }
            for _ in 0..50 {
                sink.push_frame(&[0.0005; FRAME_LEN]);
            }
        }
        let (tx, rx) = event_channel(4); // nobody reads until the end
        let w = VadWorker::spawn(frames, &VadConfig::default(), tx).unwrap();
        let stats = w.finish().unwrap();
        assert_eq!(stats.frames, 750, "every frame was drained");
        assert_eq!(rx.try_iter().count(), 4);
        assert_eq!(stats.dropped_events, 30 - 4);
    }

    fn utterances(rx: &Receiver<VadEvent>) -> Vec<crate::Utterance> {
        rx.try_iter()
            .filter_map(|e| match e {
                VadEvent::Utterance(u) => Some(u),
                _ => None,
            })
            .collect()
    }

    fn energy() -> Box<dyn Vad> {
        Box::new(EnergyVad::new(VadConfig::default().energy))
    }

    /// Poll `stats` until `done` holds, for at most 3 s.
    fn wait_for(w: &VadWorker, done: impl Fn(&WorkerStats) -> bool) -> WorkerStats {
        let until = Instant::now() + Duration::from_secs(3);
        loop {
            let s = w.stats();
            if done(&s) || Instant::now() > until {
                return s;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn wall_clock_catch_up_needs_a_quiet_stall_and_a_lane_behind() {
        let zero = Instant::now();
        let at = |ms| zero + Duration::from_millis(ms);
        let pos = Duration::from_millis;
        // Ten seconds in, no frame ever: up to the wall clock.
        assert_eq!(catch_up(zero, None, at(10_000), pos(0)), Some(pos(10_000)));
        // The last frame came 0.4 s ago: still within a quantum's jitter, left alone.
        assert_eq!(catch_up(zero, Some(at(9_600)), at(10_000), pos(0)), None);
        // Quiet long enough, but only 0.3 s behind: left alone.
        assert_eq!(
            catch_up(zero, Some(at(9_000)), at(10_000), pos(9_700)),
            None
        );
        // Ahead of the wall clock (a fast sound card, or frames buffered at start): never back.
        assert_eq!(catch_up(zero, None, at(10_000), pos(12_000)), None);
        // Quiet for 0.5 s since the clock started, with no frame before it.
        assert_eq!(catch_up(zero, Some(at(0)), at(400), pos(0)), None);
        assert_eq!(catch_up(at(100), None, at(700), pos(0)), Some(pos(600)));
    }

    #[test]
    fn wall_clock_a_silent_live_lane_catches_up_and_stamps_the_next_utterance_at_wall_time() {
        let (mut sink, frames) = frame_channel(Source::Them, 400);
        let (tx, rx) = event_channel(64);
        let clock = LaneClock::new();
        let zero = Instant::now()
            .checked_sub(Duration::from_secs(10))
            .expect("the clock is past 10 s");
        clock.start(zero);
        let w = VadWorker::spawn_live(frames, energy(), &VadConfig::default().segmenter, tx, clock)
            .unwrap();
        // The app has played nothing since the session started 10 s ago.
        let stats = wait_for(&w, |s| s.skipped_ms > 0);
        assert!(stats.skipped_ms >= 10_000, "{stats:?}");
        assert_eq!(stats.frames, 0);
        // Then it speaks for a second.
        for _ in 0..50 {
            sink.push_frame(&loud());
        }
        w.finish().unwrap();
        let u = utterances(&rx);
        assert_eq!(u.len(), 1);
        let start = u[0].start.as_secs_f64();
        assert!(
            (10.0..12.0).contains(&start),
            "at wall time, not 0: {start}"
        );
        assert_eq!(u[0].end - u[0].start, Duration::from_secs(1));
    }

    #[test]
    fn wall_clock_frames_that_keep_arriving_are_never_moved() {
        let (mut sink, frames) = frame_channel(Source::You, 400);
        let (tx, rx) = event_channel(64);
        let clock = LaneClock::new();
        // The lane is 5 s behind the wall clock but its frames keep coming: a running mic is
        // never moved, whatever its start-up delay.
        clock.start(
            Instant::now()
                .checked_sub(Duration::from_secs(5))
                .expect("the clock is past 5 s"),
        );
        // Its first frame is already there when the worker starts.
        sink.push_frame(&[0.0005; FRAME_LEN]);
        let w = VadWorker::spawn_live(frames, energy(), &VadConfig::default().segmenter, tx, clock)
            .unwrap();
        for i in 1..60 {
            sink.push_frame(&if (20..45).contains(&i) {
                loud()
            } else {
                [0.0005; FRAME_LEN]
            });
            std::thread::sleep(Duration::from_millis(20));
        }
        let stats = w.finish().unwrap();
        assert_eq!(stats.skipped_ms, 0, "{stats:?}");
        let u = utterances(&rx);
        assert_eq!(u.len(), 1);
        assert_eq!(u[0].start, Duration::from_millis(400), "frame-count time");
    }

    #[test]
    fn wall_clock_none_or_unstarted_never_skips() {
        for live in [false, true] {
            let (mut sink, frames) = frame_channel(Source::Them, 400);
            let (tx, rx) = event_channel(64);
            let seg = VadConfig::default().segmenter;
            // A replay, or a live lane before the session's sources have started.
            let w = if live {
                VadWorker::spawn_live(frames, energy(), &seg, tx, LaneClock::new())
            } else {
                VadWorker::spawn_with(frames, energy(), &seg, tx)
            }
            .unwrap();
            std::thread::sleep(STALL + Duration::from_millis(200));
            for _ in 0..50 {
                sink.push_frame(&loud());
            }
            let stats = w.finish().unwrap();
            assert_eq!(stats.skipped_ms, 0, "live: {live}");
            assert_eq!(utterances(&rx)[0].start, Duration::ZERO, "live: {live}");
        }
    }

    #[test]
    fn stop_ends_a_worker_whose_producer_never_finishes() {
        let (_sink, frames) = frame_channel(Source::You, 10);
        let (tx, _rx) = event_channel(4);
        let w = VadWorker::spawn(frames, &VadConfig::default(), tx).unwrap();
        let t = std::time::Instant::now();
        w.stop().unwrap();
        assert!(t.elapsed() < Duration::from_millis(200));
    }

    #[test]
    fn a_dropped_receiver_ends_the_worker() {
        let (mut sink, frames) = frame_channel(Source::You, 400);
        let (tx, rx) = event_channel(4);
        drop(rx);
        let w = VadWorker::spawn(frames, &VadConfig::default(), tx).unwrap();
        for _ in 0..40 {
            sink.push_frame(&loud());
        }
        // finish() must not hang even though the receiver is gone.
        w.finish().unwrap();
    }
}
