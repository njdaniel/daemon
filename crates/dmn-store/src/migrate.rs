//! The migration runner and the v1 schema.
//!
//! Migrations are an ordered list of `(version, sql)` pairs. The applied version is tracked in
//! SQLite's `user_version` pragma, so opening an existing database applies only the migrations
//! newer than what is already there — re-opening a current database is a no-op. Each migration
//! runs in its own transaction together with the `user_version` bump, so a failed migration
//! leaves the database at the previous version.
//!
//! Timestamp convention (v1): wall-clock times (`started_at`, `ended_at`, `created_at`) are
//! INTEGER unix epoch seconds; segment `start_time` / `end_time` are REAL seconds relative to
//! the session start, mirroring the STT `Segment` shape (SPEC § Transcription).
//!
//! v2 (#16) adds what `dmn bench --live` needs to report latency and CPU from a stored session:
//! per question, the audio time (REAL seconds, the same clock as segment times) at which the
//! detector emitted it, its question line was produced and its first Approach line was; a
//! `cpu_samples` table of process CPU time over the session; and the session's ring-buffer
//! overrun count.

use rusqlite::Connection;

use crate::error::StoreError;

/// The schema version a freshly migrated database reports in `user_version`.
pub const SCHEMA_VERSION: i64 = 2;

struct Migration {
    version: i64,
    sql: &'static str,
}

const MIGRATIONS: &[Migration] = &[
    Migration {
        version: 1,
        sql: V1,
    },
    Migration {
        version: 2,
        sql: V2,
    },
];

/// The v1 schema: sessions, their transcript segments, the detector's question candidates and
/// the LLM suggestions per question. `suggestions.payload` is deliberately raw TEXT holding the
/// prompt-contract JSON: the typed `Suggestion` struct is owned by `dmn-llm` (#12), and keeping
/// the payload untyped leaves this crate decoupled from it. Chunks and summaries (M2 source
/// ingestion) are intentionally absent.
const V1: &str = "
CREATE TABLE sessions (
    id          INTEGER PRIMARY KEY,
    started_at  INTEGER NOT NULL,
    ended_at    INTEGER,
    label       TEXT NOT NULL DEFAULT ''
);

CREATE TABLE segments (
    id          INTEGER PRIMARY KEY,
    session_id  INTEGER NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
    source      TEXT NOT NULL CHECK (source IN ('interviewer', 'you', 'unknown')),
    start_time  REAL NOT NULL,
    end_time    REAL NOT NULL,
    text        TEXT NOT NULL,
    is_final    INTEGER NOT NULL,
    avg_logprob REAL NOT NULL
);
CREATE INDEX idx_segments_session ON segments(session_id);

CREATE TABLE questions (
    id          INTEGER PRIMARY KEY,
    session_id  INTEGER NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
    segment_id  INTEGER NOT NULL REFERENCES segments(id) ON DELETE CASCADE,
    score       INTEGER NOT NULL,
    text        TEXT,
    kind        TEXT,
    focus       TEXT,
    pinned      INTEGER NOT NULL DEFAULT 0,
    created_at  INTEGER NOT NULL
);
CREATE INDEX idx_questions_session ON questions(session_id);
CREATE INDEX idx_questions_segment ON questions(segment_id);

CREATE TABLE suggestions (
    id          INTEGER PRIMARY KEY,
    question_id INTEGER NOT NULL REFERENCES questions(id) ON DELETE CASCADE,
    phase       TEXT NOT NULL CHECK (phase IN ('fast', 'slow')),
    payload     TEXT NOT NULL,
    created_at  INTEGER NOT NULL
);
CREATE INDEX idx_suggestions_question ON suggestions(question_id);
";

/// v2 (#16): timing and CPU for `dmn bench --live`. Every new question column is nullable, so
/// rows written by a v1 build read back as "not measured" rather than as zero.
///
/// - `questions.emitted_at` / `question_at` / `approach_at`: audio seconds from the session
///   start (the clock segment times use) at which the detector emitted the candidate, the fast
///   phase produced its question (or clarification) line, and its first Approach line (or STAR
///   card). The first showing counts: a regenerate does not move them.
/// - `cpu_samples`: the process's CPU seconds (`cpu`) and whisper's inference seconds
///   (`whisper`) at audio time `at` and wall seconds `wall` since the session started, sampled
///   every few seconds. A window in which `whisper` did not move is one nobody was speaking in.
/// - `sessions.overruns`: ring-buffer frames dropped over the session (SPEC § Threading rules).
const V2: &str = "
ALTER TABLE sessions ADD COLUMN overruns INTEGER NOT NULL DEFAULT 0;

ALTER TABLE questions ADD COLUMN emitted_at REAL;
ALTER TABLE questions ADD COLUMN question_at REAL;
ALTER TABLE questions ADD COLUMN approach_at REAL;

CREATE TABLE cpu_samples (
    id          INTEGER PRIMARY KEY,
    session_id  INTEGER NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
    at          REAL NOT NULL,
    wall        REAL NOT NULL,
    cpu         REAL NOT NULL,
    whisper     REAL NOT NULL
);
CREATE INDEX idx_cpu_samples_session ON cpu_samples(session_id);
";

/// Apply every migration newer than the database's `user_version`, each in a transaction.
pub fn migrate(conn: &mut Connection) -> Result<(), StoreError> {
    migrate_to(conn, SCHEMA_VERSION)
}

/// Apply the migrations up to and including `target`. Tests use it to build an older database
/// and check that opening it upgrades the rows already there.
pub(crate) fn migrate_to(conn: &mut Connection, target: i64) -> Result<(), StoreError> {
    let current: i64 = conn.pragma_query_value(None, "user_version", |row| row.get(0))?;
    for migration in MIGRATIONS {
        if migration.version <= current || migration.version > target {
            continue;
        }
        let tx = conn.transaction()?;
        tx.execute_batch(migration.sql)?;
        tx.pragma_update(None, "user_version", migration.version)?;
        tx.commit()?;
    }
    Ok(())
}
