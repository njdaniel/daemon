//! `dmn speak` (#107): enroll a reference clip and synthesize text in the cloned voice through
//! the local TTS server (chatterbox-tts-api, `[speak].tts_url`). Nothing plays audio yet:
//! without `--out` the speak command says so (playback is #108). Both subcommands refuse to run
//! while `[speak].enabled = false`, and while `tts_url` is not on this machine.
//!
//! Audio on disk (CLAUDE.md: never unless `privacy.record_audio`): that rule is about the
//! call. These commands write audio only where the user asks by name, in the same command:
//! `--out <file>` writes the synthesized speech there, and `enroll --wav <clip>` copies the
//! user's own clip to `<data dir>/<name>.wav` (the file `[speak].voice_ref` names, kept so the
//! voice can be enrolled again after the server's library is reset). Nothing from the call is
//! recorded, and nothing is written that the command line did not name.

use std::path::{Path, PathBuf};

use dmn_core::Paths;
use dmn_core::config::{Config, ConfigError};
use dmn_tts::{SpeakRequest, TtsError, TtsProvider};

/// A reference clip needs at least this much clean speech for the clone to hold up.
const MIN_CLIP_SECS: f64 = 6.0;
/// chatterbox-tts-api refuses voice files over 10 MB (`voices.py`); say so before uploading.
const MAX_CLIP_BYTES: u64 = 10 * 1024 * 1024;
/// The default voice name, and so the stored clip `voice.wav` the issue names.
const DEFAULT_NAME: &str = "voice";
const MAX_NAME: usize = 64;

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
        "nothing plays audio yet: playback lands in #108; pass --out <file.wav> to write the synthesized speech to a file"
    )]
    NoPlayback,
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
fn check_gates(cfg: &Config, paths: &Paths) -> Result<(), SpeakError> {
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
fn tts_error(e: TtsError, paths: &Paths) -> SpeakError {
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
fn voice_name(voice_ref: &str) -> String {
    Path::new(voice_ref)
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| DEFAULT_NAME.to_owned())
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
/// registration changes nothing on disk.
pub fn enroll(
    cfg: &Config,
    paths: &Paths,
    args: &EnrollArgs,
    provider: &dyn TtsProvider,
) -> Result<String, SpeakError> {
    check_gates(cfg, paths)?;
    let name = args.name.as_deref().unwrap_or(DEFAULT_NAME);
    check_name(name)?;
    let note = validate_clip(&args.wav)?;
    let enrolled = rt()?
        .block_on(provider.enroll(name, &args.wav, args.language.as_deref()))
        .map_err(|e| tts_error(e, paths))?;
    let dest = paths.data_dir.join(format!("{name}.wav"));
    store_clip(&args.wav, &dest)?;
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
        args.language
            .as_deref()
            .map(|l| format!(", speaking {l}"))
            .unwrap_or_default(),
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
    check_gates(cfg, paths)?;
    if cfg.speak.voice_ref.is_empty() {
        return Err(SpeakError::NotEnrolled {
            config: paths.config_file.display().to_string(),
        });
    }
    let out = out.ok_or(SpeakError::NoPlayback)?;
    if text.trim().is_empty() {
        return Err(SpeakError::EmptyText);
    }
    let voice = voice_name(&cfg.speak.voice_ref);
    let req = SpeakRequest::new(text.trim(), voice.clone());
    let started = std::time::Instant::now();
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
    let frames = write_wav(out, stream, paths)?;
    let secs = frames as f64 / f64::from(sample_rate);
    tracing::info!(voice = %voice, path = %out.display(), frames, "spoke");
    Ok(format!(
        "wrote {secs:.1} s of speech at {sample_rate} Hz in voice {voice:?} to {} ({:.1} s)\n",
        out.display(),
        started.elapsed().as_secs_f64()
    ))
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
    /// `<data>/voice.wav`, and `voice_ref` set in the user's file with its comments kept.
    #[test]
    fn enroll_registers_stores_and_records_the_clip() {
        let server = serve(vec![Reply::new(
            "201 Created",
            "application/json",
            r#"{"voice":{"name":"voice","language":"es"}}"#,
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
        assert!(out.contains("enrolled \"voice\""), "{out}");
        assert!(out.contains("speaking es"), "{out}");

        assert_eq!(server.request_lines(), ["POST /v1/voices"]);
        let requests = server.requests();
        let (_, body) = MockServer::head_and_body(&requests[0]);
        let text = String::from_utf8_lossy(body);
        assert!(
            text.contains("name=\"voice_name\"\r\n\r\nvoice\r\n"),
            "{text}"
        );
        assert!(text.contains("name=\"language\"\r\n\r\nes\r\n"), "{text}");

        let stored = paths.data_dir.join("voice.wav");
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
        assert!(cfg.lookup.enabled);
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

    #[test]
    fn speak_without_out_says_playback_is_108() {
        let (_dir, paths, mut cfg) = setup("http://localhost:4123");
        cfg.speak.voice_ref = "/data/voice.wav".into();
        let mock = MockTtsProvider::new();
        let err = speak(&cfg, &paths, "hola", None, &mock).unwrap_err();
        assert!(matches!(err, SpeakError::NoPlayback), "{err}");
        assert!(err.to_string().contains("playback lands in #108"), "{err}");
        assert!(mock.requests().is_empty());
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
