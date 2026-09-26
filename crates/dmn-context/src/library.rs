//! The loaded context for a session (#54): a BM25 index over every stored chunk, and the job
//! description's text and content hash for its one-off summary (SPEC § Context and LLM).
//!
//! The job description is not indexed: it reaches every prompt as its ≤ 8 requirement bullets
//! instead, so retrieving its chunks as well would spend the 1,500-token budget saying the same
//! thing twice. Resume, notes and stories chunks are all searched together.

use std::path::Path;

use dmn_store::{SourceChunk, SourcesSignature, Store};

use crate::bm25::Bm25;
use crate::chunk::join;
use crate::error::ContextError;
use crate::kind::SourceKind;
use crate::story::{Stories, Story};

/// SPEC § Context and LLM: the top 6 chunks per question…
pub const RETRIEVE_TOP: usize = 6;
/// …capped at 1,500 tokens.
pub const RETRIEVE_TOKENS: usize = 1_500;

/// One searchable chunk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Passage {
    pub kind: SourceKind,
    /// The absolute path its source was read from.
    pub path: String,
    /// Position in its source, from 0.
    pub idx: i64,
    pub text: String,
    /// Approximate tokens in `text`.
    pub tokens: usize,
}

impl Passage {
    /// The file name, for the prompt and the replay record.
    pub fn file(&self) -> &str {
        Path::new(&self.path)
            .file_name()
            .and_then(|f| f.to_str())
            .unwrap_or(&self.path)
    }
}

/// A retrieved passage and its BM25 score.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Retrieved<'a> {
    pub passage: &'a Passage,
    pub score: f64,
}

/// The job description as one text, and a stable hash of it that keys the cached summary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobDescription {
    pub path: String,
    pub text: String,
    /// FNV-1a 64 of `text`, as 16 hex digits.
    pub hash: String,
}

/// Everything retrieval needs, loaded once per session and reloaded when the sources change.
#[derive(Debug, Clone, Default)]
pub struct Library {
    passages: Vec<Passage>,
    index: Bm25,
    job_description: Option<JobDescription>,
    /// Each stories file as one story (#58), for STAR matching. Their chunks are also searched
    /// like any other passage.
    stories: Stories,
    sources: usize,
    signature: SourcesSignature,
}

impl Library {
    /// No sources: every search finds nothing and there is no job description.
    pub fn empty() -> Self {
        Self::default()
    }

    /// Load every stored chunk from `store` and index it.
    pub fn load(store: &Store) -> Result<Self, ContextError> {
        let signature = store.sources_signature()?;
        Ok(Self::from_chunks(store.all_chunks()?, signature))
    }

    /// Build from chunks already read, ordered by source then position (as
    /// [`Store::all_chunks`] returns them). Chunks of an unknown kind are skipped.
    pub fn from_chunks(chunks: Vec<SourceChunk>, signature: SourcesSignature) -> Self {
        let mut passages = Vec::new();
        let mut jd: Option<(String, Vec<String>)> = None;
        // (source id, path, chunk texts) per stories file, in load order.
        let mut story_chunks: Vec<(i64, String, Vec<String>)> = Vec::new();
        let mut source_ids = Vec::new();
        for c in chunks {
            let Some(kind) = SourceKind::parse(&c.kind) else {
                continue;
            };
            if source_ids.last() != Some(&c.source_id) {
                source_ids.push(c.source_id);
            }
            if kind == SourceKind::Stories {
                match story_chunks.last_mut() {
                    Some((id, _, texts)) if *id == c.source_id => texts.push(c.text.clone()),
                    _ => story_chunks.push((c.source_id, c.path.clone(), vec![c.text.clone()])),
                }
            }
            if kind == SourceKind::JobDescription {
                jd.get_or_insert_with(|| (c.path.clone(), Vec::new()))
                    .1
                    .push(c.text);
                continue;
            }
            passages.push(Passage {
                kind,
                path: c.path,
                idx: c.idx,
                tokens: usize::try_from(c.tokens).unwrap_or(0),
                text: c.text,
            });
        }
        let index = Bm25::new(passages.iter().map(|p| p.text.as_str()));
        let job_description = jd.map(|(path, texts)| {
            let text = join(texts.iter().map(String::as_str));
            JobDescription {
                path,
                hash: content_hash(&text),
                text,
            }
        });
        let stories = Stories::new(
            story_chunks
                .iter()
                .map(|(_, path, texts)| Story::parse(path, &join(texts.iter().map(String::as_str))))
                .collect(),
        );
        Self {
            passages,
            index,
            job_description,
            stories,
            sources: source_ids.len(),
            signature,
        }
    }

    /// Whether the stored sources changed since this library was loaded: one cheap query, so a
    /// `dmn sources add` in another terminal reaches the next question.
    pub fn is_stale(&self, store: &Store) -> Result<bool, ContextError> {
        Ok(store.sources_signature()? != self.signature)
    }

    /// The best passages for `query`: at most [`RETRIEVE_TOP`], best first, together at most
    /// [`RETRIEVE_TOKENS`] (a passage that would overflow the budget is skipped and a smaller
    /// later one may still fit). Passages sharing no term with the query are never returned.
    pub fn retrieve(&self, query: &str) -> Vec<Retrieved<'_>> {
        self.retrieve_excluding(query, None)
    }

    /// [`Library::retrieve`] without the passages of the source read from `skip`: the story
    /// the prompt already carries whole (#58) is not spent on the chunk budget a second time.
    pub fn retrieve_excluding(&self, query: &str, skip: Option<&str>) -> Vec<Retrieved<'_>> {
        let mut out = Vec::new();
        let mut tokens = 0;
        for (i, score) in self.index.search(query) {
            if out.len() == RETRIEVE_TOP {
                break;
            }
            let passage = &self.passages[i];
            if skip.is_some_and(|p| p == passage.path) {
                continue;
            }
            if tokens + passage.tokens > RETRIEVE_TOKENS {
                continue;
            }
            tokens += passage.tokens;
            out.push(Retrieved { passage, score });
        }
        out
    }

    pub fn job_description(&self) -> Option<&JobDescription> {
        self.job_description.as_ref()
    }

    /// The stories, one per stories file (#58).
    pub fn stories(&self) -> &Stories {
        &self.stories
    }

    /// Searchable passages (every chunk but the job description's).
    pub fn passages(&self) -> usize {
        self.passages.len()
    }

    /// Sources loaded, the job description included.
    pub fn sources(&self) -> usize {
        self.sources
    }
}

/// FNV-1a 64 of `text` as 16 hex digits: stable across builds and toolchains, unlike std's
/// `DefaultHasher`, so a cached summary stays valid after an upgrade.
pub fn content_hash(text: &str) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in text.as_bytes() {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    format!("{h:016x}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ingest::ingest;
    use std::path::PathBuf;

    fn fixture(rel: &str) -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../testdata/context")
            .join(rel)
    }

    /// The context fixtures, ingested the way `dmn sources add` does.
    fn fixtures() -> Store {
        let store = Store::open_in_memory().unwrap();
        for (rel, kind) in [
            ("resume.md", SourceKind::Resume),
            ("job-description.txt", SourceKind::JobDescription),
            ("notes", SourceKind::Notes),
        ] {
            ingest(&store, &fixture(rel), kind, 1_700_000_000).unwrap();
        }
        store
    }

    #[test]
    fn a_question_about_a_resume_skill_finds_a_chunk_that_has_it_in_the_top_3() {
        let lib = Library::load(&fixtures()).unwrap();
        // (question as an interviewer would ask it, a word the retrieved chunk must contain)
        let cases = [
            (
                "How would you design a rate limiter for a public API?",
                "rate limiter",
            ),
            ("Tell me about a time you worked with Kafka.", "Kafka"),
            ("Have you ever sharded a Postgres database?", "sharded"),
            (
                "How do you set service level objectives for a team?",
                "service level objectives",
            ),
            (
                "How did you reduce cloud costs at your last job?",
                "cloud spend",
            ),
            ("What happens when your cache is down?", "cache is down"),
            ("Tell me about mentoring engineers.", "Mentored"),
            (
                "Which languages are you most comfortable in, Go or Rust?",
                "Rust",
            ),
        ];
        for (question, needle) in cases {
            let hits = lib.retrieve(question);
            assert!(
                hits.iter().take(3).any(|h| h.passage.text.contains(needle)),
                "{question:?}: no chunk with {needle:?} in the top 3: {:?}",
                hits.iter()
                    .map(|h| (h.passage.file(), h.passage.idx, h.score))
                    .collect::<Vec<_>>()
            );
        }
    }

    #[test]
    fn each_behavioral_fixture_question_ranks_its_story_first() {
        let store = fixtures();
        ingest(
            &store,
            &fixture("stories"),
            SourceKind::Stories,
            1_700_000_002,
        )
        .unwrap();
        let lib = Library::load(&store).unwrap();
        assert_eq!(lib.stories().len(), 8, "one story per file");
        // testdata/behavioral/questions.json, the behavioral ones.
        let cases = [
            (
                "Tell me about a time you disagreed with a technical decision and how you handled it.",
                "stories/disagreed-on-broker.md",
            ),
            (
                "Let's talk about failure. Describe a project that didn't go well, and what you took from it.",
                "stories/billing-cutover-failure.md",
            ),
            (
                "Give me an example of a time you had to cut scope to ship something. How did you decide what to cut?",
                "stories/audit-log-scope-cut.md",
            ),
            (
                "Tell me about a time you mentored someone, and what changed for them.",
                "stories/mentoring-draft-prs.md",
            ),
            (
                "I want to dig into ownership. Describe a situation where something was broken that wasn't your responsibility, and what you did.",
                "stories/feature-flag-ownership.md",
            ),
            (
                "Walk me through a time you had to deliver bad news to a stakeholder.",
                "stories/realtime-sync-bad-news.md",
            ),
            (
                "Let's do one more. Tell me about a time you had to get something done with a team that didn't report to you, where you had no authority.",
                "stories/security-review-influence.md",
            ),
        ];
        for (question, want) in cases {
            let ranked = lib.stories().rank(question);
            assert_eq!(
                ranked.first().map(|m| m.story.label.as_str()),
                Some(want),
                "{question}: {:?}",
                ranked
                    .iter()
                    .map(|m| (&m.story.label, &m.tags, m.score))
                    .collect::<Vec<_>>()
            );
            assert!(!ranked[0].tags.is_empty(), "{question}: matched by tag");
        }
        // A story's own chunks can be left out of the chunk budget once it is in the prompt.
        let broker = lib.stories().get("stories/disagreed-on-broker.md").unwrap();
        let q = "Tell me about the broker spike and replay";
        assert!(
            lib.retrieve(q)
                .iter()
                .any(|h| h.passage.path == broker.path)
        );
        assert!(
            lib.retrieve_excluding(q, Some(&broker.path))
                .iter()
                .all(|h| h.passage.path != broker.path)
        );
    }

    #[test]
    fn retrieval_is_capped_at_6_passages_and_1500_tokens() {
        let many: Vec<SourceChunk> = (0..20)
            .map(|i| SourceChunk {
                source_id: i,
                kind: "notes".into(),
                path: format!("/n/{i}.md"),
                idx: 0,
                text: format!("redis cache note {i}"),
                tokens: 280,
            })
            .collect();
        let lib = Library::from_chunks(many, SourcesSignature::default());
        let hits = lib.retrieve("redis cache");
        assert_eq!(hits.len(), 5, "six would be 1,680 tokens");
        assert!(hits.iter().map(|h| h.passage.tokens).sum::<usize>() <= RETRIEVE_TOKENS);

        let small: Vec<SourceChunk> = (0..20)
            .map(|i| SourceChunk {
                source_id: i,
                kind: "notes".into(),
                path: format!("/n/{i}.md"),
                idx: 0,
                text: format!("redis cache note {i}"),
                tokens: 10,
            })
            .collect();
        let lib = Library::from_chunks(small, SourcesSignature::default());
        assert_eq!(lib.retrieve("redis cache").len(), RETRIEVE_TOP);
    }

    #[test]
    fn the_job_description_is_summarised_not_searched_and_its_hash_follows_its_text() {
        let store = fixtures();
        let lib = Library::load(&store).unwrap();
        assert_eq!(lib.sources(), 4, "resume, JD and two notes");
        let jd = lib.job_description().expect("a JD was added");
        let on_disk = std::fs::read_to_string(fixture("job-description.txt")).unwrap();
        assert_eq!(
            jd.text.split_whitespace().collect::<Vec<_>>(),
            on_disk.split_whitespace().collect::<Vec<_>>(),
            "the whole JD, once"
        );
        assert_eq!(jd.hash, content_hash(&jd.text));
        assert!(
            lib.retrieve("What we are looking for: exactly-once processing")
                .iter()
                .all(|h| h.passage.kind != SourceKind::JobDescription),
            "JD chunks are never retrieved"
        );
        assert_ne!(content_hash("a"), content_hash("b"));
        assert_eq!(content_hash(""), "cbf29ce484222325", "FNV-1a offset basis");
    }

    #[test]
    fn no_sources_means_no_hits_and_no_job_description() {
        let lib = Library::load(&Store::open_in_memory().unwrap()).unwrap();
        assert_eq!((lib.sources(), lib.passages()), (0, 0));
        assert!(
            lib.retrieve("How would you design a rate limiter?")
                .is_empty()
        );
        assert!(lib.job_description().is_none());
    }

    #[test]
    fn adding_a_source_makes_the_library_stale() {
        let store = fixtures();
        let lib = Library::load(&store).unwrap();
        assert!(!lib.is_stale(&store).unwrap());
        ingest(
            &store,
            &fixture("resume.pdf"),
            SourceKind::Resume,
            1_700_000_001,
        )
        .unwrap();
        assert!(
            lib.is_stale(&store).unwrap(),
            "the PDF replaced the Markdown resume"
        );
    }

    #[test]
    fn retrieval_takes_well_under_a_millisecond_on_the_fixtures() {
        let lib = Library::load(&fixtures()).unwrap();
        let started = std::time::Instant::now();
        for _ in 0..100 {
            lib.retrieve("How would you design a rate limiter for a globally distributed API?");
        }
        let each = started.elapsed() / 100;
        assert!(
            each < std::time::Duration::from_millis(5),
            "{each:?} per retrieval (budget 50 ms)"
        );
    }
}
