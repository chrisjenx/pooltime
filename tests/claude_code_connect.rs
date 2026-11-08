//! Task 28: connect / disconnect / profile sync over a booted pool and a fake Anthropic catalog.
//! Everything runs under tempdirs; the "user home" is a tempdir, never the real one.

mod support;

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use pooltime::claude_code::statusline::{
    cache_path, is_wrapper, spawn_status_cache, wrapper_command,
};
use pooltime::claude_code::{
    connect, connect_with, disconnect, is_connected, profile_of, sync_once, Profile, SYNC_INTERVAL,
};
use pooltime::config::{Config, Paths};
use pooltime::fsutil::WriteOutcome;
use pooltime::server::AppState;
use serde_json::{json, Value};
use support::*;

const MODELS: &str = "/v1/models";

fn catalog_body(newer: bool) -> Value {
    let mut data = vec![
        json!({"id": "claude-sonnet-5", "display_name": "Claude Sonnet 5", "created_at": "2026-06-01T00:00:00Z"}),
        json!({"id": "claude-haiku-4-5", "display_name": "Claude Haiku 4.5", "created_at": "2026-01-01T00:00:00Z"}),
        json!({"id": "claude-fable-5", "display_name": "Claude Fable 5", "created_at": "2026-06-01T00:00:00Z"}),
    ];
    if newer {
        data.extend([
            json!({"id": "claude-sonnet-5-5", "display_name": "Claude Sonnet 5.5", "created_at": "2026-09-01T00:00:00Z"}),
            json!({"id": "claude-haiku-5-5", "display_name": "Claude Haiku 5.5", "created_at": "2026-09-01T00:00:00Z"}),
            json!({"id": "claude-fable-5-1", "display_name": "Claude Fable 5.1", "created_at": "2026-09-01T00:00:00Z"}),
        ]);
    }
    json!({"data": data, "has_more": false})
}

/// A booted pool whose catalog flips from old to newer when `newer` is set.
struct Fix {
    pool: Pool,
    srv: Booted,
    newer: Arc<AtomicBool>,
    /// Stand-in for the user's `$HOME`.
    home: tempfile::TempDir,
    _fake: FakeAnthropic,
}

impl Fix {
    async fn new() -> Fix {
        Fix::with(|b| b).await
    }

    async fn with(edit: impl FnOnce(Boot) -> Boot) -> Fix {
        let fake = FakeAnthropic::start().await;
        let newer = Arc::new(AtomicBool::new(false));
        let flag = newer.clone();
        fake.route(MODELS, move |_, _| {
            Reply::json(200, &catalog_body(flag.load(Ordering::SeqCst)))
        });
        let pool = Pool::new();
        let (policy, _) = fast_policy();
        let provider = pool
            .provider(&fake, policy)
            .with_catalog_ttl(Duration::ZERO);
        let srv = pool
            .boot(edit(Boot::default().provider(Arc::new(provider))))
            .await;
        Fix {
            pool,
            srv,
            newer,
            home: tempfile::tempdir().unwrap(),
            _fake: fake,
        }
    }

    fn app(&self) -> &AppState {
        &self.srv.app
    }

    fn paths(&self) -> Paths {
        Paths::under(self.pool.dir.path())
    }

    /// A profile dir under the fake home with `settings.json` and a stale Fable row.
    fn profile(&self, dir_name: &str) -> Profile {
        let dir = self.home.path().join(dir_name);
        fs::create_dir_all(&dir).unwrap();
        let p = Profile::from_dir(&dir, self.home.path());
        fs::write(
            &p.settings_json,
            serde_json::to_vec_pretty(&json!({"model": "opus", "env": {"KEEP": "1"}})).unwrap(),
        )
        .unwrap();
        fs::write(
            &p.claude_json,
            serde_json::to_vec_pretty(&json!({
                "numStartups": 3,
                "additionalModelOptionsCache": [
                    {"value": "claude-fable-4[1m]", "label": "Fable",
                     "description": "Fable 4 · Most capable"},
                    {"value": "claude-opus-5", "label": "Opus", "description": "Opus 5 · x"}
                ]
            }))
            .unwrap(),
        )
        .unwrap();
        p
    }

    fn config(&self) -> Config {
        Config::load_file(&self.paths()).unwrap()
    }
}

fn json_of(path: &Path) -> Value {
    serde_json::from_slice(&fs::read(path).unwrap()).unwrap()
}

fn env_of(p: &Profile) -> Value {
    json_of(&p.settings_json)["env"].clone()
}

fn fable_row(p: &Profile) -> Value {
    json_of(&p.claude_json)["additionalModelOptionsCache"][0].clone()
}

#[tokio::test]
async fn connect_writes_env_pins_fable_and_records_profiles() {
    let fx = Fix::new().await;
    let (a, b) = (fx.profile(".claude"), fx.profile(".claude-work"));
    let out = connect(fx.app(), &[a.clone(), b.clone()]).await.unwrap();
    assert_eq!(
        out,
        vec![
            (a.id.clone(), WriteOutcome::Written),
            (b.id.clone(), WriteOutcome::Written)
        ]
    );

    let cfg = fx.config();
    let key = cfg.proxy_key.clone().expect("proxy key generated");
    assert_eq!(key.len(), 64);
    assert!(key.chars().all(|c| c.is_ascii_hexdigit()));
    let mode = fs::metadata(fx.paths().config)
        .unwrap()
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, 0o600);
    let ids: Vec<&str> = cfg.profiles.iter().map(|p| p.id.as_str()).collect();
    assert_eq!(ids, [a.id.as_str(), b.id.as_str()]);

    let port = fx.srv.addr.port();
    for p in [&a, &b] {
        let env = env_of(p);
        assert_eq!(
            env["ANTHROPIC_BASE_URL"],
            format!("http://127.0.0.1:{port}")
        );
        assert_eq!(env["ANTHROPIC_AUTH_TOKEN"], key.as_str());
        assert_eq!(env["KEEP"], "1");
        assert_eq!(env["ANTHROPIC_DEFAULT_SONNET_MODEL"], "claude-sonnet-5[1m]");
        assert_eq!(env["ANTHROPIC_DEFAULT_HAIKU_MODEL"], "claude-haiku-4-5");
        assert!(env.get("ANTHROPIC_DEFAULT_FABLE_MODEL").is_none());
        // The default model table carries Codex rows, so a context fallback is written.
        assert!(env["CLAUDE_CODE_MAX_CONTEXT_TOKENS"]
            .as_str()
            .unwrap()
            .parse::<u32>()
            .is_ok());
        assert_eq!(json_of(&p.settings_json)["model"], "opus");
        assert!(is_connected(p, &fx.paths()));
        assert_eq!(
            fable_row(p),
            json!({"value": "claude-fable-5[1m]", "label": "Fable", "description": "Fable 5 · Most capable"})
        );
    }

    // Connecting again changes nothing, keeps the key, and does not duplicate the config entries.
    let again = connect(fx.app(), &[a.clone(), b.clone()]).await.unwrap();
    assert!(again.iter().all(|(_, o)| *o == WriteOutcome::Unchanged));
    let cfg2 = fx.config();
    assert_eq!(cfg2.proxy_key, Some(key));
    assert_eq!(cfg2.profiles, cfg.profiles);
}

#[tokio::test]
async fn connect_uses_the_running_pools_key() {
    let fx = Fix::with(|b| Boot {
        proxy_key: Some("live-pool-key".into()),
        ..b
    })
    .await;
    let p = fx.profile(".claude-work");
    connect(fx.app(), std::slice::from_ref(&p)).await.unwrap();
    assert_eq!(env_of(&p)["ANTHROPIC_AUTH_TOKEN"], "live-pool-key");
}

#[tokio::test]
async fn connect_refuses_when_pool_unhealthy() {
    // Nothing listens on the configured port.
    let free = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let fx = Fix::with(|b| Boot {
        configure: Some(Box::new(move |c| c.port = free)),
        ..b
    })
    .await;
    let p = fx.profile(".claude-work");
    let before = fs::read(&p.settings_json).unwrap();
    let err = connect(fx.app(), std::slice::from_ref(&p))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("not running"), "{err:#}");
    assert_eq!(fs::read(&p.settings_json).unwrap(), before);
    assert!(!fx.paths().config.exists(), "nothing recorded");
    assert!(!is_connected(&p, &fx.paths()));

    // Something that answers /health but is not a pooltime is refused too.
    let other = support::sockets::serve(|_| async {
        axum::response::Response::new(axum::body::Body::from("<html>dev server</html>"))
    })
    .await;
    let fx = Fix::with(|b| Boot {
        configure: Some(Box::new(move |c| c.port = other.port())),
        ..b
    })
    .await;
    let p = fx.profile(".claude-work");
    assert!(connect(fx.app(), std::slice::from_ref(&p)).await.is_err());
    assert!(env_of_is_untouched(&p));
}

fn env_of_is_untouched(p: &Profile) -> bool {
    json_of(&p.settings_json)["env"] == json!({"KEEP": "1"})
}

#[tokio::test]
async fn unreachable_catalog_changes_nothing() {
    let fx = Fix::new().await;
    let p = fx.profile(".claude-work");
    connect(fx.app(), std::slice::from_ref(&p)).await.unwrap();
    let settings = fs::read(&p.settings_json).unwrap();
    let claude_json = fs::read(&p.claude_json).unwrap();

    // Same address, but the catalog cannot be fetched: a sync is a complete no-op.
    let blind = fx.blind(Some(fx.srv.addr.port())).await;
    assert!(pooltime::claude_code::catalog(&blind.app).await.is_none());
    let synced = sync_once(&blind.app).await.unwrap();
    assert_eq!(synced, vec![(p.id.clone(), WriteOutcome::Unchanged)]);
    assert_eq!(fs::read(&p.settings_json).unwrap(), settings);
    assert_eq!(fs::read(&p.claude_json).unwrap(), claude_json);
    assert_eq!(
        env_of(&p)["ANTHROPIC_DEFAULT_SONNET_MODEL"],
        "claude-sonnet-5[1m]",
        "the Sonnet pin survives"
    );

    // Connecting without a catalog applies only the base URL and token.
    let q = fx.profile(".claude-personal");
    connect(&blind.app, std::slice::from_ref(&q)).await.unwrap();
    let env = env_of(&q);
    assert!(env["ANTHROPIC_BASE_URL"].is_string() && env["ANTHROPIC_AUTH_TOKEN"].is_string());
    let keys: Vec<&str> = env
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(keys, ["KEEP", "ANTHROPIC_BASE_URL", "ANTHROPIC_AUTH_TOKEN"]);
    // Fable rows are left alone too.
    assert_eq!(fable_row(&q)["value"], "claude-fable-4[1m]");
}

impl Fix {
    /// A pool over the same home whose Anthropic catalog is unavailable (no provider at all),
    /// listening on its own port, or reporting `port` as its configured one.
    async fn blind(&self, port: Option<u16>) -> Booted {
        self.pool
            .boot(Boot {
                configure: port.map(|p| -> ConfigEdit { Box::new(move |c| c.port = p) }),
                ..Boot::default()
            })
            .await
    }
}

#[tokio::test]
async fn reconnect_with_blind_catalog_keeps_pins() {
    let fx = Fix::new().await;
    let p = fx.profile(".claude-work");
    connect(fx.app(), std::slice::from_ref(&p)).await.unwrap();
    let env = env_of(&p);
    assert!(env["ANTHROPIC_DEFAULT_HAIKU_MODEL_DESCRIPTION"].is_string());
    assert!(env["CLAUDE_CODE_MAX_CONTEXT_TOKENS"].is_string());
    let claude_json = fs::read(&p.claude_json).unwrap();

    let blind = fx.blind(Some(fx.srv.addr.port())).await;
    let out = connect(&blind.app, std::slice::from_ref(&p)).await.unwrap();
    assert_eq!(out, vec![(p.id.clone(), WriteOutcome::Unchanged)]);
    assert_eq!(env_of(&p), env, "pins and context unchanged");
    assert_eq!(fs::read(&p.claude_json).unwrap(), claude_json);
    // Still releasable to the user's original env.
    disconnect(&fx.paths(), std::slice::from_ref(&p)).unwrap();
    assert_eq!(env_of(&p), json!({"KEEP": "1"}));
}

#[tokio::test]
async fn blind_sync_after_port_change_updates_url_keeps_pins() {
    let fx = Fix::new().await;
    let p = fx.profile(".claude-work");
    connect(fx.app(), std::slice::from_ref(&p)).await.unwrap();
    let before = env_of(&p);

    // The daemon restarted on another port and cannot reach the catalog.
    let blind = fx.blind(None).await;
    assert_ne!(blind.addr.port(), fx.srv.addr.port());
    let out = sync_once(&blind.app).await.unwrap();
    assert_eq!(out, vec![(p.id.clone(), WriteOutcome::Written)]);
    let after = env_of(&p);
    assert_eq!(
        after["ANTHROPIC_BASE_URL"],
        format!("http://127.0.0.1:{}", blind.addr.port())
    );
    let mut expected = before.clone();
    expected["ANTHROPIC_BASE_URL"] = after["ANTHROPIC_BASE_URL"].clone();
    assert_eq!(after, expected, "token, pins and context unchanged");
    assert_eq!(fable_row(&p)["value"], "claude-fable-5[1m]");
}

#[tokio::test]
async fn corrupt_claude_json_does_not_block_connect() {
    support::capture_logs();
    let fx = Fix::new().await;
    let (bad, good) = (fx.profile(".claude-bad"), fx.profile(".claude-good"));
    fs::write(&bad.claude_json, "{not json-SECRET-CONTENT").unwrap();
    let out = connect(fx.app(), &[bad.clone(), good.clone()])
        .await
        .unwrap();
    assert_eq!(
        out,
        vec![
            (bad.id.clone(), WriteOutcome::Written),
            (good.id.clone(), WriteOutcome::Written)
        ]
    );
    assert!(env_of(&bad)["ANTHROPIC_BASE_URL"].is_string());
    assert_eq!(
        env_of(&bad)["ANTHROPIC_DEFAULT_SONNET_MODEL"],
        "claude-sonnet-5[1m]"
    );
    assert_eq!(
        fs::read_to_string(&bad.claude_json).unwrap(),
        "{not json-SECRET-CONTENT"
    );
    assert_eq!(fable_row(&good)["value"], "claude-fable-5[1m]");
    let ids: Vec<String> = fx.config().profiles.into_iter().map(|p| p.id).collect();
    assert_eq!(ids, [bad.id.clone(), good.id.clone()]);
    assert!(!support::captured_logs().contains("SECRET-CONTENT"));
}

#[tokio::test]
async fn failing_profile_does_not_stop_the_others() {
    let fx = Fix::new().await;
    let (bad, good) = (fx.profile(".claude-bad"), fx.profile(".claude-good"));
    fs::write(&bad.settings_json, "{not json").unwrap();
    let err = connect(fx.app(), &[bad.clone(), good.clone()])
        .await
        .unwrap_err();
    assert!(format!("{err:#}").contains(&bad.id), "{err:#}");
    // The healthy profile after the failing one was still applied and recorded.
    assert!(env_of(&good)["ANTHROPIC_BASE_URL"].is_string());
    let ids: Vec<String> = fx.config().profiles.into_iter().map(|p| p.id).collect();
    assert_eq!(ids, std::slice::from_ref(&good.id));
    assert_eq!(fs::read_to_string(&bad.settings_json).unwrap(), "{not json");
}

#[tokio::test]
async fn no_known_key_warns_once_per_profile() {
    support::capture_logs();
    let fx = Fix::new().await;
    let p = fx.profile(".claude-nokey-warn-once");
    connect(fx.app(), std::slice::from_ref(&p)).await.unwrap();
    // The stored key is lost (hand-edited config) and this pool enforces none.
    let mut cfg = fx.config();
    cfg.proxy_key = None;
    cfg.save(&fx.paths()).unwrap();
    for _ in 0..3 {
        assert!(sync_once(fx.app()).await.unwrap().is_empty());
    }
    let logs = support::captured_logs();
    let hits = logs
        .lines()
        .filter(|l| l.contains("no proxy key") && l.contains("nokey-warn-once"))
        .count();
    assert_eq!(hits, 1, "{logs}");
}

#[test]
fn profile_of_default_needs_a_dot_claude_dir() {
    use pooltime::config::ProfileRef;
    let odd = profile_of(&ProfileRef {
        id: "default".into(),
        settings_dir: PathBuf::from("/h/elsewhere"),
    });
    assert_eq!(odd.claude_json, PathBuf::from("/h/elsewhere/.claude.json"));
}

#[tokio::test]
async fn sync_updates_all_connected_profiles() {
    let fx = Fix::new().await;
    let (a, b) = (fx.profile(".claude"), fx.profile(".claude-work"));
    let stranger = fx.profile(".claude-stranger");
    connect(fx.app(), &[a.clone(), b.clone()]).await.unwrap();

    // Nothing changed upstream: sync writes nothing.
    let quiet = sync_once(fx.app()).await.unwrap();
    assert_eq!(quiet.len(), 2);
    assert!(quiet.iter().all(|(_, o)| *o == WriteOutcome::Unchanged));

    // A newer Claude generation appears in the catalog.
    fx.newer.store(true, Ordering::SeqCst);
    let out = sync_once(fx.app()).await.unwrap();
    assert_eq!(
        out,
        vec![
            (a.id.clone(), WriteOutcome::Written),
            (b.id.clone(), WriteOutcome::Written)
        ]
    );
    for p in [&a, &b] {
        let env = env_of(p);
        assert_eq!(
            env["ANTHROPIC_DEFAULT_SONNET_MODEL"],
            "claude-sonnet-5-5[1m]"
        );
        assert_eq!(
            env["ANTHROPIC_DEFAULT_SONNET_MODEL_DESCRIPTION"],
            "Sonnet 5.5 with 1M context · Efficient for routine tasks"
        );
        assert_eq!(env["ANTHROPIC_DEFAULT_HAIKU_MODEL"], "claude-haiku-5-5");
        assert_eq!(
            fable_row(p),
            json!({"value": "claude-fable-5-1[1m]", "label": "Fable", "description": "Fable 5.1 · Most capable"})
        );
    }
    // A profile that was never connected is not touched.
    assert_eq!(env_of(&stranger), json!({"KEEP": "1"}));
    assert_eq!(fable_row(&stranger)["value"], "claude-fable-4[1m]");

    // And it settles: the next pass is a no-op.
    let settled = sync_once(fx.app()).await.unwrap();
    assert!(settled.iter().all(|(_, o)| *o == WriteOutcome::Unchanged));
}

#[tokio::test]
async fn sync_skips_profiles_that_are_no_longer_connected() {
    let fx = Fix::new().await;
    let (a, b) = (fx.profile(".claude"), fx.profile(".claude-work"));
    connect(fx.app(), &[a.clone(), b.clone()]).await.unwrap();
    // `a` is released behind pooltime's back (its snapshot is gone), so sync leaves it alone.
    fs::remove_file(pooltime::claude_code::snapshot_path(&fx.paths(), &a)).unwrap();
    fx.newer.store(true, Ordering::SeqCst);
    let out = sync_once(fx.app()).await.unwrap();
    assert_eq!(out, vec![(b.id.clone(), WriteOutcome::Written)]);
    assert_eq!(
        env_of(&a)["ANTHROPIC_DEFAULT_SONNET_MODEL"],
        "claude-sonnet-5[1m]"
    );
}

#[tokio::test]
async fn disconnect_restores_settings_and_forgets_profiles_without_a_pool() {
    let fx = Fix::new().await;
    let (a, b) = (fx.profile(".claude"), fx.profile(".claude-work"));
    let originals = [
        fs::read(&a.settings_json).unwrap(),
        fs::read(&b.settings_json).unwrap(),
    ];
    connect(fx.app(), &[a.clone(), b.clone()]).await.unwrap();
    let key = fx.config().proxy_key;

    // Only `Paths` is needed: the pool may be down.
    let paths = fx.paths();
    let out = disconnect(&paths, std::slice::from_ref(&a)).unwrap();
    assert_eq!(out, vec![(a.id.clone(), WriteOutcome::Written)]);
    assert_eq!(
        json_of(&a.settings_json),
        serde_json::from_slice::<Value>(&originals[0]).unwrap()
    );
    assert!(!is_connected(&a, &paths));
    assert!(is_connected(&b, &paths));
    let cfg = fx.config();
    assert_eq!(
        cfg.profiles
            .iter()
            .map(|p| p.id.as_str())
            .collect::<Vec<_>>(),
        [b.id.as_str()]
    );
    assert_eq!(cfg.proxy_key, key, "the proxy key is kept");

    let out = disconnect(&paths, &[a.clone(), b.clone()]).unwrap();
    assert_eq!(
        out,
        vec![
            (a.id.clone(), WriteOutcome::Unchanged),
            (b.id.clone(), WriteOutcome::Written)
        ]
    );
    assert_eq!(
        json_of(&b.settings_json),
        serde_json::from_slice::<Value>(&originals[1]).unwrap()
    );
    assert!(fx.config().profiles.is_empty());
}

#[test]
fn profile_of_follows_the_claude_json_rule() {
    use pooltime::config::ProfileRef;
    let home = PathBuf::from("/h");
    let default = profile_of(&ProfileRef {
        id: "default".into(),
        settings_dir: home.join(".claude"),
    });
    assert_eq!(default.claude_json, home.join(".claude.json"));
    assert_eq!(default.settings_json, home.join(".claude/settings.json"));
    let work = profile_of(&ProfileRef {
        id: "work".into(),
        settings_dir: home.join(".claude-work"),
    });
    assert_eq!(work.claude_json, home.join(".claude-work/.claude.json"));
    assert_eq!(work.id, "work");
}

#[test]
fn sync_interval_is_a_minute() {
    assert_eq!(SYNC_INTERVAL, Duration::from_secs(60));
}

#[tokio::test]
async fn spawn_profile_sync_runs_a_pass_immediately() {
    let fx = Fix::new().await;
    let p = fx.profile(".claude-work");
    connect(fx.app(), std::slice::from_ref(&p)).await.unwrap();
    fx.newer.store(true, Ordering::SeqCst);
    let handle = pooltime::claude_code::spawn_profile_sync(fx.srv.app.clone());
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while env_of(&p)["ANTHROPIC_DEFAULT_SONNET_MODEL"] != "claude-sonnet-5-5[1m]" {
        assert!(tokio::time::Instant::now() < deadline, "no sync pass ran");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    handle.abort();
}

fn set_statusline(p: &Profile, v: Value) {
    let mut s = json_of(&p.settings_json);
    s["statusLine"] = v;
    fs::write(&p.settings_json, serde_json::to_vec_pretty(&s).unwrap()).unwrap();
}

#[tokio::test]
async fn connect_disconnect_restores_original_statusline() {
    let fx = Fix::new().await;
    let bin = PathBuf::from("/opt/homebrew/bin/pooltime");
    // The pool's data dir is a tempdir, not ~/.pooltime: the wrapper names it.
    let home = fx.paths().home;
    let command = |id: &str| wrapper_command(&bin, id, Some(&home));
    assert!(command("claude-work").starts_with("POOLTIME_HOME="));
    let original = json!({"type": "command", "command": "~/.claude/ccline", "padding": 0});
    // The original's other fields (`padding`) are kept on the wrapper.
    let wrapper = json!({"type": "command", "command": command("claude-work"), "padding": 0});
    let snap_of = |id: &str| json_of(&fx.paths().run.join(format!("profiles/{id}.json")));

    // With an original status line: wrapped, snapshotted, restored exactly.
    let p = fx.profile(".claude-work");
    set_statusline(&p, original.clone());
    connect_with(fx.app(), std::slice::from_ref(&p), Some(&bin))
        .await
        .unwrap();
    assert_eq!(json_of(&p.settings_json)["statusLine"], wrapper);
    assert_eq!(snap_of("claude-work")["statusline_prior"], original);
    // Re-connecting changes nothing and never snapshots our own wrapper as the prior.
    let again = connect_with(fx.app(), std::slice::from_ref(&p), Some(&bin))
        .await
        .unwrap();
    assert_eq!(again, vec![(p.id.clone(), WriteOutcome::Unchanged)]);
    assert_eq!(snap_of("claude-work")["statusline_prior"], original);
    // Another binary path: the wrapper follows, the prior stays the user's.
    let moved = PathBuf::from("/usr/local/bin/pooltime");
    connect_with(fx.app(), std::slice::from_ref(&p), Some(&moved))
        .await
        .unwrap();
    assert_eq!(
        json_of(&p.settings_json)["statusLine"]["command"],
        wrapper_command(&moved, "claude-work", Some(&home))
    );
    assert_eq!(snap_of("claude-work")["statusline_prior"], original);
    // Without --statusline the installed wrapper is left as it is.
    connect(fx.app(), std::slice::from_ref(&p)).await.unwrap();
    assert!(is_wrapper(
        &json_of(&p.settings_json)["statusLine"],
        "claude-work"
    ));
    disconnect(&fx.paths(), std::slice::from_ref(&p)).unwrap();
    assert_eq!(json_of(&p.settings_json)["statusLine"], original);
    assert_eq!(env_of(&p), json!({"KEEP": "1"}));

    // Without one: the key is removed again on disconnect.
    let q = fx.profile(".claude-other");
    connect_with(fx.app(), std::slice::from_ref(&q), Some(&bin))
        .await
        .unwrap();
    assert_eq!(
        json_of(&q.settings_json)["statusLine"],
        json!({"type": "command", "command": command("claude-other")})
    );
    assert!(snap_of("claude-other")["statusline_prior"].is_null());
    disconnect(&fx.paths(), std::slice::from_ref(&q)).unwrap();
    assert!(json_of(&q.settings_json).get("statusLine").is_none());

    // Plain connect never touches statusLine.
    let r = fx.profile(".claude-plain");
    set_statusline(&r, original.clone());
    connect(fx.app(), std::slice::from_ref(&r)).await.unwrap();
    assert_eq!(json_of(&r.settings_json)["statusLine"], original);
    disconnect(&fx.paths(), std::slice::from_ref(&r)).unwrap();
    assert_eq!(json_of(&r.settings_json)["statusLine"], original);
}

#[tokio::test]
async fn state_change_rewrites_the_status_cache_promptly() {
    let pool = Pool::new();
    let srv = pool.boot(Boot::default()).await;
    let path = cache_path(&Paths::under(pool.dir.path()));
    let read = || -> Option<Value> { serde_json::from_slice(&fs::read(&path).ok()?).ok() };
    let cache = spawn_status_cache(srv.app.clone());
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while read().is_none_or(|v| v["pool"]["available"] != 1) {
        assert!(std::time::Instant::now() < deadline, "no first write");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    // Past the 1 s debounce of the first read, so this measures the change path alone.
    tokio::time::sleep(Duration::from_millis(1100)).await;
    let t = std::time::Instant::now();
    pool.state
        .mark_billing(&pool.meta.id, time::OffsetDateTime::now_utc());
    while read().is_none_or(|v| v["pool"]["available"] != 0) {
        // Change-driven, not the 60 s heartbeat: a generous bound that load cannot trip.
        assert!(
            t.elapsed() < Duration::from_secs(15),
            "no change-driven rewrite (heartbeat is {:?})",
            pooltime::claude_code::statusline::HEARTBEAT
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    // Shutdown deletes the cache, and nothing writes it again.
    cache.shutdown().await;
    assert!(!path.exists());
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(!path.exists());
}
