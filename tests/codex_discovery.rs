//! Codex usage poll (`wham/usage`) and model catalog discovery (`codex/models`) (Task 39, INV §4):
//! strict catalog validation, additive merge into the model table, hot reload into `/v1/models`
//! and routing without a restart, and the usage poll's last-check-only gate.

mod support;

use std::sync::Arc;
use std::time::Duration;

use pooltime::accounts::{AccountMeta, AccountStatus, RateLimitSnapshot, Window, WindowStatus};
use pooltime::claude_code::{connect, Profile};
use pooltime::models::{ModelTable, RouteDef};
use pooltime::providers::openai::{
    discover_codex_models, refresh_codex_catalog, spawn_usage_sweeper, OpenAiProvider,
    CODEX_CLIENT_VERSION,
};
use pooltime::providers::{Provider, ProviderId, UsageError};
use serde_json::{json, Value};
use support::fake_openai::*;
use support::sockets::{http_get, http_req, HttpResp};
use support::*;
use time::OffsetDateTime;

const MGMT_KEY: &str = "mgmt-test-key";
const TOKEN_A: &str = "codex-access-a-SECRET";
const TOKEN_B: &str = "codex-access-b-SECRET";

fn provider(pool: &Pool, fake: &FakeOpenAi) -> OpenAiProvider {
    let (policy, _) = fast_policy();
    codex_provider(pool, fake, policy, Duration::from_millis(250))
}

/// A pool with Codex accounts `a` and `b` (tokens [`TOKEN_A`] / [`TOKEN_B`]).
fn two_account_pool() -> (Pool, AccountMeta, AccountMeta) {
    let pool = Pool::new().without_default_account();
    let a = add_codex_account(&pool, "a", 1, TOKEN_A);
    let b = add_codex_account(&pool, "b", 2, TOKEN_B);
    (pool, a, b)
}

/// Serve `MODELS` per account: the reply for whichever token the request carries.
fn catalog_by_token(fake: &FakeOpenAi, a: Reply, b: Reply) {
    fake.route(MODELS, move |r, _| {
        if r.header("authorization") == Some(&format!("Bearer {TOKEN_A}")) {
            a.clone()
        } else {
            b.clone()
        }
    });
}

fn ids(defs: &[RouteDef]) -> Vec<String> {
    defs.iter().map(|d| d.id.clone()).collect()
}

fn listed(r: &HttpResp) -> Vec<String> {
    r.json()["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["id"].as_str().unwrap().to_string())
        .collect()
}

async fn mgmt_post(addr: std::net::SocketAddr, path: &str) -> HttpResp {
    http_req(
        addr,
        "POST",
        path,
        &[
            ("content-type", "application/json"),
            ("authorization", &format!("Bearer {MGMT_KEY}")),
        ],
        b"{}",
    )
    .await
}

fn row<'a>(t: &'a ModelTable, id: &str) -> Option<&'a RouteDef> {
    t.rows().iter().find(|r| r.id == id)
}

// ---- catalog validation ---------------------------------------------------------

#[tokio::test]
async fn invalid_entry_rejects_that_accounts_catalog() {
    let fake = FakeOpenAi::start().await;
    catalog_by_token(
        &fake,
        catalog(vec![
            model_entry("gpt-a-ok", "list", &["low", "high"], None, None),
            model_entry("bad slug!", "list", &["low"], None, None),
        ]),
        catalog(vec![model_entry(
            "gpt-b-ok",
            "list",
            &["low", "medium"],
            None,
            None,
        )]),
    );
    let (pool, a, b) = two_account_pool();
    let p = provider(&pool, &fake);

    // One bad entry rejects all of account a's catalog, not just that entry.
    let err = p.catalog(&a).await.unwrap_err().to_string();
    assert!(err.contains("invalid_slug"), "{err}");
    assert!(!err.contains("bad slug"), "no upstream text: {err}");
    assert_eq!(ids(&p.catalog(&b).await.unwrap()), vec!["gpt-b-ok"]);

    // Request shape: GET {api_base}/models?client_version=<const>, bearer, account id, originator.
    let reqs = fake.requests_to(MODELS);
    let r = &reqs[0];
    assert_eq!(r.method, "GET");
    assert_eq!(
        r.query.as_deref(),
        Some(format!("client_version={CODEX_CLIENT_VERSION}").as_str())
    );
    assert_eq!(r.header("chatgpt-account-id"), Some("chatgpt-a"));
    assert_eq!(r.header("originator"), Some("codex_cli_rs"));

    // Discovery across both: a fails (warned), b's models are merged.
    let srv = pool
        .boot(Boot::default().provider(boxed(provider(&pool, &fake))))
        .await;
    let report = discover_codex_models(&srv.app).await.unwrap();
    assert_eq!(report.accounts, 2);
    assert_eq!(report.added, vec!["gpt-b-ok".to_string()]);
    assert_eq!(report.failed.len(), 1);
    assert_eq!(report.failed[0].0.as_str(), "a");
    assert!(report.failed[0].1.contains("invalid_slug"));
    {
        let t = srv.app.models.read().unwrap();
        assert!(row(&t, "gpt-b-ok").is_some());
        assert!(row(&t, "gpt-a-ok").is_none());
    }

    // Every account failing is an error, and changes nothing.
    catalog_by_token(
        &fake,
        catalog(vec![json!({"slug": "x", "visibility": "list"})]),
        Reply::json(500, &json!({"detail": "SECRET upstream text"})),
    );
    let before = srv.app.models.read().unwrap().clone();
    let err = format!("{:#}", discover_codex_models(&srv.app).await.unwrap_err());
    assert!(!err.contains("SECRET"), "{err}");
    assert_eq!(*srv.app.models.read().unwrap(), before);

    // A token rejected by the catalog is refreshed and the request retried once.
    fake.script(OAUTH_TOKEN, vec![token_ok("codex-fresh-SECRET")]);
    fake.route(MODELS, |r, _| {
        if r.header("authorization") == Some("Bearer codex-fresh-SECRET") {
            catalog(vec![model_entry(
                "gpt-after-refresh",
                "list",
                &["low"],
                None,
                None,
            )])
        } else {
            Reply::json(401, &json!({"detail": "expired"}))
        }
    });
    let before = fake.requests_to(MODELS).len();
    assert_eq!(
        ids(&p.catalog(&a).await.unwrap()),
        vec!["gpt-after-refresh"]
    );
    assert_eq!(fake.requests_to(MODELS).len(), before + 2);
    assert_eq!(fake.requests_to(OAUTH_TOKEN).len(), 1);
}

#[tokio::test]
async fn only_visibility_list() {
    let fake = FakeOpenAi::start().await;
    fake.script(
        MODELS,
        vec![catalog(vec![
            model_entry("gpt-listed", "list", &["low"], None, None),
            model_entry("gpt-hidden", "hide", &["low"], None, None),
            // Not listed: the slug and levels of a non-list entry are not validated.
            model_entry("not a slug", "none", &[], None, None),
            model_entry("Gpt-Mixed.Case_1", "list", &["medium"], None, None),
        ])],
    );
    let pool = Pool::new().without_default_account();
    let a = add_codex_account(&pool, "a", 1, TOKEN_A);
    let p = provider(&pool, &fake);
    assert_eq!(
        ids(&p.catalog(&a).await.unwrap()),
        vec!["gpt-listed", "Gpt-Mixed.Case_1"]
    );

    // An unknown visibility value is a malformed catalog.
    fake.script(
        MODELS,
        vec![catalog(vec![
            model_entry("gpt-listed", "list", &["low"], None, None),
            model_entry("gpt-odd", "internal", &["low"], None, None),
        ])],
    );
    let err = p.catalog(&a).await.unwrap_err().to_string();
    assert!(err.contains("invalid_visibility"), "{err}");

    // Not an object with a `models` array: the whole response is invalid.
    fake.script(MODELS, vec![Reply::json(200, &json!({"data": []}))]);
    let err = p.catalog(&a).await.unwrap_err().to_string();
    assert!(err.contains("invalid_response"), "{err}");
}

#[tokio::test]
async fn ultra_effort_excluded() {
    let fake = FakeOpenAi::start().await;
    fake.script(
        MODELS,
        vec![catalog(vec![
            model_entry(
                "gpt-wide",
                "list",
                &["low", "ultra", "medium", "high", "xhigh", "low", "max"],
                Some(272_000),
                Some(1_000_000),
            ),
            // max below context: the max is dropped, the context kept.
            model_entry(
                "gpt-narrow",
                "list",
                &["none"],
                Some(400_000),
                Some(200_000),
            ),
            // Non-integer / non-positive windows are ignored.
            json!({"slug": "gpt-plain", "visibility": "list", "context_window": -5,
                   "max_context_window": 1.5,
                   "supported_reasoning_levels": [{"effort": "high"}, "junk", {"x": 1}]}),
        ])],
    );
    let pool = Pool::new().without_default_account();
    let a = add_codex_account(&pool, "a", 1, TOKEN_A);
    let p = provider(&pool, &fake);
    let defs = p.catalog(&a).await.unwrap();
    assert_eq!(
        defs[0],
        RouteDef {
            id: "gpt-wide".into(),
            provider: ProviderId::OpenAi,
            upstream_model: "gpt-wide".into(),
            context_window: Some(272_000),
            max_context_window: Some(1_000_000),
            supported_efforts: ["low", "medium", "high", "xhigh", "max"]
                .map(String::from)
                .to_vec(),
            effort_map: None,
        }
    );
    assert_eq!(
        (defs[1].context_window, defs[1].max_context_window),
        (Some(400_000), None)
    );
    assert_eq!(
        (
            defs[2].context_window,
            defs[2].max_context_window,
            defs[2].supported_efforts.clone()
        ),
        (None, None, vec!["high".to_string()])
    );

    // A model whose only level is `ultra` has no usable level: the catalog is rejected.
    fake.script(
        MODELS,
        vec![catalog(vec![
            model_entry("gpt-ok", "list", &["low"], None, None),
            model_entry("gpt-ultra", "list", &["ultra"], None, None),
        ])],
    );
    let err = p.catalog(&a).await.unwrap_err().to_string();
    assert!(err.contains("no_reasoning_levels"), "{err}");
    // Levels missing altogether are malformed too.
    fake.script(
        MODELS,
        vec![catalog(vec![
            json!({"slug": "gpt-x", "visibility": "list"}),
        ])],
    );
    let err = p.catalog(&a).await.unwrap_err().to_string();
    assert!(err.contains("invalid_reasoning_levels"), "{err}");
}

// ---- merge & hot reload -------------------------------------------------------

#[tokio::test]
async fn additive_only() {
    let fake = FakeOpenAi::start().await;
    fake.script(
        MODELS,
        vec![catalog(vec![
            // Same id as a configured row, different metadata: the configured row stands.
            model_entry("gpt-5.5", "list", &["low"], Some(1), Some(2)),
            model_entry("gpt-user", "list", &["low"], Some(1), None),
            model_entry("gpt-new", "list", &["low", "high"], Some(400_000), None),
        ])],
    );
    let pool = Pool::new().without_default_account();
    add_codex_account(&pool, "a", 1, TOKEN_A);
    let table = ModelTable::from_toml_str(
        r#"
        [[models]]
        id = "gpt-user"
        provider = "openai"
        upstream_model = "gpt-5.4"
        context_window = 123456
        "#,
    );
    let user_row = row(&table, "gpt-user").unwrap().clone();
    let bundled_55 = row(&table, "gpt-5.5").unwrap().clone();
    let srv = pool
        .boot(Boot {
            models: Some(table),
            ..Boot::default().provider(boxed(provider(&pool, &fake)))
        })
        .await;
    let before: Vec<RouteDef> = srv.app.models.read().unwrap().rows().to_vec();

    let report = discover_codex_models(&srv.app).await.unwrap();
    assert_eq!(report.added, vec!["gpt-new".to_string()]);
    {
        let t = srv.app.models.read().unwrap();
        assert_eq!(row(&t, "gpt-user"), Some(&user_row));
        assert_eq!(row(&t, "gpt-5.5"), Some(&bundled_55));
        // Every earlier row is still there, in order, untouched; the new one is appended.
        assert_eq!(&t.rows()[..before.len()], &before[..]);
        assert_eq!(t.rows().len(), before.len() + 1);
        assert_eq!(
            row(&t, "gpt-new").unwrap().supported_efforts,
            vec!["low", "high"]
        );
    }
    // Runtime-only: discovery writes no models.toml, and a save (e.g. a mapping edit) never
    // writes the discovered row; the next boot's discovery re-adds it. The user's row is saved.
    assert!(!ModelTable::path(&srv.app.paths).exists());
    assert!(srv.app.models.read().unwrap().is_discovered("gpt-new"));
    srv.app.models.read().unwrap().save(&srv.app.paths).unwrap();
    let text = std::fs::read_to_string(ModelTable::path(&srv.app.paths)).unwrap();
    assert!(!text.contains("gpt-new"), "{text}");
    let reloaded = ModelTable::load(&srv.app.paths).unwrap();
    assert!(row(&reloaded, "gpt-new").is_none());
    assert_eq!(row(&reloaded, "gpt-user"), Some(&user_row));

    // A later catalog refreshes the metadata of the discovered row only, and removes nothing.
    fake.script(
        MODELS,
        vec![catalog(vec![
            model_entry("gpt-5.5", "list", &["xhigh"], Some(9), Some(10)),
            model_entry("gpt-user", "list", &["xhigh"], Some(9), Some(10)),
            model_entry(
                "gpt-new",
                "list",
                &["medium"],
                Some(500_000),
                Some(1_000_000),
            ),
        ])],
    );
    let report = discover_codex_models(&srv.app).await.unwrap();
    assert!(report.added.is_empty());
    assert_eq!(report.updated, vec!["gpt-new".to_string()]);
    {
        let t = srv.app.models.read().unwrap();
        let n = row(&t, "gpt-new").unwrap();
        assert_eq!(
            (
                n.context_window,
                n.max_context_window,
                n.supported_efforts.clone()
            ),
            (Some(500_000), Some(1_000_000), vec!["medium".to_string()])
        );
        assert_eq!(row(&t, "gpt-user"), Some(&user_row));
        assert_eq!(row(&t, "gpt-5.5"), Some(&bundled_55));
    }
    fake.script(
        MODELS,
        vec![catalog(vec![model_entry(
            "gpt-5.5",
            "list",
            &["low"],
            None,
            None,
        )])],
    );
    let report = discover_codex_models(&srv.app).await.unwrap();
    assert!(report.added.is_empty() && report.updated.is_empty());
    assert!(row(&srv.app.models.read().unwrap(), "gpt-new").is_some());
}

/// Production registers both providers. With no ChatGPT account, Codex was not asked, so a Claude
/// failure is a failed refresh (502), not a success.
#[tokio::test]
async fn claude_failure_without_codex_accounts_is_502() {
    let claude = FakeAnthropic::start().await;
    claude.route("/v1/models", |_, _| {
        Reply::error(500, "api_error", "SECRET boom")
    });
    let fake = FakeOpenAi::start().await;
    let pool = Pool::new(); // only the Anthropic `work` account
    let (policy, _) = fast_policy();
    let srv = pool
        .boot(
            Boot::default()
                .provider(Arc::new(pool.provider(&claude, policy)))
                .provider(boxed(provider(&pool, &fake))),
        )
        .await;
    let r = mgmt_post(srv.addr, "/api/v1/models/refresh").await;
    assert_eq!(r.status, 502, "{}", r.text());
    assert!(r.text().contains("status:500"), "{}", r.text());
    assert!(!r.text().contains("SECRET"), "{}", r.text());
    assert!(fake.requests_to(MODELS).is_empty());
}

/// A mapping whose target is not (yet) a row stays a passthrough, is flagged `inert` in the
/// status listing, and warned about after a discovery run; discovering the target activates it.
#[tokio::test]
async fn mapping_to_missing_row_is_flagged_inert() {
    let fake = FakeOpenAi::start().await;
    fake.script(
        MODELS,
        vec![catalog(vec![model_entry(
            "gpt-later",
            "list",
            &["low"],
            None,
            None,
        )])],
    );
    let pool = Pool::new().without_default_account();
    add_codex_account(&pool, "a", 1, TOKEN_A);
    let table = ModelTable::from_toml_str(
        r#"
        mapping_enabled = true
        [[mappings]]
        from = "opus"
        to = "gpt-later"
        "#,
    );
    let srv = pool
        .boot(Boot {
            models: Some(table),
            ..Boot::default().provider(boxed(provider(&pool, &fake)))
        })
        .await;
    let opus = |v: &Value| -> Value {
        v["mapping"]["mappings"]
            .as_array()
            .unwrap()
            .iter()
            .find(|m| m["from"] == "opus")
            .unwrap()
            .clone()
    };
    let v = http_get(srv.addr, "/api/v1/status", &[]).await.json();
    assert_eq!(opus(&v)["inert"], true, "{v}");
    let sonnet = v["mapping"]["mappings"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["from"] == "sonnet")
        .unwrap()
        .clone();
    assert!(sonnet.get("inert").is_none(), "{sonnet}");
    // Passthrough fallback: no mapped route while the target is missing.
    assert!(srv
        .app
        .models
        .read()
        .unwrap()
        .mapped_route("claude-opus-5")
        .is_none());

    // A discovery run that still lacks the target warns (mapping and target only).
    fake.script(MODELS, vec![catalog(vec![])]);
    refresh_codex_catalog(&srv.app).await;
    assert!(captured_logs().lines().any(|l| l.contains("inert")
        && l.contains("\"from\":\"opus\"")
        && l.contains("\"to\":\"gpt-later\"")));

    // Once discovered, the mapping is live and no longer inert.
    fake.script(
        MODELS,
        vec![catalog(vec![model_entry(
            "gpt-later",
            "list",
            &["low"],
            None,
            None,
        )])],
    );
    refresh_codex_catalog(&srv.app).await;
    let v = http_get(srv.addr, "/api/v1/status", &[]).await.json();
    assert!(opus(&v).get("inert").is_none(), "{v}");
    assert_eq!(
        srv.app
            .models
            .read()
            .unwrap()
            .mapped_route("claude-opus-5")
            .unwrap()
            .def
            .upstream_model,
        "gpt-later"
    );
}

/// Claude's live catalog failing does not stop Codex discovery (both providers registered).
#[tokio::test]
async fn refresh_runs_codex_discovery_when_claude_catalog_fails() {
    let claude = FakeAnthropic::start().await;
    // The first listing succeeds (a last good live list), every later fetch fails.
    claude.route("/v1/models", |_, i| {
        if i == 0 {
            Reply::json(
                200,
                &json!({"data": [{"id": "claude-live-x", "created_at": "2026-06-01T00:00:00Z"}],
                        "has_more": false}),
            )
        } else {
            Reply::error(500, "api_error", "SECRET boom")
        }
    });
    let fake = FakeOpenAi::start().await;
    fake.script(
        MODELS,
        vec![catalog(vec![model_entry(
            "gpt-indep",
            "list",
            &["low"],
            Some(272_000),
            None,
        )])],
    );
    let pool = Pool::new(); // keeps the Anthropic `work` account, whose catalog fetch fails
    add_codex_account(&pool, "a", 1, TOKEN_A);
    let (policy, _) = fast_policy();
    let srv = pool
        .boot(
            Boot::default()
                .provider(Arc::new(pool.provider(&claude, policy)))
                .provider(boxed(provider(&pool, &fake))),
        )
        .await;
    assert!(listed(&http_get(srv.addr, "/v1/models", &[]).await).contains(&"claude-live-x".into()));
    let r = mgmt_post(srv.addr, "/api/v1/models/refresh").await;
    assert_eq!(r.status, 200, "{}", r.text());
    let v = r.json();
    assert_eq!(v["live"], 0);
    // `models` is what /v1/models serves: the last good live list plus the table.
    let served = listed(&http_get(srv.addr, "/v1/models", &[]).await);
    let reported: Vec<String> = v["models"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m.as_str().unwrap().to_string())
        .collect();
    assert_eq!(reported, served);
    assert!(reported.contains(&"claude-live-x".into()), "{v}");
    assert!(
        v["live_error"].as_str().unwrap().contains("status:500"),
        "{v}"
    );
    assert!(!r.text().contains("SECRET"), "{}", r.text());
    assert_eq!(v["codex"]["added"], json!(["gpt-indep"]), "{v}");
    assert!(listed(&http_get(srv.addr, "/v1/models", &[]).await).contains(&"gpt-indep".into()));

    // Both failing is a 502 (classified reasons only).
    fake.script(MODELS, vec![Reply::json(500, &json!({"detail": "SECRET"}))]);
    let r = mgmt_post(srv.addr, "/api/v1/models/refresh").await;
    assert_eq!(r.status, 502, "{}", r.text());
    assert!(!r.text().contains("SECRET"), "{}", r.text());
}

/// The boot / hourly run re-syncs connected profiles when discovery changes the table: the
/// Codex context fallback follows a newly discovered (then refreshed) row.
#[tokio::test]
async fn boot_discovery_resyncs_connected_profiles() {
    let claude = FakeAnthropic::start().await;
    claude.route("/v1/models", |_, _| {
        Reply::json(
            200,
            &json!({"data": [{"id": "claude-sonnet-5", "display_name": "Claude Sonnet 5",
                "created_at": "2026-06-01T00:00:00Z"}], "has_more": false}),
        )
    });
    let fake = FakeOpenAi::start().await;
    let small = |ctx: u64| {
        catalog(vec![model_entry(
            "gpt-small",
            "list",
            &["low"],
            Some(ctx),
            None,
        )])
    };
    fake.script(MODELS, vec![small(100_000)]);
    let pool = Pool::new();
    add_codex_account(&pool, "a", 1, TOKEN_A);
    let (policy, _) = fast_policy();
    let claude_provider = pool
        .provider(&claude, policy)
        .with_catalog_ttl(Duration::ZERO);
    let srv = pool
        .boot(
            Boot::default()
                .provider(Arc::new(claude_provider))
                .provider(boxed(provider(&pool, &fake))),
        )
        .await;
    let home = tempfile::tempdir().unwrap();
    let dir = home.path().join(".claude-work");
    std::fs::create_dir_all(&dir).unwrap();
    let profile = Profile::from_dir(&dir, home.path());
    std::fs::write(&profile.settings_json, b"{}").unwrap();
    connect(&srv.app, std::slice::from_ref(&profile))
        .await
        .unwrap();
    let ctx = || -> Value {
        let s: Value =
            serde_json::from_slice(&std::fs::read(&profile.settings_json).unwrap()).unwrap();
        s["env"]["CLAUDE_CODE_MAX_CONTEXT_TOKENS"].clone()
    };
    assert_ne!(ctx(), json!("100000"));

    refresh_codex_catalog(&srv.app).await;
    assert_eq!(ctx(), json!("100000"));

    // A metadata refresh of the discovered row re-syncs too.
    fake.script(MODELS, vec![small(90_000)]);
    refresh_codex_catalog(&srv.app).await;
    assert_eq!(ctx(), json!("90000"));
}

#[tokio::test]
async fn hot_reload_visible_in_v1_models_without_restart() {
    let fake = FakeOpenAi::start().await;
    fake.script(
        MODELS,
        vec![catalog(vec![model_entry(
            "gpt-hot-1",
            "list",
            &["low", "medium"],
            Some(272_000),
            None,
        )])],
    );
    fake.script(
        RESPONSES,
        vec![Reply::sse(frames(&text_turn(3, 1), Duration::ZERO))],
    );
    let pool = Pool::new().without_default_account();
    add_codex_account(&pool, "a", 1, TOKEN_A);
    let srv = pool
        .boot(Boot::default().provider(boxed(provider(&pool, &fake))))
        .await;

    let before = listed(&http_get(srv.addr, "/v1/models", &[]).await);
    assert!(!before.contains(&"gpt-hot-1".to_string()), "{before:?}");

    // `pooltime models refresh` -> the mgmt API -> Codex discovery, applied live.
    let r = mgmt_post(srv.addr, "/api/v1/models/refresh").await;
    assert_eq!(r.status, 200, "{}", r.text());
    let v = r.json();
    assert_eq!(v["codex"]["added"], json!(["gpt-hot-1"]), "{v}");
    assert_eq!(v["codex"]["accounts"], 1);
    assert!(v["models"]
        .as_array()
        .unwrap()
        .contains(&json!("gpt-hot-1")));

    let after = listed(&http_get(srv.addr, "/v1/models", &[]).await);
    assert!(after.contains(&"gpt-hot-1".to_string()), "{after:?}");

    // And routable on the same running server.
    let body = json!({
        "model": "gpt-hot-1",
        "max_tokens": 16,
        "messages": [{"role": "user", "content": "hi"}]
    });
    let r = http_req(
        srv.addr,
        "POST",
        "/v1/messages",
        &[("content-type", "application/json")],
        &serde_json::to_vec(&body).unwrap(),
    )
    .await;
    assert_eq!(r.status, 200, "{}", r.text());
    let sent = fake.requests_to(RESPONSES);
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].json()["model"], "gpt-hot-1");
}

// ---- usage poll ---------------------------------------------------------------

fn header_snapshot(now: OffsetDateTime) -> RateLimitSnapshot {
    RateLimitSnapshot {
        unified_status: Some("allowed".into()),
        windows: vec![Window {
            reset_synthetic: false,
            key: "5h".into(),
            model: None,
            status: WindowStatus::Allowed,
            utilization: 0.5,
            reset: Some(now + time::Duration::hours(1)),
        }],
        updated_at: now,
    }
}

#[tokio::test]
async fn codex_poll_gated_on_last_check_only() {
    let fake = FakeOpenAi::start().await;
    let reset = (OffsetDateTime::now_utc() + time::Duration::hours(2)).unix_timestamp();
    fake.route(USAGE, move |r, _| {
        if r.header("authorization") == Some(&format!("Bearer {TOKEN_A}")) {
            usage(true, false, Some((20.0, 18_000, reset)), None)
        } else {
            Reply::json(500, &json!({"detail": "SECRET"}))
        }
    });
    let (pool, a, b) = two_account_pool();
    let ttl = Duration::from_secs(600);
    let p = provider(&pool, &fake).with_usage_timing(true, ttl, Duration::from_secs(5));
    let now = OffsetDateTime::now_utc();

    // Fresh traffic headers and a bench do not gate the poll: only the last poll's own time does.
    pool.state
        .replace_header_snapshot(&a.id, header_snapshot(now));
    pool.state.mark_rate_limited(
        &a.id,
        now + time::Duration::hours(1),
        Some("5h".into()),
        now,
    );
    let snap = p.poll_usage(&a).await.unwrap();
    assert_eq!(fake.requests_to(USAGE).len(), 1);
    assert_eq!(snap.windows[0].key, "5h");
    assert!((snap.windows[0].utilization - 0.2).abs() < 1e-9);
    // The ground truth lifted the bench.
    assert_eq!(
        pool.state.status(&a.id, true, OffsetDateTime::now_utc()),
        AccountStatus::Ready
    );
    let r = &fake.requests_to(USAGE)[0];
    assert_eq!(r.method, "GET");
    assert_eq!(r.header("user-agent"), Some("codex-cli"));
    assert_eq!(r.header("chatgpt-account-id"), Some("chatgpt-a"));
    assert_eq!(
        r.header("authorization"),
        Some(format!("Bearer {TOKEN_A}").as_str())
    );

    // Within the TTL of the last check: no request.
    p.poll_usage(&a).await.unwrap();
    assert_eq!(fake.requests_to(USAGE).len(), 1);

    // A failed check gates too (no hot retry of a failing endpoint) and is recorded by label.
    assert_eq!(p.poll_usage(&b).await, Err(UsageError::Status(500)));
    assert_eq!(fake.requests_to(USAGE).len(), 2);
    let check = pool
        .state
        .state(&b.id, OffsetDateTime::now_utc())
        .usage_check
        .unwrap();
    assert_eq!(
        (check.ok, check.error.as_deref()),
        (false, Some("status:500"))
    );
    assert!(p.poll_usage(&b).await.is_err());
    assert_eq!(fake.requests_to(USAGE).len(), 2);

    // Once the TTL has passed the poll runs again.
    let p0 = provider(&pool, &fake).with_usage_timing(true, Duration::ZERO, Duration::from_secs(5));
    p0.poll_usage(&a).await.unwrap();
    assert_eq!(fake.requests_to(USAGE).len(), 3);
    // Disabled: never polls.
    let off =
        provider(&pool, &fake).with_usage_timing(false, Duration::ZERO, Duration::from_secs(5));
    assert_eq!(off.poll_usage(&a).await, Err(UsageError::NotSupported));
    assert_eq!(fake.requests_to(USAGE).len(), 3);
}

#[tokio::test]
async fn usage_rejects_window_only_when_limited_and_full() {
    let fake = FakeOpenAi::start().await;
    let weekly_reset = (OffsetDateTime::now_utc() + time::Duration::days(3)).unix_timestamp();
    let session_reset = (OffsetDateTime::now_utc() + time::Duration::hours(2)).unix_timestamp();
    fake.script(
        USAGE,
        vec![
            // Weekly in the primary slot, 100% but not enforced: allowed, not benched.
            usage(true, false, Some((100.0, 604_800, weekly_reset)), None),
            // Enforced weekly limit with an unfull session window: only the weekly is rejected.
            usage(
                false,
                true,
                Some((100.0, 604_800, weekly_reset)),
                Some((12.0, 18_000, session_reset)),
            ),
        ],
    );
    let pool = Pool::new().without_default_account();
    let a = add_codex_account(&pool, "a", 1, TOKEN_A);
    let p = provider(&pool, &fake).with_usage_timing(true, Duration::ZERO, Duration::from_secs(5));

    let snap = p.poll_usage(&a).await.unwrap();
    assert_eq!(snap.windows.len(), 1);
    assert_eq!(snap.windows[0].key, "7d");
    assert_eq!(snap.windows[0].status, WindowStatus::Allowed);
    assert_eq!(snap.unified_status.as_deref(), Some("allowed"));
    assert_eq!(
        pool.state.status(&a.id, true, OffsetDateTime::now_utc()),
        AccountStatus::Ready
    );

    let snap = p.poll_usage(&a).await.unwrap();
    let w = |k: &str| snap.windows.iter().find(|w| w.key == k).unwrap();
    assert_eq!(w("7d").status, WindowStatus::Rejected);
    assert_eq!(w("5h").status, WindowStatus::Allowed);
    assert_eq!(snap.unified_status.as_deref(), Some("rejected"));
    // Benched until the weekly reset, not the sooner session reset.
    let st = pool.state.state(&a.id, OffsetDateTime::now_utc());
    assert_eq!(
        st.rate_limited_until.map(|t| t.unix_timestamp()),
        Some(weekly_reset)
    );
}

#[tokio::test]
async fn sweeper_polls_codex_accounts() {
    let fake = FakeOpenAi::start().await;
    fake.script(
        USAGE,
        vec![usage(true, false, Some((5.0, 18_000, 0)), None)],
    );
    let (pool, _a, _b) = two_account_pool();
    let p = Arc::new(provider(&pool, &fake).with_usage_timing(
        true,
        Duration::from_secs(600),
        Duration::from_secs(5),
    ));
    let h = spawn_usage_sweeper(p, Duration::from_secs(3600));
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while fake.requests_to(USAGE).len() < 2 {
        assert!(
            tokio::time::Instant::now() < deadline,
            "sweeper did not poll"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    h.abort();
    let tokens: Vec<Value> = fake
        .requests_to(USAGE)
        .iter()
        .map(|r| json!(r.header("authorization")))
        .collect();
    assert!(tokens.contains(&json!(format!("Bearer {TOKEN_A}"))));
    assert!(tokens.contains(&json!(format!("Bearer {TOKEN_B}"))));
}
