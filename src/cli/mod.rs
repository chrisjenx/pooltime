use std::ffi::OsString;

use clap::{Parser, Subcommand};

/// Pool Claude Max and ChatGPT/Codex accounts behind one local Anthropic-compatible endpoint.
#[derive(Debug, Parser)]
#[command(name = "pooltime", version)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Command>,
}

/// Subcommands (added by later tasks).
#[derive(Debug, Subcommand)]
pub enum Command {}

/// Parse `args` (including argv[0]) and run; returns the process exit code.
pub fn run(args: Vec<OsString>) -> anyhow::Result<i32> {
    let cli = match Cli::try_parse_from(args) {
        Ok(cli) => cli,
        Err(e) => {
            e.print()?;
            return Ok(e.exit_code());
        }
    };
    match cli.command {
        Some(never) => match never {},
        None => Ok(0),
    }
}
