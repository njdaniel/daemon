//! `dmn`: the Daemon command-line entry point.
//!
//! Subcommands arrive with their issues. Now: `doctor` (#2, #6), `doctor --probe` (#5),
//! `config show` (#2), `devices` (#4). Later: `run`, `sources`, `replay`, `bench`,
//! `doctor --fetch` (#8).

// CLAUDE.md: no unwrap/expect outside tests and main.
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

mod devices;
mod doctor;
mod probe;

use std::process::ExitCode;

use clap::{Parser, Subcommand};
use dmn_core::{Config, Paths};

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
    },
    /// Inspect configuration
    Config {
        #[command(subcommand)]
        action: ConfigAction,
    },
    /// List PipeWire capture sources and application output streams
    Devices,
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

fn run(cli: Cli) -> Result<(), Box<dyn std::error::Error>> {
    let paths = Paths::from_xdg().ok_or("no home directory: cannot resolve XDG paths")?;
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
        Some(Command::Doctor { init, force, probe }) => {
            if init {
                Config::write_template(&paths.config_file, force)?;
                tracing::info!(path = %paths.config_file.display(), "wrote config template");
                println!("wrote {}", paths.config_file.display());
            } else {
                let cfg = Config::load(&paths.config_file)?;
                let mut report =
                    doctor::run_all(&cfg, &paths, &doctor::SystemRunner, &doctor::StdHttp);
                if probe {
                    report.checks.extend(probe::run());
                }
                print!("{}", report.render());
                let failed = report.failed_required();
                tracing::info!(failed, "doctor");
                if failed > 0 {
                    return Err(format!("{failed} required check(s) failed").into());
                }
            }
            Ok(())
        }
        Some(Command::Config {
            action: ConfigAction::Show,
        }) => {
            let cfg = Config::load(&paths.config_file)?;
            tracing::debug!(provider = ?cfg.llm.provider, "config show");
            print!("{}", cfg.to_toml()?);
            Ok(())
        }
        Some(Command::Devices) => {
            let cfg = Config::load(&paths.config_file)?;
            print!("{}", devices::run(&cfg)?);
            Ok(())
        }
    }
}
