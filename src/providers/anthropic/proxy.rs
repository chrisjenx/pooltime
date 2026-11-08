//! `impl Provider for AnthropicProvider`: one `/v1/messages` attempt on one account and the INV §1
//! response classification.
//!
//! | Upstream | Outcome |
//! |---|---|
//! | 2xx, non-streaming | buffer, record usage from the JSON `usage`, `Done` |
//! | 2xx, `stream: true` or `text/event-stream` | pre-commit buffering, then a pumped `Done` stream ([`super::stream`]) |
//! | 2xx stream, initial rate-limit `error` event | as a quota 429 below (bench, `Retry(Quota)`, 429 fallback) |
//! | 2xx stream, initial `overloaded_error` event | as a 529 below (same-account backoff, surfaced as **529**) |
//! | 2xx stream, transport fault / EOF / idle before commit | as a transport failure below (`Retry`) |
//! | 429 without quota signal | `Terminal(UpstreamRejected)`, replay + `x-pool-upstream-rejected: 1`, no state change |
//! | 429 with quota signal, or any other status whose `"{type}\n{message}"` has rate-limit wording (bro) | bench (reset: explicit -> blocking window -> cooldown), `Retry(Quota)` |
//! | 529 / 500 / 503 | same-account backoff ([`super::overload`]), then `Terminal(Overloaded)` replaying the last upstream error, recorded once, no sideline |
//! | 403 | entitlement wording -> billing block, else access-denied cooldown; `Retry` |
//! | 401 | refresh (forced unless someone already rotated the token) and retry once; a second 401 is "other" |
//! | other non-2xx | record `Status(code)`, `Terminal` replay |
//! | transport failure | record, `Retry(Transport)` with a 502 `api_error` + `x-pool-retryable-transport: 1` |
//! | refresh failure | `Retry(Refresh)` with a 401 `authentication_error` (see [`refresh_failure_outcome`]) |
//!
//! Session pins are not owned here: the caller evicts the account's pins when an attempt returns
//! `Retry` with `Quota`, `AccessDenied` or `Billing`.
//!
//! Privacy: upstream error *messages* reach the client in the replayed body only. Logs and
//! `ErrorRecord`s carry the status, the error `type` and the upstream `request-id`.

use std::time::Duration;

use async_trait::async_trait;
use axum::body::Body;
use axum::response::Response;
use bytes::Bytes;
use http::{HeaderMap, HeaderName, HeaderValue, StatusCode};
use time::OffsetDateTime;
use tokio::sync::oneshot;
use tokio::time::Instant;

use super::overload::{explicit_reset, OverloadPolicy};
use super::stream::{self, Committed, Prefix, StreamCtx};
use super::{
    has_quota_signal, has_rate_limit_wording, parse_unified_headers, prepare_body,
    AnthropicProvider,
};
use crate::accounts::status::is_spent;
use crate::accounts::{
    AccountId, AccountMeta, ErrorRecord, RateLimitSnapshot, StateBook, TokenUsage,
};
use crate::errors::{FailReason, RefreshErrorKind, TransportKind};
use crate::models::RouteDef;
use crate::providers::{
    is_entitlement_message, AttemptCtx, AttemptOutcome, InboundRequest, Provider, ProviderId,
    UsageError,
};
use crate::upstream::{join_url, WatchedBody};

/// Set (to `1`) on a passed-through 429 that is a per-request refusal, not account exhaustion.
pub const UPSTREAM_REJECTED_HEADER: &str = "x-pool-upstream-rejected";
/// Set (to `1`) on the fallback response of a transport failure.
pub const RETRYABLE_TRANSPORT_HEADER: &str = "x-pool-retryable-transport";
/// Name of the account that produced the response.
pub const POOL_ACCOUNT_HEADER: &str = "x-pool-account";

const MESSAGES_PATH: &str = "/v1/messages";

/// Never forwarded upstream (besides `x-api-key`, which is the pool's own credential).
const HOP_BY_HOP_REQUEST: &[&str] = &[
    "connection",
    "content-length",
    "host",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
];

/// Never copied from an upstream response onto ours.
const STRIP_RESPONSE: &[&str] = &[
    "connection",
    "content-encoding",
    "content-length",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
];

/// A fully read upstream response.
struct Buffered {
    status: StatusCode,
    headers: HeaderMap,
    body: Bytes,
}

impl Buffered {
    fn request_id(&self) -> Option<String> {
        request_id(&self.headers)
    }
}

/// Result of one upstream exchange: final, a transient overload the backoff loop handles, or a
/// committed stream for the caller of `exchange` to pump.
enum Step {
    Outcome(AttemptOutcome),
    Overloaded(Buffered),
    Stream(Box<Committed>),
}

/// Attempt-scoped values threaded through the helpers.
struct Call<'a> {
    acct: &'a AccountId,
    state: &'a StateBook,
    req: &'a InboundRequest,
}

impl AnthropicProvider {
    /// Same-account overload backoff policy (`timeouts.overload_retry_*`).
    pub fn with_overload_policy(mut self, p: OverloadPolicy) -> Self {
        self.overload = p;
        self
    }

    /// Bench length for a quota 429 with no explicit or window reset
    /// (`timeouts.rate_limit_cooldown`, default 1 h).
    pub fn with_rate_limit_cooldown(mut self, d: Duration) -> Self {
        self.rate_limit_cooldown = time::Duration::try_from(d).unwrap_or(time::Duration::MAX);
        self
    }

    async fn attempt_messages(
        &self,
        mut ctx: AttemptCtx<'_>,
        req: &InboundRequest,
        state: &StateBook,
    ) -> AttemptOutcome {
        // Moved into the stream pump on commit; otherwise released when this attempt returns.
        let reservation = ctx.reservation.take();
        let call = Call {
            acct: &ctx.account.id,
            state,
            req,
        };
        let body = prepare_body(&req.raw_body, &req.json);
        let mut token = ctx.token.clone();
        let mut refreshed = false;
        let mut retries = 0u32;
        loop {
            let last = match self
                .exchange(&ctx, &call, &body, &mut token, &mut refreshed)
                .await
            {
                Step::Outcome(o) => return o,
                Step::Stream(c) => {
                    let sc = StreamCtx {
                        account: call.acct.clone(),
                        request_id: req.request_id.clone(),
                        state: self.state.clone(),
                        rate_limit_cooldown: self.rate_limit_cooldown,
                    };
                    return stream::commit(c, sc, reservation);
                }
                Step::Overloaded(b) => b,
            };
            let now = OffsetDateTime::now_utc();
            let reset = explicit_reset(&last.headers, now);
            if retries >= self.overload.max_retries {
                return surface_overload(&call, last, reset, now);
            }
            let retry_after = reset.and_then(|r| Duration::try_from(r - now).ok());
            let delay = self.overload.delay(retries, retry_after);
            if Instant::now() + delay >= ctx.limits.deadline {
                return surface_overload(&call, last, reset, now);
            }
            tracing::info!(
                account = %call.acct,
                request_id = %req.request_id,
                status = last.status.as_u16(),
                retry = retries + 1,
                max = self.overload.max_retries,
                delay_ms = delay.as_millis() as u64,
                "anthropic overloaded; retrying same account"
            );
            self.overload.sleeper.sleep(delay).await;
            retries += 1;
        }
    }

    /// Send once (plus the one post-401 resend) and classify the response.
    async fn exchange(
        &self,
        ctx: &AttemptCtx<'_>,
        call: &Call<'_>,
        body: &Bytes,
        token: &mut String,
        refreshed: &mut bool,
    ) -> Step {
        let mut resp = match self.send(ctx, call.req, body, token).await {
            Ok(r) => r,
            Err(k) => return Step::Outcome(transport_outcome(call, k, None)),
        };
        if resp.status == StatusCode::UNAUTHORIZED && !*refreshed {
            *refreshed = true;
            drop(resp);
            match self.token_after_reject(ctx.account, token).await {
                Ok(t) => *token = t,
                Err(k) => return Step::Outcome(refresh_failure_outcome(k)),
            }
            resp = match self.send(ctx, call.req, body, token).await {
                Ok(r) => r,
                Err(k) => return Step::Outcome(transport_outcome(call, k, None)),
            };
        }

        let now = OffsetDateTime::now_utc();
        if let Some(snap) = parse_unified_headers(&resp.headers, now) {
            call.state.merge_header_snapshot(call.acct, snap);
        }

        let status = resp.status;
        let headers = resp.headers;
        if status.is_success() {
            if call.req.stream || is_event_stream(&headers) {
                return self.stream_prefix(call, status, headers, resp.body).await;
            }
            return match read_all(resp.body).await {
                Ok(bytes) => Step::Outcome(success(
                    call,
                    Buffered {
                        status,
                        headers,
                        body: bytes,
                    },
                )),
                Err(k) => Step::Outcome(transport_outcome(call, k, request_id(&headers))),
            };
        }
        // Like bro: an error body that dies mid-read is classified as if empty.
        let bytes = read_all(resp.body).await.unwrap_or_default();
        self.classify_error(
            call,
            Buffered {
                status,
                headers,
                body: bytes,
            },
        )
    }

    async fn send(
        &self,
        ctx: &AttemptCtx<'_>,
        req: &InboundRequest,
        body: &Bytes,
        token: &str,
    ) -> Result<crate::upstream::UpstreamResponse, TransportKind> {
        let request = self.build_request(req, body, token)?;
        ctx.upstream.send(request, &ctx.limits).await
    }

    fn build_request(
        &self,
        req: &InboundRequest,
        body: &Bytes,
        token: &str,
    ) -> Result<reqwest::Request, TransportKind> {
        let url: reqwest::Url = join_url(&self.cfg.api_base, MESSAGES_PATH)
            .parse()
            .map_err(|_| {
                tracing::error!("anthropic api_base is not a valid URL");
                TransportKind::Other
            })?;
        let mut auth = HeaderValue::from_str(&format!("Bearer {token}")).map_err(|_| {
            tracing::error!("stored access token is not a valid header value");
            TransportKind::Other
        })?;
        auth.set_sensitive(true);
        let mut r = reqwest::Request::new(reqwest::Method::POST, url);
        *r.headers_mut() = upstream_headers(&req.headers, auth);
        *r.body_mut() = Some(reqwest::Body::from(body.clone()));
        Ok(r)
    }

    /// Token for the resend after a 401 on `rejected`: someone else's fresh rotation if there is
    /// one, otherwise a forced refresh.
    async fn token_after_reject(
        &self,
        acct: &AccountMeta,
        rejected: &str,
    ) -> Result<String, RefreshErrorKind> {
        let current = AnthropicProvider::access_token(self, acct, false).await?;
        if current != rejected {
            return Ok(current);
        }
        AnthropicProvider::access_token(self, acct, true).await
    }

    fn classify_error(&self, call: &Call<'_>, b: Buffered) -> Step {
        let code = b.status.as_u16();
        let text = String::from_utf8_lossy(&b.body);
        let (err_type, message) = error_fields(&text);
        let upstream_request_id = b.request_id();
        let now = OffsetDateTime::now_utc();
        let log = |outcome: &str| {
            tracing::warn!(
                account = %call.acct,
                request_id = %call.req.request_id,
                status = code,
                error_type = err_type.as_deref().unwrap_or("-"),
                upstream_request_id = upstream_request_id.as_deref().unwrap_or("-"),
                outcome,
                "anthropic upstream error"
            );
        };
        // bro `classifyHttpError`: a 429 is quota only with a quota signal; any other status is
        // quota when "{error.type}\n{message}" has rate-limit wording - checked before the
        // overload and 403 rules.
        let quota = if code == 429 {
            has_quota_signal(&b.headers, &text)
        } else {
            has_rate_limit_wording(&format!("{}\n{message}", raw_error_type(&text)))
        };
        match code {
            429 if !quota => {
                // Per-request refusal (e.g. no Claude Code system prompt): every account would
                // answer the same, so no bench and no failover.
                log("upstream_rejected");
                let mut r = replay(&b, call.acct);
                r.headers_mut().insert(
                    HeaderName::from_static(UPSTREAM_REJECTED_HEADER),
                    HeaderValue::from_static("1"),
                );
                Step::Outcome(AttemptOutcome::Terminal(r, FailReason::UpstreamRejected))
            }
            _ if quota => {
                log("quota");
                Step::Outcome(self.quota_outcome(call, &b, now))
            }
            529 | 500 | 503 => Step::Overloaded(b),
            403 => {
                let reason = if is_entitlement_message(&message) {
                    call.state.mark_billing(call.acct, now);
                    FailReason::Billing
                } else {
                    call.state.mark_access_denied(call.acct, now);
                    FailReason::AccessDenied
                };
                log(reason.label());
                record(call, reason, upstream_request_id.clone(), now);
                Step::Outcome(AttemptOutcome::Retry {
                    reason,
                    fallback_response: replay(&b, call.acct),
                })
            }
            _ => {
                log("terminal");
                let reason = FailReason::Status(code);
                record(call, reason, upstream_request_id.clone(), now);
                Step::Outcome(AttemptOutcome::Terminal(replay(&b, call.acct), reason))
            }
        }
    }
}

impl AnthropicProvider {
    /// Bench (reset: explicit -> blocking window -> cooldown), record, `Retry(Quota)` replaying `b`.
    fn quota_outcome(&self, call: &Call<'_>, b: &Buffered, now: OffsetDateTime) -> AttemptOutcome {
        let (until, window) = quota_until(
            call.state,
            call.acct,
            &b.headers,
            now,
            self.rate_limit_cooldown,
        );
        call.state.mark_rate_limited(call.acct, until, window, now);
        let reason = FailReason::Quota { reset: Some(until) };
        record(call, reason, b.request_id(), now);
        AttemptOutcome::Retry {
            reason,
            fallback_response: replay(b, call.acct),
        }
    }

    /// 2xx streaming response: buffer until commit, classify what happened before it.
    async fn stream_prefix(
        &self,
        call: &Call<'_>,
        status: StatusCode,
        headers: HeaderMap,
        body: WatchedBody,
    ) -> Step {
        let upstream_request_id = request_id(&headers);
        let sse_error = |status: StatusCode, data: String| Buffered {
            status,
            headers: json_headers(&headers),
            body: Bytes::from(data),
        };
        match stream::read_prefix(status, headers.clone(), body).await {
            Prefix::Commit(c) => Step::Stream(c),
            Prefix::Fault(k) => Step::Outcome(transport_outcome(call, k, upstream_request_id)),
            Prefix::Overloaded(data) => Step::Overloaded(sse_error(
                StatusCode::from_u16(529).expect("529 is a valid status"),
                data,
            )),
            Prefix::RateLimited(data) => {
                let b = sse_error(StatusCode::TOO_MANY_REQUESTS, data);
                tracing::warn!(
                    account = %call.acct,
                    request_id = %call.req.request_id,
                    error_type = error_fields(&String::from_utf8_lossy(&b.body)).0.as_deref().unwrap_or("-"),
                    upstream_request_id = upstream_request_id.as_deref().unwrap_or("-"),
                    outcome = "quota",
                    "anthropic stream error before commit"
                );
                Step::Outcome(self.quota_outcome(call, &b, OffsetDateTime::now_utc()))
            }
        }
    }
}

/// Reset for a quota hit on `id`: the explicit reset (`retry-after` / ratelimit-reset headers,
/// future only), else the earliest spent account-wide window (with its key), else `now + cooldown`.
pub(super) fn quota_until(
    state: &StateBook,
    id: &AccountId,
    headers: &HeaderMap,
    now: OffsetDateTime,
    cooldown: time::Duration,
) -> (OffsetDateTime, Option<String>) {
    match explicit_reset(headers, now).filter(|r| *r > now) {
        Some(r) => (r, None),
        None => match blocking_window(state, id, now) {
            Some((r, key)) => (r, Some(key)),
            None => (now + cooldown, None),
        },
    }
}

#[async_trait]
impl Provider for AnthropicProvider {
    fn id(&self) -> ProviderId {
        ProviderId::Anthropic
    }

    async fn access_token(
        &self,
        acct: &AccountMeta,
        force: bool,
    ) -> Result<String, RefreshErrorKind> {
        AnthropicProvider::access_token(self, acct, force).await
    }

    async fn attempt(
        &self,
        ctx: AttemptCtx<'_>,
        req: &InboundRequest,
        state: &StateBook,
    ) -> AttemptOutcome {
        self.attempt_messages(ctx, req, state).await
    }

    async fn poll_usage(&self, acct: &AccountMeta) -> Result<RateLimitSnapshot, UsageError> {
        AnthropicProvider::poll_usage(self, acct).await
    }

    async fn catalog(&self, _acct: &AccountMeta) -> anyhow::Result<Vec<RouteDef>> {
        // TODO(T23): live model catalog.
        Ok(Vec::new())
    }
}

/// Outcome when no usable access token could be obtained for an account: 401
/// `authentication_error`, fail over. The refresh path has already recorded the error.
pub fn refresh_failure_outcome(kind: RefreshErrorKind) -> AttemptOutcome {
    AttemptOutcome::Retry {
        reason: FailReason::Refresh(kind),
        fallback_response: anthropic_error(
            401,
            "authentication_error",
            "Could not refresh the account's access token",
        ),
    }
}

/// Anthropic-shaped JSON error response `{"type":"error","error":{"type","message"}}`.
pub fn anthropic_error(status: u16, kind: &str, message: &str) -> Response {
    let body = serde_json::json!({"type": "error", "error": {"type": kind, "message": message}});
    let mut r = Response::new(Body::from(body.to_string()));
    *r.status_mut() = StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY);
    r.headers_mut().insert(
        http::header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    r
}

/// Inbound headers minus hop-by-hop, the pool credential (`x-api-key`, `authorization`) and
/// `accept-encoding` (we never decompress, so upstream must send identity bytes); plus the
/// account bearer. `anthropic-version` / `anthropic-beta` pass through as sent, never invented.
fn upstream_headers(inbound: &HeaderMap, auth: HeaderValue) -> HeaderMap {
    let mut h = inbound.clone();
    for name in HOP_BY_HOP_REQUEST {
        h.remove(*name);
    }
    h.remove("x-api-key");
    h.insert(http::header::AUTHORIZATION, auth);
    h.insert(
        http::header::ACCEPT_ENCODING,
        HeaderValue::from_static("identity"),
    );
    h
}

/// Upstream status, headers (see [`response_headers`]) and body.
fn replay(b: &Buffered, acct: &AccountId) -> Response {
    let mut r = Response::new(Body::from(b.body.clone()));
    *r.status_mut() = b.status;
    *r.headers_mut() = response_headers(&b.headers, acct);
    r
}

/// Upstream response headers minus [`STRIP_RESPONSE`] (hop-by-hop, `content-length`,
/// `content-encoding`, `transfer-encoding`), tagged with `x-pool-account`.
pub(super) fn response_headers(upstream: &HeaderMap, acct: &AccountId) -> HeaderMap {
    let mut h = HeaderMap::with_capacity(upstream.len() + 1);
    for (k, v) in upstream {
        if !STRIP_RESPONSE.contains(&k.as_str()) {
            h.append(k.clone(), v.clone());
        }
    }
    if let Ok(v) = HeaderValue::from_str(acct.as_str()) {
        h.insert(HeaderName::from_static(POOL_ACCOUNT_HEADER), v);
    }
    h
}

/// `headers` with `content-type: application/json` (an SSE error event replayed as a JSON body).
fn json_headers(headers: &HeaderMap) -> HeaderMap {
    let mut h = headers.clone();
    h.insert(
        http::header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    h
}

fn is_event_stream(h: &HeaderMap) -> bool {
    h.get(http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.to_ascii_lowercase().contains("text/event-stream"))
}

/// 2xx: record usage (from the JSON body when present), commit.
fn success(call: &Call<'_>, b: Buffered) -> AttemptOutcome {
    let usage = json_usage(&b.body);
    call.state
        .record_success(call.acct, usage, OffsetDateTime::now_utc());
    tracing::info!(
        account = %call.acct,
        request_id = %call.req.request_id,
        status = b.status.as_u16(),
        upstream_request_id = b.request_id().as_deref().unwrap_or("-"),
        input_tokens = usage.input_tokens,
        output_tokens = usage.output_tokens,
        "anthropic request served"
    );
    let (tx, rx) = oneshot::channel();
    let _ = tx.send(usage);
    AttemptOutcome::Done(replay(&b, call.acct), Some(rx))
}

/// Overload never cleared: replay the last upstream error, adding `retry-after` when it lacks
/// one and a reset time is known. Recorded once; the account is not sidelined.
fn surface_overload(
    call: &Call<'_>,
    b: Buffered,
    reset: Option<OffsetDateTime>,
    now: OffsetDateTime,
) -> AttemptOutcome {
    let upstream_request_id = b.request_id();
    tracing::warn!(
        account = %call.acct,
        request_id = %call.req.request_id,
        status = b.status.as_u16(),
        error_type = error_fields(&String::from_utf8_lossy(&b.body)).0.as_deref().unwrap_or("-"),
        upstream_request_id = upstream_request_id.as_deref().unwrap_or("-"),
        "anthropic overload persisted; surfacing upstream error"
    );
    record(call, FailReason::Overloaded, upstream_request_id, now);
    let mut r = replay(&b, call.acct);
    if !r.headers().contains_key(http::header::RETRY_AFTER) {
        let secs = reset.map(|t| (t - now).as_seconds_f64().ceil() as i64);
        if let Some(secs) = secs.filter(|s| *s > 0) {
            r.headers_mut()
                .insert(http::header::RETRY_AFTER, HeaderValue::from(secs));
        }
    }
    AttemptOutcome::Terminal(r, FailReason::Overloaded)
}

/// Transport failure (send, or a 2xx body that died): record, 502 `api_error`, fail over.
fn transport_outcome(
    call: &Call<'_>,
    kind: TransportKind,
    upstream_request_id: Option<String>,
) -> AttemptOutcome {
    tracing::warn!(
        account = %call.acct,
        request_id = %call.req.request_id,
        transport = kind.as_str(),
        upstream_request_id = upstream_request_id.as_deref().unwrap_or("-"),
        "anthropic transport failure"
    );
    let reason = FailReason::Transport(kind);
    record(call, reason, upstream_request_id, OffsetDateTime::now_utc());
    let mut r = anthropic_error(
        502,
        "api_error",
        &format!("Upstream transport failure ({})", kind.as_str()),
    );
    r.headers_mut().insert(
        HeaderName::from_static(RETRYABLE_TRANSPORT_HEADER),
        HeaderValue::from_static("1"),
    );
    AttemptOutcome::Retry {
        reason,
        fallback_response: r,
    }
}

fn record(
    call: &Call<'_>,
    reason: FailReason,
    upstream_request_id: Option<String>,
    at: OffsetDateTime,
) {
    call.state.record_error(
        call.acct,
        ErrorRecord {
            at,
            reason,
            upstream_request_id,
        },
    );
}

/// Earliest future reset among the account-wide windows the (just merged) snapshot reports
/// spent, with its key (bro `blockingWindowReset`).
fn blocking_window(
    state: &StateBook,
    id: &AccountId,
    now: OffsetDateTime,
) -> Option<(OffsetDateTime, String)> {
    let snap = state.state(id, now).snapshot?;
    snap.windows
        .iter()
        .filter(|w| w.model.is_none() && is_spent(w))
        .filter_map(|w| w.reset.filter(|r| *r > now).map(|r| (r, w.key.clone())))
        .min_by_key(|(r, _)| *r)
}

async fn read_all(mut body: WatchedBody) -> Result<Bytes, TransportKind> {
    let mut buf = Vec::new();
    while let Some(chunk) = body.next_chunk().await {
        buf.extend_from_slice(&chunk?);
    }
    Ok(Bytes::from(buf))
}

pub(super) fn request_id(h: &HeaderMap) -> Option<String> {
    h.get("request-id")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned)
}

/// `(error.type, message)`. The type is sanitized for logging (ASCII word chars, <= 64);
/// the message (`error.message`, else the first 500 chars of the body) is for classification
/// only and must never be logged or stored.
pub(super) fn error_fields(text: &str) -> (Option<String>, String) {
    let json: serde_json::Value = serde_json::from_str(text).unwrap_or_default();
    let err = json.get("error");
    let kind = err
        .and_then(|e| e.get("type"))
        .and_then(|t| t.as_str())
        .filter(|t| {
            t.len() <= 64
                && t.bytes()
                    .all(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'-')
        })
        .map(str::to_owned);
    let message = err
        .and_then(|e| e.get("message"))
        .and_then(|m| m.as_str())
        .map(str::to_owned)
        .unwrap_or_else(|| text.chars().take(500).collect());
    (kind, message)
}

/// Unsanitized `error.type` for rate-limit wording checks only (bro default `api_error` for a
/// non-429). Never log it; use [`error_fields`]' sanitized type for that.
fn raw_error_type(text: &str) -> String {
    serde_json::from_str::<serde_json::Value>(text)
        .ok()
        .and_then(|v| v.get("error")?.get("type")?.as_str().map(str::to_owned))
        .unwrap_or_else(|| "api_error".into())
}

/// Token usage from a non-streaming Messages response (zeros when absent).
fn json_usage(body: &[u8]) -> TokenUsage {
    let json: serde_json::Value = serde_json::from_slice(body).unwrap_or_default();
    let u = json.get("usage");
    let n = |k: &str| {
        u.and_then(|u| u.get(k))
            .and_then(|v| v.as_u64())
            .unwrap_or(0)
    };
    TokenUsage {
        input_tokens: n("input_tokens"),
        output_tokens: n("output_tokens"),
        cache_read: n("cache_read_input_tokens"),
        cache_write: n("cache_creation_input_tokens"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_fields_never_returns_unsafe_type() {
        let (t, m) =
            error_fields(r#"{"type":"error","error":{"type":"rate_limit_error","message":"m"}}"#);
        assert_eq!(t.as_deref(), Some("rate_limit_error"));
        assert_eq!(m, "m");
        let (t, _) = error_fields(r#"{"error":{"type":"has spaces and prompt text"}}"#);
        assert_eq!(t, None);
        let (t, m) = error_fields("plain text body");
        assert_eq!((t, m.as_str()), (None, "plain text body"));
    }

    #[test]
    fn json_usage_reads_cache_tokens_and_defaults() {
        let u = json_usage(
            br#"{"usage":{"input_tokens":1,"output_tokens":2,"cache_read_input_tokens":3,"cache_creation_input_tokens":4}}"#,
        );
        assert_eq!(
            (u.input_tokens, u.output_tokens, u.cache_read, u.cache_write),
            (1, 2, 3, 4)
        );
        assert_eq!(json_usage(b"not json"), TokenUsage::default());
    }

    #[test]
    fn upstream_headers_strip_and_swap() {
        let mut h = HeaderMap::new();
        h.insert("x-api-key", HeaderValue::from_static("k"));
        h.insert("authorization", HeaderValue::from_static("Bearer pool"));
        h.insert("connection", HeaderValue::from_static("close"));
        h.insert("accept-encoding", HeaderValue::from_static("gzip"));
        let out = upstream_headers(&h, HeaderValue::from_static("Bearer acct"));
        assert!(out.get("x-api-key").is_none());
        assert!(out.get("connection").is_none());
        assert_eq!(out["authorization"], "Bearer acct");
        assert_eq!(out["accept-encoding"], "identity");
        assert!(out.get("anthropic-version").is_none());
    }
}
