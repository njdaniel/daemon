//! `dmn`: the Daemon command-line entry point.
//!
//! Issue #1 scope: `dmn --version` and `dmn --help`. Subcommands (`run`, `doctor`,
//! `devices`, `sources`, `config`, `replay`, `bench`) arrive with their issues.

use clap::Parser;

/// Daemon: a Linux-native interview copilot.
#[derive(Parser, Debug)]
#[command(name = "dmn", version, about, long_about = None)]
struct Cli {}

fn main() {
    let _cli = Cli::parse();
    println!(
        "dmn {}: no subcommands yet; see docs/issues/",
        env!("CARGO_PKG_VERSION")
    );
}
