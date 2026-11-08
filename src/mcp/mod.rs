//! The MCP server (spec §7): pool management for Claude Code itself, at `POST /mcp`.
//!
//! # Transport
//! A minimal, stateless implementation of MCP's Streamable HTTP transport: every JSON-RPC 2.0
//! message is one `POST /mcp`, answered with a single `application/json` response (the
//! transport allows that instead of an SSE stream). Requests get `200` with the JSON-RPC
//! response; notifications and client responses (no `id`) get `202` with no body. `GET` (the
//! optional server-to-client stream) and `DELETE` (session end) answer `405`: there are no
//! sessions and nothing to push. Methods: `initialize`, `ping`, `tools/list`, `tools/call`.
//! Hand-rolled rather than `rmcp`: six tools do not justify its dependency tree, per-session
//! tasks and schema derive machinery in a daemon that must stay small at idle.
//!
//! # Auth
//! `Authorization: Bearer <MCP token>`. The token is its own 32 random bytes (hex) at
//! `run/mcp.key` (0600, [`ensure_mcp_key`]), never the management key: `/mcp` accepts only it,
//! and `/api/v1/*` never accepts it. Its scope is the tool set itself (read, start re-login, set
//! weight); there is no tool that removes accounts, reads tokens or changes Claude Code
//! connections. `/mcp` is also behind the Host/Origin guard (DNS rebinding).
//!
//! # One management layer
//! Every tool calls the same `/api/v1` handler the dashboard and the CLI use, in process (see
//! [`tools`]), so the three front-ends cannot disagree.

mod tools;

use std::path::PathBuf;
use std::sync::Arc;

use axum::body::Body;
use axum::extract::{Request, State};
use axum::middleware::{from_fn_with_state, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use http::header::AUTHORIZATION;
use http::{HeaderMap, StatusCode};
use serde_json::{json, Value};

use crate::config::Paths;
use crate::providers::anthropic::anthropic_error;
use crate::server::auth::keys_match;
use crate::server::mgmt::ensure_key_file;
use crate::server::router::{method_not_allowed, read_json_limited};
use crate::server::AppState;

/// Protocol revisions this server speaks; the first is offered when the client asks for another.
pub const PROTOCOL_VERSIONS: &[&str] = &["2025-06-18", "2025-11-25", "2025-03-26", "2024-11-05"];

/// MCP messages are small; anything larger is refused.
const BODY_LIMIT: usize = 1024 * 1024;

// JSON-RPC error codes.
const PARSE_ERROR: i64 = -32700;
const INVALID_REQUEST: i64 = -32600;
const METHOD_NOT_FOUND: i64 = -32601;
pub(crate) const INVALID_PARAMS: i64 = -32602;

/// `run/mcp.key`.
pub fn mcp_key_path(paths: &Paths) -> PathBuf {
    paths.run.join("mcp.key")
}

/// The MCP token, created like the management key (32 random bytes, hex, 0600) when absent or
/// malformed. Call at serve start.
pub fn ensure_mcp_key(paths: &Paths) -> anyhow::Result<String> {
    ensure_key_file(paths, &mcp_key_path(paths), "run/mcp.key")
}

/// `/mcp`: `POST` behind [`require_mcp`]; any other verb is `405` before auth.
pub fn routes(state: Arc<AppState>) -> Router<Arc<AppState>> {
    Router::new().route(
        "/mcp",
        post(handle)
            .route_layer(from_fn_with_state(state, require_mcp))
            .fallback(|| async { method_not_allowed("POST") }),
    )
}

/// Whether `h` carries `Bearer <key>` (constant-time compare; an empty key matches nothing).
fn bearer_matches(h: &HeaderMap, key: &str) -> bool {
    !key.is_empty()
        && h.get(AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| {
                let (scheme, rest) = v.split_once(' ')?;
                scheme.eq_ignore_ascii_case("bearer").then_some(rest)
            })
            .is_some_and(|k| keys_match(k, key))
}

async fn require_mcp(State(app): State<Arc<AppState>>, req: Request, next: Next) -> Response {
    if bearer_matches(req.headers(), &app.mcp_key) {
        next.run(req).await
    } else {
        anthropic_error(
            401,
            "authentication_error",
            "pooltime: MCP token required (Authorization: Bearer <token from run/mcp.key>)",
        )
    }
}

fn reply(status: StatusCode, v: Value) -> Response {
    (status, Json(v)).into_response()
}

fn rpc_error(id: Value, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

async fn handle(State(app): State<Arc<AppState>>, headers: HeaderMap, body: Body) -> Response {
    let msg = match read_json_limited(&headers, body, BODY_LIMIT).await {
        Ok((_, v)) => v,
        Err(r) if r.status() == StatusCode::PAYLOAD_TOO_LARGE => return r,
        Err(_) => {
            return reply(
                StatusCode::BAD_REQUEST,
                rpc_error(Value::Null, PARSE_ERROR, "Parse error"),
            )
        }
    };
    let Value::Object(msg) = msg else {
        return reply(
            StatusCode::BAD_REQUEST,
            rpc_error(
                Value::Null,
                INVALID_REQUEST,
                "expected one JSON-RPC message",
            ),
        );
    };
    // A notification (or a response to a server request, which we never send): accepted.
    let (Some(id), Some(method)) = (msg.get("id"), msg.get("method")) else {
        return StatusCode::ACCEPTED.into_response();
    };
    let id = id.clone();
    let Some(method) = method.as_str() else {
        return reply(
            StatusCode::OK,
            rpc_error(id, INVALID_REQUEST, "method must be a string"),
        );
    };
    let params = msg.get("params").cloned().unwrap_or(Value::Null);
    let result = match method {
        "initialize" => Ok(initialize(&params)),
        "ping" => Ok(json!({})),
        "tools/list" => Ok(tools::list()),
        "tools/call" => tools::call(&app, &params).await,
        other => Err((METHOD_NOT_FOUND, format!("Method not found: {other}"))),
    };
    let v = match result {
        Ok(r) => json!({ "jsonrpc": "2.0", "id": id, "result": r }),
        Err((code, message)) => rpc_error(id, code, &message),
    };
    reply(StatusCode::OK, v)
}

fn initialize(params: &Value) -> Value {
    let asked = params["protocolVersion"].as_str();
    let version = asked
        .filter(|v| PROTOCOL_VERSIONS.contains(v))
        .unwrap_or(PROTOCOL_VERSIONS[0]);
    json!({
        "protocolVersion": version,
        "capabilities": { "tools": { "listChanged": false } },
        "serverInfo": { "name": "pooltime", "version": env!("CARGO_PKG_VERSION") },
        "instructions": "pooltime pools Claude/ChatGPT subscriptions behind this Claude Code. \
            Use pool_health for account states and login expiry, pool_usage for 7-day headroom \
            and weekly burn, recent_errors/explain_request to see why a request failed, \
            start_relogin for a sign-in link, set_weight to steer routing (optionally until a \
            time, after which it reverts by itself).",
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bearer_rules() {
        let mut h = HeaderMap::new();
        assert!(!bearer_matches(&h, "k"));
        h.insert(AUTHORIZATION, "Bearer k".parse().unwrap());
        assert!(bearer_matches(&h, "k"));
        assert!(!bearer_matches(&h, ""), "an unset token authorizes nothing");
        h.insert(AUTHORIZATION, "bearer k".parse().unwrap());
        assert!(bearer_matches(&h, "k"));
        h.insert(AUTHORIZATION, "k".parse().unwrap());
        assert!(!bearer_matches(&h, "k"));
    }

    #[test]
    fn negotiates_protocol_version() {
        assert_eq!(
            initialize(&json!({"protocolVersion": "2025-03-26"}))["protocolVersion"],
            "2025-03-26"
        );
        assert_eq!(
            initialize(&json!({"protocolVersion": "1999-01-01"}))["protocolVersion"],
            PROTOCOL_VERSIONS[0]
        );
        assert_eq!(
            initialize(&Value::Null)["protocolVersion"],
            PROTOCOL_VERSIONS[0]
        );
    }

    #[test]
    fn mcp_key_is_its_own_file() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::under(dir.path());
        let k = ensure_mcp_key(&paths).unwrap();
        assert_eq!(k.len(), 64);
        assert_eq!(ensure_mcp_key(&paths).unwrap(), k, "stable once created");
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(mcp_key_path(&paths))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
        let mgmt = crate::server::mgmt::ensure_mgmt_key(&paths).unwrap();
        assert_ne!(mgmt, k);
    }
}
