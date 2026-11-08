//! Guarded JSON patching for files Claude Code rewrites concurrently.

use std::path::Path;

use anyhow::{anyhow, Context};
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
    mut f: impl FnMut(&mut Map<String, Value>) -> bool,
) -> anyhow::Result<WriteOutcome> {
    guarded_patch(path, NEW_FILE_MODE, RETRIES, |current| {
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
