//! Per-frame speech decisions → utterances, pauses and onsets, timed by frame count.

use std::collections::VecDeque;
use std::time::Duration;

use dmn_audio::{FRAME_LEN, FRAME_MS, Source};

/// Timing rules from docs/SPEC.md § Transcription and question detection.
#[derive(Debug, Clone, PartialEq)]
pub struct SegmenterConfig {
    /// Silence inside an utterance that counts as a pause (a [`VadEvent::Pause`]); 300 ms.
    pub pause: Duration,
    /// Trailing silence that closes an utterance; 700 ms.
    pub close: Duration,
    /// Longest utterance before a forced split; 12 s.
    pub max_utterance: Duration,
    /// Audio kept from before the onset and prepended to `samples`, so whisper hears the start
    /// of the first word even when its first frame was under the threshold.
    pub pre_roll: Duration,
    /// Audio kept after the last speech frame, for the same reason at the end.
    pub tail: Duration,
    /// Utterances with less speech than this are dropped as clicks or coughs.
    pub min_speech: Duration,
    /// How often to emit a [`VadEvent::Partial`] while an utterance is open, so the transcript
    /// keeps moving during a long question (SPEC § Transcription: partials every 1 s). `None`
    /// turns partials off, which is what a consumer that only wants closed utterances should do:
    /// each partial clones up to `partial_window` of audio.
    pub partial_every: Option<Duration>,
    /// How much of the open utterance a partial carries: whisper re-runs this sliding window.
    pub partial_window: Duration,
}

impl Default for SegmenterConfig {
    fn default() -> Self {
        Self {
            pause: Duration::from_millis(300),
            close: Duration::from_millis(700),
            max_utterance: Duration::from_secs(12),
            pre_roll: Duration::from_millis(200),
            tail: Duration::from_millis(100),
            min_speech: Duration::from_millis(100),
            partial_every: Some(Duration::from_secs(1)),
            partial_window: Duration::from_secs(8),
        }
    }
}

/// Why an utterance was closed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CloseReason {
    /// `close` of trailing silence.
    Silence,
    /// Reached `max_utterance`; the speaker is still talking and the next utterance continues.
    MaxLength,
    /// The source ended (end of file, or the worker was stopped) mid-utterance.
    EndOfStream,
    /// #124: the push-to-talk button was released: the utterance ends at once, with no
    /// trailing silence waited for.
    Released,
}

/// One segment of speech from one source.
#[derive(Debug, Clone, PartialEq)]
pub struct Utterance {
    pub source: Source,
    /// Onset of speech, from the start of the stream.
    pub start: Duration,
    /// End of the last speech frame.
    pub end: Duration,
    /// 16 kHz mono audio: `pre_roll` before `start`, the speech, and up to `tail` after `end`.
    pub samples: Vec<f32>,
    /// Offset of `start` inside `samples`.
    pub pre_roll: Duration,
    pub closed_by: CloseReason,
}

#[derive(Debug, Clone, PartialEq)]
pub enum VadEvent {
    SpeechStart {
        source: Source,
        at: Duration,
    },
    /// `pause` of silence inside an open utterance; `at` is where the speech stopped.
    Pause {
        source: Source,
        at: Duration,
    },
    /// A slice of an utterance that is still open, for the STT worker's 1 s partials. `start` is
    /// the onset of that utterance, so a partial and the [`Utterance`] that eventually closes it
    /// share it: that is the key a consumer uses to replace partial text with the final text.
    Partial {
        source: Source,
        start: Duration,
        /// End of the audio in `samples`.
        at: Duration,
        /// The last `partial_window` of the open utterance (less, early on).
        samples: Vec<f32>,
    },
    Utterance(Utterance),
}

#[derive(Debug)]
struct Open {
    start: u64,
    last_speech: u64,
    speech_frames: u64,
    silence: u64,
    pause_sent: bool,
    pre_roll_frames: usize,
    samples: Vec<f32>,
    /// Frame at which the last [`VadEvent::Partial`] was emitted.
    last_partial: u64,
}

#[derive(Debug)]
pub struct Segmenter {
    source: Source,
    pause: u64,
    close: u64,
    max: u64,
    pre_roll: usize,
    tail: usize,
    min_speech: u64,
    /// Frames between partials; 0 when partials are off.
    partial_every: u64,
    /// Frames of audio a partial carries.
    partial_window: usize,
    /// Frames seen so far; frame `n` covers `[n·20 ms, (n+1)·20 ms)`.
    n: u64,
    recent: VecDeque<[f32; FRAME_LEN]>,
    open: Option<Open>,
}

fn frames(d: Duration) -> u64 {
    (d.as_millis() / u128::from(FRAME_MS)) as u64
}

fn at(frame: u64) -> Duration {
    Duration::from_millis(frame * u64::from(FRAME_MS))
}

impl Segmenter {
    pub fn new(source: Source, cfg: &SegmenterConfig) -> Self {
        Self {
            source,
            pause: frames(cfg.pause).max(1),
            close: frames(cfg.close).max(1),
            max: frames(cfg.max_utterance).max(1),
            pre_roll: frames(cfg.pre_roll) as usize,
            tail: frames(cfg.tail) as usize,
            min_speech: frames(cfg.min_speech),
            partial_every: cfg.partial_every.map_or(0, |d| frames(d).max(1)),
            partial_window: frames(cfg.partial_window).max(1) as usize,
            n: 0,
            recent: VecDeque::new(),
            open: None,
        }
    }

    /// Stream time covered so far.
    pub fn position(&self) -> Duration {
        at(self.n)
    }

    /// True while an utterance is open.
    pub fn in_utterance(&self) -> bool {
        self.open.is_some()
    }

    /// Feed one frame and its VAD decision; events are appended to `out`.
    pub fn push(&mut self, frame: &[f32; FRAME_LEN], speech: bool, out: &mut Vec<VadEvent>) {
        let n = self.n;
        match self.open.as_mut() {
            None if speech => {
                let mut samples = Vec::with_capacity((self.pre_roll + 50) * FRAME_LEN);
                for f in &self.recent {
                    samples.extend_from_slice(f);
                }
                samples.extend_from_slice(frame);
                self.open = Some(Open {
                    start: n,
                    last_speech: n,
                    speech_frames: 1,
                    silence: 0,
                    pause_sent: false,
                    pre_roll_frames: self.recent.len(),
                    samples,
                    last_partial: n,
                });
                out.push(VadEvent::SpeechStart {
                    source: self.source,
                    at: at(n),
                });
            }
            None => {}
            Some(open) => {
                open.samples.extend_from_slice(frame);
                if speech {
                    open.last_speech = n;
                    open.speech_frames += 1;
                    open.silence = 0;
                    open.pause_sent = false;
                } else {
                    open.silence += 1;
                    if open.silence >= self.pause && !open.pause_sent {
                        open.pause_sent = true;
                        out.push(VadEvent::Pause {
                            source: self.source,
                            at: at(open.last_speech + 1),
                        });
                    }
                }
            }
        }

        let close = match &self.open {
            Some(o) if o.silence >= self.close => Some(CloseReason::Silence),
            Some(o) if n + 1 - o.start >= self.max => Some(CloseReason::MaxLength),
            _ => None,
        };
        if let Some(reason) = close {
            self.emit(reason, out);
        } else {
            self.emit_partial(out);
        }

        self.recent.push_back(*frame);
        while self.recent.len() > self.pre_roll {
            self.recent.pop_front();
        }
        self.n += 1;
    }

    /// #124: close the open utterance now (the push-to-talk button was released), whatever the
    /// silence so far. Nothing when none is open.
    pub fn close_now(&mut self, out: &mut Vec<VadEvent>) {
        if self.open.is_some() {
            self.emit(CloseReason::Released, out);
        }
    }

    /// Close any open utterance at end of stream.
    pub fn flush(&mut self, out: &mut Vec<VadEvent>) {
        if self.open.is_some() {
            self.emit(CloseReason::EndOfStream, out);
        }
    }

    /// Emit a [`VadEvent::Partial`] if one is due for the open utterance. Called with `self.n`
    /// still pointing at the frame just pushed, so the partial covers `[start, n+1)`.
    fn emit_partial(&mut self, out: &mut Vec<VadEvent>) {
        if self.partial_every == 0 {
            return;
        }
        let window = self.partial_window * FRAME_LEN;
        let (source, n) = (self.source, self.n);
        let Some(open) = self.open.as_mut() else {
            return;
        };
        if n + 1 - open.last_partial < self.partial_every {
            return;
        }
        if open.silence >= self.pause {
            // The speaker has already paused: the text cannot have changed since the last
            // partial, and the close is usually imminent. Don't spend an inference on it.
            return;
        }
        open.last_partial = n + 1;
        let from = open.samples.len().saturating_sub(window);
        out.push(VadEvent::Partial {
            source,
            start: at(open.start),
            at: at(n + 1),
            samples: open.samples[from..].to_vec(),
        });
    }

    fn emit(&mut self, reason: CloseReason, out: &mut Vec<VadEvent>) {
        let Some(mut open) = self.open.take() else {
            return;
        };
        if open.speech_frames < self.min_speech {
            return;
        }
        let speech = (open.last_speech - open.start + 1) as usize;
        let keep = (open.pre_roll_frames + speech + self.tail) * FRAME_LEN;
        open.samples.truncate(keep);
        out.push(VadEvent::Utterance(Utterance {
            source: self.source,
            start: at(open.start),
            end: at(open.last_speech + 1),
            samples: open.samples,
            pre_roll: at(open.pre_roll_frames as u64),
            closed_by: reason,
        }));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LOUD: [f32; FRAME_LEN] = [0.1; FRAME_LEN];
    const QUIET: [f32; FRAME_LEN] = [0.0; FRAME_LEN];

    /// Feed `pattern` as (frames, speech?) runs; return every event.
    fn run(pattern: &[(u64, bool)]) -> Vec<VadEvent> {
        let mut seg = Segmenter::new(Source::Them, &SegmenterConfig::default());
        let mut out = Vec::new();
        for &(count, speech) in pattern {
            for _ in 0..count {
                seg.push(if speech { &LOUD } else { &QUIET }, speech, &mut out);
            }
        }
        seg.flush(&mut out);
        out
    }

    fn utterances(ev: &[VadEvent]) -> Vec<&Utterance> {
        ev.iter()
            .filter_map(|e| match e {
                VadEvent::Utterance(u) => Some(u),
                _ => None,
            })
            .collect()
    }

    fn ms(v: u64) -> Duration {
        Duration::from_millis(v)
    }

    #[test]
    fn onset_is_timed_by_frame_count_and_carries_pre_roll() {
        // 1 s silence, 1 s speech, 1 s silence.
        let ev = run(&[(50, false), (50, true), (50, false)]);
        assert_eq!(
            ev[0],
            VadEvent::SpeechStart {
                source: Source::Them,
                at: ms(1000)
            }
        );
        let u = utterances(&ev);
        assert_eq!(u.len(), 1);
        assert_eq!((u[0].start, u[0].end), (ms(1000), ms(2000)));
        assert_eq!(u[0].pre_roll, ms(200));
        assert_eq!(u[0].closed_by, CloseReason::Silence);
        // 200 ms pre-roll + 1 s speech + 100 ms tail.
        assert_eq!(u[0].samples.len(), 16 * 1300);
    }

    #[test]
    fn a_300_ms_gap_is_a_pause_not_a_close() {
        let ev = run(&[(50, true), (15, false), (50, true), (40, false)]);
        let pauses: Vec<_> = ev
            .iter()
            .filter(|e| matches!(e, VadEvent::Pause { .. }))
            .collect();
        assert_eq!(
            pauses,
            vec![
                &VadEvent::Pause {
                    source: Source::Them,
                    at: ms(1000)
                },
                &VadEvent::Pause {
                    source: Source::Them,
                    at: ms(2300)
                }
            ]
        );
        let u = utterances(&ev);
        assert_eq!(u.len(), 1, "300 ms did not close it");
        assert_eq!((u[0].start, u[0].end), (ms(0), ms(2300)));
    }

    #[test]
    fn a_gap_just_under_700_ms_keeps_the_utterance_and_700_ms_closes_it() {
        let ev = run(&[(20, true), (34, false), (20, true), (35, false), (20, true)]);
        let u = utterances(&ev);
        assert_eq!(u.len(), 2);
        assert_eq!(
            (u[0].start, u[0].end),
            (ms(0), ms(1480)),
            "680 ms gap bridged"
        );
        assert_eq!(u[0].closed_by, CloseReason::Silence);
        assert_eq!((u[1].start, u[1].end), (ms(2180), ms(2580)));
        assert_eq!(u[1].closed_by, CloseReason::EndOfStream);
    }

    #[test]
    fn a_long_turn_is_force_split_at_12_s_and_continues() {
        let ev = run(&[(1000, true), (40, false)]); // 20 s of speech
        let u = utterances(&ev);
        assert_eq!(u.len(), 2);
        assert_eq!((u[0].start, u[0].end), (ms(0), ms(12_000)));
        assert_eq!(u[0].closed_by, CloseReason::MaxLength);
        assert_eq!((u[1].start, u[1].end), (ms(12_000), ms(20_000)));
        assert_eq!(u[1].closed_by, CloseReason::Silence);
    }

    #[test]
    fn a_click_shorter_than_min_speech_is_dropped() {
        let ev = run(&[(10, false), (3, true), (50, false)]);
        assert!(utterances(&ev).is_empty());
    }

    fn partials(ev: &[VadEvent]) -> Vec<(Duration, Duration, usize)> {
        ev.iter()
            .filter_map(|e| match e {
                VadEvent::Partial {
                    start, at, samples, ..
                } => Some((*start, *at, samples.len())),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn partials_arrive_every_second_and_slide_over_the_last_eight() {
        // 12 s of speech, then silence: 11 partials (1 s .. 11 s), then the close at 12 s.
        let ev = run(&[(600, true), (40, false)]);
        let p = partials(&ev);
        assert_eq!(p.len(), 11, "one per second while open: {p:?}");
        assert_eq!(p[0].0, ms(0), "every partial keys on the utterance onset");
        assert_eq!((p[0].1, p[1].1), (ms(1000), ms(2000)));
        // Early partials carry everything so far (this stream opens in speech, so no pre-roll),
        // later ones only the last 8 s.
        assert_eq!(p[0].2, 16 * 1000);
        assert_eq!(p[6].2, 16 * 7000, "7 s is still under the window");
        assert_eq!(p[7].2, 16 * 8000, "capped at the 8 s window");
        assert_eq!(p[10].2, 16 * 8000);
        // The final utterance shares the partials' start, so a consumer can replace their text.
        let u = utterances(&ev);
        assert_eq!(u[0].start, p[0].0);
    }

    #[test]
    fn no_partials_once_the_speaker_has_paused() {
        // 2 s of speech, then 2 s of silence: partials at 1 s and 2 s, then nothing, because
        // re-running whisper over trailing silence cannot change the text.
        let ev = run(&[(100, true), (100, false)]);
        let p = partials(&ev);
        assert_eq!(p.len(), 2, "{p:?}");
        assert_eq!((p[0].1, p[1].1), (ms(1000), ms(2000)));
        // And the utterance still closes 700 ms into that silence.
        let u = utterances(&ev);
        assert_eq!((u[0].start, u[0].end), (ms(0), ms(2000)));
    }

    #[test]
    fn partials_can_be_turned_off() {
        let cfg = SegmenterConfig {
            partial_every: None,
            ..SegmenterConfig::default()
        };
        let mut seg = Segmenter::new(Source::You, &cfg);
        let mut out = Vec::new();
        for _ in 0..300 {
            seg.push(&LOUD, true, &mut out);
        }
        seg.flush(&mut out);
        assert!(partials(&out).is_empty());
        assert_eq!(utterances(&out).len(), 1);
    }

    #[test]
    fn nothing_open_means_flush_emits_nothing() {
        assert!(run(&[(100, false)]).is_empty());
    }
}
