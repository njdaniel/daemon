//! `dmn`: the Daemon command-line entry point.
//!
//! Subcommands arrive with their issues. Now: `doctor` (#2, #6), `doctor --probe` (#5),
//! `doctor --fetch` and `bench` (#8), `config show` (#2), `devices` (#4), `replay` (#9).
//! Later: `run`, `sources`.

// CLAUDE.md: no unwrap/expect outside tests and main.
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

mod bench;
mod devices;
mod doctor;
mod probe;
mod replay;
mod suggest;

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
    /// `dmn bench` could not read a fixture or its turns.
    #[error("bench: {0}")]
    Bench(String),
    /// `dmn replay` could not run the pipeline over the given files.
    #[error("{0}")]
    Replay(String),
    #[error("{failed} required check(s) failed")]
    ChecksFailed { failed: usize },
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
        /// Download a whisper model (tiny.en, base.en, small.en, medium.en, large-v3-turbo)
        /// into ~/.cache/dmn/models/, then run the checks
        #[arg(long, value_name = "MODEL", conflicts_with = "init")]
        fetch: Option<String>,
    },
    /// Measure whisper speed and accuracy on fixture interviewer turns
    Bench {
        /// Model name; default is [stt].model from the config
        #[arg(long)]
        model: Option<String>,
        /// Fixture under testdata/ (sysdesign, behavioral, mixed)
        #[arg(long, default_value = "sysdesign")]
        fixture: String,
        /// Decoder threads; default is [stt].threads from the config
        #[arg(long)]
        threads: Option<usize>,
        /// Number of interviewer turns to transcribe
        #[arg(long, default_value_t = 5)]
        turns: usize,
        /// Use whisper's fixed 30 s encoder window instead of trimming it to each utterance
        #[arg(long)]
        full_window: bool,
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
    /// Run two fixture WAVs through the real pipeline and print labelled transcript segments
    Replay {
        /// The interviewer's audio
        interviewer: std::path::PathBuf,
        /// Your audio
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
        /// Transcribe the 1 s partials of an open utterance: on, since --speed always paces
        #[arg(long, value_enum, default_value_t = replay::Partials::Auto)]
        partials: replay::Partials,
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
        /// Skip the LLM fast phase; by default each candidate goes to [llm] (Ollama) and the
        /// question + approach JSON is printed
        #[arg(long)]
        no_llm: bool,
    },
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
            interviewer,
            candidate,
            speed,
            model,
            threads,
            partials,
            transcript,
            json,
            spec_literal_detector,
            no_llm,
        }) => {
            let cfg = Config::load(&paths.config_file)?;
            // As with bench above, the module keeps its boxed error and it is mapped here.
            replay::run(
                &cfg,
                &paths,
                &replay::Args {
                    interviewer,
                    candidate,
                    speed,
                    model,
                    threads,
                    partials,
                    transcript,
                    json,
                    spec_literal_detector,
                    no_llm,
                },
            )
            .map_err(|e| CliError::Replay(e.to_string()))
        }
        Some(Command::Config {
            action: ConfigAction::Show,
        }) => {
            let cfg = Config::load(&paths.config_file)?;
            tracing::debug!(provider = ?cfg.llm.provider, "config show");
            print!("{}", cfg.to_toml()?);
            Ok(())
        }
        Some(Command::Bench {
            model,
            fixture,
            threads,
            turns,
            full_window,
            testdata,
        }) => {
            let cfg = Config::load(&paths.config_file)?;
            let args = bench::BenchArgs {
                model: model.unwrap_or_else(|| cfg.stt.model.clone()),
                fixture,
                threads: threads.unwrap_or(usize::from(cfg.stt.threads)),
                turns,
                trim_audio_ctx: !full_window,
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

/// `dmn doctor --fetch <model>`: download with a progress line on stderr.
fn fetch_model(paths: &Paths, name: &str) -> Result<(), CliError> {
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
    println!("model:    {} ({len} bytes)", path.display());
    Ok(())
}
