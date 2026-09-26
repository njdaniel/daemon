//! Session memory (schema v6, #57): one summary line per answered question, and the promise the
//! candidate made in that answer, if any, with whether a later answer covered it. `dmn-session`
//! decides the lines and when a promise is covered; this module only stores them.

use rusqlite::{Row, params};
use serde::{Deserialize, Serialize};

use crate::error::StoreError;
use crate::store::Store;

/// A row of `summaries`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SummaryRow {
    pub id: i64,
    pub session_id: i64,
    /// The question the line summarises.
    pub question_id: i64,
    pub line: String,
    pub promise: Option<String>,
    /// A later You segment covered the promise.
    pub mentioned: bool,
    /// Unix epoch seconds.
    pub created_at: i64,
}

/// Insert form of [`SummaryRow`]; the id is assigned by SQLite and `mentioned` starts false.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewSummary {
    pub session_id: i64,
    pub question_id: i64,
    pub line: String,
    pub promise: Option<String>,
    pub created_at: i64,
}

impl Store {
    /// Store a question's summary line, replacing the one it had (a question has one line).
    pub fn put_summary(&self, summary: &NewSummary) -> Result<i64, StoreError> {
        self.conn.execute(
            "INSERT INTO summaries (session_id, question_id, line, promise, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT (question_id) DO UPDATE SET line = excluded.line",
            params![
                summary.session_id,
                summary.question_id,
                summary.line,
                summary.promise,
                summary.created_at,
            ],
        )?;
        Ok(self.conn.query_row(
            "SELECT id FROM summaries WHERE question_id = ?1",
            [summary.question_id],
            |row| row.get(0),
        )?)
    }

    /// The promise made in the answer to `question_id` was covered.
    pub fn set_promise_mentioned(&self, question_id: i64) -> Result<(), StoreError> {
        self.conn.execute(
            "UPDATE summaries SET mentioned = 1 WHERE question_id = ?1",
            [question_id],
        )?;
        Ok(())
    }

    /// A session's summary lines, in question order.
    pub fn summaries(&self, session_id: i64) -> Result<Vec<SummaryRow>, StoreError> {
        let mut stmt = self.conn.prepare(
            "SELECT s.id, s.session_id, s.question_id, s.line, s.promise, s.mentioned, s.created_at
             FROM summaries s JOIN questions q ON q.id = s.question_id
             WHERE s.session_id = ?1 ORDER BY q.created_at, q.id",
        )?;
        let rows = stmt
            .query_map([session_id], summary_from_row)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }
}

fn summary_from_row(row: &Row<'_>) -> rusqlite::Result<SummaryRow> {
    Ok(SummaryRow {
        id: row.get("id")?,
        session_id: row.get("session_id")?,
        question_id: row.get("question_id")?,
        line: row.get("line")?,
        promise: row.get("promise")?,
        mentioned: row.get("mentioned")?,
        created_at: row.get("created_at")?,
    })
}
