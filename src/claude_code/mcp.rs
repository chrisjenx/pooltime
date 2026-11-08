//! The pool's MCP registration in a profile's `.claude.json` (spec §16): one entry,
//! `mcpServers.pooltime = {"type":"http","url":"http://<pool>/mcp","headers":{"Authorization":
//! "Bearer <MCP token>"}}`, written with [`patch_json`] (guarded, key order kept, existing file
//! mode kept). Only our key is ever touched: other servers stay as they are, and removing ours
//! from a file that lacks it writes nothing. The token goes into that file only, never to logs.

use anyhow::anyhow;
use serde_json::{json, Map, Value};

use super::{patch_json, Profile};
use crate::fsutil::WriteOutcome;

/// Our key under `mcpServers`.
pub const MCP_SERVER_NAME: &str = "pooltime";

/// The entry Claude Code's http transport reads.
pub fn mcp_entry(url: &str, token: &str) -> Value {
    json!({
        "type": "http",
        "url": url,
        "headers": { "Authorization": format!("Bearer {token}") },
    })
}

/// Write `entry` under `mcpServers.pooltime`; with `only_if_present` an absent entry is left
/// absent (the background re-sync). A `mcpServers` that is not an object is an error, never
/// overwritten.
fn put(p: &Profile, entry: &Value, only_if_present: bool) -> anyhow::Result<WriteOutcome> {
    let mut foreign = false;
    let out = patch_json(&p.claude_json, |obj| {
        foreign = false;
        if only_if_present
            && obj
                .get("mcpServers")
                .and_then(|s| s.get(MCP_SERVER_NAME))
                .is_none()
        {
            return false;
        }
        let servers = obj
            .entry("mcpServers")
            .or_insert_with(|| Value::Object(Map::new()));
        let Value::Object(servers) = servers else {
            foreign = true;
            return false;
        };
        if servers.get(MCP_SERVER_NAME) == Some(entry) {
            return false;
        }
        servers.insert(MCP_SERVER_NAME.into(), entry.clone());
        true
    })?;
    if foreign {
        return Err(anyhow!(
            "{}: mcpServers is not an object; not changed",
            p.claude_json.display()
        ));
    }
    Ok(out)
}

/// Register the pool at `url` with `token` (idempotent).
pub fn register(p: &Profile, url: &str, token: &str) -> anyhow::Result<WriteOutcome> {
    put(p, &mcp_entry(url, token), false)
}

/// Bring an existing registration up to date (a moved port); never adds one.
pub fn refresh(p: &Profile, url: &str, token: &str) -> anyhow::Result<WriteOutcome> {
    put(p, &mcp_entry(url, token), true)
}

/// Remove `mcpServers.pooltime` and nothing else; absent (or no file) = `Unchanged`.
pub fn unregister(p: &Profile) -> anyhow::Result<WriteOutcome> {
    patch_json(&p.claude_json, |obj| {
        obj.get_mut("mcpServers")
            .and_then(Value::as_object_mut)
            .and_then(|s| s.shift_remove(MCP_SERVER_NAME))
            .is_some()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn profile(dir: &std::path::Path) -> Profile {
        Profile::from_dir(&dir.join(".claude-t"), dir)
    }

    #[test]
    fn register_refresh_unregister() {
        let home = tempfile::tempdir().unwrap();
        let p = profile(home.path());
        std::fs::create_dir_all(&p.settings_dir).unwrap();
        // No file: unregister and refresh create nothing.
        assert_eq!(unregister(&p).unwrap(), WriteOutcome::Unchanged);
        assert_eq!(refresh(&p, "u", "t").unwrap(), WriteOutcome::Unchanged);
        assert!(!p.claude_json.exists());

        assert_eq!(register(&p, "u1", "t").unwrap(), WriteOutcome::Written);
        assert_eq!(register(&p, "u1", "t").unwrap(), WriteOutcome::Unchanged);
        assert_eq!(refresh(&p, "u2", "t").unwrap(), WriteOutcome::Written);
        let v: Value = serde_json::from_slice(&std::fs::read(&p.claude_json).unwrap()).unwrap();
        assert_eq!(v["mcpServers"]["pooltime"]["url"], "u2");
        assert_eq!(unregister(&p).unwrap(), WriteOutcome::Written);
        assert_eq!(unregister(&p).unwrap(), WriteOutcome::Unchanged);

        // A foreign-shaped mcpServers is refused, never overwritten.
        std::fs::write(&p.claude_json, r#"{"mcpServers": [1]}"#).unwrap();
        assert!(register(&p, "u", "t").is_err());
        assert_eq!(
            std::fs::read_to_string(&p.claude_json).unwrap(),
            r#"{"mcpServers": [1]}"#
        );
    }
}
