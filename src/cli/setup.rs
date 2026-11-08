//! `pooltime setup`: the idempotent first-run wizard (spec §3 "Install & setup").
//!
//! 1. **Accounts.** Nothing to do when pooltime already has an account. Otherwise import from bro
//!    when `~/.claude-max-pool` or `~/.bro` exists (asked; `--import-bro`), else sign in a first
//!    Claude account (`--account NAME`, with `accounts login`'s `--email`, `--mode` and `--code`;
//!    asked for interactively). Non-interactive without `--account` skips with a note.
//! 2. **Service.** Nothing to do when it is already installed; else install it (asked;
//!    `--service`). After an install the wizard waits (bounded) for the daemon to answer.
//! 3. **Profiles.** Connect exactly the profiles named with `--connect default,claude-personal`
//!    (ids as `pooltime connect` takes them), or, interactively, the ones picked from the
//!    discovered `~/.claude*` list (current state shown). Never a profile that was not chosen.
//!    Connecting needs a running pool: without one the step fails with how to start it (or, when
//!    the service is installed but did not come up, where to look).
//! 4. **Doctor**, then the dashboard URL (`pooltime open`). (Spec step 4, the status line and MCP
//!    registration, is not part of setup yet.)
//!
//! Interactive means stdin is a terminal and `--yes` is not given; `--yes` also binds a first
//! sign-in to the account without asking. Every step reuses the command that does the same job
//! (`import bro`, `accounts login`, `service install`, `connect`, `doctor`, `open`). Exit 1 when a
//! step or a doctor check fails.

use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use clap::Args;

use super::accounts::{self, Mode, SignInFlags};
use super::client::{self, Client};
use super::doctor::{self, Check, Level};
use super::import::{self, BroArgs, ImportCmd};
use super::service::{self as service_cli, ServiceCmd};
use super::{connect, open};
use crate::accounts::Registry;
use crate::claude_code::{discover, is_connected};
use crate::config::Paths;
use crate::service::{Backend, Env, SystemRunner};

#[derive(Debug, Clone, Default, Args)]
pub struct SetupArgs {
    /// Import accounts and settings from bro-cli (when it is installed).
    #[arg(long)]
    pub import_bro: bool,
    /// Sign in this first Claude account (when pooltime has none and nothing is imported).
    #[arg(long, value_name = "NAME")]
    pub account: Option<String>,
    /// The first account's email (pre-filled on the sign-in page).
    #[arg(long, requires = "account")]
    pub email: Option<String>,
    /// How to finish the sign-in (default: paste when headless, else callback).
    #[arg(long, value_enum, requires = "account")]
    pub mode: Option<Mode>,
    /// The `code#state` line to submit instead of prompting for it.
    #[arg(long, value_name = "CODE#STATE", requires = "account")]
    pub code: Option<String>,
    /// Install the background service.
    #[arg(long)]
    pub service: bool,
    /// Claude Code profiles to connect, comma-separated (`default,claude-personal`).
    #[arg(long, value_delimiter = ',', value_name = "PROFILES")]
    pub connect: Vec<String>,
    /// Never ask: do only what the flags say (and bind a first sign-in without asking).
    #[arg(long, short)]
    pub yes: bool,
}

/// How long to wait for a freshly installed service to answer `/health`.
const SERVICE_BOOT: Duration = Duration::from_secs(20);

fn home() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .filter(|h| !h.is_empty())
        .map(PathBuf::from)
}

fn report(steps: &mut Vec<Check>, c: Check) {
    println!("{c}");
    steps.push(c);
}

async fn ask(interactive: bool, question: &str) -> anyhow::Result<bool> {
    Ok(interactive && client::confirm(question).await?)
}

async fn prompt(question: &str) -> anyhow::Result<String> {
    print!("{question}");
    std::io::stdout().flush()?;
    Ok(client::read_line().await?.unwrap_or_default())
}

pub async fn run(paths: &Paths, args: SetupArgs) -> anyhow::Result<i32> {
    let interactive = !args.yes && std::io::stdin().is_terminal();
    let mut steps = Vec::new();

    let c = accounts_step(paths, &args, interactive).await;
    report(&mut steps, c);

    let (c, installed) = service_step(paths, &args, interactive).await;
    report(&mut steps, c);

    let daemon = if installed {
        wait_for_daemon(paths, SERVICE_BOOT).await
    } else {
        client::find(paths).await.ok().flatten()
    };
    let c = profiles_step(paths, &args, interactive, installed, daemon.as_ref()).await;
    report(&mut steps, c);

    println!();
    let checks = doctor::checks(paths, &doctor::Options::from_env(false)).await;
    doctor::print(&checks);
    println!();
    if client::find(paths).await.ok().flatten().is_some() {
        println!("Dashboard:");
        open::run(paths).await?;
    } else {
        println!("Dashboard: {}", client::START_HINT);
    }
    let failed = steps.iter().any(|c| c.level == Level::Fail) || doctor::exit_code(&checks) != 0;
    Ok(i32::from(failed))
}

/// Accounts pooltime has: the running pool's, else the files'. Read-only.
async fn account_count(paths: &Paths) -> usize {
    if let Ok(Some(c)) = client::find(paths).await {
        if let Ok(v) = c.get("/api/v1/status").await {
            return v["accounts"].as_array().map_or(0, Vec::len);
        }
    }
    if !paths.accounts.is_dir() {
        return 0;
    }
    Registry::load(paths).map_or(0, |r| r.list().len())
}

fn bro_installed(home: &Path) -> bool {
    home.join(".claude-max-pool").is_dir() || home.join(".bro").is_dir()
}

async fn accounts_step(paths: &Paths, args: &SetupArgs, interactive: bool) -> Check {
    match accounts_step_inner(paths, args, interactive).await {
        Ok(c) => c,
        Err(e) => Check::new(Level::Fail, "accounts", format!("{e:#}")),
    }
}

async fn accounts_step_inner(
    paths: &Paths,
    args: &SetupArgs,
    interactive: bool,
) -> anyhow::Result<Check> {
    let label = "accounts";
    let n = account_count(paths).await;
    if n > 0 {
        return Ok(Check::new(
            Level::Ok,
            label,
            format!("{n} already set up (nothing to do)"),
        ));
    }
    let bro = home().is_some_and(|h| bro_installed(&h));
    if bro
        && (args.import_bro
            || ask(
                interactive,
                "bro-cli is installed. Import its accounts and settings?",
            )
            .await?)
    {
        let cmd = ImportCmd::Bro(BroArgs {
            dry_run: false,
            bro_pool: None,
            bro_home: None,
        });
        return Ok(match import::run(paths, cmd).await? {
            0 => Check::new(Level::Ok, label, "imported from bro"),
            _ => Check::new(Level::Fail, label, "the bro import did not run (see above)"),
        });
    }
    let mut note = String::new();
    if args.import_bro && !bro {
        note = " (no bro install found to import)".into();
    }
    let name = match &args.account {
        Some(n) => Some(n.clone()),
        None if interactive => {
            let n = prompt("Name for your first Claude account (e.g. personal; Enter to skip): ")
                .await?;
            (!n.is_empty()).then_some(n)
        }
        None => None,
    };
    let Some(name) = name else {
        return Ok(Check::new(Level::Warn, label, format!("none added{note}"))
            .hint("`pooltime accounts login <name>`, or re-run setup with `--account <name>`"));
    };
    let flags = SignInFlags {
        mode: args.mode,
        code: args.code.clone(),
        yes: args.yes,
    };
    let daemon = client::find(paths).await?;
    let saved =
        accounts::login(paths, daemon.as_ref(), &name, args.email.as_deref(), &flags).await?;
    Ok(if saved {
        Check::new(Level::Ok, label, format!("signed in '{name}'{note}"))
    } else {
        Check::new(
            Level::Fail,
            label,
            format!("'{name}' was not signed in{note}"),
        )
        .hint(format!("`pooltime accounts login {name}`"))
    })
}

/// The service step, and whether the service is installed afterwards.
async fn service_step(paths: &Paths, args: &SetupArgs, interactive: bool) -> (Check, bool) {
    let label = "service";
    let ctx = Backend::native().and_then(|b| Ok((b, Env::from_process(paths)?)));
    let (backend, env) = match ctx {
        Ok(x) => x,
        Err(e) => return (Check::new(Level::Fail, label, format!("{e:#}")), false),
    };
    let status = match backend.status(&env, &SystemRunner) {
        Ok(s) => s,
        Err(e) => return (Check::new(Level::Fail, label, format!("{e:#}")), false),
    };
    if status.installed {
        let c = if status.running {
            Check::new(Level::Ok, label, "already installed and running")
        } else {
            Check::new(Level::Warn, label, "installed but not running")
                .hint("`pooltime service restart`")
        };
        return (c, true);
    }
    let want = if args.service {
        true
    } else {
        match ask(
            interactive,
            "Install the background service (starts pooltime at login)?",
        )
        .await
        {
            Ok(w) => w,
            Err(e) => return (Check::new(Level::Fail, label, format!("{e:#}")), false),
        }
    };
    if !want {
        let c = Check::new(
            Level::Warn,
            label,
            "not installed (the pool runs only while `pooltime serve` does)",
        )
        .hint("`pooltime service install`, or re-run setup with `--service`");
        return (c, false);
    }
    match service_cli::run_with(backend, &env, &SystemRunner, ServiceCmd::Install) {
        Ok(0) => (Check::new(Level::Ok, label, "installed and started"), true),
        Ok(_) => (
            Check::new(Level::Fail, label, "install failed (see above)"),
            false,
        ),
        Err(e) => (Check::new(Level::Fail, label, format!("{e:#}")), false),
    }
}

/// Poll for the daemon until it answers or `budget` runs out.
async fn wait_for_daemon(paths: &Paths, budget: Duration) -> Option<Client> {
    let start = Instant::now();
    loop {
        if let Ok(Some(c)) = client::find(paths).await {
            return Some(c);
        }
        if start.elapsed() >= budget {
            return None;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

/// Show the discovered profiles with their state and read a selection (numbers or ids).
async fn pick_profiles(paths: &Paths) -> anyhow::Result<Vec<String>> {
    let found = home().as_deref().map(discover).unwrap_or_default();
    if found.is_empty() {
        println!("No Claude Code profiles found (~/.claude*/settings.json).");
        return Ok(Vec::new());
    }
    println!("Claude Code profiles:");
    for (i, p) in found.iter().enumerate() {
        let state = if is_connected(p, paths) {
            "connected"
        } else {
            "not connected"
        };
        println!(
            "  {}) {:<16} {}  ({state})",
            i + 1,
            p.id,
            p.settings_dir.display()
        );
    }
    let answer =
        prompt("Connect which? (numbers or ids, comma-separated; Enter for none): ").await?;
    Ok(answer
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| match s.parse::<usize>() {
            Ok(n) if (1..=found.len()).contains(&n) => found[n - 1].id.clone(),
            _ => s.to_string(),
        })
        .collect())
}

async fn profiles_step(
    paths: &Paths,
    args: &SetupArgs,
    interactive: bool,
    service_installed: bool,
    daemon: Option<&Client>,
) -> Check {
    let label = "profiles";
    let chosen = if !args.connect.is_empty() {
        Ok(args.connect.clone())
    } else if interactive {
        pick_profiles(paths).await
    } else {
        Ok(Vec::new())
    };
    let chosen = match chosen {
        Ok(c) => c,
        Err(e) => return Check::new(Level::Fail, label, format!("{e:#}")),
    };
    if chosen.is_empty() {
        return Check::new(Level::Skip, label, "none chosen")
            .hint("connect later with `pooltime connect <profile>`");
    }
    if daemon.is_none() {
        let hint = if service_installed {
            "the service did not answer: check `pooltime service status` / `pooltime logs`, \
             then re-run setup"
        } else {
            "start it with `pooltime serve` (or re-run setup with `--service`), then re-run setup"
        };
        return Check::new(
            Level::Fail,
            label,
            format!(
                "not connected ({}): pooltime is not running",
                chosen.join(", ")
            ),
        )
        .hint(hint);
    }
    match connect::connect(paths, chosen.clone(), false, false, false).await {
        Ok(0) => Check::new(Level::Ok, label, format!("connected {}", chosen.join(", "))),
        Ok(_) => Check::new(Level::Fail, label, "not connected (see above)"),
        Err(e) => Check::new(Level::Fail, label, format!("{e:#}")),
    }
}
