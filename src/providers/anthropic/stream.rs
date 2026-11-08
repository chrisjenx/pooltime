//! Streaming (`"stream": true`) half of the Anthropic attempt (INV §1 "Streaming", spec §16).
//!
//! **Pre-commit** ([`read_prefix`]): upstream bytes are buffered, not forwarded, until the
//! stream commits: any SSE event other than `ping` (normally `message_start`), or 64 KiB of
//! prefix. Each read is bounded by the idle watchdog. Before commit:
//! - an `error` event with rate-limit wording -> bench + `Retry(Quota)` (caller: proxy.rs);
//! - an `overloaded_error` event -> the same-account backoff loop, surfaced as **529**;
//! - any other `error` event commits (it is forwarded like any other byte);
//! - a transport fault, EOF or idle expiry -> `Retry(Transport)`.
//!
//! **Post-commit** ([`commit`]): a spawned task forwards the prefix and then every upstream
//! chunk *unchanged* through an mpsc channel into the response body; nothing is injected.
//! A [`Tap`] parses the same bytes on the side for usage (`message_start` / `message_delta`,
//! cache tokens included) and the terminal event. On `message_stop` or `error` the terminal
//! bytes are enqueued, the channel is closed and the reservation released at once, even if
//! upstream keeps its socket open. Outcomes:
//!
//! | End | Account | Response body | [`StreamOutcome`] |
//! |---|---|---|---|
//! | `message_stop` | `record_success(usage)`, usage future resolves | clean end | `completed` |
//! | `error` event | rate-limit wording: bench + `Quota`; `overloaded_error`: `Overloaded`; else `Status(502)` | forwarded, clean end | `error` |
//! | EOF before a terminal event | `Transport(Reset)` | errors (no clean end) | `upstream_truncated` |
//! | idle watchdog | `Transport(IdleTimeout)` | errors | `idle_timeout` |
//! | reset / other transport / deadline | `Transport(kind)` | errors | `reset` / `error` |
//! | client gone (channel closed / send fails) | **nothing** | - | `client_cancelled` |
//!
//! A terminal event is only recognised once its blank line arrived (`SseParser::push`); the
//! parser's `finish()` is never consulted, because it would dispatch a truncated last event as
//! if it were complete.
//!
//! Post-commit recording uses the provider's own `Arc<StateBook>` (the spawned task outlives the
//! `&StateBook` borrow of `attempt`); it must be the same book the caller passes to `attempt`.

use std::sync::Arc;

use axum::body::Body;
use axum::response::Response;
use bytes::Bytes;
use http::{HeaderMap, StatusCode};
use time::OffsetDateTime;
use tokio::sync::{mpsc, oneshot};

use super::has_rate_limit_wording;
use super::proxy::{error_fields, quota_until, request_id, response_headers};
use crate::accounts::{AccountId, ErrorRecord, StateBook, TokenUsage};
use crate::errors::{FailReason, TransportKind};
use crate::providers::AttemptOutcome;
use crate::routing::reserve::Reservation;
use crate::sse::{SseEvent, SseParser};
use crate::upstream::WatchedBody;

/// Pre-commit buffering cap: this much prefix commits even without a non-`ping` event.
pub const PREFIX_CAP: usize = 64 * 1024;
/// Chunks queued towards the client before the pump waits for it.
const CHANNEL_CAPACITY: usize = 16;

/// How a committed stream ended (the `outcome` field of the `anthropic stream finished` event).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamOutcome {
    Completed,
    ClientCancelled,
    UpstreamTruncated,
    IdleTimeout,
    Reset,
    Error,
}

impl StreamOutcome {
    pub fn as_str(self) -> &'static str {
        match self {
            StreamOutcome::Completed => "completed",
            StreamOutcome::ClientCancelled => "client_cancelled",
            StreamOutcome::UpstreamTruncated => "upstream_truncated",
            StreamOutcome::IdleTimeout => "idle_timeout",
            StreamOutcome::Reset => "reset",
            StreamOutcome::Error => "error",
        }
    }

    fn of_transport(k: TransportKind) -> StreamOutcome {
        match k {
            TransportKind::IdleTimeout => StreamOutcome::IdleTimeout,
            TransportKind::Reset => StreamOutcome::Reset,
            _ => StreamOutcome::Error,
        }
    }
}

/// Owned by the task pumping a committed stream. Dropping it (normal end, failure, client
/// disconnect, task abort or panic) emits one structured `anthropic stream finished` event
/// (request id, account, outcome) and releases the in-flight reservation.
///
/// TODO(F4): also write the journal row here once the journal exists.
pub struct StreamGuard {
    reservation: Option<Reservation>,
    account: AccountId,
    request_id: String,
    outcome: StreamOutcome,
}

impl StreamGuard {
    /// Outcome reported on drop until [`StreamGuard::set`] says otherwise (`error`).
    fn new(reservation: Option<Reservation>, account: AccountId, request_id: String) -> Self {
        StreamGuard {
            reservation,
            account,
            request_id,
            outcome: StreamOutcome::Error,
        }
    }

    fn set(&mut self, o: StreamOutcome) {
        self.outcome = o;
    }
}

impl Drop for StreamGuard {
    fn drop(&mut self) {
        let outcome = self.outcome.as_str();
        match self.outcome {
            StreamOutcome::Completed | StreamOutcome::ClientCancelled => tracing::info!(
                request_id = %self.request_id,
                account = %self.account,
                outcome,
                "anthropic stream finished"
            ),
            _ => tracing::warn!(
                request_id = %self.request_id,
                account = %self.account,
                outcome,
                "anthropic stream finished"
            ),
        }
        drop(self.reservation.take());
    }
}

/// Classification of an SSE `error` event (`error.type` / `error.message`, bro
/// `classifySseError`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SseErrorKind {
    RateLimit,
    Overloaded,
    Other,
}

/// A pre-commit `error` event that decides the attempt without committing.
pub(super) enum Initial {
    RateLimit(String),
    Overloaded(String),
}

/// Side parser over the forwarded bytes: commit/terminal detection and usage.
pub(super) struct Tap {
    parser: SseParser,
    pub(super) committed: bool,
    /// `message_stop` or `error` seen (complete, i.e. with its blank line).
    pub(super) terminal: bool,
    usage: TokenUsage,
    initial: Option<Initial>,
    /// Post-commit `error` event: kind and sanitized type (for logs).
    final_error: Option<(SseErrorKind, Option<String>)>,
}

impl Tap {
    fn new() -> Tap {
        Tap {
            parser: SseParser::default(),
            committed: false,
            terminal: false,
            usage: TokenUsage::default(),
            initial: None,
            final_error: None,
        }
    }

    fn push(&mut self, chunk: &[u8]) -> Result<(), crate::sse::SseError> {
        for ev in self.parser.push(chunk)? {
            self.on_event(&ev);
        }
        Ok(())
    }

    fn on_event(&mut self, ev: &SseEvent) {
        if self.initial.is_some() || self.terminal {
            return;
        }
        let data: serde_json::Value = serde_json::from_str(&ev.data).unwrap_or_default();
        let kind = data
            .get("type")
            .and_then(|t| t.as_str())
            .or(ev.event.as_deref())
            .unwrap_or("");
        match kind {
            "ping" => return,
            "message_start" => self.merge_usage(data.get("message").and_then(|m| m.get("usage"))),
            "message_delta" => self.merge_usage(data.get("usage")),
            "message_stop" => self.terminal = true,
            "error" => {
                let (kind, safe_type) = classify_sse_error(&ev.data);
                if !self.committed {
                    match kind {
                        SseErrorKind::RateLimit => {
                            self.initial = Some(Initial::RateLimit(ev.data.clone()));
                            return;
                        }
                        SseErrorKind::Overloaded => {
                            self.initial = Some(Initial::Overloaded(ev.data.clone()));
                            return;
                        }
                        SseErrorKind::Other => {}
                    }
                }
                self.terminal = true;
                self.final_error = Some((kind, safe_type));
            }
            _ => {}
        }
        self.committed = true;
    }

    /// Later values win per field (message_delta carries the cumulative output count).
    fn merge_usage(&mut self, u: Option<&serde_json::Value>) {
        let Some(u) = u else { return };
        let n = |k: &str| u.get(k).and_then(|v| v.as_u64());
        if let Some(v) = n("input_tokens") {
            self.usage.input_tokens = v;
        }
        if let Some(v) = n("output_tokens") {
            self.usage.output_tokens = v;
        }
        if let Some(v) = n("cache_read_input_tokens") {
            self.usage.cache_read = v;
        }
        if let Some(v) = n("cache_creation_input_tokens") {
            self.usage.cache_write = v;
        }
    }
}

/// `(kind, sanitized error.type)`. Message text is used for the wording check only.
fn classify_sse_error(data: &str) -> (SseErrorKind, Option<String>) {
    let json: serde_json::Value = serde_json::from_str(data).unwrap_or_default();
    let field = |k: &str| json.get("error")?.get(k)?.as_str();
    let raw_type = field("type").unwrap_or("api_error");
    let message = field("message").unwrap_or("Anthropic streaming error");
    let safe_type = error_fields(data).0;
    let kind = if has_rate_limit_wording(&format!("{raw_type}\n{message}")) {
        SseErrorKind::RateLimit
    } else if raw_type == "overloaded_error" {
        SseErrorKind::Overloaded
    } else {
        SseErrorKind::Other
    };
    (kind, safe_type)
}

/// Result of the pre-commit read.
pub(super) enum Prefix {
    Commit(Box<Committed>),
    /// Initial rate-limit `error` event (its `data`).
    RateLimited(String),
    /// Initial `overloaded_error` event (its `data`).
    Overloaded(String),
    Fault(TransportKind),
}

/// A committed stream, ready to be pumped.
pub(super) struct Committed {
    pub(super) status: StatusCode,
    pub(super) headers: HeaderMap,
    prefix: Vec<Bytes>,
    tap: Tap,
    body: WatchedBody,
}

/// Buffer until commit (or [`PREFIX_CAP`]); every read is bounded by the idle watchdog.
pub(super) async fn read_prefix(
    status: StatusCode,
    headers: HeaderMap,
    mut body: WatchedBody,
) -> Prefix {
    let mut tap = Tap::new();
    let mut prefix = Vec::new();
    let mut size = 0usize;
    while !tap.committed && size < PREFIX_CAP {
        let chunk = match body.next_chunk().await {
            // Nothing committed yet, so nothing reached the client: fail over.
            None => return Prefix::Fault(TransportKind::Reset),
            Some(Err(k)) => return Prefix::Fault(k),
            Some(Ok(b)) => b,
        };
        size += chunk.len();
        if tap.push(&chunk).is_err() {
            return Prefix::Fault(TransportKind::Other);
        }
        prefix.push(chunk);
        match tap.initial.take() {
            Some(Initial::RateLimit(data)) => return Prefix::RateLimited(data),
            Some(Initial::Overloaded(data)) => return Prefix::Overloaded(data),
            None => {}
        }
    }
    tap.committed = true;
    Prefix::Commit(Box::new(Committed {
        status,
        headers,
        prefix,
        tap,
        body,
    }))
}

/// What the pump needs to record outcomes on the account.
pub(super) struct StreamCtx {
    pub(super) account: AccountId,
    pub(super) request_id: String,
    pub(super) state: Arc<StateBook>,
    /// Bench length for a post-commit rate-limit error with no explicit/window reset.
    pub(super) rate_limit_cooldown: time::Duration,
}

type Chunk = Result<Bytes, std::io::Error>;

/// Build the streamed response and spawn the pump that owns `reservation` until the stream ends.
pub(super) fn commit(
    c: Box<Committed>,
    ctx: StreamCtx,
    reservation: Option<Reservation>,
) -> AttemptOutcome {
    let (tx, rx) = mpsc::channel::<Chunk>(CHANNEL_CAPACITY);
    let (usage_tx, usage_rx) = oneshot::channel();
    let body = Body::from_stream(futures_util::stream::unfold(rx, |mut rx| async move {
        rx.recv().await.map(|item| (item, rx))
    }));
    let mut resp = Response::new(body);
    *resp.status_mut() = c.status;
    *resp.headers_mut() = response_headers(&c.headers, &ctx.account);
    let guard = StreamGuard::new(reservation, ctx.account.clone(), ctx.request_id.clone());
    tokio::spawn(pump(c, ctx, guard, tx, usage_tx));
    AttemptOutcome::Done(resp, Some(usage_rx))
}

async fn pump(
    c: Box<Committed>,
    ctx: StreamCtx,
    mut guard: StreamGuard,
    tx: mpsc::Sender<Chunk>,
    usage_tx: oneshot::Sender<TokenUsage>,
) {
    let Committed {
        headers,
        prefix,
        mut tap,
        mut body,
        ..
    } = *c;
    let upstream_request_id = request_id(&headers);
    for chunk in prefix {
        if tx.send(Ok(chunk)).await.is_err() {
            guard.set(StreamOutcome::ClientCancelled);
            return;
        }
    }
    loop {
        if tap.terminal {
            // Close the response now: upstream may keep its socket open after the terminal event.
            drop(tx);
            drop(body);
            let outcome = finish(&ctx, &tap, &headers, upstream_request_id, usage_tx);
            guard.set(outcome);
            return;
        }
        let next = tokio::select! {
            biased;
            _ = tx.closed() => {
                guard.set(StreamOutcome::ClientCancelled);
                return;
            }
            next = body.next_chunk() => next,
        };
        let (outcome, kind) = match next {
            Some(Ok(chunk)) => {
                let parsed = tap.push(&chunk);
                if tx.send(Ok(chunk)).await.is_err() {
                    guard.set(StreamOutcome::ClientCancelled);
                    return;
                }
                if parsed.is_ok() {
                    continue;
                }
                (StreamOutcome::Error, TransportKind::Other)
            }
            None => (StreamOutcome::UpstreamTruncated, TransportKind::Reset),
            Some(Err(k)) => (StreamOutcome::of_transport(k), k),
        };
        drop(body);
        // Never record anything once the client is gone.
        if tx.is_closed() {
            guard.set(StreamOutcome::ClientCancelled);
            return;
        }
        tracing::warn!(
            account = %ctx.account,
            request_id = %ctx.request_id,
            transport = kind.as_str(),
            outcome = outcome.as_str(),
            upstream_request_id = upstream_request_id.as_deref().unwrap_or("-"),
            "anthropic stream failed after commit"
        );
        record(&ctx, FailReason::Transport(kind), upstream_request_id);
        guard.set(outcome);
        // Error the body so the client sees an aborted response, not a clean end.
        let _ = tx
            .send(Err(std::io::Error::other(format!(
                "upstream stream failed ({})",
                outcome.as_str()
            ))))
            .await;
        return;
    }
}

/// Terminal event reached: record success (resolving the usage future) or the error event.
fn finish(
    ctx: &StreamCtx,
    tap: &Tap,
    headers: &HeaderMap,
    upstream_request_id: Option<String>,
    usage_tx: oneshot::Sender<TokenUsage>,
) -> StreamOutcome {
    let now = OffsetDateTime::now_utc();
    let Some((kind, safe_type)) = &tap.final_error else {
        let usage = tap.usage;
        ctx.state.record_success(&ctx.account, usage, now);
        tracing::info!(
            account = %ctx.account,
            request_id = %ctx.request_id,
            upstream_request_id = upstream_request_id.as_deref().unwrap_or("-"),
            input_tokens = usage.input_tokens,
            output_tokens = usage.output_tokens,
            "anthropic stream served"
        );
        let _ = usage_tx.send(usage);
        return StreamOutcome::Completed;
    };
    let reason = match kind {
        SseErrorKind::RateLimit => {
            let (until, window) = quota_until(
                &ctx.state,
                &ctx.account,
                headers,
                now,
                ctx.rate_limit_cooldown,
            );
            ctx.state
                .mark_rate_limited(&ctx.account, until, window, now);
            FailReason::Quota { reset: Some(until) }
        }
        SseErrorKind::Overloaded => FailReason::Overloaded,
        SseErrorKind::Other => FailReason::Status(502),
    };
    tracing::warn!(
        account = %ctx.account,
        request_id = %ctx.request_id,
        error_type = safe_type.as_deref().unwrap_or("-"),
        reason = %reason.detail(),
        upstream_request_id = upstream_request_id.as_deref().unwrap_or("-"),
        "anthropic stream error event"
    );
    record(ctx, reason, upstream_request_id);
    StreamOutcome::Error
}

fn record(ctx: &StreamCtx, reason: FailReason, upstream_request_id: Option<String>) {
    ctx.state.record_error(
        &ctx.account,
        ErrorRecord {
            at: OffsetDateTime::now_utc(),
            reason,
            upstream_request_id,
        },
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tap_events(input: &str) -> Tap {
        let mut t = Tap::new();
        t.push(input.as_bytes()).unwrap();
        t
    }

    #[test]
    fn ping_does_not_commit_message_start_does() {
        let t = tap_events("event: ping\ndata: {\"type\":\"ping\"}\n\n");
        assert!(!t.committed && !t.terminal);
        let t = tap_events(
            "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{}}\n\n",
        );
        assert!(t.committed && !t.terminal);
    }

    #[test]
    fn initial_errors_classified_without_commit() {
        let t = tap_events(
            "event: error\ndata: {\"type\":\"error\",\"error\":{\"type\":\"rate_limit_error\",\"message\":\"x\"}}\n\n",
        );
        assert!(matches!(t.initial, Some(Initial::RateLimit(_))) && !t.committed);
        let t = tap_events(
            "event: error\ndata: {\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\",\"message\":\"Overloaded\"}}\n\n",
        );
        assert!(matches!(t.initial, Some(Initial::Overloaded(_))) && !t.committed);
        // Any other initial error commits and is terminal.
        let t = tap_events(
            "event: error\ndata: {\"type\":\"error\",\"error\":{\"type\":\"api_error\",\"message\":\"boom\"}}\n\n",
        );
        assert!(t.initial.is_none() && t.committed && t.terminal);
        assert_eq!(
            t.final_error,
            Some((SseErrorKind::Other, Some("api_error".into())))
        );
    }

    #[test]
    fn terminal_needs_complete_event() {
        let t = tap_events("event: message_stop\ndata: {\"type\":\"message_stop\"}\n");
        assert!(!t.terminal);
        let t = tap_events("event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n");
        assert!(t.terminal);
    }

    #[test]
    fn unsafe_error_type_not_kept_for_logs() {
        let (k, t) = classify_sse_error(
            "{\"type\":\"error\",\"error\":{\"type\":\"has spaces\",\"message\":\"Usage limit reached\"}}",
        );
        assert_eq!((k, t), (SseErrorKind::RateLimit, None));
        let (k, _) = classify_sse_error("not json");
        assert_eq!(k, SseErrorKind::Other);
    }

    #[test]
    fn outcome_labels() {
        let all = [
            StreamOutcome::Completed,
            StreamOutcome::ClientCancelled,
            StreamOutcome::UpstreamTruncated,
            StreamOutcome::IdleTimeout,
            StreamOutcome::Reset,
            StreamOutcome::Error,
        ];
        let got: Vec<_> = all.iter().map(|o| o.as_str()).collect();
        assert_eq!(
            got,
            [
                "completed",
                "client_cancelled",
                "upstream_truncated",
                "idle_timeout",
                "reset",
                "error"
            ]
        );
        assert_eq!(
            StreamOutcome::of_transport(TransportKind::Deadline),
            StreamOutcome::Error
        );
    }
}
