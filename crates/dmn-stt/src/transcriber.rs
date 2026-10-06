//! One whisper model + one inference state. SPEC § STT worker fixes the decode parameters,
//! including `no_context=true` (#39): see the comment at `set_no_context`.

use std::ffi::c_int;
use std::path::Path;

use whisper_rs::{
    FullParams, SamplingStrategy, WhisperContext, WhisperContextParameters, WhisperState,
};

use crate::gpu::{DeviceChoice, Placement, gpu_devices, resolve};
use crate::{Model, SAMPLE_RATE, SttError};

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

/// What language the audio is in and what whisper emits (`[stt].language`, #77).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Language {
    /// English audio, English text: `language = "en"`, no translation. The M1 path, unchanged,
    /// and the only one an English-only (`.en`) model can run.
    #[default]
    English,
    /// Detect the language of each utterance and translate its text to English. Detection runs
    /// on the utterance's first inference and the worker pins the result for its later partials
    /// and final, so it is paid once per utterance, not once per partial.
    Auto,
    /// The audio is in this language (an ISO 639-1 code whisper knows, `es`, `de`, ...);
    /// translate its text to English with no detection.
    Fixed(String),
    /// Detect the language of each utterance, as [`Language::Auto`] does, and keep its text in
    /// that language: whisper's transcribe task, no translation. #109: the You lane while
    /// `[speak]` is on, so what `t` translates and speaks is the user's own words, not
    /// whisper's English rendering of them. Needs a multilingual model, like `Auto`, but not one
    /// that translates (`large-v3-turbo` will do).
    Detect,
}

impl Language {
    /// `en`, `auto`, or a language code whisper knows.
    pub fn parse(s: &str) -> Result<Self, SttError> {
        let s = s.trim();
        match s {
            "en" | "english" => Ok(Language::English),
            "auto" => Ok(Language::Auto),
            code if whisper_rs::get_lang_id(code).is_some() => Ok(Language::Fixed(code.to_owned())),
            other => Err(SttError::UnknownLanguage(other.to_owned())),
        }
    }

    /// The English name whisper gives a language code (`es` → `spanish`), or `None` for a code
    /// it does not know. #109 names `[speak].target` this way to the model that translates.
    pub fn name_of(code: &str) -> Option<&'static str> {
        whisper_rs::get_lang_id(code).and_then(whisper_rs::get_lang_str_full)
    }

    /// The config spelling: `en`, `auto` or the code; `detect` for [`Language::Detect`], which
    /// no config names (the pipeline picks it for one lane).
    pub fn label(&self) -> &str {
        match self {
            Language::English => "en",
            Language::Auto => "auto",
            Language::Fixed(code) => code,
            Language::Detect => "detect",
        }
    }

    /// Whether whisper runs its translate task, which needs a multilingual model.
    pub fn translates(&self) -> bool {
        matches!(self, Language::Auto | Language::Fixed(_))
    }

    /// Whether whisper hears anything but English, which needs a multilingual model.
    pub fn multilingual(&self) -> bool {
        !matches!(self, Language::English)
    }

    /// The language whisper is told and whether it translates, for one inference: `pinned` is
    /// what this utterance's first inference detected (`None`: let whisper detect).
    pub fn task<'a>(&'a self, pinned: Option<&'a str>) -> (Option<&'a str>, bool) {
        match self {
            Language::English => (Some("en"), false),
            // A `None` language makes whisper.cpp detect it from the encoder output before
            // decoding; the translate task then decodes English whatever it heard.
            Language::Auto => (pinned, true),
            Language::Fixed(code) => (Some(code), true),
            // The same detection; the transcribe task then decodes in the language it heard.
            Language::Detect => (pinned, false),
        }
    }

    /// An English-only model cannot detect or translate, and `large-v3-turbo` cannot
    /// translate ([`Model::translates`]).
    pub fn check(&self, model: Model) -> Result<(), SttError> {
        if *self == Language::Detect && model.is_english_only() {
            return Err(SttError::ModelIsEnglishOnly {
                model: model.name().to_owned(),
            });
        }
        if !self.translates() || model.translates() {
            return Ok(());
        }
        let model_name = model.name().to_owned();
        Err(if model.is_english_only() {
            SttError::ModelIsEnglishOnly { model: model_name }
        } else {
            SttError::ModelCannotTranslate { model: model_name }
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SttConfig {
    /// Decoder threads (`[stt].threads`).
    pub threads: usize,
    /// The audio's language and whether to translate (#77).
    pub language: Language,
    /// Where to run (`[stt].device`, #61). A GPU asked for and unavailable, or failing to load,
    /// falls back to the CPU with a warning; [`Transcriber::placement`] says where it ended up.
    pub device: DeviceChoice,
    /// Shrink the encoder window to the utterance instead of whisper's fixed 30 s. The encoder
    /// then costs time in proportion to the audio, which is what makes a 6 s question fit the
    /// 2 s end-to-end budget on CPU. See [`audio_ctx_for`].
    pub trim_audio_ctx: bool,
}

impl Default for SttConfig {
    fn default() -> Self {
        Self {
            threads: 4,
            language: Language::English,
            device: DeviceChoice::Auto,
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
    /// The language whisper detected or was told (ISO 639-1), in any mode but
    /// [`Language::English`], where it is `None`.
    pub language: Option<String>,
}

pub struct Transcriber {
    ctx: WhisperContext,
    state: WhisperState,
    cfg: SttConfig,
    placement: Placement,
}

impl Transcriber {
    /// Load a ggml model on the device `cfg.device` resolves to (see [`crate::gpu`]). A GPU that
    /// is missing, too full for the model ([`crate::gpu::fits`], checked first because ggml
    /// aborts the process on a failed GPU allocation), or failing to load it falls back to the
    /// CPU with a warning rather than failing the run. whisper.cpp's own logging is routed into
    /// `tracing` so it never writes to the terminal the TUI owns.
    pub fn load(model_file: &Path, cfg: SttConfig) -> Result<Self, SttError> {
        let shown = model_file.display().to_string();
        if !model_file.is_file() {
            return Err(SttError::ModelMissing(shown));
        }
        // The callers check `Language::check` against the registry entry with a better message;
        // this is the backstop for a file handed in by path: the `.en` files cannot translate.
        if cfg.language.multilingual() {
            let name = model_file
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("");
            if name.ends_with(".en.bin") {
                return Err(SttError::ModelIsEnglishOnly { model: shown });
            }
            if cfg.language.translates() && name.contains("turbo") {
                return Err(SttError::ModelCannotTranslate { model: shown });
            }
        }
        whisper_rs::install_logging_hooks();
        let devices = match cfg.device {
            DeviceChoice::Cpu => Vec::new(),
            DeviceChoice::Gpu | DeviceChoice::Auto => gpu_devices(),
        };
        let model_bytes = std::fs::metadata(model_file).map_or(0, |m| m.len());
        let wanted = match resolve(cfg.device, Backend::compiled(), &devices) {
            // Reported free memory first, then a real allocation of what the load needs: a
            // failed allocation returns here, where the load itself would abort the process.
            Placement::Gpu(device) => match crate::gpu::fits(&device, model_bytes) {
                Err(why) => Placement::Cpu { reason: Some(why) },
                Ok(())
                    if !crate::gpu::probe(
                        device.ordinal,
                        model_bytes + crate::gpu::state_bytes(model_bytes),
                    ) =>
                {
                    Placement::Cpu {
                        reason: Some(format!(
                            "{} could not allocate whisper's memory",
                            device.name
                        )),
                    }
                }
                Ok(()) => Placement::Gpu(device),
            },
            cpu => cpu,
        };
        let ((ctx, state), placement) =
            with_fallback(wanted, |gpu| load_on(model_file, &shown, gpu))?;
        // `auto` means "the GPU if it has room": whatever kept it off, it falls back quietly.
        if cfg.device == DeviceChoice::Auto
            && let Placement::Cpu { reason: Some(why) } = &placement
        {
            tracing::info!(model = %shown, reason = %why, "whisper on the CPU (auto)");
        }
        let placement = placement.for_choice(cfg.device);
        match &placement {
            Placement::Cpu {
                reason: Some(why), ..
            } => {
                tracing::warn!(model = %shown, reason = %why, "whisper on the CPU: GPU unavailable");
            }
            p => tracing::info!(model = %shown, device = %p.label(), "whisper loaded"),
        }
        Ok(Self {
            ctx,
            state,
            cfg,
            placement,
        })
    }

    pub fn config(&self) -> &SttConfig {
        &self.cfg
    }

    /// Where this transcriber runs.
    pub fn placement(&self) -> &Placement {
        &self.placement
    }

    /// Transcribe one utterance (16 kHz mono f32). `prompt` is the previous utterance's text.
    /// `pinned` is the language already detected earlier in this same utterance under
    /// [`Language::Auto`] or [`Language::Detect`] (the worker passes it back so detection runs
    /// once per utterance);
    /// `None` lets whisper detect. It is ignored in the other modes.
    pub fn transcribe(
        &mut self,
        samples: &[f32],
        prompt: Option<&str>,
        pinned: Option<&str>,
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
        let (language, translate) = self.cfg.language.task(pinned);
        params.set_language(language);
        if translate {
            params.set_translate(true);
        }
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
        let language = match self.cfg.language {
            Language::English => None,
            _ => whisper_rs::get_lang_str(self.state.full_lang_id_from_state()).map(str::to_owned),
        };

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
        let mut t = transcript_from(&text, sum, n);
        t.language = language;
        Ok(t)
    }
}

/// One model context and its inference state, on the GPU with ordinal `gpu` or on the CPU.
fn load_on(
    model_file: &Path,
    shown: &str,
    gpu: Option<i32>,
) -> Result<(WhisperContext, WhisperState), SttError> {
    let mut params = WhisperContextParameters::default();
    params.use_gpu(gpu.is_some());
    params.gpu_device(gpu.unwrap_or(0));
    let ctx =
        WhisperContext::new_with_params(model_file, params).map_err(|source| SttError::Load {
            path: shown.to_owned(),
            source,
        })?;
    let state = ctx.create_state().map_err(|source| SttError::Load {
        path: shown.to_owned(),
        source,
    })?;
    Ok((ctx, state))
}

/// Try `wanted`; if it is a GPU and `load` fails there, load on the CPU instead and record why.
/// A CPU load that fails is the caller's error. Split out so the fallback is tested without a GPU.
fn with_fallback<T>(
    wanted: Placement,
    mut load: impl FnMut(Option<i32>) -> Result<T, SttError>,
) -> Result<(T, Placement), SttError> {
    match wanted {
        Placement::Gpu(device) => match load(Some(device.ordinal)) {
            Ok(t) => Ok((t, Placement::Gpu(device))),
            Err(e) => {
                let reason = format!("{} failed: {e}", device.name);
                let t = load(None)?;
                Ok((
                    t,
                    Placement::Cpu {
                        reason: Some(reason),
                    },
                ))
            }
        },
        cpu => Ok((load(None)?, cpu)),
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
        language: None,
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
        assert!(c.trim_audio_ctx && c.threads == 4);
        // `Auto` in a CPU-only build resolves to the CPU without a warning.
        assert_eq!(
            crate::gpu::resolve(c.device, Backend::compiled(), &crate::gpu_devices()),
            Placement::Cpu { reason: None }
        );
    }

    #[test]
    #[cfg(any(feature = "cuda", feature = "vulkan"))]
    fn gpu_features_are_compiled_in() {
        assert_ne!(Backend::compiled(), Backend::Cpu);
    }

    fn gpu(ordinal: i32) -> Placement {
        Placement::Gpu(crate::GpuDevice {
            ordinal,
            name: "Vulkan0".into(),
            description: "RTX 4090".into(),
            integrated: false,
            free_bytes: 1,
            total_bytes: 1,
        })
    }

    fn load_error() -> SttError {
        SttError::Io {
            path: "gpu".into(),
            source: std::io::Error::other("out of device memory"),
        }
    }

    #[test]
    fn a_gpu_that_fails_to_load_falls_back_to_the_cpu_and_says_why() {
        let mut tried = Vec::new();
        let (got, placement) = with_fallback(gpu(0), |g| {
            tried.push(g);
            match g {
                Some(_) => Err(load_error()),
                None => Ok("cpu model"),
            }
        })
        .unwrap();
        assert_eq!(got, "cpu model");
        assert_eq!(tried, [Some(0), None], "GPU first, then the CPU");
        assert!(placement.is_fallback(), "{placement:?}");
        let label = placement.label();
        assert!(
            label.contains("Vulkan0 failed") && label.contains("out of device memory"),
            "{label}"
        );
    }

    #[test]
    fn a_working_gpu_is_kept_and_a_cpu_failure_is_an_error() {
        let (_, placement) = with_fallback(gpu(1), Ok::<_, SttError>).unwrap();
        assert!(placement.is_gpu());
        let err = with_fallback(Placement::Cpu { reason: None }, |_| {
            Err::<(), _>(load_error())
        });
        assert!(err.is_err(), "no device left to fall back to");
    }

    /// #109: what each mode tells whisper. `Detect` detects as `Auto` does (and keeps what the
    /// utterance's first inference pinned) but never runs the translate task, so the You lane
    /// keeps the user's own words while `[speak]` is on.
    #[test]
    fn each_mode_sets_the_language_and_the_translate_task() {
        assert_eq!(Language::English.task(Some("es")), (Some("en"), false));
        assert_eq!(Language::Auto.task(None), (None, true));
        assert_eq!(Language::Auto.task(Some("es")), (Some("es"), true));
        let fixed = Language::Fixed("de".into());
        assert_eq!(fixed.task(Some("es")), (Some("de"), true));
        assert_eq!(Language::Detect.task(None), (None, false));
        assert_eq!(Language::Detect.task(Some("en")), (Some("en"), false));
        assert!(!Language::Detect.translates() && Language::Detect.multilingual());
        assert_eq!(Language::Detect.label(), "detect");
        assert_eq!(Language::name_of("es"), Some("spanish"));
        assert_eq!(Language::name_of("xx"), None);
    }

    #[test]
    fn detect_needs_a_multilingual_model_but_not_one_that_translates() {
        assert!(Language::Detect.check(Model::Small).is_ok());
        assert!(Language::Detect.check(Model::LargeV3Turbo).is_ok());
        assert!(matches!(
            Language::Detect.check(Model::SmallEn),
            Err(SttError::ModelIsEnglishOnly { .. })
        ));
        assert!(matches!(
            Language::Auto.check(Model::LargeV3Turbo),
            Err(SttError::ModelCannotTranslate { .. })
        ));
    }

    #[test]
    fn detect_refuses_an_english_only_file_handed_in_by_path() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ggml-small.en.bin");
        std::fs::write(&path, b"not a model").unwrap();
        let cfg = SttConfig {
            language: Language::Detect,
            ..SttConfig::default()
        };
        let err = Transcriber::load(&path, cfg).err().unwrap();
        assert!(matches!(err, SttError::ModelIsEnglishOnly { .. }), "{err}");
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
