//! `GET /v1/models` (INV §5): live Anthropic catalog merged with the model table, plus the
//! provider-level live fetch (token choice, cache, single-flight, stale-on-failure).
//!
//! Ported from bro `pool/src/upstream/models-list.test.ts`:
//! - lists the live catalog with an account's OAuth token       -> `models_merge_order_and_owners`
//! - serves the last good list when upstream fails              -> `stale_cache_preferred_on_failure`
//! - concurrent cold-cache requests share one upstream call     -> `single_flight_under_concurrency`
//! - nothing cached + upstream down -> table only               -> `table_only_when_nothing_cached_and_upstream_down`
//! - configured Codex id replaces a colliding live id           -> `configured_openai_id_replaces_colliding_live_id`
//!
//! New in pooltime: `skips_needs_login_accounts_for_token` (INV §10.8),
//! `falls_through_to_next_account_when_one_is_rejected`, `cache_serves_within_ttl`.
//! (`buildModelListing` unit cases live in `src/server/models_route.rs`.)

use crate::support;

use std::sync::Arc;
use std::time::Duration;

use pooltime::models::ModelTable;
use pooltime::providers::anthropic::AnthropicProvider;
use pooltime::providers::ProviderId;
use serde_json::{json, Value};
use support::sockets::http_get;
use support::*;
use time::OffsetDateTime;

const MODELS: &str = "/v1/models";

fn live_body() -> Value {
    json!({
        "data": [
            {"type": "model", "id": "claude-fable-5-1", "display_name": "Claude Fable 5.1",
             "created_at": "2026-08-28T00:00:00Z", "max_input_tokens": 1_000_000},
            {"type": "model", "id": "claude-opus-5", "display_name": "Claude Opus 5",
             "created_at": "2026-07-01T00:00:00Z", "max_input_tokens": 1_000_000},
            {"id": ""},
        ],
        "has_more": false,
    })
}

fn ids(list: &Value) -> Vec<String> {
    list["data"]
        .as_array()
        .expect("data array")
        .iter()
        .map(|m| m["id"].as_str().unwrap().to_string())
        .collect()
}

fn entry<'a>(list: &'a Value, id: &str) -> &'a Value {
    list["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["id"] == id)
        .unwrap_or_else(|| panic!("{id} listed"))
}

fn model_paths(fake: &FakeAnthropic) -> usize {
    fake.requests_to(MODELS).len()
}

/// Provider with an instant (never fresh) cache TTL when `ttl` is `Some`.
fn provider(pool: &Pool, fake: &FakeAnthropic, ttl: Option<Duration>) -> AnthropicProvider {
    let (policy, _) = fast_policy();
    let p = pool.provider(fake, policy);
    match ttl {
        Some(t) => p.with_catalog_ttl(t),
        None => p,
    }
}

fn dead(pool: &Pool, id: &str) {
    pool.state
        .mark_refresh_dead(&id.parse().unwrap(), REFRESH, OffsetDateTime::now_utc());
}

#[tokio::test]
async fn models_merge_order_and_owners() {
    let fake = FakeAnthropic::start().await;
    fake.script(MODELS, vec![Reply::json(200, &live_body())]);
    let pool = Pool::new();
    let booted = pool
        .boot(Boot::default().provider(Arc::new(provider(&pool, &fake, None))))
        .await;

    // Unauthenticated (bro parity): no proxy key configured here, none sent.
    let resp = http_get(booted.addr, MODELS, &[]).await;
    assert_eq!(resp.status, 200, "{}", resp.text());
    let list = resp.json();

    // Live rows first (malformed row dropped), then the table's Claude rows not listed live
    // (aliases; bundled full ids stay hidden even when live lists them), then OpenAI rows in
    // table order with the `[1m]` selector for extended-context routes.
    assert_eq!(
        ids(&list),
        [
            "claude-fable-5-1",
            "claude-opus-5",
            "opus",
            "sonnet",
            "haiku",
            "fable",
            "gpt-6-astra[1m]",
            "gpt-6-sol[1m]",
            "gpt-6-luna[1m]",
            "gpt-5.6-sol[1m]",
            "gpt-5.6-terra[1m]",
            "gpt-5.6-luna[1m]",
            "gpt-5.6[1m]",
            "gpt-5.5",
            "gpt-5.4[1m]",
            "gpt-5.4-mini",
        ]
    );
    // Envelope: Anthropic list fields plus bro's `object: "list"`.
    assert_eq!(list["object"], "list");
    assert_eq!(list["has_more"], false);
    assert_eq!(list["first_id"], "claude-fable-5-1");
    assert_eq!(list["last_id"], "gpt-5.4-mini");

    let fable = entry(&list, "claude-fable-5-1");
    assert_eq!(fable["object"], "model");
    assert_eq!(fable["owned_by"], "anthropic-claude-max-pool");
    assert_eq!(fable["display_name"], "Claude Fable 5.1");
    assert_eq!(fable["max_input_tokens"], 1_000_000);
    assert_eq!(fable["created"], 1_787_875_200); // 2026-08-28T00:00:00Z
    assert!(fable.get("context_window").is_none());

    let alias = entry(&list, "opus");
    assert_eq!(alias["owned_by"], "anthropic-claude-max-pool");
    assert_eq!(alias["created"], 0);
    assert!(alias.get("context_window").is_none());

    let sol = entry(&list, "gpt-6-sol[1m]");
    assert_eq!(sol["owned_by"], "openai-chatgpt-pool");
    assert_eq!(sol["context_window"], 272_000);
    assert_eq!(sol["max_context_window"], 872_000);
    assert!(sol.get("max_input_tokens").is_none());
    let plain = entry(&list, "gpt-5.5");
    assert_eq!(plain["max_context_window"], 272_000);

    // One upstream call with the documented request shape and the account's token.
    let reqs = fake.requests_to(MODELS);
    assert_eq!(reqs.len(), 1);
    let r = &reqs[0];
    assert_eq!(r.method, "GET");
    assert_eq!(r.query.as_deref(), Some("limit=1000"));
    assert_eq!(
        r.header("authorization"),
        Some(&*format!("Bearer {ACCESS}"))
    );
    assert_eq!(r.header("anthropic-version"), Some("2023-06-01"));
    assert_eq!(r.header("anthropic-beta"), Some("oauth-2025-04-20"));
    assert_eq!(
        r.header("user-agent"),
        Some(&*pooltime::config::AnthropicCfg::default().user_agent)
    );
}

#[tokio::test]
async fn table_only_when_nothing_cached_and_upstream_down() {
    support::capture_logs();
    let fake = FakeAnthropic::start().await;
    fake.script(MODELS, vec![Reply::error(500, "api_error", "boom-SECRET")]);
    let pool = Pool::new();
    let p = Arc::new(provider(&pool, &fake, None));
    assert!(p.live_models().await.is_err());
    let booted = pool.boot(Boot::default().provider(p)).await;

    let resp = http_get(booted.addr, MODELS, &[]).await;
    assert_eq!(resp.status, 200, "{}", resp.text());
    let list = resp.json();
    let got = ids(&list);
    assert_eq!(&got[..4], ["opus", "sonnet", "haiku", "fable"]);
    assert!(got.contains(&"gpt-5.5".to_string()));
    assert!(!got.iter().any(|i| i.starts_with("claude-")));
    assert_eq!(list["has_more"], false);
    assert_eq!(list["first_id"], "opus");

    // The failure is logged as a classified reason, never the upstream text or the token.
    let logs = captured_logs();
    assert!(logs.contains("status:500"), "{logs}");
    assert!(!logs.contains("boom-SECRET"));
    assert!(!logs.contains(ACCESS));
}

#[tokio::test]
async fn no_anthropic_provider_still_lists_the_table() {
    let pool = Pool::new();
    let booted = pool.boot(Boot::default()).await;
    let resp = http_get(booted.addr, MODELS, &[]).await;
    assert_eq!(resp.status, 200);
    assert_eq!(ids(&resp.json())[0], "opus");
}

#[tokio::test]
async fn configured_openai_id_replaces_colliding_live_id() {
    let fake = FakeAnthropic::start().await;
    fake.script(
        MODELS,
        vec![Reply::json(
            200,
            &json!({"data": [{"id": "custom"}, {"id": "custom[1m]"}, {"id": "claude-opus-5"}]}),
        )],
    );
    let pool = Pool::new();
    let table = ModelTable::from_toml_str(
        r#"[[models]]
id = "custom"
provider = "openai"
upstream_model = "custom-x"
context_window = 272000
max_context_window = 1000000
"#,
    );
    let booted = pool
        .boot(Boot {
            models: Some(table),
            ..Boot::default().provider(Arc::new(provider(&pool, &fake, None)))
        })
        .await;
    let list = http_get(booted.addr, MODELS, &[]).await.json();
    let got = ids(&list);
    assert_eq!(got[0], "claude-opus-5");
    assert!(!got.contains(&"custom".to_string()));
    assert_eq!(got.iter().filter(|i| *i == "custom[1m]").count(), 1);
    assert_eq!(
        entry(&list, "custom[1m]")["owned_by"],
        "openai-chatgpt-pool"
    );
}

#[tokio::test]
async fn stale_cache_preferred_on_failure() {
    let fake = FakeAnthropic::start().await;
    fake.script(
        MODELS,
        vec![
            Reply::json(200, &live_body()),
            Reply::error(500, "api_error", "down"),
        ],
    );
    let pool = Pool::new();
    // A zero TTL makes every cached list stale at once, so each call re-fetches.
    let p = Arc::new(provider(&pool, &fake, Some(Duration::ZERO)));
    let first = p.live_models().await.unwrap();
    assert_eq!(first.len(), 2);

    let second = p.live_models().await.unwrap();
    assert_eq!(second, first, "stale list served when the refetch fails");
    assert_eq!(model_paths(&fake), 2, "the refetch was attempted");

    // Through the endpoint too: still the live rows, not the table fallback.
    let booted = pool.boot(Boot::default().provider(p)).await;
    let list = http_get(booted.addr, MODELS, &[]).await.json();
    assert_eq!(&ids(&list)[..2], ["claude-fable-5-1", "claude-opus-5"]);
    assert_eq!(model_paths(&fake), 3);
}

#[tokio::test]
async fn cache_serves_within_ttl() {
    let fake = FakeAnthropic::start().await;
    fake.script(MODELS, vec![Reply::json(200, &live_body())]);
    let pool = Pool::new();
    let p = provider(&pool, &fake, None);
    p.live_models().await.unwrap();
    p.live_models().await.unwrap();
    p.live_models().await.unwrap();
    assert_eq!(model_paths(&fake), 1);
}

#[tokio::test]
async fn single_flight_under_concurrency() {
    let fake = FakeAnthropic::start().await;
    fake.script(
        MODELS,
        vec![Reply::json(200, &live_body()).delay_headers(Duration::from_millis(300))],
    );
    let pool = Pool::new();
    let p = Arc::new(provider(&pool, &fake, None));
    let calls: Vec<_> = (0..8)
        .map(|_| {
            let p = p.clone();
            tokio::spawn(async move { p.live_models().await })
        })
        .collect();
    let mut results = Vec::new();
    for c in calls {
        results.push(c.await.unwrap().unwrap());
    }
    assert_eq!(model_paths(&fake), 1, "one upstream call for 8 callers");
    assert!(results.iter().all(|r| *r == results[0] && r.len() == 2));

    // A failing fetch is shared by its waiters too (no per-waiter retry storm).
    let fake = FakeAnthropic::start().await;
    fake.script(
        MODELS,
        vec![Reply::error(503, "api_error", "x").delay_headers(Duration::from_millis(300))],
    );
    let pool = Pool::new();
    let p = Arc::new(provider(&pool, &fake, None));
    let calls: Vec<_> = (0..6)
        .map(|_| {
            let p = p.clone();
            tokio::spawn(async move { p.live_models().await })
        })
        .collect();
    for c in calls {
        assert!(c.await.unwrap().is_err());
    }
    assert_eq!(model_paths(&fake), 1);
}

#[tokio::test]
async fn skips_needs_login_accounts_for_token() {
    let fake = FakeAnthropic::start().await;
    fake.script(MODELS, vec![Reply::json(200, &live_body())]);
    fake.script(TOKEN, vec![Reply::error(400, "invalid_grant", "dead")]);
    let pool = Pool::new();
    pool.add_account("zz-ok", ProviderId::Anthropic, 100, "tok-ok");
    // `work` sorts first, is dead, and its access token is already expired: using it would
    // mean a refresh POST for a token the endpoint rejected.
    pool.reg
        .write_creds(&pool.meta.id, &creds(ACCESS, -10))
        .unwrap();
    dead(&pool, "work");
    // A third account whose credentials are gone is NeedsLogin as well.
    pool.reg
        .upsert(pooltime::accounts::AccountMeta::new(
            "aa-nocreds".parse().unwrap(),
            ProviderId::Anthropic,
        ))
        .unwrap();
    // And a Codex account never supplies an Anthropic token.
    pool.add_account("aa-codex", ProviderId::OpenAi, 100, "tok-codex");

    let p = provider(&pool, &fake, None);
    assert_eq!(p.live_models().await.unwrap().len(), 2);
    let reqs = fake.requests_to(MODELS);
    assert_eq!(reqs.len(), 1);
    assert_eq!(reqs[0].header("authorization"), Some("Bearer tok-ok"));
    assert!(
        fake.requests_to(TOKEN).is_empty(),
        "no refresh for NeedsLogin accounts"
    );

    // Every account NeedsLogin: no upstream call at all, and the endpoint still answers.
    dead(&pool, "zz-ok");
    let p = provider(&pool, &fake, None);
    assert!(p.live_models().await.is_err());
    assert_eq!(model_paths(&fake), 1);
    assert!(fake.requests_to(TOKEN).is_empty());
    let booted = pool.boot(Boot::default().provider(Arc::new(p))).await;
    let resp = http_get(booted.addr, MODELS, &[]).await;
    assert_eq!(resp.status, 200);
    assert_eq!(ids(&resp.json())[0], "opus");
}

#[tokio::test]
async fn falls_through_to_next_account_when_one_is_rejected() {
    let fake = FakeAnthropic::start().await;
    fake.route(MODELS, |r, _| match r.header("authorization") {
        Some("Bearer tok-a") => Reply::error(403, "permission_error", "no"),
        _ => Reply::json(200, &live_body()),
    });
    let pool = Pool::new().without_default_account();
    pool.add_account("a", ProviderId::Anthropic, 1, "tok-a");
    pool.add_account("b", ProviderId::Anthropic, 1, "tok-b");
    let p = provider(&pool, &fake, None);
    assert_eq!(p.live_models().await.unwrap().len(), 2);
    let used: Vec<_> = fake
        .requests_to(MODELS)
        .iter()
        .map(|r| r.header("authorization").unwrap().to_string())
        .collect();
    assert_eq!(used, ["Bearer tok-a", "Bearer tok-b"]);
}
