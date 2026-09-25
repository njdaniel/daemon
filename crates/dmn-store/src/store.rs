//! The [`Store`] handle and the plain row structs for each table.
//!
//! These rows mirror the shapes the pipeline already uses — the STT `Segment` (SPEC §
//! Transcription), the detector's candidate plus the LLM fast phase's fields — but are defined
//! here so the crate never depends on `dmn-transcript`, `dmn-detect` or `dmn-llm` types.
//! `suggestions.payload` stays raw JSON text for the same reason (see `migrate.rs`).

use std::path::Path;
use std::time::Duration;

use rusqlite::types::{FromSql, FromSqlError, FromSqlResult, ToSql, ToSqlOutput, ValueRef};
use rusqlite::{Connection, OpenFlags, OptionalExtension, Row, params};
use serde::{Deserialize, Serialize};

use crate::error::StoreError;
use crate::migrate;

/// Which capture stream a segment came from. Stored as TEXT (`interviewer` / `you` / `unknown`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Source {
    Interviewer,
    You,
    Unknown,
}

impl Source {
    pub fn as_str(self) -> &'static str {
        match self {
            Source::Interviewer => "interviewer",
            Source::You => "you",
            Source::Unknown => "unknown",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "interviewer" => Some(Source::Interviewer),
            "you" => Some(Source::You),
            "unknown" => Some(Source::Unknown),
            _ => None,
        }
    }
}

/// Which LLM phase produced a suggestion. Stored as TEXT (`fast` / `slow`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Phase {
    Fast,
    Slow,
}

impl Phase {
    pub fn as_str(self) -> &'static str {
        match self {
            Phase::Fast => "fast",
            Phase::Slow => "slow",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "fast" => Some(Phase::Fast),
            "slow" => Some(Phase::Slow),
            _ => None,
        }
    }
}

impl ToSql for Source {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        Ok(ToSqlOutput::from(self.as_str()))
    }
}

impl FromSql for Source {
    fn column_result(value: ValueRef<'_>) -> FromSqlResult<Self> {
        let text = value.as_str()?;
        Source::parse(text)
            .ok_or_else(|| FromSqlError::Other(format!("unknown segment source {text:?}").into()))
    }
}

impl ToSql for Phase {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        Ok(ToSqlOutput::from(self.as_str()))
    }
}

impl FromSql for Phase {
    fn column_result(value: ValueRef<'_>) -> FromSqlResult<Self> {
        let text = value.as_str()?;
        Phase::parse(text)
            .ok_or_else(|| FromSqlError::Other(format!("unknown suggestion phase {text:?}").into()))
    }
}

/// A row of `sessions`. `started_at` / `ended_at` are unix epoch seconds; `overruns` is the
/// ring-buffer frames dropped over the session (schema v2).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionRow {
    pub id: i64,
    pub started_at: i64,
    pub ended_at: Option<i64>,
    pub label: String,
    pub overruns: i64,
}

/// A row of `segments`. `start_time` / `end_time` are seconds relative to the session start.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SegmentRow {
    pub id: i64,
    pub session_id: i64,
    pub source: Source,
    pub start_time: f64,
    pub end_time: f64,
    pub text: String,
    pub is_final: bool,
    pub avg_logprob: f64,
}

/// A row of `questions`: one detector candidate plus, once the LLM fast phase has run, the
/// extracted `text`, `kind` and `focus` (all `None` until then).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QuestionRow {
    pub id: i64,
    pub session_id: i64,
    pub segment_id: i64,
    pub score: i64,
    pub text: Option<String>,
    pub kind: Option<String>,
    pub focus: Option<String>,
    pub pinned: bool,
    pub created_at: i64,
    /// Audio seconds from the session start (schema v2): when the detector emitted it, when
    /// its question line and its first Approach line were produced. `None` = not measured.
    pub emitted_at: Option<f64>,
    pub question_at: Option<f64>,
    pub approach_at: Option<f64>,
    /// The coding-solutions mode (`hints` / `full`) when it was detected (schema v3, #50);
    /// `None` for rows from older builds.
    #[serde(default)]
    pub coding_solutions: Option<String>,
}

/// Which moment of a question's life [`Store::mark_question`] records.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mark {
    /// The detector emitted the candidate.
    Emitted,
    /// The fast phase produced the question (or clarification) line.
    Question,
    /// The fast phase produced the first Approach line (or the STAR cards).
    Approach,
}

impl Mark {
    fn column(self) -> &'static str {
        match self {
            Mark::Emitted => "emitted_at",
            Mark::Question => "question_at",
            Mark::Approach => "approach_at",
        }
    }
}

/// A row of `cpu_samples` (schema v2): the process's CPU seconds `cpu` and whisper's cumulative
/// inference seconds `whisper` at audio time `at` and wall time `wall`, both seconds since the
/// session started.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CpuSampleRow {
    pub id: i64,
    pub session_id: i64,
    pub at: f64,
    pub wall: f64,
    pub cpu: f64,
    pub whisper: f64,
}

/// Insert form of [`CpuSampleRow`]; the id is assigned by SQLite.
#[derive(Debug, Clone, PartialEq)]
pub struct NewCpuSample {
    pub session_id: i64,
    pub at: f64,
    pub wall: f64,
    pub cpu: f64,
    pub whisper: f64,
}

/// A row of `suggestions`: the raw prompt-contract JSON for one phase of one question.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SuggestionRow {
    pub id: i64,
    pub question_id: i64,
    pub phase: Phase,
    pub payload: String,
    pub created_at: i64,
}

/// Insert form of [`SessionRow`]; the id is assigned by SQLite.
#[derive(Debug, Clone, PartialEq)]
pub struct NewSession {
    pub started_at: i64,
    pub label: String,
}

impl NewSession {
    pub fn new(started_at: i64, label: impl Into<String>) -> Self {
        Self {
            started_at,
            label: label.into(),
        }
    }
}

/// Insert form of [`SegmentRow`]; the id is assigned by SQLite.
#[derive(Debug, Clone, PartialEq)]
pub struct NewSegment {
    pub session_id: i64,
    pub source: Source,
    pub start_time: f64,
    pub end_time: f64,
    pub text: String,
    pub is_final: bool,
    pub avg_logprob: f64,
}

/// Insert form of [`QuestionRow`]; the id is assigned by SQLite and `text` / `kind` / `focus`
/// start as `None` until the LLM fills them via [`Store::set_question_text`].
#[derive(Debug, Clone, PartialEq)]
pub struct NewQuestion {
    pub session_id: i64,
    pub segment_id: i64,
    pub score: i64,
    pub created_at: i64,
}

/// Insert form of [`SuggestionRow`]; the id is assigned by SQLite.
#[derive(Debug, Clone, PartialEq)]
pub struct NewSuggestion {
    pub question_id: i64,
    pub phase: Phase,
    pub payload: String,
    pub created_at: i64,
}

/// How long a connection waits for another connection's write to finish.
const BUSY_TIMEOUT: Duration = Duration::from_secs(2);

/// The database handle. Opening (or creating) a database runs pending migrations, so every
/// `Store` sees the current schema. Foreign keys are always enforced.
pub struct Store {
    pub(crate) conn: Connection,
}

impl Store {
    /// Open (creating if needed) the database at `path` and migrate it. The path itself is the
    /// caller's problem: XDG resolution lives in `dmn-core`.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, StoreError> {
        Self::init(Connection::open(path)?)
    }

    /// An in-memory store, for tests and replay-without-persistence.
    pub fn open_in_memory() -> Result<Self, StoreError> {
        Self::init(Connection::open_in_memory()?)
    }

    /// Open an existing database without writing to it, for `--no-store` runs that still read
    /// the context sources (#54). `None` when there is no database yet, or when it is not at the
    /// current schema: only a writing open migrates.
    pub fn open_read_only(path: impl AsRef<Path>) -> Result<Option<Self>, StoreError> {
        let path = path.as_ref();
        if !path.is_file() {
            return Ok(None);
        }
        let conn = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_ONLY
                | OpenFlags::SQLITE_OPEN_URI
                | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        conn.busy_timeout(BUSY_TIMEOUT)?;
        let store = Self { conn };
        Ok((store.schema_version()? == migrate::SCHEMA_VERSION).then_some(store))
    }

    fn init(mut conn: Connection) -> Result<Self, StoreError> {
        conn.pragma_update(None, "foreign_keys", "ON")?;
        // The session writer and the fast phase's context reader (#54) hold connections to the
        // same file; one waits briefly for the other's write instead of failing with SQLITE_BUSY.
        conn.busy_timeout(BUSY_TIMEOUT)?;
        migrate::migrate(&mut conn)?;
        Ok(Self { conn })
    }

    /// The schema version the database is at (`user_version`).
    pub fn schema_version(&self) -> Result<i64, StoreError> {
        Ok(self
            .conn
            .pragma_query_value(None, "user_version", |row| row.get(0))?)
    }

    pub fn insert_session(&self, session: &NewSession) -> Result<i64, StoreError> {
        self.conn.execute(
            "INSERT INTO sessions (started_at, label) VALUES (?1, ?2)",
            params![session.started_at, session.label],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// Stamp `ended_at` on an open session.
    pub fn end_session(&self, id: i64, ended_at: i64) -> Result<(), StoreError> {
        self.conn.execute(
            "UPDATE sessions SET ended_at = ?2 WHERE id = ?1",
            params![id, ended_at],
        )?;
        Ok(())
    }

    /// Record how many ring-buffer frames the session has dropped so far.
    pub fn set_session_overruns(&self, id: i64, overruns: i64) -> Result<(), StoreError> {
        self.conn.execute(
            "UPDATE sessions SET overruns = ?2 WHERE id = ?1",
            params![id, overruns],
        )?;
        Ok(())
    }

    /// The newest session's id, if there is one.
    pub fn latest_session_id(&self) -> Result<Option<i64>, StoreError> {
        Ok(self
            .conn
            .query_row("SELECT max(id) FROM sessions", [], |row| row.get(0))?)
    }

    pub fn session(&self, id: i64) -> Result<Option<SessionRow>, StoreError> {
        Ok(self
            .conn
            .query_row(
                "SELECT id, started_at, ended_at, label, overruns FROM sessions WHERE id = ?1",
                [id],
                session_from_row,
            )
            .optional()?)
    }

    pub fn insert_segment(&self, segment: &NewSegment) -> Result<i64, StoreError> {
        self.conn.execute(
            "INSERT INTO segments (session_id, source, start_time, end_time, text, is_final, avg_logprob)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                segment.session_id,
                segment.source,
                segment.start_time,
                segment.end_time,
                segment.text,
                segment.is_final,
                segment.avg_logprob,
            ],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// All segments of a session, in timeline order.
    pub fn segments(&self, session_id: i64) -> Result<Vec<SegmentRow>, StoreError> {
        let mut stmt = self.conn.prepare(
            "SELECT id, session_id, source, start_time, end_time, text, is_final, avg_logprob
             FROM segments WHERE session_id = ?1 ORDER BY start_time, id",
        )?;
        let rows = stmt
            .query_map([session_id], segment_from_row)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Record a detector candidate. `text` / `kind` / `focus` stay `NULL` until the LLM fast
    /// phase fills them via [`Store::set_question_text`].
    pub fn insert_question(&self, question: &NewQuestion) -> Result<i64, StoreError> {
        self.conn.execute(
            "INSERT INTO questions (session_id, segment_id, score, created_at)
             VALUES (?1, ?2, ?3, ?4)",
            params![
                question.session_id,
                question.segment_id,
                question.score,
                question.created_at,
            ],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// Fill in the LLM fast phase's extraction for a candidate.
    pub fn set_question_text(
        &self,
        id: i64,
        text: &str,
        kind: Option<&str>,
        focus: Option<&str>,
    ) -> Result<(), StoreError> {
        self.conn.execute(
            "UPDATE questions SET text = ?2, kind = ?3, focus = ?4 WHERE id = ?1",
            params![id, text, kind, focus],
        )?;
        Ok(())
    }

    /// Record when `mark` happened for a question, in audio seconds from the session start.
    /// Only the first time counts: a regenerated reply does not move an earlier showing.
    pub fn mark_question(&self, id: i64, mark: Mark, at: f64) -> Result<(), StoreError> {
        let col = mark.column();
        self.conn.execute(
            &format!("UPDATE questions SET {col} = coalesce({col}, ?2) WHERE id = ?1"),
            params![id, at],
        )?;
        Ok(())
    }

    pub fn insert_cpu_sample(&self, sample: &NewCpuSample) -> Result<i64, StoreError> {
        self.conn.execute(
            "INSERT INTO cpu_samples (session_id, at, wall, cpu, whisper)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                sample.session_id,
                sample.at,
                sample.wall,
                sample.cpu,
                sample.whisper,
            ],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// A session's CPU samples in time order.
    pub fn cpu_samples(&self, session_id: i64) -> Result<Vec<CpuSampleRow>, StoreError> {
        let mut stmt = self.conn.prepare(
            "SELECT id, session_id, at, wall, cpu, whisper FROM cpu_samples
             WHERE session_id = ?1 ORDER BY wall, id",
        )?;
        let rows = stmt
            .query_map([session_id], |row| {
                Ok(CpuSampleRow {
                    id: row.get("id")?,
                    session_id: row.get("session_id")?,
                    at: row.get("at")?,
                    wall: row.get("wall")?,
                    cpu: row.get("cpu")?,
                    whisper: row.get("whisper")?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Record the coding-solutions mode (`hints` / `full`) in effect for a question (#50).
    pub fn set_question_coding_solutions(&self, id: i64, mode: &str) -> Result<(), StoreError> {
        self.conn.execute(
            "UPDATE questions SET coding_solutions = ?2 WHERE id = ?1",
            params![id, mode],
        )?;
        Ok(())
    }

    pub fn set_question_pinned(&self, id: i64, pinned: bool) -> Result<(), StoreError> {
        self.conn.execute(
            "UPDATE questions SET pinned = ?2 WHERE id = ?1",
            params![id, pinned],
        )?;
        Ok(())
    }

    /// All questions of a session, oldest first.
    pub fn questions(&self, session_id: i64) -> Result<Vec<QuestionRow>, StoreError> {
        let mut stmt = self.conn.prepare(
            "SELECT id, session_id, segment_id, score, text, kind, focus, pinned, created_at,
                    emitted_at, question_at, approach_at, coding_solutions
             FROM questions WHERE session_id = ?1 ORDER BY created_at, id",
        )?;
        let rows = stmt
            .query_map([session_id], question_from_row)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub fn insert_suggestion(&self, suggestion: &NewSuggestion) -> Result<i64, StoreError> {
        self.conn.execute(
            "INSERT INTO suggestions (question_id, phase, payload, created_at)
             VALUES (?1, ?2, ?3, ?4)",
            params![
                suggestion.question_id,
                suggestion.phase,
                suggestion.payload,
                suggestion.created_at,
            ],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// Ids of every session, oldest first.
    pub fn session_ids(&self) -> Result<Vec<i64>, StoreError> {
        let mut stmt = self.conn.prepare("SELECT id FROM sessions ORDER BY id")?;
        let ids = stmt
            .query_map([], |row| row.get(0))?
            .collect::<Result<Vec<i64>, _>>()?;
        Ok(ids)
    }

    /// Delete one session and, by cascade, its segments, questions and suggestions. Returns
    /// whether the session existed. Call [`Store::compact`] afterwards so the deleted text does
    /// not linger in the file's free pages.
    pub fn delete_session(&self, id: i64) -> Result<bool, StoreError> {
        Ok(self
            .conn
            .execute("DELETE FROM sessions WHERE id = ?1", [id])?
            > 0)
    }

    /// Rewrite the database file (`VACUUM`), dropping free pages that still hold deleted rows.
    pub fn compact(&self) -> Result<(), StoreError> {
        self.conn.execute_batch("VACUUM")?;
        Ok(())
    }

    /// Number of rows in each table, for reports and tests: (sessions, segments, questions,
    /// suggestions).
    pub fn counts(&self) -> Result<(i64, i64, i64, i64), StoreError> {
        Ok(self.conn.query_row(
            "SELECT (SELECT count(*) FROM sessions), (SELECT count(*) FROM segments),
                    (SELECT count(*) FROM questions), (SELECT count(*) FROM suggestions)",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )?)
    }

    /// All suggestions for one question, oldest first.
    pub fn suggestions_for_question(
        &self,
        question_id: i64,
    ) -> Result<Vec<SuggestionRow>, StoreError> {
        let mut stmt = self.conn.prepare(
            "SELECT id, question_id, phase, payload, created_at
             FROM suggestions WHERE question_id = ?1 ORDER BY created_at, id",
        )?;
        let rows = stmt
            .query_map([question_id], suggestion_from_row)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// All suggestions of a session, grouped by question.
    pub fn suggestions_for_session(
        &self,
        session_id: i64,
    ) -> Result<Vec<SuggestionRow>, StoreError> {
        let mut stmt = self.conn.prepare(
            "SELECT s.id, s.question_id, s.phase, s.payload, s.created_at
             FROM suggestions s JOIN questions q ON q.id = s.question_id
             WHERE q.session_id = ?1 ORDER BY s.question_id, s.created_at, s.id",
        )?;
        let rows = stmt
            .query_map([session_id], suggestion_from_row)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }
}

#[cfg(test)]
impl Store {
    /// Run raw SQL the typed API doesn't expose (constraint and cascade tests).
    pub(crate) fn execute_raw(&self, sql: &str, param: i64) -> Result<usize, StoreError> {
        Ok(self.conn.execute(sql, [param])?)
    }

    /// Table and index names in `sqlite_master`, for schema assertions.
    pub(crate) fn schema_object_names(&self) -> Result<Vec<String>, StoreError> {
        let mut stmt = self
            .conn
            .prepare("SELECT name FROM sqlite_master WHERE type IN ('table', 'index')")?;
        let names = stmt
            .query_map([], |row| row.get(0))?
            .collect::<Result<Vec<String>, _>>()?;
        Ok(names)
    }
}

fn session_from_row(row: &Row<'_>) -> rusqlite::Result<SessionRow> {
    Ok(SessionRow {
        id: row.get("id")?,
        started_at: row.get("started_at")?,
        ended_at: row.get("ended_at")?,
        label: row.get("label")?,
        overruns: row.get("overruns")?,
    })
}

fn segment_from_row(row: &Row<'_>) -> rusqlite::Result<SegmentRow> {
    Ok(SegmentRow {
        id: row.get("id")?,
        session_id: row.get("session_id")?,
        source: row.get("source")?,
        start_time: row.get("start_time")?,
        end_time: row.get("end_time")?,
        text: row.get("text")?,
        is_final: row.get("is_final")?,
        avg_logprob: row.get("avg_logprob")?,
    })
}

fn question_from_row(row: &Row<'_>) -> rusqlite::Result<QuestionRow> {
    Ok(QuestionRow {
        id: row.get("id")?,
        session_id: row.get("session_id")?,
        segment_id: row.get("segment_id")?,
        score: row.get("score")?,
        text: row.get("text")?,
        kind: row.get("kind")?,
        focus: row.get("focus")?,
        pinned: row.get("pinned")?,
        created_at: row.get("created_at")?,
        emitted_at: row.get("emitted_at")?,
        question_at: row.get("question_at")?,
        approach_at: row.get("approach_at")?,
        coding_solutions: row.get("coding_solutions")?,
    })
}

fn suggestion_from_row(row: &Row<'_>) -> rusqlite::Result<SuggestionRow> {
    Ok(SuggestionRow {
        id: row.get("id")?,
        question_id: row.get("question_id")?,
        phase: row.get("phase")?,
        payload: row.get("payload")?,
        created_at: row.get("created_at")?,
    })
}
