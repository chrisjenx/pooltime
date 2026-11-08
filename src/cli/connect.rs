//! `pooltime connect [profile…]` / `pooltime disconnect [profile…]`.
//!
//! A profile is named by id (`default` for `~/.claude`, `claude-personal` for `~/.claude-personal`, …)
//! or by its settings directory. `connect` needs a running pool (spec §16) and connects only what
//! is named (or everything discovered with `--all`); with nothing named it lists what it found
//! and changes nothing, so no profile is connected by accident. `disconnect` with nothing named
//! releases every connected profile, and works with the pool down (offline, from the files).

use std::path::{Path, PathBuf};

use anyhow::Context;
use serde_json::{json, Value};

use super::client;
use crate::claude_code::{discover, is_connected};
use crate::config::{Config, Paths};
use crate::server::mgmt::{profile_id_arg, profiles_json, resolve_profiles};

fn home() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .filter(|h| !h.is_empty())
        .map(PathBuf::from)
}

/// An argument without `/` is always a profile id ([`profile_id_arg`]: `.claude` is `default`,
/// `.claude-work` is `claude-work`), never a directory relative to the cwd: `pooltime connect
/// .claude` inside a repository must not write the repository's `.claude/settings.json`. Ids
/// found among this user's `~/.claude*` dirs are sent as their directory (so the daemon need not
/// share `$HOME`); other ids pass through for the daemon to resolve from its config. Only an
/// argument containing `/` (`~/x`, `./x`, `x/y`, `/x`) is a path; it is made absolute.
fn normalize(args: &[String]) -> anyhow::Result<Vec<String>> {
    normalize_in(args, home().as_deref())
}

fn normalize_in(args: &[String], home: Option<&Path>) -> anyhow::Result<Vec<String>> {
    let found = home.map(discover).unwrap_or_default();
    args.iter()
        .map(|a| {
            if !a.contains('/') {
                let id = profile_id_arg(a);
                return Ok(found
                    .iter()
                    .find(|p| p.id == id)
                    .map_or(id, |p| p.settings_dir.to_string_lossy().into()));
            }
            let p = match (a.strip_prefix("~/"), home) {
                (Some(rest), Some(h)) => h.join(rest),
                (Some(_), None) => anyhow::bail!("HOME is not set; cannot resolve {a}"),
                _ => PathBuf::from(a),
            };
            let abs = std::path::absolute(&p).with_context(|| format!("resolving {a}"))?;
            Ok(abs.to_string_lossy().into_owned())
        })
        .collect()
}

fn describe(outcome: &str) -> &'static str {
    match outcome {
        "written" => "updated",
        "unchanged" => "already up to date",
        "gave_up" => "NOT changed: the file kept changing (is Claude Code writing it?); retry",
        _ => "done",
    }
}

fn print_profiles(verb: &str, list: &Value) {
    let rows = list.as_array().cloned().unwrap_or_default();
    if rows.is_empty() {
        println!("no profiles {verb}");
    }
    for p in rows {
        println!(
            "{verb} {} ({}): {}",
            p["id"].as_str().unwrap_or("?"),
            p["settings_dir"].as_str().unwrap_or("?"),
            describe(p["outcome"].as_str().unwrap_or(""))
        );
    }
}

pub async fn connect(paths: &Paths, profiles: Vec<String>, all: bool) -> anyhow::Result<i32> {
    let c = client::require(paths).await?;
    if profiles.is_empty() && !all {
        let found = home().as_deref().map(discover).unwrap_or_default();
        eprintln!("Name the profiles to connect (or pass --all). Claude Code profiles found:");
        if found.is_empty() {
            eprintln!("  (none: no ~/.claude*/settings.json)");
        }
        for p in &found {
            let state = if is_connected(p, paths) {
                "connected"
            } else {
                "not connected"
            };
            eprintln!("  {:<12} {}  ({state})", p.id, p.settings_dir.display());
        }
        return Ok(2);
    }
    let v = c
        .post(
            "/api/v1/profiles/connect",
            json!({ "profiles": normalize(&profiles)?, "all": all }),
        )
        .await?;
    print_profiles("connected", &v["profiles"]);
    Ok(0)
}

pub async fn disconnect(paths: &Paths, profiles: Vec<String>) -> anyhow::Result<i32> {
    let names = normalize(&profiles)?;
    match client::locate(paths).await? {
        client::Located::Daemon(c) => {
            let v = c
                .post("/api/v1/profiles/disconnect", json!({ "profiles": names }))
                .await?;
            print_profiles("disconnected", &v["profiles"]);
            return Ok(0);
        }
        client::Located::Absent { addr, probe } => eprintln!(
            "warning: no pooltime answers on {addr} ({probe:?}); restoring the profiles offline"
        ),
    }
    // No pool to ask: restore from the files directly.
    let cfg = Config::load_file(paths)?;
    let targets = if names.is_empty() {
        cfg.profiles
            .iter()
            .map(crate::claude_code::profile_of)
            .collect()
    } else {
        resolve_profiles(&names, &cfg.profiles).map_err(anyhow::Error::msg)?
    };
    let outcomes = crate::claude_code::disconnect(paths, &targets)?;
    print_profiles("disconnected", &profiles_json(&targets, &outcomes));
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arguments_become_absolute_dirs() {
        let home = tempfile::tempdir().unwrap();
        let prof = home.path().join(".claude-work2");
        std::fs::create_dir_all(&prof).unwrap();
        std::fs::write(prof.join("settings.json"), "{}").unwrap();
        let args: Vec<String> = [
            "/abs/dir",
            "rel/dir",
            "~/x",
            "claude-work2",
            "other",
            ".claude-work2",
            ".claude",
            "./.claude",
        ]
        .map(String::from)
        .to_vec();
        let out = normalize_in(&args, Some(home.path())).unwrap();
        assert_eq!(out[0], "/abs/dir");
        assert!(Path::new(&out[1]).is_absolute() && out[1].ends_with("rel/dir"));
        assert_eq!(out[2], home.path().join("x").to_string_lossy());
        assert_eq!(
            out[3],
            prof.to_string_lossy(),
            "a discovered id becomes its dir"
        );
        assert_eq!(out[4], "other", "unknown ids pass through to the daemon");
        assert_eq!(out[5], prof.to_string_lossy(), "a dotted name is an id");
        assert_eq!(out[6], "default", "`.claude` is never the cwd's .claude");
        assert!(
            Path::new(&out[7]).is_absolute() && out[7].ends_with(".claude"),
            "an explicit ./ path is a path"
        );
    }
}
