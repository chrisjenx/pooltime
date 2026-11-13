//! `pooltime doctor`: one line per check, `✓` (fine), `!` (worth a look), `✗` (broken) or `i`
//! (for information, never a problem), each problem with the command that fixes it. Exit 1 when
//! any check is `✗`.
//!
//! Checks, in order:
//! - **service**: launchd/systemd definition installed and running (`!` when not installed: the
//!   pool then runs only while `pooltime serve` does; `✗` when installed but not running; `!`
//!   when the definition predates seamless restarts and needs one `pooltime service install`).
//! - **health**: what answers `/health` on the configured address (pooltime `✓`; bro, another
//!   server, a hung listener or nothing `✗`).
//! - **profile <id>**, per profile recorded in `config.toml`: still connected (a profile
//!   snapshot plus the base URL), pointing at this pool, pins current. Connected while nothing
//!   pooltime answers is `✗` ("connected, pool down"): Claude Code has no fallback for an
//!   unreachable base URL. A pool that answers but whose status cache is missing or older than
//!   [`STALE_AFTER`] is one `!` **status cache** line (status lines then say `pool down`). Pins are compared with the pins derived from the live Anthropic
//!   rows of the pool's `/v1/models` listing (see [`live_rows`]); with no live rows they are not
//!   compared. Stale pins are `!` (the pool re-syncs connected profiles every minute). A foreign
//!   base URL is shown as its origin only.
//! - **statusline <id>**, per recorded profile whose `statusLine` is our wrapper: the binary it
//!   runs exists and is executable (`✗`), the snapshot holding the original status line is
//!   readable (`!`), and the data dir it reads is this one (`!`).
//! - **mcp <id>**, per recorded profile whose snapshot says the MCP server is registered: the
//!   `.claude.json` entry is there, on this port, with the token in `run/mcp.key` (compared in
//!   constant time, never shown). Any mismatch is `!`.
//! - **account <id>**: the running pool's status (unreadable status: `✗`, no fallback), else,
//!   with no pool, the files. `needs_login` / corrupt credentials `✗`; a recorded login expiry
//!   that passed (unconfirmed: only a refused probe refresh means needs_login) or falls within
//!   the largest `alerts.warn_days` `!`.
//! - **errors**: the pool's journal stats over the last 24 h; an account with at least
//!   [`ERROR_MIN_ATTEMPTS`] attempts and an error rate of [`ERROR_RATE_WARN`] or more is `!`
//!   (with its most frequent error class and latency), else one `✓` summary. Failover hand-overs
//!   and `invalid_request` attempts are not counted. No pool: `i`.
//! - **upstream <origin>**: `HEAD` to the Anthropic API and token origins, plus the OpenAI/Codex
//!   API and token origins when an OpenAI account exists (all from config, so tests point them at
//!   local fakes). Any HTTP answer is reachable; a timeout is `!` (slow or offline network); a
//!   DNS, connect or TLS failure is `✗`.
//! - **version**: the latest GitHub release (the only request beyond the pool and the upstream
//!   origins), `!` when newer than this binary; any failure is `i`. Off with
//!   `--no-version-check` or `POOLTIME_NO_VERSION_CHECK=1`; `POOLTIME_RELEASES_API` points it
//!   elsewhere (tests).
//! - **clock**: skew against the `Date` header of the first upstream answer, else of the release
//!   check's answer (`!` over 60 s, `✗` over 5 min: sign-in and token expiry depend on it); `i`
//!   when no server sent one.
//! - **notifications**: the desktop notifier alerts would use (`i` when `alerts.notify` is off;
//!   `!` when none is installed).
//! - **permissions**: data dir and its subdirs without group/other access (0700), config,
//!   database, management and MCP keys, profile snapshots and account files likewise (0600); each
//!   offender is a `✗` line with its `chmod`.
//! - **storage**: database against `retention.journal_max_mb` (at the cap is normal; `!` only
//!   above 125 % of it, when pruning is not keeping up; with no cap, `!` above 1 GiB with the
//!   logs), logs, and journal rows against `retention.journal_max_requests` (from the pool, when
//!   it runs). **retention**: the settings (`i`).
//!
//! Every call to the pool is bounded by 5 s, as is each upstream `HEAD`; the release check by
//! 3 s. The doctor only reads: it never creates a file, and it never prints a credential.

use std::fmt;
use std::net::SocketAddr;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::Value;
use subtle::ConstantTimeEq;
use time::OffsetDateTime;

use super::client::{self, Client, Located};
use crate::accounts::Registry;
use crate::claude_code::snapshot::{self, SnapshotState};
use crate::claude_code::statusline::{cache_is_fresh, wrapper_parts, STALE_AFTER};
use crate::claude_code::{derive_pins, is_connected, mcp, profile_of};
use crate::config::{Config, Paths, ProfileRef, RetentionCfg};
use crate::health::notify::{Notification, SystemNotifier};
use crate::providers::anthropic::ANTHROPIC_OWNER;
use crate::server::health::{probe_addr, Probe};
use crate::service::{Backend, Env, SystemRunner};

/// How bad a check came out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Ok,
    Warn,
    Fail,
    /// Not done (a setup step that was skipped).
    Skip,
    /// For information only: a check that was not run, or a setting.
    Info,
}

impl Level {
    pub fn symbol(self) -> &'static str {
        match self {
            Level::Ok => "✓",
            Level::Warn => "!",
            Level::Fail => "✗",
            Level::Skip => "-",
            Level::Info => "i",
        }
    }
}

/// One output line: `<symbol> <label>: <detail> — <hint>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Check {
    pub level: Level,
    pub label: String,
    pub detail: String,
    pub hint: Option<String>,
}

impl Check {
    pub fn new(level: Level, label: impl Into<String>, detail: impl Into<String>) -> Check {
        Check {
            level,
            label: label.into(),
            detail: detail.into(),
            hint: None,
        }
    }

    pub fn hint(mut self, hint: impl Into<String>) -> Check {
        self.hint = Some(hint.into());
        self
    }
}

impl fmt::Display for Check {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} {}: {}", self.level.symbol(), self.label, self.detail)?;
        if let Some(h) = &self.hint {
            write!(f, " — {h}")?;
        }
        Ok(())
    }
}

/// Clock skew bounds.
const SKEW_WARN: i64 = 60;
const SKEW_FAIL: i64 = 5 * 60;
/// With no database cap, database plus logs above this is `!`.
const DISK_WARN: u64 = 1 << 30;
/// Per upstream `HEAD`.
const UPSTREAM_TIMEOUT: Duration = Duration::from_secs(5);
/// The release check.
const RELEASE_TIMEOUT: Duration = Duration::from_secs(3);
/// An account needs this many attempts in the window before its error rate counts...
pub const ERROR_MIN_ATTEMPTS: u64 = 20;
/// ...and is `!` at this rate or above.
pub const ERROR_RATE_WARN: f64 = 0.10;
/// The database is `!` above this multiple of `retention.journal_max_mb` (pruning to the cap is
/// the steady state; this far past it, pruning is not keeping up).
const DB_OVER_CAP: f64 = 1.25;

/// The latest release of pooltime.
pub const RELEASES_API: &str = "https://api.github.com/repos/chrisjenx/pooltime/releases/latest";
/// Points the release check elsewhere (tests).
pub const RELEASES_API_ENV: &str = "POOLTIME_RELEASES_API";
/// `1` (or any value but `0` / `false`) turns the release check off.
pub const NO_VERSION_CHECK_ENV: &str = "POOLTIME_NO_VERSION_CHECK";

/// What the doctor may do beyond the local checks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Options {
    /// Ask GitHub for the latest release.
    pub version_check: bool,
    pub releases_api: String,
}

impl Options {
    /// From the environment; `no_version_check` is the `--no-version-check` flag.
    pub fn from_env(no_version_check: bool) -> Options {
        let var = |k: &str| std::env::var(k).ok().filter(|v| !v.trim().is_empty());
        let env_off = var(NO_VERSION_CHECK_ENV)
            .is_some_and(|v| !matches!(v.trim().to_ascii_lowercase().as_str(), "0" | "false"));
        Options {
            version_check: !(no_version_check || env_off),
            releases_api: var(RELEASES_API_ENV).unwrap_or_else(|| RELEASES_API.to_string()),
        }
    }
}

/// `pooltime doctor`.
pub async fn run(paths: &Paths, opts: &Options) -> anyhow::Result<i32> {
    let checks = checks(paths, opts).await;
    print(&checks);
    Ok(exit_code(&checks))
}

/// 1 when any check failed.
pub fn exit_code(checks: &[Check]) -> i32 {
    i32::from(checks.iter().any(|c| c.level == Level::Fail))
}

pub fn print(checks: &[Check]) {
    println!("pooltime {}", env!("CARGO_PKG_VERSION"));
    for c in checks {
        println!("{c}");
    }
}

/// Run every check.
pub async fn checks(paths: &Paths, opts: &Options) -> Vec<Check> {
    let mut out = Vec::new();
    // A parse error could quote config.toml (it may hold the proxy key): no details.
    let cfg = match Config::load(paths) {
        Ok(c) => c,
        Err(_) => {
            out.push(
                Check::new(Level::Fail, "config", "config.toml could not be loaded")
                    .hint(format!("fix or move aside {}", paths.config.display())),
            );
            Config::default()
        }
    };
    out.push(service_check(paths));

    let addr = probe_addr(cfg.host, cfg.port);
    let located = client::locate(paths).await.ok();
    let daemon = match &located {
        Some(Located::Daemon(c)) => Some(c),
        _ => None,
    };
    let probe = match &located {
        Some(Located::Daemon(_)) => Probe::Pooltime,
        Some(Located::Absent { probe, .. }) => *probe,
        // Only a config error gets here; the probe itself never fails.
        None => crate::server::health::probe(addr).await,
    };
    out.push(health_check(addr, probe));

    let catalog = match daemon {
        Some(c) => get_within(c, "/v1/models")
            .await
            .ok()
            .and_then(|v| v["data"].as_array().map(|rows| live_rows(rows))),
        None => None,
    };
    let pool = if probe != Probe::Pooltime {
        PoolState::Down
    } else if !cache_is_fresh(paths, OffsetDateTime::now_utc().unix_timestamp()) {
        PoolState::CacheStale
    } else {
        PoolState::Up
    };
    out.extend(status_cache_check(pool));
    out.extend(profile_checks(
        paths,
        &cfg.profiles,
        &format!("http://{addr}"),
        pool,
        catalog.as_deref(),
    ));
    out.extend(statusline_checks(
        paths,
        &cfg.profiles,
        home_default().as_deref(),
    ));
    out.extend(mcp_checks(paths, &cfg.profiles, cfg.port));
    let warn_days = cfg.alerts.warn_days.iter().copied().max().unwrap_or(3);
    let facts = account_facts(paths, daemon).await;
    // Codex hosts matter only with an OpenAI account (unknown when the status is unreadable).
    let openai = facts
        .as_ref()
        .is_ok_and(|(f, _)| f.iter().any(|a| a.provider == "openai"));
    out.extend(account_checks(facts, warn_days, OffsetDateTime::now_utc()));
    let stats = match daemon {
        Some(c) => Some(get_stats(c).await),
        None => None,
    };
    out.extend(error_checks(stats.as_ref()));

    let (upstream, date) = upstream_checks(&cfg, openai).await;
    out.extend(upstream);
    let (version, release_date) = version_check(opts).await;
    out.push(version);
    out.push(clock_check(
        date.or(release_date),
        OffsetDateTime::now_utc(),
    ));
    out.push(notify_check(
        cfg.alerts.notify,
        &SystemNotifier::from_env(),
        cfg!(target_os = "macos"),
    ));
    out.extend(permission_checks(paths));
    let journal_rows = stats
        .as_ref()
        .and_then(|s| s.as_ref().ok())
        .and_then(|v| v["journal_requests"].as_u64());
    out.push(storage_check(paths, &cfg.retention, journal_rows));
    out.push(retention_check(&cfg.retention));
    out
}

/// `$HOME/.pooltime`, the data dir a status line wrapper without `POOLTIME_HOME=` reads.
fn home_default() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .filter(|h| !h.is_empty())
        .map(|h| Path::new(&h).join(".pooltime"))
}

fn service_check(paths: &Paths) -> Check {
    let st = Backend::native()
        .and_then(|b| Ok((b, Env::from_process(paths)?)))
        .and_then(|(b, env)| Ok((b.status(&env, &SystemRunner)?, b.supervised(&env))));
    match st {
        Ok((s, supervised)) => service_verdict(&s, supervised),
        Err(e) => Check::new(
            Level::Warn,
            "service",
            format!("could not ask the service manager: {e:#}"),
        ),
    }
}

/// `supervised`: whether the installed definition runs the supervisor (`None`: none installed).
fn service_verdict(s: &crate::service::Status, supervised: Option<bool>) -> Check {
    let label = "service";
    match (s.installed, s.running, supervised) {
        (true, running, Some(false)) => Check::new(
            Level::Warn,
            label,
            format!(
                "installed{}, but from before seamless restarts (a restart makes new requests \
                 wait for the in-flight ones)",
                if running {
                    " and running"
                } else {
                    ", not running"
                }
            ),
        )
        .hint("`pooltime service install` once (`pooltime service restart` also does it)"),
        (_, true, _) => Check::new(Level::Ok, label, "installed and running"),
        (true, false, _) => Check::new(Level::Fail, label, "installed but not running")
            .hint("`pooltime service restart`, then `pooltime logs` if it stops again"),
        (false, false, _) => Check::new(
            Level::Warn,
            label,
            "not installed (the pool runs only while `pooltime serve` does)",
        )
        .hint("`pooltime service install`"),
    }
}

fn health_check(addr: SocketAddr, probe: Probe) -> Check {
    let label = "health";
    let port_hint = "free the port, or set another `port` in config.toml";
    match probe {
        Probe::Pooltime => Check::new(Level::Ok, label, format!("pooltime answers on {addr}")),
        Probe::Free => Check::new(
            Level::Fail,
            label,
            format!("pooltime is not running on {addr}"),
        )
        .hint("start it: `pooltime service install` or `pooltime serve`"),
        Probe::Bro => Check::new(
            Level::Fail,
            label,
            format!("bro's pool answers on {addr}, not pooltime"),
        )
        .hint("stop bro (`bro pool down`) or set another `port` in config.toml"),
        Probe::Other => Check::new(
            Level::Fail,
            label,
            format!("something other than pooltime answers on {addr}"),
        )
        .hint(port_hint),
        Probe::Unresponsive => Check::new(
            Level::Fail,
            label,
            format!("something on {addr} accepts connections but does not answer"),
        )
        .hint(port_hint),
    }
}

/// The `env` of a profile's `settings.json` (empty when unreadable).
fn settings_env(settings_json: &Path) -> serde_json::Map<String, Value> {
    std::fs::read(settings_json)
        .ok()
        .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
        .and_then(|v| v.get("env")?.as_object().cloned())
        .unwrap_or_default()
}

/// Every doctor call to the pool is bounded by this.
const POOL_TIMEOUT: Duration = Duration::from_secs(5);

/// `GET path` from the pool, giving up after [`POOL_TIMEOUT`].
async fn get_within(c: &Client, path: &str) -> anyhow::Result<Value> {
    match tokio::time::timeout(POOL_TIMEOUT, c.get(path)).await {
        Ok(r) => r,
        Err(_) => anyhow::bail!("no answer within {} s", POOL_TIMEOUT.as_secs()),
    }
}

/// The rows of a `/v1/models` listing that came from Anthropic's live list. The pool does not say
/// whether its live list was available, so they are told from the model table's stand-in rows,
/// which carry only `id`, `object`, `created: 0` and `owned_by`: a live row has a `display_name`,
/// a `max_input_tokens` or a non-zero `created`. Pins are compared only against live rows, so an
/// unavailable catalog never reads as stale pins.
fn live_rows(rows: &[Value]) -> Vec<Value> {
    rows.iter()
        .filter(|r| {
            r["owned_by"] == ANTHROPIC_OWNER
                && (r.get("display_name").is_some()
                    || r.get("max_input_tokens").is_some()
                    || r["created"].as_i64().is_some_and(|c| c != 0))
        })
        .cloned()
        .collect()
}

/// Whether connected profiles can use the pool.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PoolState {
    Up,
    /// Nothing pooltime answers on the configured address (the health line says what does).
    Down,
    /// The pool answers, but the status cache is missing or stale: status lines say `pool down`.
    CacheStale,
}

/// `!` when the pool answers but its status cache is missing or stale; `None` otherwise (a pool
/// that does not answer is the health line's).
fn status_cache_check(pool: PoolState) -> Option<Check> {
    (pool == PoolState::CacheStale).then(|| {
        Check::new(
            Level::Warn,
            "status cache",
            format!(
                "missing or older than {} s (status lines show \"pool down\")",
                STALE_AFTER.as_secs()
            ),
        )
        .hint("`pooltime logs` / `pooltime service status`")
    })
}

fn profile_checks(
    paths: &Paths,
    profiles: &[ProfileRef],
    pool_url: &str,
    pool: PoolState,
    catalog: Option<&[Value]>,
) -> Vec<Check> {
    if profiles.is_empty() {
        return vec![Check::new(
            Level::Warn,
            "profiles",
            "no Claude Code profile is connected",
        )
        .hint("`pooltime connect <profile>`")];
    }
    let want = catalog
        .filter(|c| !c.is_empty())
        .map(derive_pins)
        .unwrap_or_default();
    profiles
        .iter()
        .map(|r| {
            let label = format!("profile {}", r.id);
            let p = profile_of(r);
            if !is_connected(&p, paths) {
                return Check::new(
                    Level::Warn,
                    label,
                    format!("recorded but not connected ({})", p.settings_json.display()),
                )
                .hint(format!("`pooltime connect {}`", r.id));
            }
            // A stale status cache alone is not "down": the pool answers ([`status_cache_check`]).
            if pool == PoolState::Down {
                return Check::new(Level::Fail, label, "connected, pool down").hint(
                    "Claude Code can't reach the pool: check `pooltime service status` \
                     or run `pooltime serve`, else `pooltime disconnect`",
                );
            }
            let env = settings_env(&p.settings_json);
            let url = env
                .get("ANTHROPIC_BASE_URL")
                .and_then(Value::as_str)
                .unwrap_or("");
            if url != pool_url {
                return Check::new(
                    Level::Fail,
                    label,
                    format!("points at {}, not this pool ({pool_url})", shown_url(url)),
                )
                .hint(format!("`pooltime connect {}`", r.id));
            }
            let stale: Vec<String> = [
                ("Sonnet", "SONNET", &want.sonnet),
                ("Haiku", "HAIKU", &want.haiku),
            ]
            .into_iter()
            .filter_map(|(name, key, pin)| {
                let pin = pin.as_ref()?;
                let have = env
                    .get(&format!("ANTHROPIC_DEFAULT_{key}_MODEL"))
                    .and_then(Value::as_str);
                (have != Some(pin.id.as_str()))
                    .then(|| format!("{name} {} (current {})", have.unwrap_or("unset"), pin.id))
            })
            .collect();
            if !stale.is_empty() {
                return Check::new(
                    Level::Warn,
                    label,
                    format!("pins out of date: {}", stale.join(", ")),
                )
                .hint(format!(
                    "`pooltime connect {}` (the pool also re-syncs every minute)",
                    r.id
                ));
            }
            let pins = if want.sonnet.is_some() || want.haiku.is_some() {
                "pins current"
            } else {
                "pins not compared (no live model catalog)"
            };
            Check::new(Level::Ok, label, format!("connected to {pool_url}, {pins}"))
        })
        .collect()
}

/// A JSON file (`settings.json`, `.claude.json`), parsed; `None` when missing or unreadable.
fn read_json(p: &Path) -> Option<Value> {
    std::fs::read(p)
        .ok()
        .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
}

/// Whether `a` and `b` name the same directory: canonical paths when both resolve (symlinks,
/// `..`), else the paths as written.
fn same_path(a: &Path, b: &Path) -> bool {
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(x), Ok(y)) => x == y,
        _ => a == b,
    }
}

fn is_executable(p: &Path) -> bool {
    std::fs::metadata(p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

/// One line per recorded profile whose `statusLine` is our wrapper. `default_home` is the data
/// dir a wrapper without `POOLTIME_HOME=` reads.
fn statusline_checks(
    paths: &Paths,
    profiles: &[ProfileRef],
    default_home: Option<&Path>,
) -> Vec<Check> {
    profiles
        .iter()
        .filter_map(|r| {
            let p = profile_of(r);
            let line = read_json(&p.settings_json)?.get("statusLine")?.clone();
            let parts = wrapper_parts(&line, &r.id)?;
            let label = format!("statusline {}", r.id);
            let fix = format!("`pooltime connect --statusline {}`", r.id);
            if !is_executable(&parts.bin) {
                return Some(
                    Check::new(
                        Level::Fail,
                        label,
                        format!(
                            "the wrapper runs {}, which is missing or not executable",
                            parts.bin.display()
                        ),
                    )
                    .hint(fix),
                );
            }
            match snapshot::read(&snapshot::snapshot_path(paths, &p)) {
                SnapshotState::Present(_) => {}
                _ => {
                    return Some(
                        Check::new(
                            Level::Warn,
                            label,
                            "no readable snapshot of the original status line \
                             (disconnect cannot restore it)",
                        )
                        .hint(fix),
                    )
                }
            }
            let reads = parts.home.as_deref().or(default_home);
            if !reads.is_some_and(|h| same_path(h, &paths.home)) {
                let shown = reads.map_or("an unknown data dir".to_string(), |h| {
                    h.display().to_string()
                });
                return Some(
                    Check::new(
                        Level::Warn,
                        label,
                        format!(
                            "the wrapper reads {shown}, not this data dir ({})",
                            paths.home.display()
                        ),
                    )
                    .hint(fix),
                );
            }
            Some(Check::new(Level::Ok, label, "wrapper installed"))
        })
        .collect()
}

/// One line per recorded profile whose snapshot says the MCP server is registered there. Only
/// those profiles' `.claude.json` is read.
fn mcp_checks(paths: &Paths, profiles: &[ProfileRef], port: u16) -> Vec<Check> {
    let key = std::fs::read_to_string(crate::mcp::mcp_key_path(paths))
        .ok()
        .map(|k| k.trim().to_string())
        .filter(|k| !k.is_empty());
    profiles
        .iter()
        .filter_map(|r| {
            let p = profile_of(r);
            if !mcp::is_registered(paths, &p) {
                return None;
            }
            let label = format!("mcp {}", r.id);
            let fix = format!("`pooltime connect --mcp {}`", r.id);
            let entry = read_json(&p.claude_json)
                .and_then(|v| v.get("mcpServers")?.get(mcp::MCP_SERVER_NAME).cloned());
            let Some(entry) = entry else {
                return Some(
                    Check::new(
                        Level::Warn,
                        label,
                        format!(
                            "registered, but {} has no mcpServers.{}",
                            p.claude_json.display(),
                            mcp::MCP_SERVER_NAME
                        ),
                    )
                    .hint(fix),
                );
            };
            let entry_port = entry["url"]
                .as_str()
                .and_then(|u| reqwest::Url::parse(u).ok())
                .and_then(|u| u.port_or_known_default());
            if entry_port != Some(port) {
                let shown = entry_port.map_or("no port".to_string(), |p| format!("port {p}"));
                return Some(
                    Check::new(
                        Level::Warn,
                        label,
                        format!("registered on {shown}, but the pool is on port {port}"),
                    )
                    .hint(fix),
                );
            }
            let Some(key) = &key else {
                return Some(
                    Check::new(
                        Level::Warn,
                        label,
                        "run/mcp.key is missing (the pool writes it when it starts)",
                    )
                    .hint(format!("start the pool, then {fix}")),
                );
            };
            let have = entry["headers"]["Authorization"].as_str().unwrap_or("");
            let want = format!("Bearer {key}");
            if !bool::from(have.as_bytes().ct_eq(want.as_bytes())) {
                return Some(
                    Check::new(
                        Level::Warn,
                        label,
                        "registered token does not match run/mcp.key",
                    )
                    .hint(fix),
                );
            }
            Some(Check::new(
                Level::Ok,
                label,
                format!("registered on port {port}"),
            ))
        })
        .collect()
}

/// The pool's `GET /api/v1/stats` (management key), bounded by [`POOL_TIMEOUT`].
async fn get_stats(c: &Client) -> Result<Value, String> {
    match tokio::time::timeout(POOL_TIMEOUT, c.get_authed_opt("/api/v1/stats")).await {
        Ok(Ok(Some(v))) => Ok(v),
        Ok(Ok(None)) => Err("the pool has no journal stats".into()),
        Ok(Err(e)) => Err(format!("{e:#}")),
        Err(_) => Err(format!("no answer within {} s", POOL_TIMEOUT.as_secs())),
    }
}

/// Error rates over the stats window. `None`: no pool.
fn error_checks(stats: Option<&Result<Value, String>>) -> Vec<Check> {
    let label = "errors";
    let v = match stats {
        None => {
            return vec![Check::new(
                Level::Info,
                label,
                "not checked (pool not running)",
            )]
        }
        Some(Err(e)) => {
            return vec![Check::new(
                Level::Info,
                label,
                format!("not checked (could not read the journal stats: {e})"),
            )]
        }
        Some(Ok(v)) => v,
    };
    let hours = v["window_secs"].as_u64().unwrap_or(86_400) / 3600;
    let rows = v["accounts"].as_array().cloned().unwrap_or_default();
    let mut out = Vec::new();
    let (mut attempts, mut errors) = (0u64, 0u64);
    for a in &rows {
        // Failover hand-overs and the client's invalid requests are not the account's failures
        // (`rated_*`; a pool without them: everything).
        let n = a["rated_attempts"]
            .as_u64()
            .or_else(|| a["attempts"].as_u64())
            .unwrap_or(0);
        let e = a["rated_errors"]
            .as_u64()
            .or_else(|| a["errors"].as_u64())
            .unwrap_or(0)
            .min(n);
        attempts += n;
        errors += e;
        let rate = if n == 0 { 0.0 } else { e as f64 / n as f64 };
        if n < ERROR_MIN_ATTEMPTS || rate < ERROR_RATE_WARN {
            continue;
        }
        let id = a["account"].as_str().unwrap_or("?");
        let mut detail = format!(
            "{:.0}% of attempts failed in the last {hours} h ({e} of {n}, not counting failovers \
             or invalid requests)",
            rate * 100.0
        );
        if let Some(top) = a["top_error"].as_str() {
            detail.push_str(&format!(", most common error {top}"));
        }
        if let (Some(p50), Some(p95)) = (a["p50_ms"].as_i64(), a["p95_ms"].as_i64()) {
            detail.push_str(&format!("; latency p50 {p50} ms, p95 {p95} ms"));
        }
        out.push(
            Check::new(Level::Warn, format!("{label} {id}"), detail)
                .hint("see the dashboard's request journal, or `pooltime logs`"),
        );
    }
    if out.is_empty() {
        out.push(Check::new(
            Level::Ok,
            label,
            format!(
                "{attempts} attempts in the last {hours} h, {errors} failed (not counting \
                 failovers or invalid requests); no account at {:.0}% or more",
                ERROR_RATE_WARN * 100.0
            ),
        ));
    }
    out
}

/// A release version: `major.minor.patch[-pre]` (a leading `v`/`V` and any `+build` ignored).
#[derive(Debug, Clone, PartialEq, Eq)]
struct Version {
    core: [u64; 3],
    pre: Vec<String>,
}

impl Version {
    fn parse(s: &str) -> Option<Version> {
        let s = s.trim();
        let s = s.strip_prefix(['v', 'V']).unwrap_or(s);
        let s = s.split('+').next()?;
        let (core, pre) = match s.split_once('-') {
            Some((c, p)) => (c, Some(p)),
            None => (s, None),
        };
        let nums: Vec<u64> = core
            .split('.')
            .map(|n| {
                (!n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
                    .then(|| n.parse().ok())
                    .flatten()
            })
            .collect::<Option<_>>()?;
        let [major, minor, patch] = nums[..] else {
            return None;
        };
        let pre = match pre {
            None => Vec::new(),
            Some(p) => {
                let ids: Vec<String> = p.split('.').map(str::to_string).collect();
                let ok = ids.iter().all(|i| {
                    !i.is_empty() && i.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
                });
                if !ok {
                    return None;
                }
                ids
            }
        };
        Some(Version {
            core: [major, minor, patch],
            pre,
        })
    }
}

impl Ord for Version {
    /// Semver precedence: the core numbers, then a pre-release below the release, pre-release
    /// identifiers compared numerically when both are numbers, numbers below words, and a
    /// shorter list below a longer one it starts.
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        use std::cmp::Ordering;
        self.core.cmp(&other.core).then_with(|| {
            match (self.pre.is_empty(), other.pre.is_empty()) {
                (true, true) => return Ordering::Equal,
                (true, false) => return Ordering::Greater,
                (false, true) => return Ordering::Less,
                _ => {}
            }
            for (a, b) in self.pre.iter().zip(&other.pre) {
                let o = match (a.parse::<u64>(), b.parse::<u64>()) {
                    (Ok(x), Ok(y)) => x.cmp(&y),
                    (Ok(_), Err(_)) => Ordering::Less,
                    (Err(_), Ok(_)) => Ordering::Greater,
                    (Err(_), Err(_)) => a.cmp(b),
                };
                if o != Ordering::Equal {
                    return o;
                }
            }
            self.pre.len().cmp(&other.pre.len())
        })
    }
}

impl PartialOrd for Version {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let [a, b, c] = self.core;
        write!(f, "{a}.{b}.{c}")?;
        if !self.pre.is_empty() {
            write!(f, "-{}", self.pre.join("."))?;
        }
        Ok(())
    }
}

/// The check for the latest release `tag` against this build's `current` version.
fn version_verdict(current: &str, tag: Option<&str>) -> Check {
    let label = "version";
    let (Some(cur), Some(latest)) = (Version::parse(current), tag.and_then(Version::parse)) else {
        return Check::new(
            Level::Info,
            label,
            "not checked (the latest release has no version tag)",
        );
    };
    if latest > cur {
        Check::new(
            Level::Warn,
            label,
            format!("{latest} available (brew upgrade pooltime / re-run install)"),
        )
    } else {
        Check::new(Level::Ok, label, format!("up to date ({cur})"))
    }
}

/// Ask for the latest release (unless opted out); returns the check and the answer's `Date`
/// (with the origin it came from), for the clock check.
async fn version_check(opts: &Options) -> (Check, Option<(String, OffsetDateTime)>) {
    let label = "version";
    if !opts.version_check {
        return (Check::new(Level::Info, label, "release check off"), None);
    }
    let offline = |why: String| Check::new(Level::Info, label, format!("not checked ({why})"));
    let http = match reqwest::Client::builder()
        .connect_timeout(RELEASE_TIMEOUT)
        .timeout(RELEASE_TIMEOUT)
        .build()
    {
        Ok(c) => c,
        Err(e) => return (offline(format!("no HTTP client: {e}")), None),
    };
    let resp = http
        .get(&opts.releases_api)
        .header(http::header::ACCEPT, "application/vnd.github+json")
        .header(
            http::header::USER_AGENT,
            concat!("pooltime/", env!("CARGO_PKG_VERSION")),
        )
        .send()
        .await;
    let resp = match resp {
        Ok(r) => r,
        Err(e) if e.is_timeout() => {
            return (
                offline(format!("no answer within {} s", RELEASE_TIMEOUT.as_secs())),
                None,
            )
        }
        Err(_) => return (offline("release server unreachable".into()), None),
    };
    let date = resp
        .headers()
        .get(http::header::DATE)
        .and_then(|v| v.to_str().ok())
        .and_then(parse_http_date)
        .map(|d| {
            let from = origin(&opts.releases_api).unwrap_or_else(|| "the release server".into());
            (from, d)
        });
    let status = resp.status();
    if !status.is_success() {
        return (
            offline(format!("release server answered HTTP {}", status.as_u16())),
            date,
        );
    }
    let tag = resp
        .bytes()
        .await
        .ok()
        .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
        .and_then(|v| v["tag_name"].as_str().map(str::to_string));
    (
        version_verdict(env!("CARGO_PKG_VERSION"), tag.as_deref()),
        date,
    )
}

/// Which desktop notifier alerts would use.
fn notify_check(enabled: bool, notifier: &SystemNotifier, macos: bool) -> Check {
    let label = "notifications";
    if !enabled {
        return Check::new(Level::Info, label, "off (alerts.notify = false)");
    }
    let probe = Notification {
        message: String::new(),
        action: String::new(),
        url: String::new(),
    };
    let backend = notifier
        .command_for(&probe)
        .and_then(|(prog, _)| prog.file_name().map(|f| f.to_string_lossy().into_owned()));
    match (backend.as_deref(), macos) {
        (Some("osascript"), true) => Check::new(Level::Ok, label, "osascript")
            .hint("brew install terminal-notifier for clickable notifications"),
        (Some(b), _) => Check::new(Level::Ok, label, b.to_string()),
        (None, true) => Check::new(
            Level::Warn,
            label,
            "no notifier found (alerts are only logged)",
        )
        .hint("brew install terminal-notifier"),
        (None, false) => Check::new(
            Level::Warn,
            label,
            "notify-send not found (alerts are only logged)",
        )
        .hint("install libnotify (notify-send)"),
    }
}

/// What the account checks need to know about one account.
struct AccountFacts {
    id: String,
    /// `anthropic` / `openai`.
    provider: String,
    status: String,
    /// The pool's classified reason for a non-ready status (live status only).
    reason: Option<String>,
    refresh_expires_at: Option<OffsetDateTime>,
}

fn parse_ts(v: &Value) -> Option<OffsetDateTime> {
    OffsetDateTime::parse(v.as_str()?, &time::format_description::well_known::Rfc3339).ok()
}

/// The accounts, and whether they are the running pool's view. `Err` (the reason) when a pool
/// answers but its status cannot be read: the files may be stale while it runs, so there is no
/// fallback to them then.
type Facts = Result<(Vec<AccountFacts>, bool), String>;

/// From the running pool's status, else (no pool; read-only) from the account files.
async fn account_facts(paths: &Paths, daemon: Option<&Client>) -> Facts {
    if let Some(c) = daemon {
        let v = get_within(c, "/api/v1/status")
            .await
            .map_err(|e| format!("{e:#}"))?;
        let facts = v["accounts"]
            .as_array()
            .map(|a| {
                a.iter()
                    .map(|a| AccountFacts {
                        id: a["id"].as_str().unwrap_or("?").to_string(),
                        provider: a["provider"].as_str().unwrap_or("?").to_string(),
                        status: a["status"].as_str().unwrap_or("?").to_string(),
                        reason: a["reason"].as_str().map(str::to_string),
                        refresh_expires_at: parse_ts(&a["refresh_expires_at"]),
                    })
                    .collect()
            })
            .unwrap_or_default();
        return Ok((facts, true));
    }
    // `Registry::load` creates the accounts dir: only open it when it exists.
    if !paths.accounts.is_dir() {
        return Ok((Vec::new(), false));
    }
    let Ok(reg) = Registry::load(paths) else {
        return Ok((Vec::new(), false));
    };
    let facts = reg
        .list()
        .into_iter()
        .map(|m| {
            let (status, refresh_expires_at) = match reg.read_creds(&m.id) {
                Ok(Some(c)) => ("signed in".to_string(), c.refresh_expires_at),
                Ok(None) => ("needs_login".to_string(), None),
                Err(_) => ("credentials file is corrupt".to_string(), None),
            };
            AccountFacts {
                id: m.id.as_str().to_string(),
                provider: m.provider.as_str().to_string(),
                status,
                reason: None,
                refresh_expires_at,
            }
        })
        .collect();
    Ok((facts, false))
}

fn account_checks(facts: Facts, warn_days: u32, now: OffsetDateTime) -> Vec<Check> {
    let (facts, live) = match facts {
        Ok(f) => f,
        Err(e) => {
            return vec![Check::new(
                Level::Fail,
                "accounts",
                format!("could not read pool status: {e}"),
            )
            .hint("see `pooltime logs`; restart it with `pooltime service restart`")]
        }
    };
    if facts.is_empty() {
        return vec![Check::new(Level::Fail, "accounts", "no accounts")
            .hint("`pooltime accounts login <name>` (or `pooltime import bro`)")];
    }
    facts
        .into_iter()
        .map(|a| account_check(a, live, warn_days, now))
        .collect()
}

fn account_check(a: AccountFacts, live: bool, warn_days: u32, now: OffsetDateTime) -> Check {
    let label = format!("account {}", a.id);
    let login = format!("`pooltime accounts login {}`", a.id);
    let source = if live {
        ""
    } else {
        " (pool not running: from the files)"
    };
    match a.status.as_str() {
        "needs_login" if a.reason.as_deref() == Some("state_unreadable") => {
            return Check::new(Level::Fail, label, format!("needs login{source}")).hint(format!(
                "account state was unreadable; re-verify it with `pooltime accounts recheck {}`",
                a.id
            ))
        }
        "needs_login" => {
            return Check::new(Level::Fail, label, format!("needs login{source}")).hint(login)
        }
        "credentials file is corrupt" => {
            return Check::new(Level::Fail, label, a.status).hint(login)
        }
        _ => {}
    }
    if let Some(exp) = a.refresh_expires_at {
        let left = exp - now;
        // A passed clock alone never sidelines (only a refused probe refresh makes an account
        // needs_login, handled above), so a passed expiry on a working account is a warning.
        if left <= time::Duration::ZERO {
            return Check::new(
                Level::Warn,
                label,
                "recorded login expiry passed but the account still works; re-login to refresh it",
            )
            .hint(login);
        }
        if left <= time::Duration::days(i64::from(warn_days)) {
            let when = if left >= time::Duration::days(1) {
                format!("{} days", left.whole_days())
            } else {
                format!("{} hours", left.whole_hours().max(1))
            };
            return Check::new(Level::Warn, label, format!("login expires in {when}")).hint(login);
        }
    }
    let level = match a.status.as_str() {
        "billing" | "access_denied" | "erroring" => Level::Warn,
        _ => Level::Ok,
    };
    Check::new(level, label, format!("{}{source}", a.status))
}

/// `scheme://host[:port]` of a URL.
fn origin(url: &str) -> Option<String> {
    let u = reqwest::Url::parse(url).ok()?;
    let host = u.host_str()?;
    Some(match u.port() {
        Some(p) => format!("{}://{host}:{p}", u.scheme()),
        None => format!("{}://{host}", u.scheme()),
    })
}

/// What to show of a URL found in a settings file: its origin only (never userinfo, a path or a
/// query).
fn shown_url(url: &str) -> String {
    origin(url).unwrap_or_else(|| "an unparsable URL".into())
}

/// `HEAD` each Anthropic origin (and, with `openai`, each OpenAI/Codex origin) once; returns the
/// checks and the first `Date` header seen.
async fn upstream_checks(
    cfg: &Config,
    openai: bool,
) -> (Vec<Check>, Option<(String, OffsetDateTime)>) {
    let mut urls = vec![&cfg.anthropic.api_base, &cfg.anthropic.token_url];
    if openai {
        urls.extend([&cfg.openai.api_base, &cfg.openai.token_url]);
    }
    let mut origins: Vec<String> = Vec::new();
    for u in urls {
        if let Some(o) = origin(u) {
            if !origins.contains(&o) {
                origins.push(o);
            }
        }
    }
    let http = match reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(UPSTREAM_TIMEOUT)
        .timeout(UPSTREAM_TIMEOUT)
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            return (
                vec![Check::new(
                    Level::Fail,
                    "upstream",
                    format!("no HTTP client: {e}"),
                )],
                None,
            )
        }
    };
    let mut checks = Vec::new();
    let mut date = None;
    for o in origins {
        let label = format!("upstream {o}");
        match http.head(format!("{o}/")).send().await {
            Ok(resp) => {
                if date.is_none() {
                    date = resp
                        .headers()
                        .get(http::header::DATE)
                        .and_then(|v| v.to_str().ok())
                        .and_then(parse_http_date)
                        .map(|d| (o.clone(), d));
                }
                checks.push(Check::new(
                    Level::Ok,
                    label,
                    format!("reachable (HTTP {})", resp.status().as_u16()),
                ));
            }
            Err(e) if e.is_timeout() => checks.push(
                Check::new(
                    Level::Warn,
                    label,
                    format!("no answer within {} s", UPSTREAM_TIMEOUT.as_secs()),
                )
                .hint("slow or offline network?"),
            ),
            Err(e) => checks.push(
                Check::new(
                    Level::Fail,
                    label,
                    format!(
                        "unreachable (DNS, connect or TLS failure: {})",
                        root_cause(&e)
                    ),
                )
                .hint("check the network, proxy or firewall"),
            ),
        }
    }
    (checks, date)
}

/// The innermost error message (reqwest wraps the interesting part).
fn root_cause(e: &(dyn std::error::Error + 'static)) -> String {
    let mut cur = e;
    while let Some(next) = cur.source() {
        cur = next;
    }
    cur.to_string()
}

/// An IMF-fixdate (`Sun, 06 Nov 1994 08:49:37 GMT`), the form servers send.
fn parse_http_date(s: &str) -> Option<OffsetDateTime> {
    let fmt = time::macros::format_description!(
        "[weekday repr:short], [day] [month repr:short] [year] [hour]:[minute]:[second] GMT"
    );
    time::PrimitiveDateTime::parse(s.trim(), fmt)
        .ok()
        .map(|t| t.assume_utc())
}

fn clock_check(date: Option<(String, OffsetDateTime)>, now: OffsetDateTime) -> Check {
    let label = "clock";
    let Some((from, server)) = date else {
        return Check::new(
            Level::Info,
            label,
            "not checked (no server answered with a Date header)",
        );
    };
    let skew = (now - server).whole_seconds().abs();
    let detail = format!("{skew} s off {from}");
    let hint = "sync the system clock (sign-in and token expiry depend on it)";
    if skew > SKEW_FAIL {
        Check::new(Level::Fail, label, detail).hint(hint)
    } else if skew > SKEW_WARN {
        Check::new(Level::Warn, label, detail).hint(hint)
    } else {
        Check::new(Level::Ok, label, format!("within {skew} s of {from}"))
    }
}

/// Group/other bits set on an existing path: `Some(mode)`.
fn too_open(p: &Path) -> Option<u32> {
    let mode = std::fs::metadata(p).ok()?.permissions().mode() & 0o777;
    (mode & 0o077 != 0).then_some(mode)
}

/// The data dir's directories and secret files.
fn private_paths(paths: &Paths) -> (Vec<PathBuf>, Vec<PathBuf>) {
    let mut dirs = vec![
        paths.home.clone(),
        paths.accounts.clone(),
        paths.logs.clone(),
        paths.run.clone(),
    ];
    let mut files = vec![
        paths.config.clone(),
        paths.db.clone(),
        sibling(&paths.db, "-wal"),
        sibling(&paths.db, "-shm"),
        crate::server::mgmt::mgmt_key_path(paths),
        crate::mcp::mcp_key_path(paths),
    ];
    // Profile snapshots may hold the user's own auth token.
    let snaps = paths.run.join("profiles");
    if let Ok(rd) = std::fs::read_dir(&snaps) {
        let mut found: Vec<PathBuf> = rd
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|x| x == "json"))
            .collect();
        found.sort();
        files.extend(found);
        dirs.push(snaps);
    }
    if let Ok(rd) = std::fs::read_dir(&paths.accounts) {
        let mut accounts: Vec<PathBuf> = rd
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.is_dir())
            .collect();
        accounts.sort();
        for a in accounts {
            files.push(a.join("account.toml"));
            files.push(a.join("credentials.json"));
            dirs.push(a);
        }
    }
    (dirs, files)
}

fn sibling(p: &Path, suffix: &str) -> PathBuf {
    let mut s = p.as_os_str().to_owned();
    s.push(suffix);
    PathBuf::from(s)
}

fn permission_checks(paths: &Paths) -> Vec<Check> {
    let label = "permissions";
    if !paths.home.is_dir() {
        return vec![Check::new(
            Level::Warn,
            label,
            format!("{} does not exist yet", paths.home.display()),
        )
        .hint("`pooltime setup`")];
    }
    let (dirs, files) = private_paths(paths);
    let mut out = Vec::new();
    for (list, want) in [(dirs, "700"), (files, "600")] {
        for p in list {
            if let Some(mode) = too_open(&p) {
                out.push(
                    Check::new(Level::Fail, label, format!("{} is {mode:04o}", p.display()))
                        .hint(format!("chmod {want} {}", p.display())),
                );
            }
        }
    }
    if out.is_empty() {
        out.push(Check::new(
            Level::Ok,
            label,
            "data dir 0700; credentials, database and config 0600",
        ));
    }
    out
}

fn dir_size(p: &Path) -> u64 {
    let Ok(rd) = std::fs::read_dir(p) else {
        return 0;
    };
    rd.flatten()
        .map(|e| match e.file_type() {
            Ok(t) if t.is_dir() => dir_size(&e.path()),
            Ok(t) if t.is_file() => e.metadata().map(|m| m.len()).unwrap_or(0),
            _ => 0,
        })
        .sum()
}

fn mib(n: u64) -> String {
    format!("{:.1} MiB", n as f64 / (1024.0 * 1024.0))
}

/// Database (with its WAL/SHM files) and logs against the retention caps; `journal_rows` is the
/// pool's journal size, when it answered.
fn storage_check(paths: &Paths, ret: &RetentionCfg, journal_rows: Option<u64>) -> Check {
    let label = "storage";
    let db: u64 = [
        paths.db.clone(),
        sibling(&paths.db, "-wal"),
        sibling(&paths.db, "-shm"),
    ]
    .iter()
    .filter_map(|p| std::fs::metadata(p).ok())
    .map(|m| m.len())
    .sum();
    let logs = dir_size(&paths.logs);
    let cap = u64::from(ret.journal_max_mb) * 1024 * 1024;
    let mut detail = format!("{} in all: database {}", mib(db + logs), mib(db));
    if cap > 0 {
        detail.push_str(&format!(" of {} cap", mib(cap)));
    }
    detail.push_str(&format!(", logs {}", mib(logs)));
    if let Some(n) = journal_rows {
        match ret.journal_max_requests {
            0 => detail.push_str(&format!(", journal {n} requests")),
            max => detail.push_str(&format!(", journal {n} of {max} requests")),
        }
    }
    // At the cap is the steady state (the pool prunes the oldest rows); well past it means the
    // pruning is not keeping up. With no cap, only a large total is worth a look.
    let over = if cap > 0 {
        db as f64 > cap as f64 * DB_OVER_CAP
    } else {
        db + logs > DISK_WARN
    };
    if over {
        Check::new(Level::Warn, label, detail).hint(
            "the pool prunes the oldest journal rows past the cap hourly; if this persists see \
             `pooltime logs`, or delete old files in logs/",
        )
    } else {
        Check::new(Level::Ok, label, detail)
    }
}

/// The retention settings, for reference.
fn retention_check(ret: &RetentionCfg) -> Check {
    let rows = match ret.journal_max_requests {
        0 => String::new(),
        n => format!(" / {n} requests"),
    };
    Check::new(
        Level::Info,
        "retention",
        format!(
            "logs {} d / {} MiB; journal {} d / {} MiB{rows}; usage history hourly {} d, \
             daily {} d, weekly {} d",
            ret.logs_days,
            ret.logs_max_mb,
            ret.journal_days,
            ret.journal_max_mb,
            ret.history_hourly_days,
            ret.history_daily_days,
            ret.history_weekly_days
        ),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn http_date_parses() {
        let d = parse_http_date("Sun, 06 Nov 1994 08:49:37 GMT").unwrap();
        assert_eq!(d.unix_timestamp(), 784_111_777);
        assert!(parse_http_date("yesterday").is_none());
    }

    #[test]
    fn clock_skew_levels() {
        let now = OffsetDateTime::now_utc();
        let at = |s: i64| Some(("o".to_string(), now - time::Duration::seconds(s)));
        assert_eq!(clock_check(at(5), now).level, Level::Ok);
        assert_eq!(clock_check(at(-90), now).level, Level::Warn);
        assert_eq!(clock_check(at(600), now).level, Level::Fail);
        assert_eq!(clock_check(None, now).level, Level::Info);
    }

    #[test]
    fn account_levels() {
        let now = OffsetDateTime::now_utc();
        let facts = |status: &str, days: Option<i64>| AccountFacts {
            id: "a".into(),
            provider: "anthropic".into(),
            status: status.into(),
            reason: None,
            refresh_expires_at: days.map(|d| now + time::Duration::days(d)),
        };
        let lvl = |f| account_check(f, true, 3, now).level;
        assert_eq!(lvl(facts("ready", None)), Level::Ok);
        assert_eq!(lvl(facts("ready", Some(30))), Level::Ok);
        assert_eq!(lvl(facts("ready", Some(2))), Level::Warn);
        // A passed recorded expiry on a working account: the pool still uses it.
        assert_eq!(lvl(facts("ready", Some(-1))), Level::Warn);
        assert_eq!(lvl(facts("needs_login", None)), Level::Fail);
        assert_eq!(lvl(facts("billing", None)), Level::Warn);
        assert_eq!(lvl(facts("rate_limited", None)), Level::Ok);
    }

    #[test]
    fn unreadable_state_hints_at_recheck_not_login() {
        let now = OffsetDateTime::now_utc();
        let facts = |reason: Option<&str>| AccountFacts {
            id: "a".into(),
            provider: "anthropic".into(),
            status: "needs_login".into(),
            reason: reason.map(Into::into),
            refresh_expires_at: None,
        };
        let c = account_check(facts(Some("state_unreadable")), true, 3, now);
        assert_eq!(c.level, Level::Fail);
        let line = c.to_string();
        assert!(line.starts_with("✗ account a: "), "{line}");
        assert!(
            line.contains(
                "account state was unreadable; re-verify it with `pooltime accounts recheck a`"
            ),
            "{line}"
        );
        assert!(!line.contains("accounts login"), "{line}");
        // Any other reason keeps the login hint.
        let line = account_check(facts(Some("refresh_token_dead")), true, 3, now).to_string();
        assert!(line.contains("`pooltime accounts login a`"), "{line}");
    }

    #[test]
    fn origins() {
        assert_eq!(
            origin("https://platform.claude.com/v1/oauth/token").as_deref(),
            Some("https://platform.claude.com")
        );
        assert_eq!(
            origin("http://127.0.0.1:9/x").as_deref(),
            Some("http://127.0.0.1:9")
        );
    }

    #[test]
    fn pins_compare_only_against_live_rows() {
        let table = serde_json::json!([
            {"id": "claude-sonnet-4-5", "object": "model", "created": 0, "owned_by": ANTHROPIC_OWNER},
            {"id": "gpt-5", "object": "model", "created": 0, "owned_by": "openai-chatgpt-pool"},
        ]);
        assert!(live_rows(table.as_array().unwrap()).is_empty());
        let live = serde_json::json!([
            {"id": "claude-sonnet-9", "object": "model", "created": 0, "owned_by": ANTHROPIC_OWNER,
             "display_name": "Claude Sonnet 9"},
        ]);
        assert_eq!(live_rows(live.as_array().unwrap()).len(), 1);
    }

    #[test]
    fn foreign_base_url_shows_only_its_origin() {
        assert_eq!(
            shown_url("https://user:pw@proxy.example:8443/path?key=s3cret"),
            "https://proxy.example:8443"
        );
        assert_eq!(shown_url("not a url"), "an unparsable URL");
    }

    #[test]
    fn semver_ordering() {
        let v = |s: &str| Version::parse(s).unwrap();
        assert!(v("v0.2.0") > v("0.1.9"));
        assert!(v("0.10.0") > v("0.9.0"), "numeric, not lexical");
        assert!(
            v("1.0.0") > v("1.0.0-rc.1"),
            "release above its pre-release"
        );
        assert!(v("1.0.0-rc.10") > v("1.0.0-rc.2"));
        assert!(v("1.0.0-beta") > v("1.0.0-alpha.1"));
        assert!(v("1.0.0-alpha.1") > v("1.0.0-alpha"));
        assert_eq!(v("v1.2.3+build.5"), v("1.2.3"));
        assert_eq!(v("V1.2.3"), v("1.2.3"));
        assert_eq!(v("v1.2.3-rc.1").to_string(), "1.2.3-rc.1");
        for bad in ["", "1.2", "1.2.3.4", "x.y.z", "1.2.3-", "1..3", "1.2.3-a$b"] {
            assert!(Version::parse(bad).is_none(), "{bad}");
        }
    }

    #[test]
    fn version_verdicts() {
        let c = version_verdict("0.1.0", Some("v0.2.0"));
        assert_eq!(c.level, Level::Warn);
        assert_eq!(
            c.to_string(),
            "! version: 0.2.0 available (brew upgrade pooltime / re-run install)"
        );
        assert_eq!(version_verdict("0.1.0", Some("v0.1.0")).level, Level::Ok);
        assert_eq!(version_verdict("0.2.0", Some("v0.1.0")).level, Level::Ok);
        assert_eq!(version_verdict("0.1.0", Some("nightly")).level, Level::Info);
        assert_eq!(version_verdict("0.1.0", None).level, Level::Info);
    }

    fn exe(dir: &Path, name: &str) {
        let p = dir.join(name);
        std::fs::write(&p, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    #[test]
    fn notifier_backends() {
        let d = tempfile::tempdir().unwrap();
        let n = || SystemNotifier::new(d.path().as_os_str(), true);
        assert_eq!(notify_check(false, &n(), true).level, Level::Info);
        assert_eq!(notify_check(true, &n(), true).level, Level::Warn);
        exe(d.path(), "osascript");
        let c = notify_check(true, &n(), true);
        assert_eq!(c.level, Level::Ok);
        assert!(c.to_string().contains("osascript"), "{c}");
        assert_eq!(
            c.hint.as_deref(),
            Some("brew install terminal-notifier for clickable notifications")
        );
        exe(d.path(), "terminal-notifier");
        let c = notify_check(true, &n(), true);
        assert_eq!(
            (c.level, c.detail.as_str(), c.hint),
            (Level::Ok, "terminal-notifier", None)
        );
        // Linux: notify-send or a warning.
        let linux = SystemNotifier::new(d.path().as_os_str(), false);
        assert_eq!(notify_check(true, &linux, false).level, Level::Warn);
        exe(d.path(), "notify-send");
        let linux = SystemNotifier::new(d.path().as_os_str(), false);
        assert_eq!(notify_check(true, &linux, false).detail, "notify-send");
    }

    /// A data dir and a connected profile `t` (snapshot plus base URL) under a temp dir.
    fn connected() -> (tempfile::TempDir, Paths, ProfileRef) {
        let d = tempfile::tempdir().unwrap();
        let paths = Paths::under(&d.path().join("pt"));
        let dir = d.path().join(".claude-t");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("settings.json"), "{}").unwrap();
        let r = ProfileRef {
            id: "t".into(),
            settings_dir: dir,
        };
        let block = crate::claude_code::EnvBlock {
            base_url: "http://127.0.0.1:3460".into(),
            auth_token: "k".into(),
            pins: Default::default(),
            max_context: None,
        };
        crate::claude_code::apply(&profile_of(&r), &block, &paths).unwrap();
        (d, paths, r)
    }

    #[test]
    fn stale_status_cache_is_pool_down() {
        let (_d, paths, r) = connected();
        let url = "http://127.0.0.1:3460";
        let one = |pool| profile_checks(&paths, std::slice::from_ref(&r), url, pool, None);
        assert_eq!(one(PoolState::Up)[0].level, Level::Ok);
        assert!(status_cache_check(PoolState::Up).is_none());
        // Nothing answers: the connected profile is broken.
        let c = &one(PoolState::Down)[0];
        assert_eq!(c.level, Level::Fail);
        assert!(c.detail.starts_with("connected, pool down"), "{c}");
        assert!(c.to_string().contains("pooltime service status"), "{c}");
        assert!(
            status_cache_check(PoolState::Down).is_none(),
            "the health line's"
        );
        // The pool answers but its cache is stale: one warning, the profile still verified.
        let mut checks = one(PoolState::CacheStale);
        assert_eq!(checks[0].level, Level::Ok, "{}", checks[0]);
        assert!(!checks[0].to_string().contains("pool down"));
        let cache = status_cache_check(PoolState::CacheStale).unwrap();
        assert_eq!(cache.level, Level::Warn);
        assert_eq!(
            cache.to_string(),
            "! status cache: missing or older than 150 s (status lines show \"pool down\") — \
             `pooltime logs` / `pooltime service status`"
        );
        checks.push(cache);
        assert_eq!(exit_code(&checks), 0);
    }

    #[test]
    fn statusline_wrapper_health() {
        let (d, paths, r) = connected();
        let set = |cmd: &str| {
            let v = serde_json::json!({"statusLine": {"type": "command", "command": cmd}});
            std::fs::write(r.settings_dir.join("settings.json"), v.to_string()).unwrap();
        };
        let check = || statusline_checks(&paths, std::slice::from_ref(&r), None);
        // Not our wrapper: no line.
        set("ccline");
        assert!(check().is_empty());

        let bin = d.path().join("pooltime");
        let ours = |bin: &Path, home: Option<&Path>| {
            crate::claude_code::statusline::wrapper_command(bin, "t", home)
        };
        set(&ours(&bin, Some(&paths.home)));
        let c = &check()[0];
        assert_eq!(c.level, Level::Fail, "{c}");
        assert_eq!(c.hint.as_deref(), Some("`pooltime connect --statusline t`"));

        exe(d.path(), "pooltime");
        assert_eq!(check()[0].level, Level::Ok, "{}", check()[0]);
        // Another data dir: the status line shows that pool's state, not this one's.
        set(&ours(&bin, Some(Path::new("/elsewhere"))));
        assert_eq!(check()[0].level, Level::Warn);
        // The same dir by another name (a symlink) is this one.
        let link = d.path().join("pt-link");
        std::os::unix::fs::symlink(&paths.home, &link).unwrap();
        set(&ours(&bin, Some(&link)));
        assert_eq!(check()[0].level, Level::Ok, "{}", check()[0]);
        // No prefix: reads the default dir, which here is not this one.
        set(&ours(&bin, None));
        assert_eq!(check()[0].level, Level::Warn);
        let c = &statusline_checks(&paths, std::slice::from_ref(&r), Some(&paths.home))[0];
        assert_eq!(c.level, Level::Ok);
        // The snapshot holding the original is gone.
        std::fs::remove_file(snapshot::snapshot_path(&paths, &profile_of(&r))).unwrap();
        let c = &statusline_checks(&paths, std::slice::from_ref(&r), Some(&paths.home))[0];
        assert_eq!(c.level, Level::Warn);
    }

    #[test]
    fn mcp_registration_health_never_shows_the_token() {
        let (_d, paths, r) = connected();
        let p = profile_of(&r);
        let key = "f".repeat(64);
        std::fs::write(crate::mcp::mcp_key_path(&paths), format!("{key}\n")).unwrap();
        let check = |port| mcp_checks(&paths, std::slice::from_ref(&r), port);
        // Not registered: nothing read, no line.
        assert!(check(3460).is_empty());

        mcp::register(&paths, &p, "http://127.0.0.1:3460/mcp", &key).unwrap();
        assert_eq!(check(3460)[0].level, Level::Ok);
        let c = &check(4000)[0];
        assert_eq!(c.level, Level::Warn);
        assert!(
            c.detail.contains("port 3460") && c.detail.contains("port 4000"),
            "{c}"
        );
        assert_eq!(c.hint.as_deref(), Some("`pooltime connect --mcp t`"));

        let stale = "0".repeat(64);
        mcp::register(&paths, &p, "http://127.0.0.1:3460/mcp", &stale).unwrap();
        let c = &check(3460)[0];
        assert_eq!(c.level, Level::Warn);
        let line = c.to_string();
        assert!(!line.contains(&stale) && !line.contains(&key), "{line}");

        // The entry removed behind the snapshot's back.
        std::fs::write(&p.claude_json, "{}").unwrap();
        assert_eq!(check(3460)[0].level, Level::Warn);
    }

    #[test]
    fn error_rate_thresholds() {
        let stats = |attempts: u64, errors: u64| {
            Ok(serde_json::json!({"window_secs": 86400, "accounts": [{
                "account": "a", "attempts": attempts, "errors": errors,
                "error_rate": errors as f64 / attempts as f64,
            }]}))
        };
        let one = |s| error_checks(Some(&s));
        assert_eq!(one(stats(20, 2))[0].level, Level::Warn, "10% of 20");
        assert_eq!(one(stats(19, 19))[0].level, Level::Ok, "too few attempts");
        let ok = &one(stats(100, 9))[0];
        assert_eq!((ok.level, ok.label.as_str()), (Level::Ok, "errors"));
        assert_eq!(error_checks(None)[0].level, Level::Info);
        assert_eq!(one(Err("boom".into()))[0].level, Level::Info);
        // Failovers and invalid requests (excluded from `rated_*`) do not count.
        let rated = Ok(serde_json::json!({"accounts": [{
            "account": "a", "attempts": 100, "errors": 60, "error_rate": 0.6,
            "rated_attempts": 50, "rated_errors": 2,
        }]}));
        let c = &one(rated)[0];
        assert_eq!(c.level, Level::Ok, "{c}");
        assert!(
            c.detail
                .contains("not counting failovers or invalid requests"),
            "{c}"
        );
        let rated = Ok(serde_json::json!({"accounts": [{
            "account": "a", "attempts": 100, "errors": 60, "rated_attempts": 40,
            "rated_errors": 10, "top_error": "quota",
        }]}));
        let c = &one(rated)[0];
        assert_eq!(c.level, Level::Warn, "{c}");
        assert!(c.detail.contains("(10 of 40, not counting"), "{c}");
        assert!(c.detail.contains("most common error quota"), "{c}");
    }

    #[test]
    fn storage_against_the_cap() {
        let d = tempfile::tempdir().unwrap();
        let paths = Paths::under(d.path());
        let ret = |mb| RetentionCfg {
            journal_max_mb: mb,
            ..RetentionCfg::default()
        };
        // At (even a little past) the cap is the pruned steady state.
        std::fs::write(&paths.db, vec![0u8; 1100 * 1024]).unwrap();
        let c = storage_check(&paths, &ret(1), None);
        assert_eq!(c.level, Level::Ok, "{c}");
        assert!(c.detail.contains("of 1.0 MiB cap"), "{c}");
        // Well past it: pruning is not keeping up.
        std::fs::write(&paths.db, vec![0u8; 1300 * 1024]).unwrap();
        assert_eq!(storage_check(&paths, &ret(1), None).level, Level::Warn);
        assert_eq!(storage_check(&paths, &ret(2), None).level, Level::Ok);
        assert_eq!(
            storage_check(&paths, &ret(0), None).level,
            Level::Ok,
            "no cap"
        );
        let c = storage_check(&paths, &ret(2), Some(7));
        assert!(c.detail.contains("journal 7 of 100000 requests"), "{c}");
    }

    #[test]
    fn service_from_before_seamless_restarts_is_a_warning() {
        let st = |installed, running| crate::service::Status {
            installed,
            running,
            detail: String::new(),
        };
        let old = service_verdict(&st(true, true), Some(false));
        assert_eq!(old.level, Level::Warn);
        assert!(
            old.to_string().contains("pooltime service install"),
            "{old}"
        );
        assert_eq!(
            service_verdict(&st(true, true), Some(true)).level,
            Level::Ok
        );
        assert_eq!(
            service_verdict(&st(true, false), Some(true)).level,
            Level::Fail
        );
        assert_eq!(service_verdict(&st(false, false), None).level, Level::Warn);
    }

    #[test]
    fn line_format() {
        let c = Check::new(Level::Fail, "permissions", "/x is 0755").hint("chmod 700 /x");
        assert_eq!(c.to_string(), "✗ permissions: /x is 0755 — chmod 700 /x");
    }
}
