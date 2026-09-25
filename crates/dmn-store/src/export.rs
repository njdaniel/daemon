//! JSONL export of one session: every segment, question and suggestion as one JSON object per
//! line (`~/.local/share/dmn/sessions/<id>.jsonl`, SPEC § Privacy and storage).

use std::io::Write;

use serde::Serialize;

use crate::error::StoreError;
use crate::store::{CpuSampleRow, QuestionRow, SegmentRow, SessionRow, Store, SuggestionRow};

/// One line of the export. The `type` tag tells readers which table the row came from.
#[derive(Serialize)]
#[serde(tag = "type", rename_all = "lowercase")]
enum ExportLine {
    Session(SessionRow),
    Segment(SegmentRow),
    Question(QuestionRow),
    Suggestion(SuggestionRow),
    Cpu(CpuSampleRow),
}

impl Store {
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

        let mut written = 0;
        for line in &lines {
            serde_json::to_writer(&mut out, line)?;
            out.write_all(b"\n")?;
            written += 1;
        }
        Ok(written)
    }
}
