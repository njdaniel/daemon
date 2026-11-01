//! `dmn`: the Daemon command-line entry point.
//!
//! Subcommands arrive with their issues. Now: `doctor` (#2, #6), `doctor --probe` (#5),
//! `doctor --fetch` and `bench` (#8), `config show` (#2), `devices` (#4), `replay` (#9),
//! `sessions list|export|purge` (#13), `run --replay` (#15), live `run` (#16),
//! `sources add|list|rm` (#53), `speak` (#107).

// CLAUDE.md: no unwrap/expect outside tests and main.
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

mod bench;
mod bench_live;
mod context;
mod design;
mod devices;
mod doctor;
mod latency;
mod pipeline;
mod probe;
mod replay;
mod run;
mod sources;
mod speak;
mod speaker;
mod suggest;
#[cfg(test)]
mod testollama;
mod today;
mod tui;

use std::process::ExitCode;

use clap::{Parser, Subcommand};
use dmn_core::config::ConfigError;
use dmn_core::logging::LogError;
use dmn_core::{Config, Paths};

/// Everything `dmn` can fail with. One enum per crate, like every other crate here
/// (CLAUDE.md); `main` prints it and exits 1.
#[derive(Debug, thiserror::Error)]
enum CliError {
    #[error("no home directory: cannot resolve XDG paths")]
    NoHome,
    #[error(transparent)]
    Config(#[from] ConfigError),
    #[error(transparent)]
    Log(#[from] LogError),
    /// `dmn devices` could not talk to PipeWire.
    #[error("{0}")]
    Devices(String),
    #[error(transparent)]
    Stt(#[from] dmn_stt::SttError),
    #[error(transparent)]
    Silero(dmn_vad::SileroError),
    /// `dmn bench` could not read a fixture or its turns.
    #[error("bench: {0}")]
    Bench(String),
    /// `dmn replay` could not run the pipeline over the given files.
    #[error("{0}")]
    Replay(String),
    /// `dmn run` could not start or ended with a pipeline failure.
    #[error("{0}")]
    Run(String),
    #[error(transparent)]
    Sessions(#[from] dmn_core::recorder::RecorderError),
    #[error(transparent)]
    Sources(#[from] sources::SourcesError),
    #[error(transparent)]
    Speak(#[from] speak::SpeakError),
    /// A command-line value clap could not check on its own.
    #[error("{0}")]
    Usage(String),
    #[error("{failed} required check(s) failed")]
    ChecksFailed { failed: usize },
}

/// `--vad` (#62): which voice activity detector, over `[stt].vad`.
#[derive(clap::ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
enum VadArg {
    Energy,
    Silero,
}

impl From<VadArg> for dmn_core::config::Vad {
    fn from(v: VadArg) -> Self {
        match v {
            VadArg::Energy => dmn_core::config::Vad::Energy,
            VadArg::Silero => dmn_core::config::Vad::Silero,
        }
    }
}

/// `--question-source` (#90): whose questions are detected and answered, over
/// `[detect].question_source`. clap limits the value to these three names.
#[derive(clap::ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
enum QuestionSourceArg {
    /// `interviewer`, the name before #128, still works and is not listed.
    #[value(alias = "interviewer")]
    Them,
    You,
    Both,
}

impl From<QuestionSourceArg> for dmn_core::config::QuestionSource {
    fn from(q: QuestionSourceArg) -> Self {
        match q {
            QuestionSourceArg::Them => dmn_core::config::QuestionSource::Them,
            QuestionSourceArg::You => dmn_core::config::QuestionSource::You,
            QuestionSourceArg::Both => dmn_core::config::QuestionSource::Both,
        }
    }
}

/// `--device` (#61): where whisper runs, over `[stt].device`.
#[derive(clap::ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
enum DeviceArg {
    Cpu,
    Gpu,
    Auto,
}

impl From<DeviceArg> for dmn_core::config::SttDevice {
    fn from(d: DeviceArg) -> Self {
        match d {
            DeviceArg::Cpu => dmn_core::config::SttDevice::Cpu,
            DeviceArg::Gpu => dmn_core::config::SttDevice::Gpu,
            DeviceArg::Auto => dmn_core::config::SttDevice::Auto,
        }
    }
}

/// Daemon: a Linux-native interview copilot.
#[derive(Parser, Debug)]
#[command(name = "dmn", version, about, long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Run the copilot in this terminal: live question, Approach, transcript and status.
    /// Captures the call live (your mic, the call app's output) until q.
    /// Keys: p pin/unpin, r regenerate, m mute mic, s coding solutions hints/full, w swap the
    /// STAR story, 1–6 sections, Ctrl+Shift+A assistance on/off, q quit
    Run {
        /// Feed the pipeline from two WAV files at 1× instead of live capture
        #[arg(long, num_args = 2, value_names = ["THEM", "YOU"])]
        replay: Option<Vec<std::path::PathBuf>>,
        /// Live: your mic's PipeWire node.name, or "default"; overrides [audio].mic.
        /// `dmn devices` lists them
        #[arg(long, conflicts_with = "replay")]
        mic: Option<String>,
        /// Live: the call app's application.name (e.g. Firefox, Chromium), or "" for all system
        /// audio; overrides [audio].app_stream. `dmn devices` lists the apps playing
        #[arg(long, conflicts_with = "replay")]
        app: Option<String>,
        /// Model name; default is [stt].model from the config
        #[arg(long)]
        model: Option<String>,
        /// Decoder threads; default is [stt].threads from the config
        #[arg(long)]
        threads: Option<usize>,
        /// Silence (ms) that ends an utterance; default is [stt].vad_close_ms (500)
        #[arg(long)]
        vad_close_ms: Option<u16>,
        /// Voice activity detector; default is [stt].vad (energy)
        #[arg(long, value_enum)]
        vad: Option<VadArg>,
        /// Where whisper runs; default is [stt].device. gpu needs a --features vulkan|cuda build
        #[arg(long, value_enum)]
        device: Option<DeviceArg>,
        /// The audio's language: en, auto (detect and translate to English) or a code such as
        /// es; default is [stt].language. Anything but en needs a multilingual model (small)
        #[arg(long)]
        language: Option<dmn_core::config::SttLanguage>,
        /// Whose questions are detected and answered: them (default), you or both;
        /// overrides [detect].question_source. Transcript labels and audio routing never change
        #[arg(long, value_enum)]
        question_source: Option<QuestionSourceArg>,
        /// Keep the session in memory: write nothing to ~/.local/share/dmn/dmn.sqlite
        #[arg(long)]
        no_store: bool,
        /// No terminal UI. With --replay: print how long each scripted question in the
        /// fixture's questions.json took to reach the screen (p50 / p95). Live: capture for
        /// --duration seconds, then print the summary (`dmn bench --live` reads the rest)
        #[arg(long)]
        headless: bool,
        /// Live: stop after this many seconds, as if q had been pressed
        #[arg(long, value_name = "SECS", conflicts_with = "replay")]
        duration: Option<u64>,
        /// Coding questions: "hints" (insight, edge cases, complexity; no complete code) or
        /// "full" (a worked solution too) in Deeper Answer; overrides [ui].coding_solutions for
        /// this run. `s` toggles it during the call
        #[arg(long, value_name = "MODE", value_parser = ["hints", "full"])]
        solutions: Option<String>,
        /// Start with interview assistance off: transcribe (and translate) only, no question
        /// detection and no LLM request, the transcript on the whole screen; a local Ollama model
        /// is unloaded so whisper can have the GPU (with [speak] on, kept for `t` unless
        /// [speak].model names a translator of its own). Overrides [assist].enabled; Ctrl+Shift+A turns it on
        #[arg(long)]
        no_assist: bool,
    },
    /// Check the machine and, with --init, write a commented config file
    Doctor {
        /// Write ~/.config/dmn/config.toml with comments and defaults
        #[arg(long)]
        init: bool,
        /// Overwrite an existing config file (only with --init)
        #[arg(long, requires = "init")]
        force: bool,
        /// Also play a quiet 440 Hz tone and capture it back through the app stream, the sink
        /// monitor and pw-record (about 3 s of sound through the default output)
        #[arg(long, conflicts_with = "init")]
        probe: bool,
        /// Download a whisper model (tiny.en, base.en, small.en, medium.en; multilingual: tiny,
        /// base, small, medium, large-v3, large-v3-turbo) or the Silero VAD model (silero) into
        /// ~/.cache/dmn/models/, then run the checks
        #[arg(long, value_name = "MODEL", conflicts_with = "init")]
        fetch: Option<String>,
    },
    /// Measure whisper speed and accuracy on fixture Them turns; with --live, report
    /// question end → Approach latency (p50 / p95) and CPU use from a stored session
    Bench {
        /// Read the session log instead: latency and CPU of the latest session (or --session)
        #[arg(long)]
        live: bool,
        /// With --live: the session id (`dmn sessions list`); default the latest
        #[arg(long, requires = "live")]
        session: Option<i64>,
        /// With --live: pair candidates with this questions.json; a replay session finds the
        /// one beside its Them file by itself
        #[arg(long, requires = "live")]
        questions: Option<std::path::PathBuf>,
        /// Model name; default is [stt].model from the config
        #[arg(long)]
        model: Option<String>,
        /// Fixture under testdata/ (sysdesign, behavioral, mixed)
        #[arg(long, default_value = "sysdesign")]
        fixture: String,
        /// Decoder threads; default is [stt].threads from the config
        #[arg(long)]
        threads: Option<usize>,
        /// Number of Them turns to transcribe
        #[arg(long, default_value_t = 5)]
        turns: usize,
        /// Use whisper's fixed 30 s encoder window instead of trimming it to each utterance
        #[arg(long)]
        full_window: bool,
        /// Where whisper runs; default is [stt].device. gpu needs a --features vulkan|cuda build
        #[arg(long, value_enum)]
        device: Option<DeviceArg>,
        /// The audio's language: en, auto (detect and translate to English) or a code such as
        /// es; default is [stt].language. Anything but en needs a multilingual model (small)
        #[arg(long)]
        language: Option<dmn_core::config::SttLanguage>,
        /// Directory holding the fixtures
        #[arg(long, default_value = "testdata")]
        testdata: std::path::PathBuf,
    },
    /// Inspect configuration
    Config {
        #[command(subcommand)]
        action: ConfigAction,
    },
    /// List PipeWire capture sources and application output streams
    Devices,
    /// List, export or purge stored sessions
    Sessions {
        #[command(subcommand)]
        action: SessionsAction,
    },
    /// Speak text in your enrolled voice clone through the local TTS server (#107) into the
    /// virtual microphone dmn-voice the call selects (#108): `dmn speak "text"`, or
    /// `dmn speak enroll --wav <clip>` first
    #[command(args_conflicts_with_subcommands = true)]
    Speak {
        #[command(subcommand)]
        action: Option<SpeakAction>,
        /// The text to synthesize
        text: Option<String>,
        /// Write the synthesized speech to this WAV file instead of playing it into dmn-voice
        #[arg(long)]
        out: Option<std::path::PathBuf>,
        /// Speak in this voice from the TTS server's library (`dmn speak voices`) instead of
        /// your own; with --out, a way to hear a designed voice
        #[arg(long)]
        voice: Option<String>,
    },
    /// Add, list or remove context sources: resume, job description, notes, stories
    Sources {
        #[command(subcommand)]
        action: sources::SourcesAction,
    },
    /// Run two fixture WAVs through the real pipeline and print labelled transcript segments
    Replay {
        /// The other person's audio
        them: std::path::PathBuf,
        /// Your audio
        #[arg(value_name = "YOU")]
        candidate: std::path::PathBuf,
        /// Paces delivery at N× wall-clock; 1 is a live call. Must be positive and finite
        #[arg(long, default_value_t = 1.0)]
        speed: f32,
        /// Model name; default is [stt].model from the config
        #[arg(long)]
        model: Option<String>,
        /// Decoder threads; default is [stt].threads from the config
        #[arg(long)]
        threads: Option<usize>,
        /// Silence (ms) that ends an utterance; default is [stt].vad_close_ms (500)
        #[arg(long)]
        vad_close_ms: Option<u16>,
        /// Voice activity detector; default is [stt].vad (energy)
        #[arg(long, value_enum)]
        vad: Option<VadArg>,
        /// Where whisper runs; default is [stt].device. gpu needs a --features vulkan|cuda build
        #[arg(long, value_enum)]
        device: Option<DeviceArg>,
        /// The audio's language: en, auto (detect and translate to English) or a code such as
        /// es; default is [stt].language. Anything but en needs a multilingual model (small)
        #[arg(long)]
        language: Option<dmn_core::config::SttLanguage>,
        /// Transcribe the 1 s partials of an open utterance: on, since --speed always paces
        #[arg(long, value_enum, default_value_t = pipeline::Partials::Auto)]
        partials: pipeline::Partials,
        /// Print the merged transcript and the last 60 s window at the end
        #[arg(long)]
        transcript: bool,
        /// Print one JSON object per line (segments, candidates, summary) for eval/run.py
        #[arg(long, conflicts_with = "transcript")]
        json: bool,
        /// Score questions with the SPEC's cue table exactly as written: no question-form gate
        /// and no vetoes on rhetorical tags or small talk
        #[arg(long)]
        spec_literal_detector: bool,
        /// Whose questions are detected and answered: them (default), you or both;
        /// overrides [detect].question_source. Transcript labels and audio routing never change
        #[arg(long, value_enum)]
        question_source: Option<QuestionSourceArg>,
        /// Skip the LLM fast phase; by default each candidate goes to [llm] (Ollama) and the
        /// question + approach JSON is printed
        #[arg(long)]
        no_llm: bool,
        /// Keep the session in memory: write nothing to ~/.local/share/dmn/dmn.sqlite. By
        /// default a replay writes the same session log a live call would
        #[arg(long)]
        no_store: bool,
    },
}

#[derive(Subcommand, Debug)]
enum SpeakAction {
    /// Enroll a reference clip: validate it, register it with the TTS server, store a copy
    /// under the data dir and record it as [speak].voice_ref (and, with --language, as that
    /// language's entry in [speak].languages, the list `v` picks from in dmn run)
    Enroll {
        /// The reference clip: a WAV of your voice, at least 6 s, mono 16 kHz+ preferred
        #[arg(long)]
        wav: std::path::PathBuf,
        /// Voice name on the server and for the stored clip (letters, digits, - and _);
        /// default "voice", or "voice-<code>" with --language. Enrolling a name again
        /// replaces that voice
        #[arg(long)]
        name: Option<String>,
        /// The language this voice will speak (en, es, fr, ...); default: the server's (en).
        /// chatterbox-tts-api speaks each voice in the language it was enrolled with
        #[arg(long)]
        language: Option<String>,
    },
    /// List the TTS server's voices: name, designed or enrolled, the language each speaks
    /// under Chatterbox, which are your own and which is [speak].voice (#130)
    Voices,
    /// Make a voice from a description (#130): start the Qwen3-TTS VoiceDesign server
    /// (sidecars/qwen3-tts, profile design), render a sample line, play it, ask whether to
    /// keep it, enroll it into the Base server under --name for the `o` picker, and stop the
    /// design server again. Needs [speak].engine = "qwen3"
    Design {
        /// The new voice's name (letters, digits, - and _); not one of your own voices
        #[arg(long)]
        name: String,
        /// The voice in words, its audible traits: "grumpy old pirate, gravelly, slow"
        #[arg(long)]
        describe: String,
        /// What the sample says; default: a few sentences in English (8-10 s of speech, enough
        /// to clone from). The Base model clones the voice from this sample
        #[arg(long)]
        line: Option<String>,
        /// The sample's language (en, de, zh, ...); default: the model's guess from the line
        #[arg(long)]
        language: Option<String>,
        /// Write the sample to this WAV instead of playing it
        #[arg(long)]
        out: Option<std::path::PathBuf>,
        /// Keep the sample without asking (for scripts)
        #[arg(long)]
        yes: bool,
    },
}

#[derive(Subcommand, Debug)]
enum SessionsAction {
    /// List stored sessions: id, start time, label, row counts
    List,
    /// Export a stored session under ~/.local/share/dmn/sessions/ as JSONL or readable text
    Export {
        /// Session id (from `dmn sessions list`)
        id: i64,
        /// jsonl: all stored rows; text: finalized speech with timestamps and speaker labels
        #[arg(long, value_enum, default_value_t = ExportFormatArg::Jsonl)]
        format: ExportFormatArg,
    },
    /// Delete sessions: their rows, their files under sessions/, and the free pages that held
    /// them
    Purge {
        /// A session id, or `all`
        target: String,
    },
}

#[derive(clap::ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
enum ExportFormatArg {
    Jsonl,
    Text,
}

impl From<ExportFormatArg> for dmn_core::recorder::ExportFormat {
    fn from(format: ExportFormatArg) -> Self {
        match format {
            ExportFormatArg::Jsonl => Self::Jsonl,
            ExportFormatArg::Text => Self::Text,
        }
    }
}

#[derive(Subcommand, Debug)]
enum ConfigAction {
    /// Print the effective configuration as TOML (file values over defaults)
    Show,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("dmn: {e}");
            ExitCode::FAILURE
        }
    }
}

fn run(cli: Cli) -> Result<(), CliError> {
    let paths = Paths::from_xdg().ok_or(CliError::NoHome)?;
    if let Err(e) = dmn_core::logging::init(&paths.log_file) {
        // Logging is best-effort for CLI commands; the file is the only sink during a call.
        eprintln!("dmn: logging disabled: {e}");
    }

    match cli.command {
        None => {
            println!(
                "dmn {}: run `dmn --help`; config at {}",
                env!("CARGO_PKG_VERSION"),
                paths.config_file.display()
            );
            Ok(())
        }
        Some(Command::Doctor {
            init,
            force,
            probe,
            fetch,
        }) => {
            if let Some(name) = fetch {
                fetch_model(&paths, &name)?;
            }
            if init {
                Config::write_template(&paths.config_file, force)?;
                tracing::info!(path = %paths.config_file.display(), "wrote config template");
                println!("wrote {}", paths.config_file.display());
            } else {
                let cfg = Config::load(&paths.config_file)?;
                let mut report = doctor::run_all(
                    &cfg,
                    &paths,
                    &doctor::SystemRunner,
                    &doctor::StdHttp,
                    &doctor::SystemResolver,
                );
                if probe {
                    report.checks.extend(probe::run());
                }
                print!("{}", report.render());
                let failed = report.failed_required();
                tracing::info!(failed, "doctor");
                if failed > 0 {
                    return Err(CliError::ChecksFailed { failed });
                }
            }
            Ok(())
        }
        Some(Command::Replay {
            them,
            candidate,
            speed,
            model,
            threads,
            vad_close_ms,
            vad,
            device,
            language,
            partials,
            transcript,
            json,
            spec_literal_detector,
            question_source,
            no_llm,
            no_store,
        }) => {
            let cfg = Config::load(&paths.config_file)?;
            // As with bench above, the module keeps its boxed error and it is mapped here.
            replay::run(
                &cfg,
                &paths,
                &replay::Args {
                    them,
                    candidate,
                    speed,
                    model,
                    threads,
                    vad_close_ms,
                    vad: vad.map(Into::into),
                    device: device.map(Into::into),
                    language,
                    partials,
                    transcript,
                    json,
                    spec_literal_detector,
                    question_source: question_source.map(Into::into),
                    no_llm,
                    no_store,
                },
            )
            .map_err(|e| CliError::Replay(e.to_string()))
        }
        Some(Command::Run {
            replay,
            mic,
            app,
            model,
            threads,
            vad_close_ms,
            vad,
            device,
            language,
            question_source,
            no_store,
            headless,
            duration,
            solutions,
            no_assist,
        }) => {
            let cfg = Config::load(&paths.config_file)?;
            // clap's value_parser has already limited it to the two labels.
            let solutions = solutions
                .as_deref()
                .and_then(dmn_session::CodingSolutions::from_label);
            let replay = match replay.as_deref() {
                None => None,
                Some([them, candidate]) => Some((them.clone(), candidate.clone())),
                Some(_) => {
                    return Err(CliError::Usage(
                        "run --replay takes two files: <them.wav> <you.wav>".into(),
                    ));
                }
            };
            run::run(
                &cfg,
                &paths,
                &run::Args {
                    replay,
                    mic,
                    app,
                    model,
                    threads,
                    vad_close_ms,
                    vad: vad.map(Into::into),
                    device: device.map(Into::into),
                    language,
                    question_source: question_source.map(Into::into),
                    no_store,
                    headless,
                    duration,
                    solutions,
                    no_assist,
                },
            )
            .map_err(|e| CliError::Run(e.to_string()))
        }
        Some(Command::Sessions { action }) => sessions(&paths, action),
        Some(Command::Sources { action }) => Ok(sources::run(&paths, action)?),
        Some(Command::Speak {
            action,
            text,
            out,
            voice,
        }) => {
            let cfg = Config::load(&paths.config_file)?;
            match action {
                Some(SpeakAction::Enroll {
                    wav,
                    name,
                    language,
                }) => {
                    let args = speak::EnrollArgs {
                        wav,
                        name,
                        language,
                    };
                    let provider = speak::tts_provider(&cfg, false);
                    print!("{}", speak::enroll(&cfg, &paths, &args, provider.as_ref())?);
                }
                Some(SpeakAction::Voices) => {
                    let provider = speak::tts_provider(&cfg, false);
                    print!("{}", speak::voices(&cfg, &paths, provider.as_ref())?);
                }
                Some(SpeakAction::Design {
                    name,
                    describe,
                    line,
                    language,
                    out,
                    yes,
                }) => {
                    let args = design::DesignArgs {
                        name,
                        describe,
                        line,
                        language,
                        out,
                        yes,
                        ready_within: design::READY_WITHIN,
                    };
                    let base = speak::tts_provider(&cfg, false);
                    let designer = dmn_tts::Qwen3Designer::new(&cfg.speak.design_url);
                    let interrupt = std::sync::Arc::new(design::Interrupt::default());
                    design::watch_signals(std::sync::Arc::clone(&interrupt))?;
                    print!(
                        "{}",
                        design::design(
                            &cfg,
                            &paths,
                            &args,
                            &design::Docker,
                            base.as_ref(),
                            &designer,
                            &interrupt
                        )?
                    );
                }
                None => {
                    let text = text.ok_or(CliError::Usage(
                        "speak needs the text to synthesize: dmn speak \"text\" [--out <file.wav>] (or `dmn speak enroll --wav <clip>`)".into(),
                    ))?;
                    // Playback takes the streaming route, so the first sentence plays while
                    // the server renders the next; a file is written from the whole WAV.
                    let provider = speak::tts_provider(&cfg, out.is_none());
                    print!(
                        "{}",
                        speak::speak(
                            &cfg,
                            &paths,
                            &text,
                            out.as_deref(),
                            voice.as_deref(),
                            provider.as_ref()
                        )?
                    );
                }
            }
            Ok(())
        }
        Some(Command::Config {
            action: ConfigAction::Show,
        }) => {
            let cfg = Config::load(&paths.config_file)?;
            tracing::debug!(provider = ?cfg.llm.provider, "config show");
            // SPEC § Providers: in words, as a TOML comment so the output still parses.
            if let Some(notice) = cfg.llm.privacy_notice() {
                println!("# NOTE: {notice}");
            }
            // #102: and what the web lookup sends, while it is on.
            if let Some(notice) = cfg.lookup.privacy_notice() {
                println!("# NOTE: {notice}");
            }
            // #102: values that depend on another (the lookup's timeout) as they take effect.
            print!("{}", cfg.effective().to_toml()?);
            Ok(())
        }
        Some(Command::Bench {
            live: true,
            session,
            questions,
            ..
        }) => {
            print!(
                "{}",
                bench_live::run(&paths, &bench_live::Args { session, questions })
                    .map_err(CliError::Bench)?
            );
            Ok(())
        }
        Some(Command::Bench {
            model,
            fixture,
            threads,
            turns,
            full_window,
            testdata,
            device,
            language,
            ..
        }) => {
            let cfg = Config::load(&paths.config_file)?;
            let args = bench::BenchArgs {
                model: model.unwrap_or_else(|| cfg.stt.model.clone()),
                fixture,
                threads: threads.unwrap_or(usize::from(cfg.stt.threads)),
                turns,
                trim_audio_ctx: !full_window,
                device: device.map(Into::into).unwrap_or(cfg.stt.device),
                language: language.unwrap_or_else(|| cfg.stt.language.clone()),
                testdata,
                models_dir: paths.models_dir.clone(),
            };
            // bench::run belongs to issue #9's files; its boxed error is mapped here rather
            // than restructuring that module from this PR.
            print!(
                "{}",
                bench::run(&args).map_err(|e| CliError::Bench(e.to_string()))?
            );
            Ok(())
        }
        Some(Command::Devices) => {
            let cfg = Config::load(&paths.config_file)?;
            print!("{}", devices::run(&cfg).map_err(CliError::Devices)?);
            Ok(())
        }
    }
}

/// `dmn doctor --fetch silero` (#62): the Silero VAD model, then its size and SHA-256.
fn fetch_silero(paths: &Paths) -> Result<(), CliError> {
    use dmn_vad::silero;
    let dest = silero::model_path(&paths.models_dir);
    eprintln!(
        "fetching {} ({:.1} MB) from {}",
        silero::MODEL_FILE,
        silero::MODEL_SIZE as f64 / 1e6,
        silero::MODEL_URL
    );
    let path = dmn_stt::download_file(
        &dest,
        silero::MODEL_URL,
        silero::MODEL_SIZE,
        Some(silero::MODEL_SHA256),
        dmn_stt::model::MIN_DEADLINE,
        |_, _| {},
    )?;
    if let Err(e) = silero::verify_model(&path) {
        // Right size, wrong bytes: never leave a file that ONNX Runtime would be asked to parse.
        let _ = std::fs::remove_file(&path);
        return Err(CliError::Silero(e));
    }
    tracing::info!(path = %path.display(), "silero model ready");
    println!(
        "model:    {} ({} bytes, sha256 ok)",
        path.display(),
        silero::MODEL_SIZE
    );
    Ok(())
}

/// `dmn doctor --fetch <model>`: download with a progress line on stderr.
fn fetch_model(paths: &Paths, name: &str) -> Result<(), CliError> {
    if name == dmn_vad::silero::FETCH_NAME {
        return fetch_silero(paths);
    }
    let model = dmn_stt::Model::from_name(name)?;
    let total_mb = model.size_bytes() as f64 / 1e6;
    eprintln!(
        "fetching {} ({total_mb:.0} MB) from {}",
        model.file_name(),
        dmn_stt::model::BASE_URL
    );
    let mut last_pct = u64::MAX;
    let path = dmn_stt::download_model(
        &paths.models_dir,
        model,
        dmn_stt::model::BASE_URL,
        |done, total| {
            let pct = done * 100 / total.max(1);
            if pct != last_pct && pct % 10 == 0 {
                eprintln!("  {pct:>3} %");
                last_pct = pct;
            }
        },
    )?;
    let len = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
    tracing::info!(path = %path.display(), bytes = len, "whisper model ready");
    // #84: `download_model` checked the SHA-256 before the file was put in place (or re-hashed
    // the one already there), so reaching this line means it verified.
    println!("model:    {} ({len} bytes, sha256 ok)", path.display());
    Ok(())
}

/// `dmn sessions ...` (#13).
fn sessions(paths: &Paths, action: SessionsAction) -> Result<(), CliError> {
    use dmn_core::recorder::{self, PurgeTarget};
    match action {
        SessionsAction::List => {
            if !paths.db_file.is_file() {
                println!(
                    "no sessions yet ({} does not exist)",
                    paths.db_file.display()
                );
                return Ok(());
            }
            let store = recorder::open_store(paths)?;
            let err = |source| recorder::RecorderError::Store {
                path: paths.db_file.clone(),
                source,
            };
            let ids = store.session_ids().map_err(err)?;
            if ids.is_empty() {
                println!("no sessions");
            }
            for id in ids {
                let Some(s) = store.session(id).map_err(err)? else {
                    continue;
                };
                let segments = store.segments(id).map_err(err)?.len();
                let questions = store.questions(id).map_err(err)?;
                let answered = questions.iter().filter(|q| q.text.is_some()).count();
                println!(
                    "#{id:<4} started {} ({} s){}  {segments} segments, {} questions ({answered} answered)  {}",
                    s.started_at,
                    s.ended_at.map_or(0, |e| e - s.started_at),
                    if s.ended_at.is_none() { " open" } else { "" },
                    questions.len(),
                    s.label
                );
            }
            Ok(())
        }
        SessionsAction::Export { id, format } => {
            let (path, count) = recorder::export_with_format(paths, id, format.into())?;
            let unit = match format {
                ExportFormatArg::Jsonl => "line(s)",
                ExportFormatArg::Text => "segment(s)",
            };
            println!("wrote {count} {unit} to {}", path.display());
            Ok(())
        }
        SessionsAction::Purge { target } => {
            let target = if target == "all" {
                PurgeTarget::All
            } else {
                PurgeTarget::One(target.parse().map_err(|_| {
                    CliError::Usage(format!(
                        "sessions purge: expected a session id or `all`, not {target:?}"
                    ))
                })?)
            };
            let report = recorder::purge(paths, target)?;
            tracing::info!(
                sessions = report.sessions.len(),
                files = report.files.len(),
                "sessions purge"
            );
            println!(
                "purged {} session(s) and {} file(s)",
                report.sessions.len(),
                report.files.len()
            );
            Ok(())
        }
    }
}

#[cfg(test)]
mod sessions_arg_tests {
    use super::*;

    #[test]
    fn session_export_defaults_to_jsonl_and_accepts_explicit_formats() {
        for (extra, expected) in [
            (vec![], ExportFormatArg::Jsonl),
            (vec!["--format", "jsonl"], ExportFormatArg::Jsonl),
            (vec!["--format", "text"], ExportFormatArg::Text),
        ] {
            let mut args = vec!["dmn", "sessions", "export", "42"];
            args.extend(extra);
            let parsed = Cli::try_parse_from(args).unwrap();
            let Some(Command::Sessions {
                action: SessionsAction::Export { id, format },
            }) = parsed.command
            else {
                panic!("expected sessions export");
            };
            assert_eq!(id, 42);
            assert_eq!(format, expected);
        }
        assert!(
            Cli::try_parse_from(["dmn", "sessions", "export", "42", "--format", "pdf"]).is_err()
        );
    }
}

#[cfg(test)]
mod question_source_arg_tests {
    use clap::ValueEnum;

    use super::QuestionSourceArg;

    #[test]
    fn interviewer_still_means_them_and_is_not_listed() {
        assert_eq!(
            QuestionSourceArg::from_str("interviewer", false),
            Ok(QuestionSourceArg::Them)
        );
        assert_eq!(
            QuestionSourceArg::from_str("them", false),
            Ok(QuestionSourceArg::Them)
        );
        let listed: Vec<String> = QuestionSourceArg::value_variants()
            .iter()
            .filter_map(|v| v.to_possible_value())
            .filter(|p| !p.is_hide_set())
            .map(|p| p.get_name().to_owned())
            .collect();
        assert_eq!(listed, ["them", "you", "both"]);
    }
}
