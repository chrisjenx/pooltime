//! `pooltime doctor`: one line per check, `✓` (fine), `!` (worth a look) or `✗` (broken), each
//! problem with the command that fixes it. Exit 1 when any check is `✗`.
//!
//! Checks, in order:
//! - **service**: launchd/systemd definition installed and running (`!` when not installed: the
//!   pool then runs only while `pooltime serve` does; `✗` when installed but not running).
//! - **health**: what answers `/health` on the configured address (pooltime `✓`; bro, another
//!   server, a hung listener or nothing `✗`).
//! - **profile <id>**, per profile recorded in `config.toml`: still connected, pointing at this
//!   pool, pins current. Connected while the pool is down is `✗` ("connected, pool down"):
//!   Claude Code has no fallback for an unreachable base URL. Pins are compared with the pins
//!   derived from the live Anthropic rows of the pool's `/v1/models` listing (see [`live_rows`]);
//!   with no live rows they are not compared. Stale pins are `!` (the pool re-syncs connected
//!   profiles every minute). A foreign base URL is shown as its origin only.
//! - **account <id>**: the running pool's status (unreadable status: `✗`, no fallback), else,
//!   with no pool, the files. `needs_login` / corrupt credentials `✗`; a recorded login expiry
//!   that passed (the pool never acts on it) or falls within the largest `alerts.warn_days` `!`.
//! - **upstream <origin>**: `HEAD` to the Anthropic API and token origins, plus the OpenAI/Codex
//!   API and token origins when an OpenAI account exists (all from config, so tests point them at
//!   local fakes). Any HTTP answer is reachable; a timeout is `!` (slow or offline network); a
//!   DNS, connect or TLS failure is `✗`.
//!
//! Every call to the pool is bounded by 5 s, as is each upstream `HEAD`.
//! - **clock**: skew against the `Date` header of the first upstream answer (`!` over 60 s, `✗`
//!   over 5 min: sign-in and token expiry depend on it).
//! - **permissions**: data dir and its subdirs without group/other access (0700), config,
//!   database, management key and account files likewise (0600); each offender is a `✗` line with
//!   its `chmod`.
//! - **disk**: database plus logs; `!` above 1 GiB.
//!
//! The doctor only reads: it never creates a file, and it never prints a credential.

use std::fmt;
use std::net::SocketAddr;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::Value;
use time::OffsetDateTime;

use super::client::{self, Client, Located};
use crate::accounts::Registry;
use crate::claude_code::{derive_pins, is_connected, profile_of};
use crate::config::{Config, Paths, ProfileRef};
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
}

impl Level {
    pub fn symbol(self) -> &'static str {
        match self {
            Level::Ok => "✓",
            Level::Warn => "!",
            Level::Fail => "✗",
            Level::Skip => "-",
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
/// Database plus logs above this is `!`.
const DISK_WARN: u64 = 1 << 30;
/// Per upstream `HEAD`.
const UPSTREAM_TIMEOUT: Duration = Duration::from_secs(5);

/// `pooltime doctor`.
pub async fn run(paths: &Paths) -> anyhow::Result<i32> {
    let checks = checks(paths).await;
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
pub async fn checks(paths: &Paths) -> Vec<Check> {
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
    out.extend(profile_checks(
        paths,
        &cfg.profiles,
        &format!("http://{addr}"),
        probe == Probe::Pooltime,
        catalog.as_deref(),
    ));
    let warn_days = cfg.alerts.warn_days.iter().copied().max().unwrap_or(3);
    let facts = account_facts(paths, daemon).await;
    // Codex hosts matter only with an OpenAI account (unknown when the status is unreadable).
    let openai = facts
        .as_ref()
        .is_ok_and(|(f, _)| f.iter().any(|a| a.provider == "openai"));
    out.extend(account_checks(facts, warn_days, OffsetDateTime::now_utc()));

    let (upstream, date) = upstream_checks(&cfg, openai).await;
    out.extend(upstream);
    out.push(clock_check(date, OffsetDateTime::now_utc()));
    out.extend(permission_checks(paths));
    out.push(disk_check(paths));
    out
}

fn service_check(paths: &Paths) -> Check {
    let label = "service";
    let st = Backend::native()
        .and_then(|b| Ok((b, Env::from_process(paths)?)))
        .and_then(|(b, env)| b.status(&env, &SystemRunner));
    match st {
        Ok(s) if s.running => Check::new(Level::Ok, label, "installed and running"),
        Ok(s) if s.installed => Check::new(Level::Fail, label, "installed but not running")
            .hint("`pooltime service restart`, then `pooltime logs` if it stops again"),
        Ok(_) => Check::new(
            Level::Warn,
            label,
            "not installed (the pool runs only while `pooltime serve` does)",
        )
        .hint("`pooltime service install`"),
        Err(e) => Check::new(
            Level::Warn,
            label,
            format!("could not ask the service manager: {e:#}"),
        ),
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

fn profile_checks(
    paths: &Paths,
    profiles: &[ProfileRef],
    pool_url: &str,
    pool_up: bool,
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
            if !pool_up {
                return Check::new(Level::Fail, label, "connected, pool down")
                    .hint("Claude Code can't reach the pool; start it or `pooltime disconnect`");
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
                "account state was unreadable and is being re-verified — wait for the next usage \
                 sweep or run `pooltime accounts recheck {}`",
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
        // The pool never acts on this recorded clock (only a refused refresh makes an account
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
        "billing" | "access_denied" => Level::Warn,
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
            Level::Warn,
            label,
            "not checked (no upstream answered with a Date header)",
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
    ];
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

fn disk_check(paths: &Paths) -> Check {
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
    let detail = format!(
        "{} (database {}, logs {})",
        mib(db + logs),
        mib(db),
        mib(logs)
    );
    if db + logs > DISK_WARN {
        Check::new(Level::Warn, "disk", detail)
            .hint("lower the retention in config.toml, or delete old files in logs/")
    } else {
        Check::new(Level::Ok, "disk", detail)
    }
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
        assert_eq!(clock_check(None, now).level, Level::Warn);
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
                "account state was unreadable and is being re-verified — wait for the next usage \
                 sweep or run `pooltime accounts recheck a`"
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
    fn line_format() {
        let c = Check::new(Level::Fail, "permissions", "/x is 0755").hint("chmod 700 /x");
        assert_eq!(c.to_string(), "✗ permissions: /x is 0755 — chmod 700 /x");
    }
}
