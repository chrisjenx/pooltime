//! The pool's MCP registration in a profile's `.claude.json` (spec §16): one entry,
//! `mcpServers.pooltime = {"type":"http","url":"http://<pool>/mcp","headers":{"Authorization":
//! "Bearer <MCP token>"}}`, written with the guarded JSON patcher (key order kept, an existing
//! file keeps its mode, a file we create is 0600 since it then holds the token). Only our key is
//! ever touched: other servers stay as they are, and removing ours from a file that lacks it
//! writes nothing. The token goes into that file only, never to logs.
//!
//! The profile snapshot records `mcp_registered`, so the background sync opens `.claude.json`
//! for the entry only in profiles that have one ([`is_registered`], [`refresh_entry`]).

use anyhow::anyhow;
use serde_json::{json, Map, Value};

use super::snapshot::{self, SnapshotState};
use super::{patch_json, patch_json_mode, Profile};
use crate::config::Paths;
use crate::fsutil::WriteOutcome;

/// Our key under `mcpServers`.
pub const MCP_SERVER_NAME: &str = "pooltime";

/// Mode of a `.claude.json` we create (it holds the MCP token).
const NEW_FILE_MODE: u32 = 0o600;

/// The entry Claude Code's http transport reads.
pub fn mcp_entry(url: &str, token: &str) -> Value {
    json!({
        "type": "http",
        "url": url,
        "headers": { "Authorization": format!("Bearer {token}") },
    })
}

/// Put `entry` under `mcpServers.pooltime` in a parsed `.claude.json`: `Some(changed)`, or
/// `None` when `mcpServers` exists but is not an object (left alone). With `only_if_present` an
/// absent entry stays absent.
fn put_entry(obj: &mut Map<String, Value>, entry: &Value, only_if_present: bool) -> Option<bool> {
    if only_if_present
        && obj
            .get("mcpServers")
            .and_then(|s| s.get(MCP_SERVER_NAME))
            .is_none()
    {
        return Some(false);
    }
    let Value::Object(servers) = obj
        .entry("mcpServers")
        .or_insert_with(|| Value::Object(Map::new()))
    else {
        return None;
    };
    if servers.get(MCP_SERVER_NAME) == Some(entry) {
        return Some(false);
    }
    servers.insert(MCP_SERVER_NAME.into(), entry.clone());
    Some(true)
}

/// Bring an existing entry in a parsed `.claude.json` up to date (a moved port); never adds one.
/// Whether it changed anything. For the sync's single pass over the file.
pub(super) fn refresh_entry(obj: &mut Map<String, Value>, url: &str, token: &str) -> bool {
    put_entry(obj, &mcp_entry(url, token), true) == Some(true)
}

/// Whether `p`'s snapshot says the pool is registered there.
pub(crate) fn is_registered(paths: &Paths, p: &Profile) -> bool {
    matches!(
        snapshot::read(&snapshot::snapshot_path(paths, p)),
        SnapshotState::Present(s) if s.mcp_registered
    )
}

/// Record `registered` in `p`'s snapshot (written only when it changes). No snapshot (not
/// connected) records nothing; an unreadable one is left alone.
fn mark(paths: &Paths, p: &Profile, registered: bool) -> anyhow::Result<()> {
    let path = snapshot::snapshot_path(paths, p);
    if let SnapshotState::Present(mut s) = snapshot::read(&path) {
        if s.mcp_registered != registered {
            s.mcp_registered = registered;
            snapshot::write(&path, &s)?;
        }
    }
    Ok(())
}

/// Register the pool at `url` with `token` (idempotent) and mark the snapshot.
pub fn register(
    paths: &Paths,
    p: &Profile,
    url: &str,
    token: &str,
) -> anyhow::Result<WriteOutcome> {
    let entry = mcp_entry(url, token);
    let mut foreign = false;
    let out = patch_json_mode(&p.claude_json, NEW_FILE_MODE, |obj| {
        let r = put_entry(obj, &entry, false);
        foreign = r.is_none();
        r == Some(true)
    })?;
    if foreign {
        return Err(anyhow!(
            "{}: mcpServers is not an object; not changed",
            p.claude_json.display()
        ));
    }
    if out != WriteOutcome::GaveUp {
        mark(paths, p, true)?;
    }
    Ok(out)
}

/// Remove `mcpServers.pooltime` and nothing else (absent, or no file = `Unchanged`), and clear
/// the snapshot mark.
pub fn unregister(paths: &Paths, p: &Profile) -> anyhow::Result<WriteOutcome> {
    let out = patch_json(&p.claude_json, |obj| {
        obj.get_mut("mcpServers")
            .and_then(Value::as_object_mut)
            .and_then(|s| s.shift_remove(MCP_SERVER_NAME))
            .is_some()
    })?;
    if out != WriteOutcome::GaveUp {
        mark(paths, p, false)?;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn register_and_unregister() {
        let home = tempfile::tempdir().unwrap();
        let paths = Paths::under(&home.path().join("pool"));
        let p = Profile::from_dir(&home.path().join(".claude-t"), home.path());
        std::fs::create_dir_all(&p.settings_dir).unwrap();
        let snap = snapshot::snapshot_path(&paths, &p);
        snapshot::write(&snap, &snapshot::Snapshot::new(Map::new())).unwrap();

        // No file: unregister creates nothing.
        assert_eq!(unregister(&paths, &p).unwrap(), WriteOutcome::Unchanged);
        assert!(!p.claude_json.exists());
        assert!(!is_registered(&paths, &p));

        // A file we create holds the token: 0600.
        assert_eq!(
            register(&paths, &p, "u1", "t").unwrap(),
            WriteOutcome::Written
        );
        let mode = std::fs::metadata(&p.claude_json)
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
        assert!(is_registered(&paths, &p));
        assert_eq!(
            register(&paths, &p, "u1", "t").unwrap(),
            WriteOutcome::Unchanged
        );

        let mut obj: Map<String, Value> =
            serde_json::from_slice(&std::fs::read(&p.claude_json).unwrap()).unwrap();
        assert!(refresh_entry(&mut obj, "u2", "t"));
        assert_eq!(obj["mcpServers"]["pooltime"]["url"], "u2");
        assert!(!refresh_entry(&mut Map::new(), "u2", "t"), "never adds");

        assert_eq!(unregister(&paths, &p).unwrap(), WriteOutcome::Written);
        assert!(!is_registered(&paths, &p));
        assert_eq!(unregister(&paths, &p).unwrap(), WriteOutcome::Unchanged);

        // An existing file keeps its mode.
        std::fs::write(&p.claude_json, "{}").unwrap();
        std::fs::set_permissions(&p.claude_json, std::fs::Permissions::from_mode(0o640)).unwrap();
        register(&paths, &p, "u", "t").unwrap();
        let mode = std::fs::metadata(&p.claude_json)
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o640);

        // A foreign-shaped mcpServers is refused, never overwritten.
        std::fs::write(&p.claude_json, r#"{"mcpServers": [1]}"#).unwrap();
        assert!(register(&paths, &p, "u", "t").is_err());
        assert_eq!(
            std::fs::read_to_string(&p.claude_json).unwrap(),
            r#"{"mcpServers": [1]}"#
        );
    }
}
