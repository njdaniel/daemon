//! Adding and removing sources: what `dmn sources add` and `dmn sources rm` do.

use std::path::{Path, PathBuf};

use dmn_store::{NewChunk, NewSourceDoc, SourceDocRow, Store};

use crate::chunk::{approx_tokens, chunk};
use crate::error::ContextError;
use crate::extract::{Format, extract};
use crate::kind::SourceKind;

/// One document stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Added {
    pub id: i64,
    /// Absolute path, as stored.
    pub path: PathBuf,
    pub chunks: usize,
    /// Approximate tokens in the whole document.
    pub tokens: usize,
    /// Sources this one replaced: the same path added before, or the previous resume / job
    /// description.
    pub replaced: Vec<SourceDocRow>,
}

/// One file of a folder that was not stored, and why (a one-line reason).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refused {
    pub path: PathBuf,
    pub reason: String,
}

/// What [`ingest`] did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Ingested {
    pub added: Vec<Added>,
    /// Only for a folder: files skipped or refused. A single file that cannot be stored is an
    /// error instead.
    pub refused: Vec<Refused>,
}

/// Add the file or folder at `path` as `kind`, at unix time `now`.
///
/// A file becomes one source. A folder (notes and stories only) becomes one source per
/// supported file directly inside it, in name order; hidden files are ignored, and every other
/// file is refused with its reason while the rest are stored. Re-adding a path replaces its
/// chunks; adding a resume or job description replaces the previous one.
pub fn ingest(
    store: &Store,
    path: &Path,
    kind: SourceKind,
    now: i64,
) -> Result<Ingested, ContextError> {
    let path = std::fs::canonicalize(path).map_err(|source| ContextError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    if !path.is_dir() {
        let added = store_file(store, &path, kind, now)?;
        return Ok(Ingested {
            added: vec![added],
            refused: Vec::new(),
        });
    }
    if kind.is_single_document() {
        return Err(ContextError::FolderForSingleDocument {
            path,
            kind: kind.as_str(),
        });
    }
    let mut files = Vec::new();
    let entries = std::fs::read_dir(&path).map_err(|source| ContextError::Io {
        path: path.clone(),
        source,
    })?;
    for entry in entries {
        let entry = entry.map_err(|source| ContextError::Io {
            path: path.clone(),
            source,
        })?;
        let hidden = entry.file_name().to_string_lossy().starts_with('.');
        let file = entry.path();
        if !hidden && file.is_file() {
            files.push(file);
        }
    }
    files.sort();

    let mut out = Ingested::default();
    for file in files {
        match store_file(store, &file, kind, now) {
            Ok(added) => out.added.push(added),
            // A store failure is not about this file: stop rather than report it as a refusal.
            Err(ContextError::Store(e)) => return Err(ContextError::Store(e)),
            Err(e) => out.refused.push(Refused {
                path: file,
                reason: e.to_string(),
            }),
        }
    }
    if out.added.is_empty() {
        let reason = if out.refused.is_empty() {
            "the folder has no files".to_owned()
        } else {
            format!("{} file(s) refused", out.refused.len())
        };
        return Err(ContextError::NothingIngested { path, reason });
    }
    Ok(out)
}

fn store_file(
    store: &Store,
    path: &Path,
    kind: SourceKind,
    now: i64,
) -> Result<Added, ContextError> {
    if Format::of(path).is_none() {
        return Err(ContextError::Unsupported {
            path: path.to_path_buf(),
        });
    }
    let stored_path = path
        .to_str()
        .ok_or_else(|| ContextError::NonUtf8Path {
            path: path.to_path_buf(),
        })?
        .to_owned();
    let (text, bytes) = extract(path)?;
    let chunks = chunk(&text);
    let tokens = approx_tokens(text.split_whitespace().count());
    let new = NewSourceDoc {
        kind: kind.as_str().to_owned(),
        path: stored_path,
        added_at: now,
        bytes: i64::try_from(bytes).unwrap_or(i64::MAX),
        tokens: i64::try_from(tokens).unwrap_or(i64::MAX),
    };
    let rows: Vec<NewChunk> = chunks
        .iter()
        .map(|c| NewChunk {
            text: c.text.clone(),
            tokens: i64::try_from(c.tokens).unwrap_or(i64::MAX),
        })
        .collect();
    let write = store.replace_source(&new, &rows, kind.is_single_document())?;
    Ok(Added {
        id: write.id,
        path: path.to_path_buf(),
        chunks: chunks.len(),
        tokens,
        replaced: write.replaced,
    })
}

/// Remove the sources `target` names: a source id from `dmn sources list`, a file path, or a
/// folder path (every source stored from inside it). Returns what was removed; nothing matching
/// is [`ContextError::NotFound`]. The files on disk are never touched.
pub fn remove(store: &Store, target: &str) -> Result<Vec<SourceDocRow>, ContextError> {
    let sources = store.sources()?;
    let by_id = target
        .parse::<i64>()
        .ok()
        .and_then(|id| sources.iter().find(|s| s.id == id));
    let matching: Vec<SourceDocRow> = match by_id {
        Some(s) => vec![s.clone()],
        None => {
            let wanted = resolve(Path::new(target));
            sources
                .into_iter()
                .filter(|s| Path::new(&s.path).starts_with(&wanted))
                .collect()
        }
    };
    if matching.is_empty() {
        return Err(ContextError::NotFound(target.to_owned()));
    }
    for s in &matching {
        store.delete_source(s.id)?;
    }
    Ok(matching)
}

/// `path` as it was stored: canonical when it still exists, otherwise made absolute, so a file
/// deleted from disk can still be removed from the store.
fn resolve(path: &Path) -> PathBuf {
    std::fs::canonicalize(path)
        .or_else(|_| std::path::absolute(path))
        .unwrap_or_else(|_| path.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chunk::CHUNK_TOKENS;

    fn fixtures() -> PathBuf {
        std::fs::canonicalize(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../testdata/context"))
            .unwrap()
    }

    fn store() -> Store {
        Store::open_in_memory().unwrap()
    }

    #[test]
    fn each_fixture_format_ingests_into_bounded_chunks() {
        let store = store();
        let dir = fixtures();
        for (file, kind) in [
            ("resume.pdf", SourceKind::Resume),
            ("job-description.txt", SourceKind::JobDescription),
            ("notes/system-design.md", SourceKind::Notes),
        ] {
            let out = ingest(&store, &dir.join(file), kind, 1).unwrap();
            assert_eq!(out.added.len(), 1, "{file}");
            let added = &out.added[0];
            assert!(added.chunks >= 1, "{file}");
            for c in store.source_chunks(added.id).unwrap() {
                assert!(
                    c.tokens as usize <= CHUNK_TOKENS,
                    "{file}: {} tokens",
                    c.tokens
                );
            }
        }
        let resume = store
            .sources()
            .unwrap()
            .into_iter()
            .find(|s| s.kind == "resume")
            .unwrap();
        assert!(
            resume.chunks >= 2,
            "the ~430-word resume spans more than one chunk: {resume:?}"
        );
        assert!(resume.path.ends_with("testdata/context/resume.pdf"));
    }

    #[test]
    fn a_notes_folder_adds_one_source_per_file_and_refuses_the_rest() {
        let tmp = tempfile::tempdir().unwrap();
        let notes = tmp.path().join("notes");
        std::fs::create_dir(&notes).unwrap();
        std::fs::write(notes.join("b.md"), "# B\nSecond note about caching.").unwrap();
        std::fs::write(notes.join("a.txt"), "First note about queues.").unwrap();
        std::fs::write(notes.join("empty.md"), "").unwrap();
        std::fs::write(notes.join("diagram.png"), b"\x89PNG").unwrap();
        std::fs::write(notes.join(".hidden.md"), "ignored").unwrap();
        std::fs::create_dir(notes.join("sub")).unwrap();

        let store = store();
        let out = ingest(&store, &notes, SourceKind::Notes, 1).unwrap();
        let added: Vec<&str> = out
            .added
            .iter()
            .map(|a| a.path.file_name().unwrap().to_str().unwrap())
            .collect();
        assert_eq!(
            added,
            ["a.txt", "b.md"],
            "name order, hidden and sub-folders skipped"
        );
        let refused: Vec<&str> = out
            .refused
            .iter()
            .map(|r| r.path.file_name().unwrap().to_str().unwrap())
            .collect();
        assert_eq!(refused, ["diagram.png", "empty.md"]);
        assert!(out.refused.iter().all(|r| !r.reason.contains('\n')));
        assert_eq!(store.source_counts().unwrap(), (2, 2));
    }

    #[test]
    fn a_refused_single_file_stores_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let store = store();
        for (name, bytes) in [("empty.txt", &b""[..]), ("resume.docx", &b"PK\x03\x04"[..])] {
            let p = tmp.path().join(name);
            std::fs::write(&p, bytes).unwrap();
            assert!(ingest(&store, &p, SourceKind::Resume, 1).is_err(), "{name}");
        }
        assert_eq!(store.source_counts().unwrap(), (0, 0));
    }

    #[test]
    fn a_folder_is_refused_for_a_single_document_kind() {
        let err = ingest(&store(), &fixtures().join("notes"), SourceKind::Resume, 1).unwrap_err();
        assert!(
            matches!(err, ContextError::FolderForSingleDocument { .. }),
            "{err}"
        );
    }

    #[test]
    fn a_folder_with_nothing_usable_is_an_error() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("x.png"), b"\x89PNG").unwrap();
        let store = store();
        let err = ingest(&store, tmp.path(), SourceKind::Stories, 1).unwrap_err();
        assert!(matches!(err, ContextError::NothingIngested { .. }), "{err}");
        assert_eq!(store.source_counts().unwrap(), (0, 0));
    }

    #[test]
    fn re_adding_replaces_and_a_new_resume_replaces_the_old_one() {
        let store = store();
        let dir = fixtures();
        let first = ingest(&store, &dir.join("resume.pdf"), SourceKind::Resume, 1).unwrap();
        let again = ingest(&store, &dir.join("resume.pdf"), SourceKind::Resume, 2).unwrap();
        assert_eq!(again.added[0].replaced.len(), 1);
        assert_eq!(again.added[0].replaced[0].id, first.added[0].id);
        let md = ingest(&store, &dir.join("resume.md"), SourceKind::Resume, 3).unwrap();
        assert_eq!(md.added[0].replaced.len(), 1, "one resume at a time");
        let kinds: Vec<(String, String)> = store
            .sources()
            .unwrap()
            .into_iter()
            .map(|s| (s.kind, s.path))
            .collect();
        assert_eq!(kinds.len(), 1);
        assert!(kinds[0].1.ends_with("resume.md"));
    }

    #[test]
    fn remove_by_id_by_file_and_by_folder() {
        let store = store();
        let dir = fixtures();
        let jd = ingest(
            &store,
            &dir.join("job-description.txt"),
            SourceKind::JobDescription,
            1,
        )
        .unwrap();
        ingest(&store, &dir.join("notes"), SourceKind::Notes, 1).unwrap();
        ingest(&store, &dir.join("resume.md"), SourceKind::Resume, 1).unwrap();
        assert_eq!(store.sources().unwrap().len(), 4);

        let gone = remove(&store, &jd.added[0].id.to_string()).unwrap();
        assert_eq!(gone.len(), 1);
        let gone = remove(&store, dir.join("notes").to_str().unwrap()).unwrap();
        assert_eq!(gone.len(), 2, "the folder takes both notes");
        let gone = remove(&store, dir.join("resume.md").to_str().unwrap()).unwrap();
        assert_eq!(gone[0].kind, "resume");
        assert_eq!(store.source_counts().unwrap(), (0, 0));
        assert!(matches!(
            remove(&store, "999"),
            Err(ContextError::NotFound(_))
        ));
    }

    #[test]
    fn a_folder_prefix_does_not_match_a_sibling_with_a_longer_name() {
        let tmp = tempfile::tempdir().unwrap();
        for d in ["notes", "notes-old"] {
            std::fs::create_dir(tmp.path().join(d)).unwrap();
            std::fs::write(tmp.path().join(d).join("n.md"), "A note.").unwrap();
        }
        let store = store();
        ingest(&store, &tmp.path().join("notes"), SourceKind::Notes, 1).unwrap();
        ingest(&store, &tmp.path().join("notes-old"), SourceKind::Notes, 1).unwrap();
        let gone = remove(&store, tmp.path().join("notes").to_str().unwrap()).unwrap();
        assert_eq!(gone.len(), 1, "path components, not string prefixes");
    }

    #[test]
    fn a_source_whose_file_was_deleted_can_still_be_removed_by_path() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("note.md");
        std::fs::write(&p, "A note that will be deleted.").unwrap();
        let store = store();
        ingest(&store, &p, SourceKind::Notes, 1).unwrap();
        let stored = store.sources().unwrap()[0].path.clone();
        std::fs::remove_file(&p).unwrap();
        assert_eq!(remove(&store, &stored).unwrap().len(), 1);
    }
}
