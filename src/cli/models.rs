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
            let (line, warning) = refresh_report(&v);
            println!("{line}");
            match warning {
                Some(w) => {
                    eprintln!("{w}");
                    Ok(1)
                }
                None => Ok(0),
            }
        }
    }
}

/// The summary line, plus a warning when the connected profiles could not be re-synced.
fn refresh_report(v: &serde_json::Value) -> (String, Option<String>) {
    let line = format!(
        "refreshed: {} live Claude models, {} listed; {} connected profiles re-synced",
        v["live"],
        v["models"].as_array().map_or(0, Vec::len),
        v["profiles_synced"]
    );
    let warning = v["sync_error"].as_str().map(|e| {
        format!("warning: connected profiles were not re-synced ({e}); see `pooltime logs`")
    });
    (line, warning)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refresh_report_warns_on_sync_error() {
        let ok = json!({"live": 2, "models": ["a", "b", "c"], "profiles_synced": 1});
        let (line, w) = refresh_report(&ok);
        assert_eq!(
            line,
            "refreshed: 2 live Claude models, 3 listed; 1 connected profiles re-synced"
        );
        assert!(w.is_none());
        let bad = json!({"live": 2, "models": [], "profiles_synced": 0,
            "sync_error": "profile_sync_failed"});
        assert!(refresh_report(&bad)
            .1
            .unwrap()
            .contains("profile_sync_failed"));
    }
}
