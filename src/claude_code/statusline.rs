//! The status line segment (spec §4 "Alerts", §16 "Status line composition").
//!
//! - The daemon keeps `run/status-cache.json` current ([`spawn_status_cache`]): rewritten when the
//!   pool's picture changes (at most once a second) and at least every [`HEARTBEAT`]. It holds
//!   counts only, never tokens or emails:
//!   `{written_at, port, pool: {available, total}, seven_day_left, counts: {needs_login,
//!   expiring_today, expiring, erroring}}`.
//! - [`segment`] renders it without any network: `pool 6/8 · 7d 41% left · ⚠ 2 need login`, or
//!   `pool down` when the cache is missing, unreadable or older than [`STALE_AFTER`].
//! - `connect --statusline` ([`install`]) puts `pooltime statusline --wrap <profile-id>` into the
//!   profile's `statusLine`, keeping the user's own value in the profile snapshot
//!   (`statusline_prior`); [`restore`] (run by `disconnect`) puts it back, or removes the key when
//!   there was none.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::anyhow;
use serde::Deserialize;
use serde_json::{json, Map, Value};
use time::OffsetDateTime;
use tokio::sync::broadcast::error::{RecvError, TryRecvError};
use tokio::task::JoinHandle;

use super::snapshot::{self, SnapshotState};
use super::{patch_json, Profile};
use crate::config::Paths;
use crate::fsutil::{atomic_write_relaxed, WriteOutcome};
use crate::server::AppState;

/// File name under `run/`.
pub const CACHE_FILE: &str = "status-cache.json";
/// A cache older than this means the daemon is gone: `pool down`.
pub const STALE_AFTER: Duration = Duration::from_secs(150);
/// The cache is rewritten at least this often, even when nothing changed.
pub const HEARTBEAT: Duration = Duration::from_secs(60);
/// Minimum spacing between two cache writes.
pub const MIN_WRITE_GAP: Duration = Duration::from_secs(1);
/// How often the daemon re-reads the pool when no status transition woke it (usage changes, e.g.
/// the 7-day figure, emit none).
const POLL_EVERY: Duration = Duration::from_secs(5);

/// The segment shown when the pool cannot be read.
pub const POOL_DOWN: &str = "pool down";

pub fn cache_path(paths: &Paths) -> PathBuf {
    paths.run.join(CACHE_FILE)
}

/// The cache document (without `written_at`) for a `GET /api/v1/status` document. The 7-day
/// figure is `pooltime status`'s ([`crate::cli::status::pool_seven_day_left`]).
pub fn cache_body(status: &Value, port: u16) -> Value {
    let pool = &status["pool"];
    let counts = &status["counts"];
    let n = |v: &Value| v.as_u64().unwrap_or(0);
    let erroring = status["accounts"].as_array().map_or(0, |a| {
        a.iter().filter(|a| a["status"] == "erroring").count()
    });
    json!({
        "port": port,
        "pool": { "available": n(&pool["available"]), "total": n(&pool["accounts"]) },
        "seven_day_left": crate::cli::status::pool_seven_day_left(status),
        "counts": {
            "needs_login": n(&counts["needs_login"]),
            "expiring_today": n(&counts["expiring_today"]),
            "expiring": n(&counts["expiring"]),
            "erroring": erroring,
        },
    })
}

#[derive(Deserialize)]
struct Cache {
    written_at: i64,
    pool: PoolCounts,
    seven_day_left: Option<f64>,
    counts: Counts,
}

#[derive(Deserialize)]
struct PoolCounts {
    available: u64,
    total: u64,
}

#[derive(Deserialize)]
struct Counts {
    needs_login: u64,
    expiring_today: u64,
}

/// The segment for the cache bytes `cache` (`None`: no file) at unix time `now`.
pub fn segment(cache: Option<&[u8]>, now: i64) -> String {
    let Some(c) = cache.and_then(|b| serde_json::from_slice::<Cache>(b).ok()) else {
        return POOL_DOWN.into();
    };
    if now.saturating_sub(c.written_at) > STALE_AFTER.as_secs() as i64 {
        return POOL_DOWN.into();
    }
    let mut out = format!("pool {}/{}", c.pool.available, c.pool.total);
    if let Some(left) = c.seven_day_left {
        out.push_str(&format!(
            " · 7d {} left",
            crate::cli::status::pct(Some(left.clamp(0.0, 1.0)))
        ));
    }
    if c.counts.needs_login > 0 {
        out.push_str(&format!(" · ⚠ {} need login", c.counts.needs_login));
    }
    if c.counts.expiring_today > 0 {
        out.push_str(&format!(" · ⏳ {} expiring", c.counts.expiring_today));
    }
    out
}

/// [`segment`] of the cache file under `paths`, now.
pub fn read_segment(paths: &Paths) -> String {
    let bytes = std::fs::read(cache_path(paths)).ok();
    segment(bytes.as_deref(), OffsetDateTime::now_utc().unix_timestamp())
}

/// Keep `run/status-cache.json` current until aborted: re-read the pool on every status
/// transition and every [`POLL_EVERY`], write when the document changed (never twice within
/// [`MIN_WRITE_GAP`]) or [`HEARTBEAT`] has passed. Failures are logged and retried.
pub fn spawn_status_cache(app: Arc<AppState>) -> JoinHandle<()> {
    tokio::spawn(async move {
        let path = cache_path(&app.paths);
        let mut rx = Some(app.state.transitions());
        let mut poll = tokio::time::interval(POLL_EVERY);
        poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut last: Option<(Value, Instant)> = None;
        loop {
            match rx.as_mut() {
                Some(r) => tokio::select! {
                    _ = poll.tick() => {}
                    got = r.recv() => if let Err(RecvError::Closed) = got { rx = None },
                },
                None => {
                    poll.tick().await;
                }
            }
            if let Some((_, at)) = &last {
                let gap = at.elapsed();
                if gap < MIN_WRITE_GAP {
                    tokio::time::sleep(MIN_WRITE_GAP - gap).await;
                }
            }
            // Transitions that came in meanwhile are covered by this read.
            if let Some(r) = rx.as_mut() {
                while let Ok(_) | Err(TryRecvError::Lagged(_)) = r.try_recv() {}
            }
            let now = OffsetDateTime::now_utc();
            let Ok(inputs) = crate::server::mgmt::gather(&app, now).await else {
                tracing::warn!("status cache: could not read the pool");
                continue;
            };
            let body = cache_body(
                &crate::server::mgmt::build_status(&inputs, now),
                app.cfg.port,
            );
            let due = match &last {
                None => true,
                Some((prev, at)) => *prev != body || at.elapsed() >= HEARTBEAT,
            };
            if !due {
                continue;
            }
            let mut doc = body.clone();
            doc["written_at"] = json!(now.unix_timestamp());
            let bytes = serde_json::to_vec(&doc).expect("json value");
            let p = path.clone();
            match tokio::task::spawn_blocking(move || atomic_write_relaxed(&p, &bytes, 0o600)).await
            {
                Ok(Ok(())) => last = Some((body, Instant::now())),
                Ok(Err(e)) => tracing::warn!(error = %e, "status cache write failed"),
                Err(e) => tracing::warn!(error = %e, "status cache write task failed"),
            }
        }
    })
}

/// `s` as one `sh` word.
fn sh_word(s: &str) -> String {
    let plain = !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || "/._-+:@%=,".contains(c));
    if plain {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', r"'\''"))
    }
}

/// The `statusLine` command for `id`: `<bin> statusline --wrap <id>`.
pub fn wrapper_command(bin: &Path, id: &str) -> String {
    format!(
        "{} statusline --wrap {}",
        sh_word(&bin.to_string_lossy()),
        sh_word(id)
    )
}

/// Whether the `statusLine` value `v` is our wrapper for profile `id` (whatever binary path it
/// names), so it is never snapshotted as the user's own.
pub fn is_wrapper(v: &Value, id: &str) -> bool {
    v.get("command")
        .and_then(Value::as_str)
        .is_some_and(|c| c.ends_with(&format!(" statusline --wrap {}", sh_word(id))))
}

/// Install the wrapper (`bin statusline --wrap <id>`) as `p`'s `statusLine`. The user's own value
/// (`null` when absent) goes to the profile snapshot first; a `statusLine` that already is our
/// wrapper is never snapshotted (re-connecting keeps the original prior). Needs the snapshot
/// `connect` has just written; an absent or unreadable one is an error and nothing is changed.
pub fn install(p: &Profile, paths: &Paths, bin: &Path) -> anyhow::Result<WriteOutcome> {
    let snap_path = snapshot::snapshot_path(paths, p);
    let desired = json!({ "type": "command", "command": wrapper_command(bin, &p.id) });
    let mut failure: Option<anyhow::Error> = None;
    let outcome = patch_json(&p.settings_json, |settings| {
        match install_once(settings, &desired, &p.id, &snap_path) {
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

fn install_once(
    settings: &mut Map<String, Value>,
    desired: &Value,
    id: &str,
    snap_path: &Path,
) -> anyhow::Result<bool> {
    let current = settings.get("statusLine").cloned();
    match &current {
        Some(c) if c == desired => return Ok(false),
        // Ours with an older binary path: the snapshot already holds the original.
        Some(c) if is_wrapper(c, id) => {}
        _ => {
            let SnapshotState::Present(mut snap) = snapshot::read(snap_path) else {
                return Err(anyhow!(
                    "no readable snapshot at {}; status line not installed",
                    snap_path.display()
                ));
            };
            let prior = current.unwrap_or(Value::Null);
            if snap.statusline_prior != prior {
                snap.statusline_prior = prior;
                snapshot::write(snap_path, &snap)?;
            }
        }
    }
    settings.insert("statusLine".into(), desired.clone());
    Ok(true)
}

/// Undo [`install`]: when `p`'s `statusLine` is our wrapper, put the snapshot's
/// `statusline_prior` back (`null`: remove the key). Anything else (no snapshot, a status line
/// the user changed since) is left alone (`Unchanged`).
pub fn restore(p: &Profile, paths: &Paths) -> anyhow::Result<WriteOutcome> {
    let prior = match snapshot::read(&snapshot::snapshot_path(paths, p)) {
        SnapshotState::Present(s) => s.statusline_prior,
        _ => return Ok(WriteOutcome::Unchanged),
    };
    patch_json(&p.settings_json, |settings| {
        if !settings
            .get("statusLine")
            .is_some_and(|v| is_wrapper(v, &p.id))
        {
            return false;
        }
        match &prior {
            Value::Null => settings.remove("statusLine"),
            v => settings.insert("statusLine".into(), v.clone()),
        };
        true
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cache(written_at: i64, left: Value, needs_login: u64, today: u64) -> Vec<u8> {
        serde_json::to_vec(&json!({
            "written_at": written_at,
            "port": 3460,
            "pool": {"available": 6, "total": 8},
            "seven_day_left": left,
            "counts": {"needs_login": needs_login, "expiring_today": today, "expiring": 3, "erroring": 0},
        }))
        .unwrap()
    }

    #[test]
    fn pool_down_segment() {
        let now = 1_800_000_000;
        assert_eq!(segment(None, now), "pool down", "no cache");
        assert_eq!(segment(Some(b"{not json"), now), "pool down", "unparsable");
        assert_eq!(segment(Some(b"{}"), now), "pool down", "wrong shape");
        let stale = cache(now - 151, json!(0.41), 0, 0);
        assert_eq!(segment(Some(&stale), now), "pool down", "older than 150 s");
        let fresh = cache(now - 150, json!(0.41), 2, 0);
        assert_eq!(
            segment(Some(&fresh), now),
            "pool 6/8 · 7d 41% left · ⚠ 2 need login"
        );
    }

    #[test]
    fn segment_omits_unknown_and_zero_parts() {
        let now = 1_800_000_000;
        assert_eq!(
            segment(Some(&cache(now, Value::Null, 0, 0)), now),
            "pool 6/8"
        );
        assert_eq!(
            segment(Some(&cache(now, json!(1.0), 0, 1)), now),
            "pool 6/8 · 7d 100% left · ⏳ 1 expiring"
        );
    }

    #[test]
    fn cache_body_reuses_the_status_figures() {
        let status = json!({
            "accounts": [{"status": "erroring"}, {"status": "ready"}, {"status": "needs_login"}],
            "pool": {"seven_day_left_total": 1.5, "accounts": 3, "available": 2},
            "counts": {"needs_login": 1, "expiring_today": 0, "expiring": 1},
        });
        assert_eq!(
            cache_body(&status, 3460),
            json!({
                "port": 3460,
                "pool": {"available": 2, "total": 3},
                "seven_day_left": 0.5,
                "counts": {"needs_login": 1, "expiring_today": 0, "expiring": 1, "erroring": 1},
            })
        );
        let empty = json!({"pool": {"seven_day_left_total": 0.0, "accounts": 0, "available": 0}});
        assert!(cache_body(&empty, 1)["seven_day_left"].is_null());
    }

    #[test]
    fn wrapper_command_quotes_and_is_recognised() {
        let cmd = wrapper_command(Path::new("/opt/homebrew/bin/pooltime"), "claude-work");
        assert_eq!(
            cmd,
            "/opt/homebrew/bin/pooltime statusline --wrap claude-work"
        );
        let spaced = wrapper_command(Path::new("/My Apps/pooltime"), "it's");
        assert_eq!(spaced, r"'/My Apps/pooltime' statusline --wrap 'it'\''s'");
        assert!(is_wrapper(
            &json!({"type": "command", "command": spaced}),
            "it's"
        ));
        assert!(!is_wrapper(
            &json!({"type": "command", "command": cmd}),
            "other"
        ));
        assert!(!is_wrapper(
            &json!({"type": "command", "command": "ccline"}),
            "claude-work"
        ));
    }
}
