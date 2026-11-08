//! Claude Code profiles: a settings directory plus its `.claude.json`.

use std::path::{Path, PathBuf};

/// Directory name of bro's pool config dir; never a Claude Code profile.
const POOL_DIR: &str = ".claude-max-pool";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Profile {
    /// Basename of the settings dir without leading dots; `default` for `~/.claude`.
    pub id: String,
    pub settings_dir: PathBuf,
    pub settings_json: PathBuf,
    pub claude_json: PathBuf,
}

impl Profile {
    /// `home` is passed explicitly (never read from the environment). The
    /// default profile `home/.claude` keeps its state in `home/.claude.json`;
    /// any other dir `D` uses `D/.claude.json`.
    pub fn from_dir(dir: &Path, home: &Path) -> Profile {
        let (id, claude_json) = if dir == home.join(".claude") {
            ("default".to_string(), home.join(".claude.json"))
        } else {
            let base = dir
                .file_name()
                .map(|n| n.to_string_lossy().trim_start_matches('.').to_string())
                .unwrap_or_default();
            let id = if base.is_empty() {
                "profile".to_string()
            } else {
                base
            };
            (id, dir.join(".claude.json"))
        };
        Profile {
            id,
            settings_dir: dir.to_path_buf(),
            settings_json: dir.join("settings.json"),
            claude_json,
        }
    }
}

/// `home/.claude` and `home/.claude-*` directories that contain a
/// `settings.json`, excluding bro's `.claude-max-pool`, sorted by directory
/// name (so `.claude` comes first).
pub fn discover(home: &Path) -> Vec<Profile> {
    let Ok(entries) = std::fs::read_dir(home) else {
        return Vec::new();
    };
    let mut dirs: Vec<(String, PathBuf)> = entries
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            let candidate = name == ".claude" || name.starts_with(".claude-");
            if !candidate || name == POOL_DIR {
                return None;
            }
            let path = e.path();
            // `metadata` follows symlinks, so a symlinked profile dir counts.
            let is_dir = std::fs::metadata(&path)
                .map(|m| m.is_dir())
                .unwrap_or(false);
            let has_settings = std::fs::metadata(path.join("settings.json"))
                .map(|m| m.is_file())
                .unwrap_or(false);
            (is_dir && has_settings).then_some((name, path))
        })
        .collect();
    dirs.sort_by(|a, b| a.0.cmp(&b.0));
    dirs.into_iter()
        .map(|(_, p)| Profile::from_dir(&p, home))
        .collect()
}
