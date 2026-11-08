//! The `pooltime` command line. `serve` runs the daemon; every other command is a thin client of
//! its management API ([`client`]), except `logs` (reads the log files), `disconnect` and
//! `accounts login` (both fall back to working in-process when no daemon is running).

pub mod accounts;
pub mod client;
pub mod connect;
pub mod doctor;
pub mod import;
pub mod logs;
pub mod models;
pub mod open;
pub mod service;
pub mod setup;
pub mod status;
pub mod statusline;

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
    /// One-screen summary: accounts, 7-day headroom, alerts.
    Status {
        /// Print the raw status JSON.
        #[arg(long)]
        json: bool,
    },
    /// Manage pooled accounts.
    #[command(subcommand)]
    Accounts(accounts::AccountsCmd),
    /// Point Claude Code profiles at the pool.
    Connect {
        /// Profile ids (`default`, `claude-personal`, ...) or settings directories.
        profiles: Vec<String>,
        /// Every Claude Code profile found under ~/.claude*.
        #[arg(long, conflicts_with = "profiles")]
        all: bool,
        /// Also wrap each profile's status line to show the pool segment (restored on disconnect).
        #[arg(long)]
        statusline: bool,
    },
    /// Restore Claude Code profiles' own settings (all connected ones if none are named).
    Disconnect {
        /// Profile ids or settings directories.
        profiles: Vec<String>,
    },
    /// The models the pool serves.
    #[command(subcommand)]
    Models(models::ModelsCmd),
    /// Print the dashboard URL and open it in the browser.
    Open,
    /// Show the daemon's logs.
    Logs(logs::LogsArgs),
    /// Run the daemon as a background service (launchd on macOS, systemd on Linux).
    #[command(subcommand)]
    Service(service::ServiceCmd),
    /// Migrate from another tool (bro-cli).
    #[command(subcommand)]
    Import(import::ImportCmd),
    /// First-run wizard: accounts, background service, Claude Code profiles, then doctor.
    Setup(setup::SetupArgs),
    /// Check the installation: one line per check, exit 1 on any failure.
    Doctor,
    /// Print the pool segment for Claude Code's status line (from the daemon's cache).
    Statusline {
        /// Run this profile's original status line command (Claude Code's stdin passed on) and
        /// append the segment to its output.
        #[arg(long, value_name = "PROFILE_ID")]
        wrap: Option<String>,
    },
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
    let Some(command) = cli.command else {
        return Ok(0);
    };
    let paths = Paths::resolve();
    // Runs on every status line refresh: no runtime, no network.
    if let Command::Statusline { wrap } = command {
        return Ok(statusline::run(&paths, wrap));
    }
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    rt.block_on(async move {
        match command {
            Command::Serve(flags) => {
                crate::server::daemon::run_daemon(paths, flags).await?;
                Ok(0)
            }
            Command::Status { json } => status::run(&client::require(&paths).await?, json).await,
            Command::Accounts(cmd) => accounts::run(&paths, cmd).await,
            Command::Connect {
                profiles,
                all,
                statusline,
            } => connect::connect(&paths, profiles, all, statusline).await,
            Command::Disconnect { profiles } => connect::disconnect(&paths, profiles).await,
            Command::Models(cmd) => models::run(&paths, cmd).await,
            Command::Open => open::run(&paths).await,
            // Blocking file reads; nothing else runs on this runtime meanwhile.
            Command::Logs(args) => logs::run(&paths, args).await,
            // Short blocking calls to launchctl/systemctl.
            Command::Service(cmd) => service::run(&paths, cmd),
            // Blocking file and Keychain reads, then the identity fetches.
            Command::Import(cmd) => import::run(&paths, cmd).await,
            Command::Setup(args) => setup::run(&paths, args).await,
            Command::Doctor => doctor::run(&paths).await,
            Command::Statusline { .. } => unreachable!("handled before the runtime starts"),
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn cli_definition_is_consistent() {
        Cli::command().debug_assert();
    }

    #[test]
    fn spike_flag_is_hidden_and_replaces_the_name() {
        let mut cmd = Cli::command();
        let login = cmd
            .find_subcommand_mut("accounts")
            .unwrap()
            .find_subcommand_mut("login")
            .unwrap();
        let help = login.render_long_help().to_string();
        assert!(!help.contains("spike"), "{help}");
        let parse = |args: &[&str]| Cli::try_parse_from([&["pooltime"], args].concat());
        assert!(parse(&["accounts", "login", "--spike-login-hint", "a@b"]).is_ok());
        assert!(parse(&["accounts", "login"]).is_err());
        assert!(parse(&["accounts", "relogin"]).is_err());
        assert!(parse(&["accounts", "relogin", "--expiring"]).is_ok());
        assert!(parse(&["accounts", "add", "x", "--email", "a@b"]).is_ok());
    }
}
