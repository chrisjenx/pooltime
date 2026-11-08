//! Refresh Claude Code's cached Fable picker rows (INV §6; port of bro
//! `settings.js` `refreshCachedFableRow{,File}`).
//!
//! The Fable row is not built in: it is `additionalModelOptionsCache` in the
//! profile's `.claude.json`, written by a bootstrap call that needs a live
//! claude.ai login. Behind the pool it never refreshes, so the row goes stale.
//! Pinning `ANTHROPIC_DEFAULT_FABLE_MODEL` would add a second row instead, so
//! the existing `claude-fable-*` rows are rewritten in place: only the version
//! parts change (id, and the description's leading name), the `[1m]` suffix and
//! everything else stay.

use serde_json::{Map, Value};

use super::pins::{newest_fable, Newest};
use super::{patch_json, Profile};
use crate::fsutil::WriteOutcome;

/// Rewrite the stale `claude-fable-*` rows in `p`'s `.claude.json` to the newest
/// Fable in `catalog`, through the guarded writer (`.claude.json` is rewritten
/// by running Claude Code). Nothing is written, and no file is created, when the
/// catalog has no Fable, the file is missing, has no cached rows, or the rows are
/// already current. A file that is not a JSON object is an error and untouched.
pub fn rewrite_fable_rows(p: &Profile, catalog: &[Value]) -> anyhow::Result<WriteOutcome> {
    let Some(fable) = newest_fable(catalog) else {
        return Ok(WriteOutcome::Unchanged);
    };
    patch_json(&p.claude_json, |obj| refresh_rows(obj, &fable))
}

/// Rewrite stale Fable rows of a parsed `.claude.json`; whether anything changed.
pub(super) fn refresh_rows(obj: &mut Map<String, Value>, fable: &Newest) -> bool {
    let Some(Value::Array(rows)) = obj.get_mut("additionalModelOptionsCache") else {
        return false;
    };
    let mut changed = false;
    for row in rows.iter_mut().filter_map(Value::as_object_mut) {
        let Some(value) = row.get("value").and_then(Value::as_str) else {
            continue;
        };
        if !is_fable_id(value) {
            continue;
        }
        let (base, suffix) = match value.strip_suffix("[1m]") {
            Some(b) => (b, "[1m]"),
            None => (value, ""),
        };
        let id_chars = base
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'));
        if !id_chars || base == fable.id {
            continue;
        }
        let old_name = old_name(row).unwrap_or_else(|| base.to_string());
        let new_value = format!("{}{suffix}", fable.id);
        let new_description = row
            .get("description")
            .and_then(Value::as_str)
            .filter(|d| !old_name.is_empty() && d.starts_with(&old_name))
            .map(|d| format!("{}{}", fable.name, &d[old_name.len()..]));
        row.insert("value".into(), Value::String(new_value));
        if let Some(d) = new_description {
            row.insert("description".into(), Value::String(d));
        }
        changed = true;
    }
    changed
}

/// `^claude-fable-\d`
fn is_fable_id(v: &str) -> bool {
    v.strip_prefix("claude-fable-")
        .is_some_and(|r| r.starts_with(|c: char| c.is_ascii_digit()))
}

/// The description's leading name: `"Fable 5 · Most capable…"` -> `"Fable 5"`.
fn old_name(row: &Map<String, Value>) -> Option<String> {
    let d = row.get("description")?.as_str()?;
    let head = d[..d.find('·')?].trim_end();
    head.starts_with("Fable").then(|| head.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn profile(dir: &std::path::Path) -> Profile {
        Profile::from_dir(&dir.join(".claude-work"), dir)
    }

    // bro: refreshCachedFableRow rewrites only the version-bearing parts of a stale row
    #[test]
    fn fable_rows_rewritten_in_place_only() {
        let fable = Newest {
            id: "claude-fable-5-1".into(),
            name: "Fable 5.1".into(),
        };
        let mut json = json!({
            "additionalModelOptionsCache": [
                {"value": "claude-fable-5[1m]", "label": "Fable",
                 "description": "Fable 5 · Most capable for your hardest and longest-running tasks · $10/$50 per Mtok"},
                {"value": "claude-opus-5", "label": "Opus", "description": "Opus 5 · x"},
                {"value": "claude-fable-5", "description": "no separator"},
                {"value": "claude-fable-x", "description": "not a fable id"},
                7
            ],
            "additionalModelOptionsAnsweredAt": 123
        });
        let Value::Object(obj) = &mut json else {
            unreachable!()
        };
        assert!(refresh_rows(obj, &fable));
        let rows = &json["additionalModelOptionsCache"];
        assert_eq!(
            rows[0],
            json!({"value": "claude-fable-5-1[1m]", "label": "Fable",
                   "description": "Fable 5.1 · Most capable for your hardest and longest-running tasks · $10/$50 per Mtok"})
        );
        assert_eq!(
            rows[1],
            json!({"value": "claude-opus-5", "label": "Opus", "description": "Opus 5 · x"})
        );
        // No `·`: the old name falls back to the id, which the description does not start with.
        assert_eq!(
            rows[2],
            json!({"value": "claude-fable-5-1", "description": "no separator"})
        );
        assert_eq!(rows[3]["value"], "claude-fable-x");
        assert_eq!(json["additionalModelOptionsAnsweredAt"], 123);
        assert_eq!(rows.as_array().unwrap().len(), 5);
        // Already current / no cache: untouched.
        let Value::Object(obj) = &mut json else {
            unreachable!()
        };
        assert!(!refresh_rows(obj, &fable));
        assert!(!refresh_rows(&mut Map::new(), &fable));
    }

    // bro: refreshCachedFableRowFile rewrites .claude.json in place and leaves a missing file alone
    #[test]
    fn rewrite_fable_rows_updates_claude_json_and_leaves_missing_file_alone() {
        let dir = tempfile::tempdir().unwrap();
        let p = profile(dir.path());
        std::fs::create_dir_all(&p.settings_dir).unwrap();
        std::fs::write(
            &p.claude_json,
            json!({"other": true, "additionalModelOptionsCache": [
                {"value": "claude-fable-5[1m]", "label": "Fable", "description": "Fable 5 · Most capable"}
            ]})
            .to_string(),
        )
        .unwrap();
        let catalog = [json!({"id": "claude-fable-5-1", "display_name": "Claude Fable 5.1"})];
        assert_eq!(
            rewrite_fable_rows(&p, &catalog).unwrap(),
            WriteOutcome::Written
        );
        let out: Value = serde_json::from_slice(&std::fs::read(&p.claude_json).unwrap()).unwrap();
        assert_eq!(out["other"], true);
        assert_eq!(
            out["additionalModelOptionsCache"][0]["value"],
            "claude-fable-5-1[1m]"
        );
        assert_eq!(
            out["additionalModelOptionsCache"][0]["description"],
            "Fable 5.1 · Most capable"
        );
        assert_eq!(
            rewrite_fable_rows(&p, &catalog).unwrap(),
            WriteOutcome::Unchanged
        );
        // No Fable in the catalog: untouched.
        let before = std::fs::read(&p.claude_json).unwrap();
        assert_eq!(
            rewrite_fable_rows(&p, &[json!({"id": "claude-opus-5"})]).unwrap(),
            WriteOutcome::Unchanged
        );
        assert_eq!(std::fs::read(&p.claude_json).unwrap(), before);
        // Missing file: nothing created.
        let q = Profile::from_dir(&dir.path().join(".claude-other"), dir.path());
        assert_eq!(
            rewrite_fable_rows(&q, &catalog).unwrap(),
            WriteOutcome::Unchanged
        );
        assert!(!q.claude_json.exists());
    }

    #[test]
    fn rewrite_fable_rows_refuses_corrupt_file() {
        let dir = tempfile::tempdir().unwrap();
        let p = profile(dir.path());
        std::fs::create_dir_all(&p.settings_dir).unwrap();
        std::fs::write(&p.claude_json, "{not json").unwrap();
        let catalog = [json!({"id": "claude-fable-5-1"})];
        assert!(rewrite_fable_rows(&p, &catalog).is_err());
        assert_eq!(
            std::fs::read_to_string(&p.claude_json).unwrap(),
            "{not json"
        );
    }
}
