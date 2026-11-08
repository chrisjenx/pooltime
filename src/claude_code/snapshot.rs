//! Per-profile snapshot of the user's pre-pooltime `env` values, at
//! `<run>/profiles/<profile-id>.json`: `{managed, prior: {KEY: value|null},
//! statusline_prior}`. Shaped like bro's `pool-settings*.json` so a snapshot bro
//! wrote still round-trips; unknown top-level fields are preserved on rewrite.

use std::fs::{self, DirBuilder};
use std::io;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

use super::Profile;
use crate::config::Paths;
use crate::fsutil::atomic_write;

const FILE_MODE: u32 = 0o600;
const DIR_MODE: u32 = 0o700;

/// `<run>/profiles/<profile-id>.json`. Profile ids never carry leading dots, so
/// the name is not dot-mangled, and two profiles never share a file.
pub fn snapshot_path(snaps: &Paths, p: &Profile) -> PathBuf {
    snaps.run.join("profiles").join(format!("{}.json", p.id))
}

/// `Debug` is hand-written: `prior` may hold the user's real
/// `ANTHROPIC_AUTH_TOKEN`, so only key names are ever shown.
#[derive(Clone, PartialEq)]
pub(crate) struct Snapshot {
    pub managed: bool,
    /// Managed key -> the user's value before us (`null` = was absent).
    pub prior: Map<String, Value>,
    /// Reserved for the status line override; nothing sets it yet, but it is
    /// preserved across rewrites.
    pub statusline_prior: Value,
    /// Any other top-level fields, kept verbatim.
    extra: Map<String, Value>,
}

impl std::fmt::Debug for Snapshot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Snapshot")
            .field("managed", &self.managed)
            .field("prior_keys", &self.prior.keys().collect::<Vec<_>>())
            .field("has_statusline_prior", &!self.statusline_prior.is_null())
            .field("extra_keys", &self.extra.keys().collect::<Vec<_>>())
            .finish()
    }
}

impl Snapshot {
    pub fn new(prior: Map<String, Value>) -> Snapshot {
        Snapshot {
            managed: true,
            prior,
            statusline_prior: Value::Null,
            extra: Map::new(),
        }
    }

    fn from_value(v: Value) -> Option<Snapshot> {
        let Value::Object(mut obj) = v else {
            return None;
        };
        let managed = obj.remove("managed").and_then(|v| v.as_bool()) == Some(true);
        let prior = match obj.remove("prior") {
            Some(Value::Object(m)) => m,
            _ => Map::new(),
        };
        let statusline_prior = obj.remove("statusline_prior").unwrap_or(Value::Null);
        Some(Snapshot {
            managed,
            prior,
            statusline_prior,
            extra: obj,
        })
    }

    fn to_bytes(&self) -> Vec<u8> {
        let mut obj = self.extra.clone();
        obj.insert("managed".into(), Value::Bool(self.managed));
        obj.insert("prior".into(), Value::Object(self.prior.clone()));
        obj.insert("statusline_prior".into(), self.statusline_prior.clone());
        let mut out = serde_json::to_vec_pretty(&Value::Object(obj)).expect("json value");
        out.push(b'\n');
        out
    }
}

#[derive(Debug)]
pub(crate) enum SnapshotState {
    Absent,
    /// A file is there but cannot be read or parsed as a JSON object. Callers
    /// must leave it alone.
    Unreadable,
    Present(Box<Snapshot>),
}

pub(crate) fn read(path: &Path) -> SnapshotState {
    let bytes = match fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return SnapshotState::Absent,
        Err(_) => return SnapshotState::Unreadable,
    };
    match serde_json::from_slice::<Value>(&bytes)
        .ok()
        .and_then(Snapshot::from_value)
    {
        Some(s) => SnapshotState::Present(Box::new(s)),
        None => SnapshotState::Unreadable,
    }
}

/// Atomically write the snapshot (0600), creating `run/profiles` (0700) first.
pub(crate) fn write(path: &Path, snap: &Snapshot) -> io::Result<()> {
    if let Some(dir) = path.parent() {
        DirBuilder::new()
            .recursive(true)
            .mode(DIR_MODE)
            .create(dir)?;
    }
    atomic_write(path, &snap.to_bytes(), FILE_MODE)
}

/// Delete the snapshot; an already-missing file is fine.
pub(crate) fn remove(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_shows_keys_never_values() {
        let mut prior = Map::new();
        prior.insert(
            "ANTHROPIC_AUTH_TOKEN".into(),
            Value::String("sk-real".into()),
        );
        let mut snap = Snapshot::new(prior);
        snap.statusline_prior = Value::String("sk-status".into());
        let shown = format!(
            "{:?} {:?}",
            snap,
            SnapshotState::Present(Box::new(snap.clone()))
        );
        assert!(shown.contains("ANTHROPIC_AUTH_TOKEN"));
        assert!(!shown.contains("sk-real") && !shown.contains("sk-status"));
    }
}
