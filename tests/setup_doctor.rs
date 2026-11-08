//! `pooltime setup` and `pooltime doctor`, run as the binary over a temp `HOME` /
//! `POOLTIME_HOME` / `XDG_CONFIG_HOME`, with `launchctl` / `systemctl` replaced by PATH shims and
//! every upstream URL pointed at a fake on 127.0.0.1 (never the network, never the real profiles).

mod support;

use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use pooltime::accounts::{AccountMeta, Registry};
use pooltime::claude_code::{apply, is_connected, profile_of, EnvBlock, Pins};
use pooltime::config::{Config, Paths, ProfileRef};
use pooltime::providers::ProviderId;
use serde_json::{json, Value};
use support::procs::{self, free_port};
use support::{FakeAnthropic, Reply};
use tempfile::TempDir;

const BOOT_BUDGET: Duration = Duration::from_secs(10);
/// How long a scripted setup may run before the watchdog kills it.
const WATCHDOG: Duration = Duration::from_secs(60);
const TOKEN_PATH: &str = "/v1/oauth/token";
const PROFILE_PATH: &str = "/api/oauth/profile";
const EMAIL: &str = "you@example.com";

fn health_ok(port: u16) -> bool {
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    let Ok(mut s) = TcpStream::connect_timeout(&addr, Duration::from_millis(500)) else {
        return false;
    };
    let _ = s.set_read_timeout(Some(Duration::from_secs(2)));
    if s.write_all(b"GET /health HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
        .is_err()
    {
        return false;
    }
    let mut raw = String::new();
    let _ = s.read_to_string(&mut raw);
    raw.starts_with("HTTP/1.1 200") && raw.contains(r#""service":"pooltime""#)
}

/// A temp machine: `root/home` is `$HOME`, `root/pt` the pooltime home, `root/shims` the PATH.
struct Machine {
    root: TempDir,
    port: u16,
}

impl Machine {
    fn new(upstream: &str) -> Machine {
        Machine::with(free_port(), upstream, "")
    }

    /// The pool on `port`, Anthropic URLs at `upstream`, plus `extra` TOML appended.
    fn with(port: u16, upstream: &str, extra: &str) -> Machine {
        let m = Machine {
            root: tempfile::tempdir().unwrap(),
            port,
        };
        fs::create_dir_all(m.home()).unwrap();
        let paths = m.paths();
        paths.ensure().unwrap();
        fs::write(
            &paths.config,
            format!(
                "port = {port}\n\n[anthropic]\napi_base = \"{upstream}\"\n\
                 token_url = \"{upstream}/v1/oauth/token\"\n\
                 profile_url = \"{upstream}/api/oauth/profile\"\nusage_refresh = false\n{extra}",
                port = m.port
            ),
        )
        .unwrap();
        fs::set_permissions(&paths.config, fs::Permissions::from_mode(0o600)).unwrap();
        m.write_shims();
        m
    }

    fn home(&self) -> PathBuf {
        self.root.path().join("home")
    }

    fn pt(&self) -> PathBuf {
        self.root.path().join("pt")
    }

    fn paths(&self) -> Paths {
        Paths::under(&self.pt())
    }

    fn shims(&self) -> PathBuf {
        self.root.path().join("shims")
    }

    fn calls(&self) -> String {
        fs::read_to_string(self.root.path().join("calls.log")).unwrap_or_default()
    }

    /// Stateful `launchctl` / `systemctl`: log their arguments, track "loaded" in a state file,
    /// and report a loaded service as running.
    fn write_shims(&self) {
        let dir = self.shims();
        fs::create_dir_all(&dir).unwrap();
        let state = self.root.path().join("loaded");
        let log = self.root.path().join("calls.log");
        let scripts = [
            (
                "launchctl",
                r#"case "$1" in
  bootstrap) touch "$STATE" ;;
  bootout) if [ -f "$STATE" ]; then rm "$STATE"; else echo "Could not find service" >&2; exit 113; fi ;;
  print) if [ -f "$STATE" ]; then echo "	state = running"; else exit 113; fi ;;
esac
exit 0"#,
            ),
            (
                "systemctl",
                r#"case "$2" in
  enable) touch "$STATE" ;;
  disable) rm -f "$STATE" ;;
  is-active|status) if [ -f "$STATE" ]; then echo active; else echo inactive; exit 3; fi ;;
esac
exit 0"#,
            ),
        ];
        for (tool, body) in scripts {
            let script = dir.join(tool);
            fs::write(
                &script,
                format!(
                    "#!/bin/sh\nSTATE=\"{}\"\necho \"{tool} $*\" >> \"{}\"\n{body}\n",
                    state.display(),
                    log.display()
                ),
            )
            .unwrap();
            fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
        }
    }

    /// `pooltime <args>` with nothing inherited but TMPDIR: the temp homes, the shims ahead of PATH,
    /// never a browser, no `POOLTIME_*` / `SSH_TTY` / `DISPLAY` from the test's environment.
    fn command(&self, args: &[&str]) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_pooltime"));
        c.args(args)
            .env_clear()
            // The shims first; the system dirs only for the shims' own `touch` / `rm`.
            .env("PATH", format!("{}:/usr/bin:/bin", self.shims().display()))
            .env("TMPDIR", std::env::temp_dir())
            .env("HOME", self.home())
            .env("POOLTIME_HOME", self.pt())
            .env("XDG_CONFIG_HOME", self.home().join(".config"))
            .env("POOLTIME_NO_BROWSER", "1")
            // Never a real desktop notification from a test daemon.
            .env("POOLTIME_ALERTS_NOTIFY", "false");
        c
    }

    fn run(&self, args: &[&str]) -> (bool, String) {
        let out = procs::output(self.command(args).stdin(Stdio::null()));
        let mut s = String::from_utf8_lossy(&out.stdout).into_owned();
        s.push_str(&String::from_utf8_lossy(&out.stderr));
        (out.status.success(), s)
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
            if health_ok(self.port) {
                return Served(child);
            }
            assert!(start.elapsed() < BOOT_BUDGET, "daemon did not come up");
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// A Claude Code profile dir `home/<name>` with a `settings.json`.
    fn profile(&self, name: &str) -> PathBuf {
        let dir = self.home().join(name);
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("settings.json"),
            json!({"model": "opus", "env": {"KEEP": "1"}}).to_string(),
        )
        .unwrap();
        dir
    }

    /// As if `service install` had run: the definition exists and the shim reports it loaded.
    fn mark_service_installed(&self) {
        let def = self.definition();
        fs::create_dir_all(def.parent().unwrap()).unwrap();
        fs::write(&def, "x").unwrap();
        fs::write(self.root.path().join("loaded"), "").unwrap();
    }

    /// An account on disk with credentials (the pool not running).
    fn seed_account(&self, name: &str, provider: ProviderId, refresh_expires_in_days: Option<i64>) {
        let paths = self.paths();
        let reg = Registry::load(&paths).unwrap();
        let meta = AccountMeta::new(name.parse().unwrap(), provider);
        reg.upsert(meta.clone()).unwrap();
        let mut c = support::creds("seed-access-SECRET", 3600);
        c.provider = provider;
        c.refresh_expires_at = refresh_expires_in_days
            .map(|d| time::OffsetDateTime::now_utc() + time::Duration::days(d));
        reg.write_creds(&meta.id, &c).unwrap();
    }

    fn definition(&self) -> PathBuf {
        if cfg!(target_os = "macos") {
            self.home()
                .join("Library/LaunchAgents/dev.pooltime.daemon.plist")
        } else {
            self.home().join(".config/systemd/user/pooltime.service")
        }
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

async fn fake_upstream() -> FakeAnthropic {
    let fake = FakeAnthropic::start().await;
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

/// Run `pooltime <args>`, paste `code#<state>` once the authorize URL shows, and collect all
/// output. Returns (exit success, output).
fn run_with_signin(m: &Machine, args: &[&str]) -> (bool, String) {
    let mut child = procs::spawn(
        m.command(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped()),
    );
    let (done, watch) = std::sync::mpsc::channel::<()>();
    let pid = child.id().to_string();
    let watchdog = std::thread::spawn(move || {
        if let Err(std::sync::mpsc::RecvTimeoutError::Timeout) = watch.recv_timeout(WATCHDOG) {
            let _ = procs::spawn(Command::new("/bin/kill").args(["-KILL", &pid])).wait();
        }
    });
    let mut stdin = child.stdin.take().unwrap();
    let mut err_pipe = child.stderr.take().unwrap();
    let stderr = std::thread::spawn(move || {
        let mut s = String::new();
        let _ = err_pipe.read_to_string(&mut s);
        s
    });
    let mut seen = String::new();
    for line in BufReader::new(child.stdout.take().unwrap()).lines() {
        let line = line.unwrap();
        if let Some(i) = line.find("http") {
            if line.contains("code_challenge") {
                let state = line[i..]
                    .trim()
                    .split(['?', '&'])
                    .find_map(|kv| kv.strip_prefix("state="))
                    .unwrap()
                    .to_string();
                writeln!(stdin, "the-code#{state}").unwrap();
            }
        }
        seen.push_str(&line);
        seen.push('\n');
    }
    drop(stdin);
    let st = child.wait().unwrap();
    let _ = done.send(());
    watchdog.join().unwrap();
    seen.push_str(&stderr.join().unwrap());
    (st.success(), seen)
}

fn settings(dir: &Path) -> Value {
    serde_json::from_slice(&fs::read(dir.join("settings.json")).unwrap()).unwrap()
}

/// The output line of the check/step whose text contains `needle`.
fn line_with<'a>(out: &'a str, needle: &str) -> &'a str {
    out.lines()
        .find(|l| l.contains(needle))
        .unwrap_or_else(|| panic!("no line containing {needle:?} in:\n{out}"))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn setup_noninteractive_full_path() {
    let fake = fake_upstream().await;
    let m = Machine::new(&fake.url());
    let chosen = m.profile(".claude-test");
    let other = m.profile(".claude-other");
    let other_before = fs::read(other.join("settings.json")).unwrap();

    tokio::task::block_in_place(|| {
        // The service manager is a shim, so the test runs the daemon the service would start.
        let _d = m.serve();
        let args = [
            "setup",
            "--yes",
            "--account",
            "work",
            "--email",
            EMAIL,
            "--mode",
            "paste",
            "--service",
            "--connect",
            "claude-test",
        ];
        let (ok, out) = run_with_signin(&m, &args);
        assert!(ok, "setup failed:\n{out}");
        assert!(
            !out.contains("SECRET"),
            "tokens must never be printed:\n{out}"
        );

        // Step 1: no bro install, so the first account is signed in.
        assert!(line_with(&out, "Signed in").contains(EMAIL), "{out}");
        assert_eq!(fake.requests_to(TOKEN_PATH).len(), 1);
        // Step 2: the service definition is written and loaded.
        assert!(m.definition().exists(), "{out}");
        let loads = |calls: &str| {
            calls
                .lines()
                .filter(|l| l.contains("bootstrap") || l.contains("enable"))
                .count()
        };
        assert_eq!(loads(&m.calls()), 1, "{}", m.calls());
        // Step 3: only the chosen profile is connected.
        let paths = m.paths();
        let p = profile_of(&ProfileRef {
            id: "claude-test".into(),
            settings_dir: chosen.clone(),
        });
        assert!(is_connected(&p, &paths), "{out}");
        assert_eq!(
            settings(&chosen)["env"]["ANTHROPIC_BASE_URL"],
            format!("http://127.0.0.1:{}", m.port)
        );
        assert_eq!(fs::read(other.join("settings.json")).unwrap(), other_before);
        // Step 5: doctor ran and passed; the dashboard URL is printed.
        assert!(line_with(&out, "health:").starts_with('✓'), "{out}");
        assert!(line_with(&out, "account work:").starts_with('✓'), "{out}");
        assert!(
            line_with(&out, "profile claude-test:").starts_with('✓'),
            "{out}"
        );
        assert!(!out.lines().any(|l| l.starts_with('✗')), "{out}");
        assert!(
            out.contains(&format!("http://127.0.0.1:{}/?k=", m.port)),
            "{out}"
        );

        // Idempotent: everything done, a re-run changes nothing.
        let settings_before = fs::read(chosen.join("settings.json")).unwrap();
        let claude_json_before = fs::read(chosen.join(".claude.json")).ok();
        let config_before = fs::read(&paths.config).unwrap();
        let (ok, again) = m.run(&args);
        assert!(ok, "re-run failed:\n{again}");
        assert!(!again.contains("Signed in"), "{again}");
        assert!(
            line_with(&again, "accounts:").contains("already"),
            "{again}"
        );
        assert!(line_with(&again, "service:").contains("already"), "{again}");
        assert_eq!(fake.requests_to(TOKEN_PATH).len(), 1, "no second sign-in");
        assert_eq!(loads(&m.calls()), 1, "no reinstall:\n{}", m.calls());
        assert_eq!(
            fs::read(chosen.join("settings.json")).unwrap(),
            settings_before
        );
        assert_eq!(fs::read(&paths.config).unwrap(), config_before);
        assert_eq!(
            fs::read(chosen.join(".claude.json")).ok(),
            claude_json_before
        );
        assert_eq!(fs::read(other.join("settings.json")).unwrap(), other_before);
        assert!(!again.contains("status line"), "{again}");
    });
}

/// Connect `home/.claude-test` offline (the env block plus the config record), as if the pool had
/// connected it and then gone away.
fn connect_offline(m: &Machine) -> PathBuf {
    let dir = m.profile(".claude-test");
    let r = ProfileRef {
        id: "claude-test".into(),
        settings_dir: dir.clone(),
    };
    let paths = m.paths();
    let block = EnvBlock {
        base_url: format!("http://127.0.0.1:{}", m.port),
        auth_token: "k".into(),
        pins: Pins::default(),
        max_context: None,
    };
    apply(&profile_of(&r), &block, &paths).unwrap();
    let mut cfg = Config::load_file(&paths).unwrap();
    cfg.profiles.push(r);
    cfg.save(&paths).unwrap();
    dir
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn doctor_flags_connected_but_pool_down() {
    let fake = fake_upstream().await;
    let m = Machine::new(&fake.url());
    connect_offline(&m);

    let (ok, out) = tokio::task::block_in_place(|| m.run(&["doctor"]));
    assert!(!ok, "doctor must exit 1 on a ✗:\n{out}");
    let l = line_with(&out, "profile claude-test:");
    assert!(l.starts_with('✗'), "{out}");
    assert!(l.contains("connected, pool down"), "{out}");
    assert!(l.contains("pooltime disconnect"), "{out}");
    let health = line_with(&out, "health:");
    assert!(
        health.starts_with('✗') && health.contains("not running"),
        "{out}"
    );
    // The fake answers, so upstream reachability is fine; the doctor wrote nothing.
    assert!(line_with(&out, "upstream ").starts_with('✓'), "{out}");
    assert!(
        !m.paths().db.exists(),
        "doctor must not create the database"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn doctor_flags_bad_permissions() {
    let fake = fake_upstream().await;
    let m = Machine::new(&fake.url());
    let paths = m.paths();
    fs::set_permissions(&paths.home, fs::Permissions::from_mode(0o755)).unwrap();
    fs::set_permissions(&paths.config, fs::Permissions::from_mode(0o644)).unwrap();

    let (ok, out) = tokio::task::block_in_place(|| m.run(&["doctor"]));
    assert!(!ok, "{out}");
    let dir = line_with(&out, &format!("{} ", paths.home.display()));
    assert!(dir.starts_with('✗'), "{out}");
    assert!(
        dir.contains(&format!("chmod 700 {}", paths.home.display())),
        "{out}"
    );
    let cfg = line_with(&out, &format!("{} ", paths.config.display()));
    assert!(cfg.starts_with('✗'), "{out}");
    assert!(
        cfg.contains(&format!("chmod 600 {}", paths.config.display())),
        "{out}"
    );
    // Fixed, the permission lines pass.
    fs::set_permissions(&paths.home, fs::Permissions::from_mode(0o700)).unwrap();
    fs::set_permissions(&paths.config, fs::Permissions::from_mode(0o600)).unwrap();
    let (_, out) = tokio::task::block_in_place(|| m.run(&["doctor"]));
    assert!(line_with(&out, "permissions").starts_with('✓'), "{out}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn doctor_trusts_a_ready_account_past_its_recorded_expiry() {
    let fake = fake_upstream().await;
    let m = Machine::new(&fake.url());
    m.seed_account("work", ProviderId::Anthropic, Some(-2));
    m.mark_service_installed();

    let (ok, out) = tokio::task::block_in_place(|| {
        let _d = m.serve();
        // Wait for the boot health tick's probe to land (it runs off the request path).
        let reg = Registry::load(&m.paths()).unwrap();
        let id = "work".parse().unwrap();
        let start = Instant::now();
        while reg
            .read_creds(&id)
            .unwrap()
            .unwrap()
            .refresh_expires_at
            .is_some()
        {
            assert!(start.elapsed() < BOOT_BUDGET, "expiry probe never ran");
            std::thread::sleep(Duration::from_millis(20));
        }
        m.run(&["doctor"])
    });
    // The daemon's expiry probe (health tick, at boot) refreshed the account successfully,
    // which disproves the passed expiry and replaces it (Task F1): doctor sees a ready account.
    let l = line_with(&out, "account work:");
    assert!(l.starts_with('✓'), "{out}");
    assert!(l.contains("ready"), "{out}");
    assert!(!out.lines().any(|l| l.starts_with('✗')), "{out}");
    assert!(ok, "{out}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn doctor_fails_when_the_pool_status_is_unreadable() {
    // A "pool" that passes the health identity check but cannot produce its status.
    let fake = FakeAnthropic::start().await;
    fake.script(
        "/health",
        vec![Reply::json(
            200,
            &json!({"service": "pooltime", "status": "ok"}),
        )],
    );
    fake.script(
        "/api/v1/status",
        vec![Reply::error(500, "api_error", "boom")],
    );
    let port: u16 = fake.url().rsplit(':').next().unwrap().parse().unwrap();
    let m = Machine::with(port, &fake.url(), "");
    m.seed_account("work", ProviderId::Anthropic, None);

    let (ok, out) = tokio::task::block_in_place(|| m.run(&["doctor"]));
    assert!(!ok, "{out}");
    assert!(line_with(&out, "health:").starts_with('✓'), "{out}");
    let l = line_with(&out, "accounts:");
    assert!(l.starts_with('✗'), "{out}");
    assert!(l.contains("could not read pool status"), "{out}");
    // No silent fallback to the files.
    assert!(!out.contains("account work:"), "{out}");
    assert!(!out.contains("pool not running"), "{out}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn doctor_probes_codex_hosts_only_with_an_openai_account() {
    let fake = fake_upstream().await;
    let codex = FakeAnthropic::start().await;
    let closed = "http://127.0.0.1:9";
    let m = Machine::with(
        free_port(),
        &fake.url(),
        &format!(
            "\n[openai]\napi_base = \"{}/backend-api/codex\"\ntoken_url = \"{closed}/oauth/token\"\n",
            codex.url()
        ),
    );
    m.seed_account("work", ProviderId::Anthropic, None);

    let (_, out) = tokio::task::block_in_place(|| m.run(&["doctor"]));
    assert!(!out.contains(&codex.url()), "{out}");
    assert!(!out.contains(closed), "{out}");

    m.seed_account("codex1", ProviderId::OpenAi, None);
    let (ok, out) = tokio::task::block_in_place(|| m.run(&["doctor"]));
    assert!(!ok, "{out}");
    let l = line_with(&out, &format!("upstream {}:", codex.url()));
    assert!(l.starts_with('✓'), "{out}");
    let l = line_with(&out, &format!("upstream {closed}:"));
    assert!(l.starts_with('✗'), "{out}");
}
