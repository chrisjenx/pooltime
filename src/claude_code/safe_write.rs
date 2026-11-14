//! Guarded JSON patching for files Claude Code rewrites concurrently.

use std::collections::HashMap;
use std::fmt;
use std::path::Path;

use anyhow::{anyhow, Context};
use serde::de::{Deserializer, MapAccess, Visitor};
use serde::ser::{SerializeMap, Serializer};
use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;
use serde_json::{Map, Value};

use crate::fsutil::{guarded_patch, Patch, WriteOutcome};

/// Mode for a file we have to create: what Claude Code itself gets under the
/// default umask. Existing files keep their mode.
const NEW_FILE_MODE: u32 = 0o644;
const RETRIES: u8 = 5;

/// Patch the top-level JSON object in `path` (e.g. `settings.json`).
///
/// `f` edits the parsed object and returns whether it changed anything. It is
/// re-run on a fresh parse whenever the file changes under us before the
/// rename, so Claude Code's concurrent write is never lost. A missing file
/// yields an empty object (and is created if `f` changes it); a file that is
/// unreadable, not valid JSON, or not an object is an error and is never
/// overwritten. Key order and unknown fields are preserved; output is
/// 2-space indented with a trailing newline like Claude Code writes. Nothing
/// is written when the resulting bytes equal the current ones.
/// `Ok(GaveUp)` means the file kept changing for every attempt.
pub fn patch_json(
    path: &Path,
    f: impl FnMut(&mut Map<String, Value>) -> bool,
) -> anyhow::Result<WriteOutcome> {
    patch_json_mode(path, NEW_FILE_MODE, f)
}

/// [`patch_json`], creating a missing file with `new_mode` (e.g. 0600 when what we write is a
/// secret). An existing file keeps its mode either way.
pub fn patch_json_mode(
    path: &Path,
    new_mode: u32,
    mut f: impl FnMut(&mut Map<String, Value>) -> bool,
) -> anyhow::Result<WriteOutcome> {
    guarded_patch(path, new_mode, RETRIES, |current| {
        let mut obj = match current {
            None => Map::new(),
            Some(bytes) => match serde_json::from_slice::<Value>(bytes)
                .with_context(|| format!("{} is not valid JSON", path.display()))?
            {
                Value::Object(m) => m,
                _ => return Err(anyhow!("{} is not a JSON object", path.display())),
            },
        };
        if !f(&mut obj) {
            return Ok(Patch::Unchanged);
        }
        let mut out = serde_json::to_vec_pretty(&Value::Object(obj))?;
        out.push(b'\n');
        Ok(Patch::Write(out))
    })
}

/// [`patch_json_mode`] for a big file of which we own only a few top-level `keys`
/// (`.claude.json` can be megabytes, nearly all of it Claude Code's `projects`).
///
/// Only the values of `keys` are parsed and handed to `f`, as a map holding those of them that
/// are present, in file order. Every other top-level value is validated but kept as its exact
/// bytes, borrowed from the file just read, and written back verbatim. Memory therefore does not
/// grow with the file beyond the read and write buffers, and nothing we do not own can change.
/// The output is otherwise what [`patch_json_mode`] writes: 2-space indent, trailing newline, key
/// order kept, a key `f` adds goes last, the values of `keys` re-serialized as before. For a file
/// Claude Code wrote (2-space indented) the kept bytes are what [`patch_json_mode`] would
/// re-serialize, so the output is the same. The corrupt-file rule is unchanged: a file that is
/// not a valid JSON object is an error and never overwritten. `f` should touch only `keys`;
/// another key it inserts replaces that value in place.
pub fn patch_json_keys(
    path: &Path,
    new_mode: u32,
    keys: &[&str],
    mut f: impl FnMut(&mut Map<String, Value>) -> bool,
) -> anyhow::Result<WriteOutcome> {
    guarded_patch(path, new_mode, RETRIES, |current| {
        let top = match current {
            None => RawTop::default(),
            Some(bytes) => RawTop::parse(bytes, path)?,
        };
        let mut owned = Map::new();
        for (k, raw) in &top.entries {
            if keys.contains(&k.as_str()) {
                let v = serde_json::from_str(raw.get())
                    .with_context(|| format!("{} is not valid JSON", path.display()))?;
                owned.insert(k.clone(), v);
            }
        }
        if !f(&mut owned) {
            return Ok(Patch::Unchanged);
        }
        let mut slots = Vec::with_capacity(top.entries.len() + owned.len());
        for (k, raw) in &top.entries {
            match owned.get(k) {
                Some(v) => slots.push((k.as_str(), Slot::Parsed(v))),
                None if keys.contains(&k.as_str()) => {} // removed by `f`
                None => slots.push((k.as_str(), Slot::Raw(raw))),
            }
        }
        for (k, v) in &owned {
            if !top.index.contains_key(k) {
                slots.push((k.as_str(), Slot::Parsed(v)));
            }
        }
        let mut out = serde_json::to_vec_pretty(&Slots(slots))?;
        out.push(b'\n');
        Ok(Patch::Write(out))
    })
}

/// The top-level value of `key` in the JSON object at `path`, without building any other value.
/// `None` when the file is missing, unreadable, not a valid JSON object, or lacks the key.
pub(crate) fn read_top_key(path: &Path, key: &str) -> Option<Value> {
    let bytes = std::fs::read(path).ok()?;
    let top = RawTop::parse(&bytes, path).ok()?;
    let raw = top.entries.get(*top.index.get(key)?)?.1;
    serde_json::from_str(raw.get()).ok()
}

/// A top-level JSON object read without building its values: each key with the exact bytes of
/// its value (borrowed from the input), in file order. A repeated key keeps its first position
/// and its last value, as a parse into [`Map`] does.
#[derive(Default)]
struct RawTop<'a> {
    entries: Vec<(String, &'a RawValue)>,
    index: HashMap<String, usize>,
}

impl<'a> RawTop<'a> {
    fn parse(bytes: &'a [u8], path: &Path) -> anyhow::Result<RawTop<'a>> {
        serde_json::from_slice(bytes).map_err(|e| match e.classify() {
            serde_json::error::Category::Data => {
                anyhow!("{} is not a JSON object", path.display())
            }
            _ => anyhow!(e).context(format!("{} is not valid JSON", path.display())),
        })
    }
}

impl<'de> Deserialize<'de> for RawTop<'de> {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct TopVisitor;
        impl<'de> Visitor<'de> for TopVisitor {
            type Value = RawTop<'de>;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("a JSON object")
            }
            fn visit_map<A: MapAccess<'de>>(self, mut m: A) -> Result<RawTop<'de>, A::Error> {
                let mut top = RawTop::default();
                while let Some((k, v)) = m.next_entry::<String, &'de RawValue>()? {
                    match top.index.get(&k) {
                        Some(&i) => top.entries[i].1 = v,
                        None => {
                            top.index.insert(k.clone(), top.entries.len());
                            top.entries.push((k, v));
                        }
                    }
                }
                Ok(top)
            }
        }
        d.deserialize_map(TopVisitor)
    }
}

/// One top-level value to write: kept bytes, or a value we own.
enum Slot<'a> {
    Raw(&'a RawValue),
    Parsed(&'a Value),
}

/// The object to write, in order.
struct Slots<'a>(Vec<(&'a str, Slot<'a>)>);

impl Serialize for Slots<'_> {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let mut m = s.serialize_map(Some(self.0.len()))?;
        for (k, v) in &self.0 {
            match v {
                Slot::Raw(r) => m.serialize_entry(k, r)?,
                Slot::Parsed(v) => m.serialize_entry(k, v)?,
            }
        }
        m.end()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::os::unix::fs::PermissionsExt;

    const KEYS: [&str; 2] = ["additionalModelOptionsCache", "mcpServers"];

    /// One `projects` entry of realistic shape, with what stresses a byte-exact round trip:
    /// unicode, escapes, nested arrays and numbers only `arbitrary_precision` keeps as written.
    fn project(i: usize) -> Value {
        let mut v = json!({
            "allowedTools": ["Bash(cargo fmt:*)", "Bash(cargo test:*)", "WebFetch(domain:docs.rs)"],
            "history": (0..6).map(|h| json!({
                "display": format!("fix the ✓ build #{h} in 漢字 dir — \"quoted\" \\ back\tslash \u{1F389}"),
                "pastedContents": {"1": {"id": 1, "type": "text", "content": "line1\nline2\u{0001}"}},
            })).collect::<Vec<_>>(),
            "mcpContextUris": [],
            "mcpServers": {"local": {"type": "stdio", "command": "npx", "args": ["-y", "pkg@1.2.3"]}},
            "nested": [[1, [2, [3, []]]], {"a": [{"b": null}]}],
            "hasTrustDialogAccepted": i.is_multiple_of(2),
            "lastSessionId": format!("00000000-0000-4000-8000-{i:012}"),
        });
        let big: Value = serde_json::from_str(
            r#"{"lastCost": 0.12345678901234567890123, "lastTotalInputTokens": 123456789012345678901234567890,
                "exp": 1.5e400, "neg": -0.0, "frac": 1.10}"#,
        )
        .unwrap();
        v.as_object_mut()
            .unwrap()
            .extend(big.as_object().unwrap().clone());
        v
    }

    /// A `.claude.json` the way Claude Code writes it (2-space indent, trailing newline), about
    /// `target` bytes, plus top-level values in formats our serializer would never produce (which
    /// must survive byte for byte). `url` is the pool's MCP entry.
    fn claude_json(target: usize, url: &str) -> Vec<u8> {
        let mut projects = Map::new();
        let mut size = 0;
        let mut i = 0;
        while size < target {
            let p = project(i);
            size += serde_json::to_vec_pretty(&p).unwrap().len() + 64;
            projects.insert(format!("/Users/me/src/projekt-ü-{i:05}"), p);
            i += 1;
        }
        let doc = json!({
            "numStartups": 812,
            "theme": "dark",
            "projects": projects,
            "mcpServers": {
                "other": {"type": "stdio", "command": "x"},
                "pooltime": {"type": "http", "url": url, "headers": {"Authorization": "Bearer t"}},
            },
            "additionalModelOptionsCache": [
                {"value": "claude-fable-5[1m]", "label": "Fable", "description": "Fable 5 · Most capable"},
            ],
        });
        let mut text = serde_json::to_string_pretty(&doc).unwrap();
        assert!(text.ends_with("\n}"));
        text.truncate(text.len() - 2);
        // Escapes and spacing serde_json would normalize away, inside untouched values.
        text.push_str(
            ",\n  \"weird\": {\"a\":[1,2 ,3],  \"b\" : \"\\u00e9\\/\\uD83C\\uDF89\", \"n\": 1E+2},\n  \"tail\": 100000000000000000000000000000001\n}\n",
        );
        text.into_bytes()
    }

    fn write(dir: &Path, bytes: &[u8]) -> std::path::PathBuf {
        let path = dir.join(".claude.json");
        std::fs::write(&path, bytes).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        path
    }

    fn set_url(obj: &mut Map<String, Value>, url: &str) -> bool {
        obj["mcpServers"]["pooltime"]["url"] = Value::String(url.into());
        true
    }

    #[test]
    fn large_file_round_trips_byte_for_byte() {
        let dir = tempfile::tempdir().unwrap();
        let before = claude_json(5 << 20, "http://127.0.0.1:1/mcp");
        assert!(before.len() >= 5 << 20);
        let path = write(dir.path(), &before);
        // `f` sees only our keys.
        let out = patch_json_keys(&path, 0o600, &KEYS, |obj| {
            assert_eq!(
                obj.keys().collect::<Vec<_>>(),
                ["mcpServers", "additionalModelOptionsCache"]
            );
            false
        })
        .unwrap();
        assert_eq!(out, WriteOutcome::Unchanged);
        // Even a pass that claims a change but leaves the values alone re-serializes to the same
        // bytes, so nothing is written.
        let out = patch_json_keys(&path, 0o600, &KEYS, |_| true).unwrap();
        assert_eq!(out, WriteOutcome::Unchanged);
        assert_eq!(std::fs::read(&path).unwrap(), before);
    }

    #[test]
    fn changing_our_key_changes_only_its_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let before = claude_json(5 << 20, "http://127.0.0.1:1/mcp");
        let path = write(dir.path(), &before);
        let out = patch_json_keys(&path, 0o644, &KEYS, |obj| {
            set_url(obj, "http://127.0.0.1:2/mcp")
        })
        .unwrap();
        assert_eq!(out, WriteOutcome::Written);
        let after = std::fs::read(&path).unwrap();
        assert_eq!(after, claude_json(5 << 20, "http://127.0.0.1:2/mcp"));
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "an existing file keeps its mode");
        // Exactly one line differs.
        let (b, a) = (
            String::from_utf8(before).unwrap(),
            String::from_utf8(after).unwrap(),
        );
        assert_eq!(b.lines().count(), a.lines().count());
        let diff: Vec<_> = b.lines().zip(a.lines()).filter(|(x, y)| x != y).collect();
        assert_eq!(diff.len(), 1);
    }

    #[test]
    fn output_matches_the_full_parse_writer() {
        // For a Claude Code-written file the key-scoped writer and the whole-file writer agree,
        // including a key `f` adds (last) and one it removes.
        let src = claude_json(64 << 10, "http://127.0.0.1:1/mcp");
        let mut src =
            serde_json::to_vec_pretty(&serde_json::from_slice::<Value>(&src).unwrap()).unwrap();
        src.push(b'\n');
        let edit = |obj: &mut Map<String, Value>| {
            set_url(obj, "http://127.0.0.1:3/mcp");
            obj.shift_remove("additionalModelOptionsCache");
            obj.insert("newKey".into(), json!({"x": [1, 2]}));
            true
        };
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.json");
        let b = dir.path().join("b.json");
        std::fs::write(&a, &src).unwrap();
        std::fs::write(&b, &src).unwrap();
        patch_json_mode(&a, 0o600, edit).unwrap();
        let keys = ["mcpServers", "additionalModelOptionsCache", "newKey"];
        patch_json_keys(&b, 0o600, &keys, edit).unwrap();
        assert_ne!(std::fs::read(&a).unwrap(), src);
        assert_eq!(std::fs::read(&a).unwrap(), std::fs::read(&b).unwrap());
    }

    #[test]
    fn corrupt_or_non_object_files_are_never_written() {
        let dir = tempfile::tempdir().unwrap();
        for bad in [
            "{\"a\": [1, }",
            "[1, 2]",
            "{\"mcpServers\": {}} trailing",
            "",
        ] {
            let path = write(dir.path(), bad.as_bytes());
            let err = patch_json_keys(&path, 0o600, &KEYS, |_| true).unwrap_err();
            assert!(format!("{err:#}").contains(".claude.json"), "{err:#}");
            assert_eq!(std::fs::read(&path).unwrap(), bad.as_bytes());
        }
    }

    #[test]
    fn missing_file_is_created_with_new_mode_only_on_change() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".claude.json");
        assert_eq!(
            patch_json_keys(&path, 0o600, &KEYS, |_| false).unwrap(),
            WriteOutcome::Unchanged
        );
        assert!(!path.exists());
        patch_json_keys(&path, 0o600, &KEYS, |obj| {
            obj.insert("mcpServers".into(), json!({}));
            true
        })
        .unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "{\n  \"mcpServers\": {}\n}\n"
        );
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[test]
    fn repeated_key_keeps_first_position_and_last_value() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(dir.path(), b"{\"a\": 1, \"mcpServers\": {}, \"a\": 2}");
        patch_json_keys(&path, 0o600, &KEYS, |obj| {
            obj.insert("mcpServers".into(), json!({"x": 1}));
            true
        })
        .unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "{\n  \"a\": 2,\n  \"mcpServers\": {\n    \"x\": 1\n  }\n}\n"
        );
    }

    #[test]
    fn read_top_key_parses_one_value() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(dir.path(), br#"{"env": {"A": "1"}, "big": [1, 2, 3]}"#);
        assert_eq!(read_top_key(&path, "env"), Some(json!({"A": "1"})));
        assert_eq!(read_top_key(&path, "nope"), None);
        let path = write(dir.path(), b"{\"env\": ");
        assert_eq!(read_top_key(&path, "env"), None);
        assert_eq!(read_top_key(&dir.path().join("missing"), "env"), None);
    }
}
