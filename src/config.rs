//! Config file, `POOLTIME_*` env overlay and on-disk paths.
//!
//! Precedence: built-in defaults < `config.toml` < `POOLTIME_*` env. Generic
//! `PORT`/`HOST` are deliberately ignored. After merging, durations are clamped
//! to their floors (see [`Config::normalize`]). Durations are humantime strings
//! in TOML (`"15m"`, `"2500ms"`).

use std::fs::{self, DirBuilder};
use std::io;
use std::net::{IpAddr, Ipv4Addr};
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{anyhow, Context};
use serde::{Deserialize, Serialize};

const SECS: fn(u64) -> Duration = Duration::from_secs;

/// Locations under the pooltime data directory (`~/.pooltime`, mode 0700).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Paths {
    pub home: PathBuf,
    /// `config.toml`
    pub config: PathBuf,
    pub accounts: PathBuf,
    /// `pooltime.db`
    pub db: PathBuf,
    pub logs: PathBuf,
    /// pid file, `mgmt.key`
    pub run: PathBuf,
}

impl Paths {
    /// `POOLTIME_HOME` if set (and non-empty), else `$HOME/.pooltime`.
    pub fn resolve() -> Paths {
        Self::resolve_with(|k| std::env::var(k).ok())
    }

    pub(crate) fn resolve_with(get: impl Fn(&str) -> Option<String>) -> Paths {
        let non_empty = |k: &str| get(k).filter(|v| !v.is_empty());
        let home = match non_empty("POOLTIME_HOME") {
            Some(h) => PathBuf::from(h),
            None => {
                PathBuf::from(non_empty("HOME").unwrap_or_else(|| ".".into())).join(".pooltime")
            }
        };
        Self::under(&home)
    }

    pub fn under(home: &Path) -> Paths {
        Paths {
            home: home.to_path_buf(),
            config: home.join("config.toml"),
            accounts: home.join("accounts"),
            db: home.join("pooltime.db"),
            logs: home.join("logs"),
            run: home.join("run"),
        }
    }

    /// Create the directory tree with mode 0700 (also tightens existing dirs).
    pub fn ensure(&self) -> io::Result<()> {
        for d in [&self.home, &self.accounts, &self.logs, &self.run] {
            DirBuilder::new().recursive(true).mode(0o700).create(d)?;
            fs::set_permissions(d, fs::Permissions::from_mode(0o700))?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub host: IpAddr,
    pub port: u16,
    /// Optional shared secret clients must present to the proxy.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub proxy_key: Option<String>,
    pub timeouts: Timeouts,
    pub routing: RoutingCfg,
    pub retention: RetentionCfg,
    pub profiles: Vec<ProfileRef>,
    pub anthropic: AnthropicCfg,
    pub openai: OpenAiCfg,
    pub alerts: AlertsCfg,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            host: IpAddr::V4(Ipv4Addr::LOCALHOST),
            port: 3460,
            proxy_key: None,
            timeouts: Timeouts::default(),
            routing: RoutingCfg::default(),
            retention: RetentionCfg::default(),
            profiles: Vec::new(),
            anthropic: AnthropicCfg::default(),
            openai: OpenAiCfg::default(),
            alerts: AlertsCfg::default(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Timeouts {
    /// Whole-request deadline.
    #[serde(with = "humantime_serde")]
    pub request_timeout: Duration,
    /// Max silence on an Anthropic stream (floor 30 s, <= request_timeout).
    #[serde(with = "humantime_serde")]
    pub anthropic_idle: Duration,
    /// Max silence on a Codex stream (floor 30 s).
    #[serde(with = "humantime_serde")]
    pub codex_idle: Duration,
    /// Refresh an access token this long before it expires.
    #[serde(with = "humantime_serde")]
    pub token_refresh_skew: Duration,
    #[serde(with = "humantime_serde")]
    pub token_refresh_timeout: Duration,
    /// How long a request waits on an in-flight refresh/probe of the same account.
    #[serde(with = "humantime_serde")]
    pub inflight_wait: Duration,
    /// SSE keepalive cadence before the first upstream byte (floor 100 ms).
    #[serde(with = "humantime_serde")]
    pub stream_keepalive: Duration,
    /// Rolling usage window (Anthropic five-hour window).
    #[serde(with = "humantime_serde")]
    pub usage_window: Duration,
    #[serde(with = "humantime_serde")]
    pub rate_limit_cooldown: Duration,
    #[serde(with = "humantime_serde")]
    pub access_denied_cooldown: Duration,
    /// Max retries on upstream overload (529).
    pub overload_retry_max: u32,
    #[serde(with = "humantime_serde")]
    pub overload_retry_base: Duration,
    #[serde(with = "humantime_serde")]
    pub overload_retry_cap: Duration,
    /// Session stickiness expiry (floor 60 s).
    #[serde(with = "humantime_serde")]
    pub session_idle: Duration,
}

impl Default for Timeouts {
    fn default() -> Self {
        Timeouts {
            request_timeout: SECS(900),
            anthropic_idle: SECS(600),
            codex_idle: SECS(300),
            token_refresh_skew: SECS(300),
            token_refresh_timeout: SECS(30),
            inflight_wait: SECS(90),
            stream_keepalive: SECS(1),
            usage_window: SECS(5 * 3600),
            rate_limit_cooldown: SECS(3600),
            access_denied_cooldown: SECS(3600),
            overload_retry_max: 4,
            overload_retry_base: Duration::from_millis(500),
            overload_retry_cap: SECS(8),
            session_idle: SECS(30 * 60),
        }
    }
}

/// Account selection tuning. Defaults mirror bro's `DEFAULT_TUNING_EXP`
/// (`fiveHourExp` 1, `headroomTaperStart` 0.20) and `routingMinHeadroom` 0.1.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct RoutingCfg {
    /// Minimum gate headroom (0-1 fraction) for an account to stay viable.
    pub min_headroom: f64,
    /// Exponent on the five-hour headroom factor, in [0, 5].
    pub five_hour_exp: f64,
    /// Five-hour headroom below which placement shares taper, in (0, 1].
    pub headroom_taper_start: f64,
    /// 0 = unlimited.
    pub anthropic_max_inflight: u32,
    pub codex_max_inflight: u32,
}

impl Default for RoutingCfg {
    fn default() -> Self {
        RoutingCfg {
            min_headroom: 0.1,
            five_hour_exp: 1.0,
            headroom_taper_start: 0.20,
            anthropic_max_inflight: 0,
            codex_max_inflight: 4,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct RetentionCfg {
    pub logs_days: u32,
    pub logs_max_mb: u32,
    pub journal_days: u32,
    pub journal_max_mb: u32,
    /// Usage history: hourly rows kept this long, then rolled up to daily...
    pub history_hourly_days: u32,
    /// ...kept this long, then rolled up to weekly...
    pub history_daily_days: u32,
    /// ...kept this long.
    pub history_weekly_days: u32,
}

impl Default for RetentionCfg {
    fn default() -> Self {
        RetentionCfg {
            logs_days: 7,
            logs_max_mb: 50,
            journal_days: 30,
            journal_max_mb: 200,
            history_hourly_days: 30,
            history_daily_days: 365,
            history_weekly_days: 730,
        }
    }
}

/// A Claude Code profile pooltime may point at the proxy. `settings_dir` is the
/// profile directory (`~/.claude`, `~/.claude-personal`, ...).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProfileRef {
    pub id: String,
    pub settings_dir: PathBuf,
}

/// Anthropic upstream. URLs, UA and client id are config because they drift
/// across Claude Code versions (docs/spikes.md, S1).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AnthropicCfg {
    pub api_base: String,
    pub token_url: String,
    pub authorize_url: String,
    pub manual_redirect_url: String,
    pub profile_url: String,
    pub client_id: String,
    /// Authorize scope for the claude.ai login (space separated).
    pub scope: String,
    /// UA for profile/usage calls (Claude Code version string).
    pub user_agent: String,
    pub usage_refresh: bool,
    #[serde(with = "humantime_serde")]
    pub usage_ttl: Duration,
    /// Floor 250 ms.
    #[serde(with = "humantime_serde")]
    pub usage_fetch_timeout: Duration,
}

impl Default for AnthropicCfg {
    fn default() -> Self {
        AnthropicCfg {
            api_base: "https://api.anthropic.com".into(),
            token_url: "https://platform.claude.com/v1/oauth/token".into(),
            authorize_url: "https://claude.com/cai/oauth/authorize".into(),
            manual_redirect_url: "https://platform.claude.com/oauth/code/callback".into(),
            profile_url: "https://api.anthropic.com/api/oauth/profile".into(),
            client_id: "9d1c250a-e61b-44d9-88ed-5944d1962f5e".into(),
            scope: "org:create_api_key user:profile user:inference user:sessions:claude_code \
                    user:mcp_servers user:file_upload user:plugins"
                .into(),
            user_agent: "claude-code/2.1.319".into(),
            usage_refresh: true,
            usage_ttl: SECS(120),
            usage_fetch_timeout: Duration::from_millis(2500),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct OpenAiCfg {
    pub api_base: String,
    pub usage_url: String,
    pub token_url: String,
    pub client_id: String,
    pub user_agent: String,
}

impl Default for OpenAiCfg {
    fn default() -> Self {
        OpenAiCfg {
            api_base: "https://chatgpt.com/backend-api/codex".into(),
            usage_url: "https://chatgpt.com/backend-api/wham/usage".into(),
            token_url: "https://auth.openai.com/oauth/token".into(),
            client_id: "app_EMoamEEZ73f0CkXaXp7hrann".into(),
            user_agent: "codex-cli".into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AlertsCfg {
    /// Warn this many days before a credential/subscription expires.
    pub warn_days: Vec<u32>,
}

impl Default for AlertsCfg {
    fn default() -> Self {
        AlertsCfg {
            warn_days: vec![3, 1],
        }
    }
}

impl Config {
    /// Load `config.toml` (missing file = defaults, corrupt file = error), apply
    /// the `POOLTIME_*` env overlay, then clamp.
    pub fn load(p: &Paths) -> anyhow::Result<Config> {
        Self::load_with_env(p, |k| std::env::var(k).ok())
    }

    pub(crate) fn load_with_env(
        p: &Paths,
        get: impl Fn(&str) -> Option<String>,
    ) -> anyhow::Result<Config> {
        let mut cfg = match fs::read_to_string(&p.config) {
            Ok(s) => toml::from_str::<Config>(&s)
                .with_context(|| format!("parsing {}", p.config.display()))?,
            Err(e) if e.kind() == io::ErrorKind::NotFound => Config::default(),
            Err(e) => return Err(e).with_context(|| format!("reading {}", p.config.display())),
        };
        cfg.apply_env(get)?;
        cfg.normalize();
        Ok(cfg)
    }

    /// Overlay `POOLTIME_*` variables. Only the `POOLTIME_` prefix is read.
    pub(crate) fn apply_env(&mut self, get: impl Fn(&str) -> Option<String>) -> anyhow::Result<()> {
        let var = |k: &str| get(k).filter(|v| !v.is_empty());
        if let Some(v) = var("POOLTIME_HOST") {
            self.host = v
                .parse()
                .map_err(|_| anyhow!("POOLTIME_HOST is not an IP address"))?;
        }
        if let Some(v) = var("POOLTIME_PORT") {
            self.port = v
                .parse()
                .map_err(|_| anyhow!("POOLTIME_PORT is not a valid port"))?;
        }
        if let Some(v) = var("POOLTIME_PROXY_KEY") {
            self.proxy_key = Some(v);
        }
        Ok(())
    }

    /// Enforce duration floors and `anthropic_idle <= request_timeout`.
    pub fn normalize(&mut self) {
        let t = &mut self.timeouts;
        t.anthropic_idle = t
            .anthropic_idle
            .max(SECS(30))
            .min(t.request_timeout.max(SECS(30)));
        t.codex_idle = t.codex_idle.max(SECS(30));
        t.stream_keepalive = t.stream_keepalive.max(Duration::from_millis(100));
        t.session_idle = t.session_idle.max(SECS(60));
        self.anthropic.usage_fetch_timeout = self
            .anthropic
            .usage_fetch_timeout
            .max(Duration::from_millis(250));
    }

    /// Atomically write `config.toml` with mode 0600.
    pub fn save(&self, p: &Paths) -> anyhow::Result<()> {
        p.ensure()?;
        let s = toml::to_string_pretty(self).context("serializing config")?;
        crate::fsutil::atomic_write(&p.config, s.as_bytes(), 0o600)
            .with_context(|| format!("writing {}", p.config.display()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::os::unix::fs::PermissionsExt;
    use std::time::Duration;

    fn env_of(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let m: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |k| m.get(k).cloned()
    }

    fn no_env(_: &str) -> Option<String> {
        None
    }

    #[test]
    fn defaults_match_spec() {
        let dir = tempfile::tempdir().unwrap();
        let p = Paths::under(dir.path());
        // Missing config file -> defaults, not an error.
        let c = Config::load_with_env(&p, no_env).unwrap();
        assert_eq!(c.host.to_string(), "127.0.0.1");
        assert_eq!(c.port, 3460);
        assert_eq!(c.timeouts.request_timeout, Duration::from_secs(900));
        assert_eq!(c.timeouts.anthropic_idle, Duration::from_secs(600));
        assert_eq!(c.timeouts.codex_idle, Duration::from_secs(300));
        assert_eq!(c.routing.codex_max_inflight, 4);
        assert_eq!(c.routing.anthropic_max_inflight, 0);
        assert_eq!(c.retention.journal_days, 30);
        assert_eq!(c.alerts.warn_days, vec![3, 1]);
        assert_eq!(c.anthropic.user_agent, "claude-code/2.1.319");
        assert_eq!(c.openai.client_id, "app_EMoamEEZ73f0CkXaXp7hrann");
    }

    #[test]
    fn partial_toml_loads_with_humantime_durations() {
        let dir = tempfile::tempdir().unwrap();
        let p = Paths::under(dir.path());
        std::fs::write(
            &p.config,
            "port = 4001\n[timeouts]\ninflight_wait = \"2m\"\n[[profiles]]\nid = \"main\"\nsettings_dir = \"/x/.claude\"\n",
        )
        .unwrap();
        let c = Config::load_with_env(&p, no_env).unwrap();
        assert_eq!(c.port, 4001);
        assert_eq!(c.timeouts.inflight_wait, Duration::from_secs(120));
        assert_eq!(c.timeouts.request_timeout, Duration::from_secs(900));
        assert_eq!(c.profiles[0].id, "main");
        // Corrupt file is an error.
        std::fs::write(&p.config, "port = [").unwrap();
        assert!(Config::load_with_env(&p, no_env).is_err());
    }

    #[test]
    fn env_overrides_win() {
        let dir = tempfile::tempdir().unwrap();
        let p = Paths::under(dir.path());
        std::fs::write(&p.config, "port = 3999\n").unwrap();
        let env = env_of(&[
            ("POOLTIME_PORT", "4000"),
            ("PORT", "5000"),
            ("HOST", "0.0.0.0"),
        ]);
        let c = Config::load_with_env(&p, env).unwrap();
        assert_eq!(c.port, 4000);
        assert_eq!(c.host.to_string(), "127.0.0.1");
        let c = Config::load_with_env(&p, env_of(&[("POOLTIME_HOST", "0.0.0.0")])).unwrap();
        assert_eq!(c.host.to_string(), "0.0.0.0");
        assert!(Config::load_with_env(&p, env_of(&[("POOLTIME_PORT", "nope")])).is_err());
    }

    #[test]
    fn paths_resolve_from_env() {
        let p = Paths::resolve_with(env_of(&[("POOLTIME_HOME", "/h/pt"), ("HOME", "/h/u")]));
        assert_eq!(p.home, std::path::PathBuf::from("/h/pt"));
        let p = Paths::resolve_with(env_of(&[("HOME", "/h/u")]));
        assert_eq!(p.home, std::path::PathBuf::from("/h/u/.pooltime"));
        assert_eq!(p.config, p.home.join("config.toml"));
        assert_eq!(p.db, p.home.join("pooltime.db"));
    }

    #[test]
    fn ensure_creates_0700_dirs() {
        let dir = tempfile::tempdir().unwrap();
        let p = Paths::under(&dir.path().join("pt"));
        p.ensure().unwrap();
        for d in [&p.home, &p.accounts, &p.logs, &p.run] {
            let mode = std::fs::metadata(d).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o700, "{}", d.display());
        }
    }

    #[test]
    fn idle_is_clamped() {
        let dir = tempfile::tempdir().unwrap();
        let p = Paths::under(dir.path());
        std::fs::write(&p.config, "[timeouts]\nanthropic_idle = \"10s\"\ncodex_idle = \"1s\"\nstream_keepalive = \"1ms\"\nsession_idle = \"1s\"\n").unwrap();
        let c = Config::load_with_env(&p, no_env).unwrap();
        assert_eq!(c.timeouts.anthropic_idle, Duration::from_secs(30));
        assert_eq!(c.timeouts.codex_idle, Duration::from_secs(30));
        assert_eq!(c.timeouts.stream_keepalive, Duration::from_millis(100));
        assert_eq!(c.timeouts.session_idle, Duration::from_secs(60));

        std::fs::write(
            &p.config,
            "[timeouts]\nrequest_timeout = \"100s\"\nanthropic_idle = \"1h\"\n[anthropic]\nusage_fetch_timeout = \"10ms\"\n",
        )
        .unwrap();
        let c = Config::load_with_env(&p, no_env).unwrap();
        assert_eq!(c.timeouts.anthropic_idle, Duration::from_secs(100));
        assert_eq!(c.anthropic.usage_fetch_timeout, Duration::from_millis(250));
    }

    #[test]
    fn save_then_load_roundtrips_and_file_is_0600() {
        let dir = tempfile::tempdir().unwrap();
        let p = Paths::under(&dir.path().join("pt"));
        let mut c = Config {
            port: 4321,
            proxy_key: Some("k".into()),
            profiles: vec![ProfileRef {
                id: "work".into(),
                settings_dir: "/tmp/x/.claude-work".into(),
            }],
            ..Config::default()
        };
        c.timeouts.inflight_wait = Duration::from_secs(45);
        c.save(&p).unwrap();
        let mode = std::fs::metadata(&p.config).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        let back = Config::load_with_env(&p, no_env).unwrap();
        assert_eq!(back, c);
        // No temp files left behind.
        let leftovers: Vec<_> = std::fs::read_dir(&p.home)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.contains(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
        // Overwrite stays 0600.
        c.port = 4322;
        c.save(&p).unwrap();
        let mode = std::fs::metadata(&p.config).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }
}
