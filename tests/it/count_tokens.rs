//! `POST /v1/messages/count_tokens`: forwarded to Anthropic like `/v1/messages` (pooled OAuth
//! token, client credentials stripped, proxy key enforced), but without the inference side
//! effects (sticky sessions, served-attempt accounting). Models that Codex serves get a local
//! estimate, because OpenAI has no matching endpoint.

use crate::support;

use std::sync::Arc;
use std::time::Duration;

use pooltime::models::{Mapping, ModelTable};
use pooltime::providers::{Provider, ProviderId};
use pooltime::server::inference::REQUEST_ID_HEADER;
use serde_json::{json, Value};
use support::fake_openai::{add_codex_account, boxed, codex_provider, FakeOpenAi};
use support::sockets::{http_req, HttpResp};
use support::*;
use time::OffsetDateTime;

const COUNT: &str = "/v1/messages/count_tokens";
const MGMT: (&str, &str) = ("authorization", "Bearer mgmt-test-key");
const PROXY_KEY: &str = "sekret-proxy-key";
const SESSION: &str = "user_count_tokens_session";

fn anthropic(pool: &Pool, fake: &FakeAnthropic) -> Arc<dyn Provider> {
    let (policy, _) = fast_policy();
    Arc::new(pool.provider(fake, policy))
}

fn count_body(model: &str) -> Value {
    json!({
        "model": model,
        "metadata": {"user_id": SESSION},
        "messages": [{"role": "user", "content": "how many tokens is this?"}]
    })
}

async fn post(addr: std::net::SocketAddr, body: &Value, extra: &[(&str, &str)]) -> HttpResp {
    let mut h = vec![("content-type", "application/json")];
    h.extend_from_slice(extra);
    http_req(addr, "POST", COUNT, &h, &serde_json::to_vec(body).unwrap()).await
}

async fn journaled(addr: std::net::SocketAddr, id: &str) -> Value {
    for _ in 0..100 {
        let path = format!("/api/v1/requests/{id}");
        let r = http_req(addr, "GET", &path, &[MGMT], b"").await;
        if r.status == 200 {
            return r.json();
        }
        assert_eq!(r.status, 404, "{}", r.text());
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("request {id} was never journaled");
}

fn counted(fake: &FakeAnthropic, n: u64) {
    fake.script(COUNT, vec![Reply::json(200, &json!({"input_tokens": n}))]);
}

fn not_pinned(booted: &Booted) -> bool {
    booted
        .app
        .sessions
        .pinned(ProviderId::Anthropic, SESSION, OffsetDateTime::now_utc())
        .is_none()
}

#[tokio::test]
async fn forwards_with_the_account_token_and_passes_the_response_through() {
    let fake = FakeAnthropic::start().await;
    fake.script(
        COUNT,
        vec![Reply::json(200, &json!({"input_tokens": 1234})).header("request-id", "req_up_count")],
    );
    let pool = Pool::new();
    let booted = pool
        .boot(Boot {
            proxy_key: Some(PROXY_KEY.into()),
            ..Boot::default().provider(anthropic(&pool, &fake))
        })
        .await;
    let raw = serde_json::to_vec(&count_body("claude-sonnet-5")).unwrap();
    let bearer = format!("Bearer {PROXY_KEY}");
    let r = http_req(
        booted.addr,
        "POST",
        &format!("{COUNT}?beta=true"),
        &[
            ("content-type", "application/json"),
            ("authorization", bearer.as_str()),
            ("x-api-key", PROXY_KEY),
            ("anthropic-version", "2023-06-01"),
            (
                "anthropic-beta",
                "oauth-2025-04-20,token-counting-2024-11-01",
            ),
        ],
        &raw,
    )
    .await;
    assert_eq!(r.status, 200, "{}", r.text());
    assert_eq!(r.json(), json!({"input_tokens": 1234}));
    assert_eq!(r.header("x-pool-account"), Some("work"));
    assert_eq!(r.header("request-id"), Some("req_up_count"));

    let seen = fake.requests_to(COUNT);
    assert_eq!(seen.len(), 1);
    let up = &seen[0];
    assert_eq!(up.method, "POST");
    assert_eq!(
        up.header("authorization"),
        Some(format!("Bearer {ACCESS}").as_str())
    );
    assert!(up.header("x-api-key").is_none());
    assert_eq!(up.header("anthropic-version"), Some("2023-06-01"));
    assert_eq!(
        up.header("anthropic-beta"),
        Some("oauth-2025-04-20,token-counting-2024-11-01")
    );
    assert_eq!(&up.body[..], &raw[..], "request bytes forwarded unchanged");
    // The messages endpoint was not touched, and nothing the client sent leaks upstream.
    assert!(fake.requests_to("/v1/messages").is_empty());
    for (_, v) in up.headers.iter() {
        assert!(
            !v.to_str().unwrap_or("").contains(PROXY_KEY),
            "client key leaked"
        );
    }
}

#[tokio::test]
async fn proxy_key_is_enforced_like_messages() {
    let fake = FakeAnthropic::start().await;
    counted(&fake, 5);
    let pool = Pool::new();
    let booted = pool
        .boot(Boot {
            proxy_key: Some(PROXY_KEY.into()),
            ..Boot::default().provider(anthropic(&pool, &fake))
        })
        .await;
    let body = count_body("claude-sonnet-5");
    for headers in [
        vec![],
        vec![("authorization", "Bearer wrong")],
        vec![("x-api-key", "wrong")],
    ] {
        let r = post(booted.addr, &body, &headers).await;
        assert_eq!(r.status, 401);
        assert_eq!(r.json()["error"]["type"], "authentication_error");
    }
    // Auth comes before body parsing, and nothing went upstream.
    let r = http_req(
        booted.addr,
        "POST",
        COUNT,
        &[("content-type", "application/json")],
        b"not json",
    )
    .await;
    assert_eq!(r.status, 401);
    assert!(fake.requests().is_empty());

    let bearer = format!("Bearer {PROXY_KEY}");
    for h in [("authorization", bearer.as_str()), ("x-api-key", PROXY_KEY)] {
        let r = post(booted.addr, &body, &[h]).await;
        assert_eq!(r.status, 200, "{}", r.text());
    }
    // Wrong method answers 405 with Allow, like messages; a browser Origin is refused.
    let r = http_req(booted.addr, "GET", COUNT, &[], b"").await;
    assert_eq!(r.status, 405);
    assert_eq!(r.header("allow"), Some("POST"));
    let r = post(
        booted.addr,
        &body,
        &[("origin", "http://evil.example"), ("x-api-key", PROXY_KEY)],
    )
    .await;
    assert_eq!(r.status, 403);
}

#[tokio::test]
async fn malformed_body_is_a_400_journaled_as_invalid_request() {
    let fake = FakeAnthropic::start().await;
    let pool = Pool::new();
    let booted = pool
        .boot(Boot::default().provider(anthropic(&pool, &fake)))
        .await;
    let r = http_req(
        booted.addr,
        "POST",
        COUNT,
        &[("content-type", "application/json")],
        b"{nope",
    )
    .await;
    assert_eq!(r.status, 400);
    assert_eq!(r.json()["error"]["type"], "invalid_request_error");
    let j = journaled(booted.addr, r.header(REQUEST_ID_HEADER).unwrap()).await;
    assert_eq!(j["request"]["reason"], "invalid_request");
    assert!(fake.requests().is_empty());
}

#[tokio::test]
async fn no_sticky_session_no_served_attempt_and_a_journal_row_without_attempts() {
    let fake = FakeAnthropic::start().await;
    counted(&fake, 7);
    let pool = Pool::new();
    let booted = pool
        .boot(Boot::default().provider(anthropic(&pool, &fake)))
        .await;
    let r = post(booted.addr, &count_body("claude-sonnet-5"), &[]).await;
    assert_eq!(r.status, 200, "{}", r.text());

    // No pin was written for the conversation.
    assert!(not_pinned(&booted));
    // The account was not "used": no success, no request, no tokens, no last-used bump.
    let st = pool.state.state(&pool.meta.id, OffsetDateTime::now_utc());
    assert_eq!(st.lifetime.requests, 0);
    assert_eq!(st.lifetime.successes, 0);
    assert_eq!(st.window_counters.requests, 0);
    assert!(st.last_used_at.is_none() && st.last_success_at.is_none());

    // The journal has the request (visible, not "unknown endpoint") but records no attempt, so
    // per-account attempt counts and error rates are unaffected.
    let j = journaled(booted.addr, r.header(REQUEST_ID_HEADER).unwrap()).await;
    assert_eq!(j["request"]["path"], COUNT);
    assert_eq!(j["request"]["outcome"], "completed");
    assert_eq!(j["request"]["status"], 200);
    assert_eq!(j["request"]["model"], "claude-sonnet-5");
    assert_eq!(j["request"]["provider"], "anthropic");
    assert!(j["request"]["reason"].is_null());
    assert!(j["attempts"].as_array().unwrap().is_empty());
    let errs = http_req(
        booted.addr,
        "GET",
        "/api/v1/requests?errors_only=true",
        &[MGMT],
        b"",
    )
    .await;
    assert!(errs.json()["requests"].as_array().unwrap().is_empty());
    assert!(
        !captured_logs()
            .lines()
            .any(|l| l.contains("unknown endpoint") && l.contains(COUNT)),
        "count_tokens must not be logged as an unknown endpoint"
    );
}

#[tokio::test]
async fn quota_429_fails_over_to_the_next_account() {
    let fake = FakeAnthropic::start().await;
    fake.route(COUNT, |r, _| {
        if r.header("authorization") == Some(format!("Bearer {ACCESS}").as_str()) {
            Reply::error(429, "rate_limit_error", "Rate limited").header("retry-after", "120")
        } else {
            Reply::json(200, &json!({"input_tokens": 99}))
        }
    });
    let pool = Pool::new();
    pool.add_account("second", ProviderId::Anthropic, 200, "second-access-SECRET");
    let booted = pool
        .boot(Boot::default().provider(anthropic(&pool, &fake)))
        .await;
    let r = post(booted.addr, &count_body("claude-sonnet-5"), &[]).await;
    assert_eq!(r.status, 200, "{}", r.text());
    assert_eq!(r.json(), json!({"input_tokens": 99}));
    assert_eq!(r.header("x-pool-account"), Some("second"));
    assert_eq!(fake.requests_to(COUNT).len(), 2);
    // The quota hit benched the first account, as it would on /v1/messages.
    let st = pool.state.state(&pool.meta.id, OffsetDateTime::now_utc());
    assert!(st.rate_limited_until.is_some());
    assert!(not_pinned(&booted));
}

#[tokio::test]
async fn unauthorized_refreshes_and_retries_once() {
    let fake = FakeAnthropic::start().await;
    fake.route(COUNT, |r, _| {
        if r.header("authorization") == Some("Bearer new-access-SECRET") {
            Reply::json(200, &json!({"input_tokens": 3}))
        } else {
            Reply::error(401, "authentication_error", "invalid token")
        }
    });
    fake.script(
        TOKEN,
        vec![Reply::json(
            200,
            &json!({"access_token": "new-access-SECRET", "refresh_token": "new-refresh-SECRET", "expires_in": 3600}),
        )],
    );
    let pool = Pool::new();
    let booted = pool
        .boot(Boot::default().provider(anthropic(&pool, &fake)))
        .await;
    let r = post(booted.addr, &count_body("claude-sonnet-5"), &[]).await;
    assert_eq!(r.status, 200, "{}", r.text());
    assert_eq!(r.json(), json!({"input_tokens": 3}));
    assert_eq!(fake.requests_to(TOKEN).len(), 1);
    assert_eq!(fake.requests_to(COUNT).len(), 2);
}

#[tokio::test]
async fn upstream_client_error_is_replayed_without_blaming_the_account() {
    let fake = FakeAnthropic::start().await;
    fake.script(
        COUNT,
        vec![Reply::error(
            400,
            "invalid_request_error",
            "messages: required",
        )],
    );
    let pool = Pool::new();
    let booted = pool
        .boot(Boot::default().provider(anthropic(&pool, &fake)))
        .await;
    let r = post(booted.addr, &count_body("claude-sonnet-5"), &[]).await;
    assert_eq!(r.status, 400);
    assert_eq!(r.json()["error"]["type"], "invalid_request_error");
    assert_eq!(fake.requests_to(COUNT).len(), 1);
    let st = pool.state.state(&pool.meta.id, OffsetDateTime::now_utc());
    assert_eq!(st.lifetime.errors, 0);
    assert!(st.last_error.is_none());
}

#[tokio::test]
async fn no_account_is_an_anthropic_503() {
    let fake = FakeAnthropic::start().await;
    let pool = Pool::new().without_default_account();
    let booted = pool
        .boot(Boot::default().provider(anthropic(&pool, &fake)))
        .await;
    let r = post(booted.addr, &count_body("claude-sonnet-5"), &[]).await;
    assert_eq!(r.status, 503);
    assert_eq!(r.json()["error"]["type"], "overloaded_error");
    let j = journaled(booted.addr, r.header(REQUEST_ID_HEADER).unwrap()).await;
    assert_eq!(j["request"]["reason"], "no_account");
}

#[tokio::test]
async fn codex_model_gets_a_local_estimate_and_never_reaches_a_provider() {
    let fake = FakeAnthropic::start().await;
    counted(&fake, 1);
    let pool = Pool::new();
    let booted = pool
        .boot(Boot::default().provider(anthropic(&pool, &fake)))
        .await;
    let body = count_body("gpt-6-sol");
    let raw = serde_json::to_vec(&body).unwrap();
    let r = post(booted.addr, &body, &[]).await;
    assert_eq!(r.status, 200, "{}", r.text());
    // bytes / 4 of the request body, rounded up.
    assert_eq!(
        r.json(),
        json!({"input_tokens": (raw.len() as u64).div_ceil(4)})
    );
    assert_eq!(r.header("x-pool-estimate"), Some("1"));
    assert!(
        fake.requests().is_empty(),
        "no upstream call for an estimate"
    );
    let j = journaled(booted.addr, r.header(REQUEST_ID_HEADER).unwrap()).await;
    assert_eq!(j["request"]["path"], COUNT);
    assert_eq!(j["request"]["outcome"], "completed");
    assert_eq!(j["request"]["provider"], "openai");
    assert!(j["attempts"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn claude_model_mapped_to_codex_with_no_claude_account_gets_an_estimate() {
    let fake_codex = FakeOpenAi::start().await;
    let pool = Pool::new().without_default_account();
    add_codex_account(&pool, "cx", 1, "codex-access-SECRET");
    let (policy, _) = fast_policy();
    let codex: Arc<dyn Provider> = boxed(codex_provider(
        &pool,
        &fake_codex,
        policy,
        Duration::from_secs(1),
    ));
    let mut models = ModelTable::defaults();
    models
        .apply_mappings(
            Some(true),
            Some(vec![Mapping {
                from: "sonnet".into(),
                to: "gpt-6-sol".into(),
                effort: None,
            }]),
        )
        .unwrap();
    let fake = FakeAnthropic::start().await;
    let booted = pool
        .boot(Boot {
            models: Some(models),
            ..Boot::default()
                .provider(anthropic(&pool, &fake))
                .provider(codex)
        })
        .await;
    let r = post(booted.addr, &count_body("claude-sonnet-5"), &[]).await;
    assert_eq!(r.status, 200, "{}", r.text());
    assert!(r.json()["input_tokens"].as_u64().unwrap() > 0);
    assert_eq!(r.header("x-pool-estimate"), Some("1"));
    assert!(fake.requests().is_empty());
    assert!(fake_codex.requests().is_empty());
}

#[tokio::test]
async fn other_unknown_paths_next_to_count_tokens_are_still_404() {
    let pool = Pool::new();
    let booted = pool.boot(Boot::default()).await;
    let r = http_req(booted.addr, "POST", "/v1/messages/batches", &[], b"{}").await;
    assert_eq!(r.status, 404);
}
