//! One whisper worker thread per source.
//!
//! It owns a [`Transcriber`] (one `WhisperState`, so inference is serialised per source), reads
//! [`VadEvent`]s from that source's bounded channel, and publishes [`Segment`]s: a partial每
//! second while an utterance is open, then the final pass over the closed utterance.
//!
//! Keeping up: whisper is slower than the 1 s partial cadence on a busy CPU, so before each
//! inference the worker drains whatever else is queued and keeps only the newest partial of an
//! utterance — and drops its partials entirely once that utterance's final has arrived. Stale
//! partials are counted, never transcribed.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, RecvTimeoutError};
use dmn_transcript::{Segment, normalise};
use dmn_vad::{Utterance, VadEvent};

use crate::filter::{Dropped, FilterConfig, should_drop};
use crate::{SttError, Transcriber};

/// How long the worker blocks on the channel before checking the stop flag.
const POLL: Duration = Duration::from_millis(50);
/// Upper bound on events pulled in one coalescing pass, so a flood cannot starve inference.
const MAX_BATCH: usize = 256;

#[derive(Debug, Clone, PartialEq)]
pub struct SttWorkerConfig {
    /// Transcribe [`VadEvent::Partial`]s as well as closed utterances.
    pub partials: bool,
    pub filter: FilterConfig,
}

impl Default for SttWorkerConfig {
    fn default() -> Self {
        Self {
            partials: true,
            filter: FilterConfig::default(),
        }
    }
}

/// Counters the CLI and the status bar read while a worker runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SttStats {
    pub finals: u64,
    pub partials: u64,
    /// Partials skipped because a newer partial or the final had already arrived.
    pub superseded: u64,
    pub dropped_empty: u64,
    pub dropped_low_confidence: u64,
    pub dropped_phrase: u64,
    pub errors: u64,
    /// Total time inside `whisper_full`, milliseconds.
    pub inference_ms: u64,
    /// Slowest single inference, milliseconds.
    pub slowest_ms: u64,
}

impl SttStats {
    pub fn dropped(&self) -> u64 {
        self.dropped_empty + self.dropped_low_confidence + self.dropped_phrase
    }
}

#[derive(Default)]
struct Counters {
    finals: AtomicU64,
    partials: AtomicU64,
    superseded: AtomicU64,
    empty: AtomicU64,
    low_confidence: AtomicU64,
    phrase: AtomicU64,
    errors: AtomicU64,
    inference_ms: AtomicU64,
    slowest_ms: AtomicU64,
}

impl Counters {
    fn snapshot(&self) -> SttStats {
        SttStats {
            finals: self.finals.load(Ordering::Relaxed),
            partials: self.partials.load(Ordering::Relaxed),
            superseded: self.superseded.load(Ordering::Relaxed),
            dropped_empty: self.empty.load(Ordering::Relaxed),
            dropped_low_confidence: self.low_confidence.load(Ordering::Relaxed),
            dropped_phrase: self.phrase.load(Ordering::Relaxed),
            errors: self.errors.load(Ordering::Relaxed),
            inference_ms: self.inference_ms.load(Ordering::Relaxed),
            slowest_ms: self.slowest_ms.load(Ordering::Relaxed),
        }
    }

    fn count_drop(&self, why: Dropped) {
        match why {
            Dropped::Empty => &self.empty,
            Dropped::LowConfidence => &self.low_confidence,
            Dropped::Phrase => &self.phrase,
        }
        .fetch_add(1, Ordering::Relaxed);
    }

    fn timed(&self, ms: u64) {
        self.inference_ms.fetch_add(ms, Ordering::Relaxed);
        self.slowest_ms.fetch_max(ms, Ordering::Relaxed);
    }
}

/// Anything that can turn samples into text: the real [`Transcriber`], or a fake in tests.
pub trait Speech: Send {
    fn transcribe(
        &mut self,
        samples: &[f32],
        prompt: Option<&str>,
    ) -> Result<crate::Transcript, SttError>;
}

impl Speech for Transcriber {
    fn transcribe(
        &mut self,
        samples: &[f32],
        prompt: Option<&str>,
    ) -> Result<crate::Transcript, SttError> {
        Transcriber::transcribe(self, samples, prompt)
    }
}

pub struct SttWorker {
    stop: Arc<AtomicBool>,
    counters: Arc<Counters>,
    handle: Option<JoinHandle<()>>,
}

impl SttWorker {
    /// Start a worker over `rx`, publishing through `publish`. The thread ends when the sender
    /// side closes (end of stream) or [`SttWorker::stop`] is called.
    pub fn spawn(
        speech: Box<dyn Speech>,
        rx: Receiver<VadEvent>,
        publish: Box<dyn FnMut(Segment) + Send>,
        cfg: SttWorkerConfig,
        name: &str,
    ) -> Result<Self, SttError> {
        let stop = Arc::new(AtomicBool::new(false));
        let counters = Arc::new(Counters::default());
        let mut job = Job {
            speech,
            publish,
            cfg,
            counters: Arc::clone(&counters),
            stop: Arc::clone(&stop),
            last_final: String::new(),
        };
        let handle = std::thread::Builder::new()
            .name(format!("dmn-stt-{name}"))
            .spawn(move || job.run(&rx))
            .map_err(|e| SttError::Io {
                path: format!("thread dmn-stt-{name}"),
                source: e,
            })?;
        Ok(Self {
            stop,
            counters,
            handle: Some(handle),
        })
    }

    pub fn stats(&self) -> SttStats {
        self.counters.snapshot()
    }

    /// Ask the worker to stop and wait for it, abandoning anything still queued.
    pub fn stop(mut self) -> SttStats {
        self.stop.store(true, Ordering::Release);
        self.join()
    }

    /// Wait for the worker to drain its channel and end (the sender must have been dropped).
    pub fn finish(mut self) -> SttStats {
        self.join()
    }

    fn join(&mut self) -> SttStats {
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
        self.counters.snapshot()
    }
}

impl Drop for SttWorker {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

struct Job {
    speech: Box<dyn Speech>,
    publish: Box<dyn FnMut(Segment) + Send>,
    cfg: SttWorkerConfig,
    counters: Arc<Counters>,
    stop: Arc<AtomicBool>,
    /// Text of the last final, passed to whisper as the prompt for continuity.
    last_final: String,
}

impl Job {
    fn run(&mut self, rx: &Receiver<VadEvent>) {
        let mut batch: Vec<VadEvent> = Vec::new();
        loop {
            if self.stop.load(Ordering::Acquire) {
                return;
            }
            match rx.recv_timeout(POLL) {
                Ok(ev) => batch.push(ev),
                Err(RecvTimeoutError::Timeout) => continue,
                Err(RecvTimeoutError::Disconnected) => return,
            }
            while batch.len() < MAX_BATCH {
                match rx.try_recv() {
                    Ok(ev) => batch.push(ev),
                    Err(_) => break,
                }
            }
            for ev in coalesce(std::mem::take(&mut batch), &self.counters) {
                if self.stop.load(Ordering::Acquire) {
                    return;
                }
                match ev {
                    Work::Final(u) => self.handle_final(u),
                    Work::Partial {
                        source,
                        start,
                        at,
                        samples,
                    } => self.handle_partial(source, start, at, &samples),
                }
            }
        }
    }

    fn handle_final(&mut self, u: Utterance) {
        let prompt = (!self.last_final.is_empty()).then(|| self.last_final.clone());
        let Some(t) = self.run_inference(&u.samples, prompt.as_deref()) else {
            return;
        };
        let text = normalise(&t.text);
        if let Some(why) = should_drop(&text, t.avg_logprob, t.n_tokens, &self.cfg.filter) {
            self.counters.count_drop(why);
            tracing::debug!(
                source = %u.source, start = ?u.start, reason = why.label(), text = %text,
                "dropped segment"
            );
            return;
        }
        self.counters.finals.fetch_add(1, Ordering::Relaxed);
        self.last_final = text.clone();
        (self.publish)(Segment {
            source: u.source,
            start: u.start,
            end: u.end,
            text,
            is_final: true,
            avg_logprob: t.avg_logprob,
        });
    }

    fn handle_partial(
        &mut self,
        source: dmn_audio::Source,
        start: Duration,
        at: Duration,
        samples: &[f32],
    ) {
        if !self.cfg.partials {
            return;
        }
        let prompt = (!self.last_final.is_empty()).then(|| self.last_final.clone());
        let Some(t) = self.run_inference(samples, prompt.as_deref()) else {
            return;
        };
        let text = normalise(&t.text);
        // Partials are provisional, so only the cheap checks apply: a partial that looks like a
        // hallucination simply is not shown, and the final decides.
        if let Some(why) = should_drop(&text, t.avg_logprob, t.n_tokens, &self.cfg.filter) {
            self.counters.count_drop(why);
            return;
        }
        self.counters.partials.fetch_add(1, Ordering::Relaxed);
        (self.publish)(Segment {
            source,
            start,
            end: at,
            text,
            is_final: false,
            avg_logprob: t.avg_logprob,
        });
    }

    fn run_inference(&mut self, samples: &[f32], prompt: Option<&str>) -> Option<crate::Transcript> {
        let t0 = Instant::now();
        let r = self.speech.transcribe(samples, prompt);
        self.counters.timed(t0.elapsed().as_millis() as u64);
        match r {
            Ok(t) => Some(t),
            Err(e) => {
                self.counters.errors.fetch_add(1, Ordering::Relaxed);
                tracing::warn!(error = %e, "transcription failed");
                None
            }
        }
    }
}

/// What the worker actually runs inference on.
enum Work {
    Final(Utterance),
    Partial {
        source: dmn_audio::Source,
        start: Duration,
        at: Duration,
        samples: Vec<f32>,
    },
}

/// Collapse a batch of events: finals in order, and at most one partial per utterance — the
/// newest, and only if that utterance has not already closed in this batch.
fn coalesce(batch: Vec<VadEvent>, counters: &Counters) -> Vec<Work> {
    let finalised: Vec<Duration> = batch
        .iter()
        .filter_map(|e| match e {
            VadEvent::Utterance(u) => Some(u.start),
            _ => None,
        })
        .collect();

    // Index of the last partial per utterance start.
    let mut newest: Vec<(Duration, usize)> = Vec::new();
    for (i, ev) in batch.iter().enumerate() {
        if let VadEvent::Partial { start, .. } = ev {
            match newest.iter_mut().find(|(s, _)| s == start) {
                Some(entry) => {
                    counters.superseded.fetch_add(1, Ordering::Relaxed);
                    entry.1 = i;
                }
                None => newest.push((*start, i)),
            }
        }
    }

    let mut work = Vec::new();
    for (i, ev) in batch.into_iter().enumerate() {
        match ev {
            VadEvent::Utterance(u) => work.push(Work::Final(u)),
            VadEvent::Partial {
                source,
                start,
                at,
                samples,
            } => {
                if finalised.contains(&start) {
                    counters.superseded.fetch_add(1, Ordering::Relaxed);
                    continue;
                }
                if newest.iter().any(|(s, idx)| *s == start && *idx == i) {
                    work.push(Work::Partial {
                        source,
                        start,
                        at,
                        samples,
                    });
                }
            }
            VadEvent::SpeechStart { .. } | VadEvent::Pause { .. } => {}
        }
    }
    work
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use dmn_audio::Source;
    use dmn_vad::CloseReason;

    use super::*;
    use crate::Transcript;

    /// A stand-in for whisper: returns the next scripted text, records prompts, and can be slow.
    struct Fake {
        texts: Vec<(String, f32, usize)>,
        calls: Arc<Mutex<Vec<(usize, Option<String>)>>>,
        delay: Duration,
    }

    impl Speech for Fake {
        fn transcribe(
            &mut self,
            samples: &[f32],
            prompt: Option<&str>,
        ) -> Result<Transcript, SttError> {
            std::thread::sleep(self.delay);
            let mut calls = self.calls.lock().unwrap();
            let i = calls.len();
            calls.push((samples.len(), prompt.map(str::to_owned)));
            let (text, avg_logprob, n_tokens) = self
                .texts
                .get(i)
                .cloned()
                .unwrap_or_else(|| ("...".to_owned(), -0.2, 3));
            Ok(Transcript {
                text,
                avg_logprob,
                low_confidence: avg_logprob < -1.0,
                n_tokens,
            })
        }
    }

    fn utterance(start_ms: u64, end_ms: u64) -> VadEvent {
        VadEvent::Utterance(Utterance {
            source: Source::Interviewer,
            start: Duration::from_millis(start_ms),
            end: Duration::from_millis(end_ms),
            samples: vec![0.0; 16_000],
            pre_roll: Duration::ZERO,
            closed_by: CloseReason::Silence,
        })
    }

    fn partial(start_ms: u64, at_ms: u64) -> VadEvent {
        VadEvent::Partial {
            source: Source::Interviewer,
            start: Duration::from_millis(start_ms),
            at: Duration::from_millis(at_ms),
            samples: vec![0.0; 16_000],
        }
    }

    struct Harness {
        worker: SttWorker,
        tx: crossbeam_channel::Sender<VadEvent>,
        out: Arc<Mutex<Vec<Segment>>>,
        calls: Arc<Mutex<Vec<(usize, Option<String>)>>>,
    }

    fn harness(texts: &[(&str, f32, usize)], delay: Duration) -> Harness {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let fake = Fake {
            texts: texts
                .iter()
                .map(|(t, l, n)| ((*t).to_owned(), *l, *n))
                .collect(),
            calls: Arc::clone(&calls),
            delay,
        };
        let out = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&out);
        let (tx, rx) = crossbeam_channel::bounded(64);
        let worker = SttWorker::spawn(
            Box::new(fake),
            rx,
            Box::new(move |s| sink.lock().unwrap().push(s)),
            SttWorkerConfig::default(),
            "test",
        )
        .unwrap();
        Harness {
            worker,
            tx,
            out,
            calls,
        }
    }

    #[test]
    fn a_closed_utterance_becomes_one_final_segment() {
        let h = harness(&[("  How would you design it? ", -0.2, 7)], Duration::ZERO);
        h.tx.send(utterance(1_000, 4_000)).unwrap();
        drop(h.tx);
        let stats = h.worker.finish();
        let out = h.out.lock().unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].text, "How would you design it?");
        assert!(out[0].is_final);
        assert_eq!(out[0].start, Duration::from_millis(1_000));
        assert_eq!(out[0].end, Duration::from_millis(4_000));
        assert_eq!((stats.finals, stats.partials, stats.dropped()), (1, 0, 0));
        assert!(stats.inference_ms < 5_000);
    }

    #[test]
    fn the_previous_final_is_the_next_prompt() {
        let h = harness(
            &[("First question.", -0.2, 3), ("Second question.", -0.2, 3)],
            Duration::ZERO,
        );
        h.tx.send(utterance(0, 2_000)).unwrap();
        h.tx.send(utterance(3_000, 5_000)).unwrap();
        drop(h.tx);
        h.worker.finish();
        let calls = h.calls.lock().unwrap();
        assert_eq!(calls[0].1, None, "nothing to prompt with yet");
        assert_eq!(calls[1].1.as_deref(), Some("First question."));
    }

    #[test]
    fn partials_stream_and_the_final_carries_the_same_start() {
        let h = harness(
            &[
                ("How would you", -0.2, 4),
                ("How would you design a rate limiter?", -0.2, 9),
            ],
            Duration::ZERO,
        );
        h.tx.send(partial(1_000, 2_000)).unwrap();
        // Send the final only once the partial has been consumed, so they are not coalesced.
        while h.out.lock().unwrap().is_empty() {
            std::thread::sleep(Duration::from_millis(5));
        }
        h.tx.send(utterance(1_000, 4_000)).unwrap();
        drop(h.tx);
        let stats = h.worker.finish();
        let out = h.out.lock().unwrap();
        assert_eq!(out.len(), 2);
        assert!(!out[0].is_final && out[1].is_final);
        assert_eq!(out[0].start, out[1].start, "same key, so the log replaces it");
        assert_eq!(out[0].end, Duration::from_millis(2_000));
        assert_eq!((stats.partials, stats.finals), (1, 1));
    }

    #[test]
    fn only_the_newest_partial_of_an_utterance_is_transcribed() {
        // Slow inference: everything else queues up behind the first call.
        let h = harness(&[("one", -0.2, 2); 6], Duration::from_millis(80));
        for (i, at) in [2_000u64, 3_000, 4_000, 5_000].iter().enumerate() {
            h.tx.send(partial(1_000, *at)).unwrap();
            if i == 0 {
                // Let the worker pick up the first one so the rest land in one batch.
                std::thread::sleep(Duration::from_millis(20));
            }
        }
        drop(h.tx);
        let stats = h.worker.finish();
        let out = h.out.lock().unwrap();
        assert_eq!(out.len(), 2, "the first, then the newest of the backlog");
        assert_eq!(out[1].end, Duration::from_millis(5_000));
        assert_eq!(stats.superseded, 2, "the two stale ones were never run");
    }

    #[test]
    fn a_partial_is_skipped_when_its_final_is_already_queued() {
        let h = harness(&[("done", -0.2, 2); 4], Duration::from_millis(60));
        h.tx.send(partial(0, 1_000)).unwrap();
        std::thread::sleep(Duration::from_millis(15));
        h.tx.send(partial(0, 2_000)).unwrap();
        h.tx.send(utterance(0, 3_000)).unwrap();
        drop(h.tx);
        let stats = h.worker.finish();
        let out = h.out.lock().unwrap();
        assert_eq!(out.len(), 2, "first partial, then the final: {out:?}");
        assert!(out[1].is_final);
        assert!(stats.superseded >= 1);
    }

    #[test]
    fn hallucinations_and_low_confidence_never_reach_the_transcript() {
        let h = harness(
            &[
                ("Thank you.", -0.2, 3),
                ("mumble mumble", -1.4, 4),
                ("   ", -0.2, 1),
                ("A real question?", -0.3, 4),
            ],
            Duration::ZERO,
        );
        for i in 0..4 {
            h.tx.send(utterance(i * 2_000, i * 2_000 + 1_000)).unwrap();
            // One at a time: finals are never coalesced, but this keeps the order obvious.
            std::thread::sleep(Duration::from_millis(5));
        }
        drop(h.tx);
        let stats = h.worker.finish();
        let out = h.out.lock().unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].text, "A real question?");
        assert_eq!(stats.dropped_phrase, 1);
        assert_eq!(stats.dropped_low_confidence, 1);
        assert_eq!(stats.dropped_empty, 1);
        assert_eq!(stats.finals, 1);
    }

    #[test]
    fn a_dropped_final_does_not_become_the_next_prompt() {
        let h = harness(
            &[("Thank you.", -0.2, 3), ("Real question?", -0.2, 3)],
            Duration::ZERO,
        );
        h.tx.send(utterance(0, 1_000)).unwrap();
        std::thread::sleep(Duration::from_millis(5));
        h.tx.send(utterance(2_000, 3_000)).unwrap();
        drop(h.tx);
        h.worker.finish();
        let calls = h.calls.lock().unwrap();
        assert_eq!(calls[1].1, None, "a dropped hallucination is not context");
    }

    #[test]
    fn stop_ends_a_worker_whose_sender_is_still_open() {
        let h = harness(&[("x", -0.2, 1)], Duration::ZERO);
        let t = Instant::now();
        let _stats = h.worker.stop();
        assert!(t.elapsed() < Duration::from_millis(500));
        drop(h.tx);
    }
}
