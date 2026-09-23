//! The crate's error type. Every fallible store operation returns [`StoreError`].

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("jsonl export encode: {0}")]
    Json(#[from] serde_json::Error),
    #[error("jsonl export write: {0}")]
    Io(#[from] std::io::Error),
}
