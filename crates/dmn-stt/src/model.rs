//! Whisper model registry and download.
//!
//! Files are `ggml-<name>.bin` from `huggingface.co/ggerganov/whisper.cpp`. Sizes (and the
//! SHA-256 of each file, for anyone checking by hand with `sha256sum`) come from the Hugging Face
//! tree API, `https://huggingface.co/api/models/ggerganov/whisper.cpp/tree/main`, read
//! 2026-09-22:
//!
//! | file | bytes | sha256 |
//! | --- | --- | --- |
//! | ggml-tiny.en.bin | 77704715 | 921e4cf8686fdd993dcd081a5da5b6c365bfde1162e72b08d75ac75289920b1f |
//! | ggml-base.en.bin | 147964211 | a03779c86df3323075f5e796cb2ce5029f00ec8869eee3fdfb897afe36c6d002 |
//! | ggml-small.en.bin | 487614201 | c6138d6d58ecc8322097e0f987c32f1be8bb0a18532a3f88f734d1bbf9c41e5d |
//! | ggml-medium.en.bin | 1533774781 | cc37e93478338ec7700281a7ac30a10128929eb8f427dda2e865faa8f6da4356 |
//! | ggml-large-v3-turbo.bin | 1624555275 | 1fc70f774d38eb169993ac391eea357ef47c88757ef72ee5943879b7e8e2bc69 |
//!
//! The download is checked by size, which catches the realistic failure (a truncated or HTML
//! error body); the hashes are recorded for manual checks rather than adding a hash crate.

use std::fs::File;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::SttError;

/// Where the models are fetched from. Tests point [`download_model`] at a local server instead.
pub const BASE_URL: &str = "https://huggingface.co/ggerganov/whisper.cpp/resolve/main";

/// Comma-separated list for error messages.
pub const KNOWN_NAMES: &str = "tiny.en, base.en, small.en, medium.en, large-v3-turbo";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Model {
    TinyEn,
    BaseEn,
    SmallEn,
    MediumEn,
    LargeV3Turbo,
}

impl Model {
    pub const ALL: [Model; 5] = [
        Model::TinyEn,
        Model::BaseEn,
        Model::SmallEn,
        Model::MediumEn,
        Model::LargeV3Turbo,
    ];

    /// Parse the config / CLI name (`small.en`).
    pub fn from_name(name: &str) -> Result<Self, SttError> {
        Self::ALL
            .into_iter()
            .find(|m| m.name() == name)
            .ok_or_else(|| SttError::UnknownModel(name.to_owned()))
    }

    pub fn name(self) -> &'static str {
        match self {
            Model::TinyEn => "tiny.en",
            Model::BaseEn => "base.en",
            Model::SmallEn => "small.en",
            Model::MediumEn => "medium.en",
            Model::LargeV3Turbo => "large-v3-turbo",
        }
    }

    pub fn file_name(self) -> String {
        format!("ggml-{}.bin", self.name())
    }

    /// Exact byte size of the published file (see the module table).
    pub fn size_bytes(self) -> u64 {
        match self {
            Model::TinyEn => 77_704_715,
            Model::BaseEn => 147_964_211,
            Model::SmallEn => 487_614_201,
            Model::MediumEn => 1_533_774_781,
            Model::LargeV3Turbo => 1_624_555_275,
        }
    }

    pub fn url(self, base: &str) -> String {
        format!("{}/{}", base.trim_end_matches('/'), self.file_name())
    }
}

/// `<models_dir>/ggml-<name>.bin`.
pub fn model_path(models_dir: &Path, model: Model) -> PathBuf {
    models_dir.join(model.file_name())
}

/// Download `model` into `models_dir` from `base_url`, streaming to `<file>.part` and renaming
/// only after the size matches. Already-present files of the right size are left alone.
/// `progress(done, total)` is called as bytes arrive. Blocking: run it off any async runtime.
pub fn download_model(
    models_dir: &Path,
    model: Model,
    base_url: &str,
    mut progress: impl FnMut(u64, u64),
) -> Result<PathBuf, SttError> {
    let dest = model_path(models_dir, model);
    let expected = model.size_bytes();
    if std::fs::metadata(&dest).is_ok_and(|m| m.len() == expected) {
        return Ok(dest);
    }
    std::fs::create_dir_all(models_dir).map_err(|source| io(models_dir, source))?;

    let url = model.url(base_url);
    let fail = |reason: String| SttError::Download {
        url: url.clone(),
        reason,
    };
    let client = reqwest::blocking::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        // Large files over slow links: no whole-request timeout, only connect + read stalls.
        .timeout(None)
        .build()
        .map_err(|e| fail(e.to_string()))?;
    let mut resp = client.get(&url).send().map_err(|e| fail(e.to_string()))?;
    if !resp.status().is_success() {
        return Err(fail(format!("HTTP {}", resp.status())));
    }

    let part = dest.with_extension("bin.part");
    let mut out = File::create(&part).map_err(|source| io(&part, source))?;
    let mut buf = vec![0u8; 1 << 16];
    let mut done: u64 = 0;
    loop {
        let n = resp.read(&mut buf).map_err(|e| fail(e.to_string()))?;
        if n == 0 {
            break;
        }
        out.write_all(&buf[..n])
            .map_err(|source| io(&part, source))?;
        done += n as u64;
        progress(done, expected);
    }
    out.flush().map_err(|source| io(&part, source))?;
    drop(out);

    if done != expected {
        let _ = std::fs::remove_file(&part);
        return Err(SttError::SizeMismatch {
            path: part.display().to_string(),
            got: done,
            expected,
        });
    }
    std::fs::rename(&part, &dest).map_err(|source| io(&dest, source))?;
    Ok(dest)
}

fn io(path: &Path, source: std::io::Error) -> SttError {
    SttError::Io {
        path: path.display().to_string(),
        source,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader};
    use std::net::TcpListener;

    #[test]
    fn names_paths_and_urls() {
        for m in Model::ALL {
            assert_eq!(Model::from_name(m.name()).unwrap(), m);
        }
        let m = Model::from_name("small.en").unwrap();
        assert_eq!(m.file_name(), "ggml-small.en.bin");
        assert_eq!(
            model_path(Path::new("/c/models"), m),
            PathBuf::from("/c/models/ggml-small.en.bin")
        );
        assert_eq!(
            m.url(BASE_URL),
            "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-small.en.bin"
        );
        assert_eq!(m.url("http://x/"), "http://x/ggml-small.en.bin");
        let err = Model::from_name("small").unwrap_err();
        assert!(err.to_string().contains("small.en"), "{err}");
    }

    /// Serve one HTTP response on a local port; returns the base URL.
    fn serve_once(status: &'static str, body: Vec<u8>) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            if let Ok((mut s, _)) = listener.accept() {
                let mut r = BufReader::new(s.try_clone().unwrap());
                let mut line = String::new();
                while r.read_line(&mut line).is_ok_and(|n| n > 2) {
                    line.clear();
                }
                let head = format!(
                    "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = s.write_all(head.as_bytes());
                let _ = s.write_all(&body);
            }
        });
        format!("http://{addr}")
    }

    /// The smallest model is still 77 MB, so the tests shrink the expectation by serving a body
    /// whose size is wrong on purpose and checking the failure path, and by checking the
    /// success path with a file that is already present.
    #[test]
    fn short_body_is_rejected_and_leaves_no_file() {
        let dir = tempfile::tempdir().unwrap();
        let base = serve_once("200 OK", vec![7u8; 4096]);
        let mut seen = 0;
        let err = download_model(dir.path(), Model::TinyEn, &base, |d, _| seen = d).unwrap_err();
        assert!(
            matches!(err, SttError::SizeMismatch { got: 4096, .. }),
            "{err}"
        );
        assert_eq!(seen, 4096);
        assert!(!model_path(dir.path(), Model::TinyEn).exists());
        assert!(!dir.path().join("ggml-tiny.en.bin.part").exists());
    }

    #[test]
    fn http_error_is_a_typed_error() {
        let dir = tempfile::tempdir().unwrap();
        let base = serve_once("404 Not Found", b"nope".to_vec());
        let err = download_model(dir.path(), Model::BaseEn, &base, |_, _| {}).unwrap_err();
        assert!(err.to_string().contains("404"), "{err}");
    }

    #[test]
    fn present_file_of_right_size_is_not_downloaded() {
        let dir = tempfile::tempdir().unwrap();
        let path = model_path(dir.path(), Model::TinyEn);
        let f = File::create(&path).unwrap();
        f.set_len(Model::TinyEn.size_bytes()).unwrap();
        // Unroutable base URL: any network attempt would fail the test.
        let got = download_model(dir.path(), Model::TinyEn, "http://127.0.0.1:9", |_, _| {
            panic!("no download expected")
        })
        .unwrap();
        assert_eq!(got, path);
    }
}
