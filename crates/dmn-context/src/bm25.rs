//! Okapi BM25 over chunk text (SPEC § Context and LLM: "BM25 over chunks (pure Rust, no vector DB
//! in v1)").
//!
//! Terms are lowercased alphanumeric runs with a short English stop list removed and a light
//! plural fold ("buckets" and "bucket" match), which is enough for resume and notes text matched
//! against a spoken question. Parameters are the usual `k1 = 1.2`, `b = 0.75`; the idf is the
//! non-negative `ln(1 + (N - df + 0.5) / (df + 0.5))`, so a term found in every chunk still counts
//! a little instead of subtracting.

use std::collections::HashMap;

const K1: f64 = 1.2;
const B: f64 = 0.75;

/// Words that say nothing about which chunk a question is about: function words, and the framing
/// every interview question shares ("tell me about a time you…", "how would you…").
const STOP: &[&str] = &[
    "a", "about", "after", "all", "also", "am", "an", "and", "any", "are", "as", "at", "be",
    "been", "before", "being", "between", "both", "but", "by", "can", "could", "describe", "did",
    "do", "does", "doing", "each", "explain", "for", "from", "get", "give", "go", "had", "has",
    "have", "having", "he", "her", "here", "him", "his", "how", "if", "in", "into", "is", "it",
    "its", "just", "kind", "let", "like", "me", "more", "most", "my", "no", "not", "now", "of",
    "okay", "on", "one", "only", "or", "other", "our", "out", "over", "please", "really", "say",
    "she", "should", "so", "some", "such", "talk", "tell", "than", "that", "the", "their", "them",
    "then", "there", "these", "they", "thing", "things", "think", "this", "those", "through",
    "time", "to", "too", "under", "up", "us", "very", "walk", "want", "was", "way", "we", "well",
    "were", "what", "when", "where", "which", "while", "who", "why", "will", "with", "would",
    "yeah", "yes", "you", "your", "yours",
];

/// The terms of `text`, in order, repeats kept.
pub fn terms(text: &str) -> Vec<String> {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(str::to_lowercase)
        .filter(|w| w.chars().count() > 1 || w.chars().all(|c| c.is_ascii_digit()))
        .filter(|w| !STOP.contains(&w.as_str()))
        .map(fold)
        .collect()
}

/// A light plural fold: "strategies" → "strategy", "classes" → "class", "buckets" → "bucket";
/// "class", "status", "redis" and short words are left alone.
fn fold(word: String) -> String {
    let n = word.chars().count();
    if n > 4 && word.ends_with("ies") {
        return format!("{}y", &word[..word.len() - 3]);
    }
    if word.ends_with("sses") {
        return word[..word.len() - 2].to_owned();
    }
    if n > 3
        && word.ends_with('s')
        && !word.ends_with("ss")
        && !word.ends_with("us")
        && !word.ends_with("is")
    {
        return word[..word.len() - 1].to_owned();
    }
    word
}

/// One indexed document: its length in terms and each term's frequency.
#[derive(Debug, Clone)]
struct Doc {
    len: f64,
    tf: HashMap<String, f64>,
}

/// A BM25 index over a fixed list of documents, addressed by their position in that list.
#[derive(Debug, Clone, Default)]
pub struct Bm25 {
    docs: Vec<Doc>,
    /// How many documents contain each term.
    df: HashMap<String, f64>,
    avg_len: f64,
}

impl Bm25 {
    pub fn new<'a>(texts: impl IntoIterator<Item = &'a str>) -> Self {
        let mut docs = Vec::new();
        let mut df: HashMap<String, f64> = HashMap::new();
        for text in texts {
            let mut tf: HashMap<String, f64> = HashMap::new();
            let words = terms(text);
            for w in &words {
                *tf.entry(w.clone()).or_default() += 1.0;
            }
            for w in tf.keys() {
                *df.entry(w.clone()).or_default() += 1.0;
            }
            docs.push(Doc {
                len: words.len() as f64,
                tf,
            });
        }
        let total: f64 = docs.iter().map(|d| d.len).sum();
        let avg_len = if docs.is_empty() {
            0.0
        } else {
            total / docs.len() as f64
        };
        Self { docs, df, avg_len }
    }

    pub fn len(&self) -> usize {
        self.docs.len()
    }

    pub fn is_empty(&self) -> bool {
        self.docs.is_empty()
    }

    /// Every document with a positive score for `query`, best first; ties keep document order.
    pub fn search(&self, query: &str) -> Vec<(usize, f64)> {
        let mut q = terms(query);
        q.sort();
        q.dedup();
        if q.is_empty() || self.docs.is_empty() {
            return Vec::new();
        }
        let n = self.docs.len() as f64;
        let idf: Vec<(&String, f64)> = q
            .iter()
            .filter_map(|t| {
                let df = *self.df.get(t)?;
                Some((t, (1.0 + (n - df + 0.5) / (df + 0.5)).ln()))
            })
            .collect();
        let mut scored: Vec<(usize, f64)> = self
            .docs
            .iter()
            .enumerate()
            .filter_map(|(i, d)| {
                let norm = K1 * (1.0 - B + B * d.len / self.avg_len.max(1.0));
                let score: f64 = idf
                    .iter()
                    .filter_map(|(t, idf)| {
                        let tf = *d.tf.get(*t)?;
                        Some(idf * tf * (K1 + 1.0) / (tf + norm))
                    })
                    .sum();
                (score > 0.0).then_some((i, score))
            })
            .collect();
        scored.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
        scored
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terms_drop_framing_and_fold_plurals() {
        assert_eq!(
            terms("Tell me about a time you designed token buckets in Redis!"),
            ["designed", "token", "bucket", "redis"]
        );
        assert_eq!(
            terms("Strategies, classes; status: p99 of 9 ms"),
            ["strategy", "class", "status", "p99", "9", "ms"]
        );
    }

    #[test]
    fn the_document_that_mentions_the_query_ranks_first() {
        let idx = Bm25::new([
            "Built the shipment event pipeline on Kafka with idempotent consumers.",
            "Designed a global rate limiter using token buckets in regional Redis.",
            "Mentored four engineers and ran the interview loop.",
        ]);
        let hits = idx.search("How would you design a rate limiter?");
        assert_eq!(hits.first().map(|h| h.0), Some(1));
        let hits = idx.search("Tell me about Kafka consumers");
        assert_eq!(hits.first().map(|h| h.0), Some(0));
        assert!(
            idx.search("Where do you see yourself").is_empty(),
            "no shared term, no hit"
        );
    }

    #[test]
    fn a_rare_term_outweighs_a_common_one() {
        let idx = Bm25::new([
            "postgres postgres postgres migration",
            "postgres clickhouse",
            "postgres backups",
        ]);
        let hits = idx.search("postgres clickhouse");
        assert_eq!(hits[0].0, 1, "clickhouse is in one document only");
    }

    #[test]
    fn an_empty_index_or_query_finds_nothing() {
        assert!(Bm25::new([]).search("redis").is_empty());
        assert!(Bm25::new(["redis"]).search("the and of").is_empty());
    }
}
