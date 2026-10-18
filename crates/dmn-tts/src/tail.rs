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
//! **The leading trim (#122).** Qwen3-TTS opens most sentences with 0.4–0.8 s of near-silence
//! (measured 2026-10-18 with the owner's clip; Chatterbox up to 1.5 s), which the listener sits
//! through before the first word. Until speech has been heard, the samples are held, not passed
//! on; a run of [`LEAD_RESET`] of near-silence drops what is held but the last [`KEEP_LEAD`] (so
//! a click before the speech goes too). Once speech is heard, the held samples go out, starting
//! at most [`KEEP_LEAD`] before the first loud window, so a soft first consonant is kept. The
//! hold is also a small jitter buffer: playback starts with [`SPEECH_MIN`] of speech in hand,
//! enough to cover the wait for the server's next block when the first one ends just after the
//! speech starts. A generation with no speech at all goes out as the trim left it.
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
/// How much of the silence before the cut is kept: a soft end, and with the next sentence's
/// [`KEEP_LEAD_SECS`] a natural pause between two sentences.
pub(crate) const KEEP_TAIL_SECS: f32 = 0.3;
/// #122: near-silence kept before a sentence's first sound: a soft onset ("s", "f") and a breath.
pub(crate) const KEEP_LEAD_SECS: f32 = 0.1;
/// #122: near-silence before any speech that drops what was held (a click and its silence):
/// longer than the gap inside a word (a stop consonant's closure is under 0.15 s).
pub(crate) const LEAD_RESET_SECS: f32 = 0.3;

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
    keep_lead: usize,
    lead_reset: usize,
    /// #122: before speech, everything since the last reset, not passed on yet.
    lead: Vec<f32>,
    /// #122: near-silent samples at the end of `lead`.
    lead_quiet: usize,
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
            keep_lead: at(KEEP_LEAD_SECS),
            lead_reset: at(LEAD_RESET_SECS),
            lead: Vec::new(),
            lead_quiet: 0,
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
            if !self.speech_seen() {
                self.lead_window(peak >= QUIET, out);
                continue;
            }
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

    /// #122: one window before speech has been heard (module doc, the leading trim).
    fn lead_window(&mut self, loud: bool, out: &mut Vec<f32>) {
        let len = self.partial.len();
        self.lead.append(&mut self.partial);
        if loud {
            self.loud += len;
            self.lead_quiet = 0;
        } else {
            self.lead_quiet += len;
            if self.lead_quiet >= self.lead_reset {
                // Nothing held is speech: a click, or the opening silence. Keep the tail only.
                let drop = self.lead.len().saturating_sub(self.keep_lead);
                self.lead.drain(..drop);
                self.lead_quiet = self.lead.len();
                self.loud = 0;
            }
        }
        if self.speech_seen() {
            // The first loud window is at most `lead_reset` in; keep `keep_lead` before it.
            let first_loud = self.leading_quiet();
            let drop = first_loud.saturating_sub(self.keep_lead);
            self.lead.drain(..drop);
            out.append(&mut self.lead);
            self.lead_quiet = 0;
        }
    }

    /// #122: near-silent samples at the start of `lead`, in whole windows.
    fn leading_quiet(&self) -> usize {
        self.lead
            .chunks(self.window)
            .take_while(|w| w.iter().fold(0.0f32, |m, v| m.max(v.abs())) < QUIET)
            .map(<[f32]>::len)
            .sum()
    }

    /// The body ended without a cut: whatever is held (a silence shorter than the cut, and the
    /// last partial window) goes out.
    pub(crate) fn finish(&mut self, out: &mut Vec<f32>) {
        if self.stopped {
            return;
        }
        // #122: a generation too short to count as speech goes out as the trim left it.
        out.append(&mut self.lead);
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
    /// the speech do not arm the cut; #122: they are trimmed, all but the last 0.1 s before the
    /// speech.
    #[test]
    fn silence_before_the_speech_is_trimmed_and_does_not_arm_the_cut() {
        let mut click = quiet(0.5);
        click[100] = 0.3;
        let speech = [tone(1.0, 0.2), quiet(0.2)].concat();
        let (out, stopped) = run(&[click, quiet(1.5), speech.clone()]);
        assert!(!stopped);
        let lead = out.len() - speech.len();
        assert!(
            (secs(lead) - KEEP_LEAD_SECS).abs() < 0.021,
            "{} s of lead kept",
            secs(lead)
        );
        assert!(
            out[..lead].iter().all(|v| v.abs() < QUIET),
            "the click is gone"
        );
        assert_eq!(&out[lead..], &speech[..], "the speech is untouched");
    }

    /// #122: Qwen3-TTS's opening silence (0.4-0.8 s) goes, but a soft onset in the last 0.1 s
    /// before the first loud window stays, and a short gap early in the speech (a stop
    /// consonant) does not drop what came before it.
    #[test]
    fn the_leading_trim_keeps_a_soft_onset_and_a_gap_inside_the_first_word() {
        let soft: Vec<f32> = tone(0.08, 0.015);
        let word = [tone(0.1, 0.2), quiet(0.12), tone(0.6, 0.2)].concat();
        let (out, stopped) = run(&[quiet(0.6), soft.clone(), word.clone()]);
        assert!(!stopped);
        let tail = [soft, word].concat();
        assert!(
            out.ends_with(&tail),
            "the soft onset and the whole word went out"
        );
        assert!(
            secs(out.len() - tail.len()) <= KEEP_LEAD_SECS,
            "{}",
            secs(out.len())
        );
    }

    /// #122: a generation with no speech (the warm-up's "OK." came back near-silent) ends with
    /// at most the kept lead, and short speech that never reaches the speech minimum still
    /// goes out at the end.
    #[test]
    fn a_generation_without_speech_is_mostly_dropped_and_a_short_one_still_plays() {
        let (out, stopped) = run(&[quiet(1.2)]);
        assert!(!stopped);
        assert!(
            secs(out.len()) <= LEAD_RESET_SECS + 0.02,
            "{}",
            secs(out.len())
        );

        let blip = tone(0.1, 0.3);
        let (out, _) = run(&[quiet(0.5), blip.clone(), quiet(0.1)]);
        assert!(
            out.windows(blip.len()).any(|w| w == &blip[..]),
            "a 0.1 s word still plays"
        );
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
