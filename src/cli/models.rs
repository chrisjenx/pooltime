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

/// The summary lines, warnings, and whether the command failed.
///
/// A Claude live-catalog failure (`live_error`) or a Codex discovery failure (`codex.error`,
/// per-account `codex.failed`) is only a warning while the other catalog answered; the command
/// fails (exit 1) only when every catalog failed (the API answers 502 then, which the client
/// reports as an error) or the connected profiles could not be re-synced.
fn refresh_report(v: &serde_json::Value) -> (Vec<String>, Vec<String>, bool) {
    let mut lines = vec![format!(
        "refreshed: {} live Claude models, {} listed; {} connected profiles re-synced",
        v["live"],
        v["models"].as_array().map_or(0, Vec::len),
        v["profiles_synced"]
    )];
    let mut warnings = Vec::new();
    let live_failed = match v["live_error"].as_str() {
        Some(e) => {
            warnings.push(format!(
                "warning: Claude live model catalog unavailable ({e})"
            ));
            true
        }
        None => false,
    };
    let codex = &v["codex"];
    let codex_failed = match codex["error"].as_str() {
        Some(e) => {
            warnings.push(format!("warning: Codex model discovery failed ({e})"));
            true
        }
        None if codex.is_object() => {
            let names = |k: &str| -> Vec<&str> {
                codex[k]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|m| m.as_str())
                    .collect()
            };
            let (added, updated) = (names("added"), names("updated"));
            let n = codex["accounts"].as_u64().unwrap_or(0);
            let asked = format!("{n} account{} asked", plural(n as usize));
            let mut line = match (n, added.len()) {
                (0, _) => "codex: no signed-in ChatGPT account; discovery skipped".to_string(),
                (_, 0) => format!("codex: no new models ({asked})"),
                (_, k) => format!(
                    "codex: discovered {k} new model{} ({asked}): {}",
                    plural(k),
                    added.join(", ")
                ),
            };
            if !updated.is_empty() {
                line.push_str(&format!("; refreshed {}", updated.join(", ")));
            }
            lines.push(line);
            for f in codex["failed"].as_array().into_iter().flatten() {
                warnings.push(format!(
                    "warning: Codex catalog unavailable for account {} ({})",
                    f["account"].as_str().unwrap_or("?"),
                    f["reason"].as_str().unwrap_or("unknown")
                ));
            }
            false
        }
        // No Codex provider: the Claude catalog was the only one asked.
        None => true,
    };
    let mut failed = live_failed && codex_failed;
    if let Some(e) = v["sync_error"].as_str() {
        warnings.push(format!(
            "warning: connected profiles were not re-synced ({e}); see `pooltime logs`"
        ));
        failed = true;
    }
    (lines, warnings, failed)
}

fn plural(n: usize) -> &'static str {
    if n == 1 {
        ""
    } else {
        "s"
    }
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
        assert_eq!(lines[1], "codex: no new models (1 account asked)");
        let (lines, _, _) =
            refresh_report(&base(json!({"accounts": 0, "added": [], "failed": []})));
        assert!(lines[1].contains("skipped"));
        let (lines, _, _) = refresh_report(&base(
            json!({"accounts": 1, "added": [], "updated": ["gpt-z"], "failed": []}),
        ));
        assert_eq!(
            lines[1],
            "codex: no new models (1 account asked); refreshed gpt-z"
        );

        // Codex failing while Claude answered: a warning, not a failure.
        let (_, w, failed) = refresh_report(&base(json!({"error": "unavailable (status:401)"})));
        assert!(!failed);
        assert!(w[0].contains("status:401"));

        // Claude failing while Codex answered: a warning, not a failure.
        let mut v = base(json!({"accounts": 1, "added": ["gpt-x"], "failed": []}));
        v["live_error"] = json!("live model catalog refresh failed: status:500");
        let (lines, w, failed) = refresh_report(&v);
        assert!(!failed);
        assert_eq!(
            lines[1],
            "codex: discovered 1 new model (1 account asked): gpt-x"
        );
        assert!(w[0].contains("Claude") && w[0].contains("status:500"));
        // Every catalog failed: the command fails.
        v["codex"] = json!({"error": "x"});
        assert!(refresh_report(&v).2);
    }
}
