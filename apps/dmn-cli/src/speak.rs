//! `dmn speak` (#107, #108): enroll a reference clip and synthesize text in the cloned voice
//! through the local TTS server (chatterbox-tts-api, `[speak].tts_url`). Without `--out` the
//! speech plays into the PipeWire virtual microphone `dmn-voice` (#108, `dmn_audio::VirtualMic`)
//! for the call to hear: the node is created before the request goes out, so a call or
//! `pw-record --target dmn-voice` can select it during the seconds the server takes, playback
//! starts at the first decoded chunk of the server's streaming route, and the node is gone when
//! the command ends, on success or error. With `--out <file>` the speech is written there
//! instead, as before. Both subcommands refuse to run while `[speak].enabled = false`, and while
//! `tts_url` is not on this machine.
//!
//! Audio on disk (CLAUDE.md: never unless `privacy.record_audio`): that rule is about the
//! call. These commands write audio only where the user asks by name, in the same command:
//! `--out <file>` writes the synthesized speech there, and `enroll --wav <clip>` copies the
//! user's own clip to `<data dir>/<name>.wav` (the file `[speak].voice_ref` names, kept so the
//! voice can be enrolled again after the server's library is reset). Nothing from the call is
//! recorded, and nothing is written that the command line did not name; playback writes
//! nothing at all.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use dmn_audio::{AudioError, PipeWireContext, PipeWireError, VMIC_NODE_NAME, VirtualMic};
use dmn_core::Paths;
use dmn_core::config::{Config, ConfigError, SpeakEngine};
use dmn_tts::{PcmStream, SpeakRequest, TtsError, TtsProvider};

/// A reference clip needs at least this much clean speech for the clone to hold up.
const MIN_CLIP_SECS: f64 = 6.0;
/// chatterbox-tts-api refuses voice files over 10 MB (`voices.py`); say so before uploading.
const MAX_CLIP_BYTES: u64 = 10 * 1024 * 1024;
/// The default voice name, and so the stored clip `voice.wav` the issue names.
const DEFAULT_NAME: &str = "voice";
const MAX_NAME: usize = 64;
/// How long to wait for PipeWire's registry (the graph rate comes from it).
const PW_SYNC_TIMEOUT: Duration = Duration::from_secs(2);
/// Margin over the queued audio's own length when waiting for the last sample to play.
pub(crate) const DRAIN_MARGIN: Duration = Duration::from_secs(2);

#[derive(Debug, thiserror::Error)]
pub enum SpeakError {
    #[error(
        "speak is off: set `enabled = true` under [speak] in {config} (README § Speak sets up the TTS server first)"
    )]
    Disabled { config: String },
    #[error(
        "[speak].tts_url = {url:?} in {config} is not on this machine; dmn sends text and your voice clip only to a TTS server on localhost (SPEC § Privacy)"
    )]
    NotLocal { url: String, config: String },
    #[error(
        "the TTS server at {url} is not answering ({detail}); start it (README § Speak) or fix `tts_url` under [speak] in {config}"
    )]
    ServerDown {
        url: String,
        detail: String,
        config: String,
    },
    #[error(
        "the TTS server has no voice {voice:?} (it would speak in its default voice instead); enroll it again: dmn speak enroll --wav {clip} --name {voice}"
    )]
    VoiceGone { voice: String, clip: String },
    #[error(
        "no voice enrolled ([speak].voice_ref is empty in {config}); run `dmn speak enroll --wav <clip>` first"
    )]
    NotEnrolled { config: String },
    #[error(
        "cannot reach PipeWire ({detail}); dmn speak plays into the virtual microphone {VMIC_NODE_NAME}, which needs a running PipeWire server, or pass --out <file.wav> to write the speech to a file instead"
    )]
    NoPipeWire { detail: String },
    #[error("playback into {VMIC_NODE_NAME} failed: {0}")]
    Playback(AudioError),
    #[error("nothing to say: the text is empty")]
    EmptyText,
    #[error("{path} is not a usable voice clip: {detail}")]
    InvalidWav { path: String, detail: String },
    #[error(
        "{path} is {secs:.1} s long; a voice clip needs at least {MIN_CLIP_SECS:.0} s of clean speech"
    )]
    ClipTooShort { path: String, secs: f64 },
    #[error(
        "{path} is {mb:.1} MB; the TTS server takes clips up to 10 MB, so trim it to 10-30 s of speech"
    )]
    ClipTooLarge { path: String, mb: f64 },
    #[error(
        "voice name {name:?} must be 1-{MAX_NAME} letters, digits, '-' or '_' (it names the stored clip and the server's voice)"
    )]
    BadName { name: String },
    #[error("cannot store the voice clip at {path}: {detail}")]
    Store { path: String, detail: String },
    #[error("cannot write {path}: {detail}")]
    WriteOut { path: String, detail: String },
    #[error("cannot start the async runtime: {0}")]
    Runtime(String),
    /// #109: the voice speaks only the language it was enrolled with, so `t` would speak the
    /// translation with the wrong accent, or not at all.
    #[error(
        "[speak].target = {target:?} in {config}, but the enrolled voice {voice:?} speaks {speaks:?}; re-enroll with --language {target}: dmn speak enroll --wav {clip} --name {voice} --language {target}"
    )]
    LanguageMismatch {
        target: String,
        speaks: String,
        voice: String,
        clip: String,
        config: String,
    },
    #[error(
        "[speak].target = {target:?} in {config} is not a language whisper knows; use a code such as \"es\", \"de\" or \"fr\""
    )]
    UnknownTarget { target: String, config: String },
    /// #115: a `[speak].languages` entry whose voice the server's library does not have.
    #[error(
        "[speak].languages in {config} names the voice {voice:?} for {code:?}, but the TTS server has no such voice; enroll it: dmn speak enroll --wav <clip> --language {code} --name {voice}"
    )]
    LanguageVoiceGone {
        code: String,
        voice: String,
        config: String,
    },
    /// #115: a `[speak].languages` entry whose voice was enrolled for another language. (No
    /// `config` field: a second five-string variant beside `LanguageMismatch` would cost the
    /// enum its niche and push `CliError` over clippy's size limit.)
    #[error(
        "[speak].languages says the voice {voice:?} speaks {code:?}, but the TTS server has it enrolled for {speaks:?}; re-enroll it: dmn speak enroll --wav {clip} --language {code} --name {voice}"
    )]
    LanguageVoiceMismatch {
        code: String,
        speaks: String,
        voice: String,
        clip: String,
    },
    /// #115: a `[speak].languages` code whisper does not know.
    #[error(
        "[speak].languages in {config} has the code {code:?}, which is not a language whisper knows; use a code such as \"es\", \"de\" or \"fr\""
    )]
    UnknownLanguage { code: String, config: String },
    /// #122: a language the engine's model does not speak (Qwen3-TTS has a fixed list of ten).
    #[error(
        "[speak].engine = {engine:?} does not speak {code:?} ([speak].target or [speak].languages); it speaks {speaks}. Drop the language, or set engine = \"chatterbox\""
    )]
    EngineLanguage {
        engine: &'static str,
        code: String,
        speaks: String,
    },
    /// #115: `enroll --language` takes the shape `[speak].target` takes, so the entry it
    /// writes parses.
    #[error(
        "--language {code:?} is not a two- or three-letter language code such as \"es\" (the code whisper and [speak].target use)"
    )]
    BadLanguage { code: String },
    #[error("cannot reach the [llm] provider for translation: {0}")]
    Llm(String),
    #[error(transparent)]
    Tts(TtsError),
    #[error(transparent)]
    Config(#[from] ConfigError),
}

/// `dmn speak enroll --wav <clip> [--name <n>] [--language <code>]`.
pub struct EnrollArgs {
    pub wav: PathBuf,
    pub name: Option<String>,
    /// The language the voice will speak; `None` leaves the server's default (English).
    pub language: Option<String>,
}

fn rt() -> Result<tokio::runtime::Runtime, SpeakError> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| SpeakError::Runtime(e.to_string()))
}

/// The gates both subcommands pass first: speak is on, and the server is on this machine.
pub(crate) fn check_gates(cfg: &Config, paths: &Paths) -> Result<(), SpeakError> {
    let config = paths.config_file.display().to_string();
    if !cfg.speak.enabled {
        return Err(SpeakError::Disabled { config });
    }
    if !cfg.speak.is_local() {
        return Err(SpeakError::NotLocal {
            url: cfg.speak.tts_url.clone(),
            config,
        });
    }
    Ok(())
}

/// A provider error, with the unreachable case naming the config line to fix: the server's
/// address as configured (not the route), and the root cause ("Connection refused (os error
/// 111)") without reqwest's wrapping text.
pub(crate) fn tts_error(e: TtsError, paths: &Paths) -> SpeakError {
    match e {
        TtsError::Unreachable { url, detail } => SpeakError::ServerDown {
            url: url.split('/').take(3).collect::<Vec<_>>().join("/"),
            detail: detail.rsplit(": ").next().unwrap_or(&detail).to_owned(),
            config: paths.config_file.display().to_string(),
        },
        other => SpeakError::Tts(other),
    }
}

/// The voice name the server knows the clip by is the stored clip's file stem, so `dmn speak`
/// finds it from `[speak].voice_ref` alone (the config keeps no separate name).
pub(crate) fn voice_name(voice_ref: &str) -> String {
    dmn_core::config::voice_ref_name(voice_ref)
}

fn check_name(name: &str) -> Result<(), SpeakError> {
    let ok = !name.is_empty()
        && name.len() <= MAX_NAME
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
    if ok {
        Ok(())
    } else {
        Err(SpeakError::BadName {
            name: name.to_owned(),
        })
    }
}

/// Check the clip is a WAV the server can clone from. Returns a note when it works but is not
/// the preferred shape: a stereo clip is accepted (the server mixes it down), as is one below
/// 16 kHz, but a mono 16 kHz+ recording clones best.
fn validate_clip(path: &Path) -> Result<Option<String>, SpeakError> {
    let shown = path.display().to_string();
    let invalid = |detail: String| SpeakError::InvalidWav {
        path: shown.clone(),
        detail,
    };
    let size = std::fs::metadata(path)
        .map_err(|e| invalid(e.to_string()))?
        .len();
    if size > MAX_CLIP_BYTES {
        return Err(SpeakError::ClipTooLarge {
            path: shown,
            mb: size as f64 / (1024.0 * 1024.0),
        });
    }
    let reader =
        hound::WavReader::open(path).map_err(|e| invalid(format!("not a readable WAV ({e})")))?;
    let spec = reader.spec();
    if spec.channels == 0 || spec.sample_rate == 0 {
        return Err(invalid(format!(
            "it declares {} channel(s) at {} Hz",
            spec.channels, spec.sample_rate
        )));
    }
    let secs = f64::from(reader.duration()) / f64::from(spec.sample_rate);
    if secs < MIN_CLIP_SECS {
        return Err(SpeakError::ClipTooShort { path: shown, secs });
    }
    if spec.channels != 1 || spec.sample_rate < 16_000 {
        return Ok(Some(format!(
            "note: the clip is {} channel(s) at {} Hz; it will do, but a mono 16 kHz+ recording clones best",
            spec.channels, spec.sample_rate
        )));
    }
    Ok(None)
}

/// Copy `from` to `to` through a sibling temp file, so a failed copy never leaves half a clip
/// where the old one was.
fn store_clip(from: &Path, to: &Path) -> Result<(), SpeakError> {
    let store_err = |path: &Path, e: std::io::Error| SpeakError::Store {
        path: path.display().to_string(),
        detail: e.to_string(),
    };
    if let Some(dir) = to.parent() {
        std::fs::create_dir_all(dir).map_err(|e| store_err(dir, e))?;
    }
    let tmp = to.with_extension("wav.dmn-tmp");
    let copied = std::fs::copy(from, &tmp).and_then(|_| std::fs::rename(&tmp, to));
    copied.map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        store_err(to, e)
    })
}

/// Enroll: validate the clip, register it with the server, then copy it under the data dir and
/// record it as `[speak].voice_ref`. The server goes first, so a refused or failed
/// registration changes nothing on disk. #115: with `--language <code>` the voice is named
/// `voice-<code>` unless `--name` says otherwise, and the pair is added to (or updated in)
/// `[speak].languages`, so enrolling German and Spanish gives the picker a two-entry list.
pub fn enroll(
    cfg: &Config,
    paths: &Paths,
    args: &EnrollArgs,
    provider: &dyn TtsProvider,
) -> Result<String, SpeakError> {
    check_gates(cfg, paths)?;
    let language = args.language.as_deref().map(str::trim);
    if let Some(code) = language
        && !dmn_core::config::is_language_code(code)
    {
        return Err(SpeakError::BadLanguage {
            code: code.to_owned(),
        });
    }
    let default_name = language.map(|code| format!("voice-{code}"));
    let name = args
        .name
        .as_deref()
        .or(default_name.as_deref())
        .unwrap_or(DEFAULT_NAME);
    check_name(name)?;
    let note = validate_clip(&args.wav)?;
    let enrolled = rt()?
        .block_on(provider.enroll(name, &args.wav, language))
        .map_err(|e| tts_error(e, paths))?;
    let dest = paths.data_dir.join(format!("{name}.wav"));
    store_clip(&args.wav, &dest)?;
    // The list first: a file with no list yet is seeded from its current `target` +
    // `voice_ref` pair, which must be the pair as it was before this enroll.
    if let Some(code) = language {
        Config::write_speak_language(&paths.config_file, code, name)?;
    }
    Config::write_voice_ref(&paths.config_file, &dest.display().to_string())?;
    tracing::info!(voice = %enrolled.name, clip = %dest.display(), replaced = enrolled.replaced, "voice enrolled");
    let mut out = String::new();
    if let Some(n) = note {
        out.push_str(&n);
        out.push('\n');
    }
    out.push_str(&format!(
        "enrolled {:?}{} with the TTS server at {}{}\nstored the clip as {}\nset [speak].voice_ref in {}\n",
        enrolled.name,
        if enrolled.replaced {
            " (replacing the voice of that name)"
        } else {
            ""
        },
        cfg.speak.tts_url,
        language
            .map(|l| format!(", speaking {l}"))
            .unwrap_or_default(),
        dest.display(),
        paths.config_file.display()
    ));
    if let Some(code) = language {
        out.push_str(&format!(
            "set {{ code = {code:?}, voice = {name:?} }} in [speak].languages (the picker's list, `v` in dmn run)\n"
        ));
    }
    // #122: a cross-lingual engine needs no voice per language.
    if provider.cross_lingual() {
        let speaks = provider
            .languages()
            .map(|l| format!(" ({})", l.join(", ")))
            .unwrap_or_default();
        out.push_str(&format!(
            "{} voices speak every language it knows{speaks}: one voice can serve every [speak].languages entry (voice = {name:?})\n",
            provider.name()
        ));
    }
    Ok(out)
}

/// Speak: synthesize `text` in the enrolled voice and play it into the virtual microphone
/// `dmn-voice`, or with `out` write it there as a 16-bit mono WAV. For playback `provider`
/// should be the streaming one (`ChatterboxProvider::streaming`), so the first sentence plays
/// while the server renders the next.
pub fn speak(
    cfg: &Config,
    paths: &Paths,
    text: &str,
    out: Option<&Path>,
    provider: &dyn TtsProvider,
) -> Result<String, SpeakError> {
    let started = Instant::now();
    check_gates(cfg, paths)?;
    if cfg.speak.voice_ref.is_empty() {
        return Err(SpeakError::NotEnrolled {
            config: paths.config_file.display().to_string(),
        });
    }
    if text.trim().is_empty() {
        return Err(SpeakError::EmptyText);
    }
    // The microphone first: a missing PipeWire is reported before any speech is asked for,
    // and the node is in the graph while the server renders, so the call can select it.
    let mic = match out {
        Some(_) => None,
        None => Some(open_vmic()?),
    };
    let voice = voice_name(&cfg.speak.voice_ref);
    let req = SpeakRequest::new(text.trim(), voice.clone()).with_tuning(tuning(cfg));
    let stream = rt()?
        .block_on(provider.synthesize(req))
        .map_err(|e| match e {
            TtsError::UnknownVoice { voice, .. } => SpeakError::VoiceGone {
                voice,
                clip: cfg.speak.voice_ref.clone(),
            },
            other => tts_error(other, paths),
        })?;
    let sample_rate = stream.sample_rate();
    match (out, mic) {
        (Some(out), _) => {
            let frames = write_wav(out, stream, paths)?;
            let secs = frames as f64 / f64::from(sample_rate);
            tracing::info!(voice = %voice, path = %out.display(), frames, "spoke");
            Ok(format!(
                "wrote {secs:.1} s of speech at {sample_rate} Hz in voice {voice:?} to {} ({:.1} s)\n",
                out.display(),
                started.elapsed().as_secs_f64()
            ))
        }
        (None, Some((ctx, mut mic))) => {
            let played = play_into(&mut mic, stream, started, paths);
            // Take the node out of the graph before reporting, on success or error; the
            // context (and its loop thread) goes after it.
            let closed = mic.close();
            drop(ctx);
            let played = played?;
            closed.map_err(SpeakError::Playback)?;
            tracing::info!(
                voice = %voice,
                tenths = played.tenths,
                first_audio_ms = played.first_audio.map(|d| d.as_millis() as u64),
                underruns = played.underruns,
                "spoke into dmn-voice"
            );
            Ok(played.report(&voice, started.elapsed()))
        }
        (None, None) => Err(SpeakError::Playback(AudioError::VirtualMic(
            "not opened".into(),
        ))),
    }
}

/// Connect to PipeWire and put `dmn-voice` in the graph at the graph's rate.
pub(crate) fn open_vmic() -> Result<(Arc<PipeWireContext>, VirtualMic), SpeakError> {
    open_vmic_with(None)
}

/// [`open_vmic`], with the user's own microphone passing through the node when `passthrough`
/// says (#114, `dmn run`).
pub(crate) fn open_vmic_with(
    passthrough: Option<dmn_audio::Passthrough>,
) -> Result<(Arc<PipeWireContext>, VirtualMic), SpeakError> {
    let ctx = Arc::new(PipeWireContext::connect().map_err(|e| match e {
        PipeWireError::Init(detail) | PipeWireError::Connect(detail) => {
            SpeakError::NoPipeWire { detail }
        }
        other => SpeakError::Playback(AudioError::PipeWire(other)),
    })?);
    let table = ctx.wait_for_sync(PW_SYNC_TIMEOUT);
    if let Some(why) = table.disconnected {
        return Err(SpeakError::NoPipeWire { detail: why });
    }
    let rate = table.graph_rate();
    let mic = match passthrough {
        Some(p) => VirtualMic::open_passthrough(Arc::clone(&ctx), rate, p),
        None => VirtualMic::open(Arc::clone(&ctx), rate),
    }
    .map_err(SpeakError::Playback)?;
    Ok((ctx, mic))
}

/// #113: `[speak]`'s TTS knobs for the speech request: the temperature always (0.6 by
/// default, under the server's 0.8), `cfg_weight` and `exaggeration` only when set.
/// #122: the client for `[speak].engine` on `[speak].tts_url`. `streaming` takes Chatterbox's
/// streaming route (the first sentence plays while the next renders); Qwen3-TTS always streams.
pub(crate) fn tts_provider(cfg: &Config, streaming: bool) -> Box<dyn TtsProvider> {
    let url = &cfg.speak.tts_url;
    match cfg.speak.engine {
        SpeakEngine::Chatterbox => {
            let p = dmn_tts::ChatterboxProvider::new(url);
            Box::new(if streaming { p.streaming() } else { p })
        }
        SpeakEngine::Qwen3 => Box::new(dmn_tts::Qwen3Provider::new(url)),
    }
}

pub(crate) fn tuning(cfg: &Config) -> dmn_tts::Tuning {
    dmn_tts::Tuning {
        temperature: Some(cfg.speak.temperature),
        cfg_weight: cfg.speak.cfg_weight,
        exaggeration: cfg.speak.exaggeration,
    }
}

/// Where the speech goes when it is not a file: `VirtualMic`, or a test's sink.
pub(crate) trait SpeechSink {
    /// #113: whether something is linked to the node and pulling from it now; `None` for a
    /// sink that cannot tell (a file).
    fn linked(&self) -> Option<bool> {
        None
    }
    fn rate(&self) -> u32;
    fn write(&mut self, samples: &[f32], sample_rate: u32) -> Result<(), AudioError>;
    /// Wait for the queued audio to play; `Ok(false)` when it did not within `timeout`.
    fn drain(&mut self, timeout: Duration) -> Result<bool, AudioError>;
    /// (samples written at `rate`, samples played, underruns, when the first one played).
    fn stats(&self) -> (u64, u64, u64, Option<Instant>);
    /// #114: let the user's own microphone through the sink (pass-through mode) or not
    /// (translate mode). Speech in flight keeps it out whatever this says. Nothing for a sink
    /// with no microphone (a file, `dmn speak`'s node).
    fn set_passthrough(&mut self, _on: bool) {}
}

impl SpeechSink for VirtualMic {
    fn rate(&self) -> u32 {
        VirtualMic::rate(self)
    }

    fn write(&mut self, samples: &[f32], sample_rate: u32) -> Result<(), AudioError> {
        VirtualMic::write(self, samples, sample_rate)
    }

    fn drain(&mut self, timeout: Duration) -> Result<bool, AudioError> {
        VirtualMic::drain(self, timeout)
    }

    fn stats(&self) -> (u64, u64, u64, Option<Instant>) {
        let s = VirtualMic::stats(self);
        (s.written, s.pulled, s.underruns, s.first_pull)
    }
}

/// What a playback came to, for the report and the log.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Played {
    /// Tenths of a second of speech at the sink's rate.
    tenths: u64,
    rate: u32,
    /// From the command's start to the first sample PipeWire pulled.
    first_audio: Option<Duration>,
    underruns: u64,
    /// Samples still queued when the drain timed out (0 when everything played).
    unplayed: u64,
}

impl Played {
    fn report(&self, voice: &str, total: Duration) -> String {
        let mut s = format!(
            "spoke {}.{} s of speech into {VMIC_NODE_NAME} at {} Hz in voice {voice:?}: ",
            self.tenths / 10,
            self.tenths % 10,
            self.rate
        );
        match self.first_audio {
            Some(d) => s.push_str(&format!("first audio after {:.1} s, ", d.as_secs_f64())),
            None => s.push_str("nothing was pulled, "),
        }
        s.push_str(&format!("done after {:.1} s", total.as_secs_f64()));
        if self.underruns > 0 {
            s.push_str(&format!(
                " ({} gaps: the server rendered slower than real time)",
                self.underruns
            ));
        }
        if self.unplayed > 0 {
            s.push_str(&format!(
                "; {:.1} s were never pulled: is {VMIC_NODE_NAME} selected as the microphone anywhere?",
                self.unplayed as f64 / f64::from(self.rate)
            ));
        }
        s.push('\n');
        s
    }
}

/// Feed the stream into the sink as it arrives, then wait for the last sample to play.
fn play_into<S: SpeechSink>(
    sink: &mut S,
    stream: PcmStream,
    started: Instant,
    paths: &Paths,
) -> Result<Played, SpeakError> {
    for chunk in stream {
        let chunk = chunk.map_err(|e| tts_error(e, paths))?;
        sink.write(&chunk.samples, chunk.sample_rate)
            .map_err(SpeakError::Playback)?;
    }
    let rate = sink.rate().max(1);
    let (written, pulled, _, _) = sink.stats();
    let queued = Duration::from_secs_f64(written.saturating_sub(pulled) as f64 / f64::from(rate));
    let drained = sink
        .drain(queued + DRAIN_MARGIN)
        .map_err(SpeakError::Playback)?;
    let (written, pulled, underruns, first_pull) = sink.stats();
    Ok(Played {
        tenths: written * 10 / u64::from(rate),
        rate,
        first_audio: first_pull.map(|t| t.saturating_duration_since(started)),
        underruns,
        unplayed: if drained {
            0
        } else {
            written.saturating_sub(pulled)
        },
    })
}

/// Drain the stream into a 16-bit mono WAV at `path`, through a sibling temp file renamed into
/// place at the end: a synthesis that fails midway leaves no partial file and does not touch a
/// file already at `path`. Returns the number of frames written.
fn write_wav(path: &Path, stream: dmn_tts::PcmStream, paths: &Paths) -> Result<u64, SpeakError> {
    let shown = path.display().to_string();
    let write_err = |detail: String| SpeakError::WriteOut {
        path: shown.clone(),
        detail,
    };
    let mut tmp_name = path.file_name().unwrap_or_default().to_os_string();
    tmp_name.push(".dmn-tmp");
    let tmp = path.with_file_name(tmp_name);
    let write = || -> Result<u64, SpeakError> {
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate: stream.sample_rate(),
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let mut writer =
            hound::WavWriter::create(&tmp, spec).map_err(|e| write_err(e.to_string()))?;
        let mut frames = 0u64;
        for chunk in stream {
            let chunk = chunk.map_err(|e| tts_error(e, paths))?;
            for sample in &chunk.samples {
                let v = (sample.clamp(-1.0, 1.0) * f32::from(i16::MAX)) as i16;
                writer
                    .write_sample(v)
                    .map_err(|e| write_err(e.to_string()))?;
                frames += 1;
            }
        }
        writer.finalize().map_err(|e| write_err(e.to_string()))?;
        std::fs::rename(&tmp, path).map_err(|e| write_err(e.to_string()))?;
        Ok(frames)
    };
    write().inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use dmn_tts::MockTtsProvider;
    use dmn_tts::testserver::{MockServer, Reply, closed_port_url, serve};

    /// A tempdir XDG layout (never `~/.config/dmn` or `~/.local/share/dmn`) and a config with
    /// speak on, pointed at `url`.
    fn setup(url: &str) -> (tempfile::TempDir, Paths, Config) {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::under(dir.path());
        let mut cfg = Config::default();
        cfg.speak.enabled = true;
        cfg.speak.tts_url = url.to_owned();
        (dir, paths, cfg)
    }

    fn clip(path: &Path, channels: u16, rate: u32, secs: f64) {
        let spec = hound::WavSpec {
            channels,
            sample_rate: rate,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let mut w = hound::WavWriter::create(path, spec).unwrap();
        for i in 0..(f64::from(rate) * secs) as usize {
            let t = i as f32 / rate as f32;
            let v = (0.3 * (std::f32::consts::TAU * 200.0 * t).sin() * f32::from(i16::MAX)) as i16;
            for _ in 0..channels {
                w.write_sample(v).unwrap();
            }
        }
        w.finalize().unwrap();
    }

    fn args(wav: &Path, name: Option<&str>) -> EnrollArgs {
        EnrollArgs {
            wav: wav.to_path_buf(),
            name: name.map(str::to_owned),
            language: None,
        }
    }

    fn sine_answer() -> Vec<u8> {
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate: 24_000,
            bits_per_sample: 32,
            sample_format: hound::SampleFormat::Float,
        };
        let mut buf = Vec::new();
        let mut w = hound::WavWriter::new(std::io::Cursor::new(&mut buf), spec).unwrap();
        for i in 0..12_000 {
            w.write_sample((i as f32 * 0.05).sin() * 0.5).unwrap();
        }
        w.finalize().unwrap();
        buf
    }

    /// A user's config with comments and a live `[lookup]`, the way the owner's looks.
    const USER_CONFIG: &str = "\
# mine
[lookup]
enabled = true          # SearXNG
provider = \"searxng\"

[speak]
enabled = true          # turned on for #107
target = \"es\"           # #109
";

    #[test]
    fn both_subcommands_refuse_while_speak_is_disabled_and_name_the_line() {
        let url = closed_port_url().unwrap();
        let (dir, paths, mut cfg) = setup(&url);
        cfg.speak.enabled = false;
        let mock = MockTtsProvider::new();
        let wav = dir.path().join("me.wav");
        clip(&wav, 1, 16_000, 7.0);
        let err = enroll(&cfg, &paths, &args(&wav, None), &mock).unwrap_err();
        let msg = err.to_string();
        assert!(matches!(err, SpeakError::Disabled { .. }), "{msg}");
        assert!(msg.contains("`enabled = true` under [speak]"), "{msg}");
        assert!(
            msg.contains(&paths.config_file.display().to_string()),
            "{msg}"
        );

        let out = dir.path().join("x.wav");
        let err = speak(&cfg, &paths, "hola", Some(&out), &mock).unwrap_err();
        assert!(matches!(err, SpeakError::Disabled { .. }), "{err}");
        assert!(mock.requests().is_empty() && mock.enrollments().is_empty());
        assert!(!out.exists() && !paths.data_dir.exists() && !paths.config_file.exists());
    }

    #[test]
    fn a_tts_url_off_this_machine_is_refused() {
        let (dir, paths, mut cfg) = setup("http://gpu-box.lan:4123");
        cfg.speak.voice_ref = "/v/voice.wav".into();
        let mock = MockTtsProvider::new();
        let out = dir.path().join("x.wav");
        let err = speak(&cfg, &paths, "hola", Some(&out), &mock).unwrap_err();
        assert!(matches!(err, SpeakError::NotLocal { .. }), "{err}");
        assert!(err.to_string().contains("gpu-box.lan"), "{err}");
        let wav = dir.path().join("me.wav");
        clip(&wav, 1, 16_000, 7.0);
        let err = enroll(&cfg, &paths, &args(&wav, None), &mock).unwrap_err();
        assert!(matches!(err, SpeakError::NotLocal { .. }), "{err}");
        assert!(mock.requests().is_empty() && mock.enrollments().is_empty());
    }

    /// The whole enroll against the HTTP mock: one upload, the clip stored byte for byte as
    /// `<data>/voice-es.wav` (#115: `--language es` names the voice `voice-es`), `voice_ref`
    /// set in the user's file with its comments kept, and the pair added to `languages`.
    #[test]
    fn enroll_registers_stores_and_records_the_clip() {
        let server = serve(vec![Reply::new(
            "201 Created",
            "application/json",
            r#"{"voice":{"name":"voice-es","language":"es"}}"#,
        )])
        .unwrap();
        let (dir, paths, cfg) = setup(&server.url);
        std::fs::create_dir_all(paths.config_file.parent().unwrap()).unwrap();
        std::fs::write(&paths.config_file, USER_CONFIG).unwrap();
        let wav = dir.path().join("my clip.wav");
        clip(&wav, 1, 24_000, 7.0);
        let mut a = args(&wav, None);
        a.language = Some("es".into());
        let out = enroll(
            &cfg,
            &paths,
            &a,
            &dmn_tts::ChatterboxProvider::new(&server.url),
        )
        .unwrap();
        assert!(out.contains("enrolled \"voice-es\""), "{out}");
        assert!(out.contains("speaking es"), "{out}");
        assert!(
            out.contains("set { code = \"es\", voice = \"voice-es\" } in [speak].languages"),
            "{out}"
        );

        assert_eq!(server.request_lines(), ["POST /v1/voices"]);
        let requests = server.requests();
        let (_, body) = MockServer::head_and_body(&requests[0]);
        let text = String::from_utf8_lossy(body);
        assert!(
            text.contains("name=\"voice_name\"\r\n\r\nvoice-es\r\n"),
            "{text}"
        );
        assert!(text.contains("name=\"language\"\r\n\r\nes\r\n"), "{text}");

        let stored = paths.data_dir.join("voice-es.wav");
        assert_eq!(
            std::fs::read(&stored).unwrap(),
            std::fs::read(&wav).unwrap()
        );
        let after = std::fs::read_to_string(&paths.config_file).unwrap();
        assert!(
            after.starts_with(USER_CONFIG),
            "the user's lines are untouched:\n{after}"
        );
        let cfg = Config::load(&paths.config_file).unwrap();
        assert_eq!(cfg.speak.voice_ref, stored.display().to_string());
        assert_eq!(
            cfg.speak.languages,
            [dmn_core::config::SpeakLanguage {
                code: "es".into(),
                voice: "voice-es".into(),
            }]
        );
        assert_eq!(cfg.speak.target, "es", "kept");
        assert!(cfg.lookup.enabled);
    }

    /// #115: the issue's criterion. The owner's shape of config (comments, other sections, a
    /// `target` + `voice_ref` pair and no list): enrolling German adds a two-entry list that
    /// keeps the Russian voice, enrolling Spanish makes it three, enrolling German again
    /// updates its entry, and only the `voice_ref` and `languages` lines change.
    #[test]
    fn enroll_with_a_language_adds_or_updates_that_languages_entry_and_nothing_else() {
        let owner = "\
# nick's config
[llm]
model = \"qwen3.6:35b-a3b\"      # measured

[speak]
enabled = true
tts_url = \"http://localhost:4123\"
voice_ref = \"/home/u/.local/share/dmn/voice-ru.wav\"   # enrolled 2026-10-07
target = \"ru\"
model = \"llama3.1:8b\"
temperature = 0.6

[ui]
mouse = false
";
        let (dir, paths, cfg) = setup("http://localhost:4123");
        std::fs::create_dir_all(paths.config_file.parent().unwrap()).unwrap();
        std::fs::write(&paths.config_file, owner).unwrap();
        let wav = dir.path().join("me.wav");
        clip(&wav, 1, 16_000, 7.0);
        let mock = MockTtsProvider::new();
        let mut a = args(&wav, None);
        a.language = Some("de".into());
        enroll(&cfg, &paths, &a, &mock).unwrap();
        a.language = Some("es".into());
        enroll(&cfg, &paths, &a, &mock).unwrap();
        a.language = Some("de".into());
        a.name = Some("narrator".into());
        enroll(&cfg, &paths, &a, &mock).unwrap();
        let names: Vec<(String, Option<String>)> = mock
            .enrollments()
            .iter()
            .map(|e| (e.name.clone(), e.language.clone()))
            .collect();
        assert_eq!(
            names,
            [
                ("voice-de".to_owned(), Some("de".to_owned())),
                ("voice-es".to_owned(), Some("es".to_owned())),
                ("narrator".to_owned(), Some("de".to_owned())),
            ]
        );
        for clip in ["voice-de.wav", "voice-es.wav", "narrator.wav"] {
            assert!(paths.data_dir.join(clip).is_file(), "{clip}");
        }
        let after = std::fs::read_to_string(&paths.config_file).unwrap();
        let changed: Vec<&str> = after.lines().filter(|l| !owner.contains(l)).collect();
        assert_eq!(changed.len(), 2, "only voice_ref and languages:\n{after}");
        assert!(
            changed[0].starts_with("voice_ref = ") && changed[0].ends_with("# enrolled 2026-10-07"),
            "{}",
            changed[0]
        );
        assert_eq!(
            changed[1],
            "languages = [{ code = \"ru\", voice = \"voice-ru\" }, { code = \"de\", voice = \"narrator\" }, { code = \"es\", voice = \"voice-es\" }]"
        );
        assert_eq!(after.lines().count(), owner.lines().count() + 1);
        let cfg = Config::load(&paths.config_file).unwrap();
        assert_eq!(
            cfg.speak.target, "ru",
            "the target is not changed by an enroll"
        );
        assert_eq!(cfg.speak.voice_for("ru").as_deref(), Some("voice-ru"));
        assert_eq!(cfg.speak.voice_for("de").as_deref(), Some("narrator"));
        assert_eq!(cfg.speak.model.as_deref(), Some("llama3.1:8b"));
        assert!(!cfg.ui.mouse);

        // The code must have the shape the config takes, before anything reaches the server.
        a.language = Some("German".into());
        let err = enroll(&cfg, &paths, &a, &mock).unwrap_err();
        assert!(matches!(err, SpeakError::BadLanguage { .. }), "{err}");
        assert_eq!(mock.enrollments().len(), 3);
    }

    #[test]
    fn enroll_with_a_name_uses_it_for_the_clip_and_the_server() {
        let (dir, paths, cfg) = setup("http://localhost:4123");
        let wav = dir.path().join("me.wav");
        clip(&wav, 1, 16_000, 6.5);
        let mock = MockTtsProvider::new();
        enroll(&cfg, &paths, &args(&wav, Some("narrator")), &mock).unwrap();
        assert!(paths.data_dir.join("narrator.wav").is_file());
        assert_eq!(mock.enrollments()[0].name, "narrator");
        let voice_ref = Config::load(&paths.config_file).unwrap().speak.voice_ref;
        assert_eq!(voice_name(&voice_ref), "narrator");
    }

    #[test]
    fn enroll_refuses_a_short_clip_a_non_wav_and_a_bad_name() {
        let (dir, paths, cfg) = setup("http://localhost:4123");
        let mock = MockTtsProvider::new();

        let short = dir.path().join("short.wav");
        clip(&short, 1, 16_000, 2.0);
        let err = enroll(&cfg, &paths, &args(&short, None), &mock).unwrap_err();
        assert!(matches!(err, SpeakError::ClipTooShort { .. }), "{err}");
        assert!(err.to_string().contains("2.0 s long"), "{err}");
        assert!(err.to_string().contains("at least 6 s"), "{err}");

        let text = dir.path().join("notes.wav");
        std::fs::write(&text, "not audio").unwrap();
        let err = enroll(&cfg, &paths, &args(&text, None), &mock).unwrap_err();
        assert!(matches!(err, SpeakError::InvalidWav { .. }), "{err}");
        let err = enroll(
            &cfg,
            &paths,
            &args(&dir.path().join("gone.wav"), None),
            &mock,
        )
        .unwrap_err();
        assert!(matches!(err, SpeakError::InvalidWav { .. }), "{err}");

        let ok = dir.path().join("ok.wav");
        clip(&ok, 1, 16_000, 7.0);
        for bad in ["../evil", "", "has space", "a.b", &"x".repeat(65)] {
            let err = enroll(&cfg, &paths, &args(&ok, Some(bad)), &mock).unwrap_err();
            assert!(matches!(err, SpeakError::BadName { .. }), "{bad:?}: {err}");
        }

        assert!(mock.enrollments().is_empty(), "nothing reached the server");
        assert!(!paths.data_dir.exists(), "nothing stored");
        assert!(!paths.config_file.exists(), "config untouched");
    }

    #[test]
    fn a_stereo_or_low_rate_clip_enrolls_with_a_note() {
        let (dir, paths, cfg) = setup("http://localhost:4123");
        let mock = MockTtsProvider::new();
        let stereo = dir.path().join("stereo.wav");
        clip(&stereo, 2, 44_100, 7.0);
        let out = enroll(&cfg, &paths, &args(&stereo, None), &mock).unwrap();
        assert!(out.contains("2 channel(s) at 44100 Hz"), "{out}");
        assert!(out.contains("mono 16 kHz+"), "{out}");
        let low = dir.path().join("low.wav");
        clip(&low, 1, 8_000, 7.0);
        let out = enroll(&cfg, &paths, &args(&low, Some("phone")), &mock).unwrap();
        assert!(out.contains("1 channel(s) at 8000 Hz"), "{out}");
        assert_eq!(mock.enrollments().len(), 2);
    }

    /// The server refuses (or is not running): nothing is stored and the config is unchanged,
    /// and the unreachable case says how to fix it.
    #[test]
    fn a_failed_registration_changes_nothing_on_disk() {
        let server = serve(vec![Reply::new(
            "400 Bad Request",
            "application/json",
            r#"{"detail":{"error":{"message":"Unsupported language: xx"}}}"#,
        )])
        .unwrap();
        let (dir, paths, cfg) = setup(&server.url);
        std::fs::create_dir_all(paths.config_file.parent().unwrap()).unwrap();
        std::fs::write(&paths.config_file, USER_CONFIG).unwrap();
        let wav = dir.path().join("me.wav");
        clip(&wav, 1, 16_000, 7.0);
        let mut a = args(&wav, None);
        a.language = Some("xx".into());
        let p = dmn_tts::ChatterboxProvider::new(&server.url);
        let err = enroll(&cfg, &paths, &a, &p).unwrap_err();
        assert!(
            err.to_string().contains("Unsupported language: xx"),
            "{err}"
        );

        let url = closed_port_url().unwrap();
        let (_, _, down) = setup(&url);
        let p = dmn_tts::ChatterboxProvider::new(&url);
        let err = enroll(&down, &paths, &args(&wav, None), &p).unwrap_err();
        let msg = err.to_string();
        assert!(matches!(err, SpeakError::ServerDown { .. }), "{msg}");
        assert!(
            msg.contains(&url) && msg.contains("README § Speak"),
            "{msg}"
        );
        assert!(msg.contains("`tts_url` under [speak]"), "{msg}");

        assert!(!paths.data_dir.exists(), "no clip stored");
        assert_eq!(
            std::fs::read_to_string(&paths.config_file).unwrap(),
            USER_CONFIG
        );
    }

    /// A sink standing in for `dmn-voice`: keeps what it was given, "plays" it all at `drain`.
    struct TestSink {
        rate: u32,
        chunks: Vec<(usize, u32)>,
        written: u64,
        pulled: u64,
        first: Option<Instant>,
        drains: bool,
    }

    impl TestSink {
        fn new(rate: u32, drains: bool) -> Self {
            Self {
                rate,
                chunks: Vec::new(),
                written: 0,
                pulled: 0,
                first: None,
                drains,
            }
        }
    }

    impl SpeechSink for TestSink {
        fn rate(&self) -> u32 {
            self.rate
        }

        fn write(&mut self, samples: &[f32], sample_rate: u32) -> Result<(), AudioError> {
            self.chunks.push((samples.len(), sample_rate));
            // What a 24 kHz chunk becomes at the sink's rate.
            self.written += samples.len() as u64 * u64::from(self.rate) / u64::from(sample_rate);
            Ok(())
        }

        fn drain(&mut self, _timeout: Duration) -> Result<bool, AudioError> {
            if self.drains {
                self.pulled = self.written;
                self.first.get_or_insert_with(Instant::now);
            }
            Ok(self.drains)
        }

        fn stats(&self) -> (u64, u64, u64, Option<Instant>) {
            (self.written, self.pulled, 0, self.first)
        }
    }

    /// Without `--out`, every decoded chunk reaches the sink at the server's rate, in order,
    /// and the report carries the speech length, the first-audio time and the total.
    #[test]
    fn play_into_hands_every_chunk_to_the_sink_and_reports() {
        let server = serve(vec![
            Reply::voices(&["voice"]),
            Reply::wav(sine_answer()).in_pieces_of(333),
        ])
        .unwrap();
        let (_dir, paths, _cfg) = setup(&server.url);
        let p = dmn_tts::ChatterboxProvider::new(&server.url).streaming();
        let stream = rt()
            .unwrap()
            .block_on(p.synthesize(SpeakRequest::new("hola", "voice")))
            .unwrap();
        let started = Instant::now();
        let mut sink = TestSink::new(48_000, true);
        let played = play_into(&mut sink, stream, started, &paths).unwrap();
        let total: usize = sink.chunks.iter().map(|(n, _)| n).sum();
        assert_eq!(total, 12_000, "every sample of the 0.5 s answer");
        assert!(sink.chunks.iter().all(|(_, r)| *r == 24_000));
        assert!(sink.chunks.len() > 1, "fed as it arrived, not as one block");
        assert_eq!(played.tenths, 5, "0.5 s at the sink's rate");
        assert_eq!(played.rate, 48_000);
        assert_eq!(played.unplayed, 0);
        assert!(played.first_audio.is_some());
        let report = played.report("voice", Duration::from_millis(1_234));
        assert!(report.starts_with("spoke 0.5 s of speech into dmn-voice at 48000 Hz in voice \"voice\": first audio after 0."), "{report}");
        assert!(report.contains("done after 1.2 s\n"), "{report}");
        assert!(
            !report.contains("gaps") && !report.contains("never pulled"),
            "{report}"
        );
    }

    /// Nothing selected `dmn-voice` and the clip fit the ring, so every `write` succeeded and
    /// only `drain` came up empty: the report says how much never played and asks the
    /// question, instead of failing. A clip longer than the ring never gets here: `write`
    /// itself fails after its 5 s no-listener timeout and the command exits 1.
    #[test]
    fn a_drain_that_times_out_is_reported_not_hidden() {
        let server = serve(vec![Reply::voices(&["voice"]), Reply::wav(sine_answer())]).unwrap();
        let (_dir, paths, _cfg) = setup(&server.url);
        let p = dmn_tts::ChatterboxProvider::new(&server.url).streaming();
        let stream = rt()
            .unwrap()
            .block_on(p.synthesize(SpeakRequest::new("hola", "voice")))
            .unwrap();
        let mut sink = TestSink::new(48_000, false);
        let played = play_into(&mut sink, stream, Instant::now(), &paths).unwrap();
        assert_eq!(played.unplayed, 24_000);
        assert!(played.first_audio.is_none());
        let report = played.report("voice", Duration::from_secs(3));
        assert!(report.contains("nothing was pulled"), "{report}");
        assert!(
            report.contains(
                "0.5 s were never pulled: is dmn-voice selected as the microphone anywhere?"
            ),
            "{report}"
        );
        let mut gaps = played;
        gaps.underruns = 3;
        assert!(gaps.report("voice", Duration::ZERO).contains("3 gaps"));
    }

    /// The answer breaks off mid-stream during playback: the error surfaces through the same
    /// path as a file write, after whatever arrived was fed.
    #[test]
    fn a_stream_that_fails_midway_during_playback_surfaces_the_error() {
        let mut cut = sine_answer();
        cut.truncate(cut.len() / 2);
        let server = serve(vec![Reply::voices(&["voice"]), Reply::wav(cut)]).unwrap();
        let (_dir, paths, _cfg) = setup(&server.url);
        let p = dmn_tts::ChatterboxProvider::new(&server.url);
        let stream = rt()
            .unwrap()
            .block_on(p.synthesize(SpeakRequest::new("hola", "voice")))
            .unwrap();
        let mut sink = TestSink::new(48_000, true);
        let err = play_into(&mut sink, stream, Instant::now(), &paths).unwrap_err();
        assert!(
            matches!(err, SpeakError::Tts(TtsError::Decode { .. })),
            "{err}"
        );
    }

    #[test]
    fn the_no_pipewire_error_names_the_node_the_server_and_the_file_alternative() {
        let err = SpeakError::NoPipeWire {
            detail: "Host is down".into(),
        };
        let msg = err.to_string();
        assert!(
            msg.contains("cannot reach PipeWire (Host is down)"),
            "{msg}"
        );
        assert!(msg.contains("dmn-voice"), "{msg}");
        assert!(msg.contains("--out <file.wav>"), "{msg}");
    }

    #[test]
    fn speak_without_an_enrolled_voice_or_text_says_so() {
        let (dir, paths, mut cfg) = setup("http://localhost:4123");
        let mock = MockTtsProvider::new();
        let out = dir.path().join("x.wav");
        let err = speak(&cfg, &paths, "hola", Some(&out), &mock).unwrap_err();
        assert!(matches!(err, SpeakError::NotEnrolled { .. }), "{err}");
        assert!(err.to_string().contains("dmn speak enroll --wav"), "{err}");
        cfg.speak.voice_ref = "/data/voice.wav".into();
        let err = speak(&cfg, &paths, "  ", Some(&out), &mock).unwrap_err();
        assert!(matches!(err, SpeakError::EmptyText), "{err}");
        assert!(mock.requests().is_empty());
    }

    /// The whole speak against the HTTP mock: the voice check, then the speech request with
    /// the text and the voice named by `voice_ref`; the server's float WAV, arriving in
    /// pieces, becomes a 16-bit mono WAV at its rate.
    #[test]
    fn speak_out_writes_a_parseable_wav_in_the_enrolled_voice() {
        let server = serve(vec![
            Reply::voices(&["voice"]),
            Reply::wav(sine_answer()).in_pieces_of(333),
        ])
        .unwrap();
        let (dir, paths, mut cfg) = setup(&server.url);
        cfg.speak.voice_ref = paths.data_dir.join("voice.wav").display().to_string();
        let out = dir.path().join("speech.wav");
        let p = dmn_tts::ChatterboxProvider::new(&server.url);
        let msg = speak(&cfg, &paths, " hola ", Some(&out), &p).unwrap();
        assert!(msg.contains("wrote 0.5 s of speech at 24000 Hz"), "{msg}");

        let reader = hound::WavReader::open(&out).unwrap();
        let spec = reader.spec();
        assert_eq!(
            (spec.channels, spec.sample_rate, spec.bits_per_sample),
            (1, 24_000, 16)
        );
        assert_eq!(reader.duration(), 12_000);

        assert_eq!(
            server.request_lines(),
            ["GET /v1/voices", "POST /v1/audio/speech"]
        );
        let requests = server.requests();
        let (_, body) = MockServer::head_and_body(&requests[1]);
        let json: serde_json::Value = serde_json::from_slice(body).unwrap();
        assert_eq!(json["input"], "hola");
        assert_eq!(json["voice"], "voice", "derived from voice_ref's file name");
        assert_eq!(json["response_format"], "wav");
        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().ends_with(".dmn-tmp"))
            .collect();
        assert!(leftovers.is_empty(), "no temp file left: {leftovers:?}");
    }

    #[test]
    fn speak_reports_an_unreachable_server_and_writes_nothing() {
        let url = closed_port_url().unwrap();
        let (dir, paths, mut cfg) = setup(&url);
        cfg.speak.voice_ref = "/data/voice.wav".into();
        let out = dir.path().join("x.wav");
        let p = dmn_tts::ChatterboxProvider::new(&url);
        let err = speak(&cfg, &paths, "hola", Some(&out), &p).unwrap_err();
        let msg = err.to_string();
        assert!(matches!(err, SpeakError::ServerDown { .. }), "{msg}");
        assert!(
            msg.contains(&url) && msg.to_lowercase().contains("refused"),
            "{msg}"
        );
        assert!(!out.exists(), "no file left");
    }

    /// The server's library lost the voice: refused before any speech is asked (the server
    /// would answer in its default voice), with the command that fixes it, and a file
    /// already at `--out` is left as it was.
    #[test]
    fn speak_refuses_a_voice_the_server_no_longer_has() {
        let server = serve(vec![Reply::voices(&[])]).unwrap();
        let (dir, paths, mut cfg) = setup(&server.url);
        cfg.speak.voice_ref = "/data/me.wav".into();
        let out = dir.path().join("keep.wav");
        std::fs::write(&out, "earlier output").unwrap();
        let p = dmn_tts::ChatterboxProvider::new(&server.url);
        let err = speak(&cfg, &paths, "hola", Some(&out), &p).unwrap_err();
        assert!(matches!(err, SpeakError::VoiceGone { .. }), "{err}");
        assert!(
            err.to_string()
                .contains("dmn speak enroll --wav /data/me.wav --name me"),
            "{err}"
        );
        assert_eq!(server.request_lines(), ["GET /v1/voices"]);
        assert_eq!(std::fs::read_to_string(&out).unwrap(), "earlier output");
    }

    /// The answer breaks off after the header: the error surfaces, no partial WAV is left, and
    /// a file already at `--out` survives.
    #[test]
    fn a_synthesis_that_fails_midway_leaves_no_partial_file() {
        let mut cut = sine_answer();
        cut.truncate(cut.len() / 2);
        let server = serve(vec![Reply::voices(&["voice"]), Reply::wav(cut)]).unwrap();
        let (dir, paths, mut cfg) = setup(&server.url);
        cfg.speak.voice_ref = "/data/voice.wav".into();
        let out = dir.path().join("keep.wav");
        std::fs::write(&out, "earlier output").unwrap();
        let p = dmn_tts::ChatterboxProvider::new(&server.url);
        let err = speak(&cfg, &paths, "hola", Some(&out), &p).unwrap_err();
        assert!(
            matches!(err, SpeakError::Tts(TtsError::Decode { .. })),
            "{err}"
        );
        assert_eq!(std::fs::read_to_string(&out).unwrap(), "earlier output");
        assert!(!dir.path().join("keep.wav.dmn-tmp").exists());
    }
}
