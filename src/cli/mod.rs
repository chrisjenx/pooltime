use std::ffi::OsString;
use std::net::IpAddr;

use clap::{Args, Parser, Subcommand};

use crate::config::Paths;

/// Pool Claude Max and ChatGPT/Codex accounts behind one local Anthropic-compatible endpoint.
#[derive(Debug, Parser)]
#[command(name = "pooltime", version)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Command>,
}

/// Subcommands (more are added by later tasks).
#[derive(Debug, Subcommand)]
pub enum Command {
    /// Run the pool daemon in the foreground.
    Serve(ServeFlags),
}

/// `pooltime serve` flags. `--port`/`--host` override `config.toml` and `POOLTIME_PORT`/`POOLTIME_HOST`.
#[derive(Debug, Clone, Default, Args)]
pub struct ServeFlags {
    /// Port to listen on.
    #[arg(long)]
    pub port: Option<u16>,
    /// IP address to bind.
    #[arg(long)]
    pub host: Option<IpAddr>,
    /// Confirm the bro pool is stopped, so imported accounts may be served.
    #[arg(long)]
    pub i_stopped_bro: bool,
}

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
        Some(Command::Serve(flags)) => {
            let rt = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()?;
            rt.block_on(crate::server::daemon::run_daemon(Paths::resolve(), flags))?;
            Ok(0)
        }
        None => Ok(0),
    }
}
