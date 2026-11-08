//! Task 21: the HTTP server's identity, auth, routing fallbacks, Host/Origin guard, drain and
//! startup preflight, over real sockets.

mod support;

use std::time::Duration;

use bytes::Bytes;
use pooltime::server::health::{
    check_bro_shadow, check_bro_shadow_within, preflight, probe_within, Probe, StartupError,
};
use pooltime::server::Outcome;
use serde_json::{json, Value};
use support::sockets::{http_get, http_req, raw_client, serve};
use support::{captured_logs, Boot, Pool};

const NOT_FOUND_BODY: &str = r#"{"type":"error","error":{"type":"not_found_error","message":"pooltime: unknown endpoint GET /definitely/unknown/logged"}}"#;

fn json_headers() -> [(&'static str, &'static str); 1] {
    [("content-type", "application/json")]
}

#[tokio::test]
async fn health_identity_shape() {
    let pool = Pool::new();
    let srv = pool.boot(Boot::default()).await;

    let r = http_get(srv.addr, "/health", &[]).await;
    assert_eq!(r.status, 200);
    assert_eq!(r.header("content-type"), Some("application/json"));
    let v = r.json();
    let obj = v.as_object().unwrap();
    let mut keys: Vec<&str> = obj.keys().map(String::as_str).collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        [
            "accounts",
            "available",
            "pending",
            "service",
            "status",
            "version"
        ]
    );
    assert_eq!(v["service"], "pooltime");
    assert_eq!(v["status"], "ok");
    assert_eq!(v["version"], env!("CARGO_PKG_VERSION"));
    assert_eq!(v["accounts"].as_u64(), Some(1));
    assert_eq!(v["available"].as_u64(), Some(1));
    // The health request itself is not counted.
    assert_eq!(v["pending"].as_u64(), Some(0));

    // An account whose credentials are gone is not available.
    std::fs::remove_file(
        pool.dir
            .path()
            .join("accounts")
            .join(pool.meta.id.to_string())
            .join("credentials.json"),
    )
    .unwrap();
    let v = http_get(srv.addr, "/health", &[]).await.json();
    assert_eq!(v["accounts"].as_u64(), Some(1));
    assert_eq!(v["available"].as_u64(), Some(0));
}

#[tokio::test]
async fn health_pending_counts_inflight_requests() {
    let pool = Pool::new();
    let (tx, extra) = support::stream_route("/test/stream");
    let srv = pool
        .boot(Boot {
            extra: Some(extra),
            ..Boot::default()
        })
        .await;

    let mut stream = raw_client(
        srv.addr,
        b"GET /test/stream HTTP/1.1\r\nhost: 127.0.0.1\r\n\r\n",
    )
    .await;
    tx.send(Bytes::from_static(b"hello")).await.unwrap();
    stream.read_until(b"hello", Duration::from_secs(5)).await;
    assert_eq!(
        http_get(srv.addr, "/health", &[]).await.json()["pending"].as_u64(),
        Some(1)
    );

    // Ending the stream releases the count (the guard rides on the response body).
    drop(tx);
    stream
        .read_until(b"0\r\n\r\n", Duration::from_secs(5))
        .await;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let pending = http_get(srv.addr, "/health", &[]).await.json()["pending"].as_u64();
        if pending == Some(0) {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "pending stuck at {pending:?}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test]
async fn proxy_key_required_when_set() {
    let pool = Pool::new();
    let srv = pool
        .boot(Boot {
            proxy_key: Some("sekret-proxy-key".into()),
            ..Boot::default()
        })
        .await;
    let body = br#"{"model":"claude-sonnet-5","messages":[]}"#;
    let post = |headers: Vec<(&'static str, &'static str)>| async move {
        let mut h = json_headers().to_vec();
        h.extend(headers);
        http_req(srv.addr, "POST", "/v1/messages", &h, body).await
    };

    for headers in [
        vec![],
        vec![("authorization", "Bearer wrong")],
        vec![("x-api-key", "wrong")],
        vec![("authorization", "sekret-proxy-key")],
        vec![("authorization", "Bearer ")],
    ] {
        let r = post(headers).await;
        assert_eq!(r.status, 401);
        assert_eq!(
            r.json(),
            json!({"type":"error","error":{"type":"authentication_error",
                "message":"Unauthorized: invalid or missing API key"}})
        );
        assert_eq!(r.header("content-type"), Some("application/json"));
    }
    // Auth precedes JSON parsing: a bad key never learns whether its body was valid.
    let r = http_req(
        srv.addr,
        "POST",
        "/v1/messages",
        &json_headers(),
        b"not json",
    )
    .await;
    assert_eq!(r.status, 401);

    for headers in [
        vec![("authorization", "Bearer sekret-proxy-key")],
        vec![("x-api-key", "sekret-proxy-key")],
    ] {
        let r = post(headers).await;
        assert_ne!(r.status, 401, "{}", r.text());
    }
    // The key is not required for health.
    assert_eq!(http_get(srv.addr, "/health", &[]).await.status, 200);

    // No key configured: open.
    let open = pool.boot(Boot::default()).await;
    let r = http_req(open.addr, "POST", "/v1/messages", &json_headers(), body).await;
    assert_ne!(r.status, 401);
}

#[tokio::test]
async fn inference_rejects_foreign_origin_and_host() {
    let pool = Pool::new();
    let srv = pool.boot(Boot::default()).await;
    let port = srv.addr.port();
    let body = br#"{"model":"claude-sonnet-5","messages":[]}"#;
    let evil_host = format!("evil.com:{port}");
    let ok_host = format!("127.0.0.1:{port}");
    for path in ["/v1/messages", "/v1/messages/count_tokens", "/v1/models"] {
        let method = if path == "/v1/models" { "GET" } else { "POST" };
        // A browser page (simple cross-origin POST, no preflight) always sends Origin.
        let mut h = json_headers().to_vec();
        h.push(("origin", "http://evil.example"));
        let r = http_req(srv.addr, method, path, &h, body).await;
        assert_eq!(r.status, 403, "origin on {path}");
        assert_eq!(r.json()["error"]["type"], "permission_error");
        assert!(r.header("access-control-allow-origin").is_none());
        // DNS rebinding: the page's own name as Host.
        let mut h = json_headers().to_vec();
        h.push(("host", evil_host.as_str()));
        let r = http_req(srv.addr, method, path, &h, body).await;
        assert_eq!(r.status, 403, "host on {path}");
    }
    // What Claude Code sends (loopback Host, no Origin) passes the guard.
    for host in [ok_host.clone(), format!("localhost:{port}")] {
        let mut h = json_headers().to_vec();
        h.push(("host", host.as_str()));
        let r = http_req(srv.addr, "POST", "/v1/messages", &h, body).await;
        assert_ne!(r.status, 403, "{host}: {}", r.text());
        let r = http_get(srv.addr, "/v1/models", &[("host", host.as_str())]).await;
        assert_eq!(r.status, 200, "{host}");
    }
}

#[tokio::test]
async fn invalid_json_is_400_and_oversize_is_413() {
    let pool = Pool::new();
    let srv = pool.boot(Boot::default()).await;
    let r = http_req(srv.addr, "POST", "/v1/messages", &json_headers(), b"{nope").await;
    assert_eq!(r.status, 400);
    assert_eq!(r.json()["error"]["type"], "invalid_request_error");

    // Declared size over the 64 MiB limit is refused without reading the body.
    let too_big = (64 * 1024 * 1024 + 1).to_string();
    let mut raw = raw_client(
        srv.addr,
        format!(
            "POST /v1/messages HTTP/1.1\r\nhost: {}\r\ncontent-type: application/json\r\n\
             content-length: {too_big}\r\n\r\n",
            srv.addr
        )
        .as_bytes(),
    )
    .await;
    raw.read_until(b"HTTP/1.1 413", Duration::from_secs(5))
        .await;
}

#[tokio::test]
async fn unknown_path_anthropic_404_logged() {
    let pool = Pool::new();
    let srv = pool.boot(Boot::default()).await;

    let r = http_get(
        srv.addr,
        "/definitely/unknown/logged?token=query-secret-value",
        &[],
    )
    .await;
    assert_eq!(r.status, 404);
    assert_eq!(r.header("content-type"), Some("application/json"));
    assert_eq!(r.text(), NOT_FOUND_BODY);

    let logs = captured_logs();
    let line = logs
        .lines()
        .find(|l| l.contains("/definitely/unknown/logged"))
        .expect("unknown path logged");
    let v: Value = serde_json::from_str(line).unwrap();
    assert_eq!(v["level"], "INFO");
    assert_eq!(v["fields"]["method"], "GET");
    assert_eq!(v["fields"]["path"], "/definitely/unknown/logged");
    assert!(!logs.contains("query-secret-value"), "query string logged");

    // Known path, wrong method: 405 with Allow.
    let r = http_get(srv.addr, "/v1/messages", &[]).await;
    assert_eq!(r.status, 405);
    assert_eq!(r.header("allow"), Some("POST"));
    assert_eq!(r.json()["type"], "error");
    let r = http_req(srv.addr, "POST", "/health", &[], b"").await;
    assert_eq!(r.status, 405);
    assert_eq!(r.header("allow"), Some("GET"));
}

#[tokio::test]
async fn foreign_origin_rejected_on_api() {
    let pool = Pool::new();
    let srv = pool.boot(Boot::default()).await;
    let port = srv.addr.port();
    let ok_host = format!("127.0.0.1:{port}");
    let localhost = format!("localhost:{port}");

    let status = |path: &'static str, headers: Vec<(&'static str, String)>| async move {
        let h: Vec<(&str, &str)> = headers.iter().map(|(k, v)| (*k, v.as_str())).collect();
        http_get(srv.addr, path, &h).await
    };

    // Foreign Host (DNS rebinding), wrong port, look-alike names.
    for host in [
        "evil.example".to_string(),
        "evil.example:3460".to_string(),
        "127.0.0.1:1".to_string(),
        format!("127.0.0.1.evil.example:{port}"),
        "127.0.0.1".to_string(),
    ] {
        for path in ["/api/status", "/api/accounts", "/"] {
            let r = status(path, vec![("host", host.clone())]).await;
            assert_eq!(r.status, 403, "{host} {path}");
            assert_eq!(r.json()["error"]["type"], "permission_error");
            assert!(r.header("access-control-allow-origin").is_none());
        }
    }
    // Foreign Origin with a good Host (cross-site fetch from a browser).
    for origin in [
        "http://evil.example",
        "null",
        "https://127.0.0.1",
        "http://127.0.0.1",
    ] {
        let r = status(
            "/api/status",
            vec![("host", ok_host.clone()), ("origin", origin.into())],
        )
        .await;
        assert_eq!(r.status, 403, "origin {origin}");
    }
    // Allowed combinations pass the guard (the route itself is not built yet: 404).
    for (host, origin) in [
        (ok_host.clone(), None),
        (localhost.clone(), None),
        (ok_host.clone(), Some(format!("http://127.0.0.1:{port}"))),
        (localhost.clone(), Some(format!("http://localhost:{port}"))),
    ] {
        let mut h = vec![("host", host.clone())];
        if let Some(o) = &origin {
            h.push(("origin", o.clone()));
        }
        let r = status("/api/status", h).await;
        assert_ne!(r.status, 403, "{host} {origin:?}");
        assert!(r.header("access-control-allow-origin").is_none(), "no CORS");
    }
    // A CORS preflight from a foreign page is refused and never granted.
    let r = http_req(
        srv.addr,
        "OPTIONS",
        "/api/status",
        &[
            ("origin", "http://evil.example"),
            ("access-control-request-method", "POST"),
        ],
        b"",
    )
    .await;
    assert_eq!(r.status, 403);
    assert!(r.header("access-control-allow-origin").is_none());

    // Health and inference are not guarded by Host/Origin.
    let r = status("/health", vec![("host", "evil.example".into())]).await;
    assert_eq!(r.status, 200);
}

#[tokio::test]
async fn drain_finishes_inflight_stream_then_exits() {
    let pool = Pool::new();
    let (tx, extra) = support::stream_route("/test/stream");
    let mut srv = pool
        .boot(Boot {
            extra: Some(extra),
            ..Boot::default()
        })
        .await;
    let addr = srv.addr;

    let mut stream = raw_client(
        addr,
        b"GET /test/stream HTTP/1.1\r\nhost: 127.0.0.1\r\n\r\n",
    )
    .await;
    tx.send(Bytes::from_static(b"first-chunk")).await.unwrap();
    stream
        .read_until(b"first-chunk", Duration::from_secs(5))
        .await;

    srv.signal();

    // While draining, health on a new connection answers 503 draining.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    let r = loop {
        let r = http_get(addr, "/health", &[]).await;
        if r.status == 503 {
            break r;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "never started draining"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    assert_eq!(r.json()["status"], "draining");
    assert_eq!(r.json()["service"], "pooltime");
    // New inference requests are refused with an Anthropic-shaped 503.
    let r = http_req(addr, "POST", "/v1/messages", &json_headers(), b"{}").await;
    assert_eq!(r.status, 503);
    assert_eq!(r.json()["error"]["type"], "overloaded_error");

    // The in-flight stream is still open and the server has not exited.
    assert!(!srv.join.is_finished());
    tx.send(Bytes::from_static(b"last-chunk")).await.unwrap();
    stream
        .read_until(b"last-chunk", Duration::from_secs(5))
        .await;
    assert!(!srv.join.is_finished(), "exited with a stream still open");

    drop(tx);
    let bytes = stream.read_to_end(Duration::from_secs(5)).await;
    assert!(support::sockets::contains(&bytes, b"first-chunk"));
    assert!(support::sockets::contains(&bytes, b"0\r\n\r\n"));
    assert_eq!(srv.finished(Duration::from_secs(5)).await, Outcome::Drained);

    // The port is released.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while tokio::net::TcpStream::connect(addr).await.is_ok() {
        assert!(tokio::time::Instant::now() < deadline, "still accepting");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test]
async fn drain_timeout_and_second_signal_force_exit() {
    let pool = Pool::new();

    // Drain budget exhausted with a stream that never ends.
    let (tx, extra) = support::stream_route("/test/stream");
    let mut srv = pool
        .boot(Boot {
            extra: Some(extra),
            drain_timeout: Duration::from_millis(200),
            ..Boot::default()
        })
        .await;
    let mut stream = raw_client(
        srv.addr,
        b"GET /test/stream HTTP/1.1\r\nhost: 127.0.0.1\r\n\r\n",
    )
    .await;
    tx.send(Bytes::from_static(b"x")).await.unwrap();
    stream.read_until(b"x", Duration::from_secs(5)).await;
    srv.signal();
    assert_eq!(
        srv.finished(Duration::from_secs(5)).await,
        Outcome::TimedOut
    );
    drop(tx);

    // A second signal does not wait for the budget.
    let (tx, extra) = support::stream_route("/test/stream");
    let mut srv = pool
        .boot(Boot {
            extra: Some(extra),
            ..Boot::default()
        })
        .await;
    let mut stream = raw_client(
        srv.addr,
        b"GET /test/stream HTTP/1.1\r\nhost: 127.0.0.1\r\n\r\n",
    )
    .await;
    tx.send(Bytes::from_static(b"x")).await.unwrap();
    stream.read_until(b"x", Duration::from_secs(5)).await;
    srv.signal();
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(!srv.join.is_finished());
    srv.force();
    assert_eq!(srv.finished(Duration::from_secs(5)).await, Outcome::Forced);
    drop(tx);
}

#[tokio::test]
async fn second_instance_refuses_to_start() {
    let pool = Pool::new();
    let srv = pool.boot(Boot::default()).await;
    let cfg = pooltime::config::Config {
        port: srv.addr.port(),
        ..pooltime::config::Config::default()
    };

    let err = preflight(&cfg).await.expect_err("pooltime already answers");
    assert!(
        matches!(err, StartupError::AlreadyRunning { .. }),
        "{err:?}"
    );
    assert!(err.to_string().contains("already running"), "{err}");

    // A free port passes.
    let free = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let free_port = free.local_addr().unwrap().port();
    drop(free);
    let cfg = pooltime::config::Config {
        port: free_port,
        ..pooltime::config::Config::default()
    };
    preflight(&cfg).await.expect("free port");

    // Something else on the port (not pooltime) is left for bind() to report.
    let other =
        serve(|_| async { axum::response::Response::new(axum::body::Body::from("<html>")) }).await;
    let cfg = pooltime::config::Config {
        port: other.port(),
        ..pooltime::config::Config::default()
    };
    preflight(&cfg)
        .await
        .expect("non-pooltime listener is not 'already running'");
}

#[tokio::test]
async fn bro_answering_blocks_imported_accounts_unless_flag() {
    let pool = Pool::new();
    let bro = serve(|_| async {
        let mut r = axum::response::Response::new(axum::body::Body::from(
            r#"{"status":"ok","accounts":2,"available":2,"pending":0}"#,
        ));
        r.headers_mut().insert(
            http::header::CONTENT_TYPE,
            http::HeaderValue::from_static("application/json"),
        );
        r
    })
    .await;

    // No imported accounts: bro running is irrelevant.
    check_bro_shadow(&pool.reg, bro.port(), false)
        .await
        .unwrap();

    let mut imported = pooltime::accounts::AccountMeta::new(
        "from-bro".parse().unwrap(),
        pooltime::providers::ProviderId::Anthropic,
    );
    imported.imported = true;
    pool.reg.upsert(imported).unwrap();

    let err = check_bro_shadow(&pool.reg, bro.port(), false)
        .await
        .expect_err("bro answers and imported accounts exist");
    assert!(
        matches!(err, StartupError::BroRunning { port } if port == bro.port()),
        "{err:?}"
    );
    assert!(err.to_string().contains("--i-stopped-bro"), "{err}");

    check_bro_shadow(&pool.reg, bro.port(), true).await.unwrap();

    // Nothing answering on bro's port: fine.
    let free = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let free_port = free.local_addr().unwrap().port();
    drop(free);
    check_bro_shadow(&pool.reg, free_port, false).await.unwrap();

    // A listener that is not a bro pool (a dev server answering 200 to everything) does not block.
    let spa =
        serve(|_| async { axum::response::Response::new(axum::body::Body::from("<html>")) }).await;
    check_bro_shadow(&pool.reg, spa.port(), false)
        .await
        .unwrap();
}

/// A listener that accepts connections and never answers (a saturated or wedged server).
async fn silent_listener() -> std::net::SocketAddr {
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move {
        let mut held = Vec::new();
        while let Ok((s, _)) = l.accept().await {
            held.push(s);
        }
    });
    addr
}

#[tokio::test]
async fn slow_bro_counts_as_running() {
    let pool = Pool::new();
    let silent = silent_listener().await;
    assert_eq!(
        probe_within(silent, Duration::from_millis(300)).await,
        Probe::Unresponsive
    );

    // Nothing listening is Free, not Unresponsive.
    let free = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let free_addr = free.local_addr().unwrap();
    drop(free);
    assert_eq!(
        probe_within(free_addr, Duration::from_millis(300)).await,
        Probe::Free
    );

    let mut imported = pooltime::accounts::AccountMeta::new(
        "from-bro".parse().unwrap(),
        pooltime::providers::ProviderId::Anthropic,
    );
    imported.imported = true;
    pool.reg.upsert(imported).unwrap();
    let t = Duration::from_millis(300);
    let err = check_bro_shadow_within(&pool.reg, silent.port(), false, t)
        .await
        .expect_err("a wedged listener on bro's port may be bro");
    assert!(matches!(err, StartupError::BroRunning { .. }), "{err:?}");
    check_bro_shadow_within(&pool.reg, silent.port(), true, t)
        .await
        .unwrap();
}

#[tokio::test]
async fn empty_proxy_key_means_open() {
    let pool = Pool::new();
    let srv = pool
        .boot(Boot {
            proxy_key: Some(String::new()),
            ..Boot::default()
        })
        .await;
    let r = http_req(
        srv.addr,
        "POST",
        "/v1/messages",
        &json_headers(),
        br#"{"model":"m"}"#,
    )
    .await;
    assert_ne!(r.status, 401);
}

/// Task 25: the management API (`/api/v1/*`).
mod mgmt {
    use std::net::SocketAddr;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::time::Duration;

    use async_trait::async_trait;
    use pooltime::accounts::{
        AccountId, AccountMeta, AccountState, AccountStatus, ErrorRecord, Identity,
        RateLimitSnapshot, StateBook, TimedOverride, Window, WindowStatus,
    };
    use pooltime::config::{Config, Paths};
    use pooltime::errors::{FailReason, RefreshErrorKind};
    use pooltime::models::{ModelTable, RouteDef};
    use pooltime::providers::{
        AttemptCtx, AttemptOutcome, InboundRequest, Provider, ProviderId, UsageError,
    };
    use pooltime::routing::Tuning;
    use pooltime::server::mgmt::{
        build_status, ensure_mgmt_key, mgmt_key_path, session_token, AccountView, CredsInfo,
        StatusInputs, SESSION_COOKIE,
    };
    use serde_json::{json, Value};
    use time::macros::datetime;
    use time::OffsetDateTime;

    use super::support::fake_anthropic::{FakeAnthropic, Reply, TOKEN};
    use super::support::sockets::{http_get, http_req, HttpResp};
    use super::support::{Boot, Booted, Pool};

    const KEY: &str = "mgmt-test-key";
    const UUID: &str = "aaaaaaaa-0000-0000-0000-000000000001";
    const ORG: &str = "cccccccc-0000-0000-0000-000000000003";

    fn id(s: &str) -> AccountId {
        s.parse().unwrap()
    }

    fn rfc3339(t: OffsetDateTime) -> String {
        t.format(&time::format_description::well_known::Rfc3339)
            .unwrap()
    }

    /// One management call with the mgmt key as a bearer token.
    async fn call(srv: &Booted, method: &str, path: &str, body: Option<Value>) -> HttpResp {
        raw(
            srv.addr,
            method,
            path,
            body,
            &[("authorization", &format!("Bearer {KEY}"))],
        )
        .await
    }

    async fn raw(
        addr: SocketAddr,
        method: &str,
        path: &str,
        body: Option<Value>,
        headers: &[(&str, &str)],
    ) -> HttpResp {
        let bytes = body.map(|b| b.to_string().into_bytes()).unwrap_or_default();
        let mut h = vec![("content-type", "application/json")];
        h.extend_from_slice(headers);
        http_req(addr, method, path, &h, &bytes).await
    }

    fn message(r: &HttpResp) -> String {
        r.json()["error"]["message"]
            .as_str()
            .unwrap_or_else(|| panic!("no error message in {}", r.text()))
            .to_string()
    }

    fn window(key: &str, model: Option<&str>, util: f64, reset: Option<OffsetDateTime>) -> Window {
        Window {
            reset_synthetic: false,
            key: key.into(),
            model: model.map(str::to_string),
            status: WindowStatus::Allowed,
            utilization: util,
            reset,
        }
    }

    fn snapshot(windows: Vec<Window>, at: OffsetDateTime) -> RateLimitSnapshot {
        RateLimitSnapshot {
            unified_status: Some("allowed".into()),
            windows,
            updated_at: at,
        }
    }

    // ---- status ----

    fn view(name: &str, status: AccountStatus, state: AccountState) -> AccountView {
        AccountView {
            meta: AccountMeta::new(id(name), ProviderId::Anthropic),
            available: status == AccountStatus::Ready,
            status,
            status_since: None,
            state,
            creds: CredsInfo::Present {
                expires_at: Some(datetime!(2026-11-07 18:00 UTC)),
                refresh_expires_at: Some(datetime!(2026-12-01 00:00 UTC)),
            },
            in_flight: 0,
            sessions: 0,
        }
    }

    #[test]
    fn status_shape_snapshot() {
        let now = datetime!(2026-11-07 12:00 UTC);
        let reset5 = datetime!(2026-11-07 15:00 UTC);
        let reset7 = datetime!(2026-11-12 09:00 UTC);

        // ready: both windows plus a model-scoped one, in flight, a session, an override.
        let mut ready = view(
            "alpha",
            AccountStatus::Ready,
            AccountState {
                snapshot: Some(snapshot(
                    vec![
                        window("5h", None, 0.25, Some(reset5)),
                        window("7d", None, 0.37, Some(reset7)),
                        window("7d-fable", Some("fable"), 0.9, Some(reset7)),
                    ],
                    now,
                )),
                ..AccountState::default()
            },
        );
        ready.meta.priority = 10;
        ready.meta.weight = 2.0;
        ready.meta.identity = Identity {
            email: Some("alpha@example.com".into()),
            ..Identity::default()
        };
        ready.meta.weight_override = Some(TimedOverride {
            weight: 5.0,
            until: datetime!(2026-11-08 00:00 UTC),
        });
        ready.in_flight = 2;
        ready.sessions = 3;
        ready.status_since = Some(datetime!(2026-11-05 08:30 UTC));

        // rate limited by the 5h window; last error recorded.
        let mut limited = view(
            "bravo",
            AccountStatus::RateLimited {
                until: reset5,
                window: Some("5h".into()),
            },
            AccountState {
                snapshot: Some(snapshot(
                    vec![
                        window("5h", None, 1.0, Some(reset5)),
                        window("7d", None, 0.9, Some(reset7)),
                    ],
                    now,
                )),
                last_error: Some(ErrorRecord {
                    at: datetime!(2026-11-07 11:58 UTC),
                    reason: FailReason::Quota {
                        reset: Some(reset5),
                    },
                    upstream_request_id: Some("req_abc".into()),
                }),
                ..AccountState::default()
            },
        );
        limited.status_since = Some(datetime!(2026-11-07 11:58 UTC));

        // dead account with a stale, healthy-looking snapshot: no windows, no headroom.
        let mut dead = view(
            "charlie",
            AccountStatus::NeedsLogin,
            AccountState {
                snapshot: Some(snapshot(vec![window("7d", None, 0.0, Some(reset7))], now)),
                dead_refresh_fp: Some("fp".into()),
                ..AccountState::default()
            },
        );
        dead.status_since = Some(datetime!(2026-11-06 00:00 UTC));

        let mut billing = view("delta", AccountStatus::Billing, AccountState::default());
        billing.creds = CredsInfo::Present {
            expires_at: None,
            refresh_expires_at: None,
        };

        let table = ModelTable::defaults();
        let inputs = StatusInputs {
            accounts: vec![ready, limited, dead, billing],
            tuning: Tuning::default(),
            mapping_enabled: table.mapping_enabled(),
            mappings: table.mappings().to_vec(),
            inert_mappings: vec![],
            expiry_warn: time::Duration::days(3),
        };
        let v = build_status(&inputs, now);
        insta::assert_snapshot!(serde_json::to_string_pretty(&v).unwrap());

        // The headline numbers, independent of the snapshot file.
        assert_eq!(v["pool"]["accounts"], 4);
        assert_eq!(v["pool"]["available"], 1);
        assert_eq!(v["pool"]["seven_day_left_total"].as_f64(), Some(0.73));
        assert_eq!(v["accounts"][2]["seven_day_left"], Value::Null);
        assert_eq!(v["accounts"][2]["windows"], json!([]));
    }

    #[test]
    fn expired_override_is_not_reported() {
        let now = datetime!(2026-11-07 12:00 UTC);
        let mut a = view("alpha", AccountStatus::Ready, AccountState::default());
        a.meta.weight_override = Some(TimedOverride {
            weight: 5.0,
            until: now + time::Duration::hours(1),
        });
        let inputs = StatusInputs {
            accounts: vec![a],
            tuning: Tuning::default(),
            mapping_enabled: false,
            mappings: vec![],
            inert_mappings: vec![],
            expiry_warn: time::Duration::days(3),
        };
        let live = build_status(&inputs, now);
        assert_eq!(live["accounts"][0]["weight_override"]["weight"], 5.0);
        assert_eq!(live["accounts"][0]["effective_weight"], 5.0);
        let later = build_status(&inputs, now + time::Duration::hours(2));
        assert_eq!(later["accounts"][0]["weight_override"], Value::Null);
        assert_eq!(later["accounts"][0]["effective_weight"], 1.0);
        assert_eq!(later["accounts"][0]["weight"], 1.0);
    }

    #[tokio::test]
    async fn status_over_http_is_unauthenticated_and_live() {
        let pool = Pool::new();
        let now = OffsetDateTime::now_utc();
        pool.state.merge_header_snapshot(
            &pool.meta.id,
            snapshot(
                vec![
                    window("5h", None, 0.25, Some(now + time::Duration::hours(2))),
                    window("7d", None, 0.5, Some(now + time::Duration::days(3))),
                ],
                now,
            ),
        );
        // No credentials at all: needs login, and its stale snapshot must not read as headroom.
        let dead = AccountMeta::new(id("dead"), ProviderId::Anthropic);
        pool.reg.upsert(dead.clone()).unwrap();
        pool.state
            .merge_header_snapshot(&dead.id, snapshot(vec![window("7d", None, 0.0, None)], now));
        let srv = pool.boot(Boot::default()).await;

        let r = http_get(srv.addr, "/api/v1/status", &[]).await;
        assert_eq!(r.status, 200);
        let v = r.json();
        assert_eq!(v["version"], env!("CARGO_PKG_VERSION"));
        assert!(v["now"].as_str().is_some());
        assert_eq!(v["pool"]["accounts"], 2);
        assert_eq!(v["pool"]["available"], 1);
        assert_eq!(v["pool"]["seven_day_left_total"].as_f64(), Some(0.5));
        assert_eq!(v["tuning"]["five_hour_exp"], 1.0);
        assert_eq!(v["mapping"]["enabled"], false);

        let accounts = v["accounts"].as_array().unwrap();
        let dead_v = accounts.iter().find(|a| a["id"] == "dead").unwrap();
        assert_eq!(dead_v["status"], "needs_login");
        assert_eq!(dead_v["reason"], "never_logged_in");
        assert_eq!(dead_v["seven_day_left"], Value::Null);
        assert_eq!(dead_v["windows"], json!([]));
        assert!(dead_v["status_since"].as_str().is_some(), "{dead_v}");
        assert!(dead_v["expires_at"].is_null());

        let work = accounts.iter().find(|a| a["id"] == "work").unwrap();
        assert_eq!(work["status"], "ready");
        assert_eq!(work["provider"], "anthropic");
        assert_eq!(work["seven_day_left"].as_f64(), Some(0.5));
        assert_eq!(work["windows"].as_array().unwrap().len(), 2);
        assert!(work["expires_at"].as_str().is_some());
        assert_eq!(work["priority"], 100);
        assert_eq!(work["in_flight"], 0);
        assert!(work["last_error"].is_null());
        // No credential material anywhere in the response.
        let text = r.text();
        for secret in [super::support::ACCESS, super::support::REFRESH] {
            assert!(!text.contains(secret));
        }
    }

    // ---- auth ----

    #[tokio::test]
    async fn mutations_require_key_or_cookie() {
        let pool = Pool::new();
        let srv = pool.boot(Boot::default()).await;
        let cookie = format!("{SESSION_COOKIE}={}", session_token(KEY));
        let mutations: Vec<(&str, &str, Value)> = vec![
            (
                "POST",
                "/api/v1/accounts",
                json!({"id": "x", "provider": "anthropic"}),
            ),
            ("DELETE", "/api/v1/accounts/work", json!({})),
            (
                "POST",
                "/api/v1/accounts/work/login",
                json!({"mode": "paste"}),
            ),
            (
                "POST",
                "/api/v1/accounts/work/routing",
                json!({"priority": 5}),
            ),
            ("POST", "/api/v1/accounts/work/recheck", json!({})),
            ("POST", "/api/v1/signin/nope/code", json!({"code": "a#b"})),
            ("POST", "/api/v1/signin/nope/bind", json!({})),
            ("POST", "/api/v1/tuning", json!({"five_hour_exp": 2})),
            ("POST", "/api/v1/mappings", json!({"enabled": false})),
        ];
        for (method, path, body) in &mutations {
            let anon = raw(srv.addr, method, path, Some(body.clone()), &[]).await;
            assert_eq!(anon.status, 401, "anonymous {method} {path}");
            assert_eq!(anon.json()["error"]["type"], "authentication_error");

            for bad in [
                "Bearer wrong",
                "Bearer ",
                "Basic bW10ZXN0LWtleQ==",
                "mgmt-test-key",
            ] {
                let r = raw(
                    srv.addr,
                    method,
                    path,
                    Some(body.clone()),
                    &[("authorization", bad)],
                )
                .await;
                assert_eq!(r.status, 401, "{bad:?} {method} {path}");
            }
            // The proxy-style x-api-key is not a management credential.
            let r = raw(
                srv.addr,
                method,
                path,
                Some(body.clone()),
                &[("x-api-key", KEY)],
            )
            .await;
            assert_eq!(r.status, 401, "x-api-key {method} {path}");
            // The raw key is not the cookie value: the cookie carries a derived token.
            let leaked = format!("{SESSION_COOKIE}={KEY}");
            let r = raw(
                srv.addr,
                method,
                path,
                Some(body.clone()),
                &[("cookie", &leaked)],
            )
            .await;
            assert_eq!(r.status, 401, "raw key as cookie {method} {path}");
            let r = raw(
                srv.addr,
                method,
                path,
                Some(body.clone()),
                &[("cookie", &format!("{SESSION_COOKIE}=nope"))],
            )
            .await;
            assert_eq!(r.status, 401, "bad cookie {method} {path}");
        }
        // Nothing above changed anything.
        assert!(pool.reg.get(&id("work")).is_some());
        assert!(pool.reg.get(&id("x")).is_none());

        // A good bearer or cookie gets past auth (the call itself may still be refused).
        for (method, path, body) in &mutations {
            if *method == "DELETE" {
                continue;
            }
            for h in [
                ("authorization", format!("Bearer {KEY}")),
                ("authorization", format!("bearer {KEY}")),
                ("cookie", format!("other=1; {cookie}; more=2")),
            ] {
                let r = raw(srv.addr, method, path, Some(body.clone()), &[(h.0, &h.1)]).await;
                assert_ne!(r.status, 401, "{method} {path} with {}", h.0);
                assert_ne!(r.status, 403, "{method} {path} with {}", h.0);
            }
        }
        // Reads need nothing.
        assert_eq!(http_get(srv.addr, "/api/v1/status", &[]).await.status, 200);
        assert_eq!(
            http_get(srv.addr, "/api/v1/signin/nope", &[]).await.status,
            404
        );
        // Auth is checked after the method: a wrong verb is a 405, not a 401.
        let r = raw(srv.addr, "GET", "/api/v1/tuning", None, &[]).await;
        assert_eq!(r.status, 405);
        assert_eq!(r.header("allow"), Some("POST"));
        // A valid key does not bypass the Host/Origin guard.
        let before = srv.app.tuning();
        let r = raw(
            srv.addr,
            "POST",
            "/api/v1/tuning",
            Some(json!({"five_hour_exp": 3})),
            &[
                ("authorization", &format!("Bearer {KEY}")),
                ("origin", "http://evil.example"),
            ],
        )
        .await;
        assert_eq!(r.status, 403);
        assert_eq!(srv.app.tuning(), before);
    }

    #[tokio::test]
    async fn session_exchange_sets_cookie() {
        let pool = Pool::new();
        let srv = pool.boot(Boot::default()).await;

        // Wrong or missing key: no cookie.
        for body in [
            json!({"k": "wrong"}),
            json!({"k": ""}),
            json!({}),
            json!({"k": 7}),
        ] {
            let r = raw(srv.addr, "POST", "/api/v1/session", Some(body), &[]).await;
            assert_eq!(r.status, 401);
            assert!(r.header("set-cookie").is_none());
        }

        let r = raw(
            srv.addr,
            "POST",
            "/api/v1/session",
            Some(json!({"k": KEY})),
            &[],
        )
        .await;
        assert_eq!(r.status, 200);
        assert_eq!(r.json()["ok"], true);
        let set = r.header("set-cookie").expect("set-cookie").to_string();
        assert!(set.starts_with(&format!("{SESSION_COOKIE}=")), "{set}");
        assert!(set.contains("HttpOnly"), "{set}");
        assert!(set.contains("SameSite=Strict"), "{set}");
        assert!(set.contains("Path=/"), "{set}");
        assert!(!set.contains("Secure"), "plain http on localhost: {set}");
        assert!(
            !set.contains(KEY),
            "the cookie must not carry the key itself"
        );
        assert!(!r.text().contains(KEY));

        // The cookie alone authorizes a mutation.
        let pair = set.split(';').next().unwrap().to_string();
        let r = raw(
            srv.addr,
            "POST",
            "/api/v1/tuning",
            Some(json!({"five_hour_exp": 2.0})),
            &[("cookie", &pair)],
        )
        .await;
        assert_eq!(r.status, 200, "{}", r.text());
        assert_eq!(srv.app.tuning().five_hour_exp, 2.0);

        // A foreign page cannot exchange or ride the cookie.
        let r = raw(
            srv.addr,
            "POST",
            "/api/v1/session",
            Some(json!({"k": KEY})),
            &[("origin", "http://evil.example")],
        )
        .await;
        assert_eq!(r.status, 403);
        assert!(r.header("set-cookie").is_none());
    }

    #[test]
    fn mgmt_key_written_atomically_0600_without_temp_leftovers() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::under(dir.path());
        let key = ensure_mgmt_key(&paths).unwrap();
        let p = mgmt_key_path(&paths);
        let mode = std::fs::metadata(&p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        assert_eq!(std::fs::read_to_string(&p).unwrap(), format!("{key}\n"));
        // Regenerating a malformed key replaces the file by rename: no temp file remains.
        std::fs::write(&p, "short").unwrap();
        let fresh = ensure_mgmt_key(&paths).unwrap();
        assert_eq!(std::fs::read_to_string(&p).unwrap(), format!("{fresh}\n"));
        let names: Vec<String> = std::fs::read_dir(&paths.run)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec!["mgmt.key".to_string()]);
    }

    #[test]
    fn mgmt_key_created_once_with_0600() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::under(dir.path());
        let key = ensure_mgmt_key(&paths).unwrap();
        assert_eq!(key.len(), 64);
        assert!(key.bytes().all(|b| b.is_ascii_hexdigit()));
        let p = mgmt_key_path(&paths);
        assert!(p.starts_with(&paths.run));
        let mode = |p: &std::path::Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&p), 0o600);
        assert_eq!(std::fs::read_to_string(&p).unwrap().trim(), key);
        // Stable across calls, even if the file got looser permissions.
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(ensure_mgmt_key(&paths).unwrap(), key);
        assert_eq!(mode(&p), 0o600);
        // A truncated/garbled file is replaced rather than used as a weak key.
        std::fs::write(&p, "short").unwrap();
        let fresh = ensure_mgmt_key(&paths).unwrap();
        assert_eq!(fresh.len(), 64);
        assert_ne!(fresh, key);
    }

    // ---- routing ----

    #[tokio::test]
    async fn routing_bounds_rejected() {
        let pool = Pool::new();
        let srv = pool.boot(Boot::default()).await;
        let path = "/api/v1/accounts/work/routing";
        let future = rfc3339(OffsetDateTime::now_utc() + time::Duration::hours(1));
        let past = rfc3339(OffsetDateTime::now_utc() - time::Duration::hours(1));

        let prio = "priority must be a non-negative integer";
        let wt = "weight must be a number between 0.1 and 10";
        let bad: Vec<(Value, &str)> = vec![
            (json!({}), "provide priority"),
            (json!({"priority": -1}), prio),
            (json!({"priority": 1.5}), prio),
            (json!({"priority": "high"}), prio),
            (json!({"priority": 4_294_967_296u64}), prio),
            (json!({"weight": 0.05}), wt),
            (json!({"weight": 10.5}), wt),
            (json!({"weight": "2"}), wt),
            (json!({"weight": null}), wt),
            (
                json!({"weight": 2, "until": "tomorrow"}),
                "until must be an RFC 3339",
            ),
            (
                json!({"weight": 2, "until": 12345}),
                "until must be an RFC 3339",
            ),
            (
                json!({"weight": 2, "until": past}),
                "until must be in the future",
            ),
            (json!({"until": future}), "until requires weight"),
            (json!({"weight": 20, "until": future}), wt),
            // One bad field rejects the whole request: priority must not be applied.
            (json!({"priority": 1, "weight": 99}), wt),
            (json!([1, 2]), "JSON object"),
        ];
        for (body, want) in bad {
            let r = call(&srv, "POST", path, Some(body.clone())).await;
            assert_eq!(r.status, 400, "{body}: {}", r.text());
            assert_eq!(r.json()["error"]["type"], "invalid_request_error");
            assert!(message(&r).contains(want), "{body}: {}", message(&r));
        }
        let meta = pool.reg.get(&id("work")).unwrap();
        assert_eq!(meta.priority, 100);
        assert_eq!(meta.weight, 1.0);
        assert!(meta.weight_override.is_none());

        // Unknown account and malformed id.
        let r = call(
            &srv,
            "POST",
            "/api/v1/accounts/ghost/routing",
            Some(json!({"priority": 1})),
        )
        .await;
        assert_eq!(r.status, 404);
        assert!(message(&r).contains("ghost"));
        let r = call(
            &srv,
            "POST",
            "/api/v1/accounts/bad%20id/routing",
            Some(json!({"priority": 1})),
        )
        .await;
        assert_eq!(r.status, 400);
        let r = call(&srv, "POST", path, None).await;
        assert_eq!(r.status, 400, "empty body is not JSON");

        // Boundary values are accepted.
        for body in [
            json!({"priority": 0}),
            json!({"priority": 4_294_967_295u64}),
            json!({"weight": 0.1}),
            json!({"weight": 10}),
            json!({"priority": 7, "weight": 2.5}),
        ] {
            let r = call(&srv, "POST", path, Some(body.clone())).await;
            assert_eq!(r.status, 200, "{body}: {}", r.text());
        }
        let meta = pool.reg.get(&id("work")).unwrap();
        assert_eq!((meta.priority, meta.weight), (7, 2.5));
        let r = call(&srv, "POST", path, Some(json!({"priority": 3}))).await;
        assert_eq!(r.json()["priority"], 3);
        assert_eq!(r.json()["weight"], 2.5, "an omitted field is left alone");
    }

    #[tokio::test]
    async fn timed_weight_override_reverts() {
        let pool = Pool::new();
        let srv = pool.boot(Boot::default()).await;
        let path = "/api/v1/accounts/work/routing";
        let until = OffsetDateTime::now_utc() + time::Duration::milliseconds(1500);

        let r = call(
            &srv,
            "POST",
            path,
            Some(json!({"weight": 8, "until": rfc3339(until)})),
        )
        .await;
        assert_eq!(r.status, 200, "{}", r.text());
        let meta = pool.reg.get(&id("work")).unwrap();
        assert_eq!(meta.weight, 1.0, "the base weight is untouched");
        assert_eq!(meta.weight_override.as_ref().map(|o| o.weight), Some(8.0));

        // Visible while active, and what routing scores with.
        let v = http_get(srv.addr, "/api/v1/status", &[]).await.json();
        let a = &v["accounts"][0];
        assert_eq!(a["weight"], 1.0);
        assert_eq!(a["weight_override"]["weight"], 8.0);
        assert_eq!(a["weight_override"]["until"], rfc3339(until).as_str());
        assert_eq!(a["effective_weight"], 8.0);
        let now = OffsetDateTime::now_utc();
        assert_eq!(pooltime::routing::score::effective_weight(&meta, now), 8.0);

        // After `until` it reverts without anyone touching it.
        tokio::time::sleep(Duration::from_millis(1700)).await;
        let v = http_get(srv.addr, "/api/v1/status", &[]).await.json();
        let a = &v["accounts"][0];
        assert_eq!(a["weight_override"], Value::Null);
        assert_eq!(a["effective_weight"], 1.0);
        let meta = pool.reg.get(&id("work")).unwrap();
        assert_eq!(
            pooltime::routing::score::effective_weight(&meta, OffsetDateTime::now_utc()),
            1.0
        );

        // An explicit `until: null` with no weight cancels an active override early.
        let later = rfc3339(OffsetDateTime::now_utc() + time::Duration::hours(1));
        call(
            &srv,
            "POST",
            path,
            Some(json!({"weight": 4, "until": later})),
        )
        .await;
        let r = call(&srv, "POST", path, Some(json!({"until": null}))).await;
        assert_eq!(r.status, 200, "{}", r.text());
        assert!(pool.reg.get(&id("work")).unwrap().weight_override.is_none());
    }

    // ---- tuning & mappings ----

    #[tokio::test]
    async fn tuning_bounds_and_live_apply() {
        let pool = Pool::new();
        let srv = pool.boot(Boot::default()).await;
        let before = srv.app.tuning();
        assert_eq!(before, Tuning::default());

        for (body, want) in [
            (json!({}), "at least one"),
            (json!([]), "JSON object"),
            (
                json!({"five_hour_exp": 6}),
                "five_hour_exp must be within 0..=5",
            ),
            (
                json!({"five_hour_exp": -0.1}),
                "five_hour_exp must be within 0..=5",
            ),
            (
                json!({"min_headroom": 1.5}),
                "min_headroom must be within 0..=1",
            ),
            (
                json!({"headroom_taper_start": 0}),
                "headroom_taper_start must be within (0, 1]",
            ),
            (
                json!({"headroom_taper_start": "x"}),
                "headroom_taper_start must be a number",
            ),
            (json!({"bogus": 1}), "unknown tuning field 'bogus'"),
            // Atomic: the valid half is not applied.
            (
                json!({"five_hour_exp": 2, "min_headroom": 9}),
                "min_headroom",
            ),
        ] {
            let r = call(&srv, "POST", "/api/v1/tuning", Some(body.clone())).await;
            assert_eq!(r.status, 400, "{body}: {}", r.text());
            assert!(message(&r).contains(want), "{body}: {}", message(&r));
        }
        assert_eq!(srv.app.tuning(), before);

        let r = call(
            &srv,
            "POST",
            "/api/v1/tuning",
            Some(json!({"five_hour_exp": 2.5})),
        )
        .await;
        assert_eq!(r.status, 200, "{}", r.text());
        assert_eq!(r.json()["tuning"]["five_hour_exp"], 2.5);
        let now = srv.app.tuning();
        assert_eq!(now.five_hour_exp, 2.5);
        assert_eq!(
            now.min_headroom, before.min_headroom,
            "a partial patch keeps the rest"
        );
        let v = http_get(srv.addr, "/api/v1/status", &[]).await.json();
        assert_eq!(v["tuning"]["five_hour_exp"], 2.5);

        // Persisted to config.toml (so it survives a restart), other settings untouched.
        let saved = Config::load_file(&Paths::under(pool.dir.path())).unwrap();
        assert_eq!(saved.routing.five_hour_exp, 2.5);
        assert_eq!(saved.routing.min_headroom, before.min_headroom);
        assert_eq!(saved.port, Config::default().port);
    }

    #[tokio::test]
    async fn mappings_validation_and_persist() {
        let pool = Pool::new();
        let srv = pool.boot(Boot::default()).await;
        for (body, want) in [
            (json!({}), "provide enabled and/or mappings"),
            (json!({"enabled": "yes"}), "enabled must be a boolean"),
            (json!({"mappings": {}}), "mappings must be an array"),
            (json!({"mappings": [{"from": 1}]}), "each mapping needs"),
            (
                json!({"mappings": [{"from": "nope", "to": "gpt-6-sol"}]}),
                "unknown family 'nope'",
            ),
            (
                json!({"mappings": [{"from": "opus", "to": "no-such-model"}]}),
                "unknown target model 'no-such-model'",
            ),
            (
                json!({"mappings": [{"from": "opus", "to": "gpt-6-sol", "effort": {"low": "bogus"}}]}),
                "invalid effort value 'bogus' for tier 'low'",
            ),
            // Atomic: the flag is not flipped when a row is bad.
            (
                json!({"enabled": true, "mappings": [{"from": "x", "to": "y"}]}),
                "unknown family",
            ),
        ] {
            let r = call(&srv, "POST", "/api/v1/mappings", Some(body.clone())).await;
            assert_eq!(r.status, 400, "{body}: {}", r.text());
            assert!(message(&r).contains(want), "{body}: {}", message(&r));
        }
        assert!(!srv.app.models.read().unwrap().mapping_enabled());

        let r = call(
            &srv,
            "POST",
            "/api/v1/mappings",
            Some(json!({"enabled": true, "mappings": [{"from": "opus", "to": "gpt-6-luna", "effort": {"low": "low"}}]})),
        )
        .await;
        assert_eq!(r.status, 200, "{}", r.text());
        let v = r.json();
        assert_eq!(v["mapping"]["enabled"], true);
        let rows = v["mapping"]["mappings"].as_array().unwrap();
        let opus = rows.iter().find(|m| m["from"] == "opus").unwrap();
        assert_eq!(opus["to"], "gpt-6-luna");
        assert_eq!(opus["effort"]["low"], "low");
        // Untouched families keep their rows.
        assert!(rows
            .iter()
            .any(|m| m["from"] == "haiku" && m["to"] == "gpt-6-luna"));
        assert!(rows
            .iter()
            .any(|m| m["from"] == "fable" && m["to"] == "gpt-6-astra"));

        let status = http_get(srv.addr, "/api/v1/status", &[]).await.json();
        assert_eq!(status["mapping"]["enabled"], true);

        // Persisted: a fresh load sees it.
        let loaded = ModelTable::load(&Paths::under(pool.dir.path())).unwrap();
        assert!(loaded.mapping_enabled());
        assert_eq!(
            loaded
                .mappings()
                .iter()
                .find(|m| m.from == "opus")
                .unwrap()
                .to,
            "gpt-6-luna"
        );
    }

    // ---- accounts ----

    #[tokio::test]
    async fn account_create_delete_forgets_state() {
        let pool = Pool::new();
        let srv = pool.boot(Boot::default()).await;

        let r = call(
            &srv,
            "POST",
            "/api/v1/accounts",
            Some(json!({"id": "extra", "provider": "anthropic"})),
        )
        .await;
        assert_eq!(r.status, 201, "{}", r.text());
        assert_eq!(r.json()["id"], "extra");
        let meta = pool.reg.get(&id("extra")).unwrap();
        assert_eq!(meta.provider, ProviderId::Anthropic);
        assert_eq!(meta.priority, 100);

        for (body, status, want) in [
            (
                json!({"id": "extra", "provider": "anthropic"}),
                409,
                "already exists",
            ),
            (
                json!({"id": "../etc", "provider": "anthropic"}),
                400,
                "invalid account id",
            ),
            (
                json!({"id": "ok-id", "provider": "gemini"}),
                400,
                "unknown provider",
            ),
            (json!({"id": "ok-id"}), 400, "provider"),
            (json!({"provider": "anthropic"}), 400, "id"),
        ] {
            let r = call(&srv, "POST", "/api/v1/accounts", Some(body.clone())).await;
            assert_eq!(r.status, status, "{body}: {}", r.text());
            assert!(message(&r).contains(want), "{body}: {}", message(&r));
        }
        assert!(pool.reg.get(&id("ok-id")).is_none());
        let r = call(
            &srv,
            "POST",
            "/api/v1/accounts",
            Some(json!({"id": "oa", "provider": "openai"})),
        )
        .await;
        assert_eq!(
            r.status, 201,
            "an openai account can be registered even before it can sign in"
        );

        // Cool the account down, then delete it.
        let now = OffsetDateTime::now_utc();
        pool.state.mark_rate_limited(
            &id("extra"),
            now + time::Duration::hours(1),
            Some("5h".into()),
            now,
        );
        let r = call(&srv, "DELETE", "/api/v1/accounts/extra", None).await;
        assert_eq!(r.status, 200, "{}", r.text());
        assert!(pool.reg.get(&id("extra")).is_none());
        let v = http_get(srv.addr, "/api/v1/status", &[]).await.json();
        assert!(v["accounts"]
            .as_array()
            .unwrap()
            .iter()
            .all(|a| a["id"] != "extra"));
        assert_eq!(
            call(&srv, "DELETE", "/api/v1/accounts/extra", None)
                .await
                .status,
            404
        );

        // Re-adding the same id starts clean: the old cooldown is gone.
        pool.add_account("extra", ProviderId::Anthropic, 100, "tok");
        let v = http_get(srv.addr, "/api/v1/status", &[]).await.json();
        let extra = v["accounts"]
            .as_array()
            .unwrap()
            .iter()
            .find(|a| a["id"] == "extra")
            .unwrap()
            .clone();
        assert_eq!(extra["status"], "ready", "{extra}");
    }

    /// Counts polls; the error it answers with (if any) is scripted.
    struct PollProvider {
        polls: AtomicUsize,
        fail: Option<UsageError>,
    }

    #[async_trait]
    impl Provider for PollProvider {
        fn id(&self) -> ProviderId {
            ProviderId::Anthropic
        }
        async fn access_token(&self, _: &AccountMeta, _: bool) -> Result<String, RefreshErrorKind> {
            Ok("t".into())
        }
        async fn attempt(
            &self,
            _: AttemptCtx<'_>,
            _: &InboundRequest,
            _: &StateBook,
        ) -> AttemptOutcome {
            unreachable!("not used")
        }
        async fn poll_usage(&self, _: &AccountMeta) -> Result<RateLimitSnapshot, UsageError> {
            self.polls.fetch_add(1, Ordering::SeqCst);
            match &self.fail {
                Some(e) => Err(e.clone()),
                None => Ok(snapshot(vec![], OffsetDateTime::now_utc())),
            }
        }
        async fn catalog(&self, _: &AccountMeta) -> anyhow::Result<Vec<RouteDef>> {
            Ok(vec![])
        }
    }

    #[tokio::test]
    async fn recheck_clears_billing_and_repolls() {
        for (fail, want_poll) in [(None, "ok"), (Some(UsageError::Status(500)), "status:500")] {
            let pool = Pool::new();
            let provider = Arc::new(PollProvider {
                polls: AtomicUsize::new(0),
                fail,
            });
            let srv = pool.boot(Boot::default().provider(provider.clone())).await;
            pool.state
                .mark_billing(&pool.meta.id, OffsetDateTime::now_utc());
            let v = http_get(srv.addr, "/api/v1/status", &[]).await.json();
            assert_eq!(v["accounts"][0]["status"], "billing");

            let r = call(&srv, "POST", "/api/v1/accounts/work/recheck", None).await;
            assert_eq!(r.status, 200, "{}", r.text());
            let v = r.json();
            assert_eq!(v["poll"], want_poll);
            assert_eq!(
                v["status"], "ready",
                "the billing block is cleared either way"
            );
            assert_eq!(provider.polls.load(Ordering::SeqCst), 1);
        }

        // Unknown account; and no provider registered for the account's provider.
        let pool = Pool::new();
        let srv = pool.boot(Boot::default()).await;
        let r = call(&srv, "POST", "/api/v1/accounts/ghost/recheck", None).await;
        assert_eq!(r.status, 404);
        let r = call(&srv, "POST", "/api/v1/accounts/work/recheck", None).await;
        assert_eq!(r.status, 200);
        assert_eq!(r.json()["poll"], "unavailable");
    }

    /// A pool whose `work` account has an unreadable `account_state` row (so it loads
    /// `unverified`), served by the real Anthropic provider against `fake`.
    async fn unverified_pool(fake: &FakeAnthropic) -> (Pool, Booted) {
        let mut pool = Pool::new();
        pool.store
            .exec(|c| {
                c.execute(
                    "INSERT OR REPLACE INTO account_state(account, json) VALUES ('work', '{corrupt')",
                    [],
                )?;
                Ok(())
            })
            .await
            .unwrap();
        pool.state = Arc::new(pooltime::accounts::StateBook::load(pool.store.clone()).unwrap());
        let (policy, _) = super::support::fast_policy();
        let provider = Arc::new(pool.provider(fake, policy));
        let srv = pool.boot(Boot::default().provider(provider)).await;
        (pool, srv)
    }

    async fn work_account(srv: &Booted) -> Value {
        http_get(srv.addr, "/api/v1/status", &[]).await.json()["accounts"][0].clone()
    }

    #[tokio::test]
    async fn recheck_heals_an_unverified_account_with_a_forced_refresh() {
        let fake = FakeAnthropic::start().await;
        fake.route(TOKEN, |_, _| {
            Reply::json(
                200,
                &json!({"access_token": "at-new", "refresh_token": "rt-new", "expires_in": 28800}),
            )
        });
        // No usage route: the poll fails, so only the refresh can heal the account.
        let (_pool, srv) = unverified_pool(&fake).await;
        let a = work_account(&srv).await;
        assert_eq!(a["status"], "needs_login", "{a}");
        assert_eq!(a["reason"], "state_unreadable", "{a}");

        let r = call(&srv, "POST", "/api/v1/accounts/work/recheck", None).await;
        assert_eq!(r.status, 200, "{}", r.text());
        let v = r.json();
        assert_eq!(v["refresh"], "ok", "{v}");
        assert_ne!(v["poll"], "ok", "the heal must not depend on the poll: {v}");
        assert_eq!(v["status"], "ready", "{v}");
        assert_eq!(fake.requests_to(TOKEN).len(), 1);
        let a = work_account(&srv).await;
        assert_eq!(a["status"], "ready", "{a}");
        assert_eq!(a["reason"], Value::Null);

        // Verified now: another recheck does not spend the refresh token again.
        let r = call(&srv, "POST", "/api/v1/accounts/work/recheck", None).await;
        assert_eq!(r.json()["refresh"], Value::Null);
        assert_eq!(fake.requests_to(TOKEN).len(), 1);
    }

    #[tokio::test]
    async fn recheck_of_an_unverified_account_with_a_dead_token_marks_it_dead() {
        let fake = FakeAnthropic::start().await;
        fake.route(TOKEN, |_, _| {
            Reply::json(400, &json!({"error": "invalid_grant"}))
        });
        let (_pool, srv) = unverified_pool(&fake).await;
        let r = call(&srv, "POST", "/api/v1/accounts/work/recheck", None).await;
        assert_eq!(r.status, 200, "{}", r.text());
        let v = r.json();
        assert_eq!(v["refresh"], "invalid_grant", "{v}");
        assert_eq!(v["status"], "needs_login", "{v}");
        let a = work_account(&srv).await;
        assert_eq!(a["reason"], "refresh_token_dead", "{a}");
        // The dead marker is known now: a second recheck does not re-POST the token.
        call(&srv, "POST", "/api/v1/accounts/work/recheck", None).await;
        assert_eq!(fake.requests_to(TOKEN).len(), 1);
    }

    #[tokio::test]
    async fn recheck_with_a_transient_refresh_failure_stays_unverified() {
        let fake = FakeAnthropic::start().await;
        fake.route(TOKEN, |_, _| Reply::new(503));
        let (_pool, srv) = unverified_pool(&fake).await;
        let r = call(&srv, "POST", "/api/v1/accounts/work/recheck", None).await;
        assert_eq!(r.status, 200, "{}", r.text());
        let v = r.json();
        assert_eq!(v["refresh"], "server:503", "{v}");
        assert_eq!(v["status"], "needs_login", "{v}");
        let a = work_account(&srv).await;
        assert_eq!(a["reason"], "state_unreadable", "{a}");
    }

    #[tokio::test]
    async fn recheck_skips_the_poll_after_a_failed_refresh() {
        // A working usage route: were it polled, success would clear `unverified`.
        const USAGE: &str = "/api/oauth/usage";
        let fake = FakeAnthropic::start().await;
        fake.route(TOKEN, |_, _| Reply::new(503));
        fake.route(USAGE, |_, _| {
            Reply::json(200, &json!({"five_hour": {"utilization": 10}}))
        });
        let (_pool, srv) = unverified_pool(&fake).await;
        let r = call(&srv, "POST", "/api/v1/accounts/work/recheck", None).await;
        assert_eq!(r.status, 200, "{}", r.text());
        let v = r.json();
        assert_eq!(v["refresh"], "server:503", "{v}");
        assert_eq!(v["poll"], "skipped", "{v}");
        assert_eq!(v["status"], "needs_login", "{v}");
        let a = work_account(&srv).await;
        assert_eq!(a["status"], "needs_login", "{a}");
        assert_eq!(a["reason"], "state_unreadable", "{a}");
        assert!(fake.requests_to(USAGE).is_empty(), "usage must not be hit");
    }

    // ---- sign-in ----

    /// A fake Claude OAuth server (token + profile).
    async fn oauth_fake(profile_uuid: &'static str) -> FakeAnthropic {
        let fake = FakeAnthropic::start().await;
        fake.route(TOKEN, |_, _| {
            Reply::json(
                200,
                &json!({
                    "access_token": "at-new-SECRET",
                    "refresh_token": "rt-new-SECRET",
                    "expires_in": 28800,
                    "refresh_token_expires_in": 30 * 86_400,
                    "scope": "user:inference user:profile",
                }),
            )
        });
        fake.route("/api/oauth/profile", move |_, _| {
            Reply::json(
                200,
                &json!({
                    "account": {"uuid": profile_uuid, "email": "me@example.com"},
                    "organization": {"uuid": ORG},
                }),
            )
        });
        fake
    }

    /// A boot whose daemon config points the Anthropic OAuth endpoints at `fake`.
    fn boot_against(fake: &FakeAnthropic) -> Boot {
        let url = fake.url();
        Boot {
            configure: Some(Box::new(move |cfg: &mut Config| {
                cfg.anthropic.token_url = format!("{url}{TOKEN}");
                cfg.anthropic.api_base = url;
            })),
            ..Boot::default()
        }
    }

    fn param(url: &str, name: &str) -> Option<String> {
        reqwest::Url::parse(url)
            .unwrap()
            .query_pairs()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.into_owned())
    }

    async fn wait_signin(srv: &Booted, signin_id: &str, until_not: &str) -> Value {
        let path = format!("/api/v1/signin/{signin_id}");
        for _ in 0..100 {
            let v = http_get(srv.addr, &path, &[]).await.json();
            if v["status"] != until_not {
                return v;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("sign-in stayed {until_not}");
    }

    fn with_baseline(pool: &Pool, uuid: &str, email: &str) {
        let mut meta = pool.meta.clone();
        meta.identity = Identity {
            account_uuid: Some(uuid.into()),
            email: Some(email.into()),
            org_uuid: None,
        };
        pool.reg.upsert(meta).unwrap();
    }

    #[tokio::test]
    async fn login_returns_authorize_url_and_completes_via_paste_code() {
        let fake = oauth_fake(UUID).await;
        let pool = Pool::new();
        // The account already has a baseline identity, so the sign-in saves without binding.
        with_baseline(&pool, UUID, "me@example.com");
        let srv = pool.boot(boot_against(&fake)).await;

        let r = call(
            &srv,
            "POST",
            "/api/v1/accounts/work/login",
            Some(json!({"mode": "paste"})),
        )
        .await;
        assert_eq!(r.status, 200, "{}", r.text());
        let v = r.json();
        let signin_id = v["signin_id"].as_str().unwrap().to_string();
        let url = v["authorize_url"].as_str().unwrap().to_string();
        assert!(
            url.starts_with(&srv.app.cfg.anthropic.authorize_url),
            "{url}"
        );
        assert_eq!(
            param(&url, "code_challenge_method").as_deref(),
            Some("S256")
        );
        assert_eq!(
            param(&url, "redirect_uri").as_deref(),
            Some(srv.app.cfg.anthropic.manual_redirect_url.as_str()),
            "paste mode uses the manual redirect page"
        );
        assert_eq!(param(&url, "login_hint").as_deref(), Some("me@example.com"));
        assert!(v["expires_at"].as_str().is_some());
        let state = param(&url, "state").unwrap();

        // Pending until a code arrives.
        let r = http_get(srv.addr, &format!("/api/v1/signin/{signin_id}"), &[]).await;
        assert_eq!(r.status, 200);
        assert_eq!(r.json()["status"], "pending");
        assert_eq!(r.json()["account"], "work");

        // Bad input is refused and leaves the flow usable.
        let path = format!("/api/v1/signin/{signin_id}/code");
        let r = call(&srv, "POST", &path, Some(json!({"code": "no-hash"}))).await;
        assert_eq!(r.status, 400);
        assert!(message(&r).contains("code"), "{}", message(&r));
        let r = call(
            &srv,
            "POST",
            &path,
            Some(json!({"code": "abc#wrong-state"})),
        )
        .await;
        assert_eq!(r.status, 400);
        assert!(message(&r).contains("state"), "{}", message(&r));
        let r = call(&srv, "POST", &path, Some(json!({}))).await;
        assert_eq!(r.status, 400);
        assert_eq!(
            wait_signin(&srv, &signin_id, "x").await["status"],
            "pending"
        );

        let r = call(
            &srv,
            "POST",
            &path,
            Some(json!({"code": format!("auth-code#{state}")})),
        )
        .await;
        assert_eq!(r.status, 200, "{}", r.text());
        let done = wait_signin(&srv, &signin_id, "pending").await;
        assert_eq!(done["status"], "saved", "{done}");
        assert_eq!(done["identity"]["email"], "me@example.com");

        // Credentials really were replaced, and nothing secret is in any response.
        assert_eq!(pool.creds().access_token, "at-new-SECRET");
        assert_eq!(fake.requests_to(TOKEN)[0].json()["code"], "auth-code");
        assert!(!r.text().contains("SECRET"));
        assert!(!done.to_string().contains("SECRET"));

        // The code is single use.
        let r = call(
            &srv,
            "POST",
            &path,
            Some(json!({"code": format!("again#{state}")})),
        )
        .await;
        assert_eq!(r.status, 409, "{}", r.text());
    }

    #[tokio::test]
    async fn login_for_new_account_creates_it_and_needs_binding() {
        let fake = oauth_fake(UUID).await;
        let pool = Pool::new();
        let srv = pool.boot(boot_against(&fake)).await;

        // `provider` defaults to anthropic; the account is created by the login.
        let r = call(
            &srv,
            "POST",
            "/api/v1/accounts/fresh/login",
            Some(json!({"mode": "paste"})),
        )
        .await;
        assert_eq!(r.status, 200, "{}", r.text());
        let v = r.json();
        let signin_id = v["signin_id"].as_str().unwrap().to_string();
        let state = param(v["authorize_url"].as_str().unwrap(), "state").unwrap();
        let meta = pool.reg.get(&id("fresh")).expect("created by login");
        assert_eq!(meta.provider, ProviderId::Anthropic);

        let r = call(
            &srv,
            "POST",
            &format!("/api/v1/signin/{signin_id}/code"),
            Some(json!({"code": format!("c#{state}")})),
        )
        .await;
        assert_eq!(r.status, 200, "{}", r.text());
        let done = wait_signin(&srv, &signin_id, "pending").await;
        assert_eq!(done["status"], "needs_binding", "{done}");
        assert_eq!(done["got"]["email"], "me@example.com");
        assert!(
            pool.reg.read_creds(&id("fresh")).unwrap().is_none(),
            "nothing saved yet"
        );

        let r = call(
            &srv,
            "POST",
            &format!("/api/v1/signin/{signin_id}/bind"),
            None,
        )
        .await;
        assert_eq!(r.status, 200, "{}", r.text());
        assert_eq!(r.json()["identity"]["email"], "me@example.com");
        assert!(pool.reg.read_creds(&id("fresh")).unwrap().is_some());
        let meta = pool.reg.get(&id("fresh")).unwrap();
        assert_eq!(meta.identity.account_uuid.as_deref(), Some(UUID));
        let v = http_get(srv.addr, &format!("/api/v1/signin/{signin_id}"), &[])
            .await
            .json();
        assert_eq!(v["status"], "saved");

        // Binding twice, or an unknown flow, is refused.
        let r = call(
            &srv,
            "POST",
            &format!("/api/v1/signin/{signin_id}/bind"),
            None,
        )
        .await;
        assert_eq!(r.status, 409);
        let r = call(&srv, "POST", "/api/v1/signin/nope/bind", None).await;
        assert_eq!(r.status, 404);
        let r = call(
            &srv,
            "POST",
            "/api/v1/signin/nope/code",
            Some(json!({"code": "a#b"})),
        )
        .await;
        assert_eq!(r.status, 404);
    }

    #[tokio::test]
    async fn login_mismatch_reports_both_identities_and_saves_nothing() {
        let fake = oauth_fake("bbbbbbbb-0000-0000-0000-000000000002").await;
        let pool = Pool::new();
        with_baseline(&pool, UUID, "owner@example.com");
        let before = pool.creds();
        let srv = pool.boot(boot_against(&fake)).await;

        let r = call(
            &srv,
            "POST",
            "/api/v1/accounts/work/login",
            Some(json!({"mode": "paste"})),
        )
        .await;
        let v = r.json();
        let signin_id = v["signin_id"].as_str().unwrap().to_string();
        let state = param(v["authorize_url"].as_str().unwrap(), "state").unwrap();
        call(
            &srv,
            "POST",
            &format!("/api/v1/signin/{signin_id}/code"),
            Some(json!({"code": format!("c#{state}")})),
        )
        .await;
        let done = wait_signin(&srv, &signin_id, "pending").await;
        assert_eq!(done["status"], "mismatch", "{done}");
        assert_eq!(done["expected"]["email"], "owner@example.com");
        assert_eq!(done["got"]["email"], "me@example.com");
        let msg = done["message"].as_str().unwrap();
        assert!(
            msg.contains("owner@example.com") && msg.contains("me@example.com"),
            "{msg}"
        );
        assert_eq!(pool.creds(), before, "credentials untouched");
    }

    #[tokio::test]
    async fn login_validation() {
        let pool = Pool::new();
        let srv = pool.boot(Boot::default()).await;

        let r = call(
            &srv,
            "POST",
            "/api/v1/accounts/work/login",
            Some(json!({"mode": "telepathy"})),
        )
        .await;
        assert_eq!(r.status, 400);
        assert!(message(&r).contains("mode"), "{}", message(&r));
        let r = call(
            &srv,
            "POST",
            "/api/v1/accounts/work/login",
            Some(json!({"mode": "paste", "provider": "nope"})),
        )
        .await;
        assert_eq!(r.status, 400);
        let r = call(
            &srv,
            "POST",
            "/api/v1/accounts/work/login",
            Some(json!({"mode": "paste", "provider": "openai"})),
        )
        .await;
        assert_eq!(
            r.status, 400,
            "provider disagrees with the existing account"
        );
        let r = call(
            &srv,
            "POST",
            "/api/v1/accounts/bad%2Fid/login",
            Some(json!({"mode": "paste"})),
        )
        .await;
        assert_eq!(r.status, 400);

        // `mode` defaults to the callback listener: the URL carries a localhost redirect.
        let r = call(&srv, "POST", "/api/v1/accounts/work/login", Some(json!({}))).await;
        assert_eq!(r.status, 200, "{}", r.text());
        let url = r.json()["authorize_url"].as_str().unwrap().to_string();
        assert!(
            param(&url, "redirect_uri")
                .unwrap()
                .starts_with("http://localhost:"),
            "{url}"
        );
    }

    #[tokio::test]
    async fn openai_login_routes_to_the_chatgpt_flow() {
        let pool = Pool::new();
        // Never bind the real Codex ports (1455/1457) in tests.
        let srv = pool
            .boot(Boot {
                configure: Some(Box::new(|cfg: &mut Config| {
                    cfg.openai.callback_ports = vec![0];
                })),
                ..Boot::default()
            })
            .await;

        // A new account created by its first login, and an existing one.
        pool.add_account("codex1", ProviderId::OpenAi, 100, "x");
        let mut ids = Vec::new();
        for (path, body) in [
            (
                "/api/v1/accounts/oa/login",
                json!({"mode": "callback", "provider": "openai"}),
            ),
            ("/api/v1/accounts/codex1/login", json!({"mode": "paste"})),
        ] {
            let r = call(&srv, "POST", path, Some(body)).await;
            assert_eq!(r.status, 200, "{path}: {}", r.text());
            let v = r.json();
            let url = v["authorize_url"].as_str().unwrap();
            assert!(url.starts_with(&srv.app.cfg.openai.authorize_url), "{path}");
            assert_eq!(
                param(url, "client_id").as_deref(),
                Some("app_EMoamEEZ73f0CkXaXp7hrann")
            );
            let redirect = reqwest::Url::parse(&param(url, "redirect_uri").unwrap()).unwrap();
            assert_eq!(redirect.host_str(), Some("localhost"));
            assert_eq!(redirect.path(), "/auth/callback");
            ids.push(v["signin_id"].as_str().unwrap().to_string());
        }
        assert_eq!(
            pool.reg.get(&id("oa")).unwrap().provider,
            ProviderId::OpenAi
        );
        // The second ChatGPT sign-in superseded the first (the callback ports are fixed).
        let first = wait_signin(&srv, &ids[0], "pending").await;
        assert_eq!(first["status"], "failed");
        assert!(
            first["message"].as_str().unwrap().contains("superseded"),
            "{first}"
        );
    }

    #[tokio::test]
    async fn delete_account_cancels_pending_signin() {
        let fake = oauth_fake(UUID).await;
        let pool = Pool::new();
        with_baseline(&pool, UUID, "me@example.com");
        let srv = pool.boot(boot_against(&fake)).await;
        let r = call(
            &srv,
            "POST",
            "/api/v1/accounts/work/login",
            Some(json!({"mode": "callback"})),
        )
        .await;
        assert_eq!(r.status, 200, "{}", r.text());
        let v = r.json();
        let signin_id = v["signin_id"].as_str().unwrap().to_string();
        let url = v["authorize_url"].as_str().unwrap().to_string();
        let state = param(&url, "state").unwrap();
        let port = reqwest::Url::parse(&param(&url, "redirect_uri").unwrap())
            .unwrap()
            .port()
            .unwrap();

        let r = call(&srv, "DELETE", "/api/v1/accounts/work", None).await;
        assert_eq!(r.status, 200, "{}", r.text());
        let done = wait_signin(&srv, &signin_id, "pending").await;
        assert_eq!(done["status"], "failed", "{done}");

        // The browser comes back late: nothing is exchanged, nothing recreated.
        let late = reqwest::get(format!(
            "http://127.0.0.1:{port}/callback?code=late-code&state={state}"
        ))
        .await;
        assert!(late.map_or(true, |r| r.status() != 200));
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(fake.requests_to(TOKEN).is_empty());
        let dir = pool.dir.path().join("accounts").join("work");
        assert!(!dir.exists(), "account dir recreated");
        assert!(pool.reg.get(&id("work")).is_none());
    }

    #[tokio::test]
    async fn openai_login_with_both_ports_busy_is_409() {
        let held = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let held_port = held.local_addr().unwrap().port();
        let pool = Pool::new();
        let srv = pool
            .boot(Boot {
                configure: Some(Box::new(move |cfg: &mut Config| {
                    cfg.openai.callback_ports = vec![held_port, held_port];
                })),
                ..Boot::default()
            })
            .await;
        pool.add_account("codex1", ProviderId::OpenAi, 100, "x");
        let r = call(
            &srv,
            "POST",
            "/api/v1/accounts/codex1/login",
            Some(json!({"mode": "callback"})),
        )
        .await;
        assert_eq!(r.status, 409, "{}", r.text());
        assert!(message(&r).contains("in use"), "{}", message(&r));
        drop(held);
    }

    // ---- profiles & models (Task 30) ----

    fn settings(dir: &std::path::Path) -> Value {
        serde_json::from_slice(&std::fs::read(dir.join("settings.json")).unwrap()).unwrap()
    }

    #[tokio::test]
    async fn profiles_connect_disconnect_endpoints() {
        let pool = Pool::new();
        let srv = pool.boot(Boot::default()).await;
        // A profile under a stand-in home (addressed by absolute path: never the real `$HOME`).
        let home = tempfile::tempdir().unwrap();
        let dir = home.path().join(".claude-x");
        std::fs::create_dir_all(&dir).unwrap();
        let original = json!({"env": {"KEEP": "1"}});
        std::fs::write(dir.join("settings.json"), original.to_string()).unwrap();
        let path = dir.to_str().unwrap().to_string();

        for p in [
            "/api/v1/profiles/connect",
            "/api/v1/profiles/disconnect",
            "/api/v1/models/refresh",
        ] {
            let r = raw(srv.addr, "POST", p, Some(json!({"profiles": [path]})), &[]).await;
            assert_eq!(r.status, 401, "{p} needs the management key");
            let r = call(&srv, "GET", p, None).await;
            assert_eq!(r.status, 405, "{p}");
        }
        assert_eq!(settings(&dir), original);

        // A dir without settings.json is not a Claude Code profile.
        let bare = home.path().join(".claude-bare");
        std::fs::create_dir_all(&bare).unwrap();

        // Nothing named, a relative path, a missing dir, a dir without settings.json, a
        // non-string: refused, nothing written.
        for body in [
            json!({}),
            json!({"profiles": []}),
            json!({"profiles": ["relative/dir"]}),
            json!({"profiles": [home.path().join("missing").to_str().unwrap()]}),
            json!({"profiles": [bare.to_str().unwrap()]}),
            json!({"profiles": [7]}),
            json!({"profiles": "x"}),
            json!({"profiles": [path], "statusline": "yes"}),
        ] {
            let r = call(&srv, "POST", "/api/v1/profiles/connect", Some(body.clone())).await;
            assert_eq!(r.status, 400, "{body}: {}", r.text());
        }
        assert_eq!(settings(&dir), original);

        let r = call(
            &srv,
            "POST",
            "/api/v1/profiles/connect",
            Some(json!({"profiles": [path], "statusline": true})),
        )
        .await;
        assert_eq!(r.status, 200, "{}", r.text());
        let v = r.json();
        assert_eq!(v["profiles"][0]["id"], "claude-x");
        // The wrapper names this (test) binary; disconnect removes it again (none before).
        let line = settings(&dir)["statusLine"].clone();
        assert_eq!(line["type"], "command");
        let cmd = line["command"].as_str().unwrap();
        assert!(cmd.ends_with(" statusline --wrap claude-x"), "{cmd}");
        assert_eq!(v["profiles"][0]["settings_dir"], path);
        assert_eq!(v["profiles"][0]["outcome"], "written");
        let s = settings(&dir);
        assert_eq!(
            s["env"]["ANTHROPIC_BASE_URL"],
            format!("http://{}", srv.addr)
        );
        assert_eq!(s["env"]["KEEP"], "1");
        let cfg = Config::load_file(&Paths::under(pool.dir.path())).unwrap();
        assert_eq!(cfg.profiles.len(), 1);
        assert_eq!(cfg.profiles[0].id, "claude-x");

        // Disconnect by id (resolved from the config).
        let r = call(
            &srv,
            "POST",
            "/api/v1/profiles/disconnect",
            Some(json!({"profiles": ["claude-x"]})),
        )
        .await;
        assert_eq!(r.status, 200, "{}", r.text());
        assert_eq!(r.json()["profiles"][0]["outcome"], "written");
        assert_eq!(settings(&dir), original);
        let cfg = Config::load_file(&Paths::under(pool.dir.path())).unwrap();
        assert!(cfg.profiles.is_empty());

        // No names = every connected profile (none now).
        let r = call(&srv, "POST", "/api/v1/profiles/disconnect", Some(json!({}))).await;
        assert_eq!(r.status, 200, "{}", r.text());
        assert_eq!(r.json()["profiles"], json!([]));
        assert!(!bare.join("settings.json").exists());
    }

    #[tokio::test]
    async fn equal_ids_refused_on_connect_reported_on_disconnect() {
        let pool = Pool::new();
        let srv = pool.boot(Boot::default()).await;
        let home = tempfile::tempdir().unwrap();
        let dirs: Vec<std::path::PathBuf> = ["a", "b"]
            .iter()
            .map(|p| home.path().join(p).join(".claude-x"))
            .collect();
        for d in &dirs {
            std::fs::create_dir_all(d).unwrap();
            std::fs::write(d.join("settings.json"), "{}").unwrap();
        }
        let names: Vec<&str> = dirs.iter().map(|d| d.to_str().unwrap()).collect();
        let r = call(
            &srv,
            "POST",
            "/api/v1/profiles/connect",
            Some(json!({ "profiles": names })),
        )
        .await;
        // One id for two dirs would share a snapshot and a config entry.
        assert_eq!(r.status, 400, "{}", r.text());
        assert!(message(&r).contains("same profile id"), "{}", r.text());
        for d in &dirs {
            let settings = std::fs::read_to_string(d.join("settings.json")).unwrap();
            assert_eq!(settings, "{}", "nothing connected");
        }
        // Disconnect still reports each dir (a pair connected before the rule can be released).
        let r = call(
            &srv,
            "POST",
            "/api/v1/profiles/disconnect",
            Some(json!({ "profiles": names })),
        )
        .await;
        assert_eq!(r.status, 200, "{}", r.text());
        let v = r.json();
        assert_eq!(v["profiles"][0]["settings_dir"], names[0]);
        assert_eq!(v["profiles"][1]["settings_dir"], names[1]);
    }

    #[tokio::test]
    async fn account_create_with_email_sets_expected_identity() {
        let pool = Pool::new();
        let srv = pool.boot(Boot::default()).await;
        let r = call(
            &srv,
            "POST",
            "/api/v1/accounts",
            Some(json!({"id": "mail", "provider": "anthropic", "email": " you@example.com "})),
        )
        .await;
        assert_eq!(r.status, 201, "{}", r.text());
        assert_eq!(r.json()["email"], "you@example.com");
        let meta = pool.reg.get(&id("mail")).unwrap();
        assert_eq!(meta.identity.email.as_deref(), Some("you@example.com"));
        assert_eq!(meta.identity.account_uuid, None);
        for bad in [json!(7), json!(""), json!("no-at-sign")] {
            let r = call(
                &srv,
                "POST",
                "/api/v1/accounts",
                Some(json!({"id": "mail2", "provider": "anthropic", "email": bad})),
            )
            .await;
            assert_eq!(r.status, 400, "{bad}: {}", r.text());
            assert!(message(&r).contains("email"), "{}", message(&r));
        }
        assert!(pool.reg.get(&id("mail2")).is_none());
    }

    #[tokio::test]
    async fn models_refresh_refetches_live_catalog() {
        let fake = FakeAnthropic::start().await;
        let fail = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let f = fail.clone();
        fake.route("/v1/models", move |_, i| {
            if f.load(Ordering::SeqCst) {
                return Reply::error(500, "api_error", "boom");
            }
            Reply::json(
                200,
                &json!({"data": [{"id": format!("claude-live-{i}"), "display_name": "Live",
                    "created_at": "2026-06-01T00:00:00Z"}], "has_more": false}),
            )
        });
        let pool = Pool::new();
        let (policy, _) = super::support::fast_policy();
        let srv = pool
            .boot(Boot::default().provider(Arc::new(pool.provider(&fake, policy))))
            .await;
        let ids = |v: Value| -> Vec<String> {
            v["data"]
                .as_array()
                .unwrap()
                .iter()
                .map(|m| m["id"].as_str().unwrap().to_string())
                .collect()
        };
        assert!(ids(http_get(srv.addr, "/v1/models", &[]).await.json())
            .contains(&"claude-live-0".to_string()));
        // Cached: a second listing does not re-fetch.
        assert!(ids(http_get(srv.addr, "/v1/models", &[]).await.json())
            .contains(&"claude-live-0".to_string()));
        assert_eq!(fake.requests_to("/v1/models").len(), 1);

        let r = call(&srv, "POST", "/api/v1/models/refresh", None).await;
        assert_eq!(r.status, 200, "{}", r.text());
        let v = r.json();
        assert_eq!(v["ok"], true);
        assert_eq!(v["live"], 1);
        assert!(v["models"]
            .as_array()
            .unwrap()
            .contains(&json!("claude-live-1")));
        assert!(ids(http_get(srv.addr, "/v1/models", &[]).await.json())
            .contains(&"claude-live-1".to_string()));
        assert!(v.get("sync_error").is_none(), "{v}");

        // The profile sync failing (unreadable config.toml) is reported, without content.
        let cfg_path = pool.dir.path().join("config.toml");
        let saved_cfg = std::fs::read(&cfg_path).ok();
        std::fs::write(&cfg_path, "this is = = not toml SECRETCONTENT").unwrap();
        let r = call(&srv, "POST", "/api/v1/models/refresh", None).await;
        assert_eq!(r.status, 200, "{}", r.text());
        assert_eq!(r.json()["sync_error"], "profile_sync_failed");
        assert!(!r.text().contains("SECRETCONTENT"), "{}", r.text());
        match saved_cfg {
            Some(b) => std::fs::write(&cfg_path, b).unwrap(),
            None => std::fs::remove_file(&cfg_path).unwrap(),
        }

        // A failed refresh is an error, and the last good list keeps being served.
        fail.store(true, Ordering::SeqCst);
        let r = call(&srv, "POST", "/api/v1/models/refresh", None).await;
        assert_eq!(r.status, 502, "{}", r.text());
        assert!(!r.text().contains("boom"), "no upstream text: {}", r.text());
        // (The sync-error refresh above fetched `claude-live-2`, the last good list.)
        assert!(ids(http_get(srv.addr, "/v1/models", &[]).await.json())
            .contains(&"claude-live-2".to_string()));

        // No Anthropic provider registered: nothing to refresh.
        let pool = Pool::new();
        let srv = pool.boot(Boot::default()).await;
        let r = call(&srv, "POST", "/api/v1/models/refresh", None).await;
        assert_eq!(r.status, 503, "{}", r.text());
    }

    // ---- dashboard ----

    const APP_JS: &str = include_str!("../web/app.js");
    const STYLE_CSS: &str = include_str!("../web/style.css");

    /// Every dashboard response carries the CSP and the referrer/sniffing guards.
    fn assert_dashboard_headers(r: &HttpResp) {
        let csp = r.header("content-security-policy").expect("CSP header");
        assert!(csp.contains("default-src 'self'"), "{csp}");
        assert!(!csp.contains("unsafe-inline"), "{csp}");
        assert!(!csp.contains("unsafe-eval"), "{csp}");
        assert!(csp.contains("frame-ancestors 'none'"), "{csp}");
        assert_eq!(r.header("referrer-policy"), Some("no-referrer"));
        assert_eq!(r.header("x-content-type-options"), Some("nosniff"));
    }

    #[tokio::test]
    async fn index_served_with_csp_header() {
        let pool = Pool::new();
        let srv = pool.boot(Boot::default()).await;
        let r = http_get(srv.addr, "/", &[]).await;
        assert_eq!(r.status, 200, "{}", r.text());
        assert!(r.header("content-type").unwrap().starts_with("text/html"));
        assert_dashboard_headers(&r);
        // The page depends on the session cookie, so it is never cached.
        assert_eq!(r.header("cache-control"), Some("no-store"));
        let html = r.text();
        assert!(html.contains(r#"<script src="/assets/app.js"#), "{html}");
        assert!(html.contains(r#"href="/assets/style.css"#), "{html}");
        // Nothing inline: no script bodies, no style attributes or blocks, no remote URLs.
        assert!(!html.contains("<script>"), "{html}");
        assert!(!html.contains("<style"), "{html}");
        assert!(!html.contains("style="), "{html}");
        assert!(!html.contains("https://"), "{html}");
        // Wrong verb: 405 like every other route.
        let r = http_req(srv.addr, "POST", "/", &[], b"").await;
        assert_eq!(r.status, 405);
        // DNS rebinding: a foreign Host never gets the page.
        let r = http_get(srv.addr, "/", &[("host", "evil.example:80")]).await;
        assert_eq!(r.status, 403);
    }

    #[tokio::test]
    async fn assets_embedded_and_cached() {
        let pool = Pool::new();
        let srv = pool.boot(Boot::default()).await;
        for (path, body, mime) in [
            ("/assets/app.js", APP_JS, "text/javascript"),
            ("/assets/style.css", STYLE_CSS, "text/css"),
        ] {
            let r = http_get(srv.addr, path, &[]).await;
            assert_eq!(r.status, 200, "{path}");
            assert!(
                r.header("content-type").unwrap().starts_with(mime),
                "{path}"
            );
            assert_eq!(r.text(), body, "{path} is the embedded file");
            assert_dashboard_headers(&r);
            // ETag revalidation: cached copies are checked every load, so an upgraded binary is
            // picked up at once and an unchanged one costs a 304.
            assert_eq!(r.header("cache-control"), Some("no-cache"), "{path}");
            let etag = r.header("etag").expect("etag").to_string();
            assert!(etag.starts_with('"') && etag.ends_with('"'), "{etag}");
            let again = http_get(srv.addr, path, &[]).await;
            assert_eq!(again.header("etag"), Some(etag.as_str()), "stable etag");

            let r = http_get(srv.addr, path, &[("if-none-match", &etag)]).await;
            assert_eq!(r.status, 304, "{path}");
            assert!(r.body.is_empty());
            assert_eq!(r.header("etag"), Some(etag.as_str()));
            let r = http_get(srv.addr, path, &[("if-none-match", "\"stale\"")]).await;
            assert_eq!(r.status, 200, "{path}");
        }
        assert_ne!(
            http_get(srv.addr, "/assets/app.js", &[])
                .await
                .header("etag"),
            http_get(srv.addr, "/assets/style.css", &[])
                .await
                .header("etag"),
        );
        let r = http_get(srv.addr, "/assets/nope.js", &[]).await;
        assert_eq!(r.status, 404);
        let r = http_get(srv.addr, "/assets/app.js", &[("host", "evil.example:80")]).await;
        assert_eq!(r.status, 403);
    }

    #[tokio::test]
    async fn query_key_exchanges_for_cookie_and_redirects() {
        let pool = Pool::new();
        let srv = pool.boot(Boot::default()).await;
        let r = http_get(srv.addr, &format!("/?k={KEY}"), &[]).await;
        assert_eq!(r.status, 303, "{}", r.text());
        // The key leaves the address bar (and history) at once.
        assert_eq!(r.header("location"), Some("/"));
        assert_eq!(r.header("cache-control"), Some("no-store"));
        assert_eq!(r.header("referrer-policy"), Some("no-referrer"));
        let set = r.header("set-cookie").expect("set-cookie").to_string();
        assert!(
            set.starts_with(&format!("{SESSION_COOKIE}={};", session_token(KEY))),
            "{set}"
        );
        assert!(
            set.contains("HttpOnly") && set.contains("SameSite=Strict") && set.contains("Path=/"),
            "{set}"
        );
        assert!(!set.contains(KEY), "{set}");
        assert!(!r.text().contains(KEY));

        // The cookie opens the managing view and authorizes mutations.
        let pair = set.split(';').next().unwrap().to_string();
        let r = http_get(srv.addr, "/", &[("cookie", &pair)]).await;
        assert_eq!(r.status, 200);
        assert!(r.text().contains(r#"data-mode="manage""#), "{}", r.text());
        let r = raw(
            srv.addr,
            "POST",
            "/api/v1/tuning",
            Some(json!({"min_headroom": 0.05})),
            &[
                ("cookie", &pair),
                ("origin", &format!("http://{}", srv.addr)),
            ],
        )
        .await;
        assert_eq!(r.status, 200, "{}", r.text());

        // Percent-encoding is decoded; other parameters do not matter.
        let r = http_get(srv.addr, &format!("/?x=1&k={KEY}"), &[]).await;
        assert_eq!(r.status, 303);
        assert!(r.header("set-cookie").is_some());

        // A foreign Host cannot trade the key for a cookie.
        let r = http_get(
            srv.addr,
            &format!("/?k={KEY}"),
            &[("host", "evil.example:80")],
        )
        .await;
        assert_eq!(r.status, 403);
        assert!(r.header("set-cookie").is_none());
    }

    #[tokio::test]
    async fn index_without_session_is_read_only() {
        let pool = Pool::new();
        let srv = pool.boot(Boot::default()).await;
        for path in ["/", "/?k=wrong", "/?k=", "/?k=%zz"] {
            let r = http_get(srv.addr, path, &[]).await;
            assert_eq!(r.status, 200, "{path}");
            assert!(r.header("set-cookie").is_none(), "{path}");
            let html = r.text();
            assert!(html.contains(r#"data-mode="read-only""#), "{path}: {html}");
            assert!(html.contains("pooltime open"), "{path}: hint missing");
            assert!(!html.contains("wrong"), "the rejected key is not echoed");
        }
        // A stale or forged cookie is read-only too.
        let r = http_get(
            srv.addr,
            "/",
            &[(
                "cookie",
                &format!("{SESSION_COOKIE}={}", session_token("other")),
            )],
        )
        .await;
        assert!(r.text().contains(r#"data-mode="read-only""#));
        // The bearer key also counts (CLI-style clients).
        let r = http_get(
            srv.addr,
            "/",
            &[("authorization", &format!("Bearer {KEY}"))],
        )
        .await;
        assert!(r.text().contains(r#"data-mode="manage""#));
    }

    #[tokio::test]
    async fn status_reports_profiles_and_storage_only_when_authorized() {
        let pool = Pool::new();
        let srv = pool.boot(Boot::default()).await;
        // Unauthenticated reads keep the pool view but not the local paths.
        let v = http_get(srv.addr, "/api/v1/status", &[]).await.json();
        assert!(v.get("storage").is_none(), "{v}");
        assert!(v.get("profiles").is_none(), "{v}");
        assert!(v["accounts"].is_array(), "{v}");
        let bearer = format!("Bearer {KEY}");
        let cookie = format!("{SESSION_COOKIE}={}", session_token(KEY));
        for hdr in [("authorization", bearer), ("cookie", cookie)] {
            let v = http_get(srv.addr, "/api/v1/status", &[(hdr.0, &hdr.1)])
                .await
                .json();
            assert_eq!(v["profiles"], json!([]), "{}", hdr.0);
            assert_eq!(
                v["storage"]["home"].as_str(),
                Some(pool.dir.path().to_str().unwrap())
            );
            assert!(v["storage"]["db_bytes"].as_u64().unwrap() > 0, "{v}");
            assert!(v["storage"]["logs_bytes"].as_u64().is_some(), "{v}");
        }
    }

    #[tokio::test]
    async fn status_profiles_null_when_config_unreadable() {
        let pool = Pool::new();
        let srv = pool.boot(Boot::default()).await;
        std::fs::write(
            Paths::under(pool.dir.path()).config,
            "this is = not [valid toml",
        )
        .unwrap();
        let v = http_get(
            srv.addr,
            "/api/v1/status",
            &[("authorization", &format!("Bearer {KEY}"))],
        )
        .await
        .json();
        assert!(v["profiles"].is_null(), "{v}");
        assert!(v.get("profiles").is_some(), "{v}");
        assert!(v["storage"].is_object(), "{v}");
    }

    #[tokio::test]
    async fn fixture_state_replaces_status() {
        let raw_fixture = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/support/fixtures/dashboard.json"
        ))
        .unwrap();
        let fixture: Value = serde_json::from_str(&raw_fixture).unwrap();
        // The fixture covers every status word, and a dead account still carrying a stale
        // snapshot (the dashboard must show "—" for it, not leftover headroom).
        let accounts = fixture["accounts"].as_array().unwrap();
        for word in [
            "ready",
            "needs_login",
            "rate_limited",
            "billing",
            "access_denied",
        ] {
            assert!(accounts.iter().any(|a| a["status"] == word), "{word}");
        }
        assert!(accounts
            .iter()
            .any(|a| a["status"] == "needs_login" && !a["windows"].as_array().unwrap().is_empty()));

        let pool = Pool::new();
        let srv = pool
            .boot(Boot {
                fixture_state: Some(fixture.clone()),
                ..Boot::default()
            })
            .await;
        let r = http_get(srv.addr, "/api/v1/status", &[]).await;
        assert_eq!(r.status, 200);
        assert_eq!(r.json(), fixture);
        assert_eq!(r.header("cache-control"), Some("no-store"));
        // Without the hook the live state is served.
        let live = Pool::new();
        let srv = live.boot(Boot::default()).await;
        let v = http_get(srv.addr, "/api/v1/status", &[]).await.json();
        assert_eq!(v["accounts"][0]["id"], "work");
    }

    // ---- health states (F1) ----

    /// Give `name` a recorded refresh-token expiry (`None` = unknown).
    fn set_refresh_expiry(pool: &Pool, name: &str, at: Option<OffsetDateTime>) {
        let id = id(name);
        let mut c = pool.reg.read_creds(&id).unwrap().unwrap();
        c.refresh_expires_at = at;
        pool.reg.write_creds(&id, &c).unwrap();
    }

    fn account<'a>(v: &'a Value, name: &str) -> &'a Value {
        v["accounts"]
            .as_array()
            .unwrap()
            .iter()
            .find(|a| a["id"] == name)
            .unwrap_or_else(|| panic!("no account {name} in {v}"))
    }

    #[tokio::test]
    async fn banner_counts() {
        let pool = Pool::new();
        let now = OffsetDateTime::now_utc();
        // work: expires within 24 h; soon: in 2 days; later: in 30 days; unknown: no expiry.
        set_refresh_expiry(&pool, "work", Some(now + time::Duration::hours(12)));
        for name in ["soon", "later", "unknown", "dead", "flaky"] {
            pool.add_account(name, ProviderId::Anthropic, 0, "at");
        }
        set_refresh_expiry(&pool, "soon", Some(now + time::Duration::days(2)));
        set_refresh_expiry(&pool, "later", Some(now + time::Duration::days(30)));
        // A dead account whose clock also says "today": counted once, as needs_login.
        set_refresh_expiry(&pool, "dead", Some(now + time::Duration::hours(1)));
        pool.state.mark_refresh_dead(&id("dead"), "rt-dead", now);
        for s in 0..3 {
            pool.state.record_error(
                &id("flaky"),
                ErrorRecord {
                    at: now + time::Duration::seconds(s),
                    reason: FailReason::Refresh(RefreshErrorKind::Server(502)),
                    upstream_request_id: None,
                },
            );
        }
        let srv = pool.boot(Boot::default()).await;
        let v = http_get(srv.addr, "/api/v1/status", &[]).await.json();

        assert_eq!(
            v["counts"],
            json!({"needs_login": 1, "expiring_today": 1, "expiring": 2}),
            "{v}"
        );
        // The warn window behind `expiring` (alerts.warn_days[0] = 3 days), for the dashboard.
        assert_eq!(v["expiry_warn_secs"], 3 * 86_400, "{v}");
        let work = account(&v, "work");
        assert_eq!(work["status"], "expiring", "{work}");
        assert_eq!(work["until"], work["refresh_expires_at"], "{work}");
        assert_eq!(work["reason"], "refresh_token_expiring", "{work}");
        assert!(work["status_since"].is_string(), "{work}");
        assert_eq!(account(&v, "soon")["status"], "expiring");
        assert_eq!(account(&v, "later")["status"], "ready");
        assert_eq!(account(&v, "unknown")["status"], "ready");
        assert_eq!(account(&v, "dead")["status"], "needs_login");
        let flaky = account(&v, "flaky");
        assert_eq!(flaky["status"], "erroring", "{flaky}");
        assert_eq!(flaky["class"], "refresh:server:502", "{flaky}");
        assert_eq!(flaky["count"], 3, "{flaky}");
        assert_eq!(flaky["reason"], "refresh:server:502", "{flaky}");
        // Expiring and erroring accounts still route: they count as available.
        assert_eq!(v["pool"]["available"], 5, "{v}");
    }

    #[tokio::test]
    async fn probe_confirms_needs_login_only_on_invalid_grant() {
        use pooltime::health::{ExpiryProbe, ProbeOutcome};

        /// A pool whose `work` login expired a minute ago, refreshed against `fake`.
        async fn expired(fake: &FakeAnthropic) -> (Pool, Booted, OffsetDateTime) {
            let pool = Pool::new();
            let expiry = OffsetDateTime::now_utc() - time::Duration::minutes(1);
            set_refresh_expiry(&pool, "work", Some(expiry));
            let (policy, _) = super::support::fast_policy();
            let provider = Arc::new(pool.provider(fake, policy));
            let srv = pool.boot(Boot::default().provider(provider)).await;
            (pool, srv, expiry)
        }

        // A transient failure (5xx) is erroring, never needs_login.
        let fake = FakeAnthropic::start().await;
        fake.route(TOKEN, |_, _| Reply::new(503));
        let (_pool, srv, expiry) = expired(&fake).await;
        let probe = ExpiryProbe::default();
        // Before the expiry nothing is probed.
        let early = probe
            .tick(&srv.app, expiry - time::Duration::seconds(1))
            .await;
        assert!(early.is_empty(), "{early:?}");
        assert!(fake.requests_to(TOKEN).is_empty());
        let now = OffsetDateTime::now_utc();
        let out = probe.tick(&srv.app, now).await;
        assert_eq!(
            out,
            [(
                id("work"),
                ProbeOutcome::Erroring("refresh:server:503".into())
            )]
        );
        assert_eq!(fake.requests_to(TOKEN).len(), 1);
        let a = work_account(&srv).await;
        assert_eq!(a["status"], "erroring", "{a}");
        assert_eq!(a["class"], "refresh:server:503", "{a}");
        // Unconfirmed: no re-probe until the 15-minute retry is due, then exactly one more POST.
        assert!(probe.tick(&srv.app, now).await.is_empty());
        let almost = now + time::Duration::minutes(15) - time::Duration::seconds(1);
        assert!(probe.tick(&srv.app, almost).await.is_empty());
        assert_eq!(fake.requests_to(TOKEN).len(), 1);
        let retry = now + time::Duration::minutes(15);
        assert_eq!(
            probe.tick(&srv.app, retry).await,
            [(
                id("work"),
                ProbeOutcome::Erroring("refresh:server:503".into())
            )]
        );
        assert_eq!(fake.requests_to(TOKEN).len(), 2);
        assert!(probe.tick(&srv.app, retry).await.is_empty());
        assert_eq!(fake.requests_to(TOKEN).len(), 2);

        // account_on_hold is billing, not a probe failure, and is not retried.
        let fake = FakeAnthropic::start().await;
        fake.route(TOKEN, |_, _| {
            Reply::json(400, &json!({"error": {"type": "account_on_hold"}}))
        });
        let (_pool, srv, _) = expired(&fake).await;
        let probe = ExpiryProbe::default();
        let now = OffsetDateTime::now_utc();
        assert_eq!(
            probe.tick(&srv.app, now).await,
            [(id("work"), ProbeOutcome::Billing)]
        );
        let a = work_account(&srv).await;
        assert_eq!(a["status"], "billing", "{a}");
        assert!(srv.app.state.erroring(&id("work"), now).is_none());
        let later = now + time::Duration::minutes(30);
        assert!(probe.tick(&srv.app, later).await.is_empty());
        assert_eq!(fake.requests_to(TOKEN).len(), 1);

        // invalid_grant confirms the dead login.
        let fake = FakeAnthropic::start().await;
        fake.route(TOKEN, |_, _| {
            Reply::json(400, &json!({"error": "invalid_grant"}))
        });
        let (_pool, srv, _) = expired(&fake).await;
        let probe = ExpiryProbe::default();
        let out = probe.tick(&srv.app, OffsetDateTime::now_utc()).await;
        assert_eq!(out, [(id("work"), ProbeOutcome::NeedsLogin)]);
        let a = work_account(&srv).await;
        assert_eq!(a["status"], "needs_login", "{a}");
        assert_eq!(a["reason"], "refresh_token_dead", "{a}");

        // A refresh that works disproves the passed expiry: it is replaced (unknown here, the
        // server sent none), the account is ready, and it is not probed again.
        let fake = FakeAnthropic::start().await;
        fake.route(TOKEN, |_, _| {
            Reply::json(
                200,
                &json!({"access_token": "at-new", "refresh_token": "rt-new", "expires_in": 28800}),
            )
        });
        let (_pool, srv, _) = expired(&fake).await;
        let probe = ExpiryProbe::default();
        let now = OffsetDateTime::now_utc();
        assert_eq!(
            probe.tick(&srv.app, now).await,
            [(id("work"), ProbeOutcome::Refreshed)]
        );
        let a = work_account(&srv).await;
        assert_eq!(a["status"], "ready", "{a}");
        assert_eq!(a["refresh_expires_at"], Value::Null, "{a}");
        assert!(probe.tick(&srv.app, now).await.is_empty());
        let later = now + time::Duration::hours(1);
        assert!(probe.tick(&srv.app, later).await.is_empty());
        assert_eq!(fake.requests_to(TOKEN).len(), 1);
    }
}
