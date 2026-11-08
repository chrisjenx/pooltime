//! Structured logging, log rotation/pruning, request ids and header redaction.

use std::io::{self, IsTerminal};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use http::HeaderMap;
use rand::RngExt;
use sha2::{Digest, Sha256};
use time::OffsetDateTime;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{fmt, EnvFilter};

use crate::config::{Paths, RetentionCfg};

/// Base name of the daily-rotated log files (`pooltime.log.YYYY-MM-DD`).
pub const LOG_PREFIX: &str = "pooltime.log";

/// Env var holding an `EnvFilter` directive string (default `info`).
pub const LOG_ENV: &str = "POOLTIME_LOG";

const REDACTED: &str = "<redacted>";

/// Header names whose values are always masked.
const SECRET_HEADERS: &[&str] = &[
    "authorization",
    "proxy-authorization",
    "x-api-key",
    "cookie",
    "set-cookie",
    "chatgpt-account-id",
];

/// Keeps the non-blocking file writer alive; drop flushes pending lines.
pub struct LogGuard {
    _worker: tracing_appender::non_blocking::WorkerGuard,
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct PruneStats {
    pub deleted_files: usize,
    pub deleted_bytes: u64,
}

/// Install the global tracing subscriber.
///
/// - JSON lines to `<logs>/pooltime.log.<date>` (daily rotation, non-blocking), always.
/// - `foreground_tty`: also human-readable output to stderr; services (non-TTY) log to the file only.
/// - Filter: `POOLTIME_LOG` (EnvFilter syntax) or `info`.
///
/// Old files are pruned once at startup. Safe to call more than once: a second
/// global subscriber is ignored (the first stays installed).
///
/// Spans should carry `request_id`, `account` and `provider` fields where known.
pub fn init(paths: &Paths, cfg: &RetentionCfg, foreground_tty: bool) -> anyhow::Result<LogGuard> {
    paths.ensure()?;
    if let Err(e) = prune_logs(
        &paths.logs,
        cfg.logs_days,
        u64::from(cfg.logs_max_mb),
        SystemTime::now(),
    ) {
        eprintln!("pooltime: log pruning failed: {e}");
    }
    let directive = std::env::var(LOG_ENV).ok();
    let (subscriber, guard) = build_subscriber(&paths.logs, directive.as_deref(), foreground_tty)?;
    // try_init: a subscriber may already be installed (tests, repeated init).
    let _ = subscriber.try_init();
    Ok(guard)
}

/// Parse a `POOLTIME_LOG` directive; empty/absent means `info`, invalid warns and means `info`.
fn make_filter(directive: Option<&str>) -> EnvFilter {
    match directive.map(str::trim).filter(|v| !v.is_empty()) {
        None => EnvFilter::new("info"),
        Some(d) => EnvFilter::try_new(d).unwrap_or_else(|_| {
            eprintln!("pooltime: invalid {LOG_ENV} directive {d:?}, using \"info\"");
            EnvFilter::new("info")
        }),
    }
}

/// Build (without installing) the subscriber: JSON lines to the daily-rolled file in `logs_dir`,
/// plus human-readable stderr output when `foreground_tty`.
fn build_subscriber(
    logs_dir: &Path,
    directive: Option<&str>,
    foreground_tty: bool,
) -> anyhow::Result<(impl tracing::Subscriber + Send + Sync, LogGuard)> {
    let appender = tracing_appender::rolling::Builder::new()
        .rotation(tracing_appender::rolling::Rotation::DAILY)
        .filename_prefix(LOG_PREFIX)
        .build(logs_dir)?;
    let (writer, worker) = tracing_appender::non_blocking(appender);

    let file_layer = fmt::layer().json().with_ansi(false).with_writer(writer);
    let tty_layer = foreground_tty.then(|| {
        fmt::layer()
            .with_ansi(io::stderr().is_terminal())
            .with_writer(io::stderr)
    });
    let subscriber = tracing_subscriber::registry()
        .with(make_filter(directive))
        .with(file_layer)
        .with(tty_layer);
    Ok((subscriber, LogGuard { _worker: worker }))
}

/// Delete `pooltime.log*` files in `dir` older than `days`, then oldest-first until the
/// total size is at most `max_mb` MiB. The newest file (the current day's) is never deleted.
/// A missing directory is not an error. Other files are left alone.
pub fn prune_logs(dir: &Path, days: u32, max_mb: u64, now: SystemTime) -> io::Result<PruneStats> {
    let rd = match std::fs::read_dir(dir) {
        Ok(rd) => rd,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(PruneStats::default()),
        Err(e) => return Err(e),
    };
    // (path, mtime, size)
    let mut files = Vec::new();
    for entry in rd {
        let entry = entry?;
        if !entry.file_name().to_string_lossy().starts_with(LOG_PREFIX) {
            continue;
        }
        let md = entry.metadata()?;
        if !md.is_file() {
            continue;
        }
        files.push((entry.path(), md.modified()?, md.len()));
    }
    // Oldest first; the last element is the current file and is protected.
    files.sort_by(|a, b| a.1.cmp(&b.1).then_with(|| a.0.cmp(&b.0)));
    let protected = files.len().saturating_sub(1);

    let max_age = Duration::from_secs(u64::from(days) * 86_400);
    let cap = max_mb.saturating_mul(1024 * 1024);
    let mut total: u64 = files.iter().map(|f| f.2).sum();
    let mut stats = PruneStats::default();

    for (path, mtime, size) in files.iter().take(protected) {
        let too_old = now.duration_since(*mtime).is_ok_and(|age| age > max_age);
        if !too_old && total <= cap {
            // Sorted oldest-first: nothing later is older, and size is within cap.
            break;
        }
        match std::fs::remove_file(path) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
        total = total.saturating_sub(*size);
        stats.deleted_files += 1;
        stats.deleted_bytes += size;
    }
    Ok(stats)
}

/// `req_` + 20 lowercase RFC 4648 base32 characters (a-z, 2-7).
pub fn new_request_id() -> String {
    const ALPHABET: &[u8; 32] = b"abcdefghijklmnopqrstuvwxyz234567";
    let mut rng = rand::rng();
    let mut id = String::with_capacity(24);
    id.push_str("req_");
    for _ in 0..20 {
        id.push(ALPHABET[rng.random_range(0..32usize)] as char);
    }
    id
}

/// Header list safe to log: secret headers and OAuth `code`/`state` params are masked.
pub fn redact_headers(h: &HeaderMap) -> Vec<(String, String)> {
    h.iter()
        .map(|(name, value)| {
            let name = name.as_str().to_ascii_lowercase();
            let value = if SECRET_HEADERS.contains(&name.as_str()) {
                REDACTED.to_string()
            } else {
                redact_oauth_params(&String::from_utf8_lossy(value.as_bytes()))
            };
            (name, value)
        })
        .collect()
}

/// A URI (or path and query) safe to log: `code=` / `state=` / `k=` query values masked.
pub fn redact_uri(uri: &str) -> String {
    redact_oauth_params(uri)
}

/// One debug-mode header line: `name: value` pairs after [`redact_headers`], joined by `; `.
pub fn debug_headers(h: &HeaderMap) -> String {
    redact_headers(h)
        .into_iter()
        .map(|(k, v)| format!("{k}: {v}"))
        .collect::<Vec<_>>()
        .join("; ")
}

/// Source of "now" for [`DebugMode`] (injectable for tests).
pub type Clock = Arc<dyn Fn() -> OffsetDateTime + Send + Sync>;

/// Longest debug-mode window, in minutes.
pub const DEBUG_MAX_MINUTES: u32 = 60;

/// Time-boxed debug mode (spec §6/§16): while active, inbound and upstream request/response
/// *headers* are logged at `info` with the request id, redacted by [`redact_headers`]. Never
/// bodies. Held in memory only, so a restart ends it. Cheap to clone (shared state).
#[derive(Clone)]
pub struct DebugMode {
    until: Arc<Mutex<Option<OffsetDateTime>>>,
    clock: Clock,
}

impl Default for DebugMode {
    fn default() -> Self {
        DebugMode::with_clock(Arc::new(OffsetDateTime::now_utc))
    }
}

impl std::fmt::Debug for DebugMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DebugMode")
            .field("until", &self.until())
            .finish()
    }
}

impl DebugMode {
    pub fn with_clock(clock: Clock) -> DebugMode {
        DebugMode {
            until: Arc::new(Mutex::new(None)),
            clock,
        }
    }

    pub fn now(&self) -> OffsetDateTime {
        (self.clock)()
    }

    /// Turn on for `minutes` (1..=60) from now; returns the expiry. `None` when out of range.
    pub fn enable(&self, minutes: u32) -> Option<OffsetDateTime> {
        if !(1..=DEBUG_MAX_MINUTES).contains(&minutes) {
            return None;
        }
        let until = self.now() + time::Duration::minutes(i64::from(minutes));
        *self.until.lock().unwrap_or_else(|e| e.into_inner()) = Some(until);
        tracing::info!(until = %until, "debug mode on: request/response headers are logged (redacted)");
        Some(until)
    }

    pub fn disable(&self) {
        let was = self.until.lock().unwrap_or_else(|e| e.into_inner()).take();
        if was.is_some() {
            tracing::info!("debug mode off");
        }
    }

    /// The expiry while active, else `None` (an expired window reads as off).
    pub fn until(&self) -> Option<OffsetDateTime> {
        let now = self.now();
        let mut g = self.until.lock().unwrap_or_else(|e| e.into_inner());
        if g.is_some_and(|u| u <= now) {
            *g = None;
        }
        *g
    }

    pub fn active(&self) -> bool {
        self.until().is_some()
    }
}

/// Replace the value of any `code=` / `state=` parameter, and the dashboard's `k=` (management
/// key), at string start or after `?`, `&`, `#`.
fn redact_oauth_params(v: &str) -> String {
    let mut out = String::with_capacity(v.len());
    let mut at_param_start = true;
    let mut rest = v;
    while !rest.is_empty() {
        if at_param_start {
            let hit = ["code=", "state=", "k="]
                .into_iter()
                .find_map(|key| rest.strip_prefix(key).map(|after| (key, after)));
            if let Some((key, after)) = hit {
                out.push_str(key);
                out.push_str(REDACTED);
                rest = &after[after.find(['&', '#']).unwrap_or(after.len())..];
                at_param_start = false;
                continue;
            }
        }
        let c = rest.chars().next().expect("non-empty");
        at_param_start = matches!(c, '?' | '&' | '#');
        out.push(c);
        rest = &rest[c.len_utf8()..];
    }
    out
}

/// Stable, non-reversible session tag for logs: first 16 hex chars of sha256.
pub fn hash_session(user_id: &str) -> String {
    let digest = Sha256::digest(user_id.as_bytes());
    hex::encode(digest)[..16].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use filetime::{set_file_mtime, FileTime};
    use http::HeaderValue;
    use std::collections::HashSet;
    use std::time::Duration;

    fn get<'a>(v: &'a [(String, String)], k: &str) -> &'a str {
        v.iter()
            .find(|(n, _)| n == k)
            .map(|(_, v)| v.as_str())
            .unwrap()
    }

    #[test]
    fn redact_headers_masks_secrets() {
        let mut h = HeaderMap::new();
        h.insert(
            "authorization",
            HeaderValue::from_static("Bearer sk-secret"),
        );
        h.insert("Proxy-Authorization", HeaderValue::from_static("Basic abc"));
        h.insert("x-api-key", HeaderValue::from_static("key"));
        h.insert("cookie", HeaderValue::from_static("a=b"));
        h.insert("set-cookie", HeaderValue::from_static("a=b; HttpOnly"));
        h.insert("chatgpt-account-id", HeaderValue::from_static("acct-1"));
        h.insert("content-type", HeaderValue::from_static("application/json"));
        h.insert(
            "location",
            HeaderValue::from_static("http://localhost/cb?code=abc&state=xyz&other=1"),
        );
        let r = redact_headers(&h);
        for k in [
            "authorization",
            "proxy-authorization",
            "x-api-key",
            "cookie",
            "set-cookie",
            "chatgpt-account-id",
        ] {
            assert_eq!(get(&r, k), "<redacted>", "{k}");
        }
        assert_eq!(get(&r, "content-type"), "application/json");
        assert_eq!(
            get(&r, "location"),
            "http://localhost/cb?code=<redacted>&state=<redacted>&other=1"
        );
        let flat = format!("{r:?}");
        assert!(!flat.contains("sk-secret") && !flat.contains("abc") && !flat.contains("xyz"));
    }

    #[test]
    fn redact_location_only_code_and_state_in_fragment_and_query() {
        let mut h = HeaderMap::new();
        h.insert(
            "location",
            HeaderValue::from_static("/x?a=1&state=s#code=c"),
        );
        let r = redact_headers(&h);
        assert_eq!(
            get(&r, "location"),
            "/x?a=1&state=<redacted>#code=<redacted>"
        );
    }

    #[test]
    fn debug_header_line_and_uri_are_redacted() {
        let mut h = HeaderMap::new();
        h.insert("authorization", HeaderValue::from_static("Bearer tok"));
        h.insert("chatgpt-account-id", HeaderValue::from_static("acct-9"));
        h.insert("anthropic-version", HeaderValue::from_static("2023-06-01"));
        let line = debug_headers(&h);
        assert!(line.contains("authorization: <redacted>"), "{line}");
        assert!(line.contains("chatgpt-account-id: <redacted>"), "{line}");
        assert!(line.contains("anthropic-version: 2023-06-01"), "{line}");
        assert!(!line.contains("tok") && !line.contains("acct-9"));
        assert_eq!(
            redact_uri("/cb?code=abc&state=xyz&x=1&k=mgmtkey"),
            "/cb?code=<redacted>&state=<redacted>&x=1&k=<redacted>"
        );
    }

    #[test]
    fn log_files_pruned_by_size_and_age() {
        let dir = tempfile::tempdir().unwrap();
        let now = SystemTime::now();
        let mk = |name: &str, bytes: usize, age_days: u64| {
            let p = dir.path().join(name);
            std::fs::write(&p, vec![b'x'; bytes]).unwrap();
            let t = now - Duration::from_secs(age_days * 86400 + 60);
            set_file_mtime(&p, FileTime::from_system_time(t)).unwrap();
        };
        const MB: usize = 1024 * 1024;
        // age: 10d old gets deleted with days=7
        mk("pooltime.log.2026-10-01", 10, 10);
        mk("pooltime.log.2026-11-01", MB, 6);
        mk("pooltime.log.2026-11-03", MB, 4);
        mk("pooltime.log.2026-11-05", MB, 2);
        mk("pooltime.log.2026-11-07", MB, 0); // current
        mk("unrelated.txt", 5 * MB, 30);
        let s = prune_logs(dir.path(), 7, 3, now).unwrap();
        assert!(!dir.path().join("pooltime.log.2026-10-01").exists());
        // 4 MB remain over a 3 MB cap: oldest (11-01) removed
        assert!(!dir.path().join("pooltime.log.2026-11-01").exists());
        assert!(dir.path().join("pooltime.log.2026-11-03").exists());
        assert!(dir.path().join("pooltime.log.2026-11-05").exists());
        assert!(dir.path().join("pooltime.log.2026-11-07").exists());
        assert!(dir.path().join("unrelated.txt").exists());
        assert_eq!(s.deleted_files, 2);
        assert_eq!(s.deleted_bytes, 10 + MB as u64);

        // Cap below the current file alone: current file is never deleted.
        let s = prune_logs(dir.path(), 7, 0, now).unwrap();
        assert_eq!(s.deleted_files, 2);
        assert!(dir.path().join("pooltime.log.2026-11-07").exists());
        assert!(!dir.path().join("pooltime.log.2026-11-05").exists());
    }

    #[test]
    fn prune_missing_dir_is_ok() {
        let dir = tempfile::tempdir().unwrap();
        let s = prune_logs(&dir.path().join("nope"), 7, 1, SystemTime::now()).unwrap();
        assert_eq!(s, PruneStats::default());
    }

    #[test]
    fn request_ids_are_unique_and_prefixed() {
        let ids: HashSet<String> = (0..1000).map(|_| new_request_id()).collect();
        assert_eq!(ids.len(), 1000);
        for id in &ids {
            let rest = id.strip_prefix("req_").expect("prefix");
            assert_eq!(rest.len(), 20);
            assert!(rest.bytes().all(|b| matches!(b, b'a'..=b'z' | b'2'..=b'7')));
        }
    }

    #[test]
    fn hash_session_is_stable_16_hex() {
        let a = hash_session("user-1");
        assert_eq!(a.len(), 16);
        assert!(a.bytes().all(|b| b.is_ascii_hexdigit()));
        assert_eq!(a, hash_session("user-1"));
        assert_ne!(a, hash_session("user-2"));
        // sha256("abc") = ba7816bf8f01cfea...
        assert_eq!(hash_session("abc"), "ba7816bf8f01cfea");
    }

    /// Run `emit` under a file-only subscriber built with `directive`; return parsed JSON lines.
    fn capture(directive: Option<&str>, emit: impl FnOnce()) -> Vec<serde_json::Value> {
        crate::test_support::ensure_callsites_stay_enabled();
        let dir = tempfile::tempdir().unwrap();
        let (subscriber, guard) = build_subscriber(dir.path(), directive, false).unwrap();
        tracing::subscriber::with_default(subscriber, emit);
        drop(guard); // flushes the non-blocking writer
        let mut lines = Vec::new();
        for e in std::fs::read_dir(dir.path()).unwrap() {
            let e = e.unwrap();
            assert!(e.file_name().to_string_lossy().starts_with(LOG_PREFIX));
            for l in std::fs::read_to_string(e.path()).unwrap().lines() {
                lines.push(serde_json::from_str(l).expect("each line is JSON"));
            }
        }
        lines
    }

    fn emit_sample() {
        let span = tracing::info_span!(
            "req",
            request_id = "req_abc",
            account = "a1",
            provider = "anthropic"
        );
        let _g = span.enter();
        tracing::debug!("debug-line");
        tracing::info!("info-line");
    }

    fn msg(v: &serde_json::Value) -> &str {
        v["fields"]["message"].as_str().unwrap()
    }

    #[test]
    fn json_file_has_message_and_span_fields_and_default_filter_drops_debug() {
        let lines = capture(None, emit_sample);
        assert_eq!(lines.len(), 1, "{lines:?}");
        let l = &lines[0];
        assert_eq!(msg(l), "info-line");
        assert_eq!(l["level"], "INFO");
        assert_eq!(l["span"]["request_id"], "req_abc");
        assert_eq!(l["span"]["account"], "a1");
        assert_eq!(l["span"]["provider"], "anthropic");
    }

    #[test]
    fn pooltime_log_directive_enables_debug() {
        let lines = capture(Some("debug"), emit_sample);
        let msgs: Vec<_> = lines.iter().map(msg).collect();
        assert_eq!(msgs, ["debug-line", "info-line"]);
        // Invalid or blank directives fall back to info.
        assert_eq!(capture(Some("   "), emit_sample).len(), 1);
        assert_eq!(capture(Some("not a [valid"), emit_sample).len(), 1);
    }

    #[test]
    fn init_is_repeatable_and_creates_log_dir() {
        // init() installs a global subscriber; make sure ours wins the race so the capture
        // tests in this binary keep a permissive global fallback.
        crate::test_support::ensure_callsites_stay_enabled();
        let home = tempfile::tempdir().unwrap();
        let paths = Paths::under(home.path());
        let cfg = RetentionCfg::default();
        let g1 = init(&paths, &cfg, false).unwrap();
        let g2 = init(&paths, &cfg, false).unwrap();
        drop((g1, g2));
        assert!(paths.logs.is_dir());
    }
}
