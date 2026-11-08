//! Point a Claude Code profile's `settings.json` `env` block at the pool, and
//! undo it exactly (INV §6; port of bro `settings.js` `applyPoolEnv` /
//! `clearPoolEnv`). Only `env` is touched; every other setting is preserved.
//!
//! Reversibility rests on a per-profile snapshot ([`super::snapshot`]) of the
//! user's prior values for every managed key, taken once and backfilled for keys
//! a later version starts managing. Order of effects in [`apply`]: the snapshot
//! is persisted *before* the `settings.json` rename lands, so a crash between
//! the two never loses the user's values (the snapshot is idempotent, so a
//! half-finished apply is simply retried).

use std::path::Path;

use anyhow::{anyhow, Context};
use serde_json::{Map, Value};

use super::scrub::{
    context_is_managed, is_pin_value, own_pin_keys, AUTO_COMPACT_KEY, BRO_MARKER_KEY, CONTEXT_KEY,
    MARKER_KEY,
};
use super::snapshot::{self, Snapshot, SnapshotState};
use super::{patch_json, Profile};
use crate::config::Paths;
use crate::fsutil::WriteOutcome;

const BASE_URL_KEY: &str = "ANTHROPIC_BASE_URL";
const AUTH_TOKEN_KEY: &str = "ANTHROPIC_AUTH_TOKEN";

/// Every `env` key pooltime manages (INV §6): the connection pair, the context
/// fallback and its marker, the pin keys, and the retired auto-compact window.
/// bro's own marker (`BRO_POOL_MANAGED_MAX_CONTEXT_TOKENS`) is additionally
/// treated as managed for snapshot/cleanup, but is never written.
pub const MANAGED_KEYS: &[&str] = &[
    BASE_URL_KEY,
    AUTH_TOKEN_KEY,
    CONTEXT_KEY,
    MARKER_KEY,
    "ANTHROPIC_DEFAULT_SONNET_MODEL",
    "ANTHROPIC_DEFAULT_SONNET_MODEL_NAME",
    "ANTHROPIC_DEFAULT_SONNET_MODEL_DESCRIPTION",
    "ANTHROPIC_DEFAULT_OPUS_MODEL",
    "ANTHROPIC_DEFAULT_OPUS_MODEL_NAME",
    "ANTHROPIC_DEFAULT_OPUS_MODEL_DESCRIPTION",
    "ANTHROPIC_DEFAULT_FABLE_MODEL",
    "ANTHROPIC_DEFAULT_FABLE_MODEL_NAME",
    "ANTHROPIC_DEFAULT_FABLE_MODEL_DESCRIPTION",
    "ANTHROPIC_DEFAULT_HAIKU_MODEL",
    "ANTHROPIC_DEFAULT_HAIKU_MODEL_NAME",
    "ANTHROPIC_DEFAULT_HAIKU_MODEL_DESCRIPTION",
    AUTO_COMPACT_KEY,
];

/// Managed keys plus bro's marker: what snapshot, restore and cleanup cover.
fn cleanup_keys() -> impl Iterator<Item = &'static str> {
    MANAGED_KEYS.iter().copied().chain([BRO_MARKER_KEY])
}

/// One model pin: `ANTHROPIC_DEFAULT_<FAMILY>_MODEL{,_NAME,_DESCRIPTION}`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Pin {
    pub id: String,
    pub name: String,
    pub description: String,
}

/// Catalog-derived pins. Only Sonnet and Haiku are ever written: a Fable pin
/// would add a second picker row, and a user-set Fable pin is left alone.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Pins {
    pub sonnet: Option<Pin>,
    pub haiku: Option<Pin>,
}

/// The full `env` mutation, in one place so apply and any dry-run report agree.
#[derive(Clone, PartialEq, Eq)]
pub struct EnvBlock {
    pub base_url: String,
    /// The proxy key or a placeholder. A credential: never logged (see `Debug`).
    pub auth_token: String,
    pub pins: Pins,
    /// `Some(n)` writes `CLAUDE_CODE_MAX_CONTEXT_TOKENS=n` plus the marker that
    /// proves it is ours; `None` (or 0) writes neither.
    pub max_context: Option<u32>,
}

impl std::fmt::Debug for EnvBlock {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EnvBlock")
            .field("base_url", &self.base_url)
            .field("auth_token", &"<redacted>")
            .field("pins", &self.pins)
            .field("max_context", &self.max_context)
            .finish()
    }
}

fn set_pin(env: &mut Map<String, Value>, family: &str, pin: &Pin) {
    let base = format!("ANTHROPIC_DEFAULT_{family}_MODEL");
    env.insert(base.clone(), Value::String(pin.id.clone()));
    env.insert(format!("{base}_NAME"), Value::String(pin.name.clone()));
    env.insert(
        format!("{base}_DESCRIPTION"),
        Value::String(pin.description.clone()),
    );
}

/// The `env` object of a settings file: `None` when absent, an error (never a
/// guess) when it is not an object.
fn env_of(settings: &Map<String, Value>) -> anyhow::Result<Option<Map<String, Value>>> {
    match settings.get("env") {
        None => Ok(None),
        Some(Value::Object(m)) => Ok(Some(m.clone())),
        Some(_) => Err(anyhow!("`env` in settings.json is not an object")),
    }
}

/// The value to remember as the user's own for `k`: `null` when absent or when
/// it is recognisably ours (a pin we or bro wrote, a marker-proven context
/// value, a marker, or exactly the base URL / token we are about to write), so
/// our own values are never restored as "the user's". A URL on a different port
/// (the daemon moved) or bro's URL cannot be proven ours and is kept as found.
fn snapshot_value(k: &str, env: &Map<String, Value>, block: &EnvBlock) -> Value {
    let Some(v) = env.get(k) else {
        return Value::Null;
    };
    let ours = match k {
        MARKER_KEY | BRO_MARKER_KEY => true,
        BASE_URL_KEY => v.as_str() == Some(block.base_url.as_str()),
        AUTH_TOKEN_KEY => v.as_str() == Some(block.auth_token.as_str()),
        CONTEXT_KEY => context_is_managed(env),
        _ => is_pin_value(k, v, env),
    };
    if ours {
        Value::Null
    } else {
        v.clone()
    }
}

/// Point `p`'s settings at the pool. Snapshots prior values once (an existing
/// snapshot is only backfilled with keys it lacks), writes the block, and leaves
/// `settings.json` byte-for-byte alone when the resulting `env` is unchanged.
///
/// Snapshot handling: absent -> created (0600, in a 0700 `run/profiles`);
/// present -> backfilled; present but unreadable -> left untouched, and the
/// apply proceeds with *no* prior to restore (bro's behaviour: the user asked to
/// connect, and overwriting the corrupt file could destroy whatever it holds).
/// A corrupt `settings.json` or a non-object `env` is an error and nothing is
/// written. `Ok(GaveUp)` means Claude Code kept rewriting the file; the snapshot
/// may exist already, which is harmless.
pub fn apply(p: &Profile, block: &EnvBlock, snaps: &Paths) -> anyhow::Result<WriteOutcome> {
    let snap_path = snapshot::snapshot_path(snaps, p);
    let mut failure: Option<anyhow::Error> = None;
    let outcome = patch_json(&p.settings_json, |settings| {
        match apply_once(settings, block, &snap_path) {
            Ok(changed) => changed,
            Err(e) => {
                failure.get_or_insert(e);
                false
            }
        }
    })?;
    match failure {
        Some(e) => Err(e),
        None => Ok(outcome),
    }
}

/// One pass over a fresh parse of `settings.json` (the closure may re-run).
fn apply_once(
    settings: &mut Map<String, Value>,
    block: &EnvBlock,
    snap_path: &Path,
) -> anyhow::Result<bool> {
    let original = env_of(settings)?;
    let env = original.clone().unwrap_or_default();

    // Snapshot first (see the module docs). It is re-read on every pass so a
    // concurrent apply that wrote it first is respected (snapshot once, only
    // backfill missing keys), and derived from this pass's fresh env.
    let prior = match snapshot::read(snap_path) {
        SnapshotState::Unreadable => {
            tracing::warn!(
                path = %snap_path.display(),
                "snapshot is unreadable and was left untouched; this override cannot be \
                 released by clear until it is fixed or removed"
            );
            Map::new()
        }
        state @ (SnapshotState::Absent | SnapshotState::Present(_)) => {
            let on_disk = match state {
                SnapshotState::Present(s) => Some(*s),
                _ => None,
            };
            let mut snap = on_disk.clone().unwrap_or_else(|| Snapshot::new(Map::new()));
            snap.managed = true;
            for k in cleanup_keys() {
                if !snap.prior.contains_key(k) {
                    snap.prior
                        .insert(k.to_string(), snapshot_value(k, &env, block));
                }
            }
            if on_disk.as_ref() != Some(&snap) {
                snapshot::write(snap_path, &snap)
                    .with_context(|| format!("writing snapshot {}", snap_path.display()))?;
            }
            snap.prior
        }
    };

    let mut next = env;
    for k in cleanup_keys() {
        match prior.get(k) {
            Some(Value::Null) => {
                next.remove(k);
            }
            Some(v) => {
                next.insert(k.to_string(), v.clone());
            }
            None => {}
        }
    }
    next.insert(BASE_URL_KEY.into(), Value::String(block.base_url.clone()));
    next.insert(
        AUTH_TOKEN_KEY.into(),
        Value::String(block.auth_token.clone()),
    );
    if let Some(pin) = &block.pins.sonnet {
        set_pin(&mut next, "SONNET", pin);
    }
    if let Some(pin) = &block.pins.haiku {
        set_pin(&mut next, "HAIKU", pin);
    }
    if let Some(n) = block.max_context.filter(|n| *n > 0) {
        next.insert(CONTEXT_KEY.into(), Value::String(n.to_string()));
        next.insert(MARKER_KEY.into(), Value::String(n.to_string()));
    }

    // Every connect re-applies; don't rewrite the file under running sessions
    // when the block came out the same.
    if original.as_ref() == Some(&next) {
        return Ok(false);
    }
    settings.insert("env".into(), Value::Object(next));
    Ok(true)
}

/// Undo [`apply`]: restore prior values (`null` -> delete the key), remove
/// managed keys that have no prior entry when they are recognisably ours, drop
/// `env` if it ends up empty, then delete the snapshot.
///
/// Keys without a prior entry (a snapshot written before the key was managed):
/// the connection pair and the markers are removed; pin keys only when the value
/// is a recognisable bro/pooltime pin; the context value only when a marker
/// proves it is ours; the retired auto-compact window is left alone. Anything
/// else could be the user's own.
///
/// Nothing managed (no snapshot, `managed:false`, or an unreadable snapshot) is
/// `Ok(Unchanged)` and creates no files. A managed snapshot whose `settings.json`
/// needs no edit (absent, or `env` already clean) is also `Unchanged`, and the
/// snapshot is still deleted. A corrupt `settings.json` is an error and the
/// snapshot is kept so the clear can be retried; `GaveUp` keeps it too.
pub fn clear(p: &Profile, snaps: &Paths) -> anyhow::Result<WriteOutcome> {
    let snap_path = snapshot::snapshot_path(snaps, p);
    let snap = match snapshot::read(&snap_path) {
        SnapshotState::Present(s) if s.managed => s,
        _ => return Ok(WriteOutcome::Unchanged),
    };
    let mut failure: Option<anyhow::Error> = None;
    let outcome = patch_json(&p.settings_json, |settings| {
        match clear_once(settings, &snap.prior) {
            Ok(changed) => changed,
            Err(e) => {
                failure.get_or_insert(e);
                false
            }
        }
    })?;
    if let Some(e) = failure {
        return Err(e);
    }
    if outcome != WriteOutcome::GaveUp {
        if let Err(e) = snapshot::remove(&snap_path) {
            // The settings are already restored; a stale snapshot is the lesser harm.
            tracing::warn!(path = %snap_path.display(), error = %e, "could not delete snapshot after clear");
        }
    }
    Ok(outcome)
}

fn clear_once(
    settings: &mut Map<String, Value>,
    prior: &Map<String, Value>,
) -> anyhow::Result<bool> {
    let original = env_of(settings)?;
    let mut env = original.clone().unwrap_or_default();
    // Decide "recognisably ours" against the untouched block (Haiku keys depend
    // on a sibling, the context value on its marker).
    let ctx_ours = context_is_managed(&env);
    let own_pins = own_pin_keys(&env);
    for k in cleanup_keys() {
        match prior.get(k) {
            Some(Value::Null) => {
                env.remove(k);
            }
            Some(v) => {
                env.insert(k.to_string(), v.clone());
            }
            None => {
                let ours = match k {
                    BASE_URL_KEY | AUTH_TOKEN_KEY | MARKER_KEY | BRO_MARKER_KEY => true,
                    CONTEXT_KEY => ctx_ours,
                    AUTO_COMPACT_KEY => false,
                    _ => own_pins.contains(&k),
                };
                if ours {
                    env.remove(k);
                }
            }
        }
    }
    let next = (!env.is_empty()).then_some(env);
    if next == original {
        return Ok(false);
    }
    match next {
        Some(m) => settings.insert("env".into(), Value::Object(m)),
        None => settings.remove("env"),
    };
    Ok(true)
}

/// Whether `p` is currently pointed at the pool: a readable snapshot with
/// `managed:true` and an `ANTHROPIC_BASE_URL` present in the settings `env`.
/// (It does not compare the URL with what we would write: the daemon port can
/// change, and any override still needs a `clear`.) An unreadable settings file
/// or snapshot is "not connected".
pub fn is_connected(p: &Profile, snaps: &Paths) -> bool {
    let managed = matches!(
        snapshot::read(&snapshot::snapshot_path(snaps, p)),
        SnapshotState::Present(s) if s.managed
    );
    if !managed {
        return false;
    }
    let Ok(bytes) = std::fs::read(&p.settings_json) else {
        return false;
    };
    serde_json::from_slice::<Value>(&bytes)
        .ok()
        .and_then(|v| v.get("env")?.get(BASE_URL_KEY).cloned())
        .is_some_and(|v| !v.is_null())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::claude_code::scrub::PIN_KEYS;

    #[test]
    fn managed_keys_cover_every_pin_key_and_both_markers() {
        for k in PIN_KEYS {
            assert!(MANAGED_KEYS.contains(k), "{k}");
        }
        assert!(MANAGED_KEYS.contains(&MARKER_KEY));
        assert!(!MANAGED_KEYS.contains(&BRO_MARKER_KEY));
        assert!(cleanup_keys().any(|k| k == BRO_MARKER_KEY));
        let mut seen = std::collections::HashSet::new();
        assert!(cleanup_keys().all(|k| seen.insert(k)), "duplicate key");
    }

    #[test]
    fn env_block_debug_redacts_the_token() {
        let b = EnvBlock {
            base_url: "http://127.0.0.1:3460".into(),
            auth_token: "sk-secret".into(),
            pins: Pins::default(),
            max_context: None,
        };
        assert!(!format!("{b:?}").contains("sk-secret"));
    }
}
