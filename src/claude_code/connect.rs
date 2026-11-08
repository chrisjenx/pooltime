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
//! With the catalog unavailable (no usable Anthropic account, upstream down)
//! the pins and context already in a profile are kept as they are (and the Fable
//! rows are left alone); only the base URL and token follow the pool. A pin is
//! never dropped for a flaky fetch.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{anyhow, Context};
use rand::RngExt;
use serde_json::Value;
use tokio::task::JoinHandle;

use super::fable_cache::refresh_rows;
use super::pins::{derive_pins, max_context, newest_fable};
use super::scrub::{context_is_managed, CONTEXT_KEY};
use super::statusline::{install as install_statusline, restore as restore_statusline};
use super::{apply, clear, is_connected, patch_json, EnvBlock, Pin, Pins, Profile};
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
/// (`id == "default"` and a settings dir named `.claude`, i.e. `~/.claude`)
/// keeps `.claude.json` in the directory above its settings dir; every other
/// profile keeps it inside.
pub fn profile_of(r: &ProfileRef) -> Profile {
    let home = r.settings_dir.parent().filter(|_| {
        r.id == "default" && r.settings_dir.file_name().is_some_and(|n| n == ".claude")
    });
    let claude_json = match home {
        Some(home) => home.join(".claude.json"),
        None => r.settings_dir.join(".claude.json"),
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
    state
        .proxy_key()
        .or_else(|| non_empty(&stored.proxy_key).map(str::to_string))
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

/// What a `.claude.json` patch pass writes in: the newest Fable row and the MCP target.
#[derive(Clone, PartialEq, Eq)]
struct PatchInputs {
    fable: Option<(String, String)>,
    mcp: Option<(String, String)>,
}

/// A file as last left right by a patch pass: its `(mtime, size)` and the inputs used.
#[derive(Clone, PartialEq, Eq)]
struct PatchMemo {
    stamp: (std::time::SystemTime, u64),
    inputs: PatchInputs,
}

static PATCH_MEMO: Mutex<Option<std::collections::HashMap<PathBuf, PatchMemo>>> = Mutex::new(None);

fn file_stamp(path: &Path) -> Option<(std::time::SystemTime, u64)> {
    let m = std::fs::metadata(path).ok()?;
    Some((m.modified().ok()?, m.len()))
}

/// The file and the inputs are what the last successful pass saw: nothing to do.
fn patch_memo_hit(path: &Path, inputs: &PatchInputs) -> bool {
    let Some(stamp) = file_stamp(path) else {
        return false;
    };
    let memo = PATCH_MEMO.lock().unwrap_or_else(|e| e.into_inner());
    memo.as_ref()
        .and_then(|m| m.get(path))
        .is_some_and(|m| m.stamp == stamp && &m.inputs == inputs)
}

fn patch_memo_store(path: &Path, inputs: PatchInputs) {
    let mut memo = PATCH_MEMO.lock().unwrap_or_else(|e| e.into_inner());
    let map = memo.get_or_insert_with(Default::default);
    match file_stamp(path) {
        Some(stamp) => {
            map.insert(path.to_path_buf(), PatchMemo { stamp, inputs });
        }
        None => {
            map.remove(path);
        }
    }
}

/// Each profile id is warned about a missing proxy key once per process.
fn first_no_key_warning(id: &str) -> bool {
    static SEEN: Mutex<Option<HashSet<String>>> = Mutex::new(None);
    SEEN.lock()
        .unwrap_or_else(|e| e.into_inner())
        .get_or_insert_with(HashSet::new)
        .insert(id.to_string())
}

/// The pins and max-context already in `p`'s settings `env`, for re-applying
/// unchanged when there is no catalog. `apply` rebuilds the managed keys from
/// the user's snapshot and then writes the block, so a block without them would
/// drop the pins. A family counts only when its model, name and description are
/// all present; the context value only when a marker proves it is ours. An
/// unreadable settings file preserves nothing (and `apply` will report it).
fn current_pins(p: &Profile) -> (Pins, Option<u32>) {
    let env = std::fs::read(&p.settings_json)
        .ok()
        .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
        .and_then(|v| match v.get("env") {
            Some(Value::Object(m)) => Some(m.clone()),
            _ => None,
        })
        .unwrap_or_default();
    let pin = |family: &str| {
        let base = format!("ANTHROPIC_DEFAULT_{family}_MODEL");
        let get = |k: String| env.get(&k)?.as_str().map(str::to_string);
        Some(Pin {
            id: get(base.clone())?,
            name: get(format!("{base}_NAME"))?,
            description: get(format!("{base}_DESCRIPTION"))?,
        })
    };
    let max_context = context_is_managed(&env)
        .then(|| env.get(CONTEXT_KEY)?.as_str()?.parse::<u32>().ok())
        .flatten();
    (
        Pins {
            sonnet: pin("SONNET"),
            haiku: pin("HAIKU"),
        },
        max_context,
    )
}

/// Apply the env block and refresh the Fable rows of one profile. Blocking file
/// work.
///
/// With a catalog, pins and max-context are derived from it. Without one
/// (`None`: unreachable) the pins and context currently in the profile are
/// re-applied as they are (see [`current_pins`]) and the Fable rows are not
/// touched, so only the base URL and token can change. (Chosen over a "preserve"
/// flag on `apply` to keep that Task 27 API as it was.)
///
/// `mcp` (`(url, token)`) refreshes the pool's MCP entry, but only in a profile
/// whose snapshot says it is registered ([`super::mcp::is_registered`]). The
/// Fable rows and the MCP entry share one guarded pass over `.claude.json`, which
/// is not opened at all when neither applies.
///
/// The `.claude.json` refresh is cosmetic and never fails the call: an error is
/// logged by profile id only (never content) and the env outcome is returned; a
/// corrupt `.claude.json` is left untouched.
fn sync_profile(
    paths: &Paths,
    p: &Profile,
    base_url: &str,
    key: &str,
    catalog: Option<&[Value]>,
    mcp: Option<(&str, &str)>,
) -> anyhow::Result<WriteOutcome> {
    let (pins, max_context) = match catalog {
        Some(c) => (derive_pins(c), max_context(c)),
        None => current_pins(p),
    };
    let block = EnvBlock {
        base_url: base_url.to_string(),
        auth_token: key.to_string(),
        pins,
        max_context,
    };
    let env = apply(p, &block, paths)?;
    let fable = catalog.and_then(newest_fable);
    let mcp = mcp.filter(|_| super::mcp::is_registered(paths, p));
    if fable.is_none() && mcp.is_none() {
        return Ok(env);
    }
    // `.claude.json` can be megabytes: parse it only when it or what we would patch in changed
    // since the last pass that left it right.
    let inputs = PatchInputs {
        fable: fable.as_ref().map(|f| (f.id.clone(), f.name.clone())),
        mcp: mcp.map(|(u, t)| (u.to_string(), t.to_string())),
    };
    if patch_memo_hit(&p.claude_json, &inputs) {
        return Ok(env);
    }
    let cached = patch_json(&p.claude_json, |obj| {
        let rows = fable.as_ref().is_some_and(|f| refresh_rows(obj, f));
        let entry = mcp.is_some_and(|(url, token)| super::mcp::refresh_entry(obj, url, token));
        rows || entry
    });
    let cached = match cached {
        Ok(o) => {
            if o != WriteOutcome::GaveUp {
                patch_memo_store(&p.claude_json, inputs);
            }
            o
        }
        Err(_) => {
            tracing::warn!(profile = %p.id, "could not refresh .claude.json (Fable rows / MCP entry); left as is");
            WriteOutcome::Unchanged
        }
    };
    Ok(merge(env, cached))
}

/// Point `profiles` at this pool and record them in the config.
///
/// 1. The pool's own address must answer `/health` as a pooltime
///    (`Probe::Pooltime`); anything else is an error and nothing is written.
/// 2. The proxy key is the running pool's, else the stored one, else a freshly
///    generated 256-bit random key that is saved (0600) to the config. The
///    running pool enforces it from then on (no restart).
/// 3. Each profile gets the `env` block (pins from the catalog; with the catalog
///    unavailable the pins and context already in the profile are kept, so a
///    first connect writes only the base URL and token, and a re-connect never
///    drops pins) and refreshed Fable rows. The Fable refresh is cosmetic: its
///    failure (e.g. a corrupt `.claude.json`, left untouched) is only logged.
/// 4. Profiles that were applied are recorded in `config.profiles` (deduped by
///    id). A profile whose `env` cannot be applied (e.g. a corrupt
///    `settings.json`) does not stop the others: every other profile is applied
///    and recorded, then an error naming the failed profile(s) is returned.
///
/// Returns each profile's id with its combined write outcome.
pub async fn connect(
    state: &AppState,
    profiles: &[Profile],
) -> anyhow::Result<Vec<(String, WriteOutcome)>> {
    connect_with(state, profiles, None, false).await
}

/// `(http://<addr>/mcp, MCP token)`: what a `.claude.json` registration names. `None` when the
/// pool has no MCP token.
fn mcp_target(state: &AppState) -> Option<(String, String)> {
    (!state.mcp_key.is_empty()).then(|| {
        (
            format!("{}/mcp", base_url(&state.cfg)),
            state.mcp_key.clone(),
        )
    })
}

/// [`connect`], and with `statusline: Some(bin)` also install the status line wrapper
/// (`<bin> statusline --wrap <id>`, [`super::statusline::install`]) into each profile whose `env`
/// was applied. With `mcp` also register the pool's MCP server (with the MCP token, never the
/// management key) in each such profile's `.claude.json` ([`super::mcp::register`]). A profile
/// whose wrapper or registration cannot be written stays connected and recorded, and is reported
/// as failed. `None` / `false` never touch `statusLine` / `mcpServers`.
pub async fn connect_with(
    state: &AppState,
    profiles: &[Profile],
    statusline: Option<&Path>,
    mcp: bool,
) -> anyhow::Result<Vec<(String, WriteOutcome)>> {
    if mcp && state.mcp_key.is_empty() {
        return Err(anyhow!("this pool has no MCP token; restart it and retry"));
    }
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

    let key = {
        // One connect at a time decides the key: the first generates it, the rest find it.
        let _write = state.config_write.lock().await;
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
        // The running pool enforces the key the profiles are about to send, from now on.
        state.set_proxy_key(&key);
        key
    };
    let catalog = catalog(state).await;
    let url = base_url(&state.cfg);

    let paths = state.paths.clone();
    let owned: Vec<Profile> = profiles.to_vec();
    let statusline: Option<PathBuf> = statusline.map(Path::to_path_buf);
    let register = mcp;
    let mcp = mcp_target(state);
    let (outcomes, failures) = tokio::task::spawn_blocking(move || {
        let mut outcomes = Vec::new();
        let mut failures: Vec<(String, anyhow::Error)> = Vec::new();
        let mcp = mcp.as_ref().map(|(u, t)| (u.as_str(), t.as_str()));
        for p in &owned {
            let env = match sync_profile(&paths, p, &url, &key, catalog.as_deref(), mcp) {
                Ok(o) => o,
                Err(e) => {
                    failures.push((p.id.clone(), e));
                    continue;
                }
            };
            let line = match statusline
                .as_deref()
                .map(|b| install_statusline(p, &paths, b))
            {
                None => Ok(WriteOutcome::Unchanged),
                Some(r) => r,
            };
            let env = match line {
                Ok(o) => merge(env, o),
                Err(e) => {
                    failures.push((p.id.clone(), e.context("installing the status line")));
                    env
                }
            };
            let reg = match mcp.filter(|_| register) {
                None => Ok(WriteOutcome::Unchanged),
                Some((url, token)) => super::mcp::register(&paths, p, url, token),
            };
            match reg {
                Ok(o) => outcomes.push((p.clone(), merge(env, o))),
                Err(e) => {
                    outcomes.push((p.clone(), env));
                    failures.push((p.id.clone(), e.context("registering the MCP server")));
                }
            }
        }
        (outcomes, failures)
    })
    .await
    .context("connect task")?;

    // Re-read: something else may have saved the config while we worked.
    let _write = state.config_write.lock().await;
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
    let mut failures = failures.into_iter();
    match failures.next() {
        None => Ok(outcomes.into_iter().map(|(p, o)| (p.id, o)).collect()),
        Some((id, first)) => {
            let rest: Vec<String> = failures.map(|(id, _)| id).collect();
            let also = if rest.is_empty() {
                String::new()
            } else {
                format!(" (also failed: {})", rest.join(", "))
            };
            Err(first.context(format!("connecting profile {id}{also}")))
        }
    }
}

/// Release `profiles` (the status line wrapper replaced by the user's original,
/// then [`clear`]: prior `env` values restored, snapshot deleted) and forget
/// them in the config. Works without a running pool, so the CLI and
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
        // The status line first: `clear` deletes the snapshot that holds its original. Unless
        // that restore landed, the snapshot (and the connection) is kept for a retry.
        let line = match restore_statusline(p, paths) {
            Ok(WriteOutcome::GaveUp) => {
                out.push((p.id.clone(), WriteOutcome::GaveUp));
                continue;
            }
            Ok(o) => o,
            Err(e) => {
                failure.get_or_insert(e.context(format!("disconnecting profile {}", p.id)));
                continue;
            }
        };
        match clear(p, paths).map(|o| merge(o, line)) {
            Ok(o) => {
                if o != WriteOutcome::GaveUp {
                    released.push(&p.id);
                }
                out.push((p.id.clone(), merge(o, unregister_mcp(paths, p))));
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

/// Drop the pool's MCP registration from `p` once its `env` is restored. Best effort: a
/// `.claude.json` that cannot be parsed (Claude Code cannot read it either) or kept changing is
/// logged by profile id and does not undo the disconnect. `Unchanged` unless it wrote.
fn unregister_mcp(paths: &Paths, p: &Profile) -> WriteOutcome {
    match super::mcp::unregister(paths, p) {
        Ok(WriteOutcome::GaveUp) => {
            tracing::warn!(profile = %p.id, "MCP entry not removed: .claude.json kept changing");
            WriteOutcome::Unchanged
        }
        Ok(o) => o,
        Err(_) => {
            tracing::warn!(profile = %p.id, "MCP entry not removed: .claude.json unreadable");
            WriteOutcome::Unchanged
        }
    }
}

/// One sync pass: re-apply the env block (current port and key) and Fable rows
/// to every configured profile that is still connected.
///
/// With the catalog unavailable the pins and context already in each profile
/// are kept as they are (see [`sync_profile`]) and the Fable rows are left
/// alone, but the base URL and token are still re-applied, so a moved port
/// re-syncs even while the catalog is blind. With no key to send, nothing is
/// touched (warned once per profile). A profile that cannot be written (corrupt
/// settings, ...) is logged by id and skipped; `GaveUp` is logged as a warning.
/// Returns the processed profiles in config order.
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
    let Some(key) = existing_key(state, &cfg) else {
        for p in &connected {
            if first_no_key_warning(&p.id) {
                tracing::warn!(
                    profile = %p.id,
                    "profile sync skipped: no proxy key is known for this connected profile"
                );
            }
        }
        return Ok(Vec::new());
    };
    let catalog = catalog(state).await;
    let url = base_url(&state.cfg);
    let paths = state.paths.clone();
    // A registered profile's MCP entry follows a moved port too (never added here).
    let mcp = mcp_target(state);
    tokio::task::spawn_blocking(move || {
        let mut out = Vec::new();
        let mcp = mcp.as_ref().map(|(u, t)| (u.as_str(), t.as_str()));
        for p in connected {
            match sync_profile(&paths, &p, &url, &key, catalog.as_deref(), mcp) {
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
            if let Some(Err(e)) =
                crate::logging::catch_panic("profile sync", sync_once(&state)).await
            {
                tracing::warn!(error = %e, "profile sync failed");
            }
        }
    })
}
