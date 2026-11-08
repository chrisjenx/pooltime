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
