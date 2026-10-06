//! XDG locations for everything Daemon reads or writes. See docs/SPEC.md § Data, config and privacy.

use std::path::{Path, PathBuf};

use directories::ProjectDirs;

/// Every path Daemon touches on disk. Built once at start from XDG, or from a root for tests.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Paths {
    /// `~/.config/dmn/config.toml`
    pub config_file: PathBuf,
    /// `~/.local/share/dmn/`: the data root (the enrolled voice clip lives directly here).
    pub data_dir: PathBuf,
    /// `~/.local/share/dmn/sources/`: copies of ingested resume, JD, notes, stories.
    pub sources_dir: PathBuf,
    /// `~/.local/share/dmn/dmn.sqlite`
    pub db_file: PathBuf,
    /// `~/.local/share/dmn/sessions/`: optional JSONL exports and, if enabled, session WAVs.
    pub sessions_dir: PathBuf,
    /// `~/.cache/dmn/models/`: whisper ggml files.
    pub models_dir: PathBuf,
    /// `~/.local/state/dmn/dmn.log`
    pub log_file: PathBuf,
}

impl Paths {
    /// Resolve from the XDG base directories. `None` only when no home directory can be found.
    pub fn from_xdg() -> Option<Self> {
        let dirs = ProjectDirs::from("", "", "dmn")?;
        let state = dirs
            .state_dir()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| dirs.data_local_dir().to_path_buf());
        Some(Self {
            config_file: dirs.config_dir().join("config.toml"),
            data_dir: dirs.data_dir().to_path_buf(),
            sources_dir: dirs.data_dir().join("sources"),
            db_file: dirs.data_dir().join("dmn.sqlite"),
            sessions_dir: dirs.data_dir().join("sessions"),
            models_dir: dirs.cache_dir().join("models"),
            log_file: state.join("dmn.log"),
        })
    }

    /// Put every path under one root. Used by tests and by `--root` style overrides.
    pub fn under(root: &Path) -> Self {
        Self {
            config_file: root.join("config/config.toml"),
            data_dir: root.join("share"),
            sources_dir: root.join("share/sources"),
            db_file: root.join("share/dmn.sqlite"),
            sessions_dir: root.join("share/sessions"),
            models_dir: root.join("cache/models"),
            log_file: root.join("state/dmn.log"),
        }
    }
}
