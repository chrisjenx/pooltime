//! What every thin-client command shares: finding the running daemon, calling the management
//! API with the management key, prompts, and opening a browser.
//!
//! The daemon is found where `pooltime serve` would bind (config file, then `POOLTIME_PORT` /
//! `POOLTIME_HOST`), and only counts when its `/health` says it is a pooltime. Mutations send the
//! key from `run/mgmt.key` as `Authorization: Bearer`; it is never printed or logged here.

use std::io::{BufRead, Write};
use std::net::SocketAddr;
use std::time::Duration;

use anyhow::{anyhow, bail, Context};
use serde_json::Value;

use crate::config::{Config, Paths};
use crate::server::health::{probe, probe_addr, Probe};
use crate::server::mgmt::mgmt_key_path;

/// Set to `1` to never launch a browser (CI, tests); the URL is printed instead.
pub const NO_BROWSER_ENV: &str = "POOLTIME_NO_BROWSER";

/// Said whenever a command needs the daemon and none answers.
pub const START_HINT: &str =
    "pooltime is not running: start it with `pooltime serve` (or start the background service)";

/// The running daemon's address and management API.
pub struct Client {
    pub addr: SocketAddr,
    paths: Paths,
    http: reqwest::Client,
}

/// The daemon answering on the configured address, `None` when nothing listens there. Anything
/// else on the port (bro, another server, a hung listener) is an error: never guess.
pub async fn find(paths: &Paths) -> anyhow::Result<Option<Client>> {
    match locate(paths).await? {
        Located::Daemon(c) => Ok(Some(c)),
        Located::Absent {
            probe: Probe::Free, ..
        } => Ok(None),
        Located::Absent { addr, probe } => bail!(
            "{addr} does not answer as a pooltime ({probe:?}); is another server using the port?"
        ),
    }
}

/// What answers on the configured address.
pub enum Located {
    Daemon(Client),
    /// No pooltime there; `probe` says what was found instead.
    Absent {
        addr: SocketAddr,
        probe: Probe,
    },
}

/// Probe the configured address (for callers that can work without a daemon).
pub async fn locate(paths: &Paths) -> anyhow::Result<Located> {
    let cfg = Config::load(paths)?;
    let addr = probe_addr(cfg.host, cfg.port);
    Ok(match probe(addr).await {
        Probe::Pooltime => Located::Daemon(Client::new(addr, paths.clone())?),
        probe => Located::Absent { addr, probe },
    })
}

/// [`find`], with "not running" as an error that says how to start it.
pub async fn require(paths: &Paths) -> anyhow::Result<Client> {
    find(paths).await?.ok_or_else(|| anyhow!(START_HINT))
}

impl Client {
    fn new(addr: SocketAddr, paths: Paths) -> anyhow::Result<Client> {
        let http = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(3))
            .timeout(Duration::from_secs(120))
            .build()
            .context("building the HTTP client")?;
        Ok(Client { addr, paths, http })
    }

    /// `http://<addr>`.
    pub fn base(&self) -> String {
        format!("http://{}", self.addr)
    }

    /// The management key the daemon wrote at start (`run/mgmt.key`).
    pub fn mgmt_key(&self) -> anyhow::Result<String> {
        let path = mgmt_key_path(&self.paths);
        let key = std::fs::read_to_string(&path).with_context(|| {
            format!(
                "reading the management key at {} (is the daemon using this POOLTIME_HOME?)",
                path.display()
            )
        })?;
        Ok(key.trim().to_string())
    }

    pub async fn get(&self, path: &str) -> anyhow::Result<Value> {
        self.send(reqwest::Method::GET, path, None, false).await
    }

    /// An authenticated `POST` with a JSON body.
    pub async fn post(&self, path: &str, body: Value) -> anyhow::Result<Value> {
        self.send(reqwest::Method::POST, path, Some(body), true)
            .await
    }

    /// An authenticated `DELETE`.
    pub async fn delete(&self, path: &str) -> anyhow::Result<Value> {
        self.send(reqwest::Method::DELETE, path, None, true).await
    }

    /// An authenticated `GET`; `None` on 404.
    pub async fn get_authed_opt(&self, path: &str) -> anyhow::Result<Option<Value>> {
        let resp = self
            .http
            .get(format!("{}{path}", self.base()))
            .bearer_auth(self.mgmt_key()?)
            .send()
            .await
            .with_context(|| format!("calling pooltime at {}", self.addr))?;
        let status = resp.status();
        let bytes = resp
            .bytes()
            .await
            .context("reading the pooltime response")?;
        let v: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        if status == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if status.is_success() {
            return Ok(Some(v));
        }
        match v["error"]["message"].as_str() {
            Some(m) => bail!("{m}"),
            None => bail!("pooltime answered HTTP {status}"),
        }
    }

    async fn send(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<Value>,
        auth: bool,
    ) -> anyhow::Result<Value> {
        let mut req = self.http.request(method, format!("{}{path}", self.base()));
        if auth {
            req = req.bearer_auth(self.mgmt_key()?);
        }
        if let Some(b) = body {
            req = req
                .header(http::header::CONTENT_TYPE, "application/json")
                .body(b.to_string());
        }
        let resp = req
            .send()
            .await
            .with_context(|| format!("calling pooltime at {}", self.addr))?;
        let status = resp.status();
        let bytes = resp
            .bytes()
            .await
            .context("reading the pooltime response")?;
        let v: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        if status.is_success() {
            return Ok(v);
        }
        match v["error"]["message"].as_str() {
            Some(m) => bail!("{m}"),
            None => bail!("pooltime answered HTTP {status}"),
        }
    }
}

/// Percent-encode one path segment (account ids are `[a-z0-9_-]`, but never trust input).
pub fn seg(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// One line from stdin (trimmed), `None` at end of input. Runs off the async threads.
pub async fn read_line() -> anyhow::Result<Option<String>> {
    tokio::task::spawn_blocking(|| {
        let mut line = String::new();
        let n = std::io::stdin().lock().read_line(&mut line)?;
        Ok::<_, std::io::Error>((n > 0).then(|| line.trim().to_string()))
    })
    .await
    .context("reading stdin")?
    .context("reading stdin")
}

/// Ask `question [y/N]`; only `y`/`yes` agrees (end of input is "no").
pub async fn confirm(question: &str) -> anyhow::Result<bool> {
    print!("{question} [y/N] ");
    std::io::stdout().flush()?;
    let answer = read_line().await?;
    if answer.is_none() {
        println!();
    }
    Ok(matches!(
        answer.as_deref().map(str::to_ascii_lowercase).as_deref(),
        Some("y" | "yes")
    ))
}

/// No local browser to use: an SSH session, or Linux with neither X11 nor Wayland.
pub fn headless() -> bool {
    headless_with(|k| std::env::var_os(k).filter(|v| !v.is_empty()).is_some())
}

fn headless_with(set: impl Fn(&str) -> bool) -> bool {
    set("SSH_TTY") || (cfg!(target_os = "linux") && !set("DISPLAY") && !set("WAYLAND_DISPLAY"))
}

/// Open `url` with `open` (macOS) / `xdg-open` (Linux). Returns whether a browser was launched;
/// never when `POOLTIME_NO_BROWSER` is set or the session is [`headless`].
pub fn open_browser(url: &str) -> bool {
    if std::env::var_os(NO_BROWSER_ENV).is_some_and(|v| !v.is_empty() && v != "0") || headless() {
        return false;
    }
    let opener = if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    std::process::Command::new(opener)
        .arg(url)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn headless_rule() {
        let env = |vars: &'static [&'static str]| move |k: &str| vars.contains(&k);
        assert!(headless_with(env(&["SSH_TTY", "DISPLAY"])));
        assert!(!headless_with(env(&["DISPLAY"])));
        assert!(!headless_with(env(&["WAYLAND_DISPLAY"])));
        // Linux without a display server is headless; macOS always has a browser.
        assert_eq!(headless_with(env(&[])), cfg!(target_os = "linux"));
    }

    #[test]
    fn seg_encodes() {
        assert_eq!(seg("work-1_a"), "work-1_a");
        assert_eq!(seg("a/b c"), "a%2Fb%20c");
    }
}
