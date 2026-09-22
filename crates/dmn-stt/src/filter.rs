//! Dropping text whisper produced but nobody said.
//!
//! Two rules, both from docs/SPEC.md § STT worker: segments whose mean token log-probability is
//! below [`LOW_CONFIDENCE_LOGPROB`](crate::transcriber::LOW_CONFIDENCE_LOGPROB), and short
//! segments matching whisper's stock hallucinations on silence.

use crate::transcriber::LOW_CONFIDENCE_LOGPROB;

/// Phrases whisper emits when handed silence or noise, and which nobody says in an interview.
/// docs/SPEC.md names "Thank you." and "Thanks for watching."; the rest are subtitle and podcast
/// outros from whisper's training data, plus the bracketed markers it emits for non-speech.
/// Matching is case- and punctuation-insensitive and only applies below
/// [`FilterConfig::max_words`], so a sentence containing "thank you" is never dropped.
pub const HALLUCINATION_PHRASES: &[&str] = &[
    "thank you",
    "thank you very much",
    "thanks for watching",
    "thank you for watching",
    "please subscribe",
    "like and subscribe",
    "silence",
    "blank audio",
    "music",
    "applause",
    "subtitles by the amara org community",
    "amara org",
    "transcription by castingwords",
];

/// Whisper's other stock fillers on silence. These are also things a candidate really says
/// ("Okay.", "So…", "Bye."), so text alone cannot tell them apart and dropping them outright
/// would delete real replies. They go only when whisper was also unsure
/// ([`FilterConfig::ambiguous_min_logprob`]): a clearly-heard "Okay." stays in the transcript.
pub const AMBIGUOUS_PHRASES: &[&str] = &["you", "okay", "so", "bye", "bye bye", "the end", "yeah"];

#[derive(Debug, Clone, PartialEq)]
pub struct FilterConfig {
    /// Drop below this mean token log-probability.
    pub min_logprob: f32,
    /// Only segments with at most this many words are matched against the phrase lists.
    pub max_words: usize,
    /// An [`AMBIGUOUS_PHRASES`] match is dropped only below this. Well above `min_logprob`:
    /// whisper is very sure of a real "Okay." and much less sure of one it invented.
    pub ambiguous_min_logprob: f32,
}

impl Default for FilterConfig {
    fn default() -> Self {
        Self {
            min_logprob: LOW_CONFIDENCE_LOGPROB,
            max_words: 3,
            ambiguous_min_logprob: -0.5,
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
pub fn should_drop(
    text: &str,
    avg_logprob: f32,
    n_tokens: usize,
    cfg: &FilterConfig,
) -> Option<Dropped> {
    let fp = fingerprint(text);
    if fp.is_empty() {
        return Some(Dropped::Empty);
    }
    if n_tokens > 0 && avg_logprob < cfg.min_logprob {
        return Some(Dropped::LowConfidence);
    }
    let words = fp.split_whitespace().count();
    if words <= cfg.max_words {
        if HALLUCINATION_PHRASES.contains(&fp.as_str()) {
            return Some(Dropped::Phrase);
        }
        if AMBIGUOUS_PHRASES.contains(&fp.as_str())
            && n_tokens > 0
            && avg_logprob < cfg.ambiguous_min_logprob
        {
            return Some(Dropped::Phrase);
        }
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
    fn a_short_real_reply_survives_even_when_whisper_also_hallucinates_it() {
        let cfg = FilterConfig::default();
        // Clearly heard: these are things candidates really say.
        for text in ["Okay.", "So,", "Bye.", "Yeah.", "You?"] {
            assert_eq!(
                should_drop(text, -0.15, 2, &cfg),
                None,
                "{text:?} was heard clearly and must stay"
            );
        }
        // The same words, with whisper unsure, are its stock filler on silence.
        for text in ["Okay.", "So,", "Bye.", "you"] {
            assert_eq!(
                should_drop(text, -0.8, 2, &cfg),
                Some(Dropped::Phrase),
                "{text:?} with low confidence is a hallucination"
            );
        }
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
            should_drop(
                "Thank you, that covers my questions on caching.",
                OK,
                12,
                &cfg
            ),
            None
        );
        assert_eq!(
            should_drop("So, tell me about yourself.", OK, 7, &cfg),
            None
        );
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
