//! The fast phase's context (#54): the retrieved chunks and the job-description requirements
//! that go into every `FastRequest` (SPEC § Context and LLM).
//!
//! Loaded on the fast phase's own thread when a session starts: the stored sources are indexed
//! ([`dmn_context::Library`]) and the job description's ≤ 8 requirement bullets are read from the
//! cache or, the first time a job description is seen, summarised once by the model and cached
//! by content hash. That one call happens before the first question is served, never on a
//! question's path. Per question, [`Context::retrieve`] is a cheap staleness check (so a
//! `dmn sources add` in another terminal reaches the next question) plus a BM25 search.
//!
//! With `--no-store` the database is opened read-only: the sources are still used, but a new
//! summary is kept in memory only. With no database or no sources, every prompt is exactly the
//! M1 prompt.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use dmn_context::{Library, Retrieved, asks_for_a_story};
use dmn_core::wire;
use dmn_llm::{CancellationToken, LlmError, Provider, StoryText, summarise_job_description};
use dmn_store::Store;

/// How long caching a new summary may wait for the session writer (at load, not per question).
const CACHE_WRITE_WAIT: Duration = Duration::from_secs(2);

/// Where the context comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContextSource {
    /// No context, for tests that never reach a prompt.
    #[cfg(test)]
    Off,
    /// The session database. `write: false` (`--no-store`) opens it read-only.
    Db { path: PathBuf, write: bool },
}

/// Where the job requirements in the prompts came from.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum RequirementsFrom {
    /// No job description loaded.
    #[default]
    None,
    /// Read from the cache: this job description was summarised before.
    Cached,
    /// Summarised by the model at the start of this session.
    Summarised,
    /// The summary failed; prompts go without requirements.
    Failed,
}

impl RequirementsFrom {
    pub fn label(self) -> &'static str {
        match self {
            RequirementsFrom::None => "no job description",
            RequirementsFrom::Cached => "cached",
            RequirementsFrom::Summarised => "summarised now",
            RequirementsFrom::Failed => "summary failed",
        }
    }
}

/// What was loaded, for the run summary.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ContextInfo {
    pub sources: usize,
    pub passages: usize,
    /// Stories files loaded for STAR matching (#58).
    pub stories: usize,
    pub requirements: usize,
    pub from: RequirementsFrom,
}

/// One retrieved chunk as the replay record names it.
#[derive(Debug, Clone, PartialEq)]
pub struct ChunkRef {
    pub kind: &'static str,
    pub file: String,
    pub idx: i64,
    pub score: f64,
    pub tokens: usize,
}

/// What one retrieval put into a prompt.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Retrieval {
    /// The chunks as the prompt carries them: `kind (file): text`.
    pub chunks: Vec<String>,
    pub refs: Vec<ChunkRef>,
    /// The story the prompt carries whole (#58), when one was matched or chosen.
    pub story: Option<StoryText>,
    /// For a new behavioral question with a matched story: every matched story, best first
    /// (the first is `story`), for Swap. Empty for a regenerate or a swap, which keep the
    /// ranking the question already has.
    pub stories: Vec<String>,
    /// The staleness check, any reload, the story match and the search.
    pub took: Duration,
}

/// Which story a request carries (#58).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StoryPick<'a> {
    /// A new candidate: match the stories to this text when it asks for one ("tell me about a
    /// time…"). The whole utterance, not only the sentence the detector picked: "Give me an
    /// example of a time you had to cut scope. How did you decide what to cut?" is a candidate
    /// whose question sentence alone has no behavioral cue and names no theme.
    Match(&'a str),
    /// `r` or Swap on a question already built from this story (`stories/<file>`).
    Chosen(&'a str),
    /// No story: a regenerate of a question that had none.
    Off,
}

impl Retrieval {
    /// `"chunks"` for `dmn replay --json`.
    pub fn json(&self) -> serde_json::Value {
        serde_json::Value::Array(
            self.refs
                .iter()
                .map(|r| {
                    serde_json::json!({
                        "kind": r.kind,
                        "file": r.file,
                        "idx": r.idx,
                        "score": (r.score * 100.0).round() / 100.0,
                        "tokens": r.tokens,
                    })
                })
                .collect(),
        )
    }
}

/// The loaded context. Lives on the fast phase's thread.
#[derive(Default)]
pub struct Context {
    library: Library,
    store: Option<Store>,
    write: bool,
    requirements: Vec<String>,
    from: RequirementsFrom,
}

impl Context {
    /// No sources and no requirements: every prompt is the M1 prompt.
    pub fn empty() -> Self {
        Self::default()
    }

    /// Open the database named by `source` and load what is stored. `Ok(empty)` when there is
    /// no database yet or it is off.
    pub fn open(source: &ContextSource) -> Result<Self, String> {
        let (store, write) = match source {
            #[cfg(test)]
            ContextSource::Off => return Ok(Self::empty()),
            ContextSource::Db { path, write: true } => (
                Some(Store::open(path).map_err(|e| format!("{}: {e}", path.display()))?),
                true,
            ),
            ContextSource::Db { path, write: false } => (
                Store::open_read_only(path).map_err(|e| format!("{}: {e}", path.display()))?,
                false,
            ),
        };
        match store {
            Some(store) => Self::from_store(store, write),
            None => Ok(Self::empty()),
        }
    }

    /// Load from an open store. `write` allows caching a new job-description summary.
    pub fn from_store(store: Store, write: bool) -> Result<Self, String> {
        let library = Library::load(&store).map_err(|e| e.to_string())?;
        let mut ctx = Self {
            library,
            store: Some(store),
            write,
            ..Self::default()
        };
        ctx.read_cached_requirements()?;
        // From here on the connection is read on a question's path, where the session writer's
        // commits must not add a wait: a busy database is checked again next question instead.
        // Measured on `dmn replay`: with the 2 s busy timeout, 10 of 18 retrievals waited
        // 82–133 ms on the writer; the search itself takes 80–420 µs.
        if let Some(store) = &ctx.store {
            store
                .set_busy_timeout(Duration::ZERO)
                .map_err(|e| e.to_string())?;
        }
        Ok(ctx)
    }

    /// Take the requirements from the cache when this job description has been summarised.
    /// Nothing changes when the cache cannot be read.
    fn read_cached_requirements(&mut self) -> Result<(), String> {
        let (Some(store), Some(jd)) = (&self.store, self.library.job_description()) else {
            self.requirements.clear();
            self.from = RequirementsFrom::None;
            return Ok(());
        };
        match store.jd_summary(&jd.hash).map_err(|e| e.to_string())? {
            Some(bullets) => {
                self.requirements = bullets;
                self.from = RequirementsFrom::Cached;
            }
            None => {
                self.requirements.clear();
                self.from = RequirementsFrom::None;
            }
        }
        Ok(())
    }

    /// Whether a job description is loaded that has no requirements yet.
    pub fn needs_summary(&self) -> bool {
        self.library.job_description().is_some() && self.from == RequirementsFrom::None
    }

    /// Summarise the job description on `provider` if it has not been, and cache the bullets
    /// when the store is writable. Called once, before the first question is served.
    pub fn ensure_requirements<P: Provider + ?Sized>(
        &mut self,
        rt: &tokio::runtime::Runtime,
        provider: &P,
        cancel: &CancellationToken,
    ) -> Result<(), LlmError> {
        if !self.needs_summary() {
            return Ok(());
        }
        let Some(jd) = self.library.job_description() else {
            return Ok(());
        };
        let (text, hash) = (jd.text.clone(), jd.hash.clone());
        match rt.block_on(summarise_job_description(
            provider,
            &text,
            cancel.child_token(),
        )) {
            Ok(r) => {
                if self.write
                    && let Some(store) = &self.store
                {
                    // Off every question's path: this write may wait for the session writer.
                    let cached = store
                        .set_busy_timeout(CACHE_WRITE_WAIT)
                        .and_then(|()| store.put_jd_summary(&hash, &r.bullets, wire::unix_now()));
                    if let Err(e) = cached {
                        tracing::warn!("job requirements not cached: {e}");
                    }
                    if let Err(e) = store.set_busy_timeout(Duration::ZERO) {
                        tracing::warn!("context reader may now wait on the session writer: {e}");
                    }
                }
                self.requirements = r.bullets;
                self.from = RequirementsFrom::Summarised;
                Ok(())
            }
            Err(e) => {
                self.from = RequirementsFrom::Failed;
                Err(e)
            }
        }
    }

    /// The requirement bullets every prompt carries.
    pub fn requirements(&self) -> &[String] {
        &self.requirements
    }

    /// Reload the library if the stored sources changed since it was loaded. A new job
    /// description's requirements are taken from the cache if it was summarised before; it is
    /// never summarised here, on a question's path: the next session does that. Never waits:
    /// while the session writer holds the file, the check is skipped until the next question.
    fn refresh(&mut self) {
        let Some(store) = &self.store else {
            return;
        };
        match self.library.is_stale(store) {
            Ok(false) => {}
            Ok(true) => {
                let old = self.library.job_description().map(|j| j.hash.clone());
                match Library::load(store) {
                    Ok(library) => {
                        self.library = library;
                        let new = self.library.job_description().map(|j| j.hash.clone());
                        if new != old
                            && let Err(e) = self.read_cached_requirements()
                        {
                            tracing::debug!("job requirements not reloaded yet: {e}");
                        }
                        tracing::info!(
                            passages = self.library.passages(),
                            "context reloaded: the sources changed"
                        );
                    }
                    Err(e) => tracing::debug!("context not reloaded yet: {e}"),
                }
            }
            Err(e) => tracing::debug!("context staleness check skipped: {e}"),
        }
    }

    /// The chunks for `query`, after reloading the library if the sources changed.
    #[cfg(test)]
    pub fn retrieve(&mut self, query: &str) -> Retrieval {
        self.retrieve_for(query, StoryPick::Off)
    }

    /// [`Context::retrieve`], and the story `pick` names (#58). A matched story goes whole into
    /// the prompt, so its own chunks are left out of the chunk budget.
    pub fn retrieve_for(&mut self, query: &str, pick: StoryPick<'_>) -> Retrieval {
        let started = Instant::now();
        self.refresh();
        let mut out = Retrieval::default();
        let stories = self.library.stories();
        let story = match pick {
            StoryPick::Match(said) if asks_for_a_story(said) => {
                let ranked = stories.rank(said);
                out.stories = ranked.iter().map(|m| m.story.label.clone()).collect();
                ranked.first().map(|m| m.story)
            }
            StoryPick::Chosen(label) => stories.get(label),
            StoryPick::Match(_) | StoryPick::Off => None,
        };
        let skip = story.map(|s| s.path.clone());
        out.story = story.map(|s| StoryText {
            label: s.label.clone(),
            text: s.text.clone(),
        });
        let hits = self.library.retrieve_excluding(query, skip.as_deref());
        for Retrieved { passage, score } in hits {
            out.chunks.push(format!(
                "{} ({}): {}",
                passage.kind,
                passage.file(),
                passage.text
            ));
            out.refs.push(ChunkRef {
                kind: passage.kind.as_str(),
                file: passage.file().to_owned(),
                idx: passage.idx,
                score,
                tokens: passage.tokens,
            });
        }
        out.took = started.elapsed();
        out
    }

    pub fn info(&self) -> ContextInfo {
        ContextInfo {
            sources: self.library.sources(),
            passages: self.library.passages(),
            stories: self.library.stories().len(),
            requirements: self.requirements.len(),
            from: self.from,
        }
    }
}

/// The context fixtures (testdata/context) in a database file under `dir`, as `dmn sources
/// add` would store them.
#[cfg(test)]
pub(crate) fn fixture_db(dir: &std::path::Path) -> PathBuf {
    use dmn_context::{SourceKind, ingest};
    let fixture = |rel: &str| {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../testdata/context")
            .join(rel)
    };
    let path = dir.join("dmn.sqlite");
    let store = Store::open(&path).unwrap();
    for (rel, kind) in [
        ("resume.md", SourceKind::Resume),
        ("job-description.txt", SourceKind::JobDescription),
        ("notes", SourceKind::Notes),
    ] {
        ingest(&store, &fixture(rel), kind, 1_700_000_000).unwrap();
    }
    path
}

#[cfg(test)]
mod tests {
    use super::*;
    use dmn_context::{SourceKind, content_hash, ingest};

    fn requirements_reply() -> dmn_llm::MockProvider {
        dmn_llm::MockProvider::json(&serde_json::json!({
            "requirements": ["Go or Rust services", "Postgres sharding", "Kafka streaming"]
        }))
    }

    fn rt() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
    }

    #[test]
    fn the_job_description_is_summarised_once_and_then_read_from_the_cache() {
        let dir = tempfile::tempdir().unwrap();
        let db = fixture_db(dir.path());
        let source = ContextSource::Db {
            path: db.clone(),
            write: true,
        };
        let rt = rt();
        let cancel = CancellationToken::new();

        let first = requirements_reply();
        let mut ctx = Context::open(&source).unwrap();
        assert!(ctx.needs_summary());
        ctx.ensure_requirements(&rt, &first, &cancel).unwrap();
        assert_eq!(first.requests().len(), 1, "one call for a new JD");
        assert_eq!(ctx.info().from, RequirementsFrom::Summarised);
        assert_eq!(ctx.requirements().len(), 3);

        let second = requirements_reply();
        let mut again = Context::open(&source).unwrap();
        assert_eq!(again.info().from, RequirementsFrom::Cached);
        again.ensure_requirements(&rt, &second, &cancel).unwrap();
        assert!(second.requests().is_empty(), "the cache answers");
        assert_eq!(again.requirements(), ctx.requirements());

        // A different JD is a different hash, so it is summarised again.
        let store = Store::open(&db).unwrap();
        let jd = dir.path().join("jd2.txt");
        std::fs::write(&jd, "We need a Haskell engineer for trading systems.").unwrap();
        ingest(&store, &jd, SourceKind::JobDescription, 1_700_000_100).unwrap();
        let third = requirements_reply();
        let mut changed = Context::open(&source).unwrap();
        assert!(changed.needs_summary());
        changed.ensure_requirements(&rt, &third, &cancel).unwrap();
        assert_eq!(third.requests().len(), 1);
        let sent = &third.requests()[0].messages[1].content;
        assert!(sent.contains("Haskell"), "{sent}");
    }

    #[test]
    fn no_store_reads_the_sources_but_caches_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let db = fixture_db(dir.path());
        let ro = ContextSource::Db {
            path: db.clone(),
            write: false,
        };
        let rt = rt();
        let mock = requirements_reply();
        let mut ctx = Context::open(&ro).unwrap();
        assert_eq!(ctx.info().sources, 4);
        ctx.ensure_requirements(&rt, &mock, &CancellationToken::new())
            .unwrap();
        assert_eq!(ctx.requirements().len(), 3, "used for this run");
        let store = Store::open(&db).unwrap();
        let hash = content_hash(
            &Library::load(&store)
                .unwrap()
                .job_description()
                .unwrap()
                .text,
        );
        assert_eq!(store.jd_summary(&hash).unwrap(), None, "but not cached");

        let missing = ContextSource::Db {
            path: dir.path().join("none.sqlite"),
            write: false,
        };
        assert_eq!(
            Context::open(&missing).unwrap().info(),
            ContextInfo::default()
        );
        assert!(!dir.path().join("none.sqlite").exists(), "nothing created");
    }

    #[test]
    fn a_failed_summary_leaves_prompts_without_requirements() {
        let dir = tempfile::tempdir().unwrap();
        let db = fixture_db(dir.path());
        let mut ctx = Context::open(&ContextSource::Db {
            path: db,
            write: true,
        })
        .unwrap();
        let broken =
            dmn_llm::MockProvider::new(vec![dmn_llm::mock::Step::Error("model crashed".into())]);
        assert!(
            ctx.ensure_requirements(&rt(), &broken, &CancellationToken::new())
                .is_err()
        );
        assert_eq!(ctx.info().from, RequirementsFrom::Failed);
        assert!(ctx.requirements().is_empty());
        assert!(!ctx.needs_summary(), "not retried per question");
    }

    #[test]
    fn retrieval_names_its_chunks_and_a_source_added_mid_session_is_found() {
        let dir = tempfile::tempdir().unwrap();
        let db = fixture_db(dir.path());
        let mut ctx = Context::open(&ContextSource::Db {
            path: db.clone(),
            write: true,
        })
        .unwrap();
        let r = ctx.retrieve("How would you design a rate limiter?");
        assert!(!r.chunks.is_empty());
        assert!(
            r.chunks[0].starts_with("resume (resume.md): "),
            "{}",
            r.chunks[0]
        );
        assert_eq!(r.refs[0].file, "resume.md");
        assert_eq!(r.json()[0]["kind"], "resume");
        assert!(ctx.retrieve("Tell me about Erlang").chunks.is_empty());

        let note = dir.path().join("erlang.md");
        std::fs::write(&note, "Erlang supervision trees restart failed processes.").unwrap();
        ingest(
            &Store::open(&db).unwrap(),
            &note,
            SourceKind::Notes,
            1_700_000_200,
        )
        .unwrap();
        let r = ctx.retrieve("Tell me about Erlang");
        assert_eq!(r.refs.first().map(|c| c.file.as_str()), Some("erlang.md"));
    }

    #[test]
    fn retrieval_never_waits_on_the_session_writer() {
        let dir = tempfile::tempdir().unwrap();
        let db = fixture_db(dir.path());
        let mut ctx = Context::open(&ContextSource::Db {
            path: db.clone(),
            write: true,
        })
        .unwrap();
        // Another connection mid-commit, as the session writer is between segments.
        let writer = rusqlite::Connection::open(&db).unwrap();
        writer.execute_batch("BEGIN EXCLUSIVE").unwrap();
        let r = ctx.retrieve("How would you design a rate limiter?");
        assert!(
            r.took < Duration::from_millis(50),
            "{:?} while the file was locked",
            r.took
        );
        assert!(!r.chunks.is_empty(), "the loaded library still answers");
        writer.execute_batch("ROLLBACK").unwrap();
    }

    /// #58: stories are matched on the whole utterance. Replaying testdata/behavioral, the
    /// detector picked "How did you decide what to cut?" out of "Give me an example of a time
    /// you had to cut scope…", and that sentence alone asks for no story.
    #[test]
    fn stories_are_matched_on_the_whole_utterance_not_only_the_question_sentence() {
        let dir = tempfile::tempdir().unwrap();
        let db = fixture_db(dir.path());
        let stories =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../testdata/context/stories");
        ingest(
            &Store::open(&db).unwrap(),
            &stories,
            SourceKind::Stories,
            1_700_000_050,
        )
        .unwrap();
        let mut ctx = Context::open(&ContextSource::Db {
            path: db,
            write: false,
        })
        .unwrap();
        let sentence = "How did you decide what to cut?";
        let line = "Give me an example of a time you had to cut scope to ship something. \
                    How did you decide what to cut?";
        let alone = ctx.retrieve_for(sentence, StoryPick::Match(sentence));
        assert_eq!(alone.story, None);
        let whole = ctx.retrieve_for(sentence, StoryPick::Match(line));
        assert_eq!(
            whole.story.map(|s| s.label).as_deref(),
            Some("stories/audit-log-scope-cut.md")
        );
        assert!(whole.stories.len() > 1, "{:?}", whole.stories);
        let off = ctx.retrieve_for(sentence, StoryPick::Off);
        assert!(off.story.is_none() && off.stories.is_empty());
    }
}
