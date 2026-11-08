//! Anthropic `/v1/messages` attempt: request shaping and the INV §1 response classification.

mod support;

use std::time::Duration;

use http::HeaderMap;
use pooltime::accounts::{AccountStatus, TokenUsage};
use pooltime::errors::{FailReason, RefreshErrorKind, TransportKind};
use pooltime::providers::anthropic::AnthropicProvider;
use pooltime::providers::{AttemptCtx, AttemptOutcome, InboundRequest, Provider};
use pooltime::upstream::Limits;
use serde_json::json;
use support::*;
use time::OffsetDateTime;

async fn run(
    p: &AnthropicProvider,
    pool: &Pool,
    req: &InboundRequest,
    lim: Limits,
) -> AttemptOutcome {
    let token = p.access_token(&pool.meta, false).await.unwrap();
    let route = route(&req.model);
    let ctx = AttemptCtx {
        account: &pool.meta,
        token,
        route: &route,
        limits: lim,
        upstream: &pool.up,
        reservation: None,
    };
    p.attempt(ctx, req, &pool.state).await
}

fn kind(o: &AttemptOutcome) -> String {
    match o {
        AttemptOutcome::Done(r, _) => format!("done {}", r.status()),
        AttemptOutcome::Terminal(r, why) => format!("terminal {} {}", r.status(), why.detail()),
        AttemptOutcome::Retry {
            reason,
            fallback_response,
        } => format!("retry {} {}", fallback_response.status(), reason.detail()),
    }
}

fn response(o: AttemptOutcome) -> axum::response::Response {
    match o {
        AttemptOutcome::Done(r, _) => r,
        AttemptOutcome::Terminal(r, _) => r,
        AttemptOutcome::Retry {
            fallback_response, ..
        } => fallback_response,
    }
}

fn header<'a>(r: &'a axum::response::Response, name: &str) -> Option<&'a str> {
    r.headers().get(name).and_then(|v| v.to_str().ok())
}

fn unix(t: OffsetDateTime) -> String {
    t.unix_timestamp().to_string()
}

fn near(a: OffsetDateTime, b: OffsetDateTime) -> bool {
    (a - b).abs() < time::Duration::seconds(5)
}

fn ok_message(input: u64, output: u64) -> serde_json::Value {
    json!({
        "id": "msg_1",
        "type": "message",
        "role": "assistant",
        "content": [{"type": "text", "text": "ok"}],
        "usage": {
            "input_tokens": input,
            "output_tokens": output,
            "cache_read_input_tokens": 7,
            "cache_creation_input_tokens": 11
        }
    })
}

#[tokio::test]
async fn headerless_429_is_terminal_passthrough_without_bench() {
    let fake = FakeAnthropic::start().await;
    let upstream_body = r#"{"type":"error","error":{"type":"rate_limit_error","message":"Error"}}"#;
    fake.script(
        MESSAGES,
        vec![Reply::bytes(429, upstream_body)
            .header("content-type", "application/json")
            .header("request-id", "req_rejected")],
    );
    let pool = Pool::new();
    let (policy, sleeper) = fast_policy();
    let p = pool.provider(&fake, policy);

    let out = run(
        &p,
        &pool,
        &inbound(&simple_body(), HeaderMap::new()),
        limits(),
    )
    .await;
    assert_eq!(
        kind(&out),
        "terminal 429 Too Many Requests upstream_rejected"
    );
    let resp = response(out);
    assert_eq!(header(&resp, "x-pool-upstream-rejected"), Some("1"));
    assert_eq!(header(&resp, "request-id"), Some("req_rejected"));
    assert_eq!(&body_bytes(resp).await[..], upstream_body.as_bytes());

    // One call, no backoff, and the account is untouched: no bench, no error, no counters.
    assert_eq!(fake.requests_to(MESSAGES).len(), 1);
    assert!(sleeper.slept().is_empty());
    let now = OffsetDateTime::now_utc();
    let st = pool.state.state(&pool.meta.id, now);
    assert_eq!(st.rate_limited_until, None);
    assert!(st.last_error.is_none());
    assert_eq!(st.lifetime.errors, 0);
    assert_eq!(
        pool.state.status(&pool.meta.id, true, now),
        AccountStatus::Ready
    );
}

#[tokio::test]
async fn quota_429_benches_with_reset_precedence() {
    let now = OffsetDateTime::now_utc();
    let window_reset = now + time::Duration::hours(2);
    let quota_body = r#"{"type":"error","error":{"type":"rate_limit_error","message":"Error"}}"#;

    // 1. Explicit retry-after beats a spent window's reset.
    // 2. No explicit reset: the spent account-wide window's reset (and its key).
    // 3. Neither: the configured cooldown (1 h default).
    let cases: Vec<(Reply, OffsetDateTime, Option<&str>)> = vec![
        (
            Reply::bytes(429, quota_body)
                .header("retry-after", "120")
                .header("anthropic-ratelimit-unified-5h-status", "rejected")
                .header("anthropic-ratelimit-unified-5h-reset", &unix(window_reset)),
            now + time::Duration::seconds(120),
            None,
        ),
        (
            Reply::bytes(429, quota_body)
                .header("anthropic-ratelimit-unified-status", "rejected")
                .header("anthropic-ratelimit-unified-5h-status", "rejected")
                .header("anthropic-ratelimit-unified-5h-utilization", "1.0")
                .header("anthropic-ratelimit-unified-5h-reset", &unix(window_reset))
                .header("anthropic-ratelimit-unified-7d-status", "allowed")
                .header(
                    "anthropic-ratelimit-unified-7d-reset",
                    &unix(now + time::Duration::days(3)),
                ),
            window_reset,
            Some("5h"),
        ),
        (
            Reply::error(
                429,
                "rate_limit_error",
                "Rate limit reached for this account",
            ),
            now + time::Duration::hours(1),
            None,
        ),
    ];

    for (i, (reply, want_until, want_window)) in cases.into_iter().enumerate() {
        let fake = FakeAnthropic::start().await;
        fake.script(MESSAGES, vec![reply]);
        let pool = Pool::new();
        let (policy, sleeper) = fast_policy();
        let p = pool.provider(&fake, policy);

        let out = run(
            &p,
            &pool,
            &inbound(&simple_body(), HeaderMap::new()),
            limits(),
        )
        .await;
        let AttemptOutcome::Retry {
            reason: FailReason::Quota { reset: Some(reset) },
            fallback_response,
        } = out
        else {
            panic!("case {i}: expected quota retry, got {}", kind(&out));
        };
        assert!(
            near(reset, want_until),
            "case {i}: reset {reset} vs {want_until}"
        );
        assert_eq!(fallback_response.status(), 429, "case {i}");

        let st = pool.state.state(&pool.meta.id, OffsetDateTime::now_utc());
        let until = st.rate_limited_until.expect("benched");
        assert!(
            near(until, want_until),
            "case {i}: until {until} vs {want_until}"
        );
        assert_eq!(until, reset, "case {i}");
        assert_eq!(st.rate_limited_window.as_deref(), want_window, "case {i}");
        assert!(matches!(
            pool.state
                .status(&pool.meta.id, true, OffsetDateTime::now_utc()),
            AccountStatus::RateLimited { .. }
        ));
        assert_eq!(
            fake.requests_to(MESSAGES).len(),
            1,
            "case {i}: no same-account retry"
        );
        assert!(sleeper.slept().is_empty());
        if i == 1 {
            let snap = st.snapshot.expect("headers merged into snapshot");
            assert_eq!(snap.windows.len(), 2);
        }
    }
}

#[tokio::test]
async fn overload_retries_same_account_then_replays_upstream_body() {
    for status in [529u16, 500, 503] {
        let fake = FakeAnthropic::start().await;
        let body = format!(
            r#"{{"type":"error","error":{{"type":"overloaded_error","message":"Overloaded {status}"}}}}"#
        );
        fake.script(
            MESSAGES,
            vec![Reply::bytes(status, body.clone())
                .header("content-type", "application/json")
                .header("content-encoding", "identity")
                .header("request-id", "req_xyz")
                .header("x-upstream-diag", "kept")],
        );
        let pool = Pool::new();
        let (policy, sleeper) = fast_policy();
        let p = pool.provider(&fake, policy);

        let out = run(
            &p,
            &pool,
            &inbound(&simple_body(), HeaderMap::new()),
            limits(),
        )
        .await;
        let AttemptOutcome::Terminal(resp, FailReason::Overloaded) = out else {
            panic!("{status}: expected surfaced overload, got {}", kind(&out));
        };
        assert_eq!(resp.status(), status);
        assert_eq!(header(&resp, "request-id"), Some("req_xyz"));
        assert_eq!(header(&resp, "x-upstream-diag"), Some("kept"));
        assert_eq!(header(&resp, "x-pool-account"), Some("work"));
        assert_eq!(header(&resp, "content-encoding"), None);
        assert_eq!(header(&resp, "content-length"), None);
        assert_eq!(header(&resp, "content-type"), Some("application/json"));
        assert_eq!(&body_bytes(resp).await[..], body.as_bytes());

        // 1 + 4 retries, all on this account, full-jitter ceilings 0.5/1/2/4 s.
        let calls = fake.requests_to(MESSAGES);
        assert_eq!(calls.len(), 5, "{status}");
        assert!(calls
            .iter()
            .all(|c| c.header("authorization") == Some(&format!("Bearer {ACCESS}"))));
        let ms: Vec<u128> = sleeper.slept().iter().map(|d| d.as_millis()).collect();
        assert_eq!(ms, vec![500, 1000, 2000, 4000]);

        // Recorded once, never sidelined.
        let now = OffsetDateTime::now_utc();
        let st = pool.state.state(&pool.meta.id, now);
        assert_eq!(st.lifetime.errors, 1);
        let err = st.last_error.expect("recorded");
        assert_eq!(err.reason, FailReason::Overloaded);
        assert_eq!(err.upstream_request_id.as_deref(), Some("req_xyz"));
        assert_eq!(
            pool.state.status(&pool.meta.id, true, now),
            AccountStatus::Ready
        );
    }

    // A transient overload that clears returns the eventual success from the same account.
    let fake = FakeAnthropic::start().await;
    fake.script(
        MESSAGES,
        vec![
            Reply::error(529, "overloaded_error", "Overloaded"),
            Reply::json(200, &ok_message(2, 3)),
        ],
    );
    let pool = Pool::new();
    let (policy, sleeper) = fast_policy();
    let p = pool.provider(&fake, policy);
    let out = run(
        &p,
        &pool,
        &inbound(&simple_body(), HeaderMap::new()),
        limits(),
    )
    .await;
    assert_eq!(kind(&out), "done 200 OK");
    assert_eq!(sleeper.slept().len(), 1);
    let st = pool.state.state(&pool.meta.id, OffsetDateTime::now_utc());
    assert_eq!((st.lifetime.successes, st.lifetime.errors), (1, 0));
}

#[tokio::test]
async fn retry_after_header_wins_capped() {
    // retry-after beyond the cap sleeps the cap; the upstream header is replayed as-is.
    for (ra, want) in [("30", 8000u128), ("3", 3000)] {
        let fake = FakeAnthropic::start().await;
        fake.script(
            MESSAGES,
            vec![Reply::error(529, "overloaded_error", "Overloaded").header("retry-after", ra)],
        );
        let pool = Pool::new();
        let (policy, sleeper) = fast_policy();
        let p = pool.provider(&fake, policy);
        let out = run(
            &p,
            &pool,
            &inbound(&simple_body(), HeaderMap::new()),
            limits(),
        )
        .await;
        assert_eq!(kind(&out), "terminal 529 <unknown status code> overloaded");
        let resp = response(out);
        assert_eq!(header(&resp, "retry-after"), Some(ra));
        let ms: Vec<u128> = sleeper.slept().iter().map(|d| d.as_millis()).collect();
        assert_eq!(ms, vec![want; 4], "retry-after {ra}");
    }

    // A reset known only from a reset header: delays capped, retry-after synthesized from it.
    let fake = FakeAnthropic::start().await;
    let reset = OffsetDateTime::now_utc() + time::Duration::seconds(20);
    let reset_hdr = reset
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap();
    fake.script(
        MESSAGES,
        vec![Reply::error(529, "overloaded_error", "Overloaded")
            .header("anthropic-ratelimit-tokens-reset", &reset_hdr)],
    );
    let pool = Pool::new();
    let (policy, sleeper) = fast_policy();
    let p = pool.provider(&fake, policy);
    let out = run(
        &p,
        &pool,
        &inbound(&simple_body(), HeaderMap::new()),
        limits(),
    )
    .await;
    let resp = response(out);
    let ra: i64 = header(&resp, "retry-after").unwrap().parse().unwrap();
    assert!((18..=21).contains(&ra), "synthesized retry-after {ra}");
    assert!(sleeper.slept().iter().all(|d| *d == Duration::from_secs(8)));

    // The attempt deadline wins over the backoff: surface instead of sleeping past it.
    let fake = FakeAnthropic::start().await;
    fake.script(
        MESSAGES,
        vec![Reply::error(529, "overloaded_error", "Overloaded").header("retry-after", "3")],
    );
    let pool = Pool::new();
    let (policy, sleeper) = fast_policy();
    let p = pool.provider(&fake, policy);
    let lim = Limits {
        deadline: tokio::time::Instant::now() + Duration::from_secs(2),
        ..limits()
    };
    let out = run(&p, &pool, &inbound(&simple_body(), HeaderMap::new()), lim).await;
    assert_eq!(kind(&out), "terminal 529 <unknown status code> overloaded");
    assert_eq!(fake.requests_to(MESSAGES).len(), 1);
    assert!(sleeper.slept().is_empty());
}

#[tokio::test]
async fn forbidden_entitlement_vs_generic() {
    let cases = [
        (
            "OAuth authentication is currently not allowed for this organization.",
            FailReason::Billing,
        ),
        (
            "Request blocked by safety filter.",
            FailReason::AccessDenied,
        ),
    ];
    for (msg, want) in cases {
        let fake = FakeAnthropic::start().await;
        fake.script(
            MESSAGES,
            vec![Reply::error(403, "permission_error", msg).header("request-id", "req_403")],
        );
        let pool = Pool::new();
        let (policy, _) = fast_policy();
        let p = pool.provider(&fake, policy);
        let out = run(
            &p,
            &pool,
            &inbound(&simple_body(), HeaderMap::new()),
            limits(),
        )
        .await;
        let AttemptOutcome::Retry {
            reason,
            fallback_response,
        } = out
        else {
            panic!("{msg}: expected retry, got {}", kind(&out));
        };
        assert_eq!(reason, want, "{msg}");
        assert_eq!(fallback_response.status(), 403);
        // The fallback replays the upstream error (to the client only).
        let body: serde_json::Value =
            serde_json::from_slice(&body_bytes(fallback_response).await).unwrap();
        assert_eq!(body["error"]["message"], msg);

        let now = OffsetDateTime::now_utc();
        let st = pool.state.state(&pool.meta.id, now);
        assert!(st.access_denied_until.is_some_and(|u| u > now), "{msg}");
        assert_eq!(st.billing_blocked_at.is_some(), want == FailReason::Billing);
        let status = pool.state.status(&pool.meta.id, true, now);
        match want {
            FailReason::Billing => assert!(matches!(status, AccountStatus::Billing)),
            _ => assert!(matches!(status, AccountStatus::AccessDenied { .. })),
        }
        assert_eq!(fake.requests_to(MESSAGES).len(), 1);
    }
}

#[tokio::test]
async fn unauthorized_forces_refresh_then_retries_once() {
    let token_ok = || {
        Reply::json(
            200,
            &json!({"access_token": "new-access-SECRET", "refresh_token": "new-refresh-SECRET", "expires_in": 3600}),
        )
    };

    // 401 on the stale token, refresh, success on the new one.
    let fake = FakeAnthropic::start().await;
    fake.route(MESSAGES, |r, _| {
        if r.header("authorization") == Some("Bearer new-access-SECRET") {
            Reply::json(200, &ok_message(1, 1))
        } else {
            Reply::error(401, "authentication_error", "invalid token")
        }
    });
    fake.script(TOKEN, vec![token_ok()]);
    let pool = Pool::new();
    let (policy, _) = fast_policy();
    let p = pool.provider(&fake, policy);
    let out = run(
        &p,
        &pool,
        &inbound(&simple_body(), HeaderMap::new()),
        limits(),
    )
    .await;
    assert_eq!(kind(&out), "done 200 OK");
    let auths: Vec<_> = fake
        .requests_to(MESSAGES)
        .iter()
        .map(|r| r.header("authorization").unwrap().to_string())
        .collect();
    assert_eq!(
        auths,
        vec![
            format!("Bearer {ACCESS}"),
            "Bearer new-access-SECRET".to_string()
        ]
    );
    assert_eq!(fake.requests_to(TOKEN).len(), 1);
    assert_eq!(pool.creds().access_token, "new-access-SECRET");

    // 401 again after the refresh: terminal, recorded, exactly one retry.
    let fake = FakeAnthropic::start().await;
    fake.script(
        MESSAGES,
        vec![Reply::error(401, "authentication_error", "invalid token")],
    );
    fake.script(TOKEN, vec![token_ok()]);
    let pool = Pool::new();
    let (policy, _) = fast_policy();
    let p = pool.provider(&fake, policy);
    let out = run(
        &p,
        &pool,
        &inbound(&simple_body(), HeaderMap::new()),
        limits(),
    )
    .await;
    assert_eq!(kind(&out), "terminal 401 Unauthorized status:401");
    assert_eq!(fake.requests_to(MESSAGES).len(), 2);
    assert_eq!(fake.requests_to(TOKEN).len(), 1);
    let st = pool.state.state(&pool.meta.id, OffsetDateTime::now_utc());
    assert_eq!(st.last_error.unwrap().reason, FailReason::Status(401));
    // bro parity (INV §1): the persistent 401 changes no status; the account stays ready
    // (an `erroring` state comes later, spec §16; open item in docs/parity.md §10.3).
    assert_eq!(
        pool.state
            .status(&pool.meta.id, true, OffsetDateTime::now_utc()),
        AccountStatus::Ready
    );

    // Refresh fails: 401 authentication_error fallback, fail over.
    let fake = FakeAnthropic::start().await;
    fake.script(
        MESSAGES,
        vec![Reply::error(401, "authentication_error", "invalid token")],
    );
    fake.script(
        TOKEN,
        vec![Reply::json(400, &json!({"error": "invalid_grant"}))],
    );
    let pool = Pool::new();
    let (policy, _) = fast_policy();
    let p = pool.provider(&fake, policy);
    let out = run(
        &p,
        &pool,
        &inbound(&simple_body(), HeaderMap::new()),
        limits(),
    )
    .await;
    let AttemptOutcome::Retry {
        reason: FailReason::Refresh(RefreshErrorKind::InvalidGrant),
        fallback_response,
    } = out
    else {
        panic!("expected refresh retry, got {}", kind(&out));
    };
    assert_eq!(fallback_response.status(), 401);
    let body: serde_json::Value =
        serde_json::from_slice(&body_bytes(fallback_response).await).unwrap();
    assert_eq!(body["type"], "error");
    assert_eq!(body["error"]["type"], "authentication_error");
    assert_eq!(fake.requests_to(MESSAGES).len(), 1);
}

#[tokio::test]
async fn unauthorized_with_already_rotated_token_resends_without_refresh() {
    // The caller's token is stale; the store already holds a fresh one (another request
    // refreshed meanwhile): resend with the stored token, no token-endpoint call.
    let fake = FakeAnthropic::start().await;
    fake.route(MESSAGES, |r, _| {
        if r.header("authorization") == Some(&format!("Bearer {ACCESS}")) {
            Reply::json(200, &ok_message(1, 1))
        } else {
            Reply::error(401, "authentication_error", "invalid token")
        }
    });
    fake.script(
        TOKEN,
        vec![Reply::error(500, "api_error", "must not be called")],
    );
    let pool = Pool::new();
    let (policy, _) = fast_policy();
    let p = pool.provider(&fake, policy);
    let req = inbound(&simple_body(), HeaderMap::new());
    let route = route(&req.model);
    let ctx = AttemptCtx {
        account: &pool.meta,
        token: "stale-access-SECRET".into(),
        route: &route,
        limits: limits(),
        upstream: &pool.up,
        reservation: None,
    };
    let out = p.attempt(ctx, &req, &pool.state).await;
    assert_eq!(kind(&out), "done 200 OK");
    let auths: Vec<_> = fake
        .requests_to(MESSAGES)
        .iter()
        .map(|r| r.header("authorization").unwrap().to_string())
        .collect();
    assert_eq!(
        auths,
        vec![
            "Bearer stale-access-SECRET".to_string(),
            format!("Bearer {ACCESS}")
        ]
    );
    assert_eq!(fake.requests_to(TOKEN).len(), 0);
    assert_eq!(pool.creds().access_token, ACCESS);
}

#[tokio::test]
async fn rate_limit_wording_on_non_429_is_quota() {
    // bro: for a non-429 status, rate-limit wording in "{type}\n{message}" means quota, checked
    // before the overload (529/500/503) and 403 rules.
    let cases = [
        Reply::error(403, "rate_limit_error", "Forbidden"),
        Reply::error(529, "overloaded_error", "Rate limit exceeded, slow down"),
        Reply::error(
            400,
            "invalid_request_error",
            "Your usage limit reached for today",
        ),
    ];
    for reply in cases {
        let status = reply.status;
        let fake = FakeAnthropic::start().await;
        fake.script(MESSAGES, vec![reply]);
        let pool = Pool::new();
        let (policy, sleeper) = fast_policy();
        let p = pool.provider(&fake, policy);
        let out = run(
            &p,
            &pool,
            &inbound(&simple_body(), HeaderMap::new()),
            limits(),
        )
        .await;
        let AttemptOutcome::Retry {
            reason: FailReason::Quota { reset: Some(reset) },
            fallback_response,
        } = out
        else {
            panic!("{status}: expected quota retry, got {}", kind(&out));
        };
        assert_eq!(fallback_response.status(), status);
        let now = OffsetDateTime::now_utc();
        assert!(
            near(reset, now + time::Duration::hours(1)),
            "{status}: cooldown"
        );
        let st = pool.state.state(&pool.meta.id, now);
        assert_eq!(st.rate_limited_until, Some(reset));
        assert_eq!(st.access_denied_until, None, "{status}: not access-denied");
        assert!(matches!(
            pool.state.status(&pool.meta.id, true, now),
            AccountStatus::RateLimited { .. }
        ));
        assert_eq!(fake.requests_to(MESSAGES).len(), 1, "{status}: no backoff");
        assert!(sleeper.slept().is_empty());
    }
}

#[tokio::test]
async fn transport_error_is_retry_with_flag() {
    // Nothing listening: connect failure.
    let closed = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap()
    };
    let fake = FakeAnthropic::start().await;
    let pool = Pool::new();
    let (policy, _) = fast_policy();
    let cfg = pooltime::config::AnthropicCfg {
        api_base: format!("http://{closed}"),
        token_url: format!("{}{}", fake.url(), TOKEN),
        ..Default::default()
    };
    let p = AnthropicProvider::new(cfg, pool.reg.clone(), pool.state.clone(), pool.up.clone())
        .with_overload_policy(policy);
    let out = run(
        &p,
        &pool,
        &inbound(&simple_body(), HeaderMap::new()),
        limits(),
    )
    .await;
    let AttemptOutcome::Retry {
        reason: FailReason::Transport(TransportKind::Connect),
        fallback_response,
    } = out
    else {
        panic!("expected transport retry, got {}", kind(&out));
    };
    assert_eq!(fallback_response.status(), 502);
    assert_eq!(
        header(&fallback_response, "x-pool-retryable-transport"),
        Some("1")
    );
    let body: serde_json::Value =
        serde_json::from_slice(&body_bytes(fallback_response).await).unwrap();
    assert_eq!(body["error"]["type"], "api_error");
    let st = pool.state.state(&pool.meta.id, OffsetDateTime::now_utc());
    assert_eq!(
        st.last_error.unwrap().reason,
        FailReason::Transport(TransportKind::Connect)
    );

    // A 200 whose body dies before completion fails over instead of returning a 502.
    let fake = FakeAnthropic::start().await;
    fake.script(
        MESSAGES,
        vec![Reply::json(200, &ok_message(1, 1)).drop_after_bytes(10)],
    );
    let pool = Pool::new();
    let (policy, _) = fast_policy();
    let p = pool.provider(&fake, policy);
    let out = run(
        &p,
        &pool,
        &inbound(&simple_body(), HeaderMap::new()),
        limits(),
    )
    .await;
    let AttemptOutcome::Retry {
        reason: FailReason::Transport(_),
        fallback_response,
    } = out
    else {
        panic!("expected transport retry, got {}", kind(&out));
    };
    assert_eq!(fallback_response.status(), 502);
    assert_eq!(
        header(&fallback_response, "x-pool-retryable-transport"),
        Some("1")
    );
    let st = pool.state.state(&pool.meta.id, OffsetDateTime::now_utc());
    assert_eq!(st.lifetime.successes, 0);
    assert!(matches!(
        st.last_error.unwrap().reason,
        FailReason::Transport(_)
    ));
}

#[tokio::test]
async fn hop_by_hop_and_x_api_key_stripped_and_auth_swapped() {
    let fake = FakeAnthropic::start().await;
    fake.script(MESSAGES, vec![Reply::json(200, &ok_message(1, 1))]);
    let pool = Pool::new();
    let (policy, _) = fast_policy();
    let p = pool.provider(&fake, policy);

    let mut h = HeaderMap::new();
    for (k, v) in [
        ("connection", "x-hop-secret"),
        ("keep-alive", "timeout=5"),
        ("proxy-authorization", "Basic cHJveHk="),
        ("proxy-authenticate", "Basic"),
        ("te", "trailers"),
        ("trailer", "x-trailer"),
        ("upgrade", "h2c"),
        ("host", "localhost:3460"),
        ("content-length", "99999"),
        ("x-api-key", "pool-local-key-SECRET"),
        ("authorization", "Bearer pool-local-key-SECRET"),
        ("accept-encoding", "gzip, br"),
        ("anthropic-version", "2023-06-01"),
        ("anthropic-beta", "oauth-2025-04-20,claude-code-20250219"),
        ("user-agent", "claude-cli/2.1.319"),
        ("x-custom", "passes"),
    ] {
        h.insert(http::HeaderName::from_static(k), v.parse().unwrap());
    }
    let req = inbound(&simple_body(), h);
    let out = run(&p, &pool, &req, limits()).await;
    assert_eq!(kind(&out), "done 200 OK");

    let got = &fake.requests_to(MESSAGES)[0];
    assert_eq!(got.method, "POST");
    assert_eq!(got.path, "/v1/messages");
    for gone in [
        "keep-alive",
        "proxy-authorization",
        "proxy-authenticate",
        "te",
        "trailer",
        "upgrade",
        "x-api-key",
    ] {
        assert_eq!(got.header(gone), None, "{gone} must not be forwarded");
    }
    assert_ne!(got.header("connection"), Some("x-hop-secret"));
    assert_eq!(got.header("host"), Some(fake.addr.to_string().as_str()));
    assert_eq!(
        got.header("content-length"),
        Some(req.raw_body.len().to_string().as_str())
    );
    let bearer = format!("Bearer {ACCESS}");
    assert_eq!(got.header("authorization"), Some(bearer.as_str()));
    assert_eq!(got.headers.get_all("authorization").iter().count(), 1);
    // No decompression on our side, so ask upstream for identity bytes.
    assert_eq!(got.header("accept-encoding"), Some("identity"));
    assert_eq!(got.header("anthropic-version"), Some("2023-06-01"));
    assert_eq!(
        got.header("anthropic-beta"),
        Some("oauth-2025-04-20,claude-code-20250219")
    );
    assert_eq!(got.header("user-agent"), Some("claude-cli/2.1.319"));
    assert_eq!(got.header("x-custom"), Some("passes"));
    // Untouched body is forwarded byte-for-byte.
    assert_eq!(got.body, req.raw_body);
}

#[tokio::test]
async fn anthropic_version_not_synthesized() {
    let fake = FakeAnthropic::start().await;
    fake.script(MESSAGES, vec![Reply::json(200, &ok_message(1, 1))]);
    let pool = Pool::new();
    let (policy, _) = fast_policy();
    let p = pool.provider(&fake, policy);
    let out = run(
        &p,
        &pool,
        &inbound(&simple_body(), HeaderMap::new()),
        limits(),
    )
    .await;
    assert_eq!(kind(&out), "done 200 OK");
    let got = &fake.requests_to(MESSAGES)[0];
    assert_eq!(got.header("anthropic-version"), None);
    assert_eq!(got.header("anthropic-beta"), None);
}

#[tokio::test]
async fn success_records_usage_from_json() {
    let fake = FakeAnthropic::start().await;
    let upstream_body = serde_json::to_vec_pretty(&ok_message(12, 34)).unwrap();
    fake.script(
        MESSAGES,
        vec![Reply::bytes(200, upstream_body.clone())
            .header("content-type", "application/json")
            .header("request-id", "req_ok")
            .header("anthropic-ratelimit-unified-5h-status", "allowed")
            .header("anthropic-ratelimit-unified-5h-utilization", "0.25")
            .header(
                "anthropic-ratelimit-unified-5h-reset",
                &unix(OffsetDateTime::now_utc() + time::Duration::hours(3)),
            )],
    );
    let pool = Pool::new();
    let (policy, _) = fast_policy();
    let p = pool.provider(&fake, policy);
    let out = run(
        &p,
        &pool,
        &inbound(&simple_body(), HeaderMap::new()),
        limits(),
    )
    .await;
    let AttemptOutcome::Done(resp, usage) = out else {
        panic!("expected done, got {}", kind(&out));
    };
    assert_eq!(resp.status(), 200);
    assert_eq!(header(&resp, "request-id"), Some("req_ok"));
    assert_eq!(header(&resp, "x-pool-account"), Some("work"));
    assert_eq!(&body_bytes(resp).await[..], &upstream_body[..]);
    let want = TokenUsage {
        input_tokens: 12,
        output_tokens: 34,
        cache_read: 7,
        cache_write: 11,
    };
    assert_eq!(usage.expect("usage future").await.unwrap(), want);

    let st = pool.state.state(&pool.meta.id, OffsetDateTime::now_utc());
    assert_eq!(st.lifetime.successes, 1);
    assert_eq!(st.lifetime.input_tokens, 12);
    assert_eq!(st.lifetime.output_tokens, 34);
    assert_eq!(st.lifetime.cache_read, 7);
    assert_eq!(st.lifetime.cache_write, 11);
    assert!(st.last_success_at.is_some());
    let snap = st.snapshot.expect("rate-limit headers merged");
    assert_eq!(snap.windows[0].key, "5h");
    assert_eq!(snap.windows[0].utilization, 0.25);
}

#[tokio::test]
async fn upstream_error_message_not_persisted() {
    capture_logs();
    const SECRET: &str = "PROMPT-ECHO-7f3a9c";
    let msg = format!("Something about your prompt: {SECRET}");
    let replies = [
        Reply::error(400, "invalid_request_error", &msg).header("request-id", "req_400"),
        Reply::error(403, "permission_error", &msg),
        Reply::error(403, "permission_error", &format!("organization {msg}")),
        Reply::error(429, "rate_limit_error", &msg),
        Reply::error(429, "rate_limit_error", &format!("rate limit {msg}")),
        Reply::error(529, "overloaded_error", &msg),
        Reply::bytes(500, format!("raw text {msg}")),
    ];
    let pool = Pool::new();
    for reply in replies {
        let fake = FakeAnthropic::start().await;
        fake.script(MESSAGES, vec![reply]);
        let (policy, _) = fast_policy();
        let p = pool.provider(&fake, policy);
        let out = run(
            &p,
            &pool,
            &inbound(&simple_body(), HeaderMap::new()),
            limits(),
        )
        .await;
        // The client still sees the upstream message.
        let body = body_bytes(response(out)).await;
        assert!(String::from_utf8_lossy(&body).contains(SECRET));
    }
    pool.state.flush().await.unwrap();

    let st = pool.state.state(&pool.meta.id, OffsetDateTime::now_utc());
    assert!(!serde_json::to_string(&st).unwrap().contains(SECRET));
    let logs = captured_logs();
    assert!(
        logs.contains("anthropic upstream error") && logs.contains("upstream_rejected"),
        "log capture works"
    );
    assert!(!logs.contains(SECRET), "upstream message leaked into logs");
    // No bearer/refresh token of any test in this binary (all end in `-SECRET`).
    assert!(!logs.contains("-SECRET"), "a token leaked into logs");
    for entry in walk(pool.dir.path()) {
        let bytes = std::fs::read(&entry).unwrap();
        assert!(
            !bytes.windows(SECRET.len()).any(|w| w == SECRET.as_bytes()),
            "upstream message persisted in {}",
            entry.display()
        );
    }
}

fn walk(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    for e in std::fs::read_dir(dir).unwrap() {
        let p = e.unwrap().path();
        if p.is_dir() {
            out.extend(walk(&p));
        } else {
            out.push(p);
        }
    }
    out
}

// ---- streaming (Task 19) ----

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use bytes::Bytes;
use pooltime::providers::ProviderId;
use pooltime::routing::reserve::{Reservation, Reservations, WaitBudget};
use support::sockets::{contains, post_json, raw_client, serve};

fn stream_body() -> serde_json::Value {
    let mut b = simple_body();
    b["stream"] = json!(true);
    b
}

fn stream_req(id: &str) -> InboundRequest {
    let mut r = inbound(&stream_body(), HeaderMap::new());
    r.request_id = id.into();
    r
}

fn ev(name: &str, data: serde_json::Value) -> String {
    format!("event: {name}\ndata: {data}\n\n")
}

fn ping() -> String {
    ev("ping", json!({"type": "ping"}))
}

fn message_start() -> String {
    ev(
        "message_start",
        json!({"type": "message_start", "message": {
            "id": "msg_s", "type": "message", "role": "assistant", "content": [],
            "usage": {"input_tokens": 10, "output_tokens": 1,
                      "cache_read_input_tokens": 7, "cache_creation_input_tokens": 11}
        }}),
    )
}

fn text_delta(text: &str) -> String {
    ev(
        "content_block_delta",
        json!({"type": "content_block_delta", "index": 0,
               "delta": {"type": "text_delta", "text": text}}),
    )
}

fn message_delta(output: u64) -> String {
    ev(
        "message_delta",
        json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"},
               "usage": {"output_tokens": output}}),
    )
}

fn message_stop() -> String {
    ev("message_stop", json!({"type": "message_stop"}))
}

fn sse_error(kind: &str, message: &str) -> String {
    ev(
        "error",
        json!({"type": "error", "error": {"type": kind, "message": message}}),
    )
}

/// A complete stream with upstream pings, one event split across two frames.
fn full_stream() -> Vec<String> {
    let delta = text_delta("Hello");
    let (a, b) = delta.split_at(17);
    vec![
        message_start(),
        ping(),
        ev(
            "content_block_start",
            json!({"type": "content_block_start", "index": 0,
                   "content_block": {"type": "text", "text": ""}}),
        ),
        a.to_string(),
        b.to_string(),
        ping(),
        ev(
            "content_block_stop",
            json!({"type": "content_block_stop", "index": 0}),
        ),
        message_delta(42),
        message_stop(),
    ]
}

fn frames(fs: &[String], gap: Duration) -> Vec<(Duration, String)> {
    fs.iter().map(|f| (gap, f.clone())).collect()
}

fn reservations() -> Reservations {
    Reservations::new(HashMap::new())
}

async fn reserve(rs: &Reservations, pool: &Pool) -> Reservation {
    let id = pool.meta.id.clone();
    rs.reserve(
        move |has_room| has_room(&id).then(|| id.clone()),
        ProviderId::Anthropic,
        &WaitBudget::new(Duration::from_secs(1)),
    )
    .await
    .unwrap()
}

async fn run_stream(
    p: &AnthropicProvider,
    pool: &Pool,
    req: &InboundRequest,
    lim: Limits,
    reservation: Option<Reservation>,
) -> AttemptOutcome {
    let token = p.access_token(&pool.meta, false).await.unwrap();
    let route = route(&req.model);
    let ctx = AttemptCtx {
        account: &pool.meta,
        token,
        route: &route,
        limits: lim,
        upstream: &pool.up,
        reservation,
    };
    p.attempt(ctx, req, &pool.state).await
}

fn limits_idle(idle: Duration) -> Limits {
    Limits { idle, ..limits() }
}

/// Wait (up to 3 s) until the account has no reservation left.
async fn released(rs: &Reservations, pool: &Pool) -> bool {
    for _ in 0..300 {
        if rs.in_flight(&pool.meta.id) == 0 {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    false
}

fn state(pool: &Pool) -> pooltime::accounts::AccountState {
    pool.state.state(&pool.meta.id, OffsetDateTime::now_utc())
}

fn log_line_with(needles: &[&str]) -> bool {
    captured_logs()
        .lines()
        .any(|l| needles.iter().all(|n| l.contains(n)))
}

#[tokio::test]
async fn stream_bytes_forwarded_unchanged() {
    let fake = FakeAnthropic::start().await;
    let fs = full_stream();
    fake.script(
        MESSAGES,
        vec![Reply::sse(frames(&fs, Duration::from_millis(5)))
            .header("request-id", "req_stream_ok")
            .header("content-encoding", "identity")],
    );
    let pool = Pool::new();
    let (policy, _) = fast_policy();
    let p = pool.provider(&fake, policy);
    let rs = reservations();
    let slot = reserve(&rs, &pool).await;

    let out = run_stream(
        &p,
        &pool,
        &stream_req("req_t19_bytes"),
        limits(),
        Some(slot),
    )
    .await;
    assert_eq!(kind(&out), "done 200 OK");
    let AttemptOutcome::Done(resp, Some(usage)) = out else {
        panic!("expected Done with usage future");
    };
    assert_eq!(header(&resp, "content-type"), Some("text/event-stream"));
    assert_eq!(header(&resp, "x-pool-account"), Some("work"));
    assert_eq!(header(&resp, "request-id"), Some("req_stream_ok"));
    for h in ["content-length", "content-encoding", "transfer-encoding"] {
        assert!(resp.headers().get(h).is_none(), "{h} forwarded");
    }
    let got = body_bytes(resp).await;
    assert_eq!(
        String::from_utf8_lossy(&got),
        fs.concat(),
        "bytes must be forwarded unchanged, pings included, nothing injected"
    );
    let u = usage.await.unwrap();
    assert_eq!((u.input_tokens, u.output_tokens), (10, 42));
    assert!(released(&rs, &pool).await, "reservation not released");
    let st = state(&pool);
    assert_eq!((st.lifetime.successes, st.lifetime.errors), (1, 0));
    assert_eq!(fake.requests_to(MESSAGES)[0].json()["stream"], json!(true));
    assert!(log_line_with(&["req_t19_bytes", "completed"]));
}

#[tokio::test]
async fn ping_does_not_commit_then_rate_limit_error_fails_over() {
    let fake = FakeAnthropic::start().await;
    fake.script(
        MESSAGES,
        vec![Reply::sse(frames(
            &[
                ping(),
                ping(),
                sse_error("rate_limit_error", "Rate limited"),
            ],
            Duration::from_millis(5),
        ))
        .hold(Hold::Forever)],
    );
    let pool = Pool::new();
    let (policy, sleeper) = fast_policy();
    let p = pool.provider(&fake, policy);

    let before = OffsetDateTime::now_utc();
    let out = run_stream(&p, &pool, &stream_req("req_t19_rl"), limits(), None).await;
    assert_eq!(kind(&out), "retry 429 Too Many Requests quota");
    let resp = response(out);
    assert_eq!(header(&resp, "content-type"), Some("application/json"));
    assert_eq!(header(&resp, "x-pool-account"), Some("work"));
    let body: serde_json::Value = serde_json::from_slice(&body_bytes(resp).await).unwrap();
    assert_eq!(body["error"]["type"], "rate_limit_error");

    let st = state(&pool);
    let until = st.rate_limited_until.expect("benched");
    assert!(near(until, before + time::Duration::hours(1)));
    assert!(matches!(
        st.last_error.unwrap().reason,
        FailReason::Quota { .. }
    ));
    assert_eq!(st.lifetime.successes, 0);
    assert_eq!(fake.requests_to(MESSAGES).len(), 1);
    assert!(sleeper.slept().is_empty());
}

#[tokio::test]
async fn initial_overload_never_clears_surfaces_529() {
    let fake = FakeAnthropic::start().await;
    fake.script(
        MESSAGES,
        vec![Reply::sse(frames(
            &[ping(), sse_error("overloaded_error", "Overloaded")],
            Duration::ZERO,
        ))
        .header("request-id", "req_ovl")
        .hold(Hold::Forever)],
    );
    let pool = Pool::new();
    let (policy, sleeper) = fast_policy();
    let p = pool.provider(&fake, policy);

    let out = run_stream(&p, &pool, &stream_req("req_t19_ovl"), limits(), None).await;
    assert_eq!(kind(&out), "terminal 529 <unknown status code> overloaded");
    let resp = response(out);
    assert_eq!(header(&resp, "content-type"), Some("application/json"));
    assert_eq!(header(&resp, "x-pool-account"), Some("work"));
    let body: serde_json::Value = serde_json::from_slice(&body_bytes(resp).await).unwrap();
    assert_eq!(body["type"], "error");
    assert_eq!(body["error"]["type"], "overloaded_error");

    // Same-account backoff: 1 + 4 retries, then surfaced; recorded once, never sidelined.
    assert_eq!(fake.requests_to(MESSAGES).len(), 5);
    assert_eq!(sleeper.slept().len(), 4);
    let st = state(&pool);
    assert_eq!(st.lifetime.errors, 1);
    assert_eq!(st.last_error.unwrap().reason, FailReason::Overloaded);
    assert_eq!(
        pool.state
            .status(&pool.meta.id, true, OffsetDateTime::now_utc()),
        AccountStatus::Ready
    );
}

#[tokio::test]
async fn initial_overload_that_clears_streams_on_same_account() {
    let fake = FakeAnthropic::start().await;
    let fs = full_stream();
    fake.script(
        MESSAGES,
        vec![
            Reply::sse(frames(
                &[sse_error("overloaded_error", "Overloaded")],
                Duration::ZERO,
            )),
            Reply::sse(frames(&fs, Duration::ZERO)),
        ],
    );
    let pool = Pool::new();
    let (policy, sleeper) = fast_policy();
    let p = pool.provider(&fake, policy);

    let out = run_stream(&p, &pool, &stream_req("req_t19_ovl_ok"), limits(), None).await;
    assert_eq!(kind(&out), "done 200 OK");
    assert_eq!(
        String::from_utf8_lossy(&body_bytes(response(out)).await),
        fs.concat()
    );
    assert_eq!(sleeper.slept().len(), 1);
    let st = state(&pool);
    assert_eq!((st.lifetime.successes, st.lifetime.errors), (1, 0));
}

#[tokio::test]
async fn pre_commit_transport_fault_fails_over() {
    // Connection dropped after a ping (nothing committed).
    let fake = FakeAnthropic::start().await;
    let first = ping();
    fake.script(
        MESSAGES,
        vec![
            Reply::sse(frames(&[first.clone(), message_start()], Duration::ZERO))
                .drop_after_bytes(first.len() + 5),
        ],
    );
    let pool = Pool::new();
    let (policy, _) = fast_policy();
    let p = pool.provider(&fake, policy);
    let out = run_stream(&p, &pool, &stream_req("req_t19_drop"), limits(), None).await;
    let k = kind(&out);
    assert!(k.starts_with("retry 502 Bad Gateway transport:"), "{k}");
    let resp = response(out);
    assert_eq!(header(&resp, "x-pool-retryable-transport"), Some("1"));
    let st = state(&pool);
    assert!(matches!(
        st.last_error.unwrap().reason,
        FailReason::Transport(_)
    ));
    assert_eq!(st.lifetime.successes, 0);

    // Headers then silence: the idle watchdog bounds pre-commit buffering.
    let fake = FakeAnthropic::start().await;
    fake.script(
        MESSAGES,
        vec![Reply::sse(frames(&[ping()], Duration::ZERO)).hold(Hold::Forever)],
    );
    let pool = Pool::new();
    let (policy, _) = fast_policy();
    let p = pool.provider(&fake, policy);
    let started = tokio::time::Instant::now();
    let out = run_stream(
        &p,
        &pool,
        &stream_req("req_t19_idle_pre"),
        limits_idle(Duration::from_millis(200)),
        None,
    )
    .await;
    assert_eq!(kind(&out), "retry 502 Bad Gateway transport:idle_timeout");
    assert!(started.elapsed() < Duration::from_secs(3));

    // EOF with only pings: nothing committed, fail over.
    let fake = FakeAnthropic::start().await;
    fake.script(
        MESSAGES,
        vec![Reply::sse(frames(&[ping()], Duration::ZERO))],
    );
    let pool = Pool::new();
    let (policy, _) = fast_policy();
    let p = pool.provider(&fake, policy);
    let out = run_stream(&p, &pool, &stream_req("req_t19_eof_pre"), limits(), None).await;
    assert_eq!(kind(&out), "retry 502 Bad Gateway transport:reset");
}

#[tokio::test]
async fn terminal_event_closes_even_if_upstream_holds_socket() {
    let fake = FakeAnthropic::start().await;
    let fs = full_stream();
    fake.script(
        MESSAGES,
        vec![Reply::sse(frames(&fs, Duration::ZERO)).hold(Hold::Forever)],
    );
    let pool = Pool::new();
    let (policy, _) = fast_policy();
    let p = pool.provider(&fake, policy);
    let rs = reservations();
    let slot = reserve(&rs, &pool).await;

    let out = run_stream(
        &p,
        &pool,
        &stream_req("req_t19_hold"),
        limits_idle(Duration::from_secs(30)),
        Some(slot),
    )
    .await;
    let AttemptOutcome::Done(resp, Some(usage)) = out else {
        panic!("expected Done");
    };
    let got = tokio::time::timeout(Duration::from_secs(2), body_bytes(resp))
        .await
        .expect("response must close right after message_stop");
    assert_eq!(String::from_utf8_lossy(&got), fs.concat());
    assert!(usage.await.is_ok());
    assert!(
        released(&rs, &pool).await,
        "reservation held by upstream socket"
    );
    assert_eq!(state(&pool).lifetime.successes, 1);
}

#[tokio::test]
async fn eof_before_message_stop_is_truncation() {
    let fake = FakeAnthropic::start().await;
    fake.script(
        MESSAGES,
        vec![Reply::sse(frames(
            // The last event even lacks its blank line: still not a terminal event.
            &[
                message_start(),
                text_delta("partial"),
                "event: message_stop\ndata: {\"type\":\"message_stop\"}\n".into(),
            ],
            Duration::ZERO,
        ))],
    );
    let pool = Pool::new();
    let (policy, _) = fast_policy();
    let p = pool.provider(&fake, policy);
    let rs = reservations();
    let slot = reserve(&rs, &pool).await;

    let out = run_stream(
        &p,
        &pool,
        &stream_req("req_t19_trunc"),
        limits(),
        Some(slot),
    )
    .await;
    let AttemptOutcome::Done(resp, Some(usage)) = out else {
        panic!("expected Done (committed on message_start)");
    };
    let res = axum::body::to_bytes(resp.into_body(), usize::MAX).await;
    assert!(
        res.is_err(),
        "truncated stream must end in an error, not a clean EOF"
    );
    assert!(usage.await.is_err(), "no usage for a truncated stream");
    assert!(released(&rs, &pool).await);
    let st = state(&pool);
    assert_eq!(st.lifetime.successes, 0);
    assert_eq!(
        st.last_error.unwrap().reason,
        FailReason::Transport(TransportKind::Reset)
    );
    assert!(log_line_with(&["req_t19_trunc", "upstream_truncated"]));
}

#[tokio::test]
async fn post_commit_error_forwarded_and_recorded() {
    // Rate-limit error after commit: forwarded verbatim, account benched.
    let fake = FakeAnthropic::start().await;
    let fs = vec![
        message_start(),
        text_delta("a"),
        sse_error("rate_limit_error", "Rate limited"),
    ];
    fake.script(
        MESSAGES,
        vec![Reply::sse(frames(&fs, Duration::ZERO))
            .header("request-id", "req_post_err")
            .hold(Hold::Forever)],
    );
    let pool = Pool::new();
    let (policy, _) = fast_policy();
    let p = pool.provider(&fake, policy);
    let before = OffsetDateTime::now_utc();
    let out = run_stream(&p, &pool, &stream_req("req_t19_post_rl"), limits(), None).await;
    assert_eq!(kind(&out), "done 200 OK");
    let got = tokio::time::timeout(Duration::from_secs(2), body_bytes(response(out)))
        .await
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&got), fs.concat());
    let st = state(&pool);
    assert!(near(
        st.rate_limited_until.unwrap(),
        before + time::Duration::hours(1)
    ));
    let err = st.last_error.unwrap();
    assert!(matches!(err.reason, FailReason::Quota { .. }));
    assert_eq!(err.upstream_request_id.as_deref(), Some("req_post_err"));
    assert_eq!(st.lifetime.successes, 0);

    // Any other error type after commit: forwarded, recorded, not benched.
    let fake = FakeAnthropic::start().await;
    let fs = vec![message_start(), sse_error("api_error", "Internal error")];
    fake.script(MESSAGES, vec![Reply::sse(frames(&fs, Duration::ZERO))]);
    let pool = Pool::new();
    let (policy, _) = fast_policy();
    let p = pool.provider(&fake, policy);
    let out = run_stream(&p, &pool, &stream_req("req_t19_post_api"), limits(), None).await;
    let got = body_bytes(response(out)).await;
    assert_eq!(String::from_utf8_lossy(&got), fs.concat());
    let st = state(&pool);
    assert_eq!(st.rate_limited_until, None);
    assert_eq!(st.last_error.unwrap().reason, FailReason::Status(502));
    assert_eq!((st.lifetime.successes, st.lifetime.errors), (0, 1));
}

#[tokio::test]
async fn usage_tap_counts_cache_tokens() {
    let fake = FakeAnthropic::start().await;
    fake.script(
        MESSAGES,
        vec![Reply::sse(frames(&full_stream(), Duration::ZERO))],
    );
    let pool = Pool::new();
    let (policy, _) = fast_policy();
    let p = pool.provider(&fake, policy);
    let out = run_stream(&p, &pool, &stream_req("req_t19_usage"), limits(), None).await;
    let AttemptOutcome::Done(resp, Some(usage)) = out else {
        panic!("expected Done");
    };
    body_bytes(resp).await;
    assert_eq!(
        usage.await.unwrap(),
        TokenUsage {
            input_tokens: 10,
            output_tokens: 42,
            cache_read: 7,
            cache_write: 11,
        }
    );
    let st = state(&pool);
    assert_eq!(
        (
            st.lifetime.input_tokens,
            st.lifetime.output_tokens,
            st.lifetime.cache_read,
            st.lifetime.cache_write
        ),
        (10, 42, 7, 11)
    );
}

#[tokio::test]
async fn prefix_cap_64kib_commits() {
    // 64 KiB of pings (which never commit) and then silence: the cap commits anyway.
    let pings: String = std::iter::repeat_n(ping(), 64 * 1024 / ping().len() + 1).collect();
    assert!(pings.len() >= 64 * 1024);
    let fake = FakeAnthropic::start().await;
    fake.script(
        MESSAGES,
        vec![Reply::sse(vec![(Duration::ZERO, pings.clone())]).hold(Hold::Forever)],
    );
    let pool = Pool::new();
    let (policy, _) = fast_policy();
    let p = pool.provider(&fake, policy);
    let out = tokio::time::timeout(
        Duration::from_secs(3),
        run_stream(
            &p,
            &pool,
            &stream_req("req_t19_cap"),
            limits_idle(Duration::from_secs(30)),
            None,
        ),
    )
    .await
    .expect("64 KiB of prefix must commit without waiting for more");
    assert_eq!(kind(&out), "done 200 OK");
    let mut body = response(out).into_body();
    let mut got = Vec::new();
    while got.len() < pings.len() {
        use http_body_util::BodyExt;
        let frame = body.frame().await.unwrap().unwrap();
        got.extend_from_slice(&frame.into_data().unwrap());
    }
    assert_eq!(String::from_utf8_lossy(&got), pings);

    // Below the cap, pings alone never commit: the idle watchdog ends the wait.
    let fake = FakeAnthropic::start().await;
    fake.script(
        MESSAGES,
        vec![Reply::sse(frames(&[ping(), ping()], Duration::ZERO)).hold(Hold::Forever)],
    );
    let pool = Pool::new();
    let (policy, _) = fast_policy();
    let p = pool.provider(&fake, policy);
    let out = run_stream(
        &p,
        &pool,
        &stream_req("req_t19_cap_small"),
        limits_idle(Duration::from_millis(200)),
        None,
    )
    .await;
    assert_eq!(kind(&out), "retry 502 Bad Gateway transport:idle_timeout");
}

#[tokio::test]
async fn idle_watchdog_mid_stream_records_idle_timeout() {
    let fake = FakeAnthropic::start().await;
    fake.script(
        MESSAGES,
        vec![
            Reply::sse(frames(&[message_start(), text_delta("a")], Duration::ZERO))
                .hold(Hold::Forever),
        ],
    );
    let pool = Pool::new();
    let (policy, _) = fast_policy();
    let p = pool.provider(&fake, policy);
    let rs = reservations();
    let slot = reserve(&rs, &pool).await;
    let idle = Duration::from_millis(300);
    let out = run_stream(
        &p,
        &pool,
        &stream_req("req_t19_idle"),
        limits_idle(idle),
        Some(slot),
    )
    .await;
    assert_eq!(kind(&out), "done 200 OK");
    let started = tokio::time::Instant::now();
    let res = axum::body::to_bytes(response(out).into_body(), usize::MAX).await;
    let waited = started.elapsed();
    assert!(res.is_err(), "idle stream must end in an error");
    assert!(waited >= idle - Duration::from_millis(50), "{waited:?}");
    assert!(waited < Duration::from_secs(3), "{waited:?}");
    assert!(released(&rs, &pool).await);
    let st = state(&pool);
    assert_eq!(
        st.last_error.unwrap().reason,
        FailReason::Transport(TransportKind::IdleTimeout)
    );
    assert_eq!(st.lifetime.successes, 0);
    assert!(log_line_with(&["req_t19_idle", "idle_timeout"]));
}

/// A minimal real HTTP server: each request reserves the account, runs one attempt with the
/// reservation and returns the attempt's response.
async fn attempt_server(
    p: Arc<AnthropicProvider>,
    pool: Arc<Pool>,
    rs: Reservations,
) -> std::net::SocketAddr {
    let seq = Arc::new(AtomicUsize::new(0));
    serve(move |req: http::Request<Bytes>| {
        let (p, pool, rs, seq) = (p.clone(), pool.clone(), rs.clone(), seq.clone());
        async move {
            let json: serde_json::Value = serde_json::from_slice(req.body()).unwrap();
            let mut inbound = inbound(&json, req.headers().clone());
            inbound.request_id = format!("req_t19_sock_{}", seq.fetch_add(1, Ordering::SeqCst));
            let slot = reserve(&rs, &pool).await;
            let lim = limits_idle(Duration::from_secs(30));
            response(run_stream(&p, &pool, &inbound, lim, Some(slot)).await)
        }
    })
    .await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn client_disconnect_mid_stream_releases_and_records_nothing() {
    let fake = FakeAnthropic::start().await;
    let fs = full_stream();
    fake.script(
        MESSAGES,
        vec![
            // Esc while the model is "thinking": upstream goes silent after one delta.
            Reply::sse(frames(
                &[message_start(), text_delta("partial")],
                Duration::ZERO,
            ))
            .hold(Hold::Forever),
            Reply::sse(frames(&fs, Duration::ZERO)),
        ],
    );
    let pool = Arc::new(Pool::new());
    let (policy, _) = fast_policy();
    let p = Arc::new(pool.provider(&fake, policy));
    let rs = reservations();
    let addr = attempt_server(p, pool.clone(), rs.clone()).await;
    let request = post_json("/v1/messages", stream_body().to_string().as_bytes());

    let mut client = raw_client(addr, &request).await;
    client.read_until(b"partial", Duration::from_secs(5)).await;
    assert!(contains(&client.received, b"HTTP/1.1 200"));
    assert_eq!(rs.in_flight(&pool.meta.id), 1, "held while streaming");
    client.abort();

    assert!(
        released(&rs, &pool).await,
        "reservation leaked after client abort"
    );
    let st = state(&pool);
    assert!(
        st.last_error.is_none(),
        "client abort recorded on the account"
    );
    assert_eq!((st.lifetime.requests, st.lifetime.successes), (0, 0));
    assert!(log_line_with(&["req_t19_sock_0", "client_cancelled"]));

    // The server keeps serving.
    let mut next = raw_client(addr, &request).await;
    let got = next.read_to_end(Duration::from_secs(5)).await;
    assert!(contains(&got, b"HTTP/1.1 200"));
    assert!(contains(&got, message_stop().as_bytes()));
    assert!(released(&rs, &pool).await);
    assert_eq!(state(&pool).lifetime.successes, 1);
}

// ---- Task 22: inference handler (failover loop, exhaustion, classifier, cross-provider) ----

mod inference {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    use pooltime::models::ModelTable;
    use pooltime::providers::ProviderId;
    use pooltime::server::classifier::MARKER;
    use pooltime::server::inference::{ALL_UNAVAILABLE_MESSAGE, REQUEST_ID_HEADER};
    use support::fake_provider::{done, quota_retry_watched, FakeProvider};
    use support::sockets::{http_get, http_req, HttpResp};

    async fn post(addr: std::net::SocketAddr, body: &serde_json::Value) -> HttpResp {
        post_with(addr, body, &[]).await
    }

    async fn post_with(
        addr: std::net::SocketAddr,
        body: &serde_json::Value,
        extra: &[(&str, &str)],
    ) -> HttpResp {
        let mut h = vec![("content-type", "application/json")];
        h.extend_from_slice(extra);
        http_req(
            addr,
            "POST",
            "/v1/messages",
            &h,
            &serde_json::to_vec(body).unwrap(),
        )
        .await
    }

    fn body_with_user(user: &str) -> serde_json::Value {
        let mut b = simple_body();
        b["metadata"] = json!({ "user_id": user });
        b
    }

    fn classifier_body(user: &str) -> serde_json::Value {
        json!({
            "model": "claude-sonnet-5",
            "max_tokens": 64,
            "system": [{"type": "text", "text": format!("{MARKER}. Judge the action.")}],
            "messages": [{"role": "user", "content": "tool call"}],
            "metadata": {"user_id": user}
        })
    }

    fn mapped_models() -> ModelTable {
        let mut t = ModelTable::defaults();
        t.apply_mappings(Some(true), None).unwrap();
        assert!(t.mapped_route("claude-sonnet-5").is_some());
        t
    }

    fn quota_429() -> Reply {
        Reply::error(429, "rate_limit_error", "Rate limited").header("retry-after", "120")
    }

    /// Log lines (JSON) that mention `request_id`.
    fn lines_for(request_id: &str) -> Vec<String> {
        captured_logs()
            .lines()
            .filter(|l| l.contains(request_id))
            .map(str::to_string)
            .collect()
    }

    fn anthropic(pool: &Pool, fake: &FakeAnthropic) -> Arc<dyn Provider> {
        let (policy, _) = fast_policy();
        Arc::new(pool.provider(fake, policy))
    }

    fn limited(pool: &Pool, id: &str) {
        let now = OffsetDateTime::now_utc();
        pool.state.mark_rate_limited(
            &id.parse().unwrap(),
            now + time::Duration::hours(1),
            None,
            now,
        );
    }

    #[tokio::test]
    async fn fails_over_on_quota_and_logs_each_attempt_reason() {
        let fake = FakeAnthropic::start().await;
        let pool = Pool::new().without_default_account();
        let a1 = pool.add_account("a1", ProviderId::Anthropic, 1, "tok-a1");
        pool.add_account("a2", ProviderId::Anthropic, 2, "tok-a2");
        fake.route(MESSAGES, |r, _| match r.header("authorization") {
            Some("Bearer tok-a1") => quota_429(),
            _ => Reply::json(200, &ok_message(3, 4)),
        });
        let booted = pool
            .boot(Boot::default().provider(anthropic(&pool, &fake)))
            .await;
        // Two sessions are pinned to a1; the quota bench must evict every pin of a1.
        for s in ["sess-q", "sess-other"] {
            booted
                .app
                .sessions
                .set(ProviderId::Anthropic, s, &a1.id, OffsetDateTime::now_utc());
        }

        let resp = post(booted.addr, &body_with_user("sess-q")).await;
        assert_eq!(resp.status, 200, "{}", resp.text());
        assert_eq!(resp.header("x-pool-account"), Some("a2"));
        assert_eq!(fake.requests_to(MESSAGES).len(), 2);
        let now = OffsetDateTime::now_utc();
        assert!(matches!(
            pool.state.status(&a1.id, true, now),
            AccountStatus::RateLimited { .. }
        ));
        // Old pin evicted, success re-pinned to the serving account.
        assert_eq!(
            booted
                .app
                .sessions
                .pinned(ProviderId::Anthropic, "sess-q", now)
                .map(|a| a.to_string()),
            Some("a2".to_string())
        );
        assert_eq!(
            booted
                .app
                .sessions
                .pinned(ProviderId::Anthropic, "sess-other", now),
            None
        );

        let rid = resp
            .header(REQUEST_ID_HEADER)
            .expect("request id")
            .to_string();
        let lines = lines_for(&rid);
        assert!(
            lines.iter().any(|l| l.contains(r#""account":"a1""#)
                && l.contains(r#""reason":"quota""#)
                && l.contains(r#""provider":"anthropic""#)),
            "no quota attempt line for a1: {lines:#?}"
        );
        assert!(
            lines
                .iter()
                .any(|l| l.contains(r#""account":"a2""#) && l.contains(r#""outcome":"done""#)),
            "no success attempt line for a2: {lines:#?}"
        );
        // Never the body or the upstream message.
        assert!(!lines.iter().any(|l| l.contains("Rate limited")));
        assert!(!lines.iter().any(|l| l.contains("\"hi\"")));
    }

    #[tokio::test]
    async fn exhaustion_messages_match_bro() {
        // 1. No account at all.
        let fake = FakeAnthropic::start().await;
        let pool = Pool::new().without_default_account();
        let booted = pool
            .boot(Boot::default().provider(anthropic(&pool, &fake)))
            .await;
        let resp = post(booted.addr, &simple_body()).await;
        assert_eq!(resp.status, 503);
        let j = resp.json();
        assert_eq!(j["type"], "error");
        assert_eq!(j["error"]["type"], "overloaded_error");
        let msg = j["error"]["message"].as_str().unwrap();
        assert!(
            msg.starts_with("No Claude accounts configured. Add one with: "),
            "{msg}"
        );

        // 2. Accounts exist, none usable.
        let fake = FakeAnthropic::start().await;
        let pool = Pool::new();
        limited(&pool, "work");
        let booted = pool
            .boot(Boot::default().provider(anthropic(&pool, &fake)))
            .await;
        let resp = post(booted.addr, &simple_body()).await;
        assert_eq!(resp.status, 503);
        let j = resp.json();
        assert_eq!(j["error"]["type"], "overloaded_error");
        assert_eq!(j["error"]["message"], ALL_UNAVAILABLE_MESSAGE);
        assert_eq!(
            ALL_UNAVAILABLE_MESSAGE,
            "All Claude accounts are currently unavailable (logged out or rate limited). Check the dashboard."
        );
        assert!(fake.requests_to(MESSAGES).is_empty());
        assert!(resp.header("x-pool-retryable-transport").is_none());

        // 3. Every account retried: the last retry's status/type/message.
        let fake = FakeAnthropic::start().await;
        fake.script(MESSAGES, vec![quota_429()]);
        let pool = Pool::new();
        let booted = pool
            .boot(Boot::default().provider(anthropic(&pool, &fake)))
            .await;
        let resp = post(booted.addr, &simple_body()).await;
        assert_eq!(resp.status, 429);
        let j = resp.json();
        assert_eq!(j["error"]["type"], "rate_limit_error");
        assert_eq!(j["error"]["message"], "Rate limited");
        assert!(resp.header("x-pool-retryable-transport").is_none());
    }

    #[tokio::test]
    async fn transport_exhaustion_sets_retryable_header() {
        let closed = {
            let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            l.local_addr().unwrap()
        };
        let fake = FakeAnthropic::start().await;
        let pool = Pool::new();
        pool.add_account("second", ProviderId::Anthropic, 100, "tok-second");
        let (policy, _) = fast_policy();
        let cfg = pooltime::config::AnthropicCfg {
            api_base: format!("http://{closed}"),
            token_url: format!("{}{}", fake.url(), TOKEN),
            ..Default::default()
        };
        let p = AnthropicProvider::new(cfg, pool.reg.clone(), pool.state.clone(), pool.up.clone())
            .with_overload_policy(policy);
        let booted = pool.boot(Boot::default().provider(Arc::new(p))).await;
        let resp = post(booted.addr, &simple_body()).await;
        assert_eq!(resp.status, 502, "{}", resp.text());
        assert_eq!(resp.json()["error"]["type"], "api_error");
        assert_eq!(resp.header("x-pool-retryable-transport"), Some("1"));
        // Both accounts were tried before giving up.
        let rid = resp.header(REQUEST_ID_HEADER).unwrap().to_string();
        let retries = lines_for(&rid)
            .into_iter()
            .filter(|l| l.contains(r#""reason":"transport:connect""#))
            .count();
        assert_eq!(retries, 2);
    }

    #[tokio::test]
    async fn classifier_starts_on_anthropic_no_affinity() {
        let fake = FakeAnthropic::start().await;
        fake.script(MESSAGES, vec![Reply::json(200, &ok_message(1, 1))]);
        let pool = Pool::new();
        pool.add_account("codex1", ProviderId::OpenAi, 100, "tok-codex");
        let codex = FakeProvider::new(ProviderId::OpenAi, |_, _| done(json!({"from": "codex"})));
        let booted = pool
            .boot(Boot {
                models: Some(mapped_models()),
                ..Boot::default()
                    .provider(anthropic(&pool, &fake))
                    .provider(codex.clone())
            })
            .await;
        // A pin to the codex account must not attract the classifier (it never reads affinity).
        let codex_id: pooltime::accounts::AccountId = "codex1".parse().unwrap();
        booted.app.sessions.set(
            ProviderId::OpenAi,
            "sess-c",
            &codex_id,
            OffsetDateTime::now_utc(),
        );

        let resp = post(booted.addr, &classifier_body("sess-c")).await;
        assert_eq!(resp.status, 200, "{}", resp.text());
        assert_eq!(resp.header("x-pool-route"), Some("classifier"));
        assert_eq!(resp.header("x-pool-account"), Some("work"));
        assert!(codex.calls().is_empty());
        // ... and never writes it.
        assert_eq!(
            booted
                .app
                .sessions
                .pinned(ProviderId::Anthropic, "sess-c", OffsetDateTime::now_utc()),
            None
        );

        // Anthropic exhausted: the mapped fallback still carries it.
        limited(&pool, "work");
        let resp = post(booted.addr, &classifier_body("sess-c")).await;
        assert_eq!(resp.status, 200, "{}", resp.text());
        assert_eq!(resp.json()["from"], "codex");
        assert_eq!(resp.header("x-pool-route"), Some("classifier"));
        assert_eq!(resp.header("x-pool-account"), Some("codex1"));
        assert_eq!(codex.calls().len(), 1);

        // classifier_route = "default": ordinary mapped routing, no classifier header.
        let fake2 = FakeAnthropic::start().await;
        let pool2 = Pool::new();
        pool2.add_account("codex1", ProviderId::OpenAi, 100, "tok-codex");
        let codex2 = FakeProvider::new(ProviderId::OpenAi, |_, _| done(json!({"from": "codex"})));
        limited(&pool2, "work");
        let booted2 = pool2
            .boot(Boot {
                models: Some(mapped_models()),
                configure: Some(Box::new(|c| {
                    c.routing.classifier_route = pooltime::config::ClassifierRoute::Default
                })),
                ..Boot::default()
                    .provider(anthropic(&pool2, &fake2))
                    .provider(codex2.clone())
            })
            .await;
        let resp = post(booted2.addr, &classifier_body("sess-d")).await;
        assert_eq!(resp.status, 200);
        assert_eq!(resp.header("x-pool-route"), None);
        // Ordinary routing does pin the session.
        assert!(booted2
            .app
            .sessions
            .pinned(ProviderId::OpenAi, "sess-d", OffsetDateTime::now_utc())
            .is_some());
    }

    #[tokio::test]
    async fn cross_provider_one_hop_then_returns_first() {
        // Anthropic overloaded past its backoff (529, cross-provider retryable).
        let fake = FakeAnthropic::start().await;
        fake.script(
            MESSAGES,
            vec![Reply::error(529, "overloaded_error", "Overloaded")],
        );
        let pool = Pool::new();
        pool.add_account("codex1", ProviderId::OpenAi, 100, "tok-codex");
        let dropped = Arc::new(AtomicBool::new(false));
        let d = dropped.clone();
        let codex = FakeProvider::new(ProviderId::OpenAi, move |_, _| {
            quota_retry_watched(d.clone())
        });
        let booted = pool
            .boot(Boot {
                models: Some(mapped_models()),
                ..Boot::default()
                    .provider(anthropic(&pool, &fake))
                    .provider(codex.clone())
            })
            .await;
        let resp = post(booted.addr, &simple_body()).await;
        // Both failed: the FIRST provider's response is returned.
        assert_eq!(resp.status, 529, "{}", resp.text());
        assert_eq!(resp.json()["error"]["type"], "overloaded_error");
        assert_eq!(resp.header("x-pool-account"), Some("work"));
        // Exactly one hop: one serve per provider (1 + 4 backoff retries upstream on Anthropic).
        assert_eq!(codex.calls().len(), 1);
        assert_eq!(fake.requests_to(MESSAGES).len(), 5);
        // The discarded response body was dropped (cancelled) before returning.
        assert!(dropped.load(Ordering::SeqCst));
        let rid = resp.header(REQUEST_ID_HEADER).unwrap().to_string();
        let lines = lines_for(&rid);
        assert!(
            lines
                .iter()
                .any(|l| l.contains(r#""first_reason":"overloaded""#)
                    && l.contains(r#""second_reason":"quota""#)),
            "both reasons not logged: {lines:#?}"
        );

        // Recovered fallback: the alternate's success is returned.
        let fake = FakeAnthropic::start().await;
        fake.script(MESSAGES, vec![quota_429()]);
        let pool = Pool::new();
        pool.add_account("codex1", ProviderId::OpenAi, 100, "tok-codex");
        let codex = FakeProvider::new(ProviderId::OpenAi, |_, _| done(json!({"from": "codex"})));
        let booted = pool
            .boot(Boot {
                models: Some(mapped_models()),
                ..Boot::default()
                    .provider(anthropic(&pool, &fake))
                    .provider(codex.clone())
            })
            .await;
        let resp = post(booted.addr, &body_with_user("sess-x")).await;
        assert_eq!(resp.status, 200, "{}", resp.text());
        assert_eq!(resp.json()["from"], "codex");
        assert_eq!(resp.header("x-pool-account"), Some("codex1"));
        assert_eq!(codex.calls().len(), 1);
    }

    #[tokio::test]
    async fn upstream_rejected_never_hops_provider() {
        let fake = FakeAnthropic::start().await;
        let upstream_body =
            r#"{"type":"error","error":{"type":"rate_limit_error","message":"Error"}}"#;
        fake.script(
            MESSAGES,
            vec![Reply::bytes(429, upstream_body).header("content-type", "application/json")],
        );
        let pool = Pool::new();
        pool.add_account("second", ProviderId::Anthropic, 100, "tok-second");
        pool.add_account("codex1", ProviderId::OpenAi, 100, "tok-codex");
        let codex = FakeProvider::new(ProviderId::OpenAi, |_, _| done(json!({"from": "codex"})));
        let booted = pool
            .boot(Boot {
                models: Some(mapped_models()),
                ..Boot::default()
                    .provider(anthropic(&pool, &fake))
                    .provider(codex.clone())
            })
            .await;
        let resp = post(booted.addr, &simple_body()).await;
        assert_eq!(resp.status, 429);
        assert_eq!(resp.header("x-pool-upstream-rejected"), Some("1"));
        assert_eq!(resp.text(), upstream_body);
        // No failover, no hop, no bench.
        assert_eq!(fake.requests_to(MESSAGES).len(), 1);
        assert!(codex.calls().is_empty());
        let now = OffsetDateTime::now_utc();
        for id in ["work", "second"] {
            assert_eq!(
                pool.state.status(&id.parse().unwrap(), true, now),
                AccountStatus::Ready
            );
        }
    }

    #[tokio::test]
    async fn request_id_header_on_every_response() {
        let fake = FakeAnthropic::start().await;
        fake.script(MESSAGES, vec![Reply::json(200, &ok_message(1, 1))]);
        let pool = Pool::new();
        let booted = pool
            .boot(Boot {
                proxy_key: Some("k".into()),
                ..Boot::default().provider(anthropic(&pool, &fake))
            })
            .await;
        let auth = [("x-api-key", "k")];
        let mut ids = Vec::new();
        let mut check = |what: &str, r: &HttpResp, status: u16| {
            assert_eq!(r.status, status, "{what}: {}", r.text());
            let id = r
                .header(REQUEST_ID_HEADER)
                .unwrap_or_else(|| panic!("{what}: no request id header"));
            assert!(id.starts_with("req_"), "{what}: {id}");
            ids.push(id.to_string());
        };
        check(
            "success",
            &post_with(booted.addr, &simple_body(), &auth).await,
            200,
        );
        check(
            "unauthorized",
            &post(booted.addr, &simple_body()).await,
            401,
        );
        let bad = http_req(
            booted.addr,
            "POST",
            "/v1/messages",
            &[("x-api-key", "k")],
            b"{nope",
        )
        .await;
        check("bad json", &bad, 400);
        check(
            "unknown path",
            &http_get(booted.addr, "/v1/nope", &[]).await,
            404,
        );
        check(
            "wrong method",
            &http_get(booted.addr, "/v1/messages", &[]).await,
            405,
        );
        check("health", &http_get(booted.addr, "/health", &[]).await, 200);
        // Exhaustion from the handler itself.
        limited(&pool, "work");
        check(
            "exhausted",
            &post_with(booted.addr, &simple_body(), &auth).await,
            503,
        );
        let unique: std::collections::HashSet<_> = ids.iter().collect();
        assert_eq!(unique.len(), ids.len(), "ids not unique: {ids:?}");
    }

    fn capped(c: &mut pooltime::config::Config) {
        c.routing.anthropic_max_inflight = 1;
        c.timeouts.inflight_wait = Duration::from_secs(30);
    }

    /// Hold `id`'s only slot (cap 1) until the returned reservation drops.
    async fn hold_slot(booted: &support::Booted, id: &str) -> Reservation {
        let id: pooltime::accounts::AccountId = id.parse().unwrap();
        booted
            .app
            .reservations
            .reserve(
                move |_| Some(id.clone()),
                ProviderId::Anthropic,
                &WaitBudget::new(Duration::from_secs(1)),
            )
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn slot_wait_sees_live_account_state() {
        // 1. The only account is at cap; it gets benched while the request waits for its slot.
        //    The wait must end at once (nothing usable) instead of running out the 30 s budget
        //    and then over-cap-reserving a benched account.
        let fake = FakeAnthropic::start().await;
        fake.script(MESSAGES, vec![Reply::json(200, &ok_message(1, 1))]);
        let pool = Pool::new();
        let booted = pool
            .boot(Boot {
                configure: Some(Box::new(capped)),
                ..Boot::default().provider(anthropic(&pool, &fake))
            })
            .await;
        let held = hold_slot(&booted, "work").await;
        let addr = booted.addr;
        let started = std::time::Instant::now();
        let waiting = tokio::spawn(async move { post(addr, &simple_body()).await });
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(!waiting.is_finished(), "request should wait for the slot");
        limited(&pool, "work");
        let resp = tokio::time::timeout(Duration::from_secs(5), waiting)
            .await
            .expect("waiting request did not notice the bench")
            .unwrap();
        assert!(started.elapsed() < Duration::from_secs(5));
        assert_eq!(resp.status, 503, "{}", resp.text());
        assert_eq!(resp.json()["error"]["message"], ALL_UNAVAILABLE_MESSAGE);
        assert!(fake.requests_to(MESSAGES).is_empty());
        drop(held);

        // 2. The other account's cooldown expires during the wait: it becomes pickable.
        let fake = FakeAnthropic::start().await;
        fake.script(MESSAGES, vec![Reply::json(200, &ok_message(1, 1))]);
        let pool = Pool::new();
        pool.add_account("b", ProviderId::Anthropic, 100, "tok-b");
        let now = OffsetDateTime::now_utc();
        pool.state.mark_rate_limited(
            &"b".parse().unwrap(),
            now + time::Duration::seconds(1),
            None,
            now,
        );
        let booted = pool
            .boot(Boot {
                configure: Some(Box::new(capped)),
                ..Boot::default().provider(anthropic(&pool, &fake))
            })
            .await;
        let held = hold_slot(&booted, "work").await;
        let started = std::time::Instant::now();
        let resp = tokio::time::timeout(Duration::from_secs(6), post(booted.addr, &simple_body()))
            .await
            .expect("expired cooldown was not noticed during the wait");
        assert!(started.elapsed() < Duration::from_secs(6));
        assert_eq!(resp.status, 200, "{}", resp.text());
        assert_eq!(resp.header("x-pool-account"), Some("b"));
        drop(held);
    }

    #[tokio::test]
    async fn quota_wording_on_400_does_not_hop() {
        // Classified as quota (bench + failover within Anthropic), but the surfaced status is
        // 400, which is not in bro's cross-provider set: no hop.
        let fake = FakeAnthropic::start().await;
        fake.script(
            MESSAGES,
            vec![Reply::error(
                400,
                "invalid_request_error",
                "usage limit reached",
            )],
        );
        let pool = Pool::new();
        pool.add_account("codex1", ProviderId::OpenAi, 100, "tok-codex");
        let codex = FakeProvider::new(ProviderId::OpenAi, |_, _| done(json!({"from": "codex"})));
        let booted = pool
            .boot(Boot {
                models: Some(mapped_models()),
                ..Boot::default()
                    .provider(anthropic(&pool, &fake))
                    .provider(codex.clone())
            })
            .await;
        let resp = post(booted.addr, &simple_body()).await;
        assert_eq!(resp.status, 400, "{}", resp.text());
        assert!(codex.calls().is_empty());
        assert!(matches!(
            pool.state
                .status(&pool.meta.id, true, OffsetDateTime::now_utc()),
            AccountStatus::RateLimited { .. }
        ));
    }
}
