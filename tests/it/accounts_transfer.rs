//! `pooltime accounts export|import` end to end, through the binary, over temp homes (never the
//! real `$HOME`) and ports nothing else uses. No network: the upstreams point at the discard
//! port and usage polling is off.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use predicates::prelude::*;
use serde_json::Value;
use tempfile::TempDir;

use crate::support::procs::{self, free_port};

const DEAD_UPSTREAM: &str = "http://127.0.0.1:9";
const PASSPHRASE: &str = "correct horse battery staple";

/// A temp HOME with a pooltime home under `pt/` on its own port.
struct Home {
    dir: TempDir,
    port: u16,
}

impl Home {
    fn new() -> Home {
        let h = Home {
            dir: tempfile::tempdir().unwrap(),
            port: free_port(),
        };
        std::fs::create_dir_all(h.pt()).unwrap();
        h
    }

    fn pt(&self) -> PathBuf {
        self.dir.path().join("pt")
    }

    fn file(&self, name: &str) -> PathBuf {
        self.dir.path().join(name)
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_pooltime"));
        c.args(args)
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("TMPDIR", std::env::temp_dir())
            .env("HOME", self.dir.path())
            .env("POOLTIME_HOME", self.pt())
            // Pinned here (not only in config.toml): an import may restore another port.
            .env("POOLTIME_PORT", self.port.to_string())
            .env("POOLTIME_NO_BROWSER", "1")
            .env("POOLTIME_ALERTS_NOTIFY", "false")
            .stdin(Stdio::null());
        c
    }

    fn cli(&self, args: &[&str]) -> assert_cmd::assert::Assert {
        procs::assert(&mut self.command(args))
    }

    fn passphrase_file(&self, pw: &str) -> String {
        let p = self.file(&format!("pw-{}", pw.len()));
        std::fs::write(&p, format!("{pw}\n")).unwrap();
        p.to_string_lossy().into_owned()
    }

    fn export(&self, out: &Path, extra: &[&str]) -> assert_cmd::assert::Assert {
        let pw = self.passphrase_file(PASSPHRASE);
        let out = out.to_string_lossy();
        let mut args = vec!["accounts", "export", &out, "--passphrase-file", &pw];
        args.extend_from_slice(extra);
        self.cli(&args)
    }

    fn import(&self, file: &Path, pw: &str, extra: &[&str]) -> assert_cmd::assert::Assert {
        let pw = self.passphrase_file(pw);
        let file = file.to_string_lossy();
        let mut args = vec!["accounts", "import", &file, "--passphrase-file", &pw];
        args.extend_from_slice(extra);
        self.cli(&args)
    }

    fn account_toml(&self, id: &str) -> String {
        std::fs::read_to_string(self.pt().join("accounts").join(id).join("account.toml")).unwrap()
    }

    fn creds(&self, id: &str) -> Option<Value> {
        let p = self.pt().join("accounts").join(id).join("credentials.json");
        std::fs::read(p)
            .ok()
            .map(|b| serde_json::from_slice(&b).unwrap())
    }

    fn write_config(&self) {
        std::fs::write(
            self.pt().join("config.toml"),
            format!(
                "port = {}\n\n[anthropic]\napi_base = \"{DEAD_UPSTREAM}\"\n\
                 token_url = \"{DEAD_UPSTREAM}/v1/oauth/token\"\nusage_refresh = false\n",
                self.port
            ),
        )
        .unwrap();
    }

    fn serve(&self) -> Served {
        let mut child = procs::spawn(
            self.command(&["serve", "--i-stopped-bro"])
                .stdout(Stdio::null())
                .stderr(Stdio::null()),
        );
        let start = Instant::now();
        loop {
            if let Some(st) = child.try_wait().unwrap() {
                panic!("daemon exited early: {st}");
            }
            if self.cli(&["status", "--json"]).try_success().is_ok() {
                return Served(child);
            }
            assert!(
                start.elapsed() < Duration::from_secs(10),
                "daemon did not come up"
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

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

fn mode_of(p: &Path) -> u32 {
    std::fs::metadata(p).unwrap().permissions().mode() & 0o777
}

/// The old machine: a Claude account with a live sign-in and custom routing, a Codex account
/// whose sign-in has expired, tuning, a Claude Code profile record and a models.toml.
fn source() -> Home {
    let h = Home::new();
    let acc = h.pt().join("accounts");
    std::fs::create_dir_all(acc.join("acct-a")).unwrap();
    std::fs::create_dir_all(acc.join("acct-b")).unwrap();
    std::fs::write(
        acc.join("acct-a/account.toml"),
        "id = \"acct-a\"\nprovider = \"anthropic\"\npriority = 5\nweight = 2.5\n\
         [identity]\nemail = \"user@example.com\"\n",
    )
    .unwrap();
    std::fs::write(
        acc.join("acct-a/credentials.json"),
        r#"{"provider":"anthropic","access_token":"SECRET-ACCESS-A",
            "refresh_token":"SECRET-REFRESH-A","expires_at":"2020-01-01T00:00:00Z"}"#,
    )
    .unwrap();
    std::fs::write(
        acc.join("acct-b/account.toml"),
        "id = \"acct-b\"\nprovider = \"openai\"\n",
    )
    .unwrap();
    std::fs::write(
        acc.join("acct-b/credentials.json"),
        r#"{"provider":"openai","access_token":"SECRET-ACCESS-B",
            "expires_at":"2020-01-01T00:00:00Z"}"#,
    )
    .unwrap();
    std::fs::write(
        h.pt().join("config.toml"),
        format!(
            "port = {}\n[routing]\nmin_headroom = 0.25\n[[profiles]]\nid = \"main\"\n\
             settings_dir = \"/old-machine/.claude\"\n",
            h.port
        ),
    )
    .unwrap();
    std::fs::write(h.pt().join("models.toml"), "mapping_enabled = true\n").unwrap();
    h
}

#[test]
fn export_then_import_restores_accounts_and_settings() {
    let src = source();
    let out = src.file("pool.age");
    let a = src.export(&out, &[]).success();
    let text = stdout_of(&a);
    assert!(text.contains("exported 2 accounts"), "{text}");
    assert!(text.contains("refresh tokens"), "{text}");
    assert!(text.contains("pooltime service uninstall"), "{text}");
    assert!(!text.contains("example.com"), "{text}");
    assert_eq!(mode_of(&out), 0o600);
    let raw = std::fs::read(&out).unwrap();
    let raw = String::from_utf8_lossy(&raw);
    assert!(raw.starts_with("age-encryption.org/v1"));
    assert!(!raw.contains("SECRET") && !raw.contains("example.com"));

    // Never overwritten without --force.
    std::fs::write(&out, "keep me").unwrap();
    src.export(&out, &[])
        .failure()
        .stderr(predicate::str::contains("--force"));
    assert_eq!(std::fs::read_to_string(&out).unwrap(), "keep me");
    src.export(&out, &["--force"]).success();
    assert_eq!(mode_of(&out), 0o600);

    let dst = Home::new();
    let a = dst.import(&out, PASSPHRASE, &[]).success();
    let text = stdout_of(&a);
    assert!(
        text.contains("2 accounts (1 Claude, 1 ChatGPT/Codex)"),
        "{text}"
    );
    assert!(text.contains("acct-a (anthropic): imported"), "{text}");
    assert!(
        text.contains("acct-b (openai): imported, needs sign-in"),
        "{text}"
    );
    assert!(text.contains("pooltime accounts login acct-b"), "{text}");
    assert!(text.contains("Delete"), "{text}");
    assert!(
        !text.contains("example.com") && !text.contains("SECRET"),
        "{text}"
    );

    let meta = dst.account_toml("acct-a");
    assert!(
        meta.contains("priority = 5") && meta.contains("weight = 2.5"),
        "{meta}"
    );
    assert!(meta.contains("user@example.com"), "identity kept: {meta}");
    let creds = dst.creds("acct-a").expect("live sign-in restored");
    assert_eq!(creds["refresh_token"], "SECRET-REFRESH-A");
    assert_eq!(creds["access_token"], "SECRET-ACCESS-A");
    // An expired login is not restored.
    assert!(dst.creds("acct-b").is_none());
    for f in [
        "accounts/acct-a/credentials.json",
        "accounts/acct-a/account.toml",
        "config.toml",
        "models.toml",
    ] {
        assert_eq!(mode_of(&dst.pt().join(f)), 0o600, "{f}");
    }
    let cfg = std::fs::read_to_string(dst.pt().join("config.toml")).unwrap();
    assert!(cfg.contains("min_headroom = 0.25"), "{cfg}");
    assert!(!cfg.contains("old-machine"), "profiles stay behind: {cfg}");
    assert_eq!(
        std::fs::read_to_string(dst.pt().join("models.toml")).unwrap(),
        "mapping_enabled = true\n"
    );
}

#[test]
fn wrong_passphrase_tampering_and_missing_passphrase_fail_cleanly() {
    let src = source();
    let out = src.file("pool.age");
    src.export(&out, &[]).success();
    let dst = Home::new();

    dst.import(&out, "not the passphrase", &[])
        .failure()
        .stderr(predicate::str::contains("wrong passphrase"));
    let mut bytes = std::fs::read(&out).unwrap();
    let n = bytes.len();
    bytes[n - 5] ^= 0x40;
    let bad = dst.file("tampered.age");
    std::fs::write(&bad, &bytes).unwrap();
    dst.import(&bad, PASSPHRASE, &[])
        .failure()
        .stderr(predicate::str::contains("changed or damaged"));
    let junk = dst.file("junk.age");
    std::fs::write(&junk, "not an export").unwrap();
    dst.import(&junk, PASSPHRASE, &[])
        .failure()
        .stderr(predicate::str::contains("not a pooltime export"));
    assert!(
        !dst.pt().join("accounts/acct-a").exists(),
        "nothing written"
    );

    // No passphrase source and no terminal; an empty passphrase is refused.
    let o = out.to_string_lossy();
    dst.cli(&["accounts", "import", &o])
        .failure()
        .stderr(predicate::str::contains("--passphrase-file"));
    dst.import(&out, "", &[])
        .failure()
        .stderr(predicate::str::contains("must not be empty"));
    let fresh = src.file("other.age");
    let empty = src.passphrase_file("");
    src.cli(&[
        "accounts",
        "export",
        &fresh.to_string_lossy(),
        "--passphrase-file",
        &empty,
    ])
    .failure()
    .stderr(predicate::str::contains("must not be empty"));
    assert!(!fresh.exists());
    // The env var works for automation.
    let mut c = dst.command(&["accounts", "import", &o]);
    c.env("POOLTIME_EXPORT_PASSPHRASE", PASSPHRASE);
    procs::assert(&mut c).success();
    assert!(dst.creds("acct-a").is_some());
}

#[test]
fn existing_accounts_are_skipped_unless_replace() {
    let src = source();
    let out = src.file("pool.age");
    src.export(&out, &[]).success();
    let dst = Home::new();
    dst.import(&out, PASSPHRASE, &[]).success();

    // Changed here after the first import.
    let p = dst.pt().join("accounts/acct-a/account.toml");
    std::fs::write(
        &p,
        "id = \"acct-a\"\nprovider = \"anthropic\"\nweight = 9.0\n",
    )
    .unwrap();
    std::fs::write(dst.pt().join("models.toml"), "mapping_enabled = false\n").unwrap();
    let text = stdout_of(&dst.import(&out, PASSPHRASE, &[]).success());
    assert!(text.contains("acct-a (anthropic): skipped"), "{text}");
    assert!(text.contains("imported 0 of 2"), "{text}");
    assert!(text.contains("models.toml: kept"), "{text}");
    assert!(dst.account_toml("acct-a").contains("weight = 9.0"));

    let text = stdout_of(&dst.import(&out, PASSPHRASE, &["--replace"]).success());
    assert!(text.contains("imported 2 of 2"), "{text}");
    assert!(dst.account_toml("acct-a").contains("weight = 2.5"));
    assert_eq!(
        std::fs::read_to_string(dst.pt().join("models.toml")).unwrap(),
        "mapping_enabled = true\n"
    );
}

#[test]
fn import_into_a_running_daemon_serves_the_accounts_at_once() {
    let src = source();
    let out = src.file("pool.age");
    src.export(&out, &[]).success();

    let dst = Home::new();
    dst.write_config();
    let _d = dst.serve();
    let text = stdout_of(&dst.import(&out, PASSPHRASE, &[]).success());
    assert!(text.contains("imported 2 of 2"), "{text}");
    // The daemon's own config.toml is kept (it pins the test port and dead upstreams).
    assert!(text.contains("config.toml: kept"), "{text}");

    let a = dst.cli(&["accounts", "list", "--json"]).success();
    let list: Value = serde_json::from_str(&stdout_of(&a)).unwrap();
    let by_id = |id: &str| {
        list.as_array()
            .unwrap()
            .iter()
            .find(|a| a["id"] == id)
            .cloned()
            .unwrap_or_else(|| panic!("{id} missing: {list}"))
    };
    let a = by_id("acct-a");
    assert_ne!(a["status"], "needs_login", "{a}");
    assert_eq!(a["weight"], 2.5, "{a}");
    assert_eq!(by_id("acct-b")["status"], "needs_login");

    // --replace goes through the daemon's delete + create.
    let text = stdout_of(&dst.import(&out, PASSPHRASE, &["--replace"]).success());
    assert!(text.contains("imported 2 of 2"), "{text}");
    assert!(dst.creds("acct-a").is_some());
}
