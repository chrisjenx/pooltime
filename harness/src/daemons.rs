//! Booting bro and pooltime in throwaway sandboxes.
//!
//! Safety (both daemons): the child environment is cleared and rebuilt from an explicit list;
//! `HOME`/`TMPDIR` point into the scenario's temp dir; ports are fresh and never bro's live 3456
//! (nor pooltime's default 3460); every upstream URL points at a 127.0.0.1 fake; accounts are
//! seeded as files holding fake tokens. `PATH` starts with a shim directory whose `security`
//! records any call and fails, so bro can never read the macOS Keychain, and the harness fails a
//! scenario if the shim was ever invoked.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use anyhow::{bail, ensure, Context};
use serde_json::{json, Value};
use tokio::process::{Child, Command};

use crate::fake::{access_token, refresh_token, Upstreams, OAUTH_TOKEN, TOKEN};
use crate::scenarios::{AccountSpec, Provider, Scenario};

/// Ports the harness must never bind or talk to: bro's live pool and pooltime's default.
pub const FORBIDDEN_PORTS: &[u16] = &[3456, 3460];

/// Same-account overload retry knobs, identical for both daemons (fast but non-zero backoff).
const OVERLOAD_RETRY_MAX: u32 = 3;
const OVERLOAD_BASE_MS: u64 = 20;
const OVERLOAD_CAP_MS: u64 = 80;

const BOOT_TIMEOUT: Duration = Duration::from_secs(30);

/// Where the binaries under test live.
pub struct Tools {
    pub bun: PathBuf,
    /// bro-cli checkout root (contains `pool/src/index.ts`).
    pub bro: PathBuf,
    pub pooltime: PathBuf,
    /// The invoking user's real home, used only for safety assertions.
    pub real_home: Option<PathBuf>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Bro,
    Pooltime,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Bro => "bro",
            Kind::Pooltime => "pooltime",
        }
    }
}

/// A running daemon in its sandbox.
pub struct Daemon {
    pub kind: Kind,
    pub base: String,
    child: Child,
    /// pooltime's management key (`run/mgmt.key`); bro's status API is unauthenticated.
    mgmt_key: Option<String>,
    sandbox: PathBuf,
}

impl Daemon {
    /// Stop the child and wait for it.
    pub async fn stop(mut self) {
        let _ = self.child.start_kill();
        let _ = tokio::time::timeout(Duration::from_secs(5), self.child.wait()).await;
    }

    /// True when the `security` shim was invoked (bro tried the Keychain).
    pub fn keychain_touched(&self) -> bool {
        self.sandbox.join("keychain-calls.log").exists()
    }

    /// Account state normalized to `name -> (status label, cooldown bucket)`.
    pub async fn accounts(
        &self,
        client: &reqwest::Client,
    ) -> anyhow::Result<Vec<(String, String, String)>> {
        match self.kind {
            Kind::Bro => {
                let v: Value = client
                    .get(format!("{}/api/status", self.base))
                    .send()
                    .await?
                    .json()
                    .await?;
                let now = v["now"].as_f64().unwrap_or(0.0);
                let mut out = Vec::new();
                for a in v["accounts"].as_array().cloned().unwrap_or_default() {
                    out.push(bro_account(&a, now));
                }
                Ok(out)
            }
            Kind::Pooltime => {
                let key = self.mgmt_key.as_deref().unwrap_or("");
                let v: Value = client
                    .get(format!("{}/api/v1/status", self.base))
                    .bearer_auth(key)
                    .send()
                    .await?
                    .json()
                    .await?;
                let now = v["now"]
                    .as_str()
                    .and_then(parse_rfc3339)
                    .unwrap_or_else(now_ms);
                let mut out = Vec::new();
                for a in v["accounts"].as_array().cloned().unwrap_or_default() {
                    let until = a["until"].as_str().and_then(parse_rfc3339);
                    out.push((
                        a["id"].as_str().unwrap_or("?").to_string(),
                        a["status"].as_str().unwrap_or("?").to_string(),
                        cooldown_bucket(until, now),
                    ));
                }
                Ok(out)
            }
        }
    }
}

/// bro's `/api/status` account reduced to pooltime's status labels (spec §16):
/// `ready | needs_login | rate_limited | billing | access_denied`.
fn bro_account(a: &Value, now: f64) -> (String, String, String) {
    let name = a["name"].as_str().unwrap_or("?").to_string();
    let u = &a["usage"];
    let future = |k: &str| u[k].as_f64().filter(|t| *t > now);
    // Logged out, or a dead refresh token (only counts while it still blocks the account).
    let dead = a["available"] == false && !u["deadRefreshToken"].is_null();
    let (label, until) = if a["authenticated"] != true || dead {
        ("needs_login", None)
    } else if let Some(t) = future("accessDeniedUntil") {
        if a["billingBlocked"] == true {
            ("billing", None)
        } else {
            ("access_denied", Some(t))
        }
    } else if let Some(t) = future("rateLimitedUntil") {
        ("rate_limited", Some(t))
    } else if a["available"] == false {
        // Spent window from the rate-limit snapshot (no explicit cooldown).
        ("rate_limited", None)
    } else {
        ("ready", None)
    };
    (name, label.to_string(), cooldown_bucket(until, now))
}

/// Remaining cooldown rounded to minutes (`-` when none), so the two daemons' clocks and the
/// seconds between their status reads do not matter.
fn cooldown_bucket(until_ms: Option<f64>, now_ms: f64) -> String {
    match until_ms {
        Some(t) if t > now_ms => format!("{}m", ((t - now_ms) / 60_000.0).round()),
        _ => "-".into(),
    }
}

fn now_ms() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as f64)
        .unwrap_or(0.0)
}

fn parse_rfc3339(s: &str) -> Option<f64> {
    let t = time::OffsetDateTime::parse(s, &time::format_description::well_known::Rfc3339).ok()?;
    Some((t.unix_timestamp_nanos() / 1_000_000) as f64)
}

/// A free 127.0.0.1 port that is not one of [`FORBIDDEN_PORTS`].
fn free_port() -> anyhow::Result<u16> {
    for _ in 0..20 {
        let l = std::net::TcpListener::bind("127.0.0.1:0")?;
        let port = l.local_addr()?.port();
        if !FORBIDDEN_PORTS.contains(&port) {
            return Ok(port);
        }
    }
    bail!("no free port")
}

/// Refuse to run a daemon whose home could be the user's real one.
fn assert_sandboxed(tools: &Tools, home: &Path, port: u16) -> anyhow::Result<()> {
    ensure!(
        !FORBIDDEN_PORTS.contains(&port),
        "refusing to use port {port}"
    );
    let home = home.canonicalize()?;
    if let Some(real) = &tools.real_home {
        let real = real.canonicalize().unwrap_or_else(|_| real.clone());
        ensure!(
            home != real && !real.starts_with(&home),
            "sandbox home {} overlaps the real home",
            home.display()
        );
        for d in [".claude-max-pool", ".bro", ".pooltime", ".claude"] {
            ensure!(
                !home.starts_with(real.join(d)),
                "sandbox home {} is inside ~/{d}",
                home.display()
            );
        }
    }
    Ok(())
}

/// `PATH` for children: the Keychain shim first, then the system and bun's directory.
fn sandbox_path(tools: &Tools, sandbox: &Path) -> anyhow::Result<String> {
    let bin = sandbox.join("bin");
    fs::create_dir_all(&bin)?;
    let shim = bin.join("security");
    let log = sandbox.join("keychain-calls.log");
    fs::write(
        &shim,
        format!("#!/bin/sh\necho called >> '{}'\nexit 44\n", log.display()),
    )?;
    fs::set_permissions(&shim, fs::Permissions::from_mode(0o755))?;
    let bun_dir = tools
        .bun
        .parent()
        .map(|p| p.display().to_string())
        .unwrap_or_default();
    Ok(format!("{}:{bun_dir}:/usr/bin:/bin", bin.display()))
}

fn write_private(path: &Path, text: &str) -> anyhow::Result<()> {
    if let Some(p) = path.parent() {
        fs::create_dir_all(p)?;
        fs::set_permissions(p, fs::Permissions::from_mode(0o700))?;
    }
    fs::write(path, text)?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    Ok(())
}

fn expiry_ms(a: &AccountSpec) -> i64 {
    let now = now_ms() as i64;
    if a.expired {
        now - 3_600_000
    } else {
        now + 24 * 3_600_000
    }
}

// ---- bro ---------------------------------------------------------------------------------------

fn seed_bro(pool: &Path, scenario: &Scenario) -> anyhow::Result<()> {
    for a in &scenario.accounts {
        let dir = pool.join("accounts").join(a.name);
        match a.provider {
            Provider::Anthropic => write_private(
                &dir.join(".credentials.json"),
                &json!({"claudeAiOauth": {
                    "accessToken": access_token("a", a.name),
                    "refreshToken": refresh_token(a.name),
                    "expiresAt": expiry_ms(a),
                    "scopes": ["user:inference", "user:profile"],
                    "subscriptionType": "max",
                }})
                .to_string(),
            )?,
            Provider::OpenAi => write_private(
                &dir.join("openai-auth.json"),
                &json!({
                    "accessToken": access_token("o", a.name),
                    "refreshToken": refresh_token(a.name),
                    "accountId": format!("chatgpt-{}", a.name),
                    "expiresAt": expiry_ms(a),
                    "planType": "plus",
                })
                .to_string(),
            )?,
        }
        write_private(
            &dir.join("routing.json"),
            &json!({"priority": a.priority}).to_string(),
        )?;
    }
    if scenario.mapping {
        write_private(
            &pool.join("models.json"),
            &json!({"mappingEnabled": true}).to_string(),
        )?;
    }
    Ok(())
}

pub async fn start_bro(
    tools: &Tools,
    scenario: &Scenario,
    up: &Upstreams,
    sandbox: &Path,
) -> anyhow::Result<Daemon> {
    let home = sandbox.join("home");
    let tmp = sandbox.join("tmp");
    let pool = sandbox.join("pool");
    let claude_cfg = sandbox.join("claude-config");
    for d in [&home, &tmp, &pool, &claude_cfg] {
        fs::create_dir_all(d)?;
    }
    let port = free_port()?;
    assert_sandboxed(tools, &home, port)?;
    seed_bro(&pool, scenario)?;
    let path = sandbox_path(tools, sandbox)?;
    let log = fs::File::create(sandbox.join("daemon.log"))?;
    let a = up.anthropic.url();
    let o = up.openai.url();
    let child = Command::new(&tools.bun)
        .args(["run", "src/index.ts", "serve"])
        .current_dir(tools.bro.join("pool"))
        .env_clear()
        .env("PATH", path)
        .env("HOME", &home)
        .env("TMPDIR", &tmp)
        .env("CLAUDE_POOL_DIR", &pool)
        .env("CLAUDE_CONFIG_DIR", &claude_cfg)
        .env("HOST", "127.0.0.1")
        .env("PORT", port.to_string())
        .env("ANTHROPIC_API_BASE_URL", &a)
        .env("CLAUDE_OAUTH_TOKEN_URL", format!("{a}{TOKEN}"))
        .env("CODEX_API_BASE_URL", &o)
        .env("OPENAI_OAUTH_TOKEN_URL", format!("{o}{OAUTH_TOKEN}"))
        .env("CODEX_USAGE_URL", format!("{o}/wham/usage"))
        .env("CLAUDE_USAGE_REFRESH", "0")
        .env("OVERLOAD_RETRY_MAX", OVERLOAD_RETRY_MAX.to_string())
        .env("OVERLOAD_RETRY_BASE_MS", OVERLOAD_BASE_MS.to_string())
        .env("OVERLOAD_RETRY_MAX_DELAY_MS", OVERLOAD_CAP_MS.to_string())
        .env("NO_COLOR", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::from(log.try_clone()?))
        .stderr(Stdio::from(log))
        .kill_on_drop(true)
        .spawn()
        .context("spawning bro (bun)")?;
    let d = Daemon {
        kind: Kind::Bro,
        base: format!("http://127.0.0.1:{port}"),
        child,
        mgmt_key: None,
        sandbox: sandbox.to_path_buf(),
    };
    wait_healthy(d).await
}

// ---- pooltime ----------------------------------------------------------------------------------

fn seed_pooltime(
    home: &Path,
    scenario: &Scenario,
    up: &Upstreams,
    port: u16,
) -> anyhow::Result<()> {
    let a = up.anthropic.url();
    let o = up.openai.url();
    let config = format!(
        r#"host = "127.0.0.1"
port = {port}

[timeouts]
overload_retry_max = {OVERLOAD_RETRY_MAX}
overload_retry_base = "{OVERLOAD_BASE_MS}ms"
overload_retry_cap = "{OVERLOAD_CAP_MS}ms"

[anthropic]
api_base = "{a}"
token_url = "{a}{TOKEN}"
profile_url = "{a}/api/oauth/profile"
usage_refresh = false

[openai]
api_base = "{o}"
token_url = "{o}{OAUTH_TOKEN}"
usage_url = "{o}/wham/usage"
"#
    );
    write_private(&home.join("config.toml"), &config)?;
    if scenario.mapping {
        write_private(&home.join("models.toml"), "mapping_enabled = true\n")?;
    }
    for acc in &scenario.accounts {
        let dir = home.join("accounts").join(acc.name);
        let provider = match acc.provider {
            Provider::Anthropic => "anthropic",
            Provider::OpenAi => "openai",
        };
        write_private(
            &dir.join("account.toml"),
            &format!(
                "id = \"{}\"\nprovider = \"{provider}\"\npriority = {}\n",
                acc.name, acc.priority
            ),
        )?;
        let expires = time::OffsetDateTime::from_unix_timestamp(expiry_ms(acc) / 1000)?
            .format(&time::format_description::well_known::Rfc3339)?;
        let creds = match acc.provider {
            Provider::Anthropic => json!({
                "provider": "anthropic",
                "access_token": access_token("a", acc.name),
                "refresh_token": refresh_token(acc.name),
                "expires_at": expires,
                "scopes": ["user:inference", "user:profile"],
                "plan": "max",
            }),
            Provider::OpenAi => json!({
                "provider": "openai",
                "access_token": access_token("o", acc.name),
                "refresh_token": refresh_token(acc.name),
                "expires_at": expires,
                "scopes": [],
                "plan": "plus",
                "openai_account_id": format!("chatgpt-{}", acc.name),
            }),
        };
        write_private(&dir.join("credentials.json"), &creds.to_string())?;
    }
    Ok(())
}

pub async fn start_pooltime(
    tools: &Tools,
    scenario: &Scenario,
    up: &Upstreams,
    sandbox: &Path,
) -> anyhow::Result<Daemon> {
    let home = sandbox.join("home");
    let tmp = sandbox.join("tmp");
    let pt_home = sandbox.join("pooltime");
    for d in [&home, &tmp, &pt_home] {
        fs::create_dir_all(d)?;
    }
    fs::set_permissions(&pt_home, fs::Permissions::from_mode(0o700))?;
    let port = free_port()?;
    assert_sandboxed(tools, &home, port)?;
    seed_pooltime(&pt_home, scenario, up, port)?;
    let path = sandbox_path(tools, sandbox)?;
    let log = fs::File::create(sandbox.join("daemon.log"))?;
    let child = Command::new(&tools.pooltime)
        .args(["serve", "--i-stopped-bro"])
        .current_dir(sandbox)
        .env_clear()
        .env("PATH", path)
        .env("HOME", &home)
        .env("TMPDIR", &tmp)
        .env("POOLTIME_HOME", &pt_home)
        .stdin(Stdio::null())
        .stdout(Stdio::from(log.try_clone()?))
        .stderr(Stdio::from(log))
        .kill_on_drop(true)
        .spawn()
        .context("spawning pooltime")?;
    let mut d = Daemon {
        kind: Kind::Pooltime,
        base: format!("http://127.0.0.1:{port}"),
        child,
        mgmt_key: None,
        sandbox: sandbox.to_path_buf(),
    };
    d = wait_healthy(d).await?;
    let key = fs::read_to_string(pt_home.join("run").join("mgmt.key"))
        .context("reading pooltime's mgmt.key")?;
    d.mgmt_key = Some(key.trim().to_string());
    Ok(d)
}

async fn wait_healthy(mut d: Daemon) -> anyhow::Result<Daemon> {
    let client = reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(2))
        .build()?;
    let deadline = tokio::time::Instant::now() + BOOT_TIMEOUT;
    loop {
        if let Ok(Some(status)) = d.child.try_wait() {
            bail!(
                "{} exited during boot ({status}); see its log in the sandbox",
                d.kind.as_str()
            );
        }
        if let Ok(r) = client.get(format!("{}/health", d.base)).send().await {
            if r.status().is_success() {
                return Ok(d);
            }
        }
        if tokio::time::Instant::now() > deadline {
            let _ = d.child.start_kill();
            bail!("{} did not become healthy in time", d.kind.as_str());
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Last lines of a daemon's log with anything token-shaped masked (fake tokens only, but the
/// harness never prints credentials of any kind).
pub fn log_tail(sandbox: &Path, lines: usize) -> String {
    let text = fs::read_to_string(sandbox.join("daemon.log")).unwrap_or_default();
    let re = regex::Regex::new(r"fake-[aor]-[A-Za-z0-9_.-]+").expect("valid regex");
    let all: Vec<&str> = text.lines().collect();
    let start = all.len().saturating_sub(lines);
    re.replace_all(&all[start..].join("\n"), "<token>")
        .into_owned()
}
