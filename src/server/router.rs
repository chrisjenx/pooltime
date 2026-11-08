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
use http::header::{ALLOW, CONTENT_LENGTH};
use http::{HeaderMap, HeaderValue};
use http_body::Frame;
use serde_json::Value;

use super::{auth, health, AppState, PendingGuard};
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
    let messages = post(messages)
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
/// accounting, then the Host/Origin guard.
pub fn finish(app: Router, state: Arc<AppState>) -> Router {
    app.layer(from_fn_with_state(state.clone(), auth::host_origin_guard))
        .layer(from_fn_with_state(state, track_pending))
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

/// `POST /v1/messages`. Task 22 replaces the body of this handler; the ingress contract it
/// already honours is route -> proxy auth (layer) -> JSON parse (400) -> handler.
async fn messages(State(_app): State<Arc<AppState>>, req: Request) -> Response {
    let (parts, body) = req.into_parts();
    match read_json(&parts.headers, body).await {
        Ok(_) => anthropic_error(
            501,
            "api_error",
            "pooltime: /v1/messages is not implemented yet",
        ),
        Err(resp) => resp,
    }
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
    let too_large = || {
        anthropic_error(
            413,
            "request_too_large",
            "pooltime: request body exceeds the 64 MiB limit",
        )
    };
    let declared = headers
        .get(CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok());
    if declared.is_some_and(|n| n > BODY_LIMIT as u64) {
        return Err(too_large());
    }
    let bytes = match axum::body::to_bytes(body, BODY_LIMIT).await {
        Ok(b) => b,
        Err(e) => {
            // Over-limit (chunked) vs. the client going away mid-body.
            return Err(if e.to_string().contains("length limit") {
                too_large()
            } else {
                anthropic_error(
                    400,
                    "invalid_request_error",
                    "pooltime: could not read request body",
                )
            });
        }
    };
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
