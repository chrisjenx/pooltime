//! Codex `/responses` attempt (Task 38): request shaping, `x-codex-*` windows, the INV §4
//! response classification, pre-commit buffering, timer-driven keep-alives, teardown and
//! client-disconnect rules, context carry, and the inference path (translation errors,
//! mapping-row effort, access-denied eviction).
//!
//! Timing: the keep-alive test scales bro's "upstream silent 5 s at a 1 s keep-alive -> at
//! least 4 pings" down by 4 (250 ms keep-alive, 1.25 s silence -> at least 4 pings).

mod support;

use std::collections::HashMap;
use std::time::Duration;

use http::HeaderMap;
use http_body_util::BodyExt;
use pooltime::accounts::{AccountMeta, AccountStatus, WindowStatus};
use pooltime::errors::{FailReason, TransportKind};
use pooltime::models::{Mapping, ModelTable, Route};
use pooltime::providers::openai::OpenAiProvider;
use pooltime::providers::{AttemptCtx, AttemptOutcome, InboundRequest, Provider, ProviderId};
use pooltime::routing::reserve::{Reservation, Reservations, WaitBudget};
use pooltime::server::inference::REQUEST_ID_HEADER;
use pooltime::upstream::Limits;
use serde_json::{json, Value};
use support::fake_openai::*;
use support::sockets::{contains, http_req, post_json, raw_client, HttpResp};
use support::*;
use time::OffsetDateTime;

const KEEPALIVE: Duration = Duration::from_millis(250);
const CODEX_ACCESS: &str = "codex-access-token-SECRET";

// ---- helpers ------------------------------------------------------------------

/// A pool whose only account is the Codex account `cx`.
fn codex_pool() -> (Pool, AccountMeta) {
    let pool = Pool::new().without_default_account();
    let meta = add_codex_account(&pool, "cx", 1, CODEX_ACCESS);
    (pool, meta)
}

fn provider(pool: &Pool, fake: &FakeOpenAi) -> OpenAiProvider {
    let (policy, _) = fast_policy();
    codex_provider(pool, fake, policy, KEEPALIVE)
}

fn codex_body(stream: bool) -> Value {
    json!({
        "model": "gpt-5.5",
        "max_tokens": 64,
        "stream": stream,
        "messages": [{"role": "user", "content": "hi"}]
    })
}

fn codex_req(stream: bool, id: &str) -> InboundRequest {
    let mut r = inbound(&codex_body(stream), HeaderMap::new());
    r.request_id = id.into();
    r
}

fn codex_route() -> Route {
    ModelTable::defaults().resolve("gpt-5.5")
}

async fn run(
    p: &OpenAiProvider,
    pool: &Pool,
    meta: &AccountMeta,
    req: &InboundRequest,
    reservation: Option<Reservation>,
) -> AttemptOutcome {
    let token = p.access_token(meta, false).await.unwrap();
    let route = codex_route();
    let ctx = AttemptCtx {
        account: meta,
        token,
        route: &route,
        limits: Limits {
            idle: Duration::from_secs(30),
            ..limits()
        },
        upstream: &pool.up,
        reservation,
    };
    p.attempt(ctx, req, &pool.state).await
}

fn kind(o: &AttemptOutcome) -> String {
    match o {
        AttemptOutcome::Done(r, _) => format!("done {}", r.status().as_u16()),
        AttemptOutcome::Terminal(r, why) => {
            format!("terminal {} {}", r.status().as_u16(), why.detail())
        }
        AttemptOutcome::Retry {
            reason,
            fallback_response,
        } => format!(
            "retry {} {}",
            fallback_response.status().as_u16(),
            reason.detail()
        ),
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

async fn body_json(o: AttemptOutcome) -> Value {
    serde_json::from_slice(&body_bytes(response(o)).await).unwrap()
}

fn state(pool: &Pool, meta: &AccountMeta) -> pooltime::accounts::AccountState {
    pool.state.state(&meta.id, OffsetDateTime::now_utc())
}

fn near(a: OffsetDateTime, b: OffsetDateTime) -> bool {
    (a - b).abs() < time::Duration::seconds(5)
}

fn unix_in(d: time::Duration) -> i64 {
    (OffsetDateTime::now_utc() + d).unix_timestamp()
}

fn log_line_with(needles: &[&str]) -> bool {
    captured_logs()
        .lines()
        .any(|l| needles.iter().all(|n| l.contains(n)))
}

fn count(haystack: &[u8], needle: &str) -> usize {
    String::from_utf8_lossy(haystack).matches(needle).count()
}

fn reservations() -> Reservations {
    Reservations::new(HashMap::new())
}

async fn reserve(rs: &Reservations, meta: &AccountMeta) -> Reservation {
    let id = meta.id.clone();
    rs.reserve(
        move |has_room| has_room(&id).then(|| id.clone()),
        ProviderId::OpenAi,
        &WaitBudget::new(Duration::from_secs(1)),
    )
    .await
    .unwrap()
}

/// Wait (up to 3 s) until `id` holds no reservation.
async fn released(rs: &Reservations, meta: &AccountMeta) -> bool {
    for _ in 0..300 {
        if rs.in_flight(&meta.id) == 0 {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    false
}

/// `message_start.message.usage.input_tokens` of a streamed Anthropic body.
fn opening_input(body: &[u8]) -> u64 {
    let text = String::from_utf8_lossy(body);
    let line = text
        .lines()
        .find(|l| l.starts_with("data: ") && l.contains("\"message_start\""))
        .expect("a message_start frame");
    let v: Value = serde_json::from_str(&line["data: ".len()..]).unwrap();
    v["message"]["usage"]["input_tokens"].as_u64().unwrap()
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

async fn post(addr: std::net::SocketAddr, body: &Value) -> HttpResp {
    http_req(
        addr,
        "POST",
        "/v1/messages",
        &[("content-type", "application/json")],
        &serde_json::to_vec(body).unwrap(),
    )
    .await
}

// ---- request shape ------------------------------------------------------------

#[tokio::test]
async fn upstream_request_shape() {
    let fake = FakeOpenAi::start().await;
    fake.script(
        RESPONSES,
        vec![Reply::sse(frames(&text_turn(3, 1), Duration::ZERO))],
    );
    let (pool, meta) = codex_pool();
    let p = provider(&pool, &fake);
    let out = run(&p, &pool, &meta, &codex_req(false, "req_t38_shape"), None).await;
    assert_eq!(kind(&out), "done 200");

    let reqs = fake.requests_to(RESPONSES);
    assert_eq!(reqs.len(), 1);
    let r = &reqs[0];
    assert_eq!(r.method, "POST");
    assert_eq!(
        r.header("authorization"),
        Some(format!("Bearer {CODEX_ACCESS}").as_str())
    );
    assert_eq!(r.header("chatgpt-account-id"), Some("chatgpt-cx"));
    assert_eq!(r.header("originator"), Some("codex_cli_rs"));
    assert_eq!(r.header("accept"), Some("text/event-stream"));
    assert_eq!(r.header("content-type"), Some("application/json"));
    let b = r.json();
    assert_eq!(b["model"], "gpt-5.5");
    assert_eq!(
        (b["stream"].clone(), b["store"].clone()),
        (json!(true), json!(false))
    );
    assert!(b.get("max_output_tokens").is_none() && b.get("max_tokens").is_none());
}

#[tokio::test]
async fn non_stream_client_gets_folded_message() {
    let fake = FakeOpenAi::start().await;
    fake.script(
        RESPONSES,
        vec![Reply::sse(frames(&text_turn(3, 1), Duration::ZERO))],
    );
    let (pool, meta) = codex_pool();
    let p = provider(&pool, &fake);
    let out = run(&p, &pool, &meta, &codex_req(false, "req_t38_fold"), None).await;
    let AttemptOutcome::Done(resp, Some(usage)) = out else {
        panic!("expected Done");
    };
    assert_eq!(resp.headers()["content-type"], "application/json");
    assert_eq!(resp.headers()["x-pool-account"], "cx");
    let msg: Value = serde_json::from_slice(&body_bytes(resp).await).unwrap();
    assert_eq!(msg["type"], "message");
    assert_eq!(msg["model"], "gpt-5.5");
    assert_eq!(msg["content"], json!([{"type": "text", "text": "Hi"}]));
    assert_eq!(msg["stop_reason"], "end_turn");
    assert_eq!(usage.await.unwrap().input_tokens, 3);
    let st = state(&pool, &meta);
    assert_eq!(st.lifetime.successes, 1);
}

// ---- windows ------------------------------------------------------------------

#[tokio::test]
async fn window_key_by_duration_not_slot() {
    let fake = FakeOpenAi::start().await;
    let ok = || Reply::sse(frames(&text_turn(3, 1), Duration::ZERO));
    let weekly_reset = unix_in(time::Duration::days(5));
    let session_reset = unix_in(time::Duration::hours(2));
    fake.script(
        RESPONSES,
        vec![
            // The weekly window sits in the primary slot, the session window in the secondary.
            window(
                window(ok(), "primary", Some(30.0), Some(10080), Some(weekly_reset)),
                "secondary",
                Some(50.0),
                Some(300),
                Some(session_reset),
            ),
            // No x-codex-* headers: the known windows are kept.
            ok(),
            // Only a primary window without a duration: slot default (5h); replace mode drops 7d.
            window(ok(), "primary", Some(10.0), None, Some(session_reset)),
        ],
    );
    let (pool, meta) = codex_pool();
    let p = provider(&pool, &fake);
    let windows = |pool: &Pool| {
        let mut w: Vec<_> = state(pool, &meta)
            .snapshot
            .unwrap()
            .windows
            .into_iter()
            .map(|w| {
                (
                    w.key,
                    (w.utilization * 100.0).round() as u64,
                    w.status,
                    w.reset.unwrap().unix_timestamp(),
                )
            })
            .collect();
        w.sort_by(|a, b| a.0.cmp(&b.0));
        w
    };

    let out = run(&p, &pool, &meta, &codex_req(false, "req_t38_w1"), None).await;
    assert_eq!(kind(&out), "done 200");
    let want = vec![
        ("5h".to_string(), 50, WindowStatus::Allowed, session_reset),
        ("7d".to_string(), 30, WindowStatus::Allowed, weekly_reset),
    ];
    assert_eq!(windows(&pool), want);

    run(&p, &pool, &meta, &codex_req(false, "req_t38_w2"), None).await;
    assert_eq!(
        windows(&pool),
        want,
        "a header-less response keeps the windows"
    );

    run(&p, &pool, &meta, &codex_req(false, "req_t38_w3"), None).await;
    assert_eq!(
        windows(&pool),
        vec![("5h".to_string(), 10, WindowStatus::Allowed, session_reset)]
    );
}

#[tokio::test]
async fn weekly_in_primary_slot_does_not_bench_7d_on_5h_reset() {
    let now = OffsetDateTime::now_utc();
    let limited = || Reply::json(429, &json!({"detail": "You have hit your usage limit"}));
    let fake = FakeOpenAi::start().await;
    fake.script(
        RESPONSES,
        vec![
            window(
                window(
                    limited(),
                    "primary",
                    Some(40.0),
                    Some(10080),
                    Some(unix_in(time::Duration::days(5))),
                ),
                "secondary",
                Some(100.0),
                Some(300),
                Some(unix_in(time::Duration::hours(2))),
            ),
            // A stale (past) reset is ignored; no window left -> retry-after.
            window(
                limited().header("retry-after", "90"),
                "primary",
                Some(100.0),
                Some(300),
                Some(unix_in(-time::Duration::minutes(5))),
            ),
            // Nothing at all -> the configured cooldown (1 h).
            limited(),
        ],
    );
    let (pool, meta) = codex_pool();
    let p = provider(&pool, &fake);

    let out = run(&p, &pool, &meta, &codex_req(true, "req_t38_q1"), None).await;
    assert_eq!(kind(&out), "retry 429 quota");
    let resp = response(out);
    assert!(resp.headers().contains_key("retry-after"));
    let body: Value = serde_json::from_slice(&body_bytes(resp).await).unwrap();
    assert_eq!(body["error"]["type"], "rate_limit_error");
    let msg = body["error"]["message"].as_str().unwrap();
    assert!(msg.contains("HTTP 429") && msg.contains("usage limit") && msg.contains("\"cx\""));
    let st = state(&pool, &meta);
    let until = st.rate_limited_until.unwrap();
    assert!(near(until, now + time::Duration::hours(2)), "{until}");
    assert_eq!(st.rate_limited_window.as_deref(), Some("5h"));
    let snap = st.snapshot.unwrap();
    let status = |k: &str| snap.windows.iter().find(|w| w.key == k).unwrap().status;
    assert_eq!(
        (status("5h"), status("7d")),
        (WindowStatus::Rejected, WindowStatus::Allowed)
    );
    assert!(matches!(
        st.last_error.unwrap().reason,
        FailReason::Quota { .. }
    ));

    run(&p, &pool, &meta, &codex_req(true, "req_t38_q2"), None).await;
    let until = state(&pool, &meta).rate_limited_until.unwrap();
    assert!(near(until, now + time::Duration::seconds(90)), "{until}");

    run(&p, &pool, &meta, &codex_req(true, "req_t38_q3"), None).await;
    let until = state(&pool, &meta).rate_limited_until.unwrap();
    assert!(near(until, now + time::Duration::hours(1)), "{until}");
}

#[tokio::test]
async fn allowed_at_100_unless_429() {
    let fake = FakeOpenAi::start().await;
    let reset = unix_in(time::Duration::hours(1));
    fake.script(
        RESPONSES,
        vec![
            window(
                Reply::sse(frames(&text_turn(3, 1), Duration::ZERO)),
                "primary",
                Some(100.0),
                Some(300),
                Some(reset),
            ),
            window(
                Reply::json(429, &json!({"detail": "limit"})),
                "primary",
                Some(100.0),
                Some(300),
                Some(reset),
            ),
        ],
    );
    let (pool, meta) = codex_pool();
    let p = provider(&pool, &fake);
    let now = OffsetDateTime::now_utc;

    let out = run(&p, &pool, &meta, &codex_req(false, "req_t38_a1"), None).await;
    assert_eq!(kind(&out), "done 200");
    let w = state(&pool, &meta).snapshot.unwrap().windows[0].clone();
    assert_eq!((w.key.as_str(), w.status), ("5h", WindowStatus::Allowed));
    assert!((w.utilization - 1.0).abs() < 1e-9);
    assert_eq!(
        pool.state.status(&meta.id, true, now()),
        AccountStatus::Ready,
        "a served response at 100% is not a bench"
    );

    let out = run(&p, &pool, &meta, &codex_req(false, "req_t38_a2"), None).await;
    assert_eq!(kind(&out), "retry 429 quota");
    let w = state(&pool, &meta).snapshot.unwrap().windows[0].clone();
    assert_eq!(w.status, WindowStatus::Rejected);
    assert!(matches!(
        pool.state.status(&meta.id, true, now()),
        AccountStatus::RateLimited { .. }
    ));
}

// ---- 401/403, other non-OK, 5xx ------------------------------------------------

#[tokio::test]
async fn unauthorized_refreshes_and_retries_once() {
    let fake = FakeOpenAi::start().await;
    fake.script(
        RESPONSES,
        vec![
            Reply::json(401, &json!({"detail": "expired"})),
            Reply::sse(frames(&text_turn(3, 1), Duration::ZERO)),
        ],
    );
    fake.script(OAUTH_TOKEN, vec![token_ok("codex-new-access-SECRET")]);
    let (pool, meta) = codex_pool();
    let p = provider(&pool, &fake);
    let out = run(&p, &pool, &meta, &codex_req(false, "req_t38_401ok"), None).await;
    assert_eq!(kind(&out), "done 200");
    assert_eq!(fake.requests_to(OAUTH_TOKEN).len(), 1);
    let reqs = fake.requests_to(RESPONSES);
    assert_eq!(
        reqs[1].header("authorization"),
        Some("Bearer codex-new-access-SECRET")
    );
    let rotated = pool.reg.read_creds(&meta.id).unwrap().unwrap();
    assert_eq!(rotated.access_token, "codex-new-access-SECRET");
}

#[tokio::test]
async fn unauthorized_refresh_then_access_denied_evicts() {
    let fake = FakeOpenAi::start().await;
    fake.script(
        RESPONSES,
        vec![Reply::json(
            403,
            &json!({"error": {"message": "workspace deactivated SECRET-DETAIL"}}),
        )],
    );
    fake.script(OAUTH_TOKEN, vec![token_ok("codex-new-access-SECRET")]);
    let (pool, meta) = codex_pool();
    let booted = pool
        .boot(Boot::default().provider(boxed(provider(&pool, &fake))))
        .await;
    let now = OffsetDateTime::now_utc();
    booted
        .app
        .sessions
        .set(ProviderId::OpenAi, "user-evict", &meta.id, now);

    let mut body = codex_body(false);
    body["metadata"] = json!({"user_id": "user-evict"});
    let resp = post(booted.addr, &body).await;

    // Refresh once, resend once with the new token, then give up on the account.
    assert_eq!(fake.requests_to(OAUTH_TOKEN).len(), 1);
    let reqs = fake.requests_to(RESPONSES);
    assert_eq!(reqs.len(), 2);
    assert_eq!(
        reqs[1].header("authorization"),
        Some("Bearer codex-new-access-SECRET")
    );
    // No other account: the fallback response is the access refusal.
    assert_eq!(resp.status, 403);
    assert_eq!(resp.json()["error"]["type"], "permission_error");
    assert!(resp.text().contains("workspace deactivated"));
    assert!(matches!(
        pool.state.status(&meta.id, true, OffsetDateTime::now_utc()),
        AccountStatus::AccessDenied { .. }
    ));
    assert_eq!(
        booted
            .app
            .sessions
            .pinned(ProviderId::OpenAi, "user-evict", OffsetDateTime::now_utc()),
        None,
        "the account's pins are evicted"
    );
    assert_eq!(
        state(&pool, &meta).last_error.unwrap().reason,
        FailReason::AccessDenied
    );
    let logs = captured_logs();
    assert!(!logs.contains("SECRET-DETAIL") && !logs.contains("-SECRET"));
}

#[tokio::test]
async fn other_non_ok_is_terminal_with_legible_message_not_persisted() {
    const SECRET: &str = "SECRET-DETAIL-max_output_tokens";
    let fake = FakeOpenAi::start().await;
    fake.script(
        RESPONSES,
        vec![Reply::json(
            400,
            &json!({"detail": format!("Unsupported parameter: {SECRET}")}),
        )
        .header("x-request-id", "req_up_400")],
    );
    let (pool, meta) = codex_pool();
    let p = provider(&pool, &fake);
    let out = run(&p, &pool, &meta, &codex_req(true, "req_t38_400"), None).await;
    assert_eq!(kind(&out), "terminal 400 status:400");
    let body = body_json(out).await;
    assert_eq!(body["error"]["type"], "api_error");
    assert_eq!(
        body["error"]["message"],
        format!(
            "Codex backend rejected the request (HTTP 400): Unsupported parameter: {SECRET} \
             [account \"cx\"]"
        )
    );
    assert_eq!(fake.requests_to(RESPONSES).len(), 1, "no failover retry");
    let st = state(&pool, &meta);
    let err = st.last_error.unwrap();
    assert_eq!(err.reason, FailReason::Status(400));
    assert_eq!(err.upstream_request_id.as_deref(), Some("req_up_400"));
    assert!(log_line_with(&["req_t38_400", "req_up_400"]));
    assert!(!captured_logs().contains(SECRET), "upstream message logged");
    for entry in walk(pool.dir.path()) {
        let bytes = std::fs::read(&entry).unwrap();
        assert!(
            !bytes.windows(SECRET.len()).any(|w| w == SECRET.as_bytes()),
            "upstream message persisted in {}",
            entry.display()
        );
    }
}

#[tokio::test]
async fn server_error_backs_off_then_terminal() {
    let fake = FakeOpenAi::start().await;
    fake.script(
        RESPONSES,
        vec![Reply::json(500, &json!({"detail": "upstream exploded"}))],
    );
    let (pool, meta) = codex_pool();
    let (policy, sleeper) = fast_policy();
    let p = codex_provider(&pool, &fake, policy, KEEPALIVE);
    let out = run(&p, &pool, &meta, &codex_req(true, "req_t38_500"), None).await;
    assert_eq!(kind(&out), "terminal 500 overloaded");
    assert_eq!(
        fake.requests_to(RESPONSES).len(),
        5,
        "1 + 4 same-account retries"
    );
    assert_eq!(sleeper.slept().len(), 4);
    let body = body_json(out).await;
    assert!(body["error"]["message"]
        .as_str()
        .unwrap()
        .contains("upstream exploded"));
    let st = state(&pool, &meta);
    assert_eq!(st.last_error.unwrap().reason, FailReason::Overloaded);
    assert_eq!(st.lifetime.errors, 1, "recorded once");
    assert_eq!(
        pool.state.status(&meta.id, true, OffsetDateTime::now_utc()),
        AccountStatus::Ready,
        "no sideline"
    );

    // A 5xx that clears is served on the same account.
    let fake = FakeOpenAi::start().await;
    fake.script(
        RESPONSES,
        vec![
            Reply::json(503, &json!({})),
            Reply::sse(frames(&text_turn(3, 1), Duration::ZERO)),
        ],
    );
    let p = provider(&pool, &fake);
    let out = run(&p, &pool, &meta, &codex_req(true, "req_t38_503"), None).await;
    assert_eq!(kind(&out), "done 200");
}

// ---- streaming -----------------------------------------------------------------

#[tokio::test]
async fn pre_commit_error_not_streamed_in_200() {
    let fake = FakeOpenAi::start().await;
    let overloaded =
        event(json!({"type": "error", "code": "server_is_overloaded", "message": "busy"}));
    fake.script(
        RESPONSES,
        vec![
            // 0: failed before anything else.
            Reply::sse(frames(&[failed("server_error", "boom")], Duration::ZERO)),
            // 1: message_start is ready (and a dropped event pings) but no content: the failure
            // that follows in a later chunk still settles without streaming.
            Reply::sse(frames(
                &[
                    created("r"),
                    event(json!({"type": "response.in_progress"})),
                    failed("invalid_prompt", "bad prompt"),
                ],
                Duration::from_millis(50),
            )),
            // 2: rate limited before commit.
            Reply::sse(frames(
                &[failed("rate_limit_exceeded", "slow down")],
                Duration::ZERO,
            )),
            // 3..: overloaded before commit, every time.
            Reply::sse(frames(&[created("r"), overloaded], Duration::ZERO)),
        ],
    );
    let (pool, meta) = codex_pool();
    let (policy, sleeper) = fast_policy();
    let p = codex_provider(&pool, &fake, policy, KEEPALIVE);
    let req = codex_req(true, "req_t38_pre");

    let out = run(&p, &pool, &meta, &req, None).await;
    assert_eq!(kind(&out), "terminal 502 status:502");
    let body = body_json(out).await;
    assert_eq!(
        body,
        json!({"type": "error", "error": {"type": "api_error", "message": "boom"}})
    );

    let out = run(&p, &pool, &meta, &req, None).await;
    assert_eq!(kind(&out), "terminal 502 status:502");
    assert_eq!(
        body_json(out).await["error"]["type"],
        "invalid_request_error"
    );

    let out = run(&p, &pool, &meta, &req, None).await;
    assert_eq!(kind(&out), "retry 429 quota");
    assert!(state(&pool, &meta).rate_limited_until.is_some());

    let out = run(&p, &pool, &meta, &req, None).await;
    assert_eq!(kind(&out), "terminal 529 overloaded");
    assert_eq!(sleeper.slept().len(), 4, "same-account backoff first");
    assert_eq!(body_json(out).await["error"]["type"], "overloaded_error");
}

#[tokio::test]
async fn eof_before_terminal_pre_commit_fails_over() {
    let fake = FakeOpenAi::start().await;
    // Ends without a terminal event before anything reached the client (stream and fold).
    fake.script(
        RESPONSES,
        vec![Reply::sse(frames(&[created("r")], Duration::ZERO))],
    );
    let (pool, meta) = codex_pool();
    let p = provider(&pool, &fake);
    for stream in [true, false] {
        let out = run(&p, &pool, &meta, &codex_req(stream, "req_t38_eof"), None).await;
        assert_eq!(kind(&out), "retry 502 transport:reset");
        let resp = response(out);
        assert_eq!(resp.headers()["x-pool-retryable-transport"], "1");
    }
}

#[tokio::test]
async fn oversized_response_created_forces_message_start() {
    let big = "x".repeat(100_000);
    let created_big = event(json!({
        "type": "response.created",
        "response": {"id": "resp_big", "instructions": big}
    }));
    assert!(created_big.len() > 64 * 1024);
    let (head, tail) = created_big.split_at(70_000);
    let mut fs = vec![
        (Duration::ZERO, head.to_string()),
        (Duration::from_millis(1500), tail.to_string()),
    ];
    fs.extend(frames(&text_turn(3, 1)[1..], Duration::ZERO));
    let fake = FakeOpenAi::start().await;
    fake.script(RESPONSES, vec![Reply::sse(fs)]);
    let (pool, meta) = codex_pool();
    let p = provider(&pool, &fake);

    let out = run(&p, &pool, &meta, &codex_req(true, "req_t38_big"), None).await;
    let AttemptOutcome::Done(resp, _) = out else {
        panic!("expected Done");
    };
    assert_eq!(resp.status(), 200);
    let mut body = resp.into_body();
    let first = tokio::time::timeout(Duration::from_millis(1000), body.frame())
        .await
        .expect("message_start must not wait for the oversized event")
        .unwrap()
        .unwrap()
        .into_data()
        .unwrap();
    assert!(contains(&first, b"event: message_start"));
    let mut all = first.to_vec();
    while let Some(f) = tokio::time::timeout(Duration::from_secs(5), body.frame())
        .await
        .unwrap()
    {
        if let Ok(d) = f.unwrap().into_data() {
            all.extend_from_slice(&d);
        }
    }
    assert_eq!(count(&all, "event: message_start"), 1);
    assert_eq!(count(&all, "event: message_stop"), 1);
    assert!(contains(&all, b"\"text\":\"Hi\""));
    assert!(!contains(&all, big.as_bytes()), "instructions never echoed");
}

#[tokio::test]
async fn teardown_on_terminal_without_eof() {
    let fake = FakeOpenAi::start().await;
    fake.script(
        RESPONSES,
        vec![Reply::sse(frames(&text_turn(3, 1), Duration::ZERO)).hold(Hold::Forever)],
    );
    let (pool, meta) = codex_pool();
    let p = provider(&pool, &fake);
    let rs = reservations();
    let slot = reserve(&rs, &meta).await;
    let out = run(
        &p,
        &pool,
        &meta,
        &codex_req(true, "req_t38_hold"),
        Some(slot),
    )
    .await;
    let AttemptOutcome::Done(resp, Some(usage)) = out else {
        panic!("expected Done");
    };
    let got = tokio::time::timeout(Duration::from_secs(2), body_bytes(resp))
        .await
        .expect("response must close right after the terminal event");
    assert!(contains(&got, b"event: message_stop"));
    assert_eq!(usage.await.unwrap().output_tokens, 1);
    assert!(
        released(&rs, &meta).await,
        "reservation held by upstream socket"
    );
    assert_eq!(state(&pool, &meta).lifetime.successes, 1);
}

#[tokio::test]
async fn post_commit_error_event_is_framed_and_recorded() {
    let fake = FakeOpenAi::start().await;
    let mut fs = frames(
        &[created("r"), added("message"), text_delta("par")],
        Duration::ZERO,
    );
    fs.push((
        Duration::from_millis(100),
        failed("rate_limit_exceeded", "Rate limit reached"),
    ));
    fake.script(RESPONSES, vec![Reply::sse(fs)]);
    let (pool, meta) = codex_pool();
    let p = provider(&pool, &fake);
    let out = run(&p, &pool, &meta, &codex_req(true, "req_t38_post"), None).await;
    let AttemptOutcome::Done(resp, usage) = out else {
        panic!("expected Done");
    };
    let got = body_bytes(resp).await;
    assert!(contains(&got, b"\"text\":\"par\""));
    assert!(contains(&got, b"event: error"));
    assert!(contains(&got, b"\"type\":\"rate_limit_error\""));
    assert!(!contains(&got, b"message_stop"));
    assert!(usage.unwrap().await.is_err(), "no usage on a failed stream");
    let st = state(&pool, &meta);
    assert!(st.rate_limited_until.is_some());
    assert!(matches!(
        st.last_error.unwrap().reason,
        FailReason::Quota { .. }
    ));
    assert_eq!(st.lifetime.successes, 0);
}

#[tokio::test]
async fn idle_mid_stream_records_idle_timeout() {
    let fake = FakeOpenAi::start().await;
    fake.script(
        RESPONSES,
        vec![
            Reply::sse(frames(&[created("r"), added("message")], Duration::ZERO))
                .hold(Hold::Forever),
        ],
    );
    let (pool, meta) = codex_pool();
    let p = provider(&pool, &fake);
    let token = p.access_token(&meta, false).await.unwrap();
    let route = codex_route();
    let ctx = AttemptCtx {
        account: &meta,
        token,
        route: &route,
        // Idle (600 ms) > keep-alive (250 ms): pings must not re-arm the idle watchdog.
        limits: Limits {
            idle: Duration::from_millis(600),
            ..limits()
        },
        upstream: &pool.up,
        reservation: None,
    };
    let out = p
        .attempt(ctx, &codex_req(true, "req_t38_idle"), &pool.state)
        .await;
    let AttemptOutcome::Done(resp, _) = out else {
        panic!("expected Done");
    };
    let res = tokio::time::timeout(
        Duration::from_secs(3),
        axum::body::to_bytes(resp.into_body(), usize::MAX),
    )
    .await
    .expect("idle watchdog must end the stream");
    assert!(
        res.is_err(),
        "an idle stream ends with an error, not a clean close"
    );
    assert_eq!(
        state(&pool, &meta).last_error.unwrap().reason,
        FailReason::Transport(TransportKind::IdleTimeout)
    );
    assert!(log_line_with(&["req_t38_idle", "idle_timeout"]));
}

#[tokio::test]
async fn context_carry_seeds_next_turn_through_proxy() {
    let fake = FakeOpenAi::start().await;
    let mut failing = frames(
        &[created("r"), added("message"), text_delta("x")],
        Duration::ZERO,
    );
    failing.push((Duration::from_millis(100), failed("server_error", "boom")));
    fake.script(
        RESPONSES,
        vec![
            Reply::sse(frames(&text_turn(10, 5), Duration::ZERO)),
            Reply::sse(failing),
            Reply::sse(frames(&text_turn(20, 2), Duration::ZERO)),
            Reply::sse(frames(&text_turn(1, 1), Duration::ZERO)),
            Reply::sse(frames(&text_turn(1, 1), Duration::ZERO)),
        ],
    );
    let (pool, meta) = codex_pool();
    let p = provider(&pool, &fake);
    let turn = |user: Option<&'static str>| {
        let mut r = codex_req(true, "req_t38_carry");
        r.session_user_id = user.map(str::to_string);
        r
    };
    let open = |o: AttemptOutcome| async move { opening_input(&body_bytes(response(o)).await) };

    let a = Some("session-a");
    assert_eq!(open(run(&p, &pool, &meta, &turn(a), None).await).await, 0);
    // Seeded with input + output of the previous turn; the failed turn records nothing.
    assert_eq!(open(run(&p, &pool, &meta, &turn(a), None).await).await, 15);
    assert_eq!(open(run(&p, &pool, &meta, &turn(a), None).await).await, 15);
    assert_eq!(open(run(&p, &pool, &meta, &turn(a), None).await).await, 22);
    // Another session starts from nothing.
    assert_eq!(
        open(run(&p, &pool, &meta, &turn(Some("session-b")), None).await).await,
        0
    );
    assert_eq!(p.context_carry().len(), 2);
}

#[tokio::test]
async fn no_context_carry_without_affinity() {
    let fake = FakeOpenAi::start().await;
    fake.script(
        RESPONSES,
        vec![Reply::sse(frames(&text_turn(10, 5), Duration::ZERO))],
    );
    let (pool, meta) = codex_pool();
    let p = provider(&pool, &fake);
    for _ in 0..2 {
        let mut r = codex_req(true, "req_t38_noaff");
        r.session_user_id = Some("classifier-session".into());
        r.affinity = false;
        let out = run(&p, &pool, &meta, &r, None).await;
        assert_eq!(opening_input(&body_bytes(response(out)).await), 0);
    }
    let mut r = codex_req(false, "req_t38_noaff_fold");
    r.session_user_id = Some("classifier-session".into());
    r.affinity = false;
    assert_eq!(kind(&run(&p, &pool, &meta, &r, None).await), "done 200");
    assert!(p.context_carry().is_empty(), "nothing seeded or recorded");
}

#[tokio::test]
async fn rate_limit_event_benches_until_spent_window_reset() {
    let now = OffsetDateTime::now_utc();
    let reset = unix_in(time::Duration::hours(2));
    let full_5h = |r: Reply| window(r, "primary", Some(100.0), Some(300), Some(reset));
    let limited = failed("usage_limit_reached", "You have hit your usage limit");
    let mut post = frames(
        &[created("r"), added("message"), text_delta("par")],
        Duration::ZERO,
    );
    post.push((Duration::from_millis(100), limited.clone()));
    let fake = FakeOpenAi::start().await;
    fake.script(
        RESPONSES,
        vec![
            // Before commit.
            full_5h(Reply::sse(frames(
                std::slice::from_ref(&limited),
                Duration::ZERO,
            ))),
            // After commit.
            full_5h(Reply::sse(post)),
            // No windows at all: the cooldown.
            Reply::sse(frames(&[limited], Duration::ZERO)),
        ],
    );
    let (pool, meta) = codex_pool();
    let p = provider(&pool, &fake);
    let bench = |pool: &Pool| {
        let st = state(pool, &meta);
        (st.rate_limited_until.unwrap(), st.rate_limited_window)
    };

    let out = run(&p, &pool, &meta, &codex_req(true, "req_t38_rle1"), None).await;
    assert_eq!(kind(&out), "retry 429 quota");
    let (until, window_key) = bench(&pool);
    assert_eq!(until.unix_timestamp(), reset);
    assert_eq!(window_key.as_deref(), Some("5h"));
    let w = state(&pool, &meta).snapshot.unwrap().windows[0].clone();
    assert_eq!(
        w.status,
        WindowStatus::Rejected,
        "an error event counts as a 429"
    );

    pool.state
        .record_success(&meta.id, Default::default(), OffsetDateTime::now_utc());
    let out = run(&p, &pool, &meta, &codex_req(true, "req_t38_rle2"), None).await;
    let got = body_bytes(response(out)).await;
    assert!(contains(&got, b"event: error"));
    let (until, window_key) = bench(&pool);
    assert_eq!(until.unix_timestamp(), reset);
    assert_eq!(window_key.as_deref(), Some("5h"));

    // A fresh account without windows: cooldown (1 h).
    let fresh = add_codex_account(&pool, "cx3", 1, "codex-fresh-SECRET");
    let out = run(&p, &pool, &fresh, &codex_req(true, "req_t38_rle3"), None).await;
    assert_eq!(kind(&out), "retry 429 quota");
    let until = state(&pool, &fresh).rate_limited_until.unwrap();
    assert!(near(until, now + time::Duration::hours(1)), "{until}");
}

// ---- through the real server ---------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn keepalive_pings_while_upstream_silent() {
    let fake = FakeOpenAi::start().await;
    let mut fs = frames(&[created("r"), added("reasoning")], Duration::ZERO);
    // Upstream silent for 1.25 s (5 keep-alive periods) while the model "thinks".
    fs.push((Duration::from_millis(1250), reasoning_delta("plan")));
    fs.extend(frames(
        &[
            done("reasoning"),
            added("message"),
            text_delta("Hi"),
            done("message"),
            completed(3, 1),
        ],
        Duration::ZERO,
    ));
    fake.script(RESPONSES, vec![Reply::sse(fs)]);
    let (pool, _meta) = codex_pool();
    let booted = pool
        .boot(Boot::default().provider(boxed(provider(&pool, &fake))))
        .await;

    let request = post_json("/v1/messages", codex_body(true).to_string().as_bytes());
    let mut client = raw_client(booted.addr, &request).await;
    client
        .read_until(b"content_block_start", Duration::from_secs(2))
        .await;
    let got = client.read_to_end(Duration::from_secs(5)).await;
    let text = String::from_utf8_lossy(&got);
    let silent = &text[..text
        .find("thinking_delta")
        .expect("the delta after the silence")];
    let pings = count(silent.as_bytes(), "event: ping");
    assert!(
        (4..=7).contains(&pings),
        "expected >= 4 timer pings during 1.25 s of silence, got {pings}"
    );
    assert!(contains(&got, b"event: message_stop"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn client_disconnect_mid_stream_releases_and_records_nothing() {
    let fake = FakeOpenAi::start().await;
    fake.script(
        RESPONSES,
        vec![
            // Esc while the model is "thinking": upstream goes silent after one delta.
            Reply::sse(frames(
                &[created("r"), added("message"), text_delta("partial")],
                Duration::ZERO,
            ))
            .hold(Hold::Forever),
            Reply::sse(frames(&text_turn(3, 1), Duration::ZERO)),
        ],
    );
    let (pool, meta) = codex_pool();
    let booted = pool
        .boot(Boot::default().provider(boxed(provider(&pool, &fake))))
        .await;
    let request = post_json("/v1/messages", codex_body(true).to_string().as_bytes());

    let mut client = raw_client(booted.addr, &request).await;
    client.read_until(b"partial", Duration::from_secs(5)).await;
    assert!(contains(&client.received, b"HTTP/1.1 200"));
    let head = String::from_utf8_lossy(&client.received).to_string();
    let rid = head
        .lines()
        .find_map(|l| l.strip_prefix(&format!("{REQUEST_ID_HEADER}: ")))
        .expect("request id header")
        .trim()
        .to_string();
    assert_eq!(
        booted.app.reservations.in_flight(&meta.id),
        1,
        "held while streaming"
    );
    client.abort();

    let mut freed = false;
    for _ in 0..300 {
        if booted.app.reservations.in_flight(&meta.id) == 0 {
            freed = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(freed, "reservation leaked after client abort");
    let st = state(&pool, &meta);
    assert!(
        st.last_error.is_none(),
        "client abort recorded on the account"
    );
    assert_eq!((st.lifetime.requests, st.lifetime.successes), (0, 0));
    assert!(log_line_with(&[
        &rid,
        "codex stream finished",
        "client_cancelled"
    ]));

    // The server keeps serving.
    let mut next = raw_client(booted.addr, &request).await;
    let got = next.read_to_end(Duration::from_secs(5)).await;
    assert!(contains(&got, b"HTTP/1.1 200"));
    assert!(contains(&got, b"event: message_stop"));
    assert_eq!(state(&pool, &meta).lifetime.successes, 1);
}

#[tokio::test]
async fn translation_error_is_400_without_failover() {
    let fake = FakeOpenAi::start().await;
    fake.script(
        RESPONSES,
        vec![Reply::sse(frames(&text_turn(3, 1), Duration::ZERO))],
    );
    let (pool, meta) = codex_pool();
    let other = add_codex_account(&pool, "cx2", 1, "codex-other-SECRET");
    let booted = pool
        .boot(Boot::default().provider(boxed(provider(&pool, &fake))))
        .await;
    let mut body = codex_body(false);
    body["messages"] = json!({"role": "user", "content": "SECRET-PROMPT"});
    let resp = post(booted.addr, &body).await;
    assert_eq!(resp.status, 400);
    assert_eq!(resp.json()["error"]["type"], "invalid_request_error");
    assert!(!resp.text().contains("SECRET-PROMPT"), "no content echo");
    assert!(fake.requests().is_empty(), "nothing sent upstream");
    for m in [&meta, &other] {
        let st = state(&pool, m);
        assert!(st.last_error.is_none());
        assert_eq!(st.lifetime.errors, 0);
    }
    let rid = resp.header(REQUEST_ID_HEADER).unwrap().to_string();
    let attempts = captured_logs()
        .lines()
        .filter(|l| l.contains(&rid) && l.contains("attempt"))
        .count();
    assert_eq!(attempts, 1, "no failover to the second account");
}

#[tokio::test]
async fn mapping_row_effort_reaches_codex() {
    let fake = FakeOpenAi::start().await;
    fake.script(
        RESPONSES,
        vec![Reply::sse(frames(&text_turn(3, 1), Duration::ZERO))],
    );
    let (pool, _meta) = codex_pool();
    let mut models = ModelTable::defaults();
    models
        .apply_mappings(
            Some(true),
            Some(vec![Mapping {
                from: "sonnet".into(),
                to: "gpt-6-sol".into(),
                effort: Some(HashMap::from([("high".to_string(), "low".to_string())])),
            }]),
        )
        .unwrap();
    let booted = pool
        .boot(Boot {
            models: Some(models),
            ..Boot::default().provider(boxed(provider(&pool, &fake)))
        })
        .await;
    let ask = |effort: &str| {
        json!({
            "model": "claude-sonnet-5",
            "max_tokens": 64,
            "output_config": {"effort": effort},
            "messages": [{"role": "user", "content": "hi"}]
        })
    };

    let resp = post(booted.addr, &ask("high")).await;
    assert_eq!(resp.status, 200, "{}", resp.text());
    assert_eq!(resp.json()["model"], "claude-sonnet-5");
    let resp = post(booted.addr, &ask("medium")).await;
    assert_eq!(resp.status, 200);

    let reqs = fake.requests_to(RESPONSES);
    let sent: Vec<Value> = reqs.iter().map(|r| r.json()).collect();
    assert_eq!(sent[0]["model"], "gpt-6-sol");
    assert_eq!(
        sent[0]["reasoning"]["effort"], "low",
        "mapping row override"
    );
    assert_eq!(
        sent[1]["reasoning"]["effort"], "medium",
        "unmapped tier passes through"
    );
}
