//! The status line segment (spec §4 "Alerts", §16 "Status line composition").
//!
//! - The daemon keeps `run/status-cache.json` current ([`spawn_status_cache`]): rewritten when the
//!   pool's picture changes (at most once a second) and at least every [`HEARTBEAT`]. It holds
//!   counts only, never tokens or emails:
//!   `{written_at, port, pool: {available, total}, seven_day_used, seven_day_left, counts:
//!   {needs_login, expiring_today, expiring, erroring}}` (`seven_day_left` is kept for older
//!   readers; the segment shows `seven_day_used`).
//! - [`segment`] renders it without any network: `pool 6/8 · 7d 59% used · ⚠ 2 need login`, or
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
use tokio::sync::watch;
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

/// The segment shown when the pool cannot be read.
pub const POOL_DOWN: &str = "pool down";

pub fn cache_path(paths: &Paths) -> PathBuf {
    paths.run.join(CACHE_FILE)
}

/// The cache document (without `written_at`) for a `GET /api/v1/status` document. The 7-day
/// figure is `pooltime status`'s ([`crate::cli::status::pool_seven_day_used`]).
pub fn cache_body(status: &Value, port: u16) -> Value {
    let pool = &status["pool"];
    let counts = &status["counts"];
    let n = |v: &Value| v.as_u64().unwrap_or(0);
    let erroring = status["accounts"].as_array().map_or(0, |a| {
        a.iter().filter(|a| a["status"] == "erroring").count()
    });
    let used = crate::cli::status::pool_seven_day_used(status);
    json!({
        "port": port,
        "pool": { "available": n(&pool["available"]), "total": n(&pool["accounts"]) },
        "seven_day_used": used,
        // Older readers (a status line from a previous version) still look for this.
        "seven_day_left": used.map(|u| 1.0 - u),
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
    seven_day_used: Option<f64>,
    /// Written by a daemon older than `seven_day_used`; read as `1 - left` when that is absent.
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

/// The cache bytes `cache` (`None`: no file) parsed, unless unreadable or stale at unix time `now`.
fn fresh_cache(cache: Option<&[u8]>, now: i64) -> Option<Cache> {
    let c = cache.and_then(|b| serde_json::from_slice::<Cache>(b).ok())?;
    (now.saturating_sub(c.written_at) <= STALE_AFTER.as_secs() as i64).then_some(c)
}

/// Whether the status line would show the pool (not [`POOL_DOWN`]) for the cache file under
/// `paths` at unix time `now`.
pub fn cache_is_fresh(paths: &Paths, now: i64) -> bool {
    let bytes = std::fs::read(cache_path(paths)).ok();
    fresh_cache(bytes.as_deref(), now).is_some()
}

/// The segment for the cache bytes `cache` (`None`: no file) at unix time `now`.
pub fn segment(cache: Option<&[u8]>, now: i64) -> String {
    let Some(c) = fresh_cache(cache, now) else {
        return POOL_DOWN.into();
    };
    let mut out = format!("pool {}/{}", c.pool.available, c.pool.total);
    if let Some(used) = c.seven_day_used.or(c.seven_day_left.map(|left| 1.0 - left)) {
        out.push_str(&format!(
            " · 7d {} used",
            crate::cli::status::pct(Some(used.clamp(0.0, 1.0)))
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

/// The running cache writer ([`spawn_status_cache`]). The writer itself deletes the cache when it
/// stops, after any write in progress, so a late write never brings the file back.
pub struct StatusCache {
    stop: watch::Sender<Stop>,
    task: JoinHandle<()>,
}

/// What the writer does when it stops.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stop {
    Running,
    /// Delete the cache: the pool is going away.
    Remove,
    /// Leave the cache: a newer daemon generation takes it over.
    Keep,
}

impl StatusCache {
    /// Stop the writer and wait until it has deleted the cache (status lines then say
    /// `pool down` at once). Dropping a `StatusCache` stops it too, without waiting.
    pub async fn shutdown(self) {
        self.stop_with(Stop::Remove).await;
    }

    /// Stop the writer and wait for it, leaving the cache in place: a newer daemon generation
    /// (a supervised restart) keeps it current from now on.
    pub async fn release(self) {
        self.stop_with(Stop::Keep).await;
    }

    async fn stop_with(self, how: Stop) {
        let StatusCache { stop, task } = self;
        let _ = stop.send(how);
        let _ = task.await;
    }
}

/// Keep `run/status-cache.json` current: re-read the pool when [`StateBook::changes`]
/// (crate::accounts::StateBook::changes) reports a change (at most once per [`MIN_WRITE_GAP`]) and
/// on every [`HEARTBEAT`]. A change writes only when the document differs; a heartbeat always
/// writes (it refreshes `written_at`). Nothing is read while nothing changes between heartbeats. A
/// failed read is warned about once per failure streak (with its cause), the recovery logged.
pub fn spawn_status_cache(app: Arc<AppState>) -> StatusCache {
    let (stop, mut stopped) = watch::channel(Stop::Running);
    let task = tokio::spawn(async move {
        let path = cache_path(&app.paths);
        let mut changes = Some(app.state.changes());
        let mut beat = tokio::time::interval(HEARTBEAT);
        beat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut last_read: Option<Instant> = None;
        let mut last_body: Option<Value> = None;
        let mut failing = false;
        loop {
            let heartbeat = tokio::select! {
                biased;
                _ = stopped.changed() => break,
                _ = beat.tick() => true,
                r = async { changes.as_mut().expect("guarded").changed().await }, if changes.is_some() => {
                    if r.is_err() {
                        changes = None; // the book is gone: heartbeats only
                    }
                    false
                }
            };
            if let Some(at) = last_read {
                let gap = at.elapsed();
                if gap < MIN_WRITE_GAP {
                    tokio::select! {
                        biased;
                        _ = stopped.changed() => break,
                        _ = tokio::time::sleep(MIN_WRITE_GAP - gap) => {}
                    }
                }
            }
            // Changes up to here are covered by this read.
            if let Some(c) = changes.as_mut() {
                c.borrow_and_update();
            }
            last_read = Some(Instant::now());
            let now = OffsetDateTime::now_utc();
            let read = async {
                let inputs = crate::server::mgmt::try_gather(&app, now)
                    .await
                    .map_err(|e| e.to_string())?;
                Ok::<_, String>(cache_body(
                    &crate::server::mgmt::build_status(&inputs, now),
                    app.cfg.port,
                ))
            };
            let read = crate::logging::catch_panic("status cache", read)
                .await
                .unwrap_or_else(|| Err("reading the pool panicked".into()));
            let body = match read {
                Ok(b) => b,
                Err(e) => {
                    if !failing {
                        tracing::warn!(error = %e, "status cache: could not read the pool");
                    }
                    failing = true;
                    continue;
                }
            };
            if failing {
                tracing::info!("status cache: reading the pool works again");
                failing = false;
            }
            if !heartbeat && last_body.as_ref() == Some(&body) {
                continue;
            }
            let mut doc = body.clone();
            doc["written_at"] = json!(now.unix_timestamp());
            let bytes = serde_json::to_vec(&doc).expect("json value");
            let p = path.clone();
            match crate::background::blocking(move || atomic_write_relaxed(&p, &bytes, 0o600)).await
            {
                Ok(Ok(())) => last_body = Some(body),
                Ok(Err(e)) => tracing::warn!(error = %e, "status cache write failed"),
                Err(e) => tracing::warn!(error = %e, "status cache write task failed"),
            }
        }
        // Stopped: nothing of ours is in flight any more.
        if *stopped.borrow() == Stop::Keep {
            return;
        }
        if let Err(e) = std::fs::remove_file(&path) {
            if e.kind() != std::io::ErrorKind::NotFound {
                tracing::warn!(error = %e, "could not delete the status cache");
            }
        }
    });
    StatusCache { stop, task }
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

/// One `sh` word as [`sh_word`] writes it (plain, or single-quoted with `'\''` escapes) at the
/// start of `s`; returns the rest. `None` if `s` does not start with one.
fn take_word(s: &str) -> Option<&str> {
    read_word(s).map(|(_, rest)| rest)
}

/// [`take_word`], with the word's value (quotes removed).
fn read_word(s: &str) -> Option<(String, &str)> {
    if let Some(mut rest) = s.strip_prefix('\'') {
        let mut word = String::new();
        loop {
            let end = rest.find('\'')?;
            word.push_str(&rest[..end]);
            rest = &rest[end + 1..];
            match rest.strip_prefix(r"\''") {
                Some(more) => {
                    word.push('\'');
                    rest = more;
                }
                None => return Some((word, rest)),
            }
        }
    }
    let end = s
        .find(|c: char| !(c.is_ascii_alphanumeric() || "/._-+:@%=,".contains(c)))
        .unwrap_or(s.len());
    (end > 0).then(|| (s[..end].to_string(), &s[end..]))
}

/// What a wrapper `statusLine` ([`is_wrapper`]) names: its `POOLTIME_HOME` (if any) and binary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WrapperParts {
    pub home: Option<PathBuf>,
    pub bin: PathBuf,
}

/// The parts of `v` when it is our wrapper for profile `id`; `None` otherwise.
pub fn wrapper_parts(v: &Value, id: &str) -> Option<WrapperParts> {
    if !is_wrapper(v, id) {
        return None;
    }
    let c = v.get("command")?.as_str()?;
    let (home, c) = match c.strip_prefix("POOLTIME_HOME=") {
        Some(rest) => {
            let (h, r) = read_word(rest)?;
            (Some(PathBuf::from(h)), r.strip_prefix(' ')?)
        }
        None => (None, c),
    };
    let (bin, _) = read_word(c)?;
    Some(WrapperParts {
        home,
        bin: PathBuf::from(bin),
    })
}

/// The `statusLine` command for `id`: `[POOLTIME_HOME=<home> ]<bin> statusline --wrap <id>`.
/// `home` is the daemon's data dir when it is not the default one, so the wrapper reads the cache
/// and snapshot of the pool that installed it whatever Claude Code's environment says.
pub fn wrapper_command(bin: &Path, id: &str, home: Option<&Path>) -> String {
    let env = home.map_or(String::new(), |h| {
        format!("POOLTIME_HOME={} ", sh_word(&h.to_string_lossy()))
    });
    format!(
        "{env}{} statusline --wrap {}",
        sh_word(&bin.to_string_lossy()),
        sh_word(id)
    )
}

/// Whether the `statusLine` value `v` is our wrapper for profile `id`: exactly the shape
/// [`wrapper_command`] writes (optional `POOLTIME_HOME=<word> `, one word naming the binary, then
/// ` statusline --wrap <id>`), whatever binary or home it names. Never snapshotted as the user's.
pub fn is_wrapper(v: &Value, id: &str) -> bool {
    let Some(c) = v.get("command").and_then(Value::as_str) else {
        return false;
    };
    let c = match c.strip_prefix("POOLTIME_HOME=") {
        Some(rest) => match take_word(rest).and_then(|r| r.strip_prefix(' ')) {
            Some(r) => r,
            None => return false,
        },
        None => c,
    };
    take_word(c).is_some_and(|rest| rest == format!(" statusline --wrap {}", sh_word(id)))
}

/// The data dir to name in the wrapper: `paths.home` unless it is the default
/// (`$HOME/.pooltime`).
fn non_default_home(paths: &Paths) -> Option<&Path> {
    let default = std::env::var_os("HOME")
        .filter(|h| !h.is_empty())
        .map(|h| Path::new(&h).join(".pooltime"));
    (default.as_deref() != Some(paths.home.as_path())).then_some(paths.home.as_path())
}

/// The wrapper `statusLine`: the user's own object (`base`) with `type` and `command` replaced,
/// so its other fields (`padding`, ...) are kept; just `{type, command}` otherwise.
fn wrapper_value(base: &Value, command: &str) -> Value {
    let mut obj = match base {
        Value::Object(m) => m.clone(),
        _ => Map::new(),
    };
    obj.insert("type".into(), json!("command"));
    obj.insert("command".into(), json!(command));
    Value::Object(obj)
}

/// Install the wrapper ([`wrapper_command`]) as `p`'s `statusLine`. The user's own value (`null`
/// when absent) goes to the profile snapshot first; a `statusLine` that already is our wrapper is
/// never snapshotted (re-connecting keeps the original prior). Needs the snapshot `connect` has
/// just written; an absent or unreadable one is an error and nothing is changed.
pub fn install(p: &Profile, paths: &Paths, bin: &Path) -> anyhow::Result<WriteOutcome> {
    let snap_path = snapshot::snapshot_path(paths, p);
    let command = wrapper_command(bin, &p.id, non_default_home(paths));
    let mut failure: Option<anyhow::Error> = None;
    let outcome = patch_json(&p.settings_json, |settings| {
        match install_once(settings, &command, &p.id, &snap_path) {
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
    command: &str,
    id: &str,
    snap_path: &Path,
) -> anyhow::Result<bool> {
    let SnapshotState::Present(mut snap) = snapshot::read(snap_path) else {
        return Err(anyhow!(
            "no readable snapshot at {}; status line not installed",
            snap_path.display()
        ));
    };
    let current = settings.get("statusLine").cloned();
    let prior = match current {
        // Ours already (maybe another binary or home): the snapshot holds the original.
        Some(c) if is_wrapper(&c, id) => snap.statusline_prior,
        other => {
            let prior = other.unwrap_or(Value::Null);
            if snap.statusline_prior != prior {
                snap.statusline_prior = prior.clone();
                snapshot::write(snap_path, &snap)?;
            }
            prior
        }
    };
    let desired = wrapper_value(&prior, command);
    if settings.get("statusLine") == Some(&desired) {
        return Ok(false);
    }
    settings.insert("statusLine".into(), desired);
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

    fn cache(written_at: i64, used: Value, needs_login: u64, today: u64) -> Vec<u8> {
        serde_json::to_vec(&json!({
            "written_at": written_at,
            "port": 3460,
            "pool": {"available": 6, "total": 8},
            "seven_day_used": used,
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
        let stale = cache(now - 151, json!(0.59), 0, 0);
        assert_eq!(segment(Some(&stale), now), "pool down", "older than 150 s");
        let fresh = cache(now - 150, json!(0.59), 2, 0);
        assert_eq!(
            segment(Some(&fresh), now),
            "pool 6/8 · 7d 59% used · ⚠ 2 need login"
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
            "pool 6/8 · 7d 100% used · ⏳ 1 expiring"
        );
        assert_eq!(
            segment(Some(&cache(now, json!(0.0), 0, 0)), now),
            "pool 6/8 · 7d 0% used"
        );
    }

    #[test]
    fn segment_reads_a_cache_from_before_the_used_field() {
        let now = 1_800_000_000;
        let old = serde_json::to_vec(&json!({
            "written_at": now, "port": 3460, "pool": {"available": 6, "total": 8},
            "seven_day_left": 0.41,
            "counts": {"needs_login": 0, "expiring_today": 0, "expiring": 0, "erroring": 0},
        }))
        .unwrap();
        assert_eq!(segment(Some(&old), now), "pool 6/8 · 7d 59% used");
    }

    #[test]
    fn cache_body_reuses_the_status_figures() {
        let status = json!({
            "accounts": [{"status": "erroring"}, {"status": "ready"}, {"status": "needs_login"}],
            "pool": {"seven_day_left_total": 1.5, "seven_day_used_total": 1.5, "accounts": 3, "available": 2},
            "counts": {"needs_login": 1, "expiring_today": 0, "expiring": 1},
        });
        assert_eq!(
            cache_body(&status, 3460),
            json!({
                "port": 3460,
                "pool": {"available": 2, "total": 3},
                "seven_day_used": 0.5,
                "seven_day_left": 0.5,
                "counts": {"needs_login": 1, "expiring_today": 0, "expiring": 1, "erroring": 1},
            })
        );
        let empty = json!({"pool": {"seven_day_used_total": 0.0, "accounts": 0, "available": 0}});
        assert!(cache_body(&empty, 1)["seven_day_used"].is_null());
        assert!(cache_body(&empty, 1)["seven_day_left"].is_null());
    }

    fn line(c: &str) -> Value {
        json!({"type": "command", "command": c})
    }

    #[test]
    fn wrapper_command_quotes_and_is_recognised() {
        let bin = Path::new("/opt/homebrew/bin/pooltime");
        let cmd = wrapper_command(bin, "claude-work", None);
        assert_eq!(
            cmd,
            "/opt/homebrew/bin/pooltime statusline --wrap claude-work"
        );
        let homed = wrapper_command(bin, "claude-work", Some(Path::new("/tmp/pt home")));
        assert_eq!(
            homed,
            "POOLTIME_HOME='/tmp/pt home' /opt/homebrew/bin/pooltime statusline --wrap claude-work"
        );
        let spaced = wrapper_command(Path::new("/My Apps/pooltime"), "it's", None);
        assert_eq!(spaced, r"'/My Apps/pooltime' statusline --wrap 'it'\''s'");
        assert!(is_wrapper(&line(&cmd), "claude-work"));
        assert!(is_wrapper(&line(&homed), "claude-work"));
        assert!(is_wrapper(&line(&spaced), "it's"));
        assert!(!is_wrapper(&line(&cmd), "other"));
        assert!(!is_wrapper(&line("ccline"), "claude-work"));
        // A user command that merely ends in the same words is not ours.
        for user in [
            "ccline && pooltime statusline --wrap claude-work",
            "env X=1 pooltime statusline --wrap claude-work",
            "POOLTIME_HOME=/x a b statusline --wrap claude-work",
            "'unterminated statusline --wrap claude-work",
            " statusline --wrap claude-work",
        ] {
            assert!(!is_wrapper(&line(user), "claude-work"), "{user}");
        }
        // The doctor reads back what was written, quotes removed.
        let parts = |c: &str, id: &str| wrapper_parts(&line(c), id).unwrap();
        assert_eq!(
            parts(&homed, "claude-work"),
            WrapperParts {
                home: Some("/tmp/pt home".into()),
                bin: bin.into()
            }
        );
        assert_eq!(parts(&spaced, "it's").bin, Path::new("/My Apps/pooltime"));
        assert_eq!(parts(&cmd, "claude-work").home, None);
        assert!(wrapper_parts(&line(&cmd), "other").is_none());
    }

    #[test]
    fn wrapper_keeps_the_originals_other_fields() {
        let w = wrapper_value(
            &json!({"type": "command", "command": "ccline", "padding": 2}),
            "pt statusline --wrap x",
        );
        assert_eq!(
            w,
            json!({"type": "command", "command": "pt statusline --wrap x", "padding": 2})
        );
        assert_eq!(
            wrapper_value(&Value::Null, "c"),
            json!({"type": "command", "command": "c"})
        );
    }
}
