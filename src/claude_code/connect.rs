//! Connect / disconnect Claude Code profiles and keep them in sync with the
//! pool (spec §16 "Profiles & Claude Code files"; INV §6).
//!
//! - [`connect`] needs a healthy pool (its own `/health` must pass the identity
//!   check), makes sure a random proxy key exists, writes the `env` override with
//!   catalog-derived pins and refreshes the cached Fable rows, and records the
//!   profile in the pooltime config.
//! - [`disconnect`] undoes that from `Paths` alone (it works with the pool down:
//!   the CLI and service-uninstall call it).
//! - [`sync_once`] / [`spawn_profile_sync`] re-derive the pins for every
//!   connected profile, so a Claude model released while the pool stays up, or a
//!   moved port after a restart, reaches the pickers without a manual reconnect.
//!
//! With the catalog unavailable (no usable Anthropic account, upstream down) a
//! sync changes nothing, so a pin is never dropped for a flaky fetch.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Context};
use rand::RngExt;
use serde_json::Value;
use tokio::task::JoinHandle;

use super::pins::{derive_pins, max_context};
use super::{apply, clear, is_connected, rewrite_fable_rows, EnvBlock, Profile};
use crate::config::{Config, Paths, ProfileRef};
use crate::fsutil::WriteOutcome;
use crate::providers::ProviderId;
use crate::server::health::{probe, probe_addr, Probe};
use crate::server::models_route::build_listing;
use crate::server::AppState;

/// How often the background sync re-derives pins for connected profiles.
pub const SYNC_INTERVAL: Duration = Duration::from_secs(60);

/// The rows `/v1/models` serves right now (live Anthropic list merged with the
/// model table), or `None` when the live Anthropic list is unavailable or empty
/// (the "unreachable catalog": callers must not change any pin).
pub async fn catalog(state: &AppState) -> Option<Vec<Value>> {
    let provider = state.providers.get(&ProviderId::Anthropic)?;
    let live = provider
        .live_models()
        .await
        .ok()
        .filter(|v| !v.is_empty())?;
    let table = state.models.read().unwrap_or_else(|e| e.into_inner());
    Some(build_listing(Some(&live), &table.rows_for_listing()))
}

/// The [`Profile`] a stored [`ProfileRef`] stands for. Only the default profile
/// (`id == "default"`, i.e. `~/.claude`) keeps `.claude.json` in the directory
/// above its settings dir; every other profile keeps it inside.
pub fn profile_of(r: &ProfileRef) -> Profile {
    let claude_json = match (r.id.as_str(), r.settings_dir.parent()) {
        ("default", Some(home)) => home.join(".claude.json"),
        _ => r.settings_dir.join(".claude.json"),
    };
    Profile {
        id: r.id.clone(),
        settings_json: r.settings_dir.join("settings.json"),
        settings_dir: r.settings_dir.clone(),
        claude_json,
    }
}

fn profile_ref(p: &Profile) -> ProfileRef {
    ProfileRef {
        id: p.id.clone(),
        settings_dir: p.settings_dir.clone(),
    }
}

/// `http://<addr>` the profiles should use: the configured bind address, a
/// wildcard bind answering on loopback.
fn base_url(cfg: &Config) -> String {
    format!("http://{}", probe_addr(cfg.host, cfg.port))
}

fn non_empty(k: &Option<String>) -> Option<&str> {
    k.as_deref().filter(|k| !k.is_empty())
}

/// The key the profiles must send: what the running pool enforces, else the
/// stored one. `None` when neither exists.
fn existing_key(state: &AppState, stored: &Config) -> Option<String> {
    non_empty(&state.cfg.proxy_key)
        .or_else(|| non_empty(&stored.proxy_key))
        .map(str::to_string)
}

fn generate_key() -> String {
    hex::encode(rand::rng().random::<[u8; 32]>())
}

/// Never-write-never-drop combination of the two files' outcomes.
fn merge(a: WriteOutcome, b: WriteOutcome) -> WriteOutcome {
    use WriteOutcome::*;
    match (a, b) {
        (GaveUp, _) | (_, GaveUp) => GaveUp,
        (Written, _) | (_, Written) => Written,
        _ => Unchanged,
    }
}

/// Apply the env block (pins and context from `catalog`, if any) and refresh the
/// Fable rows of one profile. Blocking file work.
fn sync_profile(
    paths: &Paths,
    p: &Profile,
    base_url: &str,
    key: &str,
    catalog: Option<&[Value]>,
) -> anyhow::Result<WriteOutcome> {
    let block = EnvBlock {
        base_url: base_url.to_string(),
        auth_token: key.to_string(),
        pins: catalog.map(derive_pins).unwrap_or_default(),
        max_context: catalog.and_then(max_context),
    };
    let env = apply(p, &block, paths)?;
    let fable = match catalog {
        Some(c) => rewrite_fable_rows(p, c)?,
        None => WriteOutcome::Unchanged,
    };
    Ok(merge(env, fable))
}

/// Point `profiles` at this pool and record them in the config.
///
/// 1. The pool's own address must answer `/health` as a pooltime
///    (`Probe::Pooltime`); anything else is an error and nothing is written.
/// 2. The proxy key is the running pool's, else the stored one, else a freshly
///    generated 256-bit random key that is saved (0600) to the config. A newly
///    generated key only takes effect in a pool that is restarted afterwards; a
///    pool running without a key accepts any token until then.
/// 3. Each profile gets the `env` block (pins from the catalog; with the catalog
///    unavailable only the base URL and token) and refreshed Fable rows.
///    Re-connecting a connected profile while the catalog is unavailable drops
///    its pins until the next sync restores them.
/// 4. Profiles that were applied are recorded in `config.profiles` (deduped by
///    id). If one profile fails, the ones already applied are still recorded and
///    the error (naming the profile) is returned.
///
/// Returns each profile's id with its combined write outcome.
pub async fn connect(
    state: &AppState,
    profiles: &[Profile],
) -> anyhow::Result<Vec<(String, WriteOutcome)>> {
    let addr = probe_addr(state.cfg.host, state.cfg.port);
    match probe(addr).await {
        Probe::Pooltime => {}
        Probe::Free => return Err(anyhow!("pooltime is not running on {addr}; start it first")),
        other => {
            return Err(anyhow!(
                "{addr} does not answer as a healthy pooltime ({other:?}); not connecting"
            ))
        }
    }

    let mut stored = Config::load_file(&state.paths)?;
    let key = match existing_key(state, &stored) {
        Some(k) => k,
        None => {
            let k = generate_key();
            stored.proxy_key = Some(k.clone());
            stored.save(&state.paths)?;
            k
        }
    };
    let catalog = catalog(state).await;
    let url = base_url(&state.cfg);

    let paths = state.paths.clone();
    let owned: Vec<Profile> = profiles.to_vec();
    let (outcomes, failure) = tokio::task::spawn_blocking(move || {
        let mut outcomes = Vec::new();
        let mut failure = None;
        for p in &owned {
            match sync_profile(&paths, p, &url, &key, catalog.as_deref()) {
                Ok(o) => outcomes.push((p.clone(), o)),
                Err(e) => {
                    failure = Some(e.context(format!("connecting profile {}", p.id)));
                    break;
                }
            }
        }
        (outcomes, failure)
    })
    .await
    .context("connect task")?;

    // Re-read: something else may have saved the config while we worked.
    let mut cfg = Config::load_file(&state.paths)?;
    let before = cfg.profiles.clone();
    for (p, _) in &outcomes {
        let r = profile_ref(p);
        match cfg.profiles.iter_mut().find(|e| e.id == r.id) {
            Some(existing) => *existing = r,
            None => cfg.profiles.push(r),
        }
    }
    if cfg.profiles != before {
        cfg.save(&state.paths)?;
    }
    match failure {
        Some(e) => Err(e),
        None => Ok(outcomes.into_iter().map(|(p, o)| (p.id, o)).collect()),
    }
}

/// Release `profiles` ([`clear`]: prior `env` values restored, snapshot deleted)
/// and forget them in the config. Works without a running pool, so the CLI and
/// `service uninstall` can call it; the proxy key stays in the config.
///
/// A profile whose settings file kept changing under us (`GaveUp`) stays
/// connected and recorded so a retry still finds it. If a profile fails, the
/// others are still processed and the first error is returned after the config
/// is saved. Returns each profile's id with its outcome (`Unchanged` for a
/// profile that was not connected).
pub fn disconnect(
    paths: &Paths,
    profiles: &[Profile],
) -> anyhow::Result<Vec<(String, WriteOutcome)>> {
    let mut out = Vec::new();
    let mut released: Vec<&str> = Vec::new();
    let mut failure = None;
    for p in profiles {
        match clear(p, paths) {
            Ok(o) => {
                if o != WriteOutcome::GaveUp {
                    released.push(&p.id);
                }
                out.push((p.id.clone(), o));
            }
            Err(e) => {
                failure.get_or_insert(e.context(format!("disconnecting profile {}", p.id)));
            }
        }
    }
    let mut cfg = Config::load_file(paths)?;
    let before = cfg.profiles.len();
    cfg.profiles.retain(|r| !released.contains(&r.id.as_str()));
    if cfg.profiles.len() != before {
        cfg.save(paths)?;
    }
    match failure {
        Some(e) => Err(e),
        None => Ok(out),
    }
}

/// One sync pass: re-apply the env block (current port and key) and Fable rows
/// to every configured profile that is still connected. With the catalog
/// unavailable, or no key to send, nothing is touched and the result is empty.
/// A profile that cannot be written (corrupt settings, ...) is logged by id and
/// skipped; `GaveUp` is logged as a warning. Returns the processed profiles in
/// config order.
pub async fn sync_once(state: &AppState) -> anyhow::Result<Vec<(String, WriteOutcome)>> {
    let cfg = Config::load_file(&state.paths)?;
    let connected: Vec<Profile> = cfg
        .profiles
        .iter()
        .map(profile_of)
        .filter(|p| is_connected(p, &state.paths))
        .collect();
    if connected.is_empty() {
        return Ok(Vec::new());
    }
    let Some(catalog) = catalog(state).await else {
        return Ok(Vec::new());
    };
    let Some(key) = existing_key(state, &cfg) else {
        tracing::warn!("profile sync skipped: no proxy key is known");
        return Ok(Vec::new());
    };
    let url = base_url(&state.cfg);
    let paths = state.paths.clone();
    tokio::task::spawn_blocking(move || {
        let mut out = Vec::new();
        for p in connected {
            match sync_profile(&paths, &p, &url, &key, Some(&catalog)) {
                Ok(WriteOutcome::GaveUp) => {
                    tracing::warn!(profile = %p.id, "profile sync gave up: settings kept changing");
                    out.push((p.id, WriteOutcome::GaveUp));
                }
                Ok(o) => out.push((p.id, o)),
                Err(e) => tracing::warn!(profile = %p.id, error = %e, "profile sync failed"),
            }
        }
        out
    })
    .await
    .context("sync task")
}

/// Run [`sync_once`] now and then every [`SYNC_INTERVAL`] (the first pass fixes
/// profiles left pointing at an old port after a restart). Skipped while the
/// pool drains. Abort the handle to stop it.
pub fn spawn_profile_sync(state: Arc<AppState>) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(SYNC_INTERVAL);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tick.tick().await;
            if state.runtime.draining() {
                continue;
            }
            if let Err(e) = sync_once(&state).await {
                tracing::warn!(error = %e, "profile sync failed");
            }
        }
    })
}
