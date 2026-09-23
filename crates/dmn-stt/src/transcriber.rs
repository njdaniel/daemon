//! One whisper model + one inference state. SPEC § STT worker fixes the decode parameters,
//! including `no_context=true` (#39): see the comment at `set_no_context`.

use std::ffi::c_int;
use std::path::Path;

use whisper_rs::{
    FullParams, SamplingStrategy, WhisperContext, WhisperContextParameters, WhisperState,
};

use crate::{SAMPLE_RATE, SttError};

/// Segments below this mean token log-probability are flagged; SPEC drops them in #9.
pub const LOW_CONFIDENCE_LOGPROB: f32 = -1.0;

/// whisper.cpp refuses input shorter than 1 s ("input is too short"); shorter utterances are
/// zero-padded to this length.
const MIN_SAMPLES: usize = SAMPLE_RATE + SAMPLE_RATE / 100;

/// The inference backend compiled into this binary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backend {
    Cpu,
    Cuda,
    Vulkan,
}

impl Backend {
    pub fn compiled() -> Self {
        if cfg!(feature = "cuda") {
            Backend::Cuda
        } else if cfg!(feature = "vulkan") {
            Backend::Vulkan
        } else {
            Backend::Cpu
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Backend::Cpu => "CPU",
            Backend::Cuda => "CUDA",
            Backend::Vulkan => "Vulkan",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SttConfig {
    /// Decoder threads (`[stt].threads`).
    pub threads: usize,
    /// Offload to the GPU backend when one is compiled in. On the dev box the LLM holds most of
    /// the VRAM, so CPU is the expected configuration.
    pub use_gpu: bool,
    /// Shrink the encoder window to the utterance instead of whisper's fixed 30 s. The encoder
    /// then costs time in proportion to the audio, which is what makes a 6 s question fit the
    /// 2 s end-to-end budget on CPU. See [`audio_ctx_for`].
    pub trim_audio_ctx: bool,
}

impl Default for SttConfig {
    fn default() -> Self {
        Self {
            threads: 4,
            use_gpu: Backend::compiled() != Backend::Cpu,
            trim_audio_ctx: true,
        }
    }
}

/// Headroom added to the utterance when trimming the encoder window.
pub const AUDIO_CTX_PAD_S: f64 = 2.0;
/// Smallest trimmed window. whisper was trained on 30 s windows and degrades on very small ones:
/// a sweep over 24 fixture turns (2026-09-22, small.en, 8 threads) gave mean WER 8.9 % / median
/// 1.78 s for the full window, 13.7 % / 0.60 s for clip + 1 s, and 11.3 % / 0.71 s for
/// clip + 2 s with a 10 s floor, which is the default.
pub const AUDIO_CTX_MIN_S: f64 = 10.0;

/// Encoder frames for `n_samples` of 16 kHz audio: whisper's 1500 frames cover 30 s, so 50 per
/// second; the clip plus [`AUDIO_CTX_PAD_S`], at least [`AUDIO_CTX_MIN_S`], at most 30 s.
pub fn audio_ctx_for(n_samples: usize) -> c_int {
    let secs = n_samples as f64 / SAMPLE_RATE as f64;
    let frames = ((secs + AUDIO_CTX_PAD_S).max(AUDIO_CTX_MIN_S) * 50.0).ceil() as c_int;
    frames.clamp(64, 1500)
}

#[derive(Debug, Clone, PartialEq)]
pub struct Transcript {
    pub text: String,
    /// Mean of `plog` over the text tokens (special and timestamp tokens excluded). 0.0 when
    /// whisper produced no text tokens.
    pub avg_logprob: f32,
    pub low_confidence: bool,
    pub n_tokens: usize,
}

pub struct Transcriber {
    ctx: WhisperContext,
    state: WhisperState,
    cfg: SttConfig,
}

impl Transcriber {
    /// Load a ggml model. whisper.cpp's own logging is routed into `tracing` so it never writes
    /// to the terminal the TUI owns.
    pub fn load(model_file: &Path, cfg: SttConfig) -> Result<Self, SttError> {
        let shown = model_file.display().to_string();
        if !model_file.is_file() {
            return Err(SttError::ModelMissing(shown));
        }
        whisper_rs::install_logging_hooks();
        let mut params = WhisperContextParameters::default();
        params.use_gpu(cfg.use_gpu);
        let ctx = WhisperContext::new_with_params(model_file, params).map_err(|source| {
            SttError::Load {
                path: shown.clone(),
                source,
            }
        })?;
        let state = ctx.create_state().map_err(|source| SttError::Load {
            path: shown,
            source,
        })?;
        Ok(Self { ctx, state, cfg })
    }

    pub fn config(&self) -> &SttConfig {
        &self.cfg
    }

    /// Transcribe one utterance (16 kHz mono f32). `prompt` is the previous utterance's text.
    pub fn transcribe(
        &mut self,
        samples: &[f32],
        prompt: Option<&str>,
    ) -> Result<Transcript, SttError> {
        let padded;
        let audio = if samples.len() < MIN_SAMPLES {
            let mut v = samples.to_vec();
            v.resize(MIN_SAMPLES, 0.0);
            padded = v;
            &padded[..]
        } else {
            samples
        };

        // The prompt goes in as tokens rather than `set_initial_prompt`: whisper-rs 0.16 leaks
        // the CString behind `set_initial_prompt` on every call, and borrowed tokens do not.
        // whisper.cpp keeps at most n_text_ctx / 2 prompt tokens, so keep the most recent ones.
        let tokens: Vec<c_int> = match prompt.map(str::trim).filter(|p| !p.is_empty()) {
            Some(p) => {
                let mut t = self
                    .ctx
                    .tokenize(&p.replace('\0', " "), 1024)
                    .map_err(SttError::Inference)?;
                let keep = usize::try_from(self.ctx.n_text_ctx() / 2).unwrap_or(224);
                if t.len() > keep {
                    t.drain(..t.len() - keep);
                }
                t
            }
            None => Vec::new(),
        };

        let mut params = FullParams::new(SamplingStrategy::Greedy { best_of: 1 });
        params.set_n_threads(c_int::try_from(self.cfg.threads.max(1)).unwrap_or(4));
        params.set_language(Some("en"));
        // The previous utterance's text goes in explicitly as `tokens` above. With no_context
        // false whisper.cpp would also prepend its own last decode, which here is usually a
        // partial of this same utterance, and the final would then resume after it and drop
        // everything the partial had already covered.
        params.set_no_context(true);
        params.set_single_segment(true);
        params.set_temperature(0.0);
        // No temperature fallback: a retry at a higher temperature can double the latency of a
        // hard utterance, and SPEC asks for deterministic temperature 0.
        params.set_temperature_inc(0.0);
        params.set_suppress_blank(true);
        params.set_no_timestamps(true);
        params.set_print_special(false);
        params.set_print_progress(false);
        params.set_print_realtime(false);
        params.set_print_timestamps(false);
        if !tokens.is_empty() {
            params.set_tokens(&tokens);
        }
        if self.cfg.trim_audio_ctx {
            params.set_audio_ctx(audio_ctx_for(audio.len()));
        }

        self.state
            .full(params, audio)
            .map_err(SttError::Inference)?;

        let eot = self.ctx.token_eot();
        let mut text = String::new();
        let mut sum = 0.0f32;
        let mut n = 0usize;
        for seg in self.state.as_iter() {
            if let Ok(s) = seg.to_str_lossy() {
                text.push_str(&s);
            }
            for i in 0..seg.n_tokens() {
                if let Some(tok) = seg.get_token(i) {
                    let d = tok.token_data();
                    // ids at or above EOT are special tokens (SOT, language, timestamps, ...).
                    if d.id < eot {
                        sum += d.plog;
                        n += 1;
                    }
                }
            }
        }
        Ok(transcript_from(&text, sum, n))
    }
}

/// Assemble a [`Transcript`] from whisper's text and token log-probability sum. Split out so the
/// scoring rule is testable without running inference.
fn transcript_from(text: &str, logprob_sum: f32, n_tokens: usize) -> Transcript {
    let avg_logprob = if n_tokens == 0 {
        0.0
    } else {
        logprob_sum / n_tokens as f32
    };
    Transcript {
        text: text.trim().to_owned(),
        avg_logprob,
        low_confidence: n_tokens > 0 && avg_logprob < LOW_CONFIDENCE_LOGPROB,
        n_tokens,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn audio_ctx_has_a_floor_headroom_and_cap() {
        let s = |secs: f64| (secs * SAMPLE_RATE as f64) as usize;
        // Short clips get the 10 s floor: 500 frames.
        assert_eq!(audio_ctx_for(s(1.0)), 500);
        assert_eq!(audio_ctx_for(s(8.0)), 500);
        // Longer clips get clip + 2 s at 50 frames per second.
        assert_eq!(audio_ctx_for(s(12.0)), 700);
        // Never more than whisper's full window.
        assert_eq!(audio_ctx_for(s(29.0)), 1500);
        assert_eq!(audio_ctx_for(s(60.0)), 1500);
    }

    #[test]
    fn missing_model_is_a_typed_error_with_the_fix() {
        let dir = tempfile::tempdir().unwrap();
        let err = Transcriber::load(&dir.path().join("ggml-small.en.bin"), SttConfig::default())
            .err()
            .unwrap();
        assert!(matches!(err, SttError::ModelMissing(_)));
        assert!(err.to_string().contains("dmn doctor --fetch"), "{err}");
    }

    #[test]
    #[cfg(not(any(feature = "cuda", feature = "vulkan")))]
    fn cpu_is_the_default_backend_without_gpu_features() {
        assert_eq!(Backend::compiled(), Backend::Cpu);
        let c = SttConfig::default();
        assert!(!c.use_gpu && c.trim_audio_ctx && c.threads == 4);
    }

    #[test]
    #[cfg(any(feature = "cuda", feature = "vulkan"))]
    fn gpu_features_default_to_the_gpu() {
        assert_ne!(Backend::compiled(), Backend::Cpu);
        assert!(SttConfig::default().use_gpu);
    }

    #[test]
    fn low_confidence_follows_the_mean_token_logprob() {
        // Clean speech: well above the threshold.
        let good = transcript_from("  how would you design it  ", -1.2, 6);
        assert_eq!(good.text, "how would you design it");
        assert!((good.avg_logprob - -0.2).abs() < 1e-6);
        assert!(!good.low_confidence && good.n_tokens == 6);
        // Mean below -1.0: flagged.
        let bad = transcript_from("mumble", -4.5, 3);
        assert!((bad.avg_logprob - -1.5).abs() < 1e-6);
        assert!(bad.low_confidence);
        // Exactly at the threshold is not flagged; no tokens is not flagged either.
        assert!(!transcript_from("x", -2.0, 2).low_confidence);
        let empty = transcript_from("", 0.0, 0);
        assert!(!empty.low_confidence && empty.avg_logprob == 0.0);
    }
}
