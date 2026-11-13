//! `POST /v1/messages/count_tokens`: Claude Code asks how many input tokens a request would use
//! (context accounting, compaction thresholds). bro-cli never served it (404), so Claude Code
//! always fell back to its own rough estimate; pooltime answers it.
//!
//! * Claude models: forwarded to Anthropic's own `/v1/messages/count_tokens` with a pooled
//!   account's OAuth token, exactly like `/v1/messages` (same header handling, Host/Origin guard
//!   and proxy key; account choice and failover through [`inference::serve_provider`]).
//! * Models Codex serves (an explicit OpenAI model id, or a Claude id mapped to Codex when no
//!   Claude account can answer): OpenAI has no counting endpoint, so the answer is a local
//!   **estimate**, `{"input_tokens": ceil(request bytes / 4)}`, marked `x-pool-estimate: 1`.
//!
//! Not an inference. It never reads or writes a sticky session (`affinity` is off), the provider
//! records no success, usage or request on the account ([`Provider::count_tokens`]), and the
//! journal row carries no attempts, so per-account attempt counts, error rates and the
//! utilization-based burn analytics see nothing. The row itself is kept (path, model, status,
//! outcome) so a failing count is visible in `pooltime logs` and the dashboard instead of
//! looking like an unknown endpoint.
//!
//! [`Provider::count_tokens`]: crate::providers::Provider::count_tokens

use std::sync::Arc;

use axum::body::Body;
use axum::extract::{Request, State};
use axum::response::Response;
use http::HeaderValue;
use serde_json::Value;
use time::OffsetDateTime;

use super::fallback::Verdict;
use super::inference::{self, Op, RequestId};
use super::router::{read_json, BodyReadAborted};
use super::AppState;
use crate::logging::new_request_id;
use crate::providers::{InboundRequest, ProviderId};
use crate::routing::reserve::WaitBudget;
use crate::store::journal::Journal;

/// Response header marking a locally estimated count (value `1`).
pub const ESTIMATE_HEADER: &str = "x-pool-estimate";

/// `POST /v1/messages/count_tokens`.
pub async fn count_tokens(State(app): State<Arc<AppState>>, req: Request) -> Response {
    let request_id = req
        .extensions()
        .get::<RequestId>()
        .map(|r| r.0.clone())
        .unwrap_or_else(new_request_id);
    // As in `/v1/messages`: dropping the handler (client gone) writes `client_cancelled`.
    let journal = Journal::start(app.store.clone(), &request_id, OffsetDateTime::now_utc());
    journal.set_path(req.uri().path());
    let (parts, body) = req.into_parts();
    let (raw_body, json) = match read_json(&parts.headers, body).await {
        Ok(v) => v,
        Err(resp) => {
            if resp.extensions().get::<BodyReadAborted>().is_none() {
                journal.respond(
                    resp.status().as_u16(),
                    "error",
                    Some("invalid_request".into()),
                );
            }
            return resp;
        }
    };
    let model = json
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    journal.set_model(&model);

    let (route, mapped) = {
        let table = app.models.read().unwrap_or_else(|e| e.into_inner());
        (table.resolve(&model), table.mapped_route(&model).is_some())
    };
    let estimate = |journal: &Journal, provider: ProviderId| {
        let resp = estimate_response(raw_body.len());
        journal.set_provider(provider.as_str());
        journal.respond(200, "completed", None);
        resp
    };
    if route.def.provider == ProviderId::OpenAi {
        return estimate(&journal, ProviderId::OpenAi);
    }
    journal.set_provider(ProviderId::Anthropic.as_str());

    let inbound = InboundRequest {
        raw_body: raw_body.clone(),
        json,
        headers: parts.headers,
        model,
        stream: false,
        session_user_id: None,
        affinity: false,
        request_id,
        // Attempts are deliberately not journaled (see the module docs).
        journal: Journal::disabled(),
    };
    let budget = WaitBudget::new(app.cfg.timeouts.inflight_wait);
    let served = inference::serve_provider(
        &app,
        ProviderId::Anthropic,
        &route,
        &inbound,
        &budget,
        Op::CountTokens,
    )
    .await;
    // A Claude id mapped to Codex, with no Claude account to ask: the conversation may well be
    // served by Codex, so answer with the estimate instead of failing the count.
    if mapped && matches!(served.verdict, Verdict::NoAccount) {
        return estimate(&journal, ProviderId::OpenAi);
    }
    let (outcome, reason) = match served.verdict {
        Verdict::Served => ("completed", None),
        Verdict::Failed(r) => ("error", Some(r.detail())),
        Verdict::NoAccount => ("error", Some("no_account".to_string())),
    };
    journal.set_served_provider(served.provider.as_str());
    journal.respond(served.response.status().as_u16(), outcome, reason);
    served.response
}

/// `{"input_tokens": ceil(bytes / 4)}` (at least 1), flagged as an estimate. The same rule of
/// thumb Claude Code uses for its own fallback; a different tokenizer would give a different
/// number, so this is only ever a rough figure.
fn estimate_response(body_len: usize) -> Response {
    let tokens = body_len.div_ceil(4).max(1);
    let mut r = Response::new(Body::from(
        serde_json::json!({ "input_tokens": tokens }).to_string(),
    ));
    let h = r.headers_mut();
    h.insert(
        http::header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    h.insert(ESTIMATE_HEADER, HeaderValue::from_static("1"));
    r
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn estimate_is_bytes_over_four_rounded_up_and_never_zero() {
        for (len, want) in [(0, 1), (1, 1), (4, 1), (5, 2), (400, 100), (401, 101)] {
            let body = axum::body::to_bytes(estimate_response(len).into_body(), 1024);
            let body = futures_util::FutureExt::now_or_never(body)
                .expect("ready")
                .unwrap();
            let v: Value = serde_json::from_slice(&body).unwrap();
            assert_eq!(v["input_tokens"], want, "len {len}");
        }
        let r = estimate_response(8);
        assert_eq!(r.headers()[ESTIMATE_HEADER], "1");
        assert_eq!(r.headers()["content-type"], "application/json");
    }
}
