//! Fake upstreams for the harness.
//!
//! The scriptable server itself is the integration tests' `tests/support/fake_anthropic.rs`,
//! included by path (no copy, no feature flag on the `pooltime` crate). `fake_openai.rs` is not
//! included because it builds on the tests' in-process `Pool` fixture; the few Codex event
//! builders the scenarios need live here instead.
//!
//! Every daemon gets its own pair of fakes (Anthropic + Codex) scripted identically, so what a
//! fake recorded is attributable to exactly one daemon. Fake access tokens encode the account
//! name (`fake-a-<name>`, `fake-o-<name>`, refresh `fake-r-<name>`), which is how the fakes know
//! which account a daemon chose. They are not secrets and are never printed: observations only
//! carry account names.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{json, Value};

#[allow(dead_code, unused_imports)]
#[path = "../../tests/support/fake_anthropic.rs"]
mod upstream;

pub use upstream::{FakeAnthropic as Fake, Recorded, Reply, MESSAGES, TOKEN};

/// Codex Responses path (relative to the Codex API base).
pub const RESPONSES: &str = "/responses";
/// OpenAI OAuth token path.
pub const OAUTH_TOKEN: &str = "/oauth/token";
/// Codex catalog path (pooltime polls it at boot; bro only in `models update`).
pub const CODEX_MODELS: &str = "/models";
/// Anthropic model list.
pub const ANTHROPIC_MODELS: &str = "/v1/models";

/// One daemon's upstreams.
pub struct Upstreams {
    pub anthropic: Fake,
    pub openai: Fake,
}

impl Upstreams {
    /// Start both fakes with the default routes: token endpoints that refresh any known account,
    /// and a Codex catalog that answers 404 (so pooltime's boot-time discovery adds nothing).
    pub async fn start() -> Upstreams {
        let up = Upstreams {
            anthropic: Fake::start().await,
            openai: Fake::start().await,
        };
        up.anthropic.route(TOKEN, |rec, _| refresh_ok(rec, "a"));
        up.openai.route(OAUTH_TOKEN, |rec, _| refresh_ok(rec, "o"));
        up.openai.script(
            CODEX_MODELS,
            vec![Reply::json(404, &json!({"detail": "harness: no catalog"}))],
        );
        up
    }
}

/// Access token for account `name`; `kind` is `a` (Anthropic) or `o` (Codex).
pub fn access_token(kind: &str, name: &str) -> String {
    format!("fake-{kind}-{name}")
}

pub fn refresh_token(name: &str) -> String {
    format!("fake-r-{name}")
}

/// The account a request was sent for, from its bearer token (`?` when unknown).
pub fn account_of(rec: &Recorded) -> String {
    let token = rec
        .header("authorization")
        .and_then(|h| h.strip_prefix("Bearer "))
        .unwrap_or("");
    let name = token
        .strip_prefix("fake-a-")
        .or_else(|| token.strip_prefix("fake-o-"));
    match name {
        Some(n) => n.split('.').next().unwrap_or(n).to_string(),
        None => "?".into(),
    }
}

/// The account named by a refresh grant's `refresh_token` (JSON or form body).
pub fn refresh_account_of(rec: &Recorded) -> String {
    let body = String::from_utf8_lossy(&rec.body).to_string();
    let token = serde_json::from_str::<Value>(&body)
        .ok()
        .and_then(|v| v["refresh_token"].as_str().map(str::to_owned))
        .or_else(|| {
            body.split('&')
                .find_map(|kv| kv.strip_prefix("refresh_token=").map(str::to_owned))
        })
        .unwrap_or_default();
    token
        .strip_prefix("fake-r-")
        .map(str::to_owned)
        .unwrap_or_else(|| "?".into())
}

fn refresh_ok(rec: &Recorded, kind: &str) -> Reply {
    let name = refresh_account_of(rec);
    Reply::json(
        200,
        &json!({
            "access_token": format!("fake-{kind}-{name}.r"),
            "refresh_token": refresh_token(&name),
            "expires_in": 3600,
            "token_type": "Bearer",
        }),
    )
}

/// Per-account handler: `f(account, hit_index_for_that_account, request)`.
pub fn by_account(
    f: impl Fn(&str, usize, &Recorded) -> Reply + Send + Sync + 'static,
) -> impl Fn(&Recorded, usize) -> Reply + Send + Sync + 'static {
    let hits: Arc<Mutex<HashMap<String, usize>>> = Arc::default();
    move |rec, _| {
        let acct = account_of(rec);
        let i = {
            let mut g = hits.lock().unwrap_or_else(|e| e.into_inner());
            let n = g.entry(acct.clone()).or_insert(0);
            let i = *n;
            *n += 1;
            i
        };
        f(&acct, i, rec)
    }
}

/// Per-account handler for refresh grants: `f(account, hit_index_for_that_account)`.
pub fn by_refresh_account(
    f: impl Fn(&str, usize) -> Reply + Send + Sync + 'static,
) -> impl Fn(&Recorded, usize) -> Reply + Send + Sync + 'static {
    let hits: Arc<Mutex<HashMap<String, usize>>> = Arc::default();
    move |rec, _| {
        let acct = refresh_account_of(rec);
        let i = {
            let mut g = hits.lock().unwrap_or_else(|e| e.into_inner());
            let n = g.entry(acct.clone()).or_insert(0);
            let i = *n;
            *n += 1;
            i
        };
        f(&acct, i)
    }
}

// ---- Anthropic replies -------------------------------------------------------------------------

/// One SSE frame.
pub fn sse_frame(event: &str, data: &Value) -> String {
    format!("event: {event}\ndata: {data}\n\n")
}

pub fn message_start() -> String {
    sse_frame(
        "message_start",
        &json!({"type": "message_start", "message": {
            "id": "msg_fake", "type": "message", "role": "assistant", "model": "claude-sonnet-5",
            "content": [], "stop_reason": null, "stop_sequence": null,
            "usage": {"input_tokens": 5, "output_tokens": 1}
        }}),
    )
}

pub fn text_block_start(index: u32) -> String {
    sse_frame(
        "content_block_start",
        &json!({"type": "content_block_start", "index": index,
                "content_block": {"type": "text", "text": ""}}),
    )
}

pub fn text_delta(index: u32, text: &str) -> String {
    sse_frame(
        "content_block_delta",
        &json!({"type": "content_block_delta", "index": index,
                "delta": {"type": "text_delta", "text": text}}),
    )
}

pub fn block_stop(index: u32) -> String {
    sse_frame(
        "content_block_stop",
        &json!({"type": "content_block_stop", "index": index}),
    )
}

pub fn message_end() -> Vec<String> {
    vec![
        sse_frame(
            "message_delta",
            &json!({"type": "message_delta", "delta": {"stop_reason": "end_turn", "stop_sequence": null},
                    "usage": {"output_tokens": 3}}),
        ),
        sse_frame("message_stop", &json!({"type": "message_stop"})),
    ]
}

pub fn sse_error(kind: &str) -> String {
    sse_frame(
        "error",
        &json!({"type": "error", "error": {"type": kind, "message": "harness: upstream error"}}),
    )
}

/// A complete Anthropic text turn.
pub fn text_turn(text: &str) -> Vec<String> {
    let mut v = vec![
        message_start(),
        text_block_start(0),
        text_delta(0, text),
        block_stop(0),
    ];
    v.extend(message_end());
    v
}

/// Unified rate-limit headers for a healthy account (5h at 10%, 7d at 20%).
pub fn allowed_headers(reply: Reply) -> Reply {
    let reset_5h = unix_in(Duration::from_secs(4 * 3600));
    let reset_7d = unix_in(Duration::from_secs(5 * 24 * 3600));
    reply
        .header("anthropic-ratelimit-unified-status", "allowed")
        .header("anthropic-ratelimit-unified-5h-status", "allowed")
        .header("anthropic-ratelimit-unified-5h-utilization", "0.1")
        .header(
            "anthropic-ratelimit-unified-5h-reset",
            &reset_5h.to_string(),
        )
        .header("anthropic-ratelimit-unified-7d-status", "allowed")
        .header("anthropic-ratelimit-unified-7d-utilization", "0.2")
        .header(
            "anthropic-ratelimit-unified-7d-reset",
            &reset_7d.to_string(),
        )
        .header("request-id", "req_fake_upstream")
}

/// A healthy streamed Anthropic reply carrying `text`.
pub fn anthropic_ok(text: &str) -> Reply {
    allowed_headers(Reply::sse(
        text_turn(text)
            .into_iter()
            .map(|f| (Duration::ZERO, f))
            .collect(),
    ))
}

/// A healthy non-streaming Anthropic reply carrying `text`.
pub fn anthropic_ok_json(text: &str) -> Reply {
    allowed_headers(Reply::json(
        200,
        &json!({
            "id": "msg_fake", "type": "message", "role": "assistant", "model": "claude-sonnet-5",
            "content": [{"type": "text", "text": text}],
            "stop_reason": "end_turn", "stop_sequence": null,
            "usage": {"input_tokens": 5, "output_tokens": 3}
        }),
    ))
}

/// Quota 429: the 5h window rejected, resetting in `reset_in`.
pub fn quota_429(reset_in: Duration) -> Reply {
    let reset = unix_in(reset_in).to_string();
    Reply::error(429, "rate_limit_error", "harness: quota exhausted")
        .header("anthropic-ratelimit-unified-status", "rejected")
        .header("anthropic-ratelimit-unified-reset", &reset)
        .header("anthropic-ratelimit-unified-5h-status", "rejected")
        .header("anthropic-ratelimit-unified-5h-utilization", "1.0")
        .header("anthropic-ratelimit-unified-5h-reset", &reset)
        .header(
            "anthropic-ratelimit-unified-representative-claim",
            "five_hour",
        )
        .header("request-id", "req_fake_upstream")
}

pub fn unix_in(d: Duration) -> u64 {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    (now + d).as_secs()
}

/// Reset the connection 300 ms after the reply's last frame. The pause matters: a reset right
/// behind the data can make the receiving socket discard bytes it has not read yet, so what the
/// daemon saw would depend on timing.
pub fn cut_after_frames(mut reply: Reply) -> Reply {
    // The fake aborts once `drop_after` bytes went out; a trailing blank line (harmless to an SSE
    // parser) carries the delay and is the last thing sent.
    let sent: usize = reply.frames.iter().map(|(_, b)| b.len()).sum();
    reply
        .frames
        .push((Duration::from_millis(300), bytes::Bytes::from_static(b"\n")));
    reply.drop_after_bytes(sent + 1)
}

/// SSE frames sent back to back.
pub fn frames_now(fs: Vec<String>) -> Vec<(Duration, String)> {
    fs.into_iter().map(|f| (Duration::ZERO, f)).collect()
}

// ---- Codex replies -----------------------------------------------------------------------------

/// One Codex SSE event: `event: <data.type>` + `data: <json>`.
pub fn codex_event(data: Value) -> String {
    let ty = data["type"].as_str().unwrap_or("message").to_string();
    format!("event: {ty}\ndata: {data}\n\n")
}

pub fn codex_created(instructions: Option<&str>) -> String {
    let mut response = json!({"id": "resp_fake", "status": "in_progress"});
    if let Some(i) = instructions {
        response["instructions"] = json!(i);
    }
    codex_event(json!({"type": "response.created", "response": response}))
}

pub fn codex_text_item() -> Vec<String> {
    vec![
        codex_event(json!({"type": "response.output_item.added",
                           "item": {"type": "message", "id": "msg_1", "role": "assistant"}})),
        codex_event(json!({"type": "response.output_text.delta", "delta": "Hi"})),
        codex_event(json!({"type": "response.output_text.delta", "delta": " there"})),
        codex_event(json!({"type": "response.output_item.done",
                           "item": {"type": "message", "id": "msg_1"}})),
    ]
}

pub fn codex_reasoning_item() -> Vec<String> {
    vec![
        codex_event(json!({"type": "response.output_item.added",
                           "item": {"type": "reasoning", "id": "rs_1"}})),
        codex_event(json!({"type": "response.reasoning_summary_text.delta", "delta": "plan"})),
        codex_event(json!({"type": "response.output_item.done",
                           "item": {"type": "reasoning", "id": "rs_1",
                                    "encrypted_content": "enc-opaque-1"}})),
    ]
}

pub fn codex_completed() -> String {
    codex_event(json!({
        "type": "response.completed",
        "response": {"id": "resp_fake", "status": "completed",
                     "usage": {"input_tokens": 7, "output_tokens": 3}}
    }))
}

pub fn codex_failed(code: &str) -> String {
    codex_event(json!({
        "type": "response.failed",
        "response": {"id": "resp_fake", "status": "failed",
                     "error": {"code": code, "message": "harness: codex failure"}}
    }))
}

/// A healthy Codex text turn.
pub fn codex_turn() -> Vec<String> {
    let mut v = vec![codex_created(None)];
    v.extend(codex_text_item());
    v.push(codex_completed());
    v
}

/// `x-codex-primary-*` headers for a healthy account.
pub fn codex_allowed(reply: Reply) -> Reply {
    reply
        .header("x-codex-primary-used-percent", "10")
        .header("x-codex-primary-window-minutes", "300")
        .header(
            "x-codex-primary-reset-at",
            &unix_in(Duration::from_secs(4 * 3600)).to_string(),
        )
}

pub fn codex_ok() -> Reply {
    codex_allowed(Reply::sse(frames_now(codex_turn())))
}
