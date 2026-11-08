//! The 2xx half of a Codex attempt: Codex SSE in, Anthropic SSE (or a folded message) out
//! (INV §4 "Stream translation"; bro `streamCodexResponse`). The translation itself is
//! [`CodexToAnthropic`]; this module owns transport, buffering, keep-alive and recording.
//!
//! **Pre-commit** ([`read_prefix`], streaming clients): upstream chunks are translated but the
//! frames are held until the first content frame (anything but `ping` and `message_start`,
//! normally the first `content_block_start`), the end of the response, or [`PREFIX_CAP`] bytes
//! of upstream prefix. Holding `message_start` too means a failure right after
//! `response.created` still fails over instead of streaming (bro committed on `message_start`).
//! At the cap a `message_start` is forced if none was produced yet (a real `response.created`
//! echoes the full instructions and tool schemas and can exceed it); the late real one is then
//! a no-op. Every read is bounded
//! by the idle watchdog. When the response ends (terminal event or EOF) before anything reached
//! the client, it is settled without streaming — an upstream error is never sent as a lone
//! frame inside a 200:
//!
//! | Pre-commit end | Outcome |
//! |---|---|
//! | clean terminal | the whole translated stream as one `Done` |
//! | `error` / `response.failed`, rate-limit type or wording | bench (cooldown), `Retry(Quota)`, 429 |
//! | ... overloaded type | same-account backoff (like an Anthropic `overloaded_error` event), then 529 |
//! | ... any other type | `Terminal(Status(502))`, the mapped Anthropic type and the upstream message |
//! | malformed tool arguments | `Terminal(Status(502))` (another account would not do better) |
//! | EOF without terminal / without `message_start` | `Retry(Transport(Reset))` |
//! | transport fault / idle | `Retry(Transport(kind))` |
//!
//! **Post-commit** ([`commit`]): a spawned task translates the rest into an mpsc channel feeding
//! the response body. The same task runs a `tokio::time::interval(keepalive)` that is reset by
//! every frame sent and, when it fires (no output for `keepalive`), queues an `event: ping` — it
//! is polled alongside the pending upstream read, so it fires while upstream is silent and never
//! depends on the client pulling; the pending read is kept across ticks, so pings never re-arm
//! the idle watchdog. Consecutive pings (translator pings for dropped events, timer pings)
//! collapse into one. On the terminal event the upstream body is dropped at once (teardown
//! without waiting for EOF) and [`CodexToAnthropic::finish`] decides:
//!
//! | End | Account | Response body | outcome |
//! |---|---|---|---|
//! | clean terminal | `record_success(usage)`, context carried, usage future resolves | closing frames, clean end | `completed` |
//! | upstream error event | rate limit: bench + `Quota`; overloaded: `Overloaded`; else `Status(502)` | `error` frame, clean end | `error` |
//! | malformed tool arguments | `Status(502)` | `error` frame, clean end | `error` |
//! | EOF before a terminal event | `Transport(Reset)` | `error` frame, clean end | `upstream_truncated` |
//! | transport fault / idle / deadline | `Transport(kind)` | errors (no clean end) | per kind |
//! | client gone | **nothing** | - | `client_cancelled` |
//!
//! **Non-streaming clients** ([`fold`]): the whole upstream stream is read through the
//! translator (bounded by the idle watchdog and the attempt deadline), then
//! [`CodexToAnthropic::into_message`]; errors settle exactly like the pre-commit table.
//!
//! Context carry: `message_start.usage.input_tokens` is seeded from the provider's
//! [`super::ContextCarry`] under (session, route id); only a successful finish records.
//! Post-commit recording uses the provider's own `Arc<StateBook>`: it must be the book the caller
//! passes to `attempt`.

use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::response::Response;
use bytes::Bytes;
use http::{HeaderMap, HeaderName, HeaderValue, StatusCode};
use time::OffsetDateTime;
use tokio::sync::{mpsc, oneshot};
use tokio::time::{Instant, MissedTickBehavior};

use super::proxy::{record, request_id, transport_outcome, Buffered, Call, Step};
use super::{CodexToAnthropic, ContextCarry, OpenAiProvider, StreamError};
use crate::accounts::{AccountId, StateBook, TokenUsage};
use crate::errors::{FailReason, TransportKind};
use crate::providers::anthropic::{anthropic_error, StreamOutcome, POOL_ACCOUNT_HEADER};
use crate::providers::AttemptOutcome;
use crate::routing::reserve::Reservation;
use crate::sse::{encode, SseParser};
use crate::upstream::WatchedBody;

/// Pre-commit buffering cap (upstream bytes): reaching it forces `message_start`.
pub const PREFIX_CAP: usize = 64 * 1024;
/// Chunks queued towards the client before the pump waits for it.
const CHANNEL_CAPACITY: usize = 16;

type Chunk = Result<Bytes, std::io::Error>;

fn ping_frame() -> Bytes {
    encode("ping", r#"{"type":"ping"}"#)
}

/// A frame that commits the response: anything but `ping` and `message_start`.
fn is_content(f: &Bytes) -> bool {
    *f != ping_frame() && !f.starts_with(b"event: message_start\n")
}

/// Upstream body + SSE parser + translator.
pub(super) struct Reader {
    body: WatchedBody,
    parser: SseParser,
    t: CodexToAnthropic,
    eof: bool,
    read: usize,
}

impl Reader {
    fn new(body: WatchedBody, t: CodexToAnthropic) -> Reader {
        Reader {
            body,
            parser: SseParser::default(),
            t,
            eof: false,
            read: 0,
        }
    }

    /// Terminal event seen or upstream ended.
    fn done(&self) -> bool {
        self.eof || self.t.is_terminal()
    }

    /// Translate one upstream chunk. An oversized SSE line is a transport fault.
    fn feed(&mut self, chunk: &[u8]) -> Result<Vec<Bytes>, TransportKind> {
        self.read += chunk.len();
        let events = self.parser.push(chunk).map_err(|_| TransportKind::Other)?;
        Ok(events.iter().flat_map(|ev| self.t.on_event(ev)).collect())
    }

    /// Upstream EOF: dispatch a last event that lacked its blank line (bro `parser.end()`).
    fn end(&mut self) -> Vec<Bytes> {
        self.eof = true;
        let events = std::mem::take(&mut self.parser)
            .finish()
            .unwrap_or_default();
        events.iter().flat_map(|ev| self.t.on_event(ev)).collect()
    }

    /// One bounded read, translated.
    async fn step(&mut self) -> Result<Vec<Bytes>, TransportKind> {
        match self.body.next_chunk().await {
            None => Ok(self.end()),
            Some(Err(k)) => Err(k),
            Some(Ok(chunk)) => self.feed(&chunk),
        }
    }
}

/// A committed stream, ready to be pumped.
pub(super) struct Committed {
    pending: Vec<Bytes>,
    reader: Reader,
    upstream_request_id: Option<String>,
}

/// Translate a 2xx Codex response: buffer until commit (streaming client) or fold the whole
/// response (non-streaming client). See the module docs.
pub(super) async fn read_prefix(
    p: &OpenAiProvider,
    call: &Call<'_>,
    headers: HeaderMap,
    body: WatchedBody,
) -> Step {
    let route_id = &call.route.def.id;
    let seed = p.carry.seed(call.req.session_user_id.as_deref(), route_id);
    let mut rd = Reader::new(body, CodexToAnthropic::new(route_id, seed));
    let rid = request_id(&headers);
    if !call.req.stream {
        return fold(p, call, rd, rid).await;
    }
    let mut pending: Vec<Bytes> = Vec::new();
    let mut committed = false;
    while !committed && !rd.done() && rd.read < PREFIX_CAP {
        match rd.step().await {
            Err(k) => return Step::Outcome(transport_outcome(call, k, rid)),
            Ok(frames) => {
                committed = frames.iter().any(is_content);
                pending.extend(frames);
            }
        }
    }
    if rd.done() {
        // Nothing reached the client yet: settle the whole response now.
        return match rd.t.finish() {
            Ok(f) => {
                pending.extend(f.frames);
                let usage = served(p, call, f.usage, f.context_tokens, rid.as_deref());
                Step::Outcome(AttemptOutcome::Done(
                    sse_response(Body::from(pending.concat()), call.acct),
                    Some(usage),
                ))
            }
            Err(e) => settle_unsent(p, call, e, rid),
        };
    }
    if !rd.t.is_started() {
        // Prefix cap before `response.created` parsed (it echoes the full instructions and can
        // exceed the cap): open the stream now; the real one becomes a no-op.
        pending.insert(0, rd.t.force_message_start());
    }
    Step::Stream(Box::new(Committed {
        pending,
        reader: rd,
        upstream_request_id: rid,
    }))
}

/// Non-streaming client: read everything, fold one message.
async fn fold(p: &OpenAiProvider, call: &Call<'_>, mut rd: Reader, rid: Option<String>) -> Step {
    while !rd.done() {
        if let Err(k) = rd.step().await {
            return Step::Outcome(transport_outcome(call, k, rid));
        }
    }
    drop(rd.body);
    let (usage, context) = (rd.t.usage(), rd.t.context_tokens());
    match rd.t.into_message() {
        Ok(msg) => {
            let usage = served(p, call, usage, context, rid.as_deref());
            let mut r = Response::new(Body::from(msg.to_string()));
            r.headers_mut().insert(
                http::header::CONTENT_TYPE,
                HeaderValue::from_static("application/json"),
            );
            tag_account(&mut r, call.acct);
            Step::Outcome(AttemptOutcome::Done(r, Some(usage)))
        }
        Err(e) => settle_unsent(p, call, e, rid),
    }
}

/// A clean response that never streamed: record success and the carried context.
fn served(
    p: &OpenAiProvider,
    call: &Call<'_>,
    usage: TokenUsage,
    context: Option<u64>,
    rid: Option<&str>,
) -> oneshot::Receiver<TokenUsage> {
    call.state
        .record_success(call.acct, usage, OffsetDateTime::now_utc());
    if let Some(tokens) = context {
        p.carry.record(
            call.req.session_user_id.as_deref(),
            &call.route.def.id,
            tokens,
        );
    }
    tracing::info!(
        account = %call.acct,
        request_id = %call.req.request_id,
        upstream_request_id = rid.unwrap_or("-"),
        input_tokens = usage.input_tokens,
        output_tokens = usage.output_tokens,
        "codex request served"
    );
    let (tx, rx) = oneshot::channel();
    let _ = tx.send(usage);
    rx
}

/// The response failed before anything reached the client (bro `failedBeforeCommit`).
fn settle_unsent(p: &OpenAiProvider, call: &Call<'_>, e: StreamError, rid: Option<String>) -> Step {
    let now = OffsetDateTime::now_utc();
    let kind = e.anthropic_type();
    let log = |outcome: &str| {
        tracing::warn!(
            account = %call.acct,
            request_id = %call.req.request_id,
            error_type = kind,
            upstream_request_id = rid.as_deref().unwrap_or("-"),
            outcome,
            "codex stream failed before commit"
        );
    };
    match &e {
        StreamError::MissingTerminal | StreamError::NoMessageStart => {
            Step::Outcome(transport_outcome(call, TransportKind::Reset, rid))
        }
        StreamError::Upstream { message, .. } if kind == "rate_limit_error" => {
            log("quota");
            let until = now + p.rate_limit_cooldown();
            call.state.mark_rate_limited(call.acct, until, None, now);
            let reason = FailReason::Quota { reset: Some(until) };
            record(call.state, call.acct, reason, rid);
            Step::Outcome(AttemptOutcome::Retry {
                reason,
                fallback_response: anthropic_error(429, kind, message),
            })
        }
        StreamError::Upstream { message, .. } if kind == "overloaded_error" => {
            log("overloaded");
            let body = serde_json::json!({"error": {"type": kind, "message": message}});
            Step::Overloaded(Buffered {
                status: StatusCode::from_u16(529).expect("529 is a valid status"),
                headers: HeaderMap::new(),
                body: Bytes::from(body.to_string()),
            })
        }
        _ => {
            log("terminal");
            let reason = FailReason::Status(502);
            record(call.state, call.acct, reason, rid);
            Step::Outcome(AttemptOutcome::Terminal(
                anthropic_error(502, kind, e.message()),
                reason,
            ))
        }
    }
}

fn tag_account(r: &mut Response, acct: &AccountId) {
    if let Ok(v) = HeaderValue::from_str(acct.as_str()) {
        r.headers_mut()
            .insert(HeaderName::from_static(POOL_ACCOUNT_HEADER), v);
    }
}

fn sse_response(body: Body, acct: &AccountId) -> Response {
    let mut r = Response::new(body);
    *r.status_mut() = StatusCode::OK;
    let h = r.headers_mut();
    h.insert(
        http::header::CONTENT_TYPE,
        HeaderValue::from_static("text/event-stream; charset=utf-8"),
    );
    h.insert(
        http::header::CACHE_CONTROL,
        HeaderValue::from_static("no-cache"),
    );
    tag_account(&mut r, acct);
    r
}

/// Owned by the task pumping a committed Codex stream. Dropping it (normal end, failure, client
/// disconnect, task abort or panic) emits one `codex stream finished` event (request id,
/// account, outcome) and releases the in-flight reservation.
pub struct CodexStreamGuard {
    reservation: Option<Reservation>,
    account: AccountId,
    request_id: String,
    outcome: StreamOutcome,
}

impl CodexStreamGuard {
    fn new(reservation: Option<Reservation>, account: AccountId, request_id: String) -> Self {
        CodexStreamGuard {
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

impl Drop for CodexStreamGuard {
    fn drop(&mut self) {
        let outcome = self.outcome.as_str();
        match self.outcome {
            StreamOutcome::Completed | StreamOutcome::ClientCancelled => tracing::info!(
                request_id = %self.request_id,
                account = %self.account,
                outcome,
                "codex stream finished"
            ),
            _ => tracing::warn!(
                request_id = %self.request_id,
                account = %self.account,
                outcome,
                "codex stream finished"
            ),
        }
        drop(self.reservation.take());
    }
}

/// What the pump needs to record outcomes on the account.
struct StreamCtx {
    account: AccountId,
    request_id: String,
    state: Arc<StateBook>,
    cooldown: time::Duration,
    keepalive: Duration,
    carry: ContextCarry,
    session: Option<String>,
    route_id: String,
}

/// Build the streamed response and spawn the pump that owns `reservation` until the stream ends.
pub(super) fn commit(
    p: &OpenAiProvider,
    call: &Call<'_>,
    c: Box<Committed>,
    reservation: Option<Reservation>,
) -> AttemptOutcome {
    let (tx, rx) = mpsc::channel::<Chunk>(CHANNEL_CAPACITY);
    let (usage_tx, usage_rx) = oneshot::channel();
    let body = Body::from_stream(futures_util::stream::unfold(rx, |mut rx| async move {
        rx.recv().await.map(|item| (item, rx))
    }));
    let resp = sse_response(body, call.acct);
    let ctx = StreamCtx {
        account: call.acct.clone(),
        request_id: call.req.request_id.clone(),
        state: p.state.clone(),
        cooldown: p.rate_limit_cooldown(),
        keepalive: p.keepalive,
        carry: p.carry.clone(),
        session: call.req.session_user_id.clone(),
        route_id: call.route.def.id.clone(),
    };
    let guard = CodexStreamGuard::new(reservation, ctx.account.clone(), ctx.request_id.clone());
    tokio::spawn(pump(*c, ctx, guard, tx, usage_tx));
    AttemptOutcome::Done(resp, Some(usage_rx))
}

/// The client side of the pump: forwards frames, collapsing consecutive pings.
struct Out {
    tx: mpsc::Sender<Chunk>,
    last_was_ping: bool,
}

impl Out {
    /// Send `frames` in order. `Ok(true)` when anything was sent; `Err` when the client is gone.
    async fn send_all(&mut self, frames: Vec<Bytes>) -> Result<bool, ()> {
        let mut sent = false;
        for f in frames {
            let ping = f == ping_frame();
            if ping && self.last_was_ping {
                continue;
            }
            self.tx.send(Ok(f)).await.map_err(|_| ())?;
            self.last_was_ping = ping;
            sent = true;
        }
        Ok(sent)
    }

    /// Keep-alive tick: queue a ping unless the client is not even draining what it has.
    fn keepalive(&mut self) -> Result<(), ()> {
        match self.tx.try_send(Ok(ping_frame())) {
            Ok(()) => {
                self.last_was_ping = true;
                Ok(())
            }
            Err(mpsc::error::TrySendError::Full(_)) => Ok(()),
            Err(mpsc::error::TrySendError::Closed(_)) => Err(()),
        }
    }
}

async fn pump(
    c: Committed,
    ctx: StreamCtx,
    mut guard: CodexStreamGuard,
    tx: mpsc::Sender<Chunk>,
    usage_tx: oneshot::Sender<TokenUsage>,
) {
    let Committed {
        pending,
        reader: mut rd,
        upstream_request_id: rid,
    } = c;
    let mut out = Out {
        tx,
        last_was_ping: false,
    };
    if out.send_all(pending).await.is_err() {
        guard.set(StreamOutcome::ClientCancelled);
        return;
    }
    let mut tick = tokio::time::interval_at(Instant::now() + ctx.keepalive, ctx.keepalive);
    tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
    while !rd.done() {
        let next = {
            // Kept across keep-alive ticks: re-creating it would re-arm the idle watchdog.
            let read = rd.body.next_chunk();
            tokio::pin!(read);
            loop {
                tokio::select! {
                    biased;
                    _ = out.tx.closed() => {
                        guard.set(StreamOutcome::ClientCancelled);
                        return;
                    }
                    r = &mut read => break r,
                    _ = tick.tick() => {
                        if out.keepalive().is_err() {
                            guard.set(StreamOutcome::ClientCancelled);
                            return;
                        }
                    }
                }
            }
        };
        let frames = match next {
            Some(Ok(chunk)) => match rd.feed(&chunk) {
                Ok(f) => f,
                Err(k) => return fault(&ctx, &mut guard, rd, out, k, rid).await,
            },
            None => rd.end(),
            Some(Err(k)) => return fault(&ctx, &mut guard, rd, out, k, rid).await,
        };
        match out.send_all(frames).await {
            Err(()) => {
                guard.set(StreamOutcome::ClientCancelled);
                return;
            }
            Ok(true) => tick.reset(),
            Ok(false) => {}
        }
    }
    // Terminal event (or EOF): tear down upstream now, never wait for its EOF.
    let Reader { body, t, eof, .. } = rd;
    drop(body);
    finalize(&ctx, &mut guard, t, eof, out, rid, usage_tx).await;
}

async fn finalize(
    ctx: &StreamCtx,
    guard: &mut CodexStreamGuard,
    t: CodexToAnthropic,
    eof: bool,
    mut out: Out,
    rid: Option<String>,
    usage_tx: oneshot::Sender<TokenUsage>,
) {
    let now = OffsetDateTime::now_utc();
    let e = match t.finish() {
        Ok(f) => {
            if out.send_all(f.frames).await.is_err() {
                guard.set(StreamOutcome::ClientCancelled);
                return;
            }
            ctx.state.record_success(&ctx.account, f.usage, now);
            if let Some(tokens) = f.context_tokens {
                ctx.carry
                    .record(ctx.session.as_deref(), &ctx.route_id, tokens);
            }
            tracing::info!(
                account = %ctx.account,
                request_id = %ctx.request_id,
                upstream_request_id = rid.as_deref().unwrap_or("-"),
                input_tokens = f.usage.input_tokens,
                output_tokens = f.usage.output_tokens,
                "codex stream served"
            );
            let _ = usage_tx.send(f.usage);
            guard.set(StreamOutcome::Completed);
            return;
        }
        Err(e) => e,
    };
    // Committed: the error frame is the client's answer (the only error frame we emit).
    if out.send_all(vec![e.to_frame()]).await.is_err() {
        guard.set(StreamOutcome::ClientCancelled);
        return;
    }
    drop(out);
    let kind = e.anthropic_type();
    let (reason, outcome) = match &e {
        StreamError::MissingTerminal | StreamError::NoMessageStart if eof => (
            FailReason::Transport(TransportKind::Reset),
            StreamOutcome::UpstreamTruncated,
        ),
        StreamError::Upstream { .. } if kind == "rate_limit_error" => {
            let until = now + ctx.cooldown;
            ctx.state.mark_rate_limited(&ctx.account, until, None, now);
            (
                FailReason::Quota { reset: Some(until) },
                StreamOutcome::Error,
            )
        }
        StreamError::Upstream { .. } if kind == "overloaded_error" => {
            (FailReason::Overloaded, StreamOutcome::Error)
        }
        _ => (FailReason::Status(502), StreamOutcome::Error),
    };
    tracing::warn!(
        account = %ctx.account,
        request_id = %ctx.request_id,
        error_type = kind,
        reason = %reason.detail(),
        upstream_request_id = rid.as_deref().unwrap_or("-"),
        "codex stream ended with an error"
    );
    record(&ctx.state, &ctx.account, reason, rid);
    guard.set(outcome);
}

/// Transport fault after commit: record (unless the client is gone) and abort the body so the
/// client sees a failed response, not a clean end.
async fn fault(
    ctx: &StreamCtx,
    guard: &mut CodexStreamGuard,
    rd: Reader,
    out: Out,
    k: TransportKind,
    rid: Option<String>,
) {
    drop(rd);
    // Never record anything once the client is gone.
    if out.tx.is_closed() {
        guard.set(StreamOutcome::ClientCancelled);
        return;
    }
    let outcome = match k {
        TransportKind::IdleTimeout => StreamOutcome::IdleTimeout,
        TransportKind::Reset => StreamOutcome::Reset,
        _ => StreamOutcome::Error,
    };
    tracing::warn!(
        account = %ctx.account,
        request_id = %ctx.request_id,
        transport = k.as_str(),
        outcome = outcome.as_str(),
        upstream_request_id = rid.as_deref().unwrap_or("-"),
        "codex stream failed after commit"
    );
    record(&ctx.state, &ctx.account, FailReason::Transport(k), rid);
    guard.set(outcome);
    let _ = out
        .tx
        .send(Err(std::io::Error::other(format!(
            "upstream stream failed ({})",
            outcome.as_str()
        ))))
        .await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ping_frame_matches_the_translator_ping() {
        // The translator's ping for a dropped event must be recognised for coalescing.
        let mut t = CodexToAnthropic::new("gpt", None);
        let created = crate::sse::SseEvent {
            event: None,
            data: r#"{"type":"response.created","response":{"id":"r"}}"#.into(),
            raw: Bytes::new(),
        };
        t.on_event(&created);
        let dropped = crate::sse::SseEvent {
            event: None,
            data: r#"{"type":"response.in_progress"}"#.into(),
            raw: Bytes::new(),
        };
        assert_eq!(t.on_event(&dropped), vec![ping_frame()]);
    }

    #[tokio::test]
    async fn consecutive_pings_collapse() {
        let (tx, mut rx) = mpsc::channel(8);
        let mut out = Out {
            tx,
            last_was_ping: false,
        };
        let text = encode("content_block_delta", "{}");
        let sent = out
            .send_all(vec![ping_frame(), ping_frame(), text.clone(), ping_frame()])
            .await
            .unwrap();
        assert!(sent);
        // Another translator ping right after a ping is dropped.
        assert!(!out.send_all(vec![ping_frame()]).await.unwrap());
        drop(out);
        let mut got = Vec::new();
        while let Some(Ok(b)) = rx.recv().await {
            got.push(b);
        }
        assert_eq!(got, vec![ping_frame(), text, ping_frame()]);
    }
}
