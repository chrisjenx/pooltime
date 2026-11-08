//! `pooltime models list|refresh`: the pool's `/v1/models` listing, and a forced re-fetch of the
//! live Claude catalog (which also re-derives the model pins of connected profiles).

use clap::Subcommand;
use serde_json::json;

use super::client;
use super::status::table;
use crate::config::Paths;

#[derive(Debug, Subcommand)]
pub enum ModelsCmd {
    /// List the models the pool serves (what Claude Code sees).
    List {
        /// Print the raw `/v1/models` JSON.
        #[arg(long)]
        json: bool,
    },
    /// Re-fetch the live Claude model catalog now and re-sync connected profiles.
    Refresh,
}

pub async fn run(paths: &Paths, cmd: ModelsCmd) -> anyhow::Result<i32> {
    let c = client::require(paths).await?;
    match cmd {
        ModelsCmd::List { json } => {
            let v = c.get("/v1/models").await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&v)?);
                return Ok(0);
            }
            let mut rows = vec![["MODEL", "NAME", "OWNER"].map(String::from).to_vec()];
            for m in v["data"].as_array().into_iter().flatten() {
                let s = |k: &str| m[k].as_str().unwrap_or("-").to_string();
                rows.push(vec![s("id"), s("display_name"), s("owned_by")]);
            }
            print!("{}", table(&rows));
            Ok(0)
        }
        ModelsCmd::Refresh => {
            let v = c.post("/api/v1/models/refresh", json!({})).await?;
            println!(
                "refreshed: {} live Claude models, {} listed; {} connected profiles re-synced",
                v["live"],
                v["models"].as_array().map_or(0, Vec::len),
                v["profiles_synced"]
            );
            Ok(0)
        }
    }
}
