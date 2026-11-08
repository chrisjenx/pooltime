//! `pooltime import bro`: migrate a bro-cli install (see [`crate::import::bro`]). Runs in-process
//! (no daemon needed); bro's files are only read.

use std::path::PathBuf;

use anyhow::Context;
use clap::{Args, Subcommand};

use crate::config::{Config, Paths};
use crate::import::bro::{apply_import, fill_identities, plan_import, SystemKeychain};
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
    if args.dry_run {
        print!("{}", plan.render(paths));
        println!("\nDry run: nothing was written.");
        return Ok(0);
    }
    let mut report = apply_import(&plan, paths, &SystemKeychain)?;
    let cfg = Config::load(paths).map(|c| c.anthropic).unwrap_or_default();
    fill_identities(paths, &mut report, &Upstream::new(), &cfg).await;
    print!("{}", report.render());
    Ok(0)
}
