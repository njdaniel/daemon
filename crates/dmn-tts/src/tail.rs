//! The trailing-silence cut (#113): Chatterbox sometimes ends a generation with a second or more
//! of silence and then a loud burst of noise (its end-of-text hallucination; one Russian render
//! at temperature 0.8 had a burst peaking at 0.73 against 0.2 for the speech). So once speech
//! has been heard, [`CUT_AFTER`] of near-silence ends the stream there: the burst, and anything
//! else after the silence, never reaches the caller (`dmn-voice`, or `dmn speak --out`'s file).
//!
//! **Near-silence** is a [`WINDOW`] whose peak stays under [`QUIET`]. **Speech** has been heard
//! once [`SPEECH_MIN`] of windows at or over it have passed, so a click or the server's leading
//! silence (up to 1.5 s measured, with the Russian voice) never arms the cut. Quiet windows after
//! speech are held back, not passed on, until either speech resumes (a pause: the held silence
//! goes out, unchanged) or the run reaches [`CUT_AFTER`] (the end: the first [`KEEP_TAIL`] of it
//! goes out, so the sentence ends softly, and the stream stops). At the end of the body the
//! held silence goes out as it is.
//!
//! The cut ends a whole generation, so it is applied to one sentence at a time: the client asks
//! the server for each sentence on its own (`chatterbox.rs`). Measured 2026-10-08 on the GPU
//! server: inside one generation of two sentences the model paused up to 0.98 s between them
//! (temperature 0.6, German), while inside single sentences the longest pause before the end was
//! 0.64 s, and every longer silence was the tail (followed by nothing, or by noise up to 0.12
//! peak, once after 3.06 s of silence). Values below.

use crate::decode::decode_reader;
use crate::provider::{PcmChunk, TtsError};

/// The analysis window: 20 ms.
pub(crate) const WINDOW_SECS: f32 = 0.02;
/// A window whose peak is under this is near-silence. The server's silence measured 0.002–0.005
/// (5th percentile of 20 ms window peaks), speech 0.2–1.0 at its loudest.
pub(crate) const QUIET: f32 = 0.02;
/// Speech heard before the cut can arm: 200 ms of windows at or over [`QUIET`].
pub(crate) const SPEECH_MIN_SECS: f32 = 0.2;
/// Near-silence after speech that ends the stream: 0.8 s (the issue's figure). Mid-sentence
/// pauses measured at most 0.64 s.
pub(crate) const CUT_AFTER_SECS: f32 = 0.8;
/// How much of the silence before the cut is kept: a soft end, and with the next sentence's own
/// leading silence a natural pause between two sentences.
pub(crate) const KEEP_TAIL_SECS: f32 = 0.3;

/// What [`TailCut::push`] decided.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Cut {
    /// Keep reading.
    More,
    /// The trailing silence was long enough: stop reading; nothing after it is wanted.
    Stop,
}

/// The cut's state for one generation. See the module doc.
#[derive(Debug)]
pub(crate) struct TailCut {
    window: usize,
    speech_min: usize,
    cut_after: usize,
    keep: usize,
    /// Samples of the window being filled.
    partial: Vec<f32>,
    /// Quiet samples after speech, not passed on yet.
    held: Vec<f32>,
    /// Samples in windows at or over [`QUIET`] so far.
    loud: usize,
    stopped: bool,
}

impl TailCut {
    pub(crate) fn new(rate: u32) -> Self {
        let at = |secs: f32| ((rate as f32 * secs) as usize).max(1);
        Self {
            window: at(WINDOW_SECS),
            speech_min: at(SPEECH_MIN_SECS),
            cut_after: at(CUT_AFTER_SECS),
            keep: at(KEEP_TAIL_SECS),
            partial: Vec::new(),
            held: Vec::new(),
            loud: 0,
            stopped: false,
        }
    }

    fn speech_seen(&self) -> bool {
        self.loud >= self.speech_min
    }

    /// Take `samples`; what may be passed on is appended to `out`.
    pub(crate) fn push(&mut self, samples: &[f32], out: &mut Vec<f32>) -> Cut {
        if self.stopped {
            return Cut::Stop;
        }
        for &s in samples {
            self.partial.push(s);
            if self.partial.len() < self.window {
                continue;
            }
            let peak = self.partial.iter().fold(0.0f32, |m, v| m.max(v.abs()));
            if peak >= QUIET {
                self.loud += self.partial.len();
                out.append(&mut self.held);
                out.append(&mut self.partial);
            } else if self.speech_seen() {
                self.held.append(&mut self.partial);
                if self.held.len() >= self.cut_after {
                    self.held.truncate(self.keep);
                    out.append(&mut self.held);
                    self.stopped = true;
                    tracing::debug!(
                        "tts: trailing silence; the rest of this generation is dropped"
                    );
                    return Cut::Stop;
                }
            } else {
                out.append(&mut self.partial);
            }
        }
        Cut::More
    }

    /// The body ended without a cut: whatever is held (a silence shorter than the cut, and the
    /// last partial window) goes out.
    pub(crate) fn finish(&mut self, out: &mut Vec<f32>) {
        if self.stopped {
            return;
        }
        out.append(&mut self.held);
        out.append(&mut self.partial);
    }

    pub(crate) fn stopped(&self) -> bool {
        self.stopped
    }
}

/// [`decode_reader`] with the [`TailCut`] between the decoder and `on_chunk`: chunks go on as
/// the cut lets them, and a cut stops the read (the rest of the body is never read). `on_chunk`
/// returning `false` stops it too, as there. Returns whether the cut ended the stream.
pub(crate) fn decode_cut<R, G, F>(reader: R, on_rate: G, mut on_chunk: F) -> bool
where
    R: std::io::Read,
    G: FnOnce(Result<u32, TtsError>),
    F: FnMut(Result<PcmChunk, TtsError>) -> bool,
{
    let mut cut: Option<TailCut> = None;
    let mut rate = 0;
    let mut gone = false;
    let mut out = Vec::new();
    decode_reader(reader, on_rate, |chunk| match chunk {
        Ok(chunk) => {
            let c = cut.get_or_insert_with(|| TailCut::new(chunk.sample_rate));
            rate = chunk.sample_rate;
            let verdict = c.push(&chunk.samples, &mut out);
            if !out.is_empty() {
                let samples = std::mem::take(&mut out);
                if !on_chunk(Ok(PcmChunk {
                    samples,
                    sample_rate: chunk.sample_rate,
                })) {
                    gone = true;
                    return false;
                }
            }
            verdict == Cut::More
        }
        Err(e) => {
            let keep = on_chunk(Err(e));
            gone |= !keep;
            keep
        }
    });
    let Some(mut c) = cut else {
        return false;
    };
    if c.stopped() {
        return true;
    }
    if !gone {
        c.finish(&mut out);
        if !out.is_empty() {
            on_chunk(Ok(PcmChunk {
                samples: out,
                sample_rate: rate,
            }));
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    const RATE: u32 = 24_000;

    fn tone(secs: f32, amp: f32) -> Vec<f32> {
        let n = (RATE as f32 * secs) as usize;
        (0..n)
            .map(|i| amp * (std::f32::consts::TAU * 220.0 * i as f32 / RATE as f32).sin())
            .collect()
    }

    fn quiet(secs: f32) -> Vec<f32> {
        // The server's silence is not digital zero: a floor of a few thousandths.
        let n = (RATE as f32 * secs) as usize;
        (0..n)
            .map(|i| if i % 2 == 0 { 0.004 } else { -0.004 })
            .collect()
    }

    /// Feed `parts` in 200 ms chunks (the decoder's size), as the decoder thread would.
    fn run(parts: &[Vec<f32>]) -> (Vec<f32>, bool) {
        let all: Vec<f32> = parts.concat();
        let mut cut = TailCut::new(RATE);
        let mut out = Vec::new();
        let mut stopped = false;
        for chunk in all.chunks(4_800) {
            if cut.push(chunk, &mut out) == Cut::Stop {
                stopped = true;
                break;
            }
        }
        if !stopped {
            cut.finish(&mut out);
        }
        (out, stopped)
    }

    fn secs(n: usize) -> f32 {
        n as f32 / RATE as f32
    }

    /// Speech, a second of silence, then the model's burst: the burst never comes out, and
    /// the speech and a short soft end do.
    #[test]
    fn a_burst_after_a_long_trailing_silence_never_comes_out() {
        let speech = tone(2.0, 0.2);
        let (out, stopped) = run(&[speech.clone(), quiet(1.0), tone(0.5, 0.73), quiet(0.2)]);
        assert!(stopped, "the silence ended the stream");
        assert_eq!(&out[..speech.len()], &speech[..], "the speech is untouched");
        let after = &out[speech.len()..];
        assert!(
            (secs(after.len()) - KEEP_TAIL_SECS).abs() < 0.03,
            "{} s of the silence kept",
            secs(after.len())
        );
        let peak = after.iter().fold(0.0f32, |m, v| m.max(v.abs()));
        assert!(peak < QUIET, "nothing of the burst: peak {peak}");
    }

    #[test]
    fn a_pause_mid_sentence_does_not_cut() {
        let parts = [tone(1.0, 0.2), quiet(0.4), tone(1.0, 0.2), quiet(0.3)];
        let (out, stopped) = run(&parts);
        assert!(!stopped);
        assert_eq!(out, parts.concat(), "every sample, in order");
        // Even a pause just under the cut passes through.
        let parts = [tone(1.0, 0.2), quiet(0.75), tone(0.5, 0.2)];
        let (out, stopped) = run(&parts);
        assert!(!stopped);
        assert_eq!(out, parts.concat());
    }

    /// The server's leading silence (1.5 s measured with the Russian voice) and a click before
    /// the speech do not arm the cut.
    #[test]
    fn silence_before_the_speech_is_not_a_trailing_silence() {
        let mut click = quiet(0.5);
        click[100] = 0.3;
        let parts = [click, quiet(1.5), tone(1.0, 0.2), quiet(0.2)];
        let (out, stopped) = run(&parts);
        assert!(!stopped);
        assert_eq!(out, parts.concat());
    }

    #[test]
    fn a_stream_that_ends_in_a_short_silence_keeps_all_of_it() {
        let parts = [tone(1.0, 0.2), quiet(0.6)];
        let (out, stopped) = run(&parts);
        assert!(!stopped);
        assert_eq!(out.len(), parts.concat().len());
    }

    /// Through the decoder: the burst never reaches the chunk callback, and the read stops.
    #[test]
    fn decode_cut_stops_the_read_at_the_trailing_silence() {
        let samples: Vec<f32> = [tone(1.5, 0.2), quiet(1.0), tone(0.5, 0.73)].concat();
        let wav = crate::decode::wav_of(RATE, &samples);
        let mut got = Vec::new();
        let mut rate = None;
        let cut = decode_cut(
            std::io::Cursor::new(wav),
            |r| rate = Some(r.unwrap()),
            |c| {
                got.extend(c.unwrap().samples);
                true
            },
        );
        assert!(cut);
        assert_eq!(rate, Some(RATE));
        let peak_after = got[(1.5 * RATE as f32) as usize..]
            .iter()
            .fold(0.0f32, |m, v| m.max(v.abs()));
        assert!(peak_after < QUIET, "the burst was cut: {peak_after}");
        assert!((secs(got.len()) - (1.5 + KEEP_TAIL_SECS)).abs() < 0.03);
    }
}
