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
    // Read per request: `connect` may have just generated the key (no restart needed).
    if let Some(key) = app.proxy_key() {
        if !presented_keys(req.headers()).any(|k| keys_match(k, &key)) {
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
    let api_key = h.get("x-api-key").and_then(|v| v.to_str().ok());
    bearer_of(h).into_iter().chain(api_key)
}

/// The token of `Authorization: Bearer <token>` (scheme matched case-insensitively). Shared by
/// the proxy key, management and MCP checks.
pub(crate) fn bearer_of(h: &HeaderMap) -> Option<&str> {
    h.get(AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| {
            let (scheme, rest) = v.split_once(' ')?;
            scheme.eq_ignore_ascii_case("bearer").then_some(rest)
        })
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

/// Inference paths (`/v1/*`): what Claude Code calls. Claude Code sends no `Origin` and a
/// loopback `Host`; a browser page always sends `Origin`, and a DNS-rebound page its own name
/// as `Host`.
fn is_inference(path: &str) -> bool {
    path == "/v1" || path.starts_with("/v1/")
}

/// The inference rule: no `Origin` at all, and (on a loopback bind) a loopback `Host` when one
/// is sent, any port (rebinding needs a foreign name, not a foreign port). A non-loopback bind
/// serves other machines, so their `Host` is accepted; it always has a proxy key
/// ([`crate::config::Config::validate`]).
fn inference_allowed(headers: &HeaderMap, host: Option<&str>, app: &AppState) -> (bool, bool) {
    let origin_ok = !headers.contains_key(ORIGIN);
    let host_ok = !app.cfg.host.is_loopback() || host.is_none_or(loopback_host);
    (host_ok, origin_ok)
}

/// `localhost`, a loopback IPv4 address or `[::1]`, with or without a port.
fn loopback_host(host: &str) -> bool {
    let name = match host.strip_prefix('[') {
        Some(rest) => match rest.split_once(']') {
            Some((v6, tail)) if tail.is_empty() || tail.starts_with(':') => v6,
            _ => return false,
        },
        None => host.split_once(':').map_or(host, |(n, _)| n),
    };
    name.eq_ignore_ascii_case("localhost")
        || name
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
}

/// Reject requests to `/`, `/assets/*` and `/api/*` whose `Host` is not `127.0.0.1:<port>` / `localhost:<port>`
/// or whose `Origin` (when sent) is not the matching `http://` origin. There is deliberately no
/// CORS support: browsers never get a grant, so only same-origin pages can read responses.
pub async fn host_origin_guard(
    State(app): State<Arc<AppState>>,
    req: Request,
    next: Next,
) -> Response {
    let path = req.uri().path();
    let (guarded, inference) = (is_guarded(path), is_inference(path));
    if guarded || inference {
        let port = app.cfg.port;
        let host = match req.headers().get(HOST) {
            Some(v) => Some(v.to_str().unwrap_or("").to_owned()),
            None => req.uri().authority().map(|a| a.as_str().to_owned()),
        };
        let (host_ok, origin_ok) = if inference {
            inference_allowed(req.headers(), host.as_deref(), &app)
        } else {
            let host_ok = host.as_deref().is_some_and(|h| host_allowed(h, port));
            let origin_ok = match req.headers().get(ORIGIN) {
                None => true,
                Some(o) => o.to_str().is_ok_and(|o| origin_allowed(o, port)),
            };
            (host_ok, origin_ok)
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
        assert!(loopback_host("127.0.0.1:3460"));
        assert!(loopback_host("localhost"));
        assert!(loopback_host("LOCALHOST:1"));
        assert!(loopback_host("[::1]:3460"));
        assert!(loopback_host("127.0.0.2:3460"));
        assert!(!loopback_host("evil.com:3460"));
        assert!(!loopback_host("127.0.0.1.evil.example:3460"));
        assert!(!loopback_host("localhost.evil.example"));
        assert!(!loopback_host("[::1]x"));
        assert!(!loopback_host(""));
        assert!(is_inference("/v1/messages"));
        assert!(is_inference("/v1/models"));
        assert!(!is_inference("/v1x"));
        assert!(!is_inference("/health"));
    }
}
