//! JSON log lines to `Paths::log_file`, rotated by size. Filter with `DMN_LOG` (default `info`).
//!
//! The file is the only log sink during a call: the TUI and the window own the terminal and the
//! status dots are where errors surface. Nothing here ever logs an API key: keys live in
//! [`crate::config::ApiKey`], whose Debug and Display are redacted.

use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use tracing_subscriber::EnvFilter;

/// Rotate when the file reaches this size. docs/SPEC.md: "rotated at 10 MB".
pub const ROTATE_AT: u64 = 10 * 1024 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum LogError {
    #[error("cannot open log {path}: {source}")]
    Open {
        path: String,
        #[source]
        source: io::Error,
    },
    #[error("a global tracing subscriber is already installed")]
    AlreadyInstalled,
}

/// A writer that keeps at most one rotated predecessor (`dmn.log.1`).
struct RotatingFile {
    path: PathBuf,
    file: File,
    len: u64,
    rotate_at: u64,
}

impl RotatingFile {
    fn open(path: &Path, rotate_at: u64) -> io::Result<Self> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let file = OpenOptions::new().create(true).append(true).open(path)?;
        let len = file.metadata()?.len();
        Ok(Self {
            path: path.to_path_buf(),
            file,
            len,
            rotate_at,
        })
    }

    fn rotate(&mut self) -> io::Result<()> {
        self.file.flush()?;
        let old = self.path.with_extension("log.1");
        // A failed rename must not lose the current file; fall through and keep appending.
        std::fs::rename(&self.path, &old)?;
        self.file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        self.len = 0;
        Ok(())
    }
}

impl Write for RotatingFile {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if self.len >= self.rotate_at {
            self.rotate()?;
        }
        let n = self.file.write(buf)?;
        self.len += n as u64;
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }
}

/// Cloneable handle handed to `tracing_subscriber` as the `MakeWriter`.
#[derive(Clone)]
pub struct LogWriter(Arc<Mutex<RotatingFile>>);

impl LogWriter {
    pub fn open(path: &Path) -> Result<Self, LogError> {
        Self::open_with_limit(path, ROTATE_AT)
    }

    pub fn open_with_limit(path: &Path, rotate_at: u64) -> Result<Self, LogError> {
        RotatingFile::open(path, rotate_at)
            .map(|f| Self(Arc::new(Mutex::new(f))))
            .map_err(|source| LogError::Open {
                path: path.display().to_string(),
                source,
            })
    }
}

impl Write for LogWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self.0.lock() {
            Ok(mut f) => f.write(buf),
            Err(_) => Err(io::Error::other("log writer poisoned")),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self.0.lock() {
            Ok(mut f) => f.flush(),
            Err(_) => Err(io::Error::other("log writer poisoned")),
        }
    }
}

/// Install the global JSON subscriber writing to `log_file`. Call once, from `main`.
pub fn init(log_file: &Path) -> Result<(), LogError> {
    let writer = LogWriter::open(log_file)?;
    let filter = EnvFilter::try_from_env("DMN_LOG").unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .json()
        .with_env_filter(filter)
        .with_writer(move || writer.clone())
        .with_ansi(false)
        .try_init()
        .map_err(|_| LogError::AlreadyInstalled)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tracing_subscriber::fmt::MakeWriter;

    #[test]
    fn rotates_at_limit_and_keeps_one_predecessor() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state/dmn.log");
        let mut w = LogWriter::open_with_limit(&path, 100).unwrap();
        let line = [b'x'; 40];
        for _ in 0..5 {
            w.write_all(&line).unwrap();
            w.write_all(b"\n").unwrap();
        }
        w.flush().unwrap();
        let rotated = path.with_extension("log.1");
        assert!(rotated.exists(), "rotation should have happened");
        let cur = std::fs::metadata(&path).unwrap().len();
        let old = std::fs::metadata(&rotated).unwrap().len();
        assert!(
            old >= 100,
            "predecessor holds the pre-rotation bytes: {old}"
        );
        assert!(cur < 100, "current file restarted: {cur}");
        assert_eq!(cur + old, 5 * 41, "no bytes lost");
    }

    #[test]
    fn lines_are_json_and_never_carry_the_key() {
        use tracing_subscriber::prelude::*;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("dmn.log");
        let writer = LogWriter::open(&path).unwrap();
        let mw = move || writer.clone();
        let layer = tracing_subscriber::fmt::layer()
            .json()
            .with_ansi(false)
            .with_writer(mw.clone());
        let sub = tracing_subscriber::registry().with(layer);
        let key = crate::config::ApiKey::new("sk-do-not-log".into());
        tracing::subscriber::with_default(sub, || {
            tracing::info!(provider = "anthropic", key = ?key, "provider ready");
        });
        mw.make_writer().flush().unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        let line = text.lines().next().expect("one log line");
        let v: serde_json::Value = serde_json::from_str(line).expect("line is JSON");
        assert_eq!(v["fields"]["message"], "provider ready");
        assert_eq!(v["fields"]["provider"], "anthropic");
        assert_eq!(v["level"], "INFO");
        assert!(!text.contains("do-not-log"), "key leaked: {text}");
        assert!(text.contains("redacted"));
    }
}
