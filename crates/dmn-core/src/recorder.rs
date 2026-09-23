//! The session log: a [`SessionHub`] plus a writer thread that persists every action to SQLite.
//!
//! [`Recorder`] is what both `dmn replay` and the live `dmn run` (#15) start, so a replay writes
//! exactly the rows a live call would. [`StoreWriter`] turns actions into rows: finals only (no
//! partials), no audio, and nothing but what the actions carry, which holds no configuration
//! and no API key. It keeps its own reduced [`Session`] so a bare `Pin` knows which question row
//! to mark.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread::JoinHandle;

use dmn_session::{Action, Session, SessionHub, Speaker, reduce};
use dmn_store::{NewQuestion, NewSegment, NewSession, NewSuggestion, Phase, Store, StoreError};
use tokio::sync::broadcast::error::RecvError;

use crate::paths::Paths;

#[derive(Debug, thiserror::Error)]
pub enum RecorderError {
    #[error("session store {path}: {source}")]
    Store {
        path: PathBuf,
        #[source]
        source: StoreError,
    },
    #[error("{path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("no session {0} in the store")]
    NoSuchSession(i64),
    #[error("session writer thread panicked")]
    WriterPanicked,
}

/// Open (creating the directory and the file if needed) the session database at `paths.db_file`.
pub fn open_store(paths: &Paths) -> Result<Store, RecorderError> {
    if let Some(dir) = paths.db_file.parent() {
        std::fs::create_dir_all(dir).map_err(|source| RecorderError::Io {
            path: dir.to_path_buf(),
            source,
        })?;
    }
    Store::open(&paths.db_file).map_err(|source| RecorderError::Store {
        path: paths.db_file.clone(),
        source,
    })
}

fn segment_key(speaker: Speaker, start: f64) -> (Speaker, u64) {
    (speaker, start.to_bits())
}

/// Stored form of a session speaker.
fn source(speaker: Speaker) -> dmn_store::Source {
    match speaker {
        Speaker::Interviewer => dmn_store::Source::Interviewer,
        Speaker::You => dmn_store::Source::You,
        Speaker::Unknown => dmn_store::Source::Unknown,
    }
}

/// Why one action produced no row.
#[derive(Debug, thiserror::Error)]
enum WriteError {
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error("{0}")]
    Unlinked(&'static str),
}

/// What a writer did over a whole session.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct WriterReport {
    pub session_id: Option<i64>,
    pub segments: u64,
    pub questions: u64,
    pub suggestions: u64,
    /// Actions the store rejected, or that referred to rows that were never written.
    pub errors: u64,
    /// Actions lost because the writer fell behind the broadcast.
    pub lagged: u64,
}

/// Turns session actions into store rows.
pub struct StoreWriter {
    store: Store,
    state: Session,
    session_id: Option<i64>,
    started_at: i64,
    segments: HashMap<(Speaker, u64), i64>,
    questions: HashMap<u64, i64>,
    report: WriterReport,
}

impl StoreWriter {
    pub fn new(store: Store) -> Self {
        Self {
            store,
            state: Session::default(),
            session_id: None,
            started_at: 0,
            segments: HashMap::new(),
            questions: HashMap::new(),
            report: WriterReport::default(),
        }
    }

    pub fn report(&self) -> WriterReport {
        self.report
    }

    pub fn store(&self) -> &Store {
        &self.store
    }

    /// Persist one action. Errors are counted and logged, never fatal: a session that cannot
    /// write one row still shows the next question.
    pub fn apply(&mut self, action: &Action) {
        if let Err(e) = self.write(action) {
            self.report.errors += 1;
            tracing::warn!(error = %e, "session writer: action not stored");
        }
        reduce(&mut self.state, action);
    }

    /// The question row for the live question, before or after `action` is reduced.
    fn live_row(&self) -> Option<i64> {
        let live = self.state.live.as_ref()?;
        self.questions.get(&live.candidate).copied()
    }

    fn question_row(&self, candidate: u64) -> Result<i64, WriteError> {
        self.questions
            .get(&candidate)
            .copied()
            .ok_or(WriteError::Unlinked(
                "fast phase for a candidate with no question row",
            ))
    }

    fn write(&mut self, action: &Action) -> Result<(), WriteError> {
        match action {
            Action::Started { label, at } => {
                let id = self.store.insert_session(&NewSession::new(*at, label))?;
                self.session_id = Some(id);
                self.report.session_id = Some(id);
                self.started_at = *at;
                self.segments.clear();
                self.questions.clear();
            }
            Action::Ended { at } => {
                if let Some(id) = self.session_id {
                    self.store.end_session(id, *at)?;
                }
            }
            Action::Segment {
                speaker,
                start,
                end,
                text,
                is_final,
                avg_logprob,
            } => {
                // Finals only: a partial is superseded by its final within seconds.
                let Some(session_id) = self.session_id else {
                    return Ok(());
                };
                if !*is_final || text.is_empty() {
                    return Ok(());
                }
                let id = self.store.insert_segment(&NewSegment {
                    session_id,
                    source: source(*speaker),
                    start_time: *start,
                    end_time: *end,
                    text: text.clone(),
                    is_final: true,
                    avg_logprob: *avg_logprob,
                })?;
                self.segments.insert(segment_key(*speaker, *start), id);
                self.report.segments += 1;
            }
            Action::Candidate {
                id,
                speaker,
                start,
                score,
                ..
            } => {
                let Some(session_id) = self.session_id else {
                    return Ok(());
                };
                let segment_id = self
                    .segments
                    .get(&segment_key(*speaker, *start))
                    .copied()
                    .ok_or(WriteError::Unlinked(
                        "candidate from a segment that was never stored",
                    ))?;
                let row = self.store.insert_question(&NewQuestion {
                    session_id,
                    segment_id,
                    score: i64::from(*score),
                    // Session start plus the offset into the audio: the same on a replay as on
                    // the call it replays, unlike the wall clock at detection time.
                    created_at: self.started_at + start.floor() as i64,
                })?;
                self.questions.insert(*id, row);
                self.report.questions += 1;
            }
            Action::Question {
                candidate,
                text,
                kind,
            } => {
                let row = self.question_row(*candidate)?;
                self.store.set_question_text(row, text, Some(kind), None)?;
            }
            Action::Clarification { candidate, text } => {
                let row = self.question_row(*candidate)?;
                self.store
                    .set_question_text(row, text, Some("clarification"), None)?;
            }
            Action::Answered {
                candidate,
                payload,
                at,
                ..
            } => {
                let row = self.question_row(*candidate)?;
                // The focus is final only now; write the extraction once more with it.
                if let Ok(s) = serde_json::from_str::<serde_json::Value>(payload) {
                    let text = s.get("question").and_then(|v| v.as_str()).unwrap_or("");
                    let kind = s.get("kind").and_then(|v| v.as_str());
                    let focus = s
                        .get("focus")
                        .and_then(|v| v.as_str())
                        .filter(|f| !f.is_empty());
                    if !text.is_empty() {
                        self.store.set_question_text(row, text, kind, focus)?;
                    }
                }
                self.store.insert_suggestion(&NewSuggestion {
                    question_id: row,
                    phase: Phase::Fast,
                    payload: payload.clone(),
                    created_at: *at,
                })?;
                self.report.suggestions += 1;
            }
            Action::Pin => {
                if let Some(row) = self.live_row() {
                    self.store.set_question_pinned(row, true)?;
                }
            }
            Action::Unpin => {
                if self.state.pinned
                    && let Some(row) = self.live_row()
                {
                    self.store.set_question_pinned(row, false)?;
                }
            }
            Action::Focus { .. }
            | Action::ApproachLine { .. }
            | Action::Star { .. }
            | Action::Ignored { .. }
            | Action::Failed { .. }
            | Action::Status { .. } => {}
        }
        Ok(())
    }
}

/// A running session: the hub every stage dispatches into, and (unless storage is off) the
/// writer thread that persists it.
pub struct Recorder {
    hub: Arc<SessionHub>,
    writer: Option<JoinHandle<WriterReport>>,
    db_file: Option<PathBuf>,
}

impl Recorder {
    /// Start a session. With `store` set the writer subscribes before anything is dispatched, so
    /// it sees every action from [`Action::Started`] on.
    pub fn start(store: Option<Store>, db_file: Option<&Path>) -> Self {
        let hub = Arc::new(SessionHub::default());
        let writer = store.map(|store| {
            let mut rx = hub.subscribe();
            std::thread::spawn(move || {
                let mut w = StoreWriter::new(store);
                loop {
                    match rx.blocking_recv() {
                        Ok(action) => w.apply(&action),
                        Err(RecvError::Lagged(n)) => {
                            w.report.lagged += n;
                            tracing::warn!(lost = n, "session writer fell behind");
                        }
                        Err(RecvError::Closed) => break,
                    }
                }
                w.report()
            })
        });
        Self {
            hub,
            writer,
            db_file: db_file.map(Path::to_path_buf),
        }
    }

    /// The hub to dispatch into (and to read the `watch` channel from).
    pub fn hub(&self) -> Arc<SessionHub> {
        Arc::clone(&self.hub)
    }

    pub fn db_file(&self) -> Option<&Path> {
        self.db_file.as_deref()
    }

    /// Dispatch [`Action::Ended`], then wait for the writer to store everything. Every other
    /// holder of the hub must have dropped it by now, or the writer never sees the channel close.
    pub fn finish(self, at: i64) -> Result<Option<WriterReport>, RecorderError> {
        self.hub.dispatch(Action::Ended { at });
        drop(self.hub);
        match self.writer {
            Some(handle) => handle
                .join()
                .map(Some)
                .map_err(|_| RecorderError::WriterPanicked),
            None => Ok(None),
        }
    }
}

/// Which sessions `dmn sessions purge` removes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PurgeTarget {
    All,
    One(i64),
}

/// What a purge removed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PurgeReport {
    pub sessions: Vec<i64>,
    pub files: Vec<PathBuf>,
}

/// Is `name` a file that belongs to session `id` (`<id>.jsonl`, `<id>-you.wav`, ...)?
fn belongs_to(name: &str, id: i64) -> bool {
    let id = id.to_string();
    name.strip_prefix(&id)
        .is_some_and(|rest| rest.starts_with('.') || rest.starts_with('-'))
}

/// `dmn sessions purge <id|all>` (SPEC § Privacy rules): delete the rows, the session's files
/// under `sessions/`, and the free pages that still held the deleted text.
pub fn purge(paths: &Paths, target: PurgeTarget) -> Result<PurgeReport, RecorderError> {
    let mut report = PurgeReport::default();
    if paths.db_file.is_file() {
        let store = open_store(paths)?;
        let err = |source| RecorderError::Store {
            path: paths.db_file.clone(),
            source,
        };
        let ids = match target {
            PurgeTarget::All => store.session_ids().map_err(err)?,
            PurgeTarget::One(id) => vec![id],
        };
        for id in ids {
            if store.delete_session(id).map_err(err)? {
                report.sessions.push(id);
            } else if target == PurgeTarget::One(id) {
                return Err(RecorderError::NoSuchSession(id));
            }
        }
        store.compact().map_err(err)?;
    } else if let PurgeTarget::One(id) = target {
        return Err(RecorderError::NoSuchSession(id));
    }

    let entries = match std::fs::read_dir(&paths.sessions_dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(report),
        Err(source) => {
            return Err(RecorderError::Io {
                path: paths.sessions_dir.clone(),
                source,
            });
        }
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        let doomed = match target {
            PurgeTarget::All => path.is_file(),
            PurgeTarget::One(id) => path.is_file() && belongs_to(&name, id),
        };
        if doomed {
            std::fs::remove_file(&path).map_err(|source| RecorderError::Io {
                path: path.clone(),
                source,
            })?;
            report.files.push(path);
        }
    }
    report.files.sort();
    Ok(report)
}

/// `dmn sessions export <id>`: write the session as JSON Lines to `sessions/<id>.jsonl`.
pub fn export(paths: &Paths, id: i64) -> Result<(PathBuf, usize), RecorderError> {
    if !paths.db_file.is_file() {
        return Err(RecorderError::NoSuchSession(id));
    }
    let store = open_store(paths)?;
    let err = |source| RecorderError::Store {
        path: paths.db_file.clone(),
        source,
    };
    if store.session(id).map_err(err)?.is_none() {
        return Err(RecorderError::NoSuchSession(id));
    }
    std::fs::create_dir_all(&paths.sessions_dir).map_err(|source| RecorderError::Io {
        path: paths.sessions_dir.clone(),
        source,
    })?;
    let path = paths.sessions_dir.join(format!("{id}.jsonl"));
    let file = std::fs::File::create(&path).map_err(|source| RecorderError::Io {
        path: path.clone(),
        source,
    })?;
    let lines = store
        .export_session_jsonl(id, std::io::BufWriter::new(file))
        .map_err(err)?;
    Ok((path, lines))
}
