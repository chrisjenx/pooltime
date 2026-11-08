//! `pooltime models list|refresh`: the pool's `/v1/models` listing, and a forced re-fetch of the
//! live Claude catalog plus Codex catalog discovery (which also re-derives the model pins of
//! connected profiles).

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
    /// Re-fetch the live Claude model catalog, discover new Codex models, and re-sync connected
    /// profiles.
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
            let (lines, warnings, failed) = refresh_report(&v);
            for l in lines {
                println!("{l}");
            }
            for w in &warnings {
                eprintln!("{w}");
            }
            Ok(if failed { 1 } else { 0 })
        }
    }
}

/// The summary lines, warnings, and whether the command failed (profiles not re-synced, or no
/// Codex account could be asked for its catalog). A partial Codex failure is only a warning.
fn refresh_report(v: &serde_json::Value) -> (Vec<String>, Vec<String>, bool) {
    let mut lines = vec![format!(
        "refreshed: {} live Claude models, {} listed; {} connected profiles re-synced",
        v["live"],
        v["models"].as_array().map_or(0, Vec::len),
        v["profiles_synced"]
    )];
    let mut warnings = Vec::new();
    let mut failed = false;
    let codex = &v["codex"];
    if let Some(e) = codex["error"].as_str() {
        warnings.push(format!("warning: Codex model discovery failed ({e})"));
        failed = true;
    } else if codex.is_object() {
        let added: Vec<&str> = codex["added"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|m| m.as_str())
            .collect();
        lines.push(
            match (codex["accounts"].as_u64().unwrap_or(0), added.len()) {
                (0, _) => "codex: no signed-in ChatGPT account; discovery skipped".to_string(),
                (n, 0) => format!("codex: no new models ({n} accounts asked)"),
                (n, k) => format!(
                    "codex: discovered {k} new model{} ({n} accounts asked): {}",
                    if k == 1 { "" } else { "s" },
                    added.join(", ")
                ),
            },
        );
        for f in codex["failed"].as_array().into_iter().flatten() {
            warnings.push(format!(
                "warning: Codex catalog unavailable for account {} ({})",
                f["account"].as_str().unwrap_or("?"),
                f["reason"].as_str().unwrap_or("unknown")
            ));
        }
    }
    if let Some(e) = v["sync_error"].as_str() {
        warnings.push(format!(
            "warning: connected profiles were not re-synced ({e}); see `pooltime logs`"
        ));
        failed = true;
    }
    (lines, warnings, failed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refresh_report_warns_on_sync_error() {
        let ok = json!({"live": 2, "models": ["a", "b", "c"], "profiles_synced": 1});
        let (lines, w, failed) = refresh_report(&ok);
        assert_eq!(
            lines,
            vec!["refreshed: 2 live Claude models, 3 listed; 1 connected profiles re-synced"]
        );
        assert!(w.is_empty() && !failed);
        let bad = json!({"live": 2, "models": [], "profiles_synced": 0,
            "sync_error": "profile_sync_failed"});
        let (_, w, failed) = refresh_report(&bad);
        assert!(failed);
        assert!(w[0].contains("profile_sync_failed"));
    }

    #[test]
    fn refresh_report_shows_codex_discovery() {
        let base = |codex: serde_json::Value| json!({"live": 0, "models": [], "profiles_synced": 0, "codex": codex});
        let (lines, w, failed) = refresh_report(&base(json!({"accounts": 2,
            "added": ["gpt-x", "gpt-y"],
            "failed": [{"account": "work", "reason": "status:500"}]})));
        assert_eq!(
            lines[1],
            "codex: discovered 2 new models (2 accounts asked): gpt-x, gpt-y"
        );
        assert_eq!(
            w,
            vec!["warning: Codex catalog unavailable for account work (status:500)"]
        );
        assert!(!failed, "a partial failure is only a warning");

        let (lines, _, _) =
            refresh_report(&base(json!({"accounts": 1, "added": [], "failed": []})));
        assert_eq!(lines[1], "codex: no new models (1 accounts asked)");
        let (lines, _, _) =
            refresh_report(&base(json!({"accounts": 0, "added": [], "failed": []})));
        assert!(lines[1].contains("skipped"));

        let (_, w, failed) = refresh_report(&base(json!({"error": "unavailable (status:401)"})));
        assert!(failed);
        assert!(w[0].contains("status:401"));
    }
}
