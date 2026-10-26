//! Let's Triage Mail: local-first email triage for any IMAP account.
//!
//! This binary is a thin front end: it parses the command line, sets up logging and hands
//! over to the command in `cli`. The logic lives in the library crate (`src/lib.rs`).
//! Errors here use `anyhow` (good for reporting to a human); the library will use `thiserror`
//! (good for typed errors that callers can match on).

mod cli;

use std::io::IsTerminal;

use clap::Parser;
use tracing_subscriber::EnvFilter;

use crate::cli::Cli;

// `#[tokio::main]` turns this `async fn` into a normal `main` that starts the tokio runtime
// and runs the function on it. Returning `Err` makes the process print the error and exit
// with a non-zero status.
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    init_logging(cli.verbose);
    cli::dispatch(cli).await
}

/// Sends `tracing` logs to stderr (stdout is kept for command output).
///
/// `-v` shows debug, `-vv` and more show trace. `RUST_LOG`, if set, wins over the flags so
/// that individual modules can be tuned while debugging.
fn init_logging(verbosity: u8) {
    let default_level = match verbosity {
        0 => "info",
        1 => "debug",
        _ => "trace",
    };
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(default_level));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        // Colour codes are noise in a log file (launchd, `> file`); keep them for terminals only.
        .with_ansi(std::io::stderr().is_terminal())
        .init();
}
