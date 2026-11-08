//! `pooltime import bro` against a synthetic bro tree (`tests/support/fixtures/bro`, FAKE tokens)
//! copied into a temp dir. A fake [`Keychain`] stands in for macOS's; nothing here touches the real
//! `$HOME`, bro's real files or the Keychain.

mod support;

use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use pooltime::accounts::{AccountId, AccountMeta, AccountStatus, Registry, StateBook};
use pooltime::config::{AnthropicCfg, Config, Paths};
use pooltime::import::bro::{
    apply_import, fill_identities, plan_import, AccountOutcome, IdentityState, ImportReport,
    ItemOutcome, Keychain,
};
use pooltime::models::ModelTable;
use pooltime::providers::ProviderId;
use pooltime::routing::sessions::Sessions;
use pooltime::store::Store;
use pooltime::upstream::Upstream;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use support::procs::{self, free_port};
use support::{FakeAnthropic, Reply};
use time::OffsetDateTime;

const FIXTURE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/support/fixtures/bro");

/// Synthetic bro install inside a temp `$HOME`, plus an empty pooltime home next to it.
struct Fx {
    dir: tempfile::TempDir,
    home: PathBuf,
    pool: PathBuf,
    bro_home: PathBuf,
    paths: Paths,
}

fn copy_tree(from: &Path, to: &Path) {
    fs::create_dir_all(to).unwrap();
    for e in fs::read_dir(from).unwrap() {
        let e = e.unwrap();
        let dest = to.join(e.file_name());
        if e.file_type().unwrap().is_dir() {
            copy_tree(&e.path(), &dest);
        } else {
            fs::copy(e.path(), &dest).unwrap();
        }
    }
}

fn fixture() -> Fx {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    let pool = home.join(".claude-max-pool");
    let bro_home = home.join(".bro");
    copy_tree(&Path::new(FIXTURE).join("claude-max-pool"), &pool);
    copy_tree(&Path::new(FIXTURE).join("bro-home"), &bro_home);
    let paths = Paths::under(&dir.path().join("pt"));
    Fx {
        dir,
        home,
        pool,
        bro_home,
        paths,
    }
}

#[derive(Default)]
struct FakeKeychain {
    items: HashMap<String, String>,
    calls: Mutex<Vec<String>>,
}

impl Keychain for FakeKeychain {
    fn read(&self, service: &str) -> anyhow::Result<Option<String>> {
        self.calls.lock().unwrap().push(service.to_string());
        Ok(self.items.get(service).cloned())
    }
}

fn sha8(s: &str) -> String {
    hex::encode(Sha256::digest(s.as_bytes()))[..8].to_string()
}

/// bro's service name, computed independently: sha256 of the account config dir string.
fn kc1_service(fx: &Fx) -> String {
    let dir = fx.pool.join("accounts").join("kc1");
    format!("Claude Code-credentials-{}", sha8(dir.to_str().unwrap()))
}

fn kc1_keychain(fx: &Fx) -> FakeKeychain {
    let blob = json!({"claudeAiOauth": {
        "accessToken": "fake-access-kc1",
        "refreshToken": "fake-refresh-kc1",
        "expiresAt": 1762520400000_i64,
        "scopes": ["user:inference"],
        "subscriptionType": "max"
    }});
    FakeKeychain {
        items: HashMap::from([(kc1_service(fx), blob.to_string())]),
        ..Default::default()
    }
}

fn import(fx: &Fx, kc: &FakeKeychain) -> ImportReport {
    let plan = plan_import(&fx.pool, &fx.bro_home, &fx.home).unwrap();
    apply_import(&plan, &fx.paths, kc).unwrap()
}

fn id(s: &str) -> AccountId {
    s.parse().unwrap()
}

fn ts(secs: i64) -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(secs).unwrap()
}

fn assert_no_secrets(s: &str) {
    for needle in ["fake-access", "fake-refresh", "user_fakehash"] {
        assert!(!s.contains(needle), "output leaks {needle}: {s}");
    }
}

fn skipped_reason(r: &ImportReport, name: &str) -> String {
    match &r
        .account(name)
        .unwrap_or_else(|| panic!("{name} not in report"))
        .outcome
    {
        AccountOutcome::Skipped { reason } => reason.clone(),
        other => panic!("{name} should be skipped, got {other:?}"),
    }
}

fn state(fx: &Fx) -> StateBook {
    StateBook::load(Arc::new(Store::open(&fx.paths.db).unwrap())).unwrap()
}

#[test]
fn imports_accounts_creds_and_routing() {
    let fx = fixture();
    let report = import(&fx, &FakeKeychain::default());
    let reg = Registry::load(&fx.paths).unwrap();
    let ids: Vec<String> = reg.list().iter().map(|m| m.id.to_string()).collect();
    assert_eq!(ids, ["alpha", "beta", "codex1", "dead1", "stale1"]);

    let alpha = reg.get(&id("alpha")).unwrap();
    assert_eq!(alpha.provider, ProviderId::Anthropic);
    assert_eq!((alpha.priority, alpha.weight), (10, 2.5));
    assert!(alpha.imported);
    let c = reg.read_creds(&id("alpha")).unwrap().unwrap();
    assert_eq!(c.provider, ProviderId::Anthropic);
    assert_eq!(c.access_token, "fake-access-alpha");
    assert_eq!(c.refresh_token.as_deref(), Some("fake-refresh-alpha"));
    assert_eq!(c.expires_at, Some(ts(1_762_520_400)));
    assert_eq!(c.refresh_expires_at, Some(ts(1_764_800_000)));
    assert_eq!(c.scopes, ["user:inference", "user:profile"]);
    assert_eq!(c.plan.as_deref(), Some("max"));
    assert_eq!(c.tier.as_deref(), Some("default_claude_max_20x"));
    let mode = fs::metadata(fx.paths.accounts.join("alpha/credentials.json"))
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o600);

    // Invalid routing.json values fall back to bro's defaults.
    let beta = reg.get(&id("beta")).unwrap();
    assert_eq!((beta.priority, beta.weight), (100, 1.0));

    // Codex: openai-auth.json maps onto Credentials (provider openai).
    let codex = reg.get(&id("codex1")).unwrap();
    assert_eq!((codex.provider, codex.priority), (ProviderId::OpenAi, 50));
    assert!(codex.imported);
    let c = reg.read_creds(&id("codex1")).unwrap().unwrap();
    assert_eq!(c.provider, ProviderId::OpenAi);
    assert_eq!(c.access_token, "fake-access-codex1");
    assert_eq!(c.refresh_token.as_deref(), Some("fake-refresh-codex1"));
    assert_eq!(
        c.openai_account_id.as_deref(),
        Some("fake-chatgpt-account-1")
    );
    assert_eq!(c.plan.as_deref(), Some("pro"));
    assert_eq!(c.expires_at, Some(ts(1_762_520_400)));

    // Keychain-only account with no Keychain item, and an invalid name: reported, skipped.
    assert!(skipped_reason(&report, "kc1").contains("Keychain"));
    assert!(skipped_reason(&report, "bad+name").contains("invalid"));
    assert!(!fx.paths.accounts.join("kc1").exists());

    let text = report.render();
    assert_no_secrets(&text);
    assert!(text.contains("bro pool down"), "{text}");
    assert!(text.contains("--i-stopped-bro"), "{text}");

    // kc1 and bad+name stay only in bro: never advise deleting the whole pool dir.
    let pool_rm = format!("rm -rf '{}'", fx.pool.display());
    assert!(!report.cleanup.contains(&pool_rm), "{:?}", report.cleanup);
    for name in ["alpha", "beta", "codex1", "dead1", "stale1"] {
        let rm = format!("rm -rf '{}'", fx.pool.join("accounts").join(name).display());
        assert!(
            report.cleanup.contains(&rm),
            "{rm} missing: {:?}",
            report.cleanup
        );
    }
    assert!(!report
        .cleanup
        .iter()
        .any(|c| c.contains("kc1") && c.starts_with("rm")));
    assert!(report
        .cleanup
        .contains(&"# kept in bro (not imported): bad+name, kc1".to_string()));
    assert!(!report.cleanup.iter().any(|c| c.starts_with("security")));
    // The orphan snapshot stays: only the imported snapshot file is listed, no glob.
    assert!(!report.cleanup.iter().any(|c| c.contains("pool-settings*")));
}

#[test]
fn per_account_write_failure_is_reported_and_import_continues() {
    let fx = fixture();
    fx.paths.ensure().unwrap();
    // A directory where beta's lock file must go makes beta's credentials write fail.
    fs::create_dir_all(fx.paths.accounts.join("beta/lock")).unwrap();
    let report = import(&fx, &FakeKeychain::default());
    assert!(
        matches!(
            &report.account("beta").unwrap().outcome,
            AccountOutcome::Failed { .. }
        ),
        "{:?}",
        report.account("beta")
    );
    assert!(matches!(
        report.account("alpha").unwrap().outcome,
        AccountOutcome::Imported { .. }
    ));
    assert_eq!(report.models, ItemOutcome::Imported);
    let reg = Registry::load(&fx.paths).unwrap();
    assert!(reg.get(&id("beta")).is_none());
    // Its seeded markers are dropped again, and bro's copy is the one to keep.
    let book = state(&fx);
    assert!(book
        .state(&id("beta"), OffsetDateTime::now_utc())
        .snapshot
        .is_none());
    assert!(report
        .cleanup
        .iter()
        .any(|c| c.starts_with("# kept in bro") && c.contains("beta")));
    assert!(report.render().contains("FAILED"));
}

#[test]
fn tuning_does_not_override_custom_config() {
    let fx = fixture();
    fx.paths.ensure().unwrap();
    fs::write(&fx.paths.config, "[routing]\nfive_hour_exp = 3.0\n").unwrap();
    let report = import(&fx, &FakeKeychain::default());
    assert!(
        matches!(&report.tuning, ItemOutcome::Skipped(r) if r.contains("five_hour_exp")),
        "{:?}",
        report.tuning
    );
    let cfg = Config::load_file(&fx.paths).unwrap();
    assert_eq!(cfg.routing.five_hour_exp, 3.0);
    assert_eq!(cfg.routing.min_headroom, 0.1);
}

#[test]
fn imports_sessions_models_and_tuning() {
    let fx = fixture();
    let report = import(&fx, &FakeKeychain::default());
    assert_eq!(report.sessions_imported, 2);
    assert_eq!(report.sessions_skipped, 1);
    let sessions = Sessions::load(
        Arc::new(Store::open(&fx.paths.db).unwrap()),
        Duration::from_secs(1800),
    );
    let seen = ts(1_762_500_001);
    assert_eq!(
        sessions.pinned(
            ProviderId::Anthropic,
            "user_fakehash_account_fake_session_one",
            seen
        ),
        Some(id("alpha"))
    );
    assert_eq!(
        sessions.pinned(
            ProviderId::OpenAi,
            "user_fakehash_account_fake_session_two",
            seen
        ),
        Some(id("codex1"))
    );
    // Only hashes are stored, never the raw user id.
    let raw = fs::read(&fx.paths.db).unwrap();
    assert!(!String::from_utf8_lossy(&raw).contains("user_fakehash"));

    assert_eq!(report.models, ItemOutcome::Imported);
    let table = ModelTable::load(&fx.paths).unwrap();
    assert!(table.mapping_enabled());
    assert!(table.rows().iter().any(|r| r.id == "my-custom-gpt"));
    assert_eq!(table.mapping_for("opus").unwrap().to, "gpt-5.5");

    assert_eq!(report.tuning, ItemOutcome::Imported);
    let cfg = Config::load_file(&fx.paths).unwrap();
    assert_eq!(cfg.routing.five_hour_exp, 2.0);
    assert_eq!(cfg.routing.min_headroom, 0.15);
    // 7 is out of range in bro too: dropped, default kept.
    assert_eq!(cfg.routing.headroom_taper_start, 0.20);
}

#[test]
fn existing_pooltime_account_is_not_overwritten() {
    let fx = fixture();
    fx.paths.ensure().unwrap();
    let reg = Registry::load(&fx.paths).unwrap();
    let mut mine = AccountMeta::new(id("alpha"), ProviderId::Anthropic);
    mine.priority = 7;
    reg.upsert(mine.clone()).unwrap();
    let report = import(&fx, &FakeKeychain::default());
    assert_eq!(
        report.account("alpha").unwrap().outcome,
        AccountOutcome::AlreadyExists
    );
    assert_eq!(reg.get(&id("alpha")).unwrap(), mine);
    assert!(reg.read_creds(&id("alpha")).unwrap().is_none());
    // The sticky session for the skipped account is not imported either.
    assert_eq!(report.sessions_imported, 1);
}

#[test]
fn keychain_fallback_used_when_no_file() {
    let fx = fixture();
    // Without the invalid account, every bro account ends up in pooltime.
    fs::remove_dir_all(fx.pool.join("accounts/bad+name")).unwrap();
    let kc = kc1_keychain(&fx);
    let report = import(&fx, &kc);
    let pool_rm = format!("rm -rf '{}'", fx.pool.display());
    assert!(report.cleanup.contains(&pool_rm), "{:?}", report.cleanup);
    // Only the account without a .credentials.json consults the Keychain, by bro's service name.
    assert_eq!(*kc.calls.lock().unwrap(), [kc1_service(&fx)]);
    let reg = Registry::load(&fx.paths).unwrap();
    let m = reg.get(&id("kc1")).unwrap();
    assert_eq!((m.priority, m.weight, m.imported), (100, 0.5, true));
    let c = reg.read_creds(&id("kc1")).unwrap().unwrap();
    assert_eq!(c.access_token, "fake-access-kc1");
    assert_eq!(c.plan.as_deref(), Some("max"));
    let text = report.render();
    assert_no_secrets(&text);
    // Cleanup is printed (never run): delete the Keychain item by its exact service name.
    let cmd = format!("security delete-generic-password -s '{}'", kc1_service(&fx));
    assert!(
        report.cleanup.iter().any(|c| c == &cmd),
        "{:?}",
        report.cleanup
    );
}

#[test]
fn dead_marker_seeded_only_when_fingerprint_matches() {
    let fx = fixture();
    let report = import(&fx, &FakeKeychain::default());
    let book = state(&fx);
    let now = OffsetDateTime::now_utc();
    assert_eq!(
        book.status(&id("dead1"), true, now),
        AccountStatus::NeedsLogin
    );
    assert!(book.is_refresh_dead(&id("dead1"), "fake-refresh-dead1"));
    // stale1's marker names an older refresh token: the current one is presumed good.
    assert_eq!(book.status(&id("stale1"), true, now), AccountStatus::Ready);
    assert_eq!(book.state(&id("stale1"), now).dead_refresh_fp, None);
    assert!(matches!(
        report.account("dead1").unwrap().outcome,
        AccountOutcome::Imported {
            needs_login: true,
            ..
        }
    ));
    assert!(matches!(
        report.account("stale1").unwrap().outcome,
        AccountOutcome::Imported {
            needs_login: false,
            ..
        }
    ));
}

#[test]
fn usage_fractions_kept() {
    let fx = fixture();
    import(&fx, &FakeKeychain::default());
    let book = state(&fx);
    let now = OffsetDateTime::now_utc();
    let snap = book.state(&id("alpha"), now).snapshot.unwrap();
    let util: BTreeMap<String, (Option<String>, f64)> = snap
        .windows
        .iter()
        .map(|w| (w.key.clone(), (w.model.clone(), w.utilization)))
        .collect();
    assert_eq!(util["5h"], (None, 0.42));
    assert_eq!(util["7d"], (None, 0.9));
    assert_eq!(util["7d-fable"], (Some("fable".into()), 0.06));
    assert_eq!(snap.updated_at, ts(1_762_500_000));
    assert_eq!(book.status(&id("alpha"), true, now), AccountStatus::Ready);
    // Legacy fixed-field snapshot (fiveHour*) converts too.
    let beta = book.state(&id("beta"), now).snapshot.unwrap();
    assert_eq!(beta.windows.len(), 1);
    assert_eq!(
        (beta.windows[0].key.as_str(), beta.windows[0].utilization),
        ("5h", 0.25)
    );
    // A future rateLimitedUntil keeps the account sidelined.
    assert!(matches!(
        book.status(&id("codex1"), true, now),
        AccountStatus::RateLimited { until, .. } if until == ts(4_102_444_800)
    ));
}

#[test]
fn snapshot_stem_mapping_and_orphans() {
    let fx = fixture();
    for p in [
        ".claude",
        ".claude-personal",
        ".claude-work",
        ".claude-team",
    ] {
        fs::create_dir_all(fx.home.join(p)).unwrap();
        fs::write(fx.home.join(p).join("settings.json"), "{}").unwrap();
    }
    let personal = json!({"managed": true, "prior": {
        "ANTHROPIC_BASE_URL": "https://example.invalid/mine",
        "ANTHROPIC_AUTH_TOKEN": null
    }});
    let stem_file = |dir: &str, stem: &str| {
        let resolved = fx.home.join(dir);
        fx.bro_home.join(format!(
            "pool-settings.{stem}.{}.json",
            sha8(resolved.to_str().unwrap())
        ))
    };
    fs::write(
        stem_file(".claude-personal", "claude-personal"),
        personal.to_string(),
    )
    .unwrap();
    fs::write(
        stem_file(".claude-team", "claude-team"),
        personal.to_string(),
    )
    .unwrap();
    // pooltime already holds a snapshot for claude-team: left alone, reported.
    let team_snap = fx.paths.run.join("profiles/claude-team.json");
    fs::create_dir_all(team_snap.parent().unwrap()).unwrap();
    fs::write(
        &team_snap,
        r#"{"managed":true,"prior":{},"statusline_prior":null}"#,
    )
    .unwrap();

    let plan = plan_import(&fx.pool, &fx.bro_home, &fx.home).unwrap();
    let orphans: Vec<String> = plan
        .orphans
        .iter()
        .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
        .collect();
    assert_eq!(orphans, ["pool-settings.prof.5019f7c5.json"]);
    let report = apply_import(&plan, &fx.paths, &FakeKeychain::default()).unwrap();

    let read = |profile: &str| -> Value {
        let p = fx
            .paths
            .run
            .join("profiles")
            .join(format!("{profile}.json"));
        serde_json::from_slice(&fs::read(p).unwrap()).unwrap()
    };
    assert_eq!(
        read("default"),
        json!({"managed": true, "prior": {
            "ANTHROPIC_BASE_URL": null,
            "ANTHROPIC_AUTH_TOKEN": null,
            "ANTHROPIC_DEFAULT_OPUS_MODEL": "claude-opus-4-8"
        }, "statusline_prior": null})
    );
    let mut expected = personal.clone();
    expected["statusline_prior"] = Value::Null;
    assert_eq!(read("claude-personal"), expected);
    assert_eq!(
        fs::read_to_string(&team_snap).unwrap(),
        r#"{"managed":true,"prior":{},"statusline_prior":null}"#
    );
    assert!(!fx.paths.run.join("profiles/claude-work.json").exists());
    assert!(!fx.paths.run.join("profiles/prof.json").exists());

    let outcome = |id: &str| {
        report
            .snapshots
            .iter()
            .find(|(p, _)| p == id)
            .map(|(_, o)| o.clone())
            .unwrap_or_else(|| panic!("{id} missing from {:?}", report.snapshots))
    };
    assert_eq!(outcome("default"), ItemOutcome::Imported);
    assert_eq!(outcome("claude-personal"), ItemOutcome::Imported);
    assert!(matches!(outcome("claude-team"), ItemOutcome::Skipped(r) if r.contains("already")));
    assert_eq!(report.orphans, plan.orphans);
    let text = report.render();
    assert!(text.contains("pool-settings.prof.5019f7c5.json"), "{text}");
    assert!(
        !text.contains("example.invalid"),
        "snapshot values are never printed: {text}"
    );
}

/// (relative path, content, mode, mtime) for every entry under `root`.
fn tree_digest(root: &Path) -> Vec<(String, Vec<u8>, u32, i64, i64)> {
    fn walk(root: &Path, dir: &Path, out: &mut Vec<(String, Vec<u8>, u32, i64, i64)>) {
        let Ok(rd) = fs::read_dir(dir) else { return };
        for e in rd {
            let p = e.unwrap().path();
            let md = fs::symlink_metadata(&p).unwrap();
            let rel = p.strip_prefix(root).unwrap().to_string_lossy().into_owned();
            let body = if md.is_file() {
                fs::read(&p).unwrap()
            } else {
                Vec::new()
            };
            out.push((rel, body, md.mode(), md.mtime(), md.mtime_nsec()));
            if md.is_dir() {
                walk(root, &p, out);
            }
        }
    }
    let mut out = Vec::new();
    walk(root, root, &mut out);
    out.sort();
    out
}

/// A `pooltime import bro` command over the fixture: temp HOME, temp POOLTIME_HOME, and a port
/// nothing answers on unless the test says so.
fn import_cmd(fx: &Fx, port: u16, extra: &[&str]) -> std::process::Command {
    let mut c = std::process::Command::new(env!("CARGO_BIN_EXE_pooltime"));
    c.args(["import", "bro"])
        .args(extra)
        .arg("--bro-pool")
        .arg(&fx.pool)
        .arg("--bro-home")
        .arg(&fx.bro_home)
        .env("HOME", &fx.home)
        .env("POOLTIME_HOME", &fx.paths.home)
        .env("POOLTIME_PORT", port.to_string())
        .env_remove("POOLTIME_HOST");
    c
}

#[tokio::test]
async fn bro_files_untouched() {
    let fx = fixture();
    fs::create_dir_all(fx.home.join(".claude")).unwrap();
    fs::write(fx.home.join(".claude/settings.json"), "{}").unwrap();
    let before = (tree_digest(&fx.pool), tree_digest(&fx.bro_home));
    assert!(before.0.len() > 10);
    let mut report = import(&fx, &kc1_keychain(&fx));
    assert!(matches!(
        report.account("kc1").unwrap().outcome,
        AccountOutcome::Imported { .. }
    ));
    let fake = FakeAnthropic::start().await;
    fake.script(
        "/api/oauth/profile",
        vec![Reply::error(500, "api_error", "x")],
    );
    let cfg = AnthropicCfg {
        profile_url: format!("{}/api/oauth/profile", fake.url()),
        ..AnthropicCfg::default()
    };
    fill_identities(&fx.paths, &mut report, &Upstream::new(), &cfg).await;
    assert_eq!((tree_digest(&fx.pool), tree_digest(&fx.bro_home)), before);
    // Cleanup commands are printed for the user, never executed.
    assert!(report.cleanup.iter().any(|c| c == "bro pool down"));
    assert!(fx.pool.exists() && fx.bro_home.join("pool-settings.json").exists());
}

#[test]
fn cli_import_leaves_bro_files_untouched() {
    let fx = fixture();
    // The real binary uses the real Keychain: keep it out of reach (no Keychain-only account).
    fs::remove_dir_all(fx.pool.join("accounts/kc1")).unwrap();
    fx.paths.ensure().unwrap();
    // Identity fetches go to a closed local port, never the network.
    fs::write(
        &fx.paths.config,
        "[anthropic]\napi_base = \"http://127.0.0.1:9\"\nprofile_url = \"http://127.0.0.1:9/api/oauth/profile\"\n",
    )
    .unwrap();
    let before = (tree_digest(&fx.pool), tree_digest(&fx.bro_home));
    let out = procs::assert(&mut import_cmd(&fx, free_port(), &[]))
        .success()
        .get_output()
        .clone();
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert_no_secrets(&stdout);
    assert_no_secrets(&String::from_utf8(out.stderr).unwrap());
    assert!(stdout.contains("identity pending"), "{stdout}");
    assert_eq!((tree_digest(&fx.pool), tree_digest(&fx.bro_home)), before);
    let reg = Registry::load(&fx.paths).unwrap();
    assert!(reg.get(&id("alpha")).unwrap().imported);
}

/// Answers every connection's `GET /health` like a pooltime daemon.
fn fake_pooltime_health() -> u16 {
    use std::io::{Read, Write};
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = l.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for mut conn in l.incoming().flatten() {
            let mut buf = [0u8; 4096];
            let _ = conn.read(&mut buf);
            let body = r#"{"service":"pooltime","status":"ok","version":"0.1.0"}"#;
            let _ = write!(
                conn,
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
        }
    });
    port
}

#[test]
fn import_refuses_while_pooltime_runs() {
    let fx = fixture();
    let port = fake_pooltime_health();
    let before = (tree_digest(&fx.pool), tree_digest(&fx.bro_home));
    let out = procs::assert(&mut import_cmd(&fx, port, &[]))
        .failure()
        .get_output()
        .clone();
    let stderr = String::from_utf8(out.stderr).unwrap();
    assert!(stderr.contains("pooltime is running"), "{stderr}");
    assert!(stderr.contains("stop it first"), "{stderr}");
    assert!(!fx.paths.home.exists(), "nothing may be written");
    assert_eq!((tree_digest(&fx.pool), tree_digest(&fx.bro_home)), before);
    // A dry run still shows the plan, with a note.
    let out = procs::assert(&mut import_cmd(&fx, port, &["--dry-run"]))
        .success()
        .get_output()
        .clone();
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(stdout.contains("pooltime is running"), "{stdout}");
    assert!(stdout.contains("alpha"), "{stdout}");
    assert!(!fx.paths.home.exists());
}

#[test]
fn dry_run_writes_nothing() {
    let fx = fixture();
    fx.paths.ensure().unwrap();
    fs::write(&fx.paths.config, "port = 3460\n").unwrap();
    let before = (
        tree_digest(&fx.paths.home),
        tree_digest(&fx.pool),
        tree_digest(&fx.bro_home),
    );
    let out = procs::assert(&mut import_cmd(&fx, free_port(), &["--dry-run"]))
        .success()
        .get_output()
        .clone();
    let stdout = String::from_utf8(out.stdout).unwrap();
    let stderr = String::from_utf8(out.stderr).unwrap();
    assert!(
        stdout.contains("alpha") && stdout.contains("codex1"),
        "{stdout}"
    );
    assert!(stdout.to_lowercase().contains("dry run"), "{stdout}");
    assert_no_secrets(&stdout);
    assert_no_secrets(&stderr);
    let after = (
        tree_digest(&fx.paths.home),
        tree_digest(&fx.pool),
        tree_digest(&fx.bro_home),
    );
    assert_eq!(after, before);
    // An absent pooltime home stays absent.
    let fresh = fx.dir.path().join("fresh-pt");
    procs::assert(import_cmd(&fx, free_port(), &["--dry-run"]).env("POOLTIME_HOME", &fresh))
        .success();
    assert!(!fresh.exists());
}

#[tokio::test]
async fn identity_fetched_for_refreshable_accounts_and_failure_is_pending() {
    let fx = fixture();
    let fake = FakeAnthropic::start().await;
    fake.route("/api/oauth/profile", |req, _| {
        if req.header("authorization") == Some("Bearer fake-access-alpha") {
            Reply::json(
                200,
                &json!({
                    "account": {"uuid": "acct-uuid-alpha", "email": "alpha@example.com"},
                    "organization": {"uuid": "org-uuid-alpha", "organization_type": "claude_max"}
                }),
            )
        } else {
            Reply::error(500, "api_error", "boom")
        }
    });
    let mut report = import(&fx, &FakeKeychain::default());
    let cfg = AnthropicCfg {
        api_base: "http://127.0.0.1:9".into(),
        profile_url: format!("{}/api/oauth/profile", fake.url()),
        ..AnthropicCfg::default()
    };
    fill_identities(&fx.paths, &mut report, &Upstream::new(), &cfg).await;

    let reg = Registry::load(&fx.paths).unwrap();
    let alpha = reg.get(&id("alpha")).unwrap();
    assert_eq!(alpha.identity.email.as_deref(), Some("alpha@example.com"));
    assert_eq!(
        alpha.identity.account_uuid.as_deref(),
        Some("acct-uuid-alpha")
    );
    assert_eq!(alpha.identity.org_uuid.as_deref(), Some("org-uuid-alpha"));
    let identity = |name: &str| match &report.account(name).unwrap().outcome {
        AccountOutcome::Imported { identity, .. } => *identity,
        other => panic!("{other:?}"),
    };
    assert_eq!(identity("alpha"), IdentityState::Resolved);
    // A failed fetch does not fail the import: the account stays, identity pending.
    assert_eq!(identity("beta"), IdentityState::Pending);
    assert!(reg.get(&id("beta")).unwrap().identity.email.is_none());
    // Codex and dead accounts are not asked.
    assert_eq!(identity("codex1"), IdentityState::NotApplicable);
    assert_eq!(identity("dead1"), IdentityState::NotApplicable);
    let auths: Vec<String> = fake
        .requests_to("/api/oauth/profile")
        .iter()
        .filter_map(|r| r.header("authorization").map(str::to_string))
        .collect();
    assert!(!auths
        .iter()
        .any(|a| a.contains("dead1") || a.contains("codex1")));
    assert_eq!(auths.len(), 3, "alpha, beta, stale1");
    let text = report.render();
    assert!(
        text.contains("identity pending (run `pooltime accounts login beta`"),
        "{text}"
    );
    assert!(
        report
            .notes
            .iter()
            .any(|n| n.starts_with("beta: identity fetch failed")),
        "{:?}",
        report.notes
    );
}
