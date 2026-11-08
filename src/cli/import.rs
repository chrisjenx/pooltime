//! `pooltime import bro`: migrate a bro-cli install (see [`crate::import::bro`]). Runs in-process
//! (no daemon needed); bro's files are only read.

use std::path::PathBuf;

use anyhow::Context;
use clap::{Args, Subcommand};

use crate::config::{Config, Paths};
use crate::import::bro::{apply_import, fill_identities, plan_import, SystemKeychain};
use crate::server::health::{probe, probe_addr, Probe};
use crate::upstream::Upstream;

#[derive(Debug, Subcommand)]
pub enum ImportCmd {
    /// Import accounts, usage markers, sticky sessions, models, tuning and Claude Code settings
    /// snapshots from bro-cli. bro's files and Keychain items are never changed.
    Bro(BroArgs),
}

#[derive(Debug, Args)]
pub struct BroArgs {
    /// Print what would be imported and write nothing.
    #[arg(long)]
    pub dry_run: bool,
    /// bro's pool directory [default: ~/.claude-max-pool].
    #[arg(long, value_name = "DIR")]
    pub bro_pool: Option<PathBuf>,
    /// bro's state directory holding pool-settings*.json [default: ~/.bro].
    #[arg(long, value_name = "DIR")]
    pub bro_home: Option<PathBuf>,
}

pub async fn run(paths: &Paths, cmd: ImportCmd) -> anyhow::Result<i32> {
    let ImportCmd::Bro(args) = cmd;
    let home = std::env::var_os("HOME")
        .filter(|h| !h.is_empty())
        .map(PathBuf::from)
        .context("HOME is not set")?;
    let pool = args
        .bro_pool
        .unwrap_or_else(|| home.join(".claude-max-pool"));
    let bro_home = args.bro_home.unwrap_or_else(|| home.join(".bro"));
    let plan = plan_import(&pool, &bro_home, &home)?;
    let cfg = Config::load(paths).unwrap_or_else(|_| {
        // The parse error could quote config.toml (it may hold the proxy key): no details.
        eprintln!(
            "note: pooltime's config.toml could not be loaded; using the default host/port and \
             Anthropic URLs"
        );
        Config::default()
    });
    // A running daemon keeps account state in memory and would overwrite what the import
    // seeds; restarting it afterwards also makes `serve` run its bro shadow check.
    let addr = probe_addr(cfg.host, cfg.port);
    let running = probe(addr).await == Probe::Pooltime;
    if args.dry_run {
        print!("{}", plan.render(paths));
        if running {
            println!("\nNote: {}", running_message(addr));
        }
        println!("\nDry run: nothing was written.");
        return Ok(0);
    }
    if running {
        eprintln!("{}", running_message(addr));
        return Ok(1);
    }
    let mut report = apply_import(&plan, paths, &SystemKeychain)?;
    fill_identities(paths, &mut report, &Upstream::new(), &cfg.anthropic).await;
    print!("{}", report.render());
    Ok(0)
}

fn running_message(addr: std::net::SocketAddr) -> String {
    format!(
        "pooltime is running on {addr} — stop it first (`pooltime service uninstall` / Ctrl-C), \
         re-run import, then start it again so the bro check runs"
    )
}
