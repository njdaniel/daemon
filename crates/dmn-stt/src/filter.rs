//! Dropping text whisper produced but nobody said.
//!
//! Two rules, both from docs/SPEC.md § STT worker: segments whose mean token log-probability is
//! below [`LOW_CONFIDENCE_LOGPROB`](crate::transcriber::LOW_CONFIDENCE_LOGPROB), and short
//! segments matching whisper's stock hallucinations on silence.

use crate::transcriber::LOW_CONFIDENCE_LOGPROB;

/// Phrases whisper emits when handed silence or noise. The SPEC names the first two; the rest
/// are the well-known ones from whisper's training data (subtitle and podcast outros), which
/// show up in whisper.cpp bug reports about silent input. Match is case- and
/// punctuation-insensitive and only applies below [`FilterConfig::max_words`], so a real
/// sentence containing "thank you" is never dropped.
pub const HALLUCINATION_PHRASES: &[&str] = &[
    "thank you",
    "thank you very much",
    "thanks for watching",
    "thanks for watching!",
    "thank you for watching",
    "please subscribe",
    "like and subscribe",
    "you",
    "bye",
    "bye bye",
    "okay",
    "so",
    "the end",
    "silence",
    "blank audio",
    "music",
    "applause",
    "subtitles by the amara org community",
    "amara org",
    "transcription by castingwords",
];

#[derive(Debug, Clone, PartialEq)]
pub struct FilterConfig {
    /// Drop below this mean token log-probability.
    pub min_logprob: f32,
    /// Only segments with at most this many words are matched against the phrase list.
    pub max_words: usize,
}

impl Default for FilterConfig {
    fn default() -> Self {
        Self {
            min_logprob: LOW_CONFIDENCE_LOGPROB,
            max_words: 3,
        }
    }
}

/// Why a segment was dropped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dropped {
    /// Nothing but whitespace.
    Empty,
    /// Mean token log-probability below the threshold.
    LowConfidence,
    /// A short segment matching [`HALLUCINATION_PHRASES`].
    Phrase,
}

impl Dropped {
    pub fn label(self) -> &'static str {
        match self {
            Dropped::Empty => "empty",
            Dropped::LowConfidence => "low confidence",
            Dropped::Phrase => "hallucination phrase",
        }
    }
}

/// Lowercase, strip everything that is not a letter, digit or space, and collapse whitespace.
fn fingerprint(text: &str) -> String {
    let cleaned: String = text
        .chars()
        .map(|c| {
            if c.is_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                ' '
            }
        })
        .collect();
    cleaned.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// `Some(reason)` when this segment should not reach the transcript.
pub fn should_drop(text: &str, avg_logprob: f32, n_tokens: usize, cfg: &FilterConfig) -> Option<Dropped> {
    let fp = fingerprint(text);
    if fp.is_empty() {
        return Some(Dropped::Empty);
    }
    if n_tokens > 0 && avg_logprob < cfg.min_logprob {
        return Some(Dropped::LowConfidence);
    }
    let words = fp.split_whitespace().count();
    if words <= cfg.max_words && HALLUCINATION_PHRASES.contains(&fp.as_str()) {
        return Some(Dropped::Phrase);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    const OK: f32 = -0.2;

    #[test]
    fn real_speech_survives() {
        let cfg = FilterConfig::default();
        assert_eq!(
            should_drop("How would you design a rate limiter?", OK, 9, &cfg),
            None
        );
        // Short but not a stock phrase.
        assert_eq!(should_drop("Yes, exactly.", OK, 3, &cfg), None);
    }

    #[test]
    fn stock_phrases_on_silence_are_dropped_however_they_are_punctuated() {
        let cfg = FilterConfig::default();
        for text in [
            "Thank you.",
            " thank you! ",
            "Thanks for watching!",
            "[BLANK_AUDIO]",
            "(applause)",
            "you",
            "Bye.",
        ] {
            assert_eq!(
                should_drop(text, OK, 2, &cfg),
                Some(Dropped::Phrase),
                "{text:?} should be dropped"
            );
        }
    }

    #[test]
    fn a_long_sentence_containing_a_stock_phrase_is_kept() {
        let cfg = FilterConfig::default();
        assert_eq!(
            should_drop("Thank you, that covers my questions on caching.", OK, 12, &cfg),
            None
        );
        assert_eq!(should_drop("So, tell me about yourself.", OK, 7, &cfg), None);
    }

    #[test]
    fn low_confidence_is_dropped_but_only_when_there_were_tokens() {
        let cfg = FilterConfig::default();
        assert_eq!(
            should_drop("mumble mumble mumble mumble", -1.5, 6, &cfg),
            Some(Dropped::LowConfidence)
        );
        assert_eq!(should_drop("exactly at the line", -1.0, 4, &cfg), None);
        // No tokens means whisper reported nothing to score, not low confidence.
        assert_eq!(should_drop("text", 0.0, 0, &cfg), None);
    }

    #[test]
    fn whitespace_and_punctuation_only_are_empty() {
        let cfg = FilterConfig::default();
        assert_eq!(should_drop("   ", OK, 1, &cfg), Some(Dropped::Empty));
        assert_eq!(should_drop("...", OK, 1, &cfg), Some(Dropped::Empty));
        assert_eq!(should_drop("♪♪", OK, 1, &cfg), Some(Dropped::Empty));
    }
}
