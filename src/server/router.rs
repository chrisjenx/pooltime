//! Route table and middleware.
//!
//! [`routes`] is the bare route table (state applied, no layers); [`finish`] adds the layers
//! every route shares; [`router`] is both. Splitting them lets tests mount extra routes before
//! the layers go on, so those routes get the same pending-request accounting as real ones.

use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use axum::body::{Body, Bytes, HttpBody};
use axum::extract::{Request, State};
use axum::http::{Method, Uri};
use axum::middleware::{from_fn_with_state, Next};
use axum::response::Response;
use axum::routing::{get, post};
use axum::Router;
use futures_util::StreamExt;
use http::header::{ALLOW, CONTENT_LENGTH};
use http::{HeaderMap, HeaderValue};
use http_body::Frame;
use serde_json::Value;

use super::inference::{self, RequestId, REQUEST_ID_HEADER};
use super::{auth, health, AppState, PendingGuard};
use crate::logging::new_request_id;
use crate::providers::anthropic::anthropic_error;

/// Largest accepted request body (Claude Code sends whole conversations).
pub const BODY_LIMIT: usize = 64 * 1024 * 1024;

/// The standard router: routes plus layers.
pub fn router(state: Arc<AppState>) -> Router {
    finish(routes(state.clone()), state)
}

/// All routes, with `state` applied but no middleware.
pub fn routes(state: Arc<AppState>) -> Router {
    // Auth is a route layer on the handler only, so a wrong method answers 405 before any
    // credential check (bro parity) and the key is never consulted for health.
    let messages = post(inference::messages)
        .route_layer(from_fn_with_state(state.clone(), auth::require_proxy_key))
        .fallback(|| async { method_not_allowed("POST") });
    Router::new()
        .route(
            "/health",
            get(health::health).fallback(|| async { method_not_allowed("GET") }),
        )
        .route("/v1/messages", messages)
        .route(
            "/v1/models",
            get(models).fallback(|| async { method_not_allowed("GET") }),
        )
        .fallback(not_found)
        .with_state(state)
}

/// Layers shared by every route (and the 404 fallback). Outermost first: pending-request
/// accounting, the request id, then the Host/Origin guard.
pub fn finish(app: Router, state: Arc<AppState>) -> Router {
    app.layer(from_fn_with_state(state.clone(), auth::host_origin_guard))
        .layer(axum::middleware::from_fn(assign_request_id))
        .layer(from_fn_with_state(state, track_pending))
}

/// Give every request an id (request extension [`RequestId`]) and return it as the
/// `x-pooltime-request-id` header on every response, errors and 404s included.
async fn assign_request_id(mut req: Request, next: Next) -> Response {
    let id = new_request_id();
    req.extensions_mut().insert(RequestId(id.clone()));
    let mut resp = next.run(req).await;
    if let Ok(v) = HeaderValue::from_str(&id) {
        resp.headers_mut().insert(REQUEST_ID_HEADER, v);
    }
    resp
}

// ---- handlers ----

/// Unknown endpoint: Anthropic-shaped 404. Logs method and path only, never the query string
/// (it can carry `code=`/`state=`/keys).
async fn not_found(method: Method, uri: Uri) -> Response {
    tracing::info!(method = %method, path = uri.path(), "unknown endpoint");
    anthropic_error(
        404,
        "not_found_error",
        &format!("pooltime: unknown endpoint {method} {}", uri.path()),
    )
}

fn method_not_allowed(allow: &'static str) -> Response {
    let mut r = anthropic_error(405, "invalid_request_error", "pooltime: method not allowed");
    r.headers_mut()
        .insert(ALLOW, HeaderValue::from_static(allow));
    r
}

/// `GET /v1/models`. Task 23 replaces this.
async fn models(State(_app): State<Arc<AppState>>) -> Response {
    anthropic_error(
        501,
        "api_error",
        "pooltime: /v1/models is not implemented yet",
    )
}

/// Read and parse a JSON request body: 413 past [`BODY_LIMIT`] (a declared oversize length is
/// refused without reading), 400 `invalid_request_error` on malformed JSON. Returns the exact
/// bytes (forwarded unchanged upstream) and the parsed value.
///
/// hyper has no ingress idle timer here, so an accepted inference request is never cut off while
/// it waits on an upstream; this is the point where bro disabled its timer.
// The error is the response to send as-is; boxing it would only add noise for callers.
#[allow(clippy::result_large_err)]
pub async fn read_json(headers: &HeaderMap, body: Body) -> Result<(Bytes, Value), Response> {
    read_json_limited(headers, body, BODY_LIMIT).await
}

/// [`read_json`] with an explicit byte limit (tests lower it). The limit is enforced here, while
/// accumulating frames, so overflow is a plain comparison rather than an error to be recognised.
#[allow(clippy::result_large_err)]
pub async fn read_json_limited(
    headers: &HeaderMap,
    body: Body,
    limit: usize,
) -> Result<(Bytes, Value), Response> {
    let too_large = || {
        anthropic_error(
            413,
            "request_too_large",
            "pooltime: request body exceeds the size limit",
        )
    };
    let declared = headers
        .get(CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok());
    if declared.is_some_and(|n| n > limit as u64) {
        return Err(too_large());
    }
    let mut buf = Vec::new();
    let mut stream = body.into_data_stream();
    while let Some(chunk) = stream.next().await {
        // An error here is the client going away (or a malformed chunked body) mid-request.
        let chunk = chunk.map_err(|_| {
            anthropic_error(
                400,
                "invalid_request_error",
                "pooltime: could not read request body",
            )
        })?;
        if buf.len() + chunk.len() > limit {
            return Err(too_large());
        }
        buf.extend_from_slice(&chunk);
    }
    let bytes = Bytes::from(buf);
    match serde_json::from_slice::<Value>(&bytes) {
        Ok(v) => Ok((bytes, v)),
        Err(_) => Err(anthropic_error(
            400,
            "invalid_request_error",
            "Invalid JSON body",
        )),
    }
}

// ---- pending-request accounting ----

/// Count every request except `/health` as in flight until its response body is finished or
/// dropped. The guard rides on the response body, so a long SSE stream keeps counting after the
/// handler returned; dropping the handler future (client gone) drops the guard too.
async fn track_pending(State(app): State<Arc<AppState>>, req: Request, next: Next) -> Response {
    if req.uri().path() == "/health" {
        return next.run(req).await;
    }
    let guard = PendingGuard::new(app);
    let resp = next.run(req).await;
    let (parts, body) = resp.into_parts();
    Response::from_parts(
        parts,
        Body::new(Guarded {
            inner: body,
            _guard: guard,
        }),
    )
}

/// A response body that owns a [`PendingGuard`]; framing, size hints and trailers pass through.
struct Guarded {
    inner: Body,
    _guard: PendingGuard,
}

impl HttpBody for Guarded {
    type Data = Bytes;
    type Error = axum::Error;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, axum::Error>>> {
        Pin::new(&mut self.inner).poll_frame(cx)
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn size_hint(&self) -> http_body::SizeHint {
        self.inner.size_hint()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::convert::Infallible;

    /// A body with no known length (as a chunked request arrives), one frame per chunk.
    fn chunked(chunks: &[&'static str]) -> Body {
        let items: Vec<Result<Bytes, Infallible>> = chunks
            .iter()
            .map(|c| Ok(Bytes::from_static(c.as_bytes())))
            .collect();
        Body::from_stream(futures_util::stream::iter(items))
    }

    async fn status_of(r: Result<(Bytes, Value), Response>) -> u16 {
        match r {
            Ok(_) => 200,
            Err(resp) => resp.status().as_u16(),
        }
    }

    #[tokio::test]
    async fn chunked_body_over_limit_is_413_without_content_length() {
        let h = HeaderMap::new();
        // 14 bytes of valid JSON in three chunks against a limit of 10.
        let over = chunked(&[r#"{"a":"#, r#""bbbb""#, "}"]);
        assert_eq!(status_of(read_json_limited(&h, over, 10).await).await, 413);
        // Exactly at the limit is accepted (and the raw bytes come back unchanged).
        let (bytes, v) = read_json_limited(&h, chunked(&[r#"{"a":"#, "1}"]), 7)
            .await
            .unwrap();
        assert_eq!(&bytes[..], br#"{"a":1}"#);
        assert_eq!(v["a"], 1);
        // One byte over, split so no single chunk exceeds the limit.
        let r = read_json_limited(&h, chunked(&[r#"{"a":"#, "12}"]), 7).await;
        assert_eq!(status_of(r).await, 413);
    }

    #[tokio::test]
    async fn declared_length_over_limit_is_413_before_reading() {
        let mut h = HeaderMap::new();
        h.insert(CONTENT_LENGTH, HeaderValue::from_static("11"));
        // The body would block forever if it were read.
        let never = Body::from_stream(futures_util::stream::pending::<Result<Bytes, Infallible>>());
        assert_eq!(status_of(read_json_limited(&h, never, 10).await).await, 413);
    }

    #[tokio::test]
    async fn malformed_json_and_broken_body_are_400() {
        let h = HeaderMap::new();
        assert_eq!(
            status_of(read_json_limited(&h, chunked(&["{nope"]), 100).await).await,
            400
        );
        let broken = Body::from_stream(futures_util::stream::iter(vec![
            Ok(Bytes::from_static(b"{")),
            Err(std::io::Error::other("client went away")),
        ]));
        assert_eq!(
            status_of(read_json_limited(&h, broken, 100).await).await,
            400
        );
    }
}
