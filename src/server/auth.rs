//! Request guards: the optional proxy key on inference routes and the Host/Origin check that
//! stops DNS-rebinding on the dashboard and management API.

use std::sync::Arc;

use axum::extract::{Request, State};
use axum::middleware::Next;
use axum::response::Response;
use http::header::{AUTHORIZATION, HOST, ORIGIN};
use http::HeaderMap;
use sha2::{Digest, Sha256};

use super::AppState;
use crate::providers::anthropic::anthropic_error;

/// Inference auth (INV §1): when `proxy_key` is configured, `Authorization: Bearer <key>` or
/// `x-api-key: <key>` must match, else 401 `authentication_error`. No key configured = open.
pub async fn require_proxy_key(
    State(app): State<Arc<AppState>>,
    req: Request,
    next: Next,
) -> Response {
    // An empty key (`proxy_key = ""`, `POOLTIME_PROXY_KEY=`) means "not set", never "match empty".
    if let Some(key) = app.cfg.proxy_key.as_deref().filter(|k| !k.is_empty()) {
        if !presented_keys(req.headers()).any(|k| keys_match(k, key)) {
            return anthropic_error(
                401,
                "authentication_error",
                "Unauthorized: invalid or missing API key",
            );
        }
    }
    next.run(req).await
}

/// The credentials a client sent: the bearer token (scheme matched case-insensitively) and
/// `x-api-key`.
fn presented_keys(h: &HeaderMap) -> impl Iterator<Item = &str> {
    let bearer = h
        .get(AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| {
            let (scheme, rest) = v.split_once(' ')?;
            scheme.eq_ignore_ascii_case("bearer").then_some(rest)
        });
    let api_key = h.get("x-api-key").and_then(|v| v.to_str().ok());
    bearer.into_iter().chain(api_key)
}

/// Comparison whose timing does not depend on how much of the key matched (digests of equal
/// length, folded without early exit).
pub(crate) fn keys_match(presented: &str, expected: &str) -> bool {
    let a = Sha256::digest(presented.as_bytes());
    let b = Sha256::digest(expected.as_bytes());
    a.iter()
        .zip(b.iter())
        .fold(0u8, |acc, (x, y)| acc | (x ^ y))
        == 0
}

/// Paths the Host/Origin guard covers: the dashboard (`/`, `/assets/*`), the management API
/// (`/api/*`) and the MCP endpoint (`/mcp`: the DNS-rebinding protection MCP's HTTP transport
/// asks for).
fn is_guarded(path: &str) -> bool {
    path == "/"
        || path == "/api"
        || path.starts_with("/api/")
        || path.starts_with("/assets/")
        || path == "/mcp"
}

/// Reject requests to `/`, `/assets/*` and `/api/*` whose `Host` is not `127.0.0.1:<port>` / `localhost:<port>`
/// or whose `Origin` (when sent) is not the matching `http://` origin. There is deliberately no
/// CORS support: browsers never get a grant, so only same-origin pages can read responses.
pub async fn host_origin_guard(
    State(app): State<Arc<AppState>>,
    req: Request,
    next: Next,
) -> Response {
    if is_guarded(req.uri().path()) {
        let port = app.cfg.port;
        let host = req
            .headers()
            .get(HOST)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned)
            .or_else(|| req.uri().authority().map(|a| a.as_str().to_owned()));
        let host_ok = host.as_deref().is_some_and(|h| host_allowed(h, port));
        let origin_ok = match req.headers().get(ORIGIN) {
            None => true,
            Some(o) => o.to_str().is_ok_and(|o| origin_allowed(o, port)),
        };
        if !(host_ok && origin_ok) {
            tracing::info!(
                method = %req.method(),
                path = req.uri().path(),
                host_ok,
                origin_ok,
                "rejected request with foreign Host/Origin"
            );
            return anthropic_error(
                403,
                "permission_error",
                "pooltime: Host or Origin not allowed for this endpoint",
            );
        }
    }
    next.run(req).await
}

fn host_allowed(host: &str, port: u16) -> bool {
    let host = host.to_ascii_lowercase();
    host == format!("127.0.0.1:{port}") || host == format!("localhost:{port}")
}

fn origin_allowed(origin: &str, port: u16) -> bool {
    origin
        .strip_prefix("http://")
        .is_some_and(|h| host_allowed(h, port))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_comparison() {
        assert!(keys_match("abc", "abc"));
        assert!(!keys_match("abc", "abd"));
        assert!(!keys_match("", "abc"));
        assert!(!keys_match("abcd", "abc"));
    }

    #[test]
    fn host_and_origin_rules() {
        assert!(host_allowed("127.0.0.1:3460", 3460));
        assert!(host_allowed("LocalHost:3460", 3460));
        assert!(!host_allowed("127.0.0.1", 3460));
        assert!(!host_allowed("127.0.0.1:3461", 3460));
        assert!(!host_allowed("127.0.0.1:3460.evil.example", 3460));
        assert!(!host_allowed("[::1]:3460", 3460));
        assert!(origin_allowed("http://localhost:3460", 3460));
        assert!(!origin_allowed("https://localhost:3460", 3460));
        assert!(!origin_allowed("null", 3460));
        assert!(is_guarded("/"));
        assert!(is_guarded("/api/status"));
        assert!(!is_guarded("/apiary"));
        assert!(is_guarded("/assets/app.js"));
        assert!(!is_guarded("/health"));
        assert!(!is_guarded("/v1/messages"));
        assert!(is_guarded("/mcp"));
    }
}
