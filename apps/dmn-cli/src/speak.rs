//! `dmn speak` (#107): enroll a reference clip and synthesize text in the cloned voice through
//! the local TTS server. Nothing plays audio yet — without `--out` the speak command says so
//! (playback is #108). Both subcommands refuse to run while `[speak].enabled = false`.

use std::path::{Path, PathBuf};

use dmn_core::config::{Config, ConfigError};
use dmn_core::Paths;
use dmn_tts::{SpeakRequest, TtsError, TtsProvider};

/// A reference clip needs at least this much clean speech for the clone to hold up.
const MIN_CLIP_SECS: f64 = 6.0;

#[derive(Debug, thiserror::Error)]
pub enum SpeakError {
    #[error(
        "speak is disabled; set `enabled = true` under [speak] in {config} (and see README § Speak for the server)"
    )]
    Disabled { config: String },
    #[error("no voice enrolled; run `dmn speak enroll --wav <clip>` first")]
    NotEnrolled,
    #[error("nothing plays audio yet: playback lands in #108; pass --out <file.wav> to write the synthesized speech to a file")]
    NoPlayback,
    #[error("invalid voice reference {path}: {detail}")]
    InvalidWav { path: String, detail: String },
    #[error(
        "voice reference {path} is {secs:.1} s; record at least {MIN_CLIP_SECS:.0} s of clean speech"
    )]
    ClipTooShort { path: String, secs: f64 },
    #[error("voice name {name:?} is not a plain file name")]
    BadName { name: String },
    #[error("cannot store the voice clip at {path}: {detail}")]
    Store { path: String, detail: String },
    #[error("cannot write {path}: {detail}")]
    WriteOut { path: String, detail: String },
    #[error("cannot start the async runtime: {0}")]
    Runtime(String),
    #[error(transparent)]
    Tts(#[from] TtsError),
    #[error(transparent)]
    Config(#[from] ConfigError),
}

/// `dmn speak enroll --wav <clip> [--name <n>]`.
pub struct EnrollArgs {
    pub wav: PathBuf,
    pub name: Option<String>,
}

fn rt() -> Result<tokio::runtime::Runtime, SpeakError> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| SpeakError::Runtime(e.to_string()))
}

/// The voice name the server knows the clip by is the stored clip's file stem, so `dmn speak`
/// finds it from `[speak].voice_ref` alone (no name is kept in the config).
fn voice_name(voice_ref: &str) -> String {
    Path::new(voice_ref)
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "voice".to_owned())
}

/// Open the clip as audio and check it is a usable voice reference. Returns a warning line when
/// the clip works but is not the preferred shape (mono, 16 kHz+).
fn validate_clip(path: &Path) -> Result<Option<String>, SpeakError> {
    let shown = path.display().to_string();
    let src = dmn_audio::WavSource::open(path, dmn_audio::Source::You, dmn_audio::Speed::Fast)
        .map_err(|e| SpeakError::InvalidWav {
            path: shown.clone(),
            detail: format!("not a readable WAV: {e}"),
        })?;
    let secs = src.duration().as_secs_f64();
    if secs < MIN_CLIP_SECS {
        return Err(SpeakError::ClipTooShort { path: shown, secs });
    }
    let (rate, channels) = src.input_format();
    if channels != 1 || rate < 16_000 {
        return Ok(Some(format!(
            "note: the clip is {channels} channel(s) at {rate} Hz; a mono 16 kHz+ recording clones best"
        )));
    }
    Ok(None)
}

/// Enroll: validate the clip, copy it under the data dir, register it with the server, and
/// record it as `[speak].voice_ref` in the config.
pub fn enroll(
    cfg: &Config,
    paths: &Paths,
    args: &EnrollArgs,
    provider: &dyn TtsProvider,
) -> Result<String, SpeakError> {
    if !cfg.speak.enabled {
        return Err(SpeakError::Disabled {
            config: paths.config_file.display().to_string(),
        });
    }
    let warning = validate_clip(&args.wav)?;
    let name = match args.name.as_deref() {
        Some(n) if n.is_empty() || n.contains(['/', '\\']) || n.starts_with('.') => {
            return Err(SpeakError::BadName { name: n.to_owned() });
        }
        Some(n) => n.to_owned(),
        None => "voice".to_owned(),
    };
    let dest = paths.data_dir.join(format!("{name}.wav"));
    if let Some(dir) = dest.parent() {
        std::fs::create_dir_all(dir).map_err(|e| SpeakError::Store {
            path: dir.display().to_string(),
            detail: e.to_string(),
        })?;
    }
    std::fs::copy(&args.wav, &dest).map_err(|e| SpeakError::Store {
        path: dest.display().to_string(),
        detail: e.to_string(),
    })?;
    let registered = rt()?.block_on(provider.enroll(&name, &dest))?;
    Config::write_voice_ref(&paths.config_file, &dest.display().to_string())?;
    tracing::info!(voice = %registered, clip = %dest.display(), "voice enrolled");
    let mut out = String::new();
    if let Some(w) = warning {
        out.push_str(&w);
        out.push('\n');
    }
    out.push_str(&format!(
        "enrolled {registered:?} from {} ({})\nrecorded as [speak].voice_ref in {}\n",
        args.wav.display(),
        dest.display(),
        paths.config_file.display()
    ));
    Ok(out)
}

/// Speak: synthesize `text` in the enrolled voice and write it to `out` as a 16-bit mono WAV.
pub fn speak(
    cfg: &Config,
    paths: &Paths,
    text: &str,
    out: Option<&Path>,
    provider: &dyn TtsProvider,
) -> Result<String, SpeakError> {
    if !cfg.speak.enabled {
        return Err(SpeakError::Disabled {
            config: paths.config_file.display().to_string(),
        });
    }
    if cfg.speak.voice_ref.is_empty() {
        return Err(SpeakError::NotEnrolled);
    }
    let out = out.ok_or(SpeakError::NoPlayback)?;
    let voice = voice_name(&cfg.speak.voice_ref);
    let req = SpeakRequest::new(text, voice.clone());
    let rt = rt()?;
    let stream = rt.block_on(provider.synthesize(req))?;
    let sample_rate = stream.sample_rate();
    let written = write_wav(out, stream)?;
    tracing::info!(voice = %voice, path = %out.display(), frames = written, "spoke");
    Ok(format!(
        "wrote {:.1} s at {sample_rate} Hz ({voice}) to {}\n",
        written as f64 / f64::from(sample_rate),
        out.display()
    ))
}

/// Drain the stream into a 16-bit mono WAV; on a stream error the partial file is removed so a
/// broken synthesis never looks like a finished one. Returns the number of frames written.
fn write_wav(path: &Path, stream: dmn_tts::PcmStream) -> Result<u64, SpeakError> {
    let write = move || -> Result<u64, SpeakError> {
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate: stream.sample_rate(),
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let mut writer = hound::WavWriter::create(path, spec).map_err(|e| SpeakError::WriteOut {
            path: path.display().to_string(),
            detail: e.to_string(),
        })?;
        let mut frames = 0u64;
        for chunk in stream {
            let chunk = chunk?;
            for sample in &chunk.samples {
                let v = (sample.clamp(-1.0, 1.0) * f32::from(i16::MAX)) as i16;
                writer
                    .write_sample(v)
                    .map_err(|e| SpeakError::WriteOut {
                        path: path.display().to_string(),
                        detail: e.to_string(),
                    })?;
                frames += 1;
            }
        }
        writer.finalize().map_err(|e| SpeakError::WriteOut {
            path: path.display().to_string(),
            detail: e.to_string(),
        })?;
        Ok(frames)
    };
    write().inspect_err(|_| {
        let _ = std::fs::remove_file(path);
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use dmn_tts::MockTtsProvider;

    /// A config with speak enabled and a tempdir XDG layout; the returned config file is the
    /// template unless `write_template` is false.
    fn setup() -> (tempfile::TempDir, Paths, Config) {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::under(dir.path());
        let mut cfg = Config::default();
        cfg.speak.enabled = true;
        (dir, paths, cfg)
    }

    fn sine_clip(path: &Path, secs: f64) {
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate: 16_000,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let mut w = hound::WavWriter::create(path, spec).unwrap();
        let n = (16_000.0 * secs) as usize;
        for i in 0..n {
            let t = i as f32 / 16_000.0;
            w.write_sample((0.3 * (std::f32::consts::TAU * 200.0 * t).sin() * i16::MAX as f32) as i16)
                .unwrap();
        }
        w.finalize().unwrap();
    }

    fn enroll_args(wav: &Path, name: Option<&str>) -> EnrollArgs {
        EnrollArgs {
            wav: wav.to_path_buf(),
            name: name.map(str::to_owned),
        }
    }

    #[test]
    fn both_subcommands_refuse_while_speak_is_disabled() {
        let (_dir, paths, cfg) = setup();
        let cfg = Config {
            speak: dmn_core::config::Speak {
                enabled: false,
                ..cfg.speak
            },
            ..cfg
        };
        let mock = MockTtsProvider::new();
        let err = enroll(&cfg, &paths, &enroll_args(Path::new("x.wav"), None), &mock).unwrap_err();
        let msg = err.to_string();
        assert!(matches!(err, SpeakError::Disabled { .. }), "{msg}");
        assert!(msg.contains("[speak]"), "{msg}");
        assert!(msg.contains("enabled"), "{msg}");

        let err = speak(&cfg, &paths, "hola", None, &mock).unwrap_err();
        assert!(matches!(err, SpeakError::Disabled { .. }), "{err}");
        assert!(mock.requests().is_empty());
        assert!(mock.enrollments().is_empty());
    }

    #[test]
    fn enroll_validates_stores_registers_and_records_the_clip() {
        let (_dir, paths, cfg) = setup();
        let clip = _dir.path().join("my clip.wav");
        sine_clip(&clip, 7.0);
        let mock = MockTtsProvider::new();
        let out = enroll(&cfg, &paths, &enroll_args(&clip, None), &mock).unwrap();
        assert!(out.contains("\"voice\""), "{out}");

        let stored = paths.data_dir.join("voice.wav");
        assert!(stored.is_file(), "clip stored under the data dir");
        assert_eq!(mock.enrollments().len(), 1, "registered with the server");
        assert_eq!(mock.enrollments()[0].0, "voice");
        let cfg = Config::load(&paths.config_file).unwrap();
        assert_eq!(cfg.speak.voice_ref, stored.display().to_string());
        // The template's other comments survived the edit.
        let text = std::fs::read_to_string(&paths.config_file).unwrap();
        assert!(text.contains("[speak]"), "{text}");
        assert!(text.contains("# Daemon (dmn) configuration"), "{text}");
    }

    #[test]
    fn enroll_with_a_name_uses_it_for_the_clip_and_the_server() {
        let (_dir, paths, cfg) = setup();
        let clip = _dir.path().join("me.wav");
        sine_clip(&clip, 6.5);
        let mock = MockTtsProvider::new();
        enroll(&cfg, &paths, &enroll_args(&clip, Some("narrator")), &mock).unwrap();
        let stored = paths.data_dir.join("narrator.wav");
        assert!(stored.is_file());
        assert_eq!(mock.enrollments()[0].0, "narrator");
        assert_eq!(voice_name(&Config::load(&paths.config_file).unwrap().speak.voice_ref), "narrator");
    }

    #[test]
    fn enroll_refuses_a_short_clip_a_non_wav_and_a_bad_name() {
        let (_dir, paths, cfg) = setup();
        let mock = MockTtsProvider::new();

        let short = _dir.path().join("short.wav");
        sine_clip(&short, 2.0);
        let err = enroll(&cfg, &paths, &enroll_args(&short, None), &mock).unwrap_err();
        assert!(matches!(err, SpeakError::ClipTooShort { .. }), "{err}");
        assert!(err.to_string().contains("6"), "{err}");

        let text = _dir.path().join("notes.wav");
        std::fs::write(&text, "not audio").unwrap();
        let err = enroll(&cfg, &paths, &enroll_args(&text, None), &mock).unwrap_err();
        assert!(matches!(err, SpeakError::InvalidWav { .. }), "{err}");

        let clip = _dir.path().join("ok.wav");
        sine_clip(&clip, 7.0);
        let err = enroll(&cfg, &paths, &enroll_args(&clip, Some("../evil")), &mock).unwrap_err();
        assert!(matches!(err, SpeakError::BadName { .. }), "{err}");

        assert!(mock.enrollments().is_empty(), "nothing reached the server");
        assert!(Config::load(&paths.config_file).unwrap().speak.voice_ref.is_empty());
    }

    #[test]
    fn a_stereo_clip_enrolls_with_a_warning() {
        let (_dir, paths, cfg) = setup();
        let clip = _dir.path().join("stereo.wav");
        let spec = hound::WavSpec {
            channels: 2,
            sample_rate: 44_100,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let mut w = hound::WavWriter::create(&clip, spec).unwrap();
        for _ in 0..44_100 * 7 {
            w.write_sample(1000i16).unwrap();
            w.write_sample(-1000i16).unwrap();
        }
        w.finalize().unwrap();
        let mock = MockTtsProvider::new();
        let out = enroll(&cfg, &paths, &enroll_args(&clip, None), &mock).unwrap();
        assert!(out.contains("mono 16 kHz"), "{out}");
        assert_eq!(mock.enrollments().len(), 1);
    }

    #[test]
    fn speak_without_out_says_playback_is_108() {
        let (_dir, paths, cfg) = setup();
        let mut cfg = cfg;
        cfg.speak.voice_ref = "/data/voice.wav".into();
        let mock = MockTtsProvider::new();
        let err = speak(&cfg, &paths, "hola", None, &mock).unwrap_err();
        assert!(matches!(err, SpeakError::NoPlayback), "{err}");
        assert!(err.to_string().contains("#108"), "{err}");
        assert!(mock.requests().is_empty());
    }

    #[test]
    fn speak_without_an_enrolled_voice_points_at_enroll() {
        let (_dir, paths, cfg) = setup();
        let mock = MockTtsProvider::new();
        let out = _dir.path().join("x.wav");
        let err = speak(&cfg, &paths, "hola", Some(&out), &mock).unwrap_err();
        assert!(matches!(err, SpeakError::NotEnrolled), "{err}");
        assert!(err.to_string().contains("enroll"), "{err}");
    }

    #[test]
    fn speak_writes_a_parseable_wav_in_the_enrolled_voice() {
        let (_dir, paths, cfg) = setup();
        let mut cfg = cfg;
        cfg.speak.voice_ref = paths.data_dir.join("voice.wav").display().to_string();
        let mock = MockTtsProvider::new();
        let out = _dir.path().join("speech.wav");
        let msg = speak(&cfg, &paths, "Buenas tardes.", Some(&out), &mock).unwrap();
        assert!(msg.contains("24000 Hz"), "{msg}");

        let reader = hound::WavReader::open(&out).unwrap();
        let spec = reader.spec();
        assert_eq!(spec.channels, 1);
        assert_eq!(spec.sample_rate, 24_000);
        assert_eq!(spec.bits_per_sample, 16);
        assert_eq!(reader.duration(), 12_000, "0.5 s of sine");

        let req = &mock.requests()[0];
        assert_eq!(req.text, "Buenas tardes.");
        assert_eq!(req.voice, "voice", "derived from voice_ref's file name");
    }

    #[test]
    fn speak_surfaces_an_unreachable_server() {
        let (_dir, paths, cfg) = setup();
        let mut cfg = cfg;
        cfg.speak.voice_ref = "/data/voice.wav".into();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        cfg.speak.tts_url = format!("http://127.0.0.1:{port}");
        let provider = dmn_tts::ChatterboxProvider::new(&cfg.speak.tts_url);
        let out = _dir.path().join("x.wav");
        let err = speak(&cfg, &paths, "hola", Some(&out), &provider).unwrap_err();
        assert!(matches!(err, SpeakError::Tts(TtsError::Unreachable { .. })), "{err}");
        assert!(!out.exists(), "no partial file left");
    }
}
