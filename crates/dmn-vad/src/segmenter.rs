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
        }

        self.recent.push_back(*frame);
        while self.recent.len() > self.pre_roll {
            self.recent.pop_front();
        }
        self.n += 1;
    }

    /// Close any open utterance at end of stream.
    pub fn flush(&mut self, out: &mut Vec<VadEvent>) {
        if self.open.is_some() {
            self.emit(CloseReason::EndOfStream, out);
        }
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
        let mut seg = Segmenter::new(Source::Interviewer, &SegmenterConfig::default());
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
                source: Source::Interviewer,
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
                    source: Source::Interviewer,
                    at: ms(1000)
                },
                &VadEvent::Pause {
                    source: Source::Interviewer,
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

    #[test]
    fn nothing_open_means_flush_emits_nothing() {
        assert!(run(&[(100, false)]).is_empty());
    }
}
