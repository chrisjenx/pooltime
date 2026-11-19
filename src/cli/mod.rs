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
pub mod update;

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
        /// Also register the pool's MCP server in each profile's .claude.json (removed on
        /// disconnect).
        #[arg(long)]
        mcp: bool,
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
    Doctor {
        /// Skip asking GitHub for the latest release (also `POOLTIME_NO_VERSION_CHECK=1`).
        #[arg(long)]
        no_version_check: bool,
    },
    /// Print the pool segment for Claude Code's status line (from the daemon's cache).
    Statusline {
        /// Run this profile's original status line command (Claude Code's stdin passed on) and
        /// append the segment to its output.
        #[arg(long, value_name = "PROFILE_ID")]
        wrap: Option<String>,
    },
    /// Install the latest release (Homebrew or the install script) and move the running service
    /// onto it without closing the port.
    Update(update::UpdateArgs),
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
    /// Run as a supervisor that owns the port and runs the daemon as worker processes, so
    /// `pooltime service restart` hands over to a new worker without closing the port (what the
    /// background service runs).
    #[arg(long)]
    pub supervise: bool,
    /// Worker mode (set by the supervisor): serve on this inherited listening socket.
    #[arg(
        long,
        hide = true,
        requires = "control_fd",
        conflicts_with = "supervise"
    )]
    pub listen_fd: Option<i32>,
    /// Worker mode (set by the supervisor): the inherited control socket.
    #[arg(long, hide = true, requires = "listen_fd")]
    pub control_fd: Option<i32>,
    /// Supervisor mode, set by a supervisor that re-executes itself in place (`pooltime
    /// update`): the state it hands over (see `supervisor::resume`).
    #[arg(long, hide = true, requires = "supervise")]
    pub resume: Option<String>,
    /// Say which supervisor hand-over formats this binary reads, then exit (a supervisor asks
    /// this before re-executing into it).
    #[arg(long, hide = true, conflicts_with_all = ["supervise", "listen_fd"])]
    pub reexec_probe: bool,
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
    if let Command::Serve(ServeFlags {
        reexec_probe: true, ..
    }) = command
    {
        return Ok(reexec_probe(&paths));
    }
    let supervise = matches!(&command, Command::Serve(f) if f.supervise);
    if matches!(&command, Command::Serve(f) if f.listen_fd.is_some()) {
        // A worker: the supervisor's upgrade signal (`pkill -USR2`, a process-group signal)
        // must not kill it (the default action), so ignore it before anything else runs.
        // SAFETY: setting a signal disposition to SIG_IGN, before any other thread exists.
        unsafe {
            libc::signal(libc::SIGUSR2, libc::SIG_IGN);
        }
    }
    let mut builder = tokio::runtime::Builder::new_multi_thread();
    builder.enable_all();
    if supervise {
        // Only the supervisor's own (main) thread takes its signals; see `supervisor::resume`.
        builder.on_thread_start(crate::supervisor::resume::block_signals);
    }
    let rt = builder.build()?;
    rt.block_on(async move {
        match command {
            Command::Serve(flags) if flags.supervise => crate::supervisor::run(paths, flags).await,
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
                mcp,
            } => connect::connect(&paths, profiles, all, statusline, mcp).await,
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
            Command::Doctor { no_version_check } => {
                doctor::run(&paths, &doctor::Options::from_env(no_version_check)).await
            }
            Command::Update(args) => update::run(&paths, args).await,
            Command::Statusline { .. } => unreachable!("handled before the runtime starts"),
        }
    })
}

/// `serve --reexec-probe`: a supervisor asks this before it re-executes into this binary. Fails
/// (without quoting it: it may hold the proxy key) when `config.toml` does not load, since the
/// new supervisor would then run on defaults, and when the data or log directory cannot be
/// prepared or written (the new supervisor's first steps).
fn reexec_probe(paths: &Paths) -> i32 {
    if let Err(e) = crate::logging::check_writable(paths) {
        eprintln!("the log directory cannot be written: {e:#}");
        return 1;
    }
    match crate::config::Config::load(paths).and_then(|c| c.validate().map(|()| c)) {
        Ok(_) => {
            println!("{}", crate::supervisor::resume::probe_line());
            0
        }
        Err(_) => {
            eprintln!(
                "config.toml could not be loaded or is invalid; fix it (`pooltime doctor`) and \
                 retry"
            );
            1
        }
    }
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
