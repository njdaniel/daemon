//! The crate's error type. Each variant renders as the one-line reason `dmn sources` prints.

use std::path::PathBuf;

#[derive(Debug, thiserror::Error)]
pub enum ContextError {
    #[error("{path}: {source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("{path}: unsupported file type (expected .pdf, .md, .markdown, .txt or .text)")]
    Unsupported { path: PathBuf },
    #[error("{path}: empty, no text to store")]
    Empty { path: PathBuf },
    #[error(
        "{path}: the PDF has no extractable text (scanned or image-only? OCR is not supported)"
    )]
    NoText { path: PathBuf },
    #[error("{path}: not UTF-8 text")]
    NotUtf8 { path: PathBuf },
    #[error("{path}: cannot read the PDF: {reason}")]
    Pdf { path: PathBuf, reason: String },
    #[error("{path}: the path is not valid UTF-8, so it cannot be stored")]
    NonUtf8Path { path: PathBuf },
    #[error("{path}: a {kind} is a single document; give a file, not a folder")]
    FolderForSingleDocument { path: PathBuf, kind: &'static str },
    #[error("{path}: nothing ingested ({reason})")]
    NothingIngested { path: PathBuf, reason: String },
    #[error("no source matches {0:?} (see `dmn sources list`)")]
    NotFound(String),
    #[error(transparent)]
    Store(#[from] dmn_store::StoreError),
}
