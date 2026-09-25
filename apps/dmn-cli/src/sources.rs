//! `dmn sources add|list|rm` (#53): the context documents answers draw on (SPEC § Context and
//! LLM, Sources). Extraction, chunking and storage live in `dmn-context`; this module parses the
//! command line and prints.

use std::path::{Path, PathBuf};

use clap::{Subcommand, ValueEnum};
use dmn_context::{ContextError, SourceKind};
use dmn_core::Paths;
use dmn_core::recorder::{self, RecorderError};

#[derive(Subcommand, Debug)]
pub enum SourcesAction {
    /// Add a file, or a folder of notes or stories (one file each). PDF, Markdown or plain
    /// text. Re-adding a path replaces it; a new resume or job description replaces the old one
    Add {
        /// The file or folder to add
        path: PathBuf,
        /// What it is
        #[arg(long, value_enum)]
        kind: KindArg,
    },
    /// List stored sources: id, kind, chunk count, approximate tokens, path
    List,
    /// Remove sources by id (from `dmn sources list`) or by path; a folder path removes every
    /// source added from it. Files on disk are not touched
    Rm {
        /// A source id or a path
        target: String,
    },
}

/// `--kind`, spelled as the SPEC does (`job_description`), with `job-description` and `jd`
/// accepted too.
#[derive(Clone, Copy, Debug, ValueEnum)]
pub enum KindArg {
    Resume,
    #[value(name = "job_description", alias = "job-description", alias = "jd")]
    JobDescription,
    Notes,
    Stories,
}

impl From<KindArg> for SourceKind {
    fn from(k: KindArg) -> Self {
        match k {
            KindArg::Resume => SourceKind::Resume,
            KindArg::JobDescription => SourceKind::JobDescription,
            KindArg::Notes => SourceKind::Notes,
            KindArg::Stories => SourceKind::Stories,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum SourcesError {
    #[error(transparent)]
    Context(#[from] ContextError),
    #[error(transparent)]
    Recorder(#[from] RecorderError),
}

pub fn run(paths: &Paths, action: SourcesAction) -> Result<(), SourcesError> {
    match action {
        SourcesAction::Add { path, kind } => add(paths, &path, kind.into()),
        SourcesAction::List => list(paths),
        SourcesAction::Rm { target } => rm(paths, &target),
    }
}

fn add(paths: &Paths, path: &Path, kind: SourceKind) -> Result<(), SourcesError> {
    let store = recorder::open_store(paths)?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX));
    let out = dmn_context::ingest(&store, path, kind, now)?;
    for a in &out.added {
        let replaced: Vec<String> = a
            .replaced
            .iter()
            .filter(|r| Path::new(&r.path) != a.path)
            .map(|r| format!("#{} {}", r.id, r.path))
            .collect();
        let verb = if a.replaced.iter().any(|r| Path::new(&r.path) == a.path) {
            "re-added"
        } else {
            "added"
        };
        println!(
            "{verb} #{} {kind} {}: {} chunk(s), ~{} tokens{}",
            a.id,
            a.path.display(),
            a.chunks,
            a.tokens,
            if replaced.is_empty() {
                String::new()
            } else {
                format!(" (replaces {})", replaced.join(", "))
            }
        );
    }
    for r in &out.refused {
        println!("refused {}", r.reason);
    }
    tracing::info!(
        kind = kind.as_str(),
        added = out.added.len(),
        refused = out.refused.len(),
        "sources add"
    );
    Ok(())
}

fn list(paths: &Paths) -> Result<(), SourcesError> {
    if !paths.db_file.is_file() {
        println!(
            "no sources yet ({} does not exist)",
            paths.db_file.display()
        );
        return Ok(());
    }
    let store = recorder::open_store(paths)?;
    let sources = store.sources().map_err(ContextError::from)?;
    if sources.is_empty() {
        println!("no sources; add one with `dmn sources add <path> --kind <kind>`");
        return Ok(());
    }
    println!(
        "{:<5} {:<16} {:>6} {:>8}  path",
        "id", "kind", "chunks", "~tokens"
    );
    for s in sources {
        println!(
            "#{:<4} {:<16} {:>6} {:>8}  {}",
            s.id, s.kind, s.chunks, s.tokens, s.path
        );
    }
    Ok(())
}

fn rm(paths: &Paths, target: &str) -> Result<(), SourcesError> {
    if !paths.db_file.is_file() {
        return Err(ContextError::NotFound(target.to_owned()).into());
    }
    let store = recorder::open_store(paths)?;
    let removed = dmn_context::remove(&store, target)?;
    // Like `sessions purge`: drop the free pages that still hold the removed text.
    store.compact().map_err(ContextError::from)?;
    for s in &removed {
        println!(
            "removed #{} {} {} ({} chunk(s))",
            s.id, s.kind, s.path, s.chunks
        );
    }
    tracing::info!(removed = removed.len(), "sources rm");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(rel: &str) -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../testdata/context")
            .join(rel)
    }

    #[test]
    fn add_list_and_rm_through_the_cli_layer() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::under(dir.path());
        list(&paths).unwrap(); // no database yet: a message, not an error
        assert!(!paths.db_file.exists(), "list does not create the database");

        for (file, kind) in [
            ("resume.pdf", KindArg::Resume),
            ("job-description.txt", KindArg::JobDescription),
            ("notes", KindArg::Notes),
        ] {
            run(
                &paths,
                SourcesAction::Add {
                    path: fixture(file),
                    kind,
                },
            )
            .unwrap();
        }
        let store = recorder::open_store(&paths).unwrap();
        let kinds: Vec<String> = store
            .sources()
            .unwrap()
            .into_iter()
            .map(|s| s.kind)
            .collect();
        assert_eq!(kinds, ["job_description", "notes", "notes", "resume"]);
        list(&paths).unwrap();

        let notes = std::fs::canonicalize(fixture("notes")).unwrap();
        run(
            &paths,
            SourcesAction::Rm {
                target: notes.to_str().unwrap().into(),
            },
        )
        .unwrap();
        assert_eq!(store.sources().unwrap().len(), 2);
        let err = run(
            &paths,
            SourcesAction::Rm {
                target: "4242".into(),
            },
        )
        .unwrap_err();
        assert!(err.to_string().contains("no source matches"), "{err}");
    }

    #[test]
    fn a_refused_file_is_an_error_with_one_line_and_stores_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::under(dir.path());
        let empty = dir.path().join("empty.md");
        std::fs::write(&empty, "").unwrap();
        let err = run(
            &paths,
            SourcesAction::Add {
                path: empty,
                kind: KindArg::Resume,
            },
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("empty"), "{err}");
        assert!(!err.contains('\n'));
        let store = recorder::open_store(&paths).unwrap();
        assert_eq!(store.source_counts().unwrap(), (0, 0));
    }

    #[test]
    fn job_description_takes_the_spec_spelling_and_two_aliases() {
        for s in ["job_description", "job-description", "jd"] {
            assert!(
                matches!(KindArg::from_str(s, false), Ok(KindArg::JobDescription)),
                "{s}"
            );
        }
    }
}
