//! Splitting one transcript line into sentences.
//!
//! An interviewer turn is rarely one sentence: "Sorry about that. Where were we. Right, the
//! pipeline. Let's switch gears. Suppose you have a service whose p99 latency doubled..." is one
//! VAD utterance and one [`Segment`](dmn_transcript::Segment). Scoring the whole line would judge
//! the question by the filler around it, and the candidate text handed to the LLM would carry the
//! filler too, so the detector scores sentences and keeps the one that reads as the question.

/// One sentence of a transcript line, with where it sits in the line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Sentence<'a> {
    pub text: &'a str,
    /// Index among the line's sentences, 0-based.
    pub index: usize,
    /// True when this is the last sentence of the line.
    pub last: bool,
}

/// Abbreviations that end in a period without ending a sentence. Whisper writes few of these, so
/// the list is short on purpose: a missed split costs a slightly long candidate, a wrong split
/// costs a truncated question.
const ABBREVIATIONS: [&str; 10] = [
    "mr", "mrs", "ms", "dr", "prof", "sr", "jr", "vs", "etc", "e.g",
];

/// Split a line into sentences at `.`, `?`, `!` and `…`, keeping the terminator with its sentence.
///
/// Whisper punctuates its output, which is what makes this possible at all; a line with no
/// terminator (a partial, or speech whisper did not punctuate) comes back as one sentence.
pub fn split(line: &str) -> Vec<Sentence<'_>> {
    let mut out = Vec::new();
    let mut start = 0;
    let bytes = line.as_bytes();
    for (i, ch) in line.char_indices() {
        if !matches!(ch, '.' | '?' | '!' | '…') {
            continue;
        }
        // "3.5" and "p99." style decimals: a period between digits is not a sentence end.
        if ch == '.'
            && i > 0
            && bytes[i - 1].is_ascii_digit()
            && line[i + ch.len_utf8()..]
                .chars()
                .next()
                .is_some_and(|c| c.is_ascii_digit())
        {
            continue;
        }
        let end = i + ch.len_utf8();
        // Run on through ?!. clusters and closing quotes so "Really?!" stays one sentence.
        let end = line[end..]
            .char_indices()
            .take_while(|(_, c)| matches!(c, '.' | '?' | '!' | '…' | '"' | '\'' | '”' | '’'))
            .map(|(j, c)| end + j + c.len_utf8())
            .last()
            .unwrap_or(end);
        let piece = line[start..end].trim();
        if ch == '.' && is_abbreviation(piece) {
            continue;
        }
        if !piece.is_empty() {
            out.push(piece);
        }
        start = end;
    }
    let tail = line[start..].trim();
    if !tail.is_empty() {
        out.push(tail);
    }
    let len = out.len();
    out.into_iter()
        .enumerate()
        .map(|(index, text)| Sentence {
            text,
            index,
            last: index + 1 == len,
        })
        .collect()
}

fn is_abbreviation(piece: &str) -> bool {
    let word = piece
        .trim_end_matches('.')
        .rsplit([' ', '\t'])
        .next()
        .unwrap_or_default()
        .trim_matches(|c: char| !c.is_alphanumeric() && c != '.')
        .to_ascii_lowercase();
    ABBREVIATIONS.contains(&word.as_str())
}

/// Words of a sentence, lowercased and stripped of punctuation. Used by every cue matcher so
/// "How's" and "how" compare the same way.
pub fn words(text: &str) -> Vec<String> {
    text.split_whitespace()
        .map(|w| {
            w.trim_matches(|c: char| !c.is_alphanumeric() && c != '\'')
                .to_ascii_lowercase()
        })
        .filter(|w| !w.is_empty())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn texts(line: &str) -> Vec<&str> {
        split(line).into_iter().map(|s| s.text).collect()
    }

    #[test]
    fn a_line_splits_at_terminators_and_keeps_them() {
        assert_eq!(
            texts("Let's switch gears. How would you design a rate limiter?"),
            vec![
                "Let's switch gears.",
                "How would you design a rate limiter?"
            ]
        );
        assert_eq!(texts("Hello? Can you hear me okay?").len(), 2);
    }

    #[test]
    fn an_unpunctuated_line_is_one_sentence() {
        assert_eq!(
            texts("how would you design a rate limiter"),
            vec!["how would you design a rate limiter"]
        );
        assert_eq!(texts("   "), Vec::<&str>::new());
    }

    #[test]
    fn decimals_and_abbreviations_do_not_split() {
        assert_eq!(
            texts("The p99 went from 1.5 to 3.5 seconds. What would you check?"),
            vec![
                "The p99 went from 1.5 to 3.5 seconds.",
                "What would you check?"
            ]
        );
        assert_eq!(texts("Ask Dr. Chen about it.").len(), 1);
    }

    #[test]
    fn clustered_terminators_stay_with_their_sentence() {
        assert_eq!(
            texts("Really?! Tell me more."),
            vec!["Really?!", "Tell me more."]
        );
    }

    #[test]
    fn index_and_last_describe_position() {
        let s = split("One. Two. Three?");
        assert_eq!(s.len(), 3);
        assert_eq!(s[0].index, 0);
        assert!(!s[0].last);
        assert!(s[2].last);
    }

    #[test]
    fn words_lowercases_and_strips_punctuation() {
        assert_eq!(words("How's the weather?"), vec!["how's", "the", "weather"]);
        assert_eq!(words("  "), Vec::<String>::new());
    }
}
