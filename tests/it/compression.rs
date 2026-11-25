//! Request-body compression: large bodies leave gzip'd (Anthropic) / zstd'd (Codex), a refused
//! coding falls back to identity and switches compression off, and compressed inbound bodies are
//! inflated before routing.

use crate::support;

use std::sync::Arc;
use std::time::Duration;

use http::HeaderMap;
use pooltime::compress::{Codec, MIN_LEN};
use pooltime::config::AnthropicCfg;
use pooltime::providers::anthropic::AnthropicProvider;
use pooltime::providers::openai::OpenAiProvider;
use pooltime::providers::{AttemptCtx, AttemptOutcome, InboundRequest, Provider};
use pooltime::upstream::Limits;
use serde_json::{json, Value};
use support::fake_openai::{self, FakeOpenAi, RESPONSES};
use support::sockets::http_req;
use support::*;

/// A conversation of roughly `n` bytes of JSON.
fn long_body(n: usize) -> Value {
    let turn = "Please read src/main.rs and explain what the run loop does. ";
    let text: String = turn.chars().cycle().take(n).collect();
    json!({
        "model": "claude-sonnet-5",
        "max_tokens": 8,
        "messages": [{"role": "user", "content": text}]
    })
}

fn ok_message() -> Value {
    json!({
        "id": "msg_1", "type": "message", "role": "assistant",
        "content": [{"type": "text", "text": "ok"}],
        "usage": {"input_tokens": 1, "output_tokens": 1}
    })
}

async fn run<P: Provider>(p: &P, pool: &Pool, req: &InboundRequest) -> AttemptOutcome {
    let meta = &pool.meta;
    let token = p.access_token(meta, false).await.unwrap();
    let route = route(&req.model);
    let ctx = AttemptCtx {
        account: meta,
        token,
        route: &route,
        limits: Limits {
            idle: Duration::from_secs(30),
            ..limits()
        },
        upstream: &pool.up,
        reservation: None,
    };
    p.attempt(ctx, req, &pool.state).await
}

fn status(o: &AttemptOutcome) -> u16 {
    match o {
        AttemptOutcome::Done(r, _) | AttemptOutcome::Terminal(r, _) => r.status().as_u16(),
        AttemptOutcome::Retry {
            fallback_response, ..
        } => fallback_response.status().as_u16(),
    }
}

fn anthropic(pool: &Pool, fake: &FakeAnthropic) -> AnthropicProvider {
    let (policy, _) = fast_policy();
    pool.provider(fake, policy)
}

#[tokio::test]
async fn large_anthropic_bodies_are_gzipped_small_ones_are_not() {
    let fake = FakeAnthropic::start().await;
    fake.script(MESSAGES, vec![Reply::json(200, &ok_message())]);
    let pool = Pool::new();
    let p = anthropic(&pool, &fake);

    let big = inbound(&long_body(200_000), HeaderMap::new());
    assert_eq!(status(&run(&p, &pool, &big).await), 200);
    let small = inbound(&simple_body(), HeaderMap::new());
    assert!(small.raw_body.len() < MIN_LEN);
    assert_eq!(status(&run(&p, &pool, &small).await), 200);

    let got = fake.requests_to(MESSAGES);
    assert_eq!(got[0].header("content-encoding"), Some("gzip"));
    assert!(
        got[0].body.len() * 20 < big.raw_body.len(),
        "sent {} of {} bytes",
        got[0].body.len(),
        big.raw_body.len()
    );
    assert_eq!(
        got[0].header("content-length"),
        Some(got[0].body.len().to_string().as_str())
    );
    let inflated = Codec::Gzip.decode(&got[0].body, 1 << 24).unwrap();
    assert_eq!(inflated, big.raw_body);

    assert_eq!(got[1].header("content-encoding"), None);
    assert_eq!(got[1].body, small.raw_body);
}

#[tokio::test]
async fn compression_off_in_config_sends_identity() {
    let fake = FakeAnthropic::start().await;
    fake.script(MESSAGES, vec![Reply::json(200, &ok_message())]);
    let pool = Pool::new();
    let cfg = AnthropicCfg {
        api_base: fake.url(),
        token_url: format!("{}{}", fake.url(), TOKEN),
        compress_requests: false,
        ..AnthropicCfg::default()
    };
    let p = AnthropicProvider::new(cfg, pool.reg.clone(), pool.state.clone(), pool.up.clone());
    let big = inbound(&long_body(50_000), HeaderMap::new());
    assert_eq!(status(&run(&p, &pool, &big).await), 200);
    let got = &fake.requests_to(MESSAGES)[0];
    assert_eq!(got.header("content-encoding"), None);
    assert_eq!(got.body, big.raw_body);
}

#[tokio::test]
async fn refused_coding_resends_identity_then_stays_off() {
    let fake = FakeAnthropic::start().await;
    fake.route(MESSAGES, |r, _| match r.header("content-encoding") {
        Some(_) => Reply::error(415, "invalid_request_error", "Unsupported Content-Encoding"),
        None => Reply::json(200, &ok_message()),
    });
    let pool = Pool::new();
    let p = anthropic(&pool, &fake);
    let big = inbound(&long_body(50_000), HeaderMap::new());

    assert_eq!(status(&run(&p, &pool, &big).await), 200);
    assert_eq!(status(&run(&p, &pool, &big).await), 200);
    let got = fake.requests_to(MESSAGES);
    let codings: Vec<_> = got.iter().map(|r| r.header("content-encoding")).collect();
    // Compressed, refused, resent plain; then plain from the start.
    assert_eq!(codings, vec![Some("gzip"), None, None]);
    assert_eq!(got[1].body, big.raw_body);
    // The account is not blamed for the refusal.
    let st = pool
        .state
        .state(&pool.meta.id, time::OffsetDateTime::now_utc());
    assert!(st.last_error.is_none());
}

#[tokio::test]
async fn an_ordinary_400_is_returned_without_a_resend() {
    let fake = FakeAnthropic::start().await;
    let msg = "max_tokens: must be at most 64000";
    fake.script(
        MESSAGES,
        vec![Reply::error(400, "invalid_request_error", msg)],
    );
    let pool = Pool::new();
    let p = anthropic(&pool, &fake);
    let big = inbound(&long_body(50_000), HeaderMap::new());
    let out = run(&p, &pool, &big).await;
    assert_eq!(status(&out), 400);
    let AttemptOutcome::Terminal(resp, _) = out else {
        panic!("expected terminal")
    };
    let body = body_bytes(resp).await;
    assert!(String::from_utf8_lossy(&body).contains(msg));
    let got = fake.requests_to(MESSAGES);
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].header("content-encoding"), Some("gzip"));
}

#[tokio::test]
async fn codex_bodies_are_zstd_compressed() {
    let fake = FakeOpenAi::start().await;
    fake.script(
        RESPONSES,
        vec![Reply::sse(fake_openai::frames(
            &fake_openai::text_turn(1, 1),
            Duration::ZERO,
        ))],
    );
    let pool = Pool::new().without_default_account();
    let meta = fake_openai::add_codex_account(&pool, "cx", 1, "codex-access-token-SECRET");
    let (policy, _) = fast_policy();
    let p: OpenAiProvider =
        fake_openai::codex_provider(&pool, &fake, policy, Duration::from_millis(250));
    let mut body = long_body(100_000);
    body["model"] = "gpt-5.5".into();
    let req = inbound(&body, HeaderMap::new());
    let route = pooltime::models::ModelTable::defaults().resolve("gpt-5.5");
    let ctx = AttemptCtx {
        account: &meta,
        token: p.access_token(&meta, false).await.unwrap(),
        route: &route,
        limits: Limits {
            idle: Duration::from_secs(30),
            ..limits()
        },
        upstream: &pool.up,
        reservation: None,
    };
    let out = p.attempt(ctx, &req, &pool.state).await;
    assert_eq!(status(&out), 200);
    let got = &fake.requests_to(RESPONSES)[0];
    assert_eq!(got.header("content-encoding"), Some("zstd"));
    assert!(got.body.len() * 20 < req.raw_body.len());
    let sent: Value =
        serde_json::from_slice(&Codec::Zstd.decode(&got.body, 1 << 24).unwrap()).unwrap();
    assert!(sent["input"].to_string().contains("run loop"));
}

#[tokio::test]
async fn gzip_and_zstd_inbound_bodies_are_inflated_before_routing() {
    let fake = FakeAnthropic::start().await;
    fake.script(MESSAGES, vec![Reply::json(200, &ok_message())]);
    let pool = Pool::new();
    let booted = pool
        .boot(Boot::default().provider(Arc::new(anthropic(&pool, &fake))))
        .await;
    let raw = serde_json::to_vec(&long_body(30_000)).unwrap();
    for (tok, codec) in [("gzip", Codec::Gzip), ("zstd", Codec::Zstd)] {
        let coded = codec.encode(&raw).unwrap();
        let r = http_req(
            booted.addr,
            "POST",
            "/v1/messages",
            &[
                ("content-type", "application/json"),
                ("content-encoding", tok),
            ],
            &coded,
        )
        .await;
        assert_eq!(r.status, 200, "{tok}: {}", r.text());
    }
    // Forwarded as our own gzip of the decoded body, never the client's coding header as is.
    for got in fake.requests_to(MESSAGES) {
        assert_eq!(got.header("content-encoding"), Some("gzip"));
        assert_eq!(Codec::Gzip.decode(&got.body, 1 << 24).unwrap(), raw);
    }

    let r = http_req(
        booted.addr,
        "POST",
        "/v1/messages",
        &[
            ("content-type", "application/json"),
            ("content-encoding", "br"),
        ],
        &raw,
    )
    .await;
    assert_eq!(r.status, 415, "{}", r.text());
    let r = http_req(
        booted.addr,
        "POST",
        "/v1/messages",
        &[
            ("content-type", "application/json"),
            ("content-encoding", "gzip"),
        ],
        b"definitely not gzip",
    )
    .await;
    assert_eq!(r.status, 400, "{}", r.text());
}
