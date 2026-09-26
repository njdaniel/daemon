//! Cutting extracted text into overlapping chunks (SPEC: "chunks of about 300 tokens with 50
//! overlap").
//!
//! Tokens are approximated as words × 1.3 (see the crate docs), so a chunk is at most
//! [`CHUNK_WORDS`] words and consecutive chunks share [`OVERLAP_WORDS`] words. A chunk is a slice
//! of the original text, from its first word to its last, so line breaks inside it survive for
//! display in Resume Context.

/// Target chunk size, in approximate tokens.
pub const CHUNK_TOKENS: usize = 300;
/// Overlap between consecutive chunks, in approximate tokens.
pub const OVERLAP_TOKENS: usize = 50;

/// Words per chunk: the most whose [`approx_tokens`] stays within [`CHUNK_TOKENS`].
const CHUNK_WORDS: usize = CHUNK_TOKENS * 10 / 13;
/// Words shared by consecutive chunks: about [`OVERLAP_TOKENS`].
const OVERLAP_WORDS: usize = OVERLAP_TOKENS * 10 / 13;

/// Approximate token count of `words` whitespace-separated words: words × 1.3, rounded up.
pub fn approx_tokens(words: usize) -> usize {
    (words * 13).div_ceil(10)
}

/// One chunk of a document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chunk {
    pub text: String,
    /// Approximate tokens in `text`.
    pub tokens: usize,
}

/// Byte ranges of the whitespace-separated words in `text`.
fn word_spans(text: &str) -> Vec<(usize, usize)> {
    let mut spans = Vec::new();
    let mut start = None;
    for (i, c) in text.char_indices() {
        match (c.is_whitespace(), start) {
            (true, Some(s)) => {
                spans.push((s, i));
                start = None;
            }
            (false, None) => start = Some(i),
            _ => {}
        }
    }
    if let Some(s) = start {
        spans.push((s, text.len()));
    }
    spans
}

/// Cut `text` into chunks of at most [`CHUNK_TOKENS`] approximate tokens, each sharing about
/// [`OVERLAP_TOKENS`] with the one before. Empty or whitespace-only text gives no chunks.
pub fn chunk(text: &str) -> Vec<Chunk> {
    let words = word_spans(text);
    let mut out = Vec::new();
    let mut first = 0;
    while first < words.len() {
        let last = (first + CHUNK_WORDS).min(words.len());
        let (start, _) = words[first];
        let (_, end) = words[last - 1];
        out.push(Chunk {
            text: text[start..end].to_owned(),
            tokens: approx_tokens(last - first),
        });
        if last == words.len() {
            break;
        }
        first = last - OVERLAP_WORDS;
    }
    out
}

/// The text [`chunk`] was cut from, rebuilt from its chunks in order: the first whole, then each
/// later one without the [`OVERLAP_TOKENS`] words it repeats. Line breaks inside a chunk survive;
/// the join between two chunks is a space. Used for the job description (#54), which is
/// summarised as one document.
pub fn join<'a>(chunks: impl IntoIterator<Item = &'a str>) -> String {
    let mut out = String::new();
    for (i, text) in chunks.into_iter().enumerate() {
        let rest = if i == 0 {
            text
        } else {
            match word_spans(text).get(OVERLAP_WORDS) {
                Some(&(start, _)) => &text[start..],
                // A chunk is never shorter than the overlap plus one word, except the one it
                // overlaps with: nothing new in it.
                None => "",
            }
        };
        if rest.is_empty() {
            continue;
        }
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(rest);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(n: usize) -> String {
        (0..n)
            .map(|i| format!("w{i}"))
            .collect::<Vec<_>>()
            .join(" ")
    }

    #[test]
    fn the_word_budget_matches_the_token_targets() {
        assert!(approx_tokens(CHUNK_WORDS) <= CHUNK_TOKENS);
        assert!(
            approx_tokens(CHUNK_WORDS + 1) > CHUNK_TOKENS,
            "as big as fits"
        );
        assert_eq!(approx_tokens(OVERLAP_WORDS), OVERLAP_TOKENS);
    }

    #[test]
    fn short_text_is_one_chunk_and_empty_text_none() {
        assert!(chunk("").is_empty());
        assert!(chunk(" \n\t ").is_empty());
        let c = chunk("  Hello there,\nworld.  ");
        assert_eq!(c.len(), 1);
        assert_eq!(
            c[0].text, "Hello there,\nworld.",
            "trimmed, line break kept"
        );
        assert_eq!(c[0].tokens, approx_tokens(3));
    }

    #[test]
    fn long_text_is_cut_into_bounded_chunks_that_overlap_and_cover_everything() {
        let text = words(1000);
        let chunks = chunk(&text);
        assert!(chunks.len() > 1);
        for c in &chunks {
            assert!(c.tokens <= CHUNK_TOKENS, "{} tokens", c.tokens);
        }
        for pair in chunks.windows(2) {
            let prev: Vec<&str> = pair[0].text.split_whitespace().collect();
            let next: Vec<&str> = pair[1].text.split_whitespace().collect();
            assert_eq!(
                &prev[prev.len() - OVERLAP_WORDS..],
                &next[..OVERLAP_WORDS],
                "consecutive chunks share the overlap"
            );
        }
        let last = chunks.last().unwrap().text.split_whitespace().last();
        assert_eq!(last, Some("w999"), "the tail is not dropped");
        assert_eq!(chunks[0].text.split_whitespace().next(), Some("w0"));
    }

    #[test]
    fn a_text_just_over_one_chunk_ends_with_a_short_tail() {
        let chunks = chunk(&words(CHUNK_WORDS + 1));
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[1].text.split_whitespace().count(), OVERLAP_WORDS + 1);
    }

    #[test]
    fn join_undoes_chunk() {
        for n in [0, 1, 5, CHUNK_WORDS, CHUNK_WORDS + 1, 1000] {
            let text = words(n);
            let chunks = chunk(&text);
            let joined = join(chunks.iter().map(|c| c.text.as_str()));
            assert_eq!(joined, text, "{n} words");
        }
        let lines = "line one\nline two\n".repeat(200);
        let joined = join(chunk(&lines).iter().map(|c| c.text.as_str()));
        assert_eq!(
            joined.split_whitespace().collect::<Vec<_>>(),
            lines.split_whitespace().collect::<Vec<_>>(),
            "every word once, in order"
        );
    }

    #[test]
    fn multibyte_text_is_sliced_on_char_boundaries() {
        let text = "Café naïve résumé — 東京 ".repeat(200);
        for c in chunk(&text) {
            assert!(!c.text.is_empty());
        }
    }
}
