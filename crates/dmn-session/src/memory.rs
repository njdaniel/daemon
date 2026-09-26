//! Session memory (#57, SPEC § Session memory): the rolling summary, one line per answered
//! question, and the promises the candidate made ("I'll come back to clock skew").
//!
//! A promise stays open until the candidate covers it. "Covers" is a plain text match, on
//! purpose simple and deterministic so a replayed action log closes the same promises at the
//! same segments: the promise's content words (lowercase, three letters or more, common function
//! words left out) are looked for among the words of each later You final. A promise of one or
//! two such words needs all of them; a longer one needs at least two thirds. A word matches when
//! it equals a said word or one is the other plus a plural `s` ("shard" / "shards").

use serde::{Deserialize, Serialize};

/// How many summary lines each prompt carries (SPEC: "the last 8 summary lines").
pub const PROMPT_LINES: usize = 8;

/// One summary line, tagged with the question it summarises.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SummaryLine {
    pub candidate: u64,
    pub line: String,
}

/// Something the candidate said they would come back to, from their answer to `candidate`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Promise {
    pub candidate: u64,
    pub text: String,
    /// A later You segment covered it; it no longer shows.
    pub mentioned: bool,
}

impl Promise {
    /// How it reads in Things I Should Mention.
    pub fn mention_item(&self) -> String {
        format!("Come back to {}: you said you would", self.text)
    }
}

/// Function words that say nothing about a topic.
const STOP: [&str; 24] = [
    "the", "and", "for", "that", "this", "with", "from", "into", "about", "back", "come", "return",
    "later", "will", "would", "you", "your", "our", "its", "how", "what", "why", "when", "then",
];

fn words(text: &str) -> Vec<String> {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|w| w.chars().count() >= 3)
        .map(str::to_lowercase)
        .filter(|w| !STOP.contains(&w.as_str()))
        .collect()
}

fn same_word(a: &str, b: &str) -> bool {
    a == b || a.strip_suffix('s') == Some(b) || b.strip_suffix('s') == Some(a)
}

/// Whether `said` covers `promise`; see the module doc for the rule.
pub fn covers(promise: &str, said: &str) -> bool {
    let wanted = words(promise);
    if wanted.is_empty() {
        return false;
    }
    let said = words(said);
    let hits = wanted
        .iter()
        .filter(|w| said.iter().any(|s| same_word(w, s)))
        .count();
    if wanted.len() <= 2 {
        hits == wanted.len()
    } else {
        hits * 3 >= wanted.len() * 2
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_promise_is_covered_by_its_words_in_any_order() {
        assert!(covers(
            "clock skew",
            "Right, so back to clock skew: we bound it with NTP."
        ));
        assert!(covers("clock skew", "Skew between the clocks is small."));
        assert!(!covers("clock skew", "The clock on the wall says three."));
        assert!(covers(
            "the sharding strategy for hot keys",
            "For hot keys the shard strategy is to split them."
        ));
        assert!(!covers(
            "the sharding strategy for hot keys",
            "Keys are hashed."
        ));
    }

    #[test]
    fn a_promise_of_only_function_words_is_never_covered() {
        assert!(!covers("that", "that that that"));
        assert!(!covers("", "anything"));
    }
}
