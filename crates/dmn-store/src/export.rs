//! Session exports: a readable transcript of finalized speech, or every stored row as JSONL.

use std::io::Write;

use serde::Serialize;

use crate::error::StoreError;
use crate::store::{
    CpuSampleRow, QuestionRow, SegmentRow, SessionRow, Source, Store, SuggestionRow,
};
use crate::summaries::SummaryRow;

/// One line of the export. The `type` tag tells readers which table the row came from.
#[derive(Serialize)]
#[serde(tag = "type", rename_all = "lowercase")]
enum ExportLine {
    Session(SessionRow),
    Segment(SegmentRow),
    Question(QuestionRow),
    Suggestion(SuggestionRow),
    Cpu(CpuSampleRow),
    Summary(SummaryRow),
}

impl Store {
    /// Write a session's finalized speech in timeline order, including an unfinished session
    /// recovered after a crash. Returns the segment count; partials and suggestions are omitted.
    pub fn export_session_text(
        &self,
        session_id: i64,
        mut out: impl Write,
    ) -> Result<usize, StoreError> {
        let session = self
            .session(session_id)?
            .ok_or(StoreError::Sqlite(rusqlite::Error::QueryReturnedNoRows))?;
        // SQLite already provides UTC calendar formatting; no date dependency is needed.
        let started: Option<String> = self.conn.query_row(
            "SELECT strftime('%Y-%m-%d %H:%M:%S UTC', ?1, 'unixepoch')",
            [session.started_at],
            |row| row.get(0),
        )?;
        writeln!(out, "Session #{} — {}", session.id, session.label)?;
        writeln!(
            out,
            "Started: {}\n",
            started.unwrap_or_else(|| format!("{} (Unix seconds)", session.started_at))
        )?;

        let mut written = 0;
        for segment in self
            .segments(session_id)?
            .into_iter()
            .filter(|s| s.is_final)
        {
            let seconds = segment.start_time.max(0.0) as u64;
            let clock = if seconds >= 3600 {
                format!(
                    "{:02}:{:02}:{:02}",
                    seconds / 3600,
                    seconds / 60 % 60,
                    seconds % 60
                )
            } else {
                format!("{:02}:{:02}", seconds / 60, seconds % 60)
            };
            let speaker = match segment.source {
                Source::Them => "Them",
                Source::You => "You",
                Source::Unknown => "Unknown",
            };
            writeln!(out, "[{clock}] {speaker}: {}", segment.text)?;
            written += 1;
        }
        Ok(written)
    }

    /// Write one session to `out` as JSON Lines: the session row first, then its segments,
    /// questions and suggestions, each in timeline order. Returns the number of lines written.
    pub fn export_session_jsonl(
        &self,
        session_id: i64,
        mut out: impl Write,
    ) -> Result<usize, StoreError> {
        let session = self
            .session(session_id)?
            .ok_or(StoreError::Sqlite(rusqlite::Error::QueryReturnedNoRows))?;

        let mut lines = vec![ExportLine::Session(session)];
        lines.extend(
            self.segments(session_id)?
                .into_iter()
                .map(ExportLine::Segment),
        );
        lines.extend(
            self.questions(session_id)?
                .into_iter()
                .map(ExportLine::Question),
        );
        lines.extend(
            self.suggestions_for_session(session_id)?
                .into_iter()
                .map(ExportLine::Suggestion),
        );
        lines.extend(
            self.cpu_samples(session_id)?
                .into_iter()
                .map(ExportLine::Cpu),
        );
        lines.extend(
            self.summaries(session_id)?
                .into_iter()
                .map(ExportLine::Summary),
        );

        let mut written = 0;
        for line in &lines {
            serde_json::to_writer(&mut out, line)?;
            out.write_all(b"\n")?;
            written += 1;
        }
        Ok(written)
    }
}
