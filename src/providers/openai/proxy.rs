//! `impl Provider for OpenAiProvider`: one Codex Responses attempt on one account and the INV §4
//! response classification (port of bro `tryCodexAccount`).
//!
//! The Anthropic body is translated first ([`to_codex_request`] with the route's resolved
//! effort, `Route::effort`); a translation error is the caller's fault: **400
//! `invalid_request_error`**, `Terminal(UpstreamRejected)`, nothing sent, nothing recorded.
//!
//! `POST {api_base}/responses` with `authorization: Bearer`, `chatgpt-account-id` (when the
//! credentials know it), `originator` (config, `codex_cli_rs`), `accept: text/event-stream`.
//!
//! Every response's `x-codex-*` windows replace the account's snapshot when it reports any
//! ([`super::windows`]). Then:
//!
//! | Upstream | Outcome |
//! |---|---|
//! | 2xx | pre-commit buffering, then a pumped stream / folded message ([`super::stream`]) |
//! | 401 / 403 | refresh (forced unless someone already rotated the token) and resend once |
//! | 401 / 403 again | access-denied cooldown, `Retry(AccessDenied)` (the caller evicts pins) |
//! | 429 | bench until the soonest future window reset, else `retry-after`, else the cooldown; `Retry(Quota)` |
//! | 5xx | same-account backoff (like Anthropic), then `Terminal(Overloaded)` |
//! | other non-2xx | `Terminal(Status(code))`, `api_error` with a legible message |
//! | transport failure | `Retry(Transport)` with a 502 + `x-pool-retryable-transport: 1` |
//! | refresh failure | `Retry(Refresh)` (see [`refresh_failure_outcome`]) |
//!
//! Decision (spec §16 "decide"): a Codex 5xx is terminal within the Codex pool after the
//! same-account backoff, exactly like an Anthropic overload; the cross-provider hop rule (5xx is
//! hop-eligible) still applies. Recorded in `docs/parity.md`.
//!
//! Privacy: the legible upstream message ([`describe_codex_error`]) reaches the client in the
//! response body only. Logs and `ErrorRecord`s carry the status, a classified reason and the
//! upstream request id.

use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use http::{HeaderMap, HeaderName, HeaderValue, StatusCode};
use time::OffsetDateTime;
use tokio::time::Instant;

use super::stream::{self, Committed};
use super::windows::{codex_quota_reset, parse_codex_rate_limits};
use super::{to_codex_request, OpenAiProvider};
use crate::accounts::{AccountId, AccountMeta, ErrorRecord, RateLimitSnapshot, StateBook};
use crate::errors::{FailReason, RefreshErrorKind, TransportKind};
use crate::models::{Route, RouteDef};
use crate::providers::anthropic::{
    anthropic_error, explicit_reset, refresh_failure_outcome, OverloadPolicy,
    RETRYABLE_TRANSPORT_HEADER,
};
use crate::providers::{
    AttemptCtx, AttemptOutcome, InboundRequest, Provider, ProviderId, UsageError,
};
use crate::upstream::{join_url, UpstreamResponse};

/// Path of the Responses endpoint under `openai.api_base`.
pub const RESPONSES_PATH: &str = "/responses";
/// Header carrying the ChatGPT account (workspace) id.
const ACCOUNT_ID_HEADER: &str = "chatgpt-account-id";
/// Cap on the upstream detail quoted in a legible error.
const DETAIL_CHARS: usize = 300;

/// A fully read non-2xx upstream response.
pub(super) struct Buffered {
    pub(super) status: StatusCode,
    pub(super) headers: HeaderMap,
    pub(super) body: Bytes,
}

/// Result of one upstream exchange.
pub(super) enum Step {
    Outcome(AttemptOutcome),
    /// Transient overload for the backoff loop.
    Overloaded(Buffered),
    /// A committed stream for the attempt to pump (it owns the reservation).
    Stream(Box<Committed>),
}

/// Attempt-scoped values threaded through the helpers.
pub(super) struct Call<'a> {
    pub(super) acct: &'a AccountId,
    pub(super) state: &'a StateBook,
    pub(super) req: &'a InboundRequest,
    pub(super) route: &'a Route,
}

impl OpenAiProvider {
    /// Same-account backoff policy for Codex 5xx (`timeouts.overload_retry_*`).
    pub fn with_overload_policy(mut self, p: OverloadPolicy) -> Self {
        self.overload = p;
        self
    }

    /// Bench length for a rate limit with no future window reset and no `retry-after`
    /// (`timeouts.rate_limit_cooldown`, default 1 h).
    pub fn with_rate_limit_cooldown(mut self, d: Duration) -> Self {
        self.rate_limit_cooldown = time::Duration::try_from(d).unwrap_or(time::Duration::MAX);
        self
    }

    /// Keep-alive cadence on committed streams (`timeouts.stream_keepalive`, default 1 s).
    pub fn with_keepalive(mut self, d: Duration) -> Self {
        self.keepalive = d.max(Duration::from_millis(1));
        self
    }

    /// The context-carry cache (shared handle).
    pub fn context_carry(&self) -> &super::ContextCarry {
        &self.carry
    }

    async fn attempt_responses(
        &self,
        mut ctx: AttemptCtx<'_>,
        req: &InboundRequest,
        state: &StateBook,
    ) -> AttemptOutcome {
        // Moved into the stream pump on commit; otherwise released when this attempt returns.
        let reservation = ctx.reservation.take();
        let body = match codex_body(req, ctx.route) {
            Ok(b) => b,
            Err(o) => return *o,
        };
        let call = Call {
            acct: &ctx.account.id,
            state,
            req,
            route: ctx.route,
        };
        let account_id = self.chatgpt_account_id(call.acct).await;
        let mut token = ctx.token.clone();
        let mut refreshed = false;
        let mut retries = 0u32;
        loop {
            let last = match self
                .exchange(
                    &ctx,
                    &call,
                    &body,
                    account_id.as_deref(),
                    &mut token,
                    &mut refreshed,
                )
                .await
            {
                Step::Outcome(o) => return o,
                Step::Stream(c) => return stream::commit(self, &call, c, reservation),
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
                "codex overloaded; retrying same account"
            );
            self.overload.sleeper.sleep(delay).await;
            retries += 1;
        }
    }

    /// The stored ChatGPT account id (read off the async runtime). `None` when unknown.
    async fn chatgpt_account_id(&self, id: &AccountId) -> Option<String> {
        let (reg, id) = (self.reg.clone(), id.clone());
        tokio::task::spawn_blocking(move || reg.read_creds(&id))
            .await
            .ok()?
            .ok()
            .flatten()?
            .openai_account_id
            .filter(|a| !a.is_empty())
    }

    /// Send once (plus the one post-401/403 resend) and classify the response.
    async fn exchange(
        &self,
        ctx: &AttemptCtx<'_>,
        call: &Call<'_>,
        body: &Bytes,
        account_id: Option<&str>,
        token: &mut String,
        refreshed: &mut bool,
    ) -> Step {
        let mut resp = match self.send(ctx, body, account_id, token).await {
            Ok(r) => r,
            Err(k) => return Step::Outcome(transport_outcome(call, k, None)),
        };
        if is_auth_refusal(resp.status) && !*refreshed {
            *refreshed = true;
            drop(resp);
            match self.token_after_reject(ctx.account, token).await {
                Ok(t) => *token = t,
                Err(k) => return Step::Outcome(refresh_failure_outcome(k)),
            }
            resp = match self.send(ctx, body, account_id, token).await {
                Ok(r) => r,
                Err(k) => return Step::Outcome(transport_outcome(call, k, None)),
            };
        }

        let now = OffsetDateTime::now_utc();
        let status = resp.status;
        let snap = parse_codex_rate_limits(&resp.headers, status.as_u16() == 429, now);
        if let Some(s) = &snap {
            call.state.replace_header_snapshot(call.acct, s.clone());
        }
        if status.is_success() {
            let UpstreamResponse { headers, body, .. } = resp;
            return stream::read_prefix(self, call, headers, body).await;
        }
        // Like bro: an error body that dies mid-read is classified as if empty.
        let bytes = resp.body.read_all().await.unwrap_or_default();
        let b = Buffered {
            status,
            headers: resp.headers,
            body: bytes,
        };
        self.classify_error(call, b, snap.as_ref(), now)
    }

    async fn send(
        &self,
        ctx: &AttemptCtx<'_>,
        body: &Bytes,
        account_id: Option<&str>,
        token: &str,
    ) -> Result<UpstreamResponse, TransportKind> {
        let request = self.build_request(body, account_id, token)?;
        ctx.upstream.send(request, &ctx.limits).await
    }

    fn build_request(
        &self,
        body: &Bytes,
        account_id: Option<&str>,
        token: &str,
    ) -> Result<reqwest::Request, TransportKind> {
        let url: reqwest::Url = join_url(&self.cfg.api_base, RESPONSES_PATH)
            .parse()
            .map_err(|_| {
                tracing::error!("openai api_base is not a valid URL");
                TransportKind::Other
            })?;
        let bad = |what: &str| {
            tracing::error!("{what} is not a valid header value");
            TransportKind::Other
        };
        let mut auth = HeaderValue::from_str(&format!("Bearer {token}"))
            .map_err(|_| bad("stored access token"))?;
        auth.set_sensitive(true);
        let mut h = HeaderMap::new();
        h.insert(http::header::AUTHORIZATION, auth);
        h.insert(
            http::header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        );
        h.insert(
            http::header::ACCEPT,
            HeaderValue::from_static("text/event-stream"),
        );
        // We never decompress: ask for identity bytes.
        h.insert(
            http::header::ACCEPT_ENCODING,
            HeaderValue::from_static("identity"),
        );
        h.insert(
            HeaderName::from_static("originator"),
            HeaderValue::from_str(&self.cfg.originator).map_err(|_| bad("openai originator"))?,
        );
        if let Some(a) = account_id {
            h.insert(
                HeaderName::from_static(ACCOUNT_ID_HEADER),
                HeaderValue::from_str(a).map_err(|_| bad("stored ChatGPT account id"))?,
            );
        }
        let mut r = reqwest::Request::new(reqwest::Method::POST, url);
        *r.headers_mut() = h;
        *r.body_mut() = Some(reqwest::Body::from(body.clone()));
        Ok(r)
    }

    /// Token for the resend after a 401/403 on `rejected`: someone else's fresh rotation if there
    /// is one, otherwise a forced refresh.
    async fn token_after_reject(
        &self,
        acct: &AccountMeta,
        rejected: &str,
    ) -> Result<String, RefreshErrorKind> {
        let current = OpenAiProvider::access_token(self, acct, false).await?;
        if current != rejected {
            return Ok(current);
        }
        OpenAiProvider::access_token(self, acct, true).await
    }

    fn classify_error(
        &self,
        call: &Call<'_>,
        b: Buffered,
        snap: Option<&RateLimitSnapshot>,
        now: OffsetDateTime,
    ) -> Step {
        let code = b.status.as_u16();
        let upstream_request_id = request_id(&b.headers);
        let text = String::from_utf8_lossy(&b.body);
        let message = describe_codex_error(code, &text, call.acct.as_str());
        let log = |outcome: &str| {
            tracing::warn!(
                account = %call.acct,
                request_id = %call.req.request_id,
                status = code,
                upstream_request_id = upstream_request_id.as_deref().unwrap_or("-"),
                outcome,
                "codex upstream error"
            );
        };
        match code {
            401 | 403 => {
                // A fresh token still refused: this account cannot serve anyone's next turn.
                log("access_denied");
                call.state.mark_access_denied(call.acct, now);
                let reason = FailReason::AccessDenied;
                record(call.state, call.acct, reason, upstream_request_id.clone());
                let kind = if code == 401 {
                    "authentication_error"
                } else {
                    "permission_error"
                };
                Step::Outcome(AttemptOutcome::Retry {
                    reason,
                    fallback_response: anthropic_error(code, kind, &message),
                })
            }
            429 => {
                log("quota");
                let (until, window) = codex_quota_reset(snap, &b.headers, now)
                    .unwrap_or((now + self.rate_limit_cooldown, None));
                call.state.mark_rate_limited(call.acct, until, window, now);
                let reason = FailReason::Quota { reset: Some(until) };
                record(call.state, call.acct, reason, upstream_request_id.clone());
                let mut r = anthropic_error(429, "rate_limit_error", &message);
                set_retry_after(&mut r, Some(until), now);
                Step::Outcome(AttemptOutcome::Retry {
                    reason,
                    fallback_response: r,
                })
            }
            500..=599 => Step::Overloaded(b),
            _ => {
                log("terminal");
                let reason = FailReason::Status(code);
                record(call.state, call.acct, reason, upstream_request_id.clone());
                Step::Outcome(AttemptOutcome::Terminal(
                    anthropic_error(code, "api_error", &message),
                    reason,
                ))
            }
        }
    }

    /// Bench length when nothing better is known.
    pub(super) fn rate_limit_cooldown(&self) -> time::Duration {
        self.rate_limit_cooldown
    }
}

/// The Codex request body, or the 400 a translation error earns (nothing is sent upstream and
/// nothing is recorded on the account: the request itself is malformed).
fn codex_body(req: &InboundRequest, route: &Route) -> Result<Bytes, Box<AttemptOutcome>> {
    let translated = to_codex_request(&req.json, &route.def, route.effort.as_deref())
        .and_then(|v| Ok(serde_json::to_vec(&v)?));
    match translated {
        Ok(v) => Ok(Bytes::from(v)),
        Err(e) => {
            // The error names only the class of problem, never request content.
            tracing::warn!(
                request_id = %req.request_id,
                outcome = "invalid_request",
                "codex request translation failed"
            );
            let r = anthropic_error(
                400,
                "invalid_request_error",
                &format!("pooltime could not translate this request for Codex: {e}"),
            );
            Err(Box::new(AttemptOutcome::Terminal(
                r,
                FailReason::UpstreamRejected,
            )))
        }
    }
}

#[async_trait]
impl Provider for OpenAiProvider {
    fn id(&self) -> ProviderId {
        ProviderId::OpenAi
    }

    async fn access_token(
        &self,
        acct: &AccountMeta,
        force: bool,
    ) -> Result<String, RefreshErrorKind> {
        OpenAiProvider::access_token(self, acct, force).await
    }

    async fn attempt(
        &self,
        ctx: AttemptCtx<'_>,
        req: &InboundRequest,
        state: &StateBook,
    ) -> AttemptOutcome {
        self.attempt_responses(ctx, req, state).await
    }

    async fn poll_usage(&self, _acct: &AccountMeta) -> Result<RateLimitSnapshot, UsageError> {
        // TODO(Task 39): `wham/usage` poll.
        Err(UsageError::NotSupported)
    }

    async fn catalog(&self, _acct: &AccountMeta) -> anyhow::Result<Vec<RouteDef>> {
        // TODO(Task 39): `codex/models` discovery. Until then the bundled rows serve.
        Ok(Vec::new())
    }
}

fn is_auth_refusal(s: StatusCode) -> bool {
    matches!(s.as_u16(), 401 | 403)
}

/// One legible line for a Codex non-OK body (bro `describeCodexError`): the backend's `detail`,
/// else `error.message`, else `message`, else the raw body (trimmed), capped at 300 chars,
/// tagged with the account. For the client response only: never log or persist it.
pub fn describe_codex_error(status: u16, body: &str, account: &str) -> String {
    let json: serde_json::Value = serde_json::from_str(body).unwrap_or_default();
    let s = |v: Option<&serde_json::Value>| v.and_then(|v| v.as_str()).map(str::to_owned);
    let detail = s(json.get("detail"))
        .or_else(|| s(json.get("error").and_then(|e| e.get("message"))))
        .or_else(|| s(json.get("message")))
        .unwrap_or_else(|| body.trim().to_string());
    let detail: String = detail.chars().take(DETAIL_CHARS).collect();
    let cause = if detail.is_empty() {
        String::new()
    } else {
        format!(": {detail}")
    };
    format!("Codex backend rejected the request (HTTP {status}){cause} [account \"{account}\"]")
}

/// Overload never cleared: `Terminal(Overloaded)` with the legible upstream message and a
/// `retry-after` when a reset time is known. Recorded once; the account is not sidelined.
fn surface_overload(
    call: &Call<'_>,
    b: Buffered,
    reset: Option<OffsetDateTime>,
    now: OffsetDateTime,
) -> AttemptOutcome {
    let upstream_request_id = request_id(&b.headers);
    tracing::warn!(
        account = %call.acct,
        request_id = %call.req.request_id,
        status = b.status.as_u16(),
        upstream_request_id = upstream_request_id.as_deref().unwrap_or("-"),
        "codex overload persisted; surfacing upstream error"
    );
    record(
        call.state,
        call.acct,
        FailReason::Overloaded,
        upstream_request_id,
    );
    let code = b.status.as_u16();
    let kind = if code == 529 || code == 503 {
        "overloaded_error"
    } else {
        "api_error"
    };
    let message = describe_codex_error(code, &String::from_utf8_lossy(&b.body), call.acct.as_str());
    let mut r = anthropic_error(code, kind, &message);
    set_retry_after(&mut r, reset, now);
    AttemptOutcome::Terminal(r, FailReason::Overloaded)
}

fn set_retry_after(
    r: &mut axum::response::Response,
    reset: Option<OffsetDateTime>,
    now: OffsetDateTime,
) {
    let secs = reset.map(|t| (t - now).as_seconds_f64().ceil() as i64);
    if let Some(secs) = secs.filter(|s| *s > 0) {
        r.headers_mut()
            .insert(http::header::RETRY_AFTER, HeaderValue::from(secs));
    }
}

/// Transport failure (send, or a 2xx body that died before commit): record, 502 `api_error`
/// flagged retryable, fail over.
pub(super) fn transport_outcome(
    call: &Call<'_>,
    kind: TransportKind,
    upstream_request_id: Option<String>,
) -> AttemptOutcome {
    tracing::warn!(
        account = %call.acct,
        request_id = %call.req.request_id,
        transport = kind.as_str(),
        upstream_request_id = upstream_request_id.as_deref().unwrap_or("-"),
        "codex transport failure"
    );
    let reason = FailReason::Transport(kind);
    record(call.state, call.acct, reason, upstream_request_id);
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

pub(super) fn record(
    state: &StateBook,
    acct: &AccountId,
    reason: FailReason,
    upstream_request_id: Option<String>,
) {
    state.record_error(
        acct,
        ErrorRecord {
            at: OffsetDateTime::now_utc(),
            reason,
            upstream_request_id,
        },
    );
}

/// Upstream request id (`x-request-id`, else `request-id`), for logs. Only token-safe characters
/// are kept.
pub(super) fn request_id(h: &HeaderMap) -> Option<String> {
    ["x-request-id", "request-id"]
        .iter()
        .find_map(|n| h.get(*n))
        .and_then(|v| v.to_str().ok())
        .filter(|v| {
            v.len() <= 128
                && v.bytes()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'_' | b'-' | b'.'))
        })
        .map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn describe_prefers_detail_then_error_message_then_message_then_raw() {
        let d = |b: &str| describe_codex_error(400, b, "acct");
        assert_eq!(
            d(r#"{"detail":"Unsupported parameter: max_output_tokens","error":{"message":"x"}}"#),
            "Codex backend rejected the request (HTTP 400): Unsupported parameter: \
             max_output_tokens [account \"acct\"]"
        );
        assert!(d(r#"{"error":{"message":"from error"},"message":"m"}"#).contains(": from error ["));
        assert!(d(r#"{"message":"top"}"#).contains(": top ["));
        assert!(d("  plain body  ").contains(": plain body ["));
        assert_eq!(
            d(""),
            "Codex backend rejected the request (HTTP 400) [account \"acct\"]"
        );
        let long = format!("{{\"detail\":\"{}\"}}", "y".repeat(1000));
        assert_eq!(d(&long).matches('y').count(), 300);
    }

    #[test]
    fn request_id_is_sanitized() {
        let mut h = HeaderMap::new();
        h.insert("x-request-id", HeaderValue::from_static("req_abc-1.2"));
        assert_eq!(request_id(&h).as_deref(), Some("req_abc-1.2"));
        h.insert("x-request-id", HeaderValue::from_static("has spaces"));
        assert_eq!(request_id(&h), None);
    }
}
