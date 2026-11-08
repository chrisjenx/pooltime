//! `GET /health` and the startup preflight that uses it to recognise a running pooltime (or bro).

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use axum::extract::State;
use axum::response::Response;
use serde_json::{json, Value};
use time::OffsetDateTime;

use super::AppState;
use crate::accounts::{AccountStatus, Registry};
use crate::config::Config;

/// Where bro's pool listens by default.
pub const BRO_PORT: u16 = 3456;

/// `{"service":"pooltime","status":"ok","version","accounts","available","pending"}`, or
/// 503 `{"service":"pooltime","status":"draining"}` once shutdown has begun.
pub async fn health(State(app): State<Arc<AppState>>) -> Response {
    if app.runtime.draining() {
        return draining_response();
    }
    let pending = app.runtime.pending();
    let reg = app.registry.clone();
    let book = app.state.clone();
    // Reads account.toml/credentials.json for every account: keep it off the async workers.
    let counts = tokio::task::spawn_blocking(move || {
        let now = OffsetDateTime::now_utc();
        let metas = reg.list();
        let available = metas
            .iter()
            .filter(|m| {
                let creds_ok = matches!(reg.read_creds(&m.id), Ok(Some(_)));
                matches!(book.status(&m.id, creds_ok, now), AccountStatus::Ready)
            })
            .count();
        (metas.len(), available)
    })
    .await;
    let (accounts, available) = counts.unwrap_or_else(|e| {
        tracing::warn!(error = %e, "health: account count task failed");
        (0, 0)
    });
    json_response(
        200,
        &json!({
            "service": "pooltime",
            "status": "ok",
            "version": env!("CARGO_PKG_VERSION"),
            "accounts": accounts,
            "available": available,
            "pending": pending,
        }),
    )
}

/// The 503 health body used while draining (also served on new connections during the drain).
pub fn draining_response() -> Response {
    json_response(503, &json!({"service": "pooltime", "status": "draining"}))
}

fn json_response(status: u16, body: &Value) -> Response {
    let mut r = Response::new(axum::body::Body::from(body.to_string()));
    *r.status_mut() = http::StatusCode::from_u16(status).unwrap_or(http::StatusCode::OK);
    r.headers_mut().insert(
        http::header::CONTENT_TYPE,
        http::HeaderValue::from_static("application/json"),
    );
    r
}

// ---- startup preflight ----

/// Why `serve` must not start.
#[derive(Debug, thiserror::Error)]
pub enum StartupError {
    #[error("pooltime is already running on {addr}")]
    AlreadyRunning { addr: SocketAddr },
    #[error(
        "a bro pool is answering /health on port {port} and imported accounts are enabled: \
         refusing to serve the same accounts from two pools (token rotation would break one of \
         them). Stop bro, or pass --i-stopped-bro if you already have"
    )]
    BroRunning { port: u16 },
}

/// What answered `/health` on a port.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Probe {
    /// Nothing is listening.
    Free,
    /// A pooltime daemon (healthy or draining).
    Pooltime,
    /// bro's pool: `status == "ok"` and a numeric `accounts`, without pooltime's `service` field.
    Bro,
    /// Something answered, but it is not a pool (e.g. a dev server returning HTML).
    Other,
    /// Connected but no usable answer in time. Never read as "free": a slow health check must
    /// not look like "not running" (INV §7).
    Unresponsive,
}

/// How long a health probe may take end to end.
const PROBE_TIMEOUT: Duration = Duration::from_secs(3);

/// `GET http://<addr>/health` and classify the answer.
pub async fn probe(addr: SocketAddr) -> Probe {
    probe_within(addr, PROBE_TIMEOUT).await
}

/// [`probe`] with an explicit overall timeout (tests shorten it).
pub async fn probe_within(addr: SocketAddr, timeout: Duration) -> Probe {
    let client = match reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(timeout.min(Duration::from_secs(1)))
        .timeout(timeout)
        .build()
    {
        Ok(c) => c,
        Err(_) => return Probe::Unresponsive,
    };
    let mut resp = match client.get(format!("http://{addr}/health")).send().await {
        Ok(r) => r,
        // Timeouts first: a connect timeout also reports `is_connect()`, but a listener too
        // busy to accept is the opposite of "nothing there".
        Err(e) if e.is_timeout() => return Probe::Unresponsive,
        Err(e) if e.is_connect() => return Probe::Free,
        Err(_) => return Probe::Unresponsive,
    };
    // A health body is tiny; refuse to buffer an arbitrary listener's output.
    let mut body = Vec::new();
    loop {
        match resp.chunk().await {
            Ok(Some(chunk)) => {
                body.extend_from_slice(&chunk);
                if body.len() > 16 * 1024 {
                    return Probe::Other;
                }
            }
            Ok(None) => break,
            // Headers arrived but the body stalled or broke: still something we cannot read.
            Err(_) => return Probe::Unresponsive,
        }
    }
    let Ok(v) = serde_json::from_slice::<Value>(&body) else {
        return Probe::Other;
    };
    if v["service"] == "pooltime" {
        Probe::Pooltime
    } else if v.get("service").is_none() && v["status"] == "ok" && v["accounts"].is_number() {
        Probe::Bro
    } else {
        Probe::Other
    }
}

/// Loopback address to probe for a configured bind address (a wildcard bind answers on loopback).
pub fn probe_addr(host: IpAddr, port: u16) -> SocketAddr {
    let ip = if host.is_unspecified() {
        match host {
            IpAddr::V4(_) => IpAddr::V4(Ipv4Addr::LOCALHOST),
            IpAddr::V6(_) => IpAddr::V6(std::net::Ipv6Addr::LOCALHOST),
        }
    } else {
        host
    };
    SocketAddr::new(ip, port)
}

/// Refuse to start when a pooltime already answers `/health` on the configured port. Anything
/// else on the port is left for `bind` to report as "address in use".
pub async fn preflight(cfg: &Config) -> Result<(), StartupError> {
    let addr = probe_addr(cfg.host, cfg.port);
    match probe(addr).await {
        Probe::Pooltime => Err(StartupError::AlreadyRunning { addr }),
        Probe::Free | Probe::Bro | Probe::Other | Probe::Unresponsive => Ok(()),
    }
}

/// Shadow-run guard (spec §16): refuse to serve imported accounts while a bro pool answers
/// `/health` on `bro_port`, unless the operator confirmed bro is stopped (`--i-stopped-bro`).
pub async fn check_bro_shadow(
    registry: &Registry,
    bro_port: u16,
    i_stopped_bro: bool,
) -> Result<(), StartupError> {
    check_bro_shadow_within(registry, bro_port, i_stopped_bro, PROBE_TIMEOUT).await
}

/// [`check_bro_shadow`] with an explicit probe timeout (tests shorten it).
pub async fn check_bro_shadow_within(
    registry: &Registry,
    bro_port: u16,
    i_stopped_bro: bool,
    timeout: Duration,
) -> Result<(), StartupError> {
    if i_stopped_bro || !registry.list().iter().any(|m| m.imported) {
        return Ok(());
    }
    let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), bro_port);
    match probe_within(addr, timeout).await {
        // An unresponsive listener may be an overloaded bro: err on the side of refusing.
        Probe::Bro | Probe::Unresponsive => Err(StartupError::BroRunning { port: bro_port }),
        Probe::Free | Probe::Pooltime | Probe::Other => Ok(()),
    }
}
