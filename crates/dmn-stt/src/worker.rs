//! One whisper worker thread per source.
//!
//! It owns a [`Transcriber`] (one `WhisperState`, so inference is serialised per source), reads
//! [`VadEvent`]s from that source's bounded channel, and publishes [`Segment`]s: a partial every
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
    /// Text of the last punctuated final, passed to whisper as the prompt for continuity. See
    /// [`carries_punctuation`] for why an unpunctuated final is not.
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

    /// Close an utterance that produced no usable text, so its partials cannot sit on screen for
    /// ever: only a final with the same `start` clears them. Both no-text paths — a filtered
    /// final and a failed inference — go through here so they cannot drift apart.
    fn close_without_text(&mut self, u: &Utterance, avg_logprob: f32) {
        (self.publish)(Segment {
            source: u.source,
            start: u.start,
            end: u.end,
            text: String::new(),
            is_final: true,
            avg_logprob,
        });
    }

    fn handle_final(&mut self, u: Utterance) {
        let prompt = (!self.last_final.is_empty()).then(|| self.last_final.clone());
        let Some(t) = self.run_inference(&u.samples, prompt.as_deref()) else {
            // `run_inference` counted the error and logged the cause. The utterance still has to
            // close, and nothing about it was heard, so it carries the lowest possible score: no
            // consumer can mistake it for a confident empty result.
            tracing::warn!(
                source = %u.source, start = ?u.start, end = ?u.end,
                "inference failed; closing the utterance with no text"
            );
            self.close_without_text(&u, f32::MIN);
            return;
        };
        let text = normalise(&t.text);
        if let Some(why) = should_drop(&text, t.avg_logprob, t.n_tokens, &self.cfg.filter) {
            self.counters.count_drop(why);
            tracing::debug!(
                source = %u.source, start = ?u.start, reason = why.label(), text = %text,
                "dropped segment"
            );
            self.close_without_text(&u, t.avg_logprob);
            return;
        }
        self.counters.finals.fetch_add(1, Ordering::Relaxed);
        if carries_punctuation(&text) {
            self.last_final = text.clone();
        }
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

    fn run_inference(
        &mut self,
        samples: &[f32],
        prompt: Option<&str>,
    ) -> Option<crate::Transcript> {
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

/// Whether a final is fit to be the next prompt. whisper copies its prompt's style: a final that
/// came out with no sentence punctuation (whisper does that on a hurried or clipped utterance)
/// used as the prompt makes the next final unpunctuated too, and then the next, for the rest of
/// the lane. The detector needs the `?` and the sentence breaks, so one such final cost Silero
/// three behavioral questions on the fixtures (#62, #63). Such a final is still published; it
/// just does not replace the previous prompt.
fn carries_punctuation(text: &str) -> bool {
    text.chars().any(|c| matches!(c, '.' | '?' | '!'))
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use dmn_audio::Source;
    use dmn_vad::CloseReason;

    use super::*;
    use crate::Transcript;

    /// Samples handed to each call, and the prompt it carried.
    type Calls = Arc<Mutex<Vec<(usize, Option<String>)>>>;

    /// A stand-in for whisper: returns the next scripted text and records prompts. With a gate
    /// it announces each call and waits to be released, so a test can hold an inference open and
    /// queue events behind it without sleeping on the scheduler.
    struct Fake {
        texts: Vec<(String, f32, usize)>,
        calls: Calls,
        gate: Option<(
            crossbeam_channel::Sender<usize>,
            crossbeam_channel::Receiver<()>,
        )>,
    }

    impl Speech for Fake {
        fn transcribe(
            &mut self,
            samples: &[f32],
            prompt: Option<&str>,
        ) -> Result<Transcript, SttError> {
            if let Some((started, release)) = &self.gate {
                let n = self.calls.lock().unwrap().len();
                let _ = started.send(n);
                let _ = release.recv();
            }
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
        calls: Calls,
        /// Set when the harness was built with a gate: `started` yields one message per
        /// inference as it begins, `release` lets that inference finish.
        started: Option<crossbeam_channel::Receiver<usize>>,
        release: Option<crossbeam_channel::Sender<()>>,
    }

    impl Harness {
        /// Wait for the next inference to begin, then let it run.
        fn step(&self) {
            self.started
                .as_ref()
                .expect("gated harness")
                .recv_timeout(Duration::from_secs(5))
                .expect("an inference started");
            self.release
                .as_ref()
                .expect("gated harness")
                .send(())
                .expect("release it");
        }

        /// Wait for the next inference to begin and leave it blocked.
        fn hold(&self) {
            self.started
                .as_ref()
                .expect("gated harness")
                .recv_timeout(Duration::from_secs(5))
                .expect("an inference started");
        }

        /// Let a held inference finish.
        fn resume(&self) {
            self.release
                .as_ref()
                .expect("gated harness")
                .send(())
                .expect("release it");
        }
    }

    fn harness(texts: &[(&str, f32, usize)], gated: bool) -> Harness {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let (started_tx, started_rx) = crossbeam_channel::unbounded();
        let (release_tx, release_rx) = crossbeam_channel::unbounded();
        let fake = Fake {
            texts: texts
                .iter()
                .map(|(t, l, n)| ((*t).to_owned(), *l, *n))
                .collect(),
            calls: Arc::clone(&calls),
            gate: gated.then_some((started_tx, release_rx)),
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
            started: gated.then_some(started_rx),
            release: gated.then_some(release_tx),
        }
    }

    #[test]
    fn a_closed_utterance_becomes_one_final_segment() {
        let h = harness(&[("  How would you design it? ", -0.2, 7)], false);
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
            false,
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
            true,
        );
        h.tx.send(partial(1_000, 2_000)).unwrap();
        h.step(); // the partial is transcribed before the final is even queued
        h.tx.send(utterance(1_000, 4_000)).unwrap();
        h.step();
        drop(h.tx);
        let stats = h.worker.finish();
        let out = h.out.lock().unwrap();
        assert_eq!(out.len(), 2);
        assert!(!out[0].is_final && out[1].is_final);
        assert_eq!(
            out[0].start, out[1].start,
            "same key, so the log replaces it"
        );
        assert_eq!(out[0].end, Duration::from_millis(2_000));
        assert_eq!((stats.partials, stats.finals), (1, 1));
    }

    #[test]
    fn only_the_newest_partial_of_an_utterance_is_transcribed() {
        let h = harness(&[("one", -0.2, 2); 6], true);
        h.tx.send(partial(1_000, 2_000)).unwrap();
        // Hold that inference open and queue three more partials of the same utterance behind it.
        h.hold();
        for at in [3_000u64, 4_000, 5_000] {
            h.tx.send(partial(1_000, at)).unwrap();
        }
        h.resume();
        h.step(); // only one more inference runs: the newest of the three
        drop(h.tx);
        let stats = h.worker.finish();
        let out = h.out.lock().unwrap();
        assert_eq!(out.len(), 2, "the first, then the newest of the backlog");
        assert_eq!(out[1].end, Duration::from_millis(5_000));
        assert_eq!(stats.superseded, 2, "the two stale ones were never run");
    }

    #[test]
    fn a_partial_is_skipped_when_its_final_is_already_queued() {
        let h = harness(&[("done", -0.2, 2); 4], true);
        h.tx.send(partial(0, 1_000)).unwrap();
        h.hold();
        // While the first partial is in flight, both a newer partial and its final arrive.
        h.tx.send(partial(0, 2_000)).unwrap();
        h.tx.send(utterance(0, 3_000)).unwrap();
        h.resume();
        h.step(); // the final, not the superseded partial
        drop(h.tx);
        let stats = h.worker.finish();
        let out = h.out.lock().unwrap();
        assert_eq!(out.len(), 2, "first partial, then the final: {out:?}");
        assert!(out[1].is_final);
        assert_eq!(stats.superseded, 1, "the queued partial was never run");
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
            false,
        );
        // Finals are never coalesced, so they keep their order however they are batched.
        for i in 0..4 {
            h.tx.send(utterance(i * 2_000, i * 2_000 + 1_000)).unwrap();
        }
        drop(h.tx);
        let stats = h.worker.finish();
        let out = h.out.lock().unwrap();
        assert_eq!(
            out.len(),
            4,
            "every utterance closes, three of them with empty text"
        );
        let spoken: Vec<&Segment> = out.iter().filter(|s| !s.text.is_empty()).collect();
        assert_eq!(spoken.len(), 1);
        assert_eq!(spoken[0].text, "A real question?");
        assert!(
            out.iter().all(|s| s.is_final),
            "a dropped utterance still closes so its partials cannot linger"
        );
        assert_eq!(stats.dropped_phrase, 1);
        assert_eq!(stats.dropped_low_confidence, 1);
        assert_eq!(stats.dropped_empty, 1);
        assert_eq!(stats.finals, 1);
    }

    #[test]
    fn an_unpunctuated_final_does_not_replace_the_prompt() {
        let h = harness(
            &[
                ("Tell me about a time you disagreed.", -0.2, 7),
                ("and what did you do about it", -0.2, 6),
                ("How did it end?", -0.2, 4),
            ],
            false,
        );
        h.tx.send(utterance(0, 2_000)).unwrap();
        h.tx.send(utterance(3_000, 5_000)).unwrap();
        h.tx.send(utterance(6_000, 8_000)).unwrap();
        drop(h.tx);
        let stats = h.worker.finish();
        assert_eq!(stats.finals, 3, "the unpunctuated final is still published");
        let calls = h.calls.lock().unwrap();
        assert_eq!(
            calls[2].1.as_deref(),
            Some("Tell me about a time you disagreed."),
            "the last punctuated final stays the prompt, so the lane keeps its punctuation"
        );
    }

    #[test]
    fn a_dropped_final_does_not_become_the_next_prompt() {
        let h = harness(
            &[("Thank you.", -0.2, 3), ("Real question?", -0.2, 3)],
            false,
        );
        h.tx.send(utterance(0, 1_000)).unwrap();
        h.tx.send(utterance(2_000, 3_000)).unwrap();
        drop(h.tx);
        h.worker.finish();
        let calls = h.calls.lock().unwrap();
        assert_eq!(calls[1].1, None, "a dropped hallucination is not context");
    }

    #[test]
    fn a_failed_inference_still_closes_the_utterance() {
        /// whisper can fail mid-session (a bad state, an OOM); the utterance must still close.
        struct Failing;
        impl Speech for Failing {
            fn transcribe(&mut self, _: &[f32], _: Option<&str>) -> Result<Transcript, SttError> {
                Err(SttError::Io {
                    path: "whisper".to_owned(),
                    source: std::io::Error::other("inference exploded"),
                })
            }
        }

        let out = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&out);
        let (tx, rx) = crossbeam_channel::bounded(8);
        let worker = SttWorker::spawn(
            Box::new(Failing),
            rx,
            Box::new(move |s| sink.lock().unwrap().push(s)),
            SttWorkerConfig::default(),
            "failing",
        )
        .unwrap();
        tx.send(utterance(1_000, 4_000)).unwrap();
        drop(tx);
        let stats = worker.finish();

        let published = out.lock().unwrap().clone();
        assert_eq!(published.len(), 1, "the utterance closed: {published:?}");
        let closing = &published[0];
        assert!(closing.is_final && closing.text.is_empty());
        assert_eq!(closing.start, Duration::from_millis(1_000));
        assert_eq!(closing.end, Duration::from_millis(4_000));
        assert_eq!(
            closing.avg_logprob,
            f32::MIN,
            "a failure cannot look like a confident empty result"
        );
        assert_eq!(stats.errors, 1, "counted as an inference failure");
        assert_eq!((stats.finals, stats.dropped()), (0, 0), "not a filter drop");

        // And that closing segment clears the partial the utterance had already shown.
        let mut log = dmn_transcript::TranscriptLog::new();
        log.apply(Segment {
            source: Source::Interviewer,
            start: Duration::from_millis(1_000),
            end: Duration::from_millis(2_000),
            text: "how would you".to_owned(),
            is_final: false,
            avg_logprob: -0.2,
        });
        assert_eq!(log.open_partials().len(), 1);
        log.apply(closing.clone());
        assert!(log.open_partials().is_empty(), "no stale partial is left");
        assert!(
            log.finals().is_empty(),
            "and no empty line in the transcript"
        );
    }

    #[test]
    fn stop_ends_a_worker_whose_sender_is_still_open() {
        let h = harness(&[("x", -0.2, 1)], false);
        let t = Instant::now();
        let _stats = h.worker.stop();
        assert!(t.elapsed() < Duration::from_millis(500));
        drop(h.tx);
    }
}
