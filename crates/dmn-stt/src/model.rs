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
//! The multilingual files (#77: they detect the language and can translate to English; the
//! `.en` files cannot), read from the same tree 2026-09-26:
//!
//! | file | bytes | sha256 |
//! | --- | --- | --- |
//! | ggml-tiny.bin | 77691713 | be07e048e1e599ad46341c8d2a135645097a538221678b7acdd1b1919c6e1b21 |
//! | ggml-base.bin | 147951465 | 60ed5bc3dd14eea856493d334349b405782ddcaf0028d4b5df4088345fba2efe |
//! | ggml-small.bin | 487601967 | 1be3a9b2063867b937e64e2ec7483364a79917e157fa98c5d94b5c1fffea987b |
//! | ggml-medium.bin | 1533763059 | 6c14d5adee5f86394037b4e4e8b59f1673b6cee10e3cf0b11bbdbee79c156208 |
//! | ggml-large-v3.bin | 3095033483 | 64d182b440b98d5203c4f9bd541544d84c605196c4f7b845dfa11fb23594d1e2 |
//!
//! #84: both are checked. The size catches a truncated or HTML error body at once; the SHA-256
//! catches a file of the right size with wrong bytes, which on the dev box (2026-09-29) was three
//! models with a handful of flipped bits each, and `medium` then returned nothing but `,` and
//! `!!!!`. [`Model::sha256`] holds the hashes below, and a test keeps this table and the code
//! in step. The download hashes as it streams; a file already in place is re-hashed before it
//! is trusted, so `dmn doctor --fetch` replaces a damaged one instead of leaving it.

use std::fs::File;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use sha2::{Digest, Sha256};

use crate::SttError;

/// Where the models are fetched from. Tests point [`download_model`] at a local server instead.
pub const BASE_URL: &str = "https://huggingface.co/ggerganov/whisper.cpp/resolve/main";

/// Slowest link a download is allowed to assume, for the whole-request deadline below.
pub const MIN_RATE_BYTES_PER_S: u64 = 200 * 1024;
/// Floor for that deadline, so a small file on a slow link is not cut off.
pub const MIN_DEADLINE: Duration = Duration::from_secs(300);
/// A `.part.<pid>` file untouched for this long is assumed to be from a killed process and is
/// deleted at the start of the next download of that model.
pub const STALE_PARTIAL_AGE: Duration = Duration::from_secs(3600);

/// Whole-request deadline for `model`: the file at [`MIN_RATE_BYTES_PER_S`], at least
/// [`MIN_DEADLINE`]. reqwest's blocking client offers only a whole-request timeout, not a
/// per-read one, so this is what keeps a stalled server from hanging the CLI for ever; it is a
/// generous bound, not a tight stall detector.
pub fn deadline_for(model: Model) -> Duration {
    Duration::from_secs(model.size_bytes() / MIN_RATE_BYTES_PER_S).max(MIN_DEADLINE)
}

/// Comma-separated list for error messages.
pub const KNOWN_NAMES: &str = "tiny.en, base.en, small.en, medium.en (English only); tiny, base, small, medium, large-v3 (multilingual, translate to English); large-v3-turbo (multilingual, no translation)";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Model {
    TinyEn,
    BaseEn,
    SmallEn,
    MediumEn,
    /// Multilingual (#77).
    Tiny,
    Base,
    Small,
    Medium,
    /// The full large model: translates, unlike its turbo distillation.
    LargeV3,
    LargeV3Turbo,
}

impl Model {
    pub const ALL: [Model; 10] = [
        Model::TinyEn,
        Model::BaseEn,
        Model::SmallEn,
        Model::MediumEn,
        Model::Tiny,
        Model::Base,
        Model::Small,
        Model::Medium,
        Model::LargeV3,
        Model::LargeV3Turbo,
    ];

    /// The `.en` files transcribe English only: they have no language detection and cannot
    /// translate, so `[stt].language` other than `en` needs a multilingual model (#77).
    pub fn is_english_only(self) -> bool {
        matches!(
            self,
            Model::TinyEn | Model::BaseEn | Model::SmallEn | Model::MediumEn
        )
    }

    /// Whether whisper's translate task works on this model. `large-v3-turbo` is multilingual
    /// but was trained without translation data: given the translate task it transcribes in
    /// the language it hears (measured 2026-09-28 on the Spanish fixture: Spanish text back,
    /// WER 99 % against the English reference; re-measured on a checksum-verified file
    /// 2026-09-29, #85: 21 of 21 finals in Spanish), so it cannot serve `[stt].language` other
    /// than `en` either.
    pub fn translates(self) -> bool {
        matches!(
            self,
            Model::Tiny | Model::Base | Model::Small | Model::Medium | Model::LargeV3
        )
    }

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
            Model::Tiny => "tiny",
            Model::Base => "base",
            Model::Small => "small",
            Model::Medium => "medium",
            Model::LargeV3 => "large-v3",
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
            Model::Tiny => 77_691_713,
            Model::Base => 147_951_465,
            Model::Small => 487_601_967,
            Model::Medium => 1_533_763_059,
            Model::LargeV3 => 3_095_033_483,
            Model::LargeV3Turbo => 1_624_555_275,
        }
    }

    /// SHA-256 of the published file, lowercase hex (see the module table).
    pub fn sha256(self) -> &'static str {
        match self {
            Model::TinyEn => "921e4cf8686fdd993dcd081a5da5b6c365bfde1162e72b08d75ac75289920b1f",
            Model::BaseEn => "a03779c86df3323075f5e796cb2ce5029f00ec8869eee3fdfb897afe36c6d002",
            Model::SmallEn => "c6138d6d58ecc8322097e0f987c32f1be8bb0a18532a3f88f734d1bbf9c41e5d",
            Model::MediumEn => "cc37e93478338ec7700281a7ac30a10128929eb8f427dda2e865faa8f6da4356",
            Model::Tiny => "be07e048e1e599ad46341c8d2a135645097a538221678b7acdd1b1919c6e1b21",
            Model::Base => "60ed5bc3dd14eea856493d334349b405782ddcaf0028d4b5df4088345fba2efe",
            Model::Small => "1be3a9b2063867b937e64e2ec7483364a79917e157fa98c5d94b5c1fffea987b",
            Model::Medium => "6c14d5adee5f86394037b4e4e8b59f1673b6cee10e3cf0b11bbdbee79c156208",
            Model::LargeV3 => "64d182b440b98d5203c4f9bd541544d84c605196c4f7b845dfa11fb23594d1e2",
            Model::LargeV3Turbo => {
                "1fc70f774d38eb169993ac391eea357ef47c88757ef72ee5943879b7e8e2bc69"
            }
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

/// #84: the file at `path` is exactly `model`'s published file: its size first (cheap, and the
/// usual failure), then its SHA-256 (about a second per GB). What `dmn doctor` runs.
pub fn verify_model(path: &Path, model: Model) -> Result<(), SttError> {
    verify_file(path, model.size_bytes(), model.sha256())
}

/// [`verify_model`] for any file: `expected` bytes whose SHA-256 is `sha256`.
pub fn verify_file(path: &Path, expected: u64, sha256: &str) -> Result<(), SttError> {
    let len = std::fs::metadata(path)
        .map_err(|source| io(path, source))?
        .len();
    if len != expected {
        return Err(SttError::SizeMismatch {
            path: path.display().to_string(),
            got: len,
            expected,
        });
    }
    let got = file_sha256(path)?;
    if got != sha256 {
        return Err(SttError::ChecksumMismatch {
            path: path.display().to_string(),
            got,
            expected: sha256.to_owned(),
        });
    }
    Ok(())
}

/// SHA-256 of the file at `path`, lowercase hex, read in 1 MB pieces.
pub fn file_sha256(path: &Path) -> Result<String, SttError> {
    let mut f = File::open(path).map_err(|source| io(path, source))?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = f.read(&mut buf).map_err(|source| io(path, source))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hex(&hasher.finalize()))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Download `model` into `models_dir` from `base_url`, streaming to `<file>.part` and renaming
/// only after the size matches. Already-present files of the right size are left alone.
/// `progress(done, total)` is called as bytes arrive. Blocking: run it off any async runtime.
pub fn download_model(
    models_dir: &Path,
    model: Model,
    base_url: &str,
    progress: impl FnMut(u64, u64),
) -> Result<PathBuf, SttError> {
    download_model_with(models_dir, model, base_url, deadline_for(model), progress)
}

/// [`download_model`] with the deadline injected; tests use a short one.
pub fn download_model_with(
    models_dir: &Path,
    model: Model,
    base_url: &str,
    deadline: Duration,
    progress: impl FnMut(u64, u64),
) -> Result<PathBuf, SttError> {
    download_file(
        &model_path(models_dir, model),
        &model.url(base_url),
        model.size_bytes(),
        Some(model.sha256()),
        deadline,
        progress,
    )
}

/// Download `url` to `dest` the way [`download_model`] does: stream to a per-process
/// `<file>.part.<pid>`, hashing as the bytes arrive, and rename only once `expected` bytes with
/// the SHA-256 `sha256` (when given) are there. A file already in place is left alone only when
/// it passes the same check, so a damaged one is downloaded again (#84). Any model file uses it,
/// not only whisper's (#62: the Silero VAD model).
pub fn download_file(
    dest: &Path,
    url: &str,
    expected: u64,
    sha256: Option<&str>,
    deadline: Duration,
    mut progress: impl FnMut(u64, u64),
) -> Result<PathBuf, SttError> {
    let dest = dest.to_path_buf();
    if std::fs::metadata(&dest).is_ok_and(|m| m.len() == expected) {
        match sha256.map(|want| (want, file_sha256(&dest))) {
            None => return Ok(dest),
            Some((want, Ok(got))) if got == want => return Ok(dest),
            Some((want, got)) => tracing::warn!(
                path = %dest.display(),
                got = ?got.ok(),
                expected = want,
                "model file has the right size and the wrong sha256; downloading it again"
            ),
        }
    }
    let dir = dest.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(dir).map_err(|source| io(dir, source))?;
    sweep_partials_of(&dest, STALE_PARTIAL_AGE);

    let url = url.to_owned();
    let fail = |reason: String| SttError::Download {
        url: url.clone(),
        reason,
    };
    let client = reqwest::blocking::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        // Generous whole-request deadline scaled to the file size (see `deadline_for`): a
        // stalled server fails instead of hanging for ever, and a slow but working link still
        // finishes.
        .timeout(deadline)
        .build()
        .map_err(|e| fail(e.to_string()))?;
    let mut resp = client.get(&url).send().map_err(|e| fail(e.to_string()))?;
    if !resp.status().is_success() {
        return Err(fail(format!("HTTP {}", resp.status())));
    }

    // Per-process scratch name: two `dmn doctor --fetch` runs at once must not write to the same
    // file. The rename at the end is atomic, so the loser simply overwrites with identical bytes.
    // `Partial` removes it on every error path; a crash is covered by the sweep above.
    let part = Partial::new(part_path(&dest, std::process::id()));
    let mut out = File::create(part.path()).map_err(|source| io(part.path(), source))?;
    let mut buf = vec![0u8; 1 << 16];
    let mut done: u64 = 0;
    let mut hasher = Sha256::new();
    loop {
        let n = resp.read(&mut buf).map_err(|e| fail(e.to_string()))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        out.write_all(&buf[..n])
            .map_err(|source| io(part.path(), source))?;
        done += n as u64;
        progress(done, expected);
    }
    out.flush().map_err(|source| io(part.path(), source))?;
    drop(out);

    if done != expected {
        return Err(SttError::SizeMismatch {
            path: part.path().display().to_string(),
            got: done,
            expected,
        });
    }
    if let Some(want) = sha256 {
        let got = hex(&hasher.finalize());
        if got != want {
            return Err(SttError::ChecksumMismatch {
                path: part.path().display().to_string(),
                got,
                expected: want.to_owned(),
            });
        }
    }
    part.rename_to(&dest)?;
    Ok(dest)
}

/// A partially downloaded file that deletes itself unless it is renamed into place. Every early
/// return from [`download_model_with`] therefore leaves the models directory as it found it.
struct Partial {
    path: PathBuf,
    armed: bool,
}

impl Partial {
    fn new(path: PathBuf) -> Self {
        Self { path, armed: true }
    }

    fn path(&self) -> &Path {
        &self.path
    }

    /// Move the finished file into place and disarm the guard. A failed rename leaves the guard
    /// armed, so the scratch file is still cleaned up.
    fn rename_to(mut self, dest: &Path) -> Result<(), SttError> {
        std::fs::rename(&self.path, dest).map_err(|source| io(dest, source))?;
        self.armed = false;
        Ok(())
    }
}

impl Drop for Partial {
    fn drop(&mut self) {
        if self.armed && std::fs::remove_file(&self.path).is_ok() {
            tracing::debug!(path = %self.path.display(), "removed partial download");
        }
    }
}

/// `<dest>.part.<pid>`: the scratch file a download of `dest` streams into.
fn part_path(dest: &Path, pid: u32) -> PathBuf {
    let name = dest
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    dest.with_file_name(format!("{name}.part.{pid}"))
}

/// Delete `<model>.bin.part.*` files older than [`STALE_PARTIAL_AGE`]. A `Drop` cannot run after
/// SIGKILL or a power cut, so leftovers are reclaimed the next time the same model is fetched.
#[cfg(test)]
fn sweep_stale_partials(models_dir: &Path, model: Model) {
    sweep_stale_partials_older_than(models_dir, model, STALE_PARTIAL_AGE);
}

/// [`sweep_stale_partials`] with the age threshold injected; tests pass zero.
#[cfg(test)]
fn sweep_stale_partials_older_than(models_dir: &Path, model: Model, max_age: Duration) {
    sweep_partials_of(&model_path(models_dir, model), max_age);
}

/// Delete `<dest>.part.*` files next to `dest` that are older than `max_age`.
fn sweep_partials_of(dest: &Path, max_age: Duration) {
    let Some(file_name) = dest.file_name() else {
        return;
    };
    let prefix = format!("{}.part.", file_name.to_string_lossy());
    let dir = dest.parent().unwrap_or_else(|| Path::new("."));
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if !name.starts_with(&prefix) {
            continue;
        }
        let old = entry
            .metadata()
            .and_then(|m| m.modified())
            .and_then(|t| t.elapsed().map_err(std::io::Error::other))
            .is_ok_and(|age| age >= max_age);
        if old && std::fs::remove_file(entry.path()).is_ok() {
            tracing::debug!(path = %entry.path().display(), "removed stale partial download");
        }
    }
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
        let err = Model::from_name("smallish").unwrap_err();
        assert!(err.to_string().contains("small.en"), "{err}");
        // #77: the multilingual files are known too, and only the `.en` ones are English-only.
        let multi = Model::from_name("small").unwrap();
        assert_eq!(multi.file_name(), "ggml-small.bin");
        assert!(!multi.is_english_only() && m.is_english_only());
        assert!(!Model::LargeV3Turbo.is_english_only());
        // The multilingual tiny..medium and large-v3 translate; turbo was trained without it.
        assert!(multi.translates() && Model::Medium.translates());
        assert!(!Model::LargeV3Turbo.translates() && !m.translates());
        assert_eq!(Model::ALL.len(), 10);
        assert!(Model::from_name("large-v3").unwrap().translates());
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

    /// Headers, a few bytes, then silence: the deadline must end it, not hang.
    #[test]
    fn a_stalled_server_fails_instead_of_hanging() {
        let dir = tempfile::tempdir().unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let stop_thread = stop.clone();
        std::thread::spawn(move || {
            if let Ok((mut s, _)) = listener.accept() {
                let head = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n",
                    Model::TinyEn.size_bytes()
                );
                let _ = s.write_all(head.as_bytes());
                let _ = s.write_all(b"partial");
                let _ = s.flush();
                while !stop_thread.load(std::sync::atomic::Ordering::Relaxed) {
                    std::thread::sleep(std::time::Duration::from_millis(20));
                }
            }
        });
        let started = std::time::Instant::now();
        let err = download_model_with(
            dir.path(),
            Model::TinyEn,
            &format!("http://{addr}"),
            Duration::from_millis(300),
            |_, _| {},
        )
        .unwrap_err();
        let took = started.elapsed();
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        assert!(took < Duration::from_secs(5), "took {took:?}");
        assert!(matches!(err, SttError::Download { .. }), "{err}");
        assert!(!model_path(dir.path(), Model::TinyEn).exists());
    }

    #[test]
    fn deadline_scales_with_size_and_has_a_floor() {
        // Every model gets at least the floor, and bigger models get proportionally longer.
        for m in Model::ALL {
            assert!(deadline_for(m) >= MIN_DEADLINE, "{}", m.name());
        }
        let big = deadline_for(Model::LargeV3Turbo);
        assert!(big > deadline_for(Model::TinyEn));
        // ~2.2 h for the 1.6 GB model at 200 KB/s: finite, and generous for a slow link.
        assert!(big < Duration::from_secs(3 * 3600), "{big:?}");
    }

    /// Every `.part.<pid>` left in `dir`.
    fn partials(dir: &Path) -> Vec<String> {
        std::fs::read_dir(dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.contains(".part."))
            .collect()
    }

    /// A connection that dies mid-body: the guard's `Drop` must take the scratch file with it.
    #[test]
    fn a_failed_read_leaves_no_partial_file() {
        let dir = tempfile::tempdir().unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            if let Ok((mut s, _)) = listener.accept() {
                let head = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n",
                    Model::TinyEn.size_bytes()
                );
                let _ = s.write_all(head.as_bytes());
                let _ = s.write_all(&vec![0u8; 8192]);
                let _ = s.flush();
                // Close with the body unfinished: the client sees an incomplete-message error.
            }
        });
        let err = download_model(
            dir.path(),
            Model::TinyEn,
            &format!("http://{addr}"),
            |_, _| {},
        )
        .unwrap_err();
        assert!(
            matches!(
                err,
                SttError::Download { .. } | SttError::SizeMismatch { .. }
            ),
            "{err}"
        );
        assert!(
            partials(dir.path()).is_empty(),
            "{:?}",
            partials(dir.path())
        );
        assert!(!model_path(dir.path(), Model::TinyEn).exists());
    }

    #[test]
    fn a_size_mismatch_leaves_no_partial_file() {
        let dir = tempfile::tempdir().unwrap();
        let base = serve_once("200 OK", vec![7u8; 4096]);
        let err = download_model(dir.path(), Model::TinyEn, &base, |_, _| {}).unwrap_err();
        assert!(matches!(err, SttError::SizeMismatch { .. }), "{err}");
        assert!(
            partials(dir.path()).is_empty(),
            "{:?}",
            partials(dir.path())
        );
    }

    /// A file orphaned by a killed process (no `Drop` ran) is reclaimed by the next download.
    #[test]
    fn stale_partials_are_swept_before_downloading() {
        let dir = tempfile::tempdir().unwrap();
        let orphan = dir.path().join("ggml-tiny.en.bin.part.999999");
        let other_model = dir.path().join("ggml-base.en.bin.part.999999");
        let unrelated = dir.path().join("notes.txt");
        for p in [&orphan, &other_model, &unrelated] {
            std::fs::write(p, b"leftover").unwrap();
        }

        // Age threshold of zero: everything already on disk counts as stale.
        sweep_stale_partials_older_than(dir.path(), Model::TinyEn, Duration::ZERO);
        assert!(!orphan.exists(), "the model's own orphan is removed");
        assert!(
            other_model.exists(),
            "another model's partial is left alone"
        );
        assert!(unrelated.exists(), "unrelated files are left alone");

        // With the real threshold a fresh partial survives, so a concurrent download is safe.
        std::fs::write(&orphan, b"fresh").unwrap();
        sweep_stale_partials(dir.path(), Model::TinyEn);
        assert!(orphan.exists(), "a partial younger than an hour is kept");
    }

    #[test]
    fn partial_file_is_per_process() {
        let dir = tempfile::tempdir().unwrap();
        let base = serve_once("200 OK", vec![7u8; 4096]);
        let _ = download_model(dir.path(), Model::TinyEn, &base, |_, _| {});
        // The scratch name carries the pid, so a second process cannot share it.
        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| Some(e.ok()?.file_name().to_string_lossy().into_owned()))
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
        assert!(
            model_path(dir.path(), Model::TinyEn)
                .with_extension(format!("bin.part.{}", std::process::id()))
                .file_name()
                .is_some()
        );
    }

    #[test]
    fn http_error_is_a_typed_error() {
        let dir = tempfile::tempdir().unwrap();
        let base = serve_once("404 Not Found", b"nope".to_vec());
        let err = download_model(dir.path(), Model::BaseEn, &base, |_, _| {}).unwrap_err();
        assert!(err.to_string().contains("404"), "{err}");
    }

    /// The SHA-256 of `bytes`, as [`file_sha256`] writes it.
    fn sha_of(bytes: &[u8]) -> String {
        hex(&Sha256::digest(bytes))
    }

    const GOOD: &[u8] = b"a whisper model, as published";

    #[test]
    fn a_present_file_that_verifies_is_not_downloaded_again() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("m.bin");
        std::fs::write(&dest, GOOD).unwrap();
        // Unroutable URL: any network attempt would fail the test.
        let got = download_file(
            &dest,
            "http://127.0.0.1:9/m.bin",
            GOOD.len() as u64,
            Some(&sha_of(GOOD)),
            MIN_DEADLINE,
            |_, _| panic!("no download expected"),
        )
        .unwrap();
        assert_eq!(got, dest);
    }

    /// #84: the dev box's `medium` was the right size with a few flipped bits. Right size is no
    /// longer enough to be left alone: `--fetch` downloads it again and the good file replaces it.
    #[test]
    fn a_present_file_of_the_right_size_and_wrong_bytes_is_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("m.bin");
        let mut damaged = GOOD.to_vec();
        damaged[3] ^= 0x20;
        std::fs::write(&dest, &damaged).unwrap();
        let base = serve_once("200 OK", GOOD.to_vec());
        download_file(
            &dest,
            &format!("{base}/m.bin"),
            GOOD.len() as u64,
            Some(&sha_of(GOOD)),
            MIN_DEADLINE,
            |_, _| {},
        )
        .unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), GOOD);
    }

    /// #84: a download of the right length whose bytes are wrong is refused and leaves nothing:
    /// no file in place and no partial.
    #[test]
    fn a_download_with_the_wrong_checksum_is_refused_and_leaves_no_file() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("m.bin");
        let base = serve_once("200 OK", b"a whisper model, as publishes".to_vec());
        let err = download_file(
            &dest,
            &format!("{base}/m.bin"),
            GOOD.len() as u64,
            Some(&sha_of(GOOD)),
            MIN_DEADLINE,
            |_, _| {},
        )
        .unwrap_err();
        assert!(matches!(err, SttError::ChecksumMismatch { .. }), "{err}");
        assert!(err.to_string().contains("dmn doctor --fetch"), "{err}");
        let left: Vec<_> = std::fs::read_dir(dir.path()).unwrap().collect();
        assert!(left.is_empty(), "{left:?}");
    }

    #[test]
    fn verify_file_checks_the_size_then_the_hash() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("m.bin");
        std::fs::write(&file, GOOD).unwrap();
        let (len, sha) = (GOOD.len() as u64, sha_of(GOOD));
        verify_file(&file, len, &sha).unwrap();
        assert!(matches!(
            verify_file(&file, len + 1, &sha),
            Err(SttError::SizeMismatch { .. })
        ));
        let err = verify_file(&file, len, &sha_of(b"other")).unwrap_err();
        assert!(
            matches!(&err, SttError::ChecksumMismatch { got, .. } if *got == sha),
            "{err}"
        );
        assert!(matches!(
            verify_file(&dir.path().join("absent"), len, &sha),
            Err(SttError::Io { .. })
        ));
    }

    /// The module doc's table (what anyone checks by hand) and [`Model::size_bytes`] /
    /// [`Model::sha256`] (what the code checks) must never drift apart.
    #[test]
    fn the_doc_table_and_the_code_agree_for_every_model() {
        let rows: Vec<(String, u64, String)> = include_str!("model.rs")
            .lines()
            .filter_map(|l| l.strip_prefix("//! | ggml-"))
            .map(|row| {
                let cells: Vec<&str> = row.split('|').map(str::trim).collect();
                (
                    format!("ggml-{}", cells[0]),
                    cells[1].parse().unwrap(),
                    cells[2].to_owned(),
                )
            })
            .collect();
        assert_eq!(rows.len(), Model::ALL.len(), "{rows:?}");
        for m in Model::ALL {
            let row = rows
                .iter()
                .find(|(file, ..)| *file == m.file_name())
                .unwrap_or_else(|| panic!("{} not in the doc table", m.file_name()));
            assert_eq!(row.1, m.size_bytes(), "{}", m.name());
            assert_eq!(row.2, m.sha256(), "{}", m.name());
            assert_eq!(m.sha256().len(), 64, "{}", m.name());
        }
    }
}
