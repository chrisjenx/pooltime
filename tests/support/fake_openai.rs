//! Fake ChatGPT Codex backend on 127.0.0.1:0, in the style of [`super::fake_anthropic`] (it is
//! the same scriptable hyper server): `POST /responses` and the OAuth token endpoint
//! `POST /oauth/token`, plus Codex SSE event builders and `x-codex-*` rate-limit headers.

use std::sync::Arc;
use std::time::Duration;

use pooltime::accounts::AccountMeta;
use pooltime::config::OpenAiCfg;
use pooltime::providers::anthropic::OverloadPolicy;
use pooltime::providers::openai::OpenAiProvider;
use pooltime::providers::ProviderId;
use serde_json::{json, Value};

#[allow(unused_imports)]
pub use super::fake_anthropic::{FakeAnthropic as FakeOpenAi, Hold, Reply};
use super::Pool;

pub const RESPONSES: &str = "/responses";
pub const OAUTH_TOKEN: &str = "/oauth/token";
pub const CODEX_REFRESH: &str = "codex-refresh-token-SECRET";

/// Provider pointed at `fake` (Responses and token endpoint), with `policy` and `keepalive`.
pub fn codex_provider(
    pool: &Pool,
    fake: &FakeOpenAi,
    policy: OverloadPolicy,
    keepalive: Duration,
) -> OpenAiProvider {
    let cfg = OpenAiCfg {
        api_base: fake.url(),
        token_url: format!("{}{}", fake.url(), OAUTH_TOKEN),
        ..OpenAiCfg::default()
    };
    OpenAiProvider::new(cfg, pool.reg.clone(), pool.state.clone(), pool.up.clone())
        .with_overload_policy(policy)
        .with_keepalive(keepalive)
}

/// Add an OpenAI account `name` (priority `priority`) with access token `access` and ChatGPT
/// account id `chatgpt-<name>`.
pub fn add_codex_account(pool: &Pool, name: &str, priority: u32, access: &str) -> AccountMeta {
    let meta = pool.add_account(name, ProviderId::OpenAi, priority, access);
    let mut c = pool.reg.read_creds(&meta.id).unwrap().unwrap();
    c.refresh_token = Some(CODEX_REFRESH.into());
    c.openai_account_id = Some(format!("chatgpt-{name}"));
    c.scopes = vec![];
    pool.reg.write_creds(&meta.id, &c).unwrap();
    meta
}

/// A successful JSON refresh-grant response.
pub fn token_ok(access: &str) -> Reply {
    Reply::json(
        200,
        &json!({"access_token": access, "refresh_token": "codex-rotated-SECRET", "expires_in": 3600}),
    )
}

/// One Codex SSE event: `event: <data.type>` + `data: <json>`.
pub fn event(data: Value) -> String {
    let ty = data["type"].as_str().unwrap_or("message").to_string();
    format!("event: {ty}\ndata: {data}\n\n")
}

pub fn created(id: &str) -> String {
    event(json!({"type": "response.created", "response": {"id": id}}))
}

pub fn added(item_type: &str) -> String {
    event(json!({"type": "response.output_item.added", "item": {"type": item_type}}))
}

pub fn text_delta(t: &str) -> String {
    event(json!({"type": "response.output_text.delta", "delta": t}))
}

pub fn reasoning_delta(t: &str) -> String {
    event(json!({"type": "response.reasoning_summary_text.delta", "delta": t}))
}

pub fn done(item_type: &str) -> String {
    event(json!({"type": "response.output_item.done", "item": {"type": item_type}}))
}

pub fn completed(input: u64, output: u64) -> String {
    event(json!({
        "type": "response.completed",
        "response": {"usage": {"input_tokens": input, "output_tokens": output}}
    }))
}

pub fn failed(code: &str, message: &str) -> String {
    event(json!({
        "type": "response.failed",
        "response": {"error": {"code": code, "message": message}}
    }))
}

/// A complete text turn: `Hi`, usage `input`/`output`.
pub fn text_turn(input: u64, output: u64) -> Vec<String> {
    vec![
        created("resp_1"),
        added("message"),
        text_delta("Hi"),
        done("message"),
        completed(input, output),
    ]
}

/// `(delay, frame)` pairs with the same delay.
pub fn frames(fs: &[String], gap: Duration) -> Vec<(Duration, String)> {
    fs.iter().map(|f| (gap, f.clone())).collect()
}

/// `x-codex-<slot>-*` headers on `reply` (`None` fields omitted).
pub fn window(
    reply: Reply,
    slot: &str,
    used_percent: Option<f64>,
    minutes: Option<u64>,
    reset_at: Option<i64>,
) -> Reply {
    let mut r = reply;
    if let Some(u) = used_percent {
        r = r.header(&format!("x-codex-{slot}-used-percent"), &u.to_string());
    }
    if let Some(m) = minutes {
        r = r.header(&format!("x-codex-{slot}-window-minutes"), &m.to_string());
    }
    if let Some(t) = reset_at {
        r = r.header(&format!("x-codex-{slot}-reset-at"), &t.to_string());
    }
    r
}

/// Wrap a provider for [`super::Boot::provider`].
pub fn boxed(p: OpenAiProvider) -> Arc<dyn pooltime::providers::Provider> {
    Arc::new(p)
}
