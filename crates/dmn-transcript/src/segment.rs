//! What one stretch of speech became, and the rules for tidying and joining such stretches.

use std::time::Duration;

use dmn_audio::Source;

/// One transcribed stretch of one source's audio (docs/SPEC.md § STT worker).
///
/// `start` is the onset of the utterance the audio belongs to, so every partial of an utterance
/// and the final that closes it carry the same `start`: that pair (`source`, `start`) is the key
/// [`TranscriptLog`](crate::TranscriptLog) uses to replace partial text with final text.
#[derive(Debug, Clone, PartialEq)]
pub struct Segment {
    pub source: Source,
    pub start: Duration,
    /// End of the audio this text covers. Grows with each partial; final at the close.
    pub end: Duration,
    pub text: String,
    /// False for a partial of a still-open utterance, true for the pass over the closed one.
    pub is_final: bool,
    /// Mean token log-probability from whisper; 0.0 when it produced no text tokens.
    pub avg_logprob: f32,
    /// ISO 639-1 code of the language whisper detected in this utterance's audio, when the STT
    /// worker runs in a language mode other than `en` (#77); `None` on the English-only path.
    /// The text is English either way: a multilingual model translates as it transcribes.
    pub language: Option<String>,
}

impl Segment {
    pub fn key(&self) -> (Source, Duration) {
        (self.source, self.start)
    }

    /// `[mm:ss.t] Them: text`, the transcript line `dmn replay` and the TUI print. A
    /// detected language other than English is tagged after the speaker: `Them (es→en)`.
    pub fn render(&self) -> String {
        format!(
            "[{}] {}{}{}: {}",
            timestamp(self.start),
            self.source.label(),
            match self.language.as_deref() {
                Some(lang) if lang != "en" => format!(" ({lang}→en)"),
                _ => String::new(),
            },
            if self.is_final { "" } else { " ~" },
            self.text
        )
    }
}

/// `mm:ss.t` from the start of the session.
pub fn timestamp(t: Duration) -> String {
    let tenths = t.as_millis() / 100;
    format!(
        "{:02}:{:02}.{}",
        tenths / 600,
        (tenths / 10) % 60,
        tenths % 10
    )
}

/// Tidy one piece of whisper output: trim, collapse runs of whitespace, and drop the leading
/// dash whisper sometimes puts on dialogue lines. Nothing here invents punctuation: whisper
/// already punctuates, and a partial legitimately ends mid-sentence.
pub fn normalise(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for (i, word) in text.split_whitespace().enumerate() {
        let word = if i == 0 {
            word.trim_start_matches(['-', '–', '—']).trim_start()
        } else {
            word
        };
        if word.is_empty() {
            continue;
        }
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(word);
    }
    out
}

/// Gap below which two finals from one source are treated as one continued sentence.
pub const MERGE_GAP: Duration = Duration::from_millis(400);

/// True when `next` continues `prev` rather than starting a new line: same source, a gap under
/// [`MERGE_GAP`], and `prev` does not end a sentence. This is what a 12 s forced split looks
/// like (docs/SPEC.md § Transcription: an utterance is cut at 12 s and the speaker continues).
pub fn continues(prev: &Segment, next: &Segment) -> bool {
    prev.source == next.source
        && prev.is_final
        && next.is_final
        && next.start >= prev.end
        && next.start - prev.end <= MERGE_GAP
        && !prev.text.trim_end().ends_with(['.', '!', '?', '…'])
}

/// Join runs of segments that [`continues`] links, so a forced split reads as one line.
pub fn merge_adjacent(segments: &[Segment]) -> Vec<Segment> {
    let mut out: Vec<Segment> = Vec::with_capacity(segments.len());
    for seg in segments {
        match out.last_mut() {
            Some(prev) if continues(prev, seg) => {
                if !seg.text.is_empty() {
                    if !prev.text.is_empty() {
                        prev.text.push(' ');
                    }
                    prev.text.push_str(&seg.text);
                }
                prev.end = seg.end;
                // Keep the worse of the two: a merged line is only as trustworthy as its weakest
                // half, and the detector uses this to discount doubtful questions.
                prev.avg_logprob = prev.avg_logprob.min(seg.avg_logprob);
            }
            _ => out.push(seg.clone()),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seg(start_ms: u64, end_ms: u64, text: &str) -> Segment {
        Segment {
            source: Source::Them,
            start: Duration::from_millis(start_ms),
            end: Duration::from_millis(end_ms),
            text: text.to_owned(),
            is_final: true,
            avg_logprob: -0.2,
            language: None,
        }
    }

    #[test]
    fn timestamps_are_mm_ss_tenths() {
        assert_eq!(timestamp(Duration::ZERO), "00:00.0");
        assert_eq!(timestamp(Duration::from_millis(12_450)), "00:12.4");
        assert_eq!(timestamp(Duration::from_secs(61)), "01:01.0");
        assert_eq!(timestamp(Duration::from_secs(3600)), "60:00.0");
    }

    #[test]
    fn normalise_trims_collapses_and_drops_a_leading_dash() {
        assert_eq!(
            normalise("  How   would you\n design it? "),
            "How would you design it?"
        );
        assert_eq!(normalise("- Tell me about a time"), "Tell me about a time");
        assert_eq!(normalise("   "), "");
        assert_eq!(normalise("—"), "");
    }

    #[test]
    fn a_partial_renders_with_a_marker_and_a_final_without() {
        let mut s = seg(12_450, 14_000, "how would you design");
        s.is_final = false;
        assert_eq!(s.render(), "[00:12.4] Them ~: how would you design");
        s.is_final = true;
        assert_eq!(s.render(), "[00:12.4] Them: how would you design");
    }

    #[test]
    fn a_forced_split_is_merged_but_a_finished_sentence_is_not() {
        // 12 s cut mid-sentence, continued 20 ms later.
        let merged = merge_adjacent(&[
            seg(0, 12_000, "so the first thing I would do is"),
            seg(12_020, 15_000, "clarify the requirements."),
        ]);
        assert_eq!(merged.len(), 1);
        assert_eq!(
            merged[0].text,
            "so the first thing I would do is clarify the requirements."
        );
        assert_eq!(merged[0].end, Duration::from_millis(15_000));

        // A completed sentence stays its own line even when the next one follows closely.
        let kept = merge_adjacent(&[
            seg(0, 12_000, "Let's switch gears."),
            seg(12_100, 15_000, "Tell me about a disagreement."),
        ]);
        assert_eq!(kept.len(), 2);
    }

    #[test]
    fn a_long_gap_or_another_speaker_never_merges() {
        let gap = merge_adjacent(&[
            seg(0, 1_000, "and then"),
            seg(2_000, 3_000, "we shipped it"),
        ]);
        assert_eq!(gap.len(), 2, "1 s gap is a new line");

        let mut other = seg(1_100, 2_000, "yes exactly");
        other.source = Source::You;
        let two = merge_adjacent(&[seg(0, 1_000, "does that make sense"), other]);
        assert_eq!(two.len(), 2, "different speakers never merge");
    }

    #[test]
    fn merging_keeps_the_lower_confidence() {
        let mut a = seg(0, 1_000, "the trickiest part is");
        a.avg_logprob = -0.1;
        let mut b = seg(1_100, 2_000, "clock skew");
        b.avg_logprob = -0.9;
        let merged = merge_adjacent(&[a, b]);
        assert_eq!(merged.len(), 1);
        assert!((merged[0].avg_logprob - -0.9).abs() < 1e-6);
    }
}
