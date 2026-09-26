//! Context sources and their chunks (schema v4, #53), and the cached job-description summary
//! (schema v5, #54).
//!
//! `dmn-context` extracts and chunks the text; this module only stores it. The source kind is
//! plain text here (`resume`, `job_description`, `notes`, `stories`, checked by the schema) so
//! the store never depends on `dmn-context`'s types, the same way `suggestions.payload` keeps it
//! off `dmn-llm`'s.

use rusqlite::{OptionalExtension, Row, params};
use serde::Serialize;

use crate::error::StoreError;
use crate::store::Store;

/// A document to store, with the chunks cut from it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewSourceDoc {
    /// `resume`, `job_description`, `notes` or `stories`.
    pub kind: String,
    /// Absolute path the text was read from; unique across sources.
    pub path: String,
    /// Unix seconds.
    pub added_at: i64,
    /// Size of the file read.
    pub bytes: i64,
    /// Approximate token count of the whole extracted text.
    pub tokens: i64,
}

/// One chunk of a [`NewSourceDoc`], in order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewChunk {
    pub text: String,
    /// Approximate token count of `text`.
    pub tokens: i64,
}

/// A stored source, with how many chunks it has.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SourceDocRow {
    pub id: i64,
    pub kind: String,
    pub path: String,
    pub added_at: i64,
    pub bytes: i64,
    pub tokens: i64,
    pub chunks: i64,
}

/// A stored chunk.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ChunkRow {
    pub id: i64,
    pub source_id: i64,
    /// Position in its source, from 0.
    pub idx: i64,
    pub text: String,
    pub tokens: i64,
}

/// A stored chunk with the kind and path of its source, for building a retrieval index (#54).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceChunk {
    pub source_id: i64,
    pub kind: String,
    pub path: String,
    /// Position in its source, from 0.
    pub idx: i64,
    pub text: String,
    pub tokens: i64,
}

/// What changes whenever a source is added, replaced or removed: the source count, the highest
/// id (ids are `AUTOINCREMENT`, so a replacement always raises it) and the chunk count. Cheap to
/// read per candidate, so a session notices `dmn sources add` run in another terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SourcesSignature {
    pub sources: i64,
    pub max_id: i64,
    pub chunks: i64,
}

/// What [`Store::replace_source`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceWrite {
    /// The new source's id.
    pub id: i64,
    /// Sources removed to make room: the same path added before, and, for a single-document
    /// kind, any other document of that kind.
    pub replaced: Vec<SourceDocRow>,
}

impl Store {
    /// Store `source` and its `chunks` in one transaction, replacing any source already stored
    /// at the same path. With `single`, every other source of the same kind is replaced too:
    /// resume and job description are single documents (SPEC § Context and LLM, Sources).
    pub fn replace_source(
        &self,
        source: &NewSourceDoc,
        chunks: &[NewChunk],
        single: bool,
    ) -> Result<SourceWrite, StoreError> {
        let tx = self.conn.unchecked_transaction()?;
        let replaced: Vec<SourceDocRow> = self
            .sources()?
            .into_iter()
            .filter(|s| s.path == source.path || (single && s.kind == source.kind))
            .collect();
        for old in &replaced {
            tx.execute("DELETE FROM sources WHERE id = ?1", [old.id])?;
        }
        tx.execute(
            "INSERT INTO sources (kind, path, added_at, bytes, tokens) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                source.kind,
                source.path,
                source.added_at,
                source.bytes,
                source.tokens
            ],
        )?;
        let id = tx.last_insert_rowid();
        {
            let mut insert = tx.prepare(
                "INSERT INTO chunks (source_id, idx, text, tokens) VALUES (?1, ?2, ?3, ?4)",
            )?;
            for (idx, chunk) in chunks.iter().enumerate() {
                let idx = i64::try_from(idx).unwrap_or(i64::MAX);
                insert.execute(params![id, idx, chunk.text, chunk.tokens])?;
            }
        }
        tx.commit()?;
        Ok(SourceWrite { id, replaced })
    }

    /// Every stored source with its chunk count, ordered by kind then path.
    pub fn sources(&self) -> Result<Vec<SourceDocRow>, StoreError> {
        let mut stmt = self.conn.prepare(
            "SELECT s.id, s.kind, s.path, s.added_at, s.bytes, s.tokens,
                    (SELECT count(*) FROM chunks c WHERE c.source_id = s.id)
             FROM sources s ORDER BY s.kind, s.path",
        )?;
        let rows = stmt
            .query_map([], source_from_row)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// The chunks of one source, in order.
    pub fn source_chunks(&self, source_id: i64) -> Result<Vec<ChunkRow>, StoreError> {
        let mut stmt = self.conn.prepare(
            "SELECT id, source_id, idx, text, tokens FROM chunks WHERE source_id = ?1 ORDER BY idx",
        )?;
        let rows = stmt
            .query_map([source_id], chunk_from_row)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Delete one source and, by cascade, its chunks. Returns whether it existed.
    pub fn delete_source(&self, id: i64) -> Result<bool, StoreError> {
        Ok(self
            .conn
            .execute("DELETE FROM sources WHERE id = ?1", [id])?
            > 0)
    }

    /// Number of (sources, chunks) stored.
    pub fn source_counts(&self) -> Result<(i64, i64), StoreError> {
        Ok(self.conn.query_row(
            "SELECT (SELECT count(*) FROM sources), (SELECT count(*) FROM chunks)",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?)
    }

    /// Every chunk of every source, with its source's kind and path, ordered by source then
    /// position.
    pub fn all_chunks(&self) -> Result<Vec<SourceChunk>, StoreError> {
        let mut stmt = self.conn.prepare(
            "SELECT c.source_id, s.kind, s.path, c.idx, c.text, c.tokens
             FROM chunks c JOIN sources s ON s.id = c.source_id
             ORDER BY c.source_id, c.idx",
        )?;
        let rows = stmt
            .query_map([], |row| {
                Ok(SourceChunk {
                    source_id: row.get(0)?,
                    kind: row.get(1)?,
                    path: row.get(2)?,
                    idx: row.get(3)?,
                    text: row.get(4)?,
                    tokens: row.get(5)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// See [`SourcesSignature`].
    pub fn sources_signature(&self) -> Result<SourcesSignature, StoreError> {
        Ok(self.conn.query_row(
            "SELECT (SELECT count(*) FROM sources), (SELECT coalesce(max(id), 0) FROM sources),
                    (SELECT count(*) FROM chunks)",
            [],
            |row| {
                Ok(SourcesSignature {
                    sources: row.get(0)?,
                    max_id: row.get(1)?,
                    chunks: row.get(2)?,
                })
            },
        )?)
    }

    /// The cached job-description summary for the job description with content hash `hash`
    /// (v5, #54), one requirement per element.
    pub fn jd_summary(&self, hash: &str) -> Result<Option<Vec<String>>, StoreError> {
        let bullets: Option<String> = self
            .conn
            .query_row(
                "SELECT bullets FROM jd_summaries WHERE hash = ?1",
                [hash],
                |row| row.get(0),
            )
            .optional()?;
        Ok(bullets.map(|b| {
            b.lines()
                .map(str::to_owned)
                .filter(|l| !l.is_empty())
                .collect()
        }))
    }

    /// Cache the summary of the job description with content hash `hash`, replacing any other:
    /// one job description is loaded at a time. Newlines inside a bullet become spaces, so the
    /// stored lines read back as the same bullets.
    pub fn put_jd_summary(
        &self,
        hash: &str,
        bullets: &[String],
        created_at: i64,
    ) -> Result<(), StoreError> {
        let joined = bullets
            .iter()
            .map(|b| b.split_whitespace().collect::<Vec<_>>().join(" "))
            .filter(|b| !b.is_empty())
            .collect::<Vec<_>>()
            .join("\n");
        let tx = self.conn.unchecked_transaction()?;
        tx.execute("DELETE FROM jd_summaries WHERE hash <> ?1", [hash])?;
        tx.execute(
            "INSERT INTO jd_summaries (hash, bullets, created_at) VALUES (?1, ?2, ?3)
             ON CONFLICT (hash) DO UPDATE SET bullets = excluded.bullets,
                                              created_at = excluded.created_at",
            params![hash, joined, created_at],
        )?;
        tx.commit()?;
        Ok(())
    }
}

fn source_from_row(row: &Row<'_>) -> rusqlite::Result<SourceDocRow> {
    Ok(SourceDocRow {
        id: row.get(0)?,
        kind: row.get(1)?,
        path: row.get(2)?,
        added_at: row.get(3)?,
        bytes: row.get(4)?,
        tokens: row.get(5)?,
        chunks: row.get(6)?,
    })
}

fn chunk_from_row(row: &Row<'_>) -> rusqlite::Result<ChunkRow> {
    Ok(ChunkRow {
        id: row.get(0)?,
        source_id: row.get(1)?,
        idx: row.get(2)?,
        text: row.get(3)?,
        tokens: row.get(4)?,
    })
}
