//! Word error rate for `dmn bench` and the eval harness: word-level Levenshtein distance over
//! normalised text (lower case, punctuation dropped, apostrophes kept), divided by the reference
//! word count.

fn words(s: &str) -> Vec<String> {
    s.split_whitespace()
        .map(|w| {
            w.chars()
                .filter(|c| c.is_alphanumeric() || *c == '\'')
                .flat_map(char::to_lowercase)
                .collect::<String>()
        })
        .filter(|w| !w.is_empty())
        .collect()
}

/// `(substitutions + deletions + insertions) / reference words`; 0.0 for two empty strings.
pub fn word_error_rate(reference: &str, hypothesis: &str) -> f64 {
    let r = words(reference);
    let h = words(hypothesis);
    if r.is_empty() {
        return if h.is_empty() { 0.0 } else { 1.0 };
    }
    let mut prev: Vec<usize> = (0..=h.len()).collect();
    let mut cur = vec![0usize; h.len() + 1];
    for (i, rw) in r.iter().enumerate() {
        cur[0] = i + 1;
        for (j, hw) in h.iter().enumerate() {
            let sub = prev[j] + usize::from(rw != hw);
            cur[j + 1] = sub.min(prev[j + 1] + 1).min(cur[j] + 1);
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[h.len()] as f64 / r.len() as f64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identical_modulo_case_and_punctuation_is_zero() {
        assert_eq!(
            word_error_rate("How would you design it?", "how would you design it"),
            0.0
        );
        assert_eq!(word_error_rate("It's fine.", "it's fine"), 0.0);
    }

    #[test]
    fn counts_substitutions_insertions_deletions() {
        // one substitution in four words
        assert_eq!(word_error_rate("a b c d", "a x c d"), 0.25);
        // one deletion
        assert_eq!(word_error_rate("a b c d", "a b d"), 0.25);
        // two insertions
        assert_eq!(word_error_rate("a b", "a q b r"), 1.0);
        assert_eq!(word_error_rate("", ""), 0.0);
        assert_eq!(word_error_rate("", "x"), 1.0);
    }
}
