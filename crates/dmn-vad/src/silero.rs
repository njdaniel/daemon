//! Silero VAD (#62): a small recurrent ONNX model run through ONNX Runtime (`ort`), behind the
//! same [`Vad`] trait as [`EnergyVad`](crate::EnergyVad).
//!
//! Silero reads 512-sample windows at 16 kHz (32 ms) with the previous window's last 64 samples
//! in front as context, and carries a `[2, 1, 128]` recurrent state from window to window. Our
//! frames are 320 samples (20 ms), so frames are buffered and a window runs whenever 512 samples
//! are waiting; each frame's decision is the latest window's, with the upstream wrapper's
//! hysteresis (speech from `threshold`, silence below `neg_threshold`).
//!
//! The model is not in git. `dmn doctor --fetch silero` downloads [`MODEL_URL`] into the models
//! directory, and [`verify_model`] checks it by size and SHA-256 before ONNX Runtime parses it.

use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use dmn_audio::FRAME_LEN;
use ort::session::Session;
use ort::session::builder::GraphOptimizationLevel;
use ort::value::{Tensor, TensorRef};
use sha2::{Digest, Sha256};

use crate::Vad;

/// The name `dmn doctor --fetch` takes.
pub const FETCH_NAME: &str = "silero";
/// File name under the models directory.
pub const MODEL_FILE: &str = "silero_vad.onnx";
/// Pinned to a release tag, so the bytes behind the URL cannot change.
pub const MODEL_URL: &str = "https://raw.githubusercontent.com/snakers4/silero-vad/v6.2.1/src/silero_vad/data/silero_vad.onnx";
/// Exact size of the file at [`MODEL_URL`] (read 2026-09-25).
pub const MODEL_SIZE: u64 = 2_327_524;
/// SHA-256 of the file at [`MODEL_URL`] (`sha256sum`, 2026-09-25).
pub const MODEL_SHA256: &str = "1a153a22f4509e292a94e67d6f9b85e8deb25b4988682b7e174c65279d8788e3";

/// Samples per inference window at 16 kHz.
const WINDOW: usize = 512;
/// Samples of the previous window prepended to each one (the upstream `OnnxWrapper` does this).
const CONTEXT: usize = 64;
/// `[2, 1, 128]` recurrent state.
const STATE_LEN: usize = 2 * 128;
const SAMPLE_RATE: i64 = 16_000;

/// `<models_dir>/silero_vad.onnx`.
pub fn model_path(models_dir: &Path) -> PathBuf {
    models_dir.join(MODEL_FILE)
}

#[derive(Debug, thiserror::Error)]
pub enum SileroError {
    #[error("Silero VAD model {path} is missing; fix: dmn doctor --fetch silero")]
    Missing { path: String },
    #[error("cannot read Silero VAD model {path}: {source}")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error(
        "{path} is not the expected Silero VAD model ({size} bytes, sha256 {sha256}); \
         fix: rm {path} && dmn doctor --fetch silero"
    )]
    Mismatch {
        path: String,
        size: u64,
        sha256: String,
    },
    #[error("ONNX Runtime cannot run the Silero VAD model {path}: {reason}")]
    Runtime { path: String, reason: String },
}

/// Size, then SHA-256: the file is exactly the pinned release. Cheap (2.3 MB), so both
/// `dmn doctor` and every load run it.
pub fn verify_model(path: &Path) -> Result<(), SileroError> {
    let shown = || path.display().to_string();
    let meta = match std::fs::metadata(path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(SileroError::Missing { path: shown() });
        }
        Err(source) => {
            return Err(SileroError::Io {
                path: shown(),
                source,
            });
        }
    };
    let sha256 = sha256_file(path).map_err(|source| SileroError::Io {
        path: shown(),
        source,
    })?;
    if meta.len() != MODEL_SIZE || sha256 != MODEL_SHA256 {
        return Err(SileroError::Mismatch {
            path: shown(),
            size: meta.len(),
            sha256,
        });
    }
    Ok(())
}

fn sha256_file(path: &Path) -> std::io::Result<String> {
    let mut f = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 1 << 16];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect())
}

/// Hysteresis on the speech probability. Upstream's defaults are 0.5 / 0.35; #62 measured the
/// three fixtures (turns within 200 ms of the voiced bounds, out of 92) and chose 0.3 / 0.2:
///
/// | | 0.5 / 0.35 | 0.3 / 0.2 | energy VAD |
/// | --- | --- | --- | --- |
/// | clean TTS | 88 | 91 | 92 |
/// | + keyboard clicks | 77 | 80 | 1 |
/// | + fan hiss | 85 | 88 | 88 |
///
/// No threshold removes Silero's lag against the energy VAD (median onset about +55 ms, end
/// about +85 ms, against -4 / +33 ms); #63 weighs that against its noise robustness.
#[derive(Debug, Clone, PartialEq)]
pub struct SileroVadConfig {
    /// Probability at or above which a window starts (or continues) speech.
    pub threshold: f32,
    /// Probability below which speech ends; between the two the previous decision holds.
    pub neg_threshold: f32,
}

impl Default for SileroVadConfig {
    fn default() -> Self {
        Self {
            threshold: 0.3,
            neg_threshold: 0.2,
        }
    }
}

/// Inference timing, for the ≤ 2 ms per-frame budget (#62) and the logs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct InferenceStats {
    /// Windows run.
    pub windows: u64,
    /// Windows whose inference failed; they count as non-speech.
    pub errors: u64,
    pub total: Duration,
    pub slowest: Duration,
}

impl InferenceStats {
    pub fn mean(&self) -> Duration {
        if self.windows == 0 {
            return Duration::ZERO;
        }
        self.total / u32::try_from(self.windows).unwrap_or(u32::MAX)
    }
}

pub struct SileroVad {
    session: Session,
    path: String,
    cfg: SileroVadConfig,
    /// Samples not yet in a window (< [`WINDOW`] between calls).
    pending: Vec<f32>,
    /// `[context | window]`, [`CONTEXT`] + [`WINDOW`] samples.
    input: Vec<f32>,
    state: Vec<f32>,
    sr: Tensor<i64>,
    probability: f32,
    speaking: bool,
    stats: InferenceStats,
}

impl std::fmt::Debug for SileroVad {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SileroVad")
            .field("path", &self.path)
            .field("cfg", &self.cfg)
            .field("speaking", &self.speaking)
            .field("stats", &self.stats)
            .finish_non_exhaustive()
    }
}

impl SileroVad {
    /// Check the model file, load it into ONNX Runtime and run one silent window, so a file that
    /// loads but cannot run fails here rather than on the first word of a call.
    pub fn load(path: &Path, cfg: SileroVadConfig) -> Result<Self, SileroError> {
        verify_model(path)?;
        let shown = path.display().to_string();
        let runtime = |e: ort::Error| SileroError::Runtime {
            path: shown.clone(),
            reason: e.to_string(),
        };
        let unusable = |reason: String| SileroError::Runtime {
            path: shown.clone(),
            reason,
        };
        // One thread each, like the upstream wrapper: a window is tiny, and whisper already
        // uses every core it can get.
        let session = Session::builder()
            .and_then(|b| b.with_optimization_level(GraphOptimizationLevel::Level3))
            .and_then(|b| b.with_intra_threads(1))
            .and_then(|b| b.with_inter_threads(1))
            .and_then(|b| b.commit_from_file(path))
            .map_err(runtime)?;
        let sr = Tensor::from_array(((), vec![SAMPLE_RATE])).map_err(runtime)?;
        let mut vad = Self {
            session,
            path: shown.clone(),
            cfg,
            pending: Vec::with_capacity(WINDOW + FRAME_LEN),
            input: vec![0.0; CONTEXT + WINDOW],
            state: vec![0.0; STATE_LEN],
            sr,
            probability: 0.0,
            speaking: false,
            stats: InferenceStats::default(),
        };
        vad.infer().map_err(unusable)?;
        vad.reset();
        Ok(vad)
    }

    /// Speech probability of the latest window.
    pub fn probability(&self) -> f32 {
        self.probability
    }

    pub fn stats(&self) -> InferenceStats {
        self.stats
    }

    /// Run the model on `input`, update `state`, return the speech probability. Every failure is
    /// an `Err`: a missing output is looked up with `get`, never indexed, so a model with other
    /// output names fails `load` with a message instead of panicking the VAD thread.
    fn infer(&mut self) -> Result<f32, String> {
        let e = |e: ort::Error| e.to_string();
        let input =
            TensorRef::from_array_view(([1usize, CONTEXT + WINDOW], &self.input[..])).map_err(e)?;
        let state = TensorRef::from_array_view(([2usize, 1, 128], &self.state[..])).map_err(e)?;
        let outputs = self
            .session
            .run(ort::inputs![
                "input" => input,
                "state" => state,
                "sr" => &self.sr,
            ])
            .map_err(e)?;
        let tensor = |name: &str| {
            outputs
                .get(name)
                .ok_or_else(|| format!("the model has no `{name}` output"))?
                .try_extract_tensor::<f32>()
                .map(|(_, data)| data)
                .map_err(e)
        };
        let p = tensor("output")?.first().copied().unwrap_or(0.0);
        let next = tensor("stateN")?;
        if next.len() != STATE_LEN {
            return Err(format!(
                "`stateN` has {} values, expected {STATE_LEN}",
                next.len()
            ));
        }
        self.state.copy_from_slice(next);
        Ok(p)
    }

    /// Run one window from the front of `pending`.
    fn run_window(&mut self) {
        self.input[CONTEXT..].copy_from_slice(&self.pending[..WINDOW]);
        let started = Instant::now();
        let p = match self.infer() {
            Ok(p) => p,
            Err(e) => {
                // A model that passed `load` does not fail on a well-formed window; if it does,
                // count it, log the first few, and treat the window as silence rather than
                // stopping the VAD thread.
                self.stats.errors += 1;
                if self.stats.errors <= 3 {
                    tracing::warn!(path = %self.path, error = %e, "Silero VAD inference failed");
                }
                0.0
            }
        };
        let took = started.elapsed();
        self.stats.windows += 1;
        self.stats.total += took;
        self.stats.slowest = self.stats.slowest.max(took);
        self.probability = p;
        // The next window's context is the last CONTEXT samples of this one.
        self.input.copy_within(WINDOW.., 0);
        self.pending.drain(..WINDOW);
        if p >= self.cfg.threshold {
            self.speaking = true;
        } else if p < self.cfg.neg_threshold {
            self.speaking = false;
        }
    }
}

impl Vad for SileroVad {
    /// Every frame gets the latest window's decision, so speech that runs to the end of the
    /// stream stays speech to its last frame. What is never scored at end of stream is the < 512
    /// samples (32 ms) waiting for a window: speech that exists only there is below the
    /// segmenter's 100 ms `min_speech` and would be dropped as a click whatever the VAD said, and
    /// the segmenter cannot re-classify frames it already holds, so padding and scoring that tail
    /// at flush would change no utterance.
    fn is_speech(&mut self, frame: &[f32; FRAME_LEN]) -> bool {
        self.pending.extend_from_slice(frame);
        while self.pending.len() >= WINDOW {
            self.run_window();
        }
        self.speaking
    }

    fn reset(&mut self) {
        self.pending.clear();
        self.input.iter_mut().for_each(|x| *x = 0.0);
        self.state.iter_mut().for_each(|x| *x = 0.0);
        self.probability = 0.0;
        self.speaking = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_model_names_the_fetch_command() {
        let dir = std::env::temp_dir().join(format!("dmn-silero-missing-{}", std::process::id()));
        let err = verify_model(&model_path(&dir)).unwrap_err();
        assert!(matches!(err, SileroError::Missing { .. }), "{err:?}");
        assert!(
            err.to_string().contains("dmn doctor --fetch silero"),
            "{err}"
        );
        let err = SileroVad::load(&model_path(&dir), SileroVadConfig::default()).unwrap_err();
        assert!(matches!(err, SileroError::Missing { .. }), "{err:?}");
    }

    #[test]
    fn a_wrong_file_is_refused_before_onnx_runtime_parses_it() {
        let dir = std::env::temp_dir().join(format!("dmn-silero-wrong-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = model_path(&dir);
        std::fs::write(&path, b"<html>rate limited</html>").unwrap();
        let err = SileroVad::load(&path, SileroVadConfig::default()).unwrap_err();
        std::fs::remove_dir_all(&dir).ok();
        assert!(
            matches!(err, SileroError::Mismatch { size: 25, .. }),
            "{err:?}"
        );
        assert!(
            err.to_string().contains("dmn doctor --fetch silero"),
            "{err}"
        );
    }

    #[test]
    fn sha256_matches_a_known_vector() {
        let dir = std::env::temp_dir().join(format!("dmn-silero-sha-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("abc");
        std::fs::write(&path, b"abc").unwrap();
        let got = sha256_file(&path).unwrap();
        std::fs::remove_dir_all(&dir).ok();
        assert_eq!(
            got,
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }
}
