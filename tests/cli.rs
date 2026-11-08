use predicates::prelude::*;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};
use tempfile::TempDir;

#[test]
fn version_flag_prints_name_and_version() {
    procs::assert(Command::new(env!("CARGO_BIN_EXE_pooltime")).arg("--version"))
        .success()
        .stdout(predicate::str::starts_with("pooltime "));
}

const BOOT_BUDGET: Duration = Duration::from_secs(10);

/// A `pooltime serve` command over a temp home (never the real `$HOME`).
fn serve_cmd(home: &TempDir, port: u16) -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_pooltime"));
    c.args(["serve", "--i-stopped-bro", "--port", &port.to_string()])
        .env("POOLTIME_HOME", home.path().join("pt"))
        .env("HOME", home.path())
        .env_remove("POOLTIME_PORT")
        .env_remove("POOLTIME_HOST")
        .stdin(Stdio::null())
        .stdout(Stdio::null());
    c
}

/// A running daemon, killed on drop.
struct Daemon {
    child: Child,
    port: u16,
    _home: TempDir,
}

impl Daemon {
    fn spawn(home: TempDir, port: u16) -> Daemon {
        let child = procs::spawn(serve_cmd(&home, port).stderr(Stdio::piped()));
        Daemon {
            child,
            port,
            _home: home,
        }
    }

    fn wait_healthy(&mut self) {
        let start = Instant::now();
        while start.elapsed() < BOOT_BUDGET {
            if let Some(st) = self.child.try_wait().unwrap() {
                panic!("daemon exited early: {st}");
            }
            if let Some((200, body)) = get_health(self.port) {
                assert!(body.contains(r#""service":"pooltime""#), "{body}");
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        panic!("daemon did not answer /health within {BOOT_BUDGET:?}");
    }

    fn sigterm(&self) {
        let st = procs::spawn(Command::new("kill").args(["-TERM", &self.child.id().to_string()]))
            .wait()
            .unwrap();
        assert!(st.success());
    }

    fn wait_exit(&mut self) -> std::process::ExitStatus {
        let start = Instant::now();
        while start.elapsed() < BOOT_BUDGET {
            if let Some(st) = self.child.try_wait().unwrap() {
                return st;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        panic!("daemon did not exit after SIGTERM");
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn get_health(port: u16) -> Option<(u16, String)> {
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    let mut s = TcpStream::connect_timeout(&addr, Duration::from_millis(500)).ok()?;
    s.set_read_timeout(Some(Duration::from_secs(2))).ok()?;
    s.write_all(b"GET /health HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
        .ok()?;
    let mut raw = String::new();
    s.read_to_string(&mut raw).ok()?;
    let status = raw.split_whitespace().nth(1)?.parse().ok()?;
    let body = raw.split("\r\n\r\n").nth(1)?.to_string();
    Some((status, body))
}

#[test]
fn serve_boots_and_answers_health() {
    let mut d = Daemon::spawn(tempfile::tempdir().unwrap(), free_port());
    d.wait_healthy();
    d.sigterm();
    let st = d.wait_exit();
    assert_eq!(st.code(), Some(0), "graceful SIGTERM exits 0, got {st}");
}

#[test]
fn second_serve_on_same_port_says_already_running() {
    let mut first = Daemon::spawn(tempfile::tempdir().unwrap(), free_port());
    first.wait_healthy();
    let second_home = tempfile::tempdir().unwrap();
    let out = procs::output(&mut serve_cmd(&second_home, first.port));
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("already running"), "{err}");
    // The first daemon is unharmed.
    assert!(matches!(get_health(first.port), Some((200, _))));
}

// ---- Task 30: thin-client commands ------------------------------------------------------------

mod support;

use support::procs::{self, free_port};

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use serde_json::{json, Value};

/// Nothing listens on the discard port: an upstream base that can never reach the network.
const DEAD_UPSTREAM: &str = "http://127.0.0.1:9";

/// A temp `HOME` with a pooltime home under `pt/` and a `config.toml` pinning the port and
/// pointing every Anthropic URL at `upstream` (never the real network).
struct Home {
    dir: TempDir,
    port: u16,
}

impl Home {
    fn new() -> Home {
        Home::with_upstream(DEAD_UPSTREAM)
    }

    fn with_upstream(upstream: &str) -> Home {
        let h = Home {
            dir: tempfile::tempdir().unwrap(),
            port: free_port(),
        };
        std::fs::create_dir_all(h.pt()).unwrap();
        std::fs::write(
            h.pt().join("config.toml"),
            format!(
                "port = {port}\n\n[anthropic]\napi_base = \"{upstream}\"\n\
                 token_url = \"{upstream}/v1/oauth/token\"\nusage_refresh = false\n",
                port = h.port
            ),
        )
        .unwrap();
        h
    }

    fn pt(&self) -> PathBuf {
        self.dir.path().join("pt")
    }

    /// Environment for every child: nothing inherited but PATH/TMPDIR (so no stray `POOLTIME_*`,
    /// `SSH_TTY` or `DISPLAY`), the temp homes, and never a browser.
    fn env(&self, c: &mut Command) {
        c.env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("TMPDIR", std::env::temp_dir())
            .env("HOME", self.dir.path())
            .env("POOLTIME_HOME", self.pt())
            .env("POOLTIME_NO_BROWSER", "1");
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_pooltime"));
        c.args(args);
        self.env(&mut c);
        c
    }

    fn cli(&self, args: &[&str]) -> assert_cmd::assert::Assert {
        procs::assert(&mut self.command(args))
    }

    fn serve(&self) -> Served {
        let mut child = procs::spawn(
            self.command(&["serve", "--i-stopped-bro"])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null()),
        );
        let start = Instant::now();
        loop {
            if let Some(st) = child.try_wait().unwrap() {
                panic!("daemon exited early: {st}");
            }
            if let Some((200, _)) = get_health(self.port) {
                return Served(child);
            }
            assert!(start.elapsed() < BOOT_BUDGET, "daemon did not come up");
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    fn mgmt_key(&self) -> String {
        std::fs::read_to_string(self.pt().join("run/mgmt.key"))
            .unwrap()
            .trim()
            .to_string()
    }

    fn config(&self) -> String {
        std::fs::read_to_string(self.pt().join("config.toml")).unwrap()
    }
}

/// A `pooltime serve` child, killed on drop.
struct Served(Child);

impl Drop for Served {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn stdout_of(a: &assert_cmd::assert::Assert) -> String {
    String::from_utf8_lossy(&a.get_output().stdout).into_owned()
}

fn status_json(h: &Home) -> Value {
    let a = h.cli(&["status", "--json"]).success();
    serde_json::from_str(&stdout_of(&a)).unwrap()
}

#[test]
fn accounts_add_list_remove() {
    let h = Home::new();
    // Without a daemon the thin client says how to start one.
    h.cli(&["accounts", "add", "alpha"])
        .failure()
        .stderr(predicate::str::contains("pooltime serve"));

    let _d = h.serve();
    h.cli(&["accounts", "add", "alpha"])
        .success()
        .stdout(predicate::str::contains("alpha"));
    h.cli(&["accounts", "add", "alpha"])
        .failure()
        .stderr(predicate::str::contains("already exists"));
    h.cli(&["accounts", "add", "Bad Name"]).failure();

    let out = stdout_of(&h.cli(&["accounts", "list"]).success());
    assert!(out.contains("alpha"), "{out}");
    assert!(out.contains("anthropic"), "{out}");
    assert!(out.contains("needs_login"), "{out}");

    // Removing asks first; no answer means no.
    h.cli(&["accounts", "remove", "alpha"]).failure();
    assert!(stdout_of(&h.cli(&["accounts", "list"]).success()).contains("alpha"));

    h.cli(&["accounts", "remove", "alpha", "--yes"])
        .success()
        .stdout(predicate::str::contains("removed"));
    let out = stdout_of(&h.cli(&["accounts", "list"]).success());
    assert!(!out.contains("alpha"), "{out}");
    assert!(out.contains("no accounts"), "{out}");
    h.cli(&["accounts", "remove", "alpha", "--yes"])
        .failure()
        .stderr(predicate::str::contains("Unknown account"));

    // `--email` without `--login` records the expected identity (shown in the list).
    h.cli(&["accounts", "add", "beta", "--email", "b@example.com"])
        .success();
    let out = stdout_of(&h.cli(&["accounts", "list"]).success());
    assert!(out.contains("b@example.com"), "{out}");
}

#[test]
fn accounts_set_bounds() {
    let h = Home::new();
    let _d = h.serve();
    h.cli(&["accounts", "add", "alpha"]).success();

    for bad in ["0", "10.5", "-1"] {
        h.cli(&["accounts", "set", "alpha", "--weight", bad])
            .failure()
            .stderr(predicate::str::contains(
                "weight must be a number between 0.1 and 10",
            ));
    }
    h.cli(&["accounts", "set", "alpha", "--priority", "-3"])
        .failure();
    h.cli(&["accounts", "set", "alpha"])
        .failure()
        .stderr(predicate::str::contains("priority"));
    h.cli(&["accounts", "set", "alpha", "--until", "2h"])
        .failure()
        .stderr(predicate::str::contains("until requires weight"));
    h.cli(&["accounts", "set", "nobody", "--weight", "2"])
        .failure()
        .stderr(predicate::str::contains("Unknown account"));

    h.cli(&[
        "accounts",
        "set",
        "alpha",
        "--priority",
        "3",
        "--weight",
        "2.5",
    ])
    .success();
    let a = &status_json(&h)["accounts"][0];
    assert_eq!(a["priority"], 3);
    assert_eq!(a["weight"].as_f64(), Some(2.5));

    // A timed override (relative duration) applies on top, then can be cleared.
    h.cli(&[
        "accounts", "set", "alpha", "--weight", "0.1", "--until", "2h",
    ])
    .success();
    let a = &status_json(&h)["accounts"][0];
    assert_eq!(a["weight"].as_f64(), Some(2.5));
    assert_eq!(a["effective_weight"].as_f64(), Some(0.1));
    h.cli(&["accounts", "set", "alpha", "--clear-override"])
        .success();
    let a = &status_json(&h)["accounts"][0];
    assert_eq!(a["effective_weight"].as_f64(), Some(2.5));
}

/// Seed an account with credentials and a usage snapshot (7-day window `util` used) before the
/// daemon starts, so `status` has something to show without any upstream call.
async fn seed_account(pt: &Path, name: &str, util: f64) {
    use pooltime::accounts::{
        AccountMeta, RateLimitSnapshot, Registry, StateBook, Window, WindowStatus,
    };
    let paths = pooltime::config::Paths::under(pt);
    paths.ensure().unwrap();
    let reg = Registry::load(&paths).unwrap();
    let meta = AccountMeta::new(
        name.parse().unwrap(),
        pooltime::providers::ProviderId::Anthropic,
    );
    reg.upsert(meta.clone()).unwrap();
    reg.write_creds(&meta.id, &support::creds("seed-access-SECRET", 3600))
        .unwrap();
    let store = std::sync::Arc::new(pooltime::store::Store::open(&paths.db).unwrap());
    let state = StateBook::load(store).unwrap();
    let now = time::OffsetDateTime::now_utc();
    let window = |key: &str, u: f64| Window {
        key: key.into(),
        model: None,
        status: WindowStatus::Allowed,
        utilization: u,
        reset: Some(now + time::Duration::days(2)),
    };
    state.replace_poll_snapshot(
        &meta.id,
        RateLimitSnapshot {
            unified_status: Some("allowed".into()),
            windows: vec![window("5h", 0.1), window("7d", util)],
            updated_at: now,
        },
        now,
    );
    state.flush().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn status_prints_7d_left() {
    let h = Home::new();
    seed_account(&h.pt(), "alpha", 0.4).await;
    seed_account(&h.pt(), "beta", 0.9).await;
    tokio::task::block_in_place(|| {
        let _d = h.serve();
        h.cli(&["accounts", "add", "gamma"]).success(); // never logged in

        let out = stdout_of(&h.cli(&["status"]).success());
        // Pool line: 2 of 3 ready; 7-day headroom 0.6 + 0.1 = 0.7 of 3 accounts = 23%.
        assert!(out.contains("2/3 ready"), "{out}");
        assert!(out.contains("7d 23% left"), "{out}");
        let line = |name: &str| {
            out.lines()
                .find(|l| l.trim_start().starts_with(name))
                .unwrap_or_else(|| panic!("no row for {name} in\n{out}"))
                .to_string()
        };
        assert!(line("alpha").contains("60%"), "{out}");
        assert!(line("beta").contains("10%"), "{out}");
        assert!(line("gamma").contains("needs_login"), "{out}");
        assert!(out.contains("gamma needs login"), "{out}");
        assert!(!out.contains("SECRET"), "{out}");

        let v = status_json(&h);
        assert_eq!(v["pool"]["seven_day_left_total"].as_f64(), Some(0.7));
    });
}

fn settings_of(p: &Path) -> Value {
    serde_json::from_slice(&std::fs::read(p.join("settings.json")).unwrap()).unwrap()
}

#[test]
fn connect_disconnect_roundtrip_on_temp_profile() {
    let h = Home::new();
    let prof = h.dir.path().join(".claude-test");
    std::fs::create_dir_all(&prof).unwrap();
    let original = json!({"model": "opus", "env": {"KEEP": "1"}});
    std::fs::write(prof.join("settings.json"), original.to_string()).unwrap();

    // connect needs the pool running.
    h.cli(&["connect", "claude-test"])
        .failure()
        .stderr(predicate::str::contains("pooltime serve"));

    let d = h.serve();
    // No profile named: nothing is touched; the discovered ones are listed.
    h.cli(&["connect"])
        .failure()
        .stderr(predicate::str::contains("claude-test"));
    assert_eq!(settings_of(&prof), original);
    h.cli(&["connect", "nope"])
        .failure()
        .stderr(predicate::str::contains("unknown profile"));

    h.cli(&["connect", "claude-test"])
        .success()
        .stdout(predicate::str::contains("claude-test"));
    let s = settings_of(&prof);
    assert_eq!(
        s["env"]["ANTHROPIC_BASE_URL"],
        format!("http://127.0.0.1:{}", h.port)
    );
    assert_eq!(s["env"]["KEEP"], "1");
    assert!(h.config().contains(".claude-test"), "{}", h.config());

    h.cli(&["disconnect", "claude-test"]).success();
    assert_eq!(settings_of(&prof), original);
    assert!(!h.config().contains(".claude-test"));

    // Connected again, then disconnected (all connected profiles) with the pool down.
    h.cli(&["connect", "claude-test"]).success();
    drop(d);
    h.cli(&["disconnect"])
        .success()
        .stdout(predicate::str::contains("claude-test"));
    assert_eq!(settings_of(&prof), original);
    assert!(!h.config().contains(".claude-test"));
}

#[test]
fn connect_dot_claude_name_resolves_to_home_profile_not_cwd() {
    let h = Home::new();
    // The user's default profile ...
    let home_prof = h.dir.path().join(".claude");
    std::fs::create_dir_all(&home_prof).unwrap();
    std::fs::write(home_prof.join("settings.json"), r#"{"env":{}}"#).unwrap();
    // ... and a repository with its own (git-tracked) .claude/settings.json, used as the cwd.
    let repo = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(repo.path().join(".claude")).unwrap();
    let repo_settings = repo.path().join(".claude/settings.json");
    let repo_bytes = br#"{"permissions":{"allow":["Bash(ls)"]}}"#;
    std::fs::write(&repo_settings, repo_bytes).unwrap();

    let _d = h.serve();
    let run = |args: &[&str]| {
        let mut c = h.command(args);
        c.current_dir(repo.path());
        procs::assert(&mut c)
    };
    run(&["connect", ".claude"])
        .success()
        .stdout(predicate::str::contains("default"));
    assert_eq!(std::fs::read(&repo_settings).unwrap(), repo_bytes);
    assert_eq!(
        settings_of(&home_prof)["env"]["ANTHROPIC_BASE_URL"],
        format!("http://127.0.0.1:{}", h.port)
    );

    // $HOME itself is never a profile dir (its .claude.json belongs to ~/.claude), even with a
    // settings.json in it.
    std::fs::write(h.dir.path().join("settings.json"), "{}").unwrap();
    run(&["connect", h.dir.path().to_str().unwrap()])
        .failure()
        .stderr(predicate::str::contains("home directory"));
    assert_eq!(
        std::fs::read_to_string(h.dir.path().join("settings.json")).unwrap(),
        "{}"
    );
    run(&["disconnect", "default"]).success();
    assert_eq!(std::fs::read(&repo_settings).unwrap(), repo_bytes);
}

#[test]
fn disconnect_falls_back_offline_when_port_is_not_pooltime() {
    let h = Home::new();
    let prof = h.dir.path().join(".claude-test");
    std::fs::create_dir_all(&prof).unwrap();
    let original = json!({"env": {"KEEP": "1"}});
    std::fs::write(prof.join("settings.json"), original.to_string()).unwrap();
    let d = h.serve();
    h.cli(&["connect", "claude-test"]).success();
    drop(d);

    // Something that accepts connections but never answers holds the port now.
    let squatter = TcpListener::bind(("127.0.0.1", h.port)).unwrap();
    h.cli(&["disconnect"])
        .success()
        .stderr(predicate::str::contains("Unresponsive"))
        .stdout(predicate::str::contains("claude-test"));
    assert_eq!(settings_of(&prof), original);
    assert!(!h.config().contains(".claude-test"));
    drop(squatter);
}

const TOKEN_PATH: &str = "/v1/oauth/token";
const PROFILE_PATH: &str = "/api/oauth/profile";
const EMAIL: &str = "you@example.com";

async fn fake_signin_upstream() -> support::FakeAnthropic {
    use support::Reply;
    let fake = support::FakeAnthropic::start().await;
    fake.script(
        TOKEN_PATH,
        vec![Reply::json(
            200,
            &json!({
                "access_token": "signin-access-SECRET",
                "refresh_token": "signin-refresh-SECRET",
                "expires_in": 3600,
                "scope": "user:inference user:profile",
                "account": {"uuid": "uuid-1", "email_address": EMAIL},
                "organization": {"uuid": "org-1"}
            }),
        )],
    );
    fake.script(
        PROFILE_PATH,
        vec![Reply::json(
            200,
            &json!({
                "account": {"uuid": "uuid-1", "email": EMAIL},
                "organization": {"uuid": "org-1", "organization_type": "claude_max"}
            }),
        )],
    );
    fake
}

/// Run `pooltime <args>` headless (an SSH session): read stdout until the authorize URL appears,
/// paste `code#<state>`, confirm the binding, and return (exit success, all output, URL).
fn headless_login(h: &Home, args: &[&str]) -> (bool, String, String) {
    let mut child = procs::spawn(
        h.command(args)
            .env("SSH_TTY", "/dev/ttys999")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped()),
    );
    // Watchdog: a regression that leaves the CLI waiting must fail the test, not hang the suite.
    let (done, watch) = std::sync::mpsc::channel::<()>();
    let pid = child.id().to_string();
    let watchdog = std::thread::spawn(move || {
        if let Err(std::sync::mpsc::RecvTimeoutError::Timeout) = watch.recv_timeout(LOGIN_WATCHDOG)
        {
            let _ = procs::spawn(Command::new("kill").args(["-KILL", &pid])).wait();
        }
    });
    let mut stdin = child.stdin.take().unwrap();
    let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
    let mut seen = String::new();
    let url = loop {
        let Some(line) = lines.next() else {
            let mut err = String::new();
            child
                .stderr
                .take()
                .unwrap()
                .read_to_string(&mut err)
                .unwrap();
            panic!("no authorize URL in:\n{seen}\nstderr:\n{err}");
        };
        let line = line.unwrap();
        seen.push_str(&line);
        seen.push('\n');
        if let Some(i) = line.find("http") {
            if line.contains("code_challenge") {
                break line[i..].trim().to_string();
            }
        }
    };
    let state = url
        .split(['?', '&'])
        .find_map(|kv| kv.strip_prefix("state="))
        .unwrap()
        .to_string();
    writeln!(stdin, "the-code#{state}").unwrap();
    writeln!(stdin, "y").unwrap();
    for line in lines {
        seen.push_str(&line.unwrap());
        seen.push('\n');
    }
    let st = child.wait().unwrap();
    let _ = done.send(());
    watchdog.join().unwrap();
    let mut err = String::new();
    child
        .stderr
        .take()
        .unwrap()
        .read_to_string(&mut err)
        .unwrap();
    seen.push_str(&err);
    (st.success(), seen, url)
}

/// How long a scripted login may run before the watchdog kills it.
const LOGIN_WATCHDOG: Duration = Duration::from_secs(30);

fn assert_signed_in(h: &Home, fake: &support::FakeAnthropic, ok: bool, out: &str, url: &str) {
    assert!(ok, "login failed:\n{out}");
    assert!(url.contains("login_hint=you%40example.com"), "{url}");
    assert!(out.contains(EMAIL), "{out}");
    assert!(
        !out.contains("SECRET"),
        "tokens must never be printed:\n{out}"
    );
    let tok = fake.requests_to(TOKEN_PATH);
    assert_eq!(tok.len(), 1);
    assert_eq!(tok[0].json()["code"], "the-code");
    let creds = std::fs::read_to_string(h.pt().join("accounts/work/credentials.json")).unwrap();
    assert!(creds.contains("signin-access-SECRET"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn login_headless_prints_url_and_accepts_code() {
    let fake = fake_signin_upstream().await;
    let h = Home::with_upstream(&fake.url());
    tokio::task::block_in_place(|| {
        let _d = h.serve();
        let (ok, out, url) = headless_login(&h, &["accounts", "login", "work", "--email", EMAIL]);
        assert_signed_in(&h, &fake, ok, &out, &url);
        let a = &status_json(&h)["accounts"][0];
        assert_eq!(a["id"], "work");
        assert_eq!(a["status"], "ready");
        assert_eq!(a["identity"]["email"], EMAIL);
    });
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn login_runs_in_process_without_daemon() {
    let fake = fake_signin_upstream().await;
    let h = Home::with_upstream(&fake.url());
    let (ok, out, url) = tokio::task::block_in_place(|| {
        headless_login(&h, &["accounts", "login", "work", "--email", EMAIL])
    });
    assert_signed_in(&h, &fake, ok, &out, &url);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn spike_login_hint_reports_identity_and_saves_nothing() {
    let fake = fake_signin_upstream().await;
    let h = Home::with_upstream(&fake.url());
    let (ok, out, url) = tokio::task::block_in_place(|| {
        headless_login(&h, &["accounts", "login", "--spike-login-hint", EMAIL])
    });
    assert!(ok, "{out}");
    assert!(url.contains("login_hint=you%40example.com"), "{url}");
    assert!(out.contains(EMAIL) && out.contains("org-1"), "{out}");
    assert!(!out.contains("SECRET"), "{out}");
    let accounts = h.pt().join("accounts");
    assert!(
        !accounts.exists() || std::fs::read_dir(&accounts).unwrap().next().is_none(),
        "the spike must not save an account"
    );
}

#[test]
fn open_prints_dashboard_url() {
    let h = Home::new();
    h.cli(&["open"])
        .failure()
        .stderr(predicate::str::contains("pooltime serve"));
    let _d = h.serve();
    let out = stdout_of(&h.cli(&["open"]).success());
    assert_eq!(
        out.trim(),
        format!("http://127.0.0.1:{}/?k={}", h.port, h.mgmt_key())
    );
}

#[test]
fn logs_filters_by_request_id() {
    let h = Home::new();
    let logs = h.pt().join("logs");
    std::fs::create_dir_all(&logs).unwrap();
    std::fs::write(
        logs.join("pooltime.log.2026-11-06"),
        concat!(
            r#"{"timestamp":"2026-11-06T10:00:00Z","level":"INFO","fields":{"message":"older","request_id":"req_a"},"target":"pooltime"}"#,
            "\n"
        ),
    )
    .unwrap();
    std::fs::write(
        logs.join("pooltime.log.2026-11-07"),
        concat!(
            r#"{"timestamp":"2026-11-07T10:00:00Z","level":"INFO","fields":{"message":"hello"},"target":"pooltime","span":{"request_id":"req_b","name":"request"}}"#,
            "\n",
            r#"{"timestamp":"2026-11-07T10:00:01Z","level":"WARN","fields":{"message":"other","request_id":"req_c"},"target":"pooltime"}"#,
            "\n",
            "not json at all\n"
        ),
    )
    .unwrap();

    let all = stdout_of(&h.cli(&["logs"]).success());
    let older = all.find("older").expect("older line");
    let hello = all.find("hello").expect("hello line");
    assert!(older < hello, "files are read oldest first:\n{all}");
    assert!(
        all.contains("other") && all.contains("not json at all"),
        "{all}"
    );

    let one = stdout_of(&h.cli(&["logs", "--request", "req_b"]).success());
    assert!(one.contains("hello"), "{one}");
    assert!(!one.contains("older") && !one.contains("other"), "{one}");

    let raw = stdout_of(&h.cli(&["logs", "--request", "req_c", "--json"]).success());
    let v: Value = serde_json::from_str(raw.trim()).unwrap();
    assert_eq!(v["fields"]["message"], "other");
}
