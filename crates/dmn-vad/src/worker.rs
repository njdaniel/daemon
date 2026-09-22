//! `VadWorker`: one thread per source that drains the ring, runs the VAD and the segmenter, and
//! sends events on a bounded channel without ever stalling the drain.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;

use crossbeam_channel::{Receiver, Sender, TrySendError};
use dmn_audio::{FRAME_LEN, Frames, Source};

use crate::segmenter::{Segmenter, SegmenterConfig, VadEvent};
use crate::{EnergyVad, Vad, VadConfig};

/// How long the worker sleeps when the ring is empty. A quarter of a frame: the ring holds 2 s,
/// so this never risks an overrun, and it keeps an idle source near 0 % CPU.
const IDLE_SLEEP: Duration = Duration::from_millis(5);

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
}

#[derive(Default)]
struct Counters {
    frames: AtomicU64,
    utterances: AtomicU64,
    dropped: AtomicU64,
    overruns: AtomicU64,
}

impl Counters {
    fn snapshot(&self) -> WorkerStats {
        WorkerStats {
            frames: self.frames.load(Ordering::Relaxed),
            utterances: self.utterances.load(Ordering::Relaxed),
            dropped_events: self.dropped.load(Ordering::Relaxed),
            overruns: self.overruns.load(Ordering::Relaxed),
        }
    }
}

/// Bounded event channel sized for a worker. 64 events is minutes of speech; if the consumer
/// stalls longer than that, events are counted and dropped rather than blocking the drain.
pub fn event_channel(capacity: usize) -> (Sender<VadEvent>, Receiver<VadEvent>) {
    crossbeam_channel::bounded(capacity)
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
        mut frames: Frames,
        mut vad: Box<dyn Vad>,
        seg_cfg: &SegmenterConfig,
        tx: Sender<VadEvent>,
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
                loop {
                    if t_stop.load(Ordering::Acquire) {
                        break;
                    }
                    if frames.pop_frame(&mut frame) {
                        let speech = vad.is_speech(&frame);
                        seg.push(&frame, speech, &mut events);
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
        let (mut sink, frames) = frame_channel(Source::Interviewer, 2000);
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
