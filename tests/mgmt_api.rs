//! Task 21: the HTTP server's identity, auth, routing fallbacks, Host/Origin guard, drain and
//! startup preflight, over real sockets.

mod support;

use std::time::Duration;

use bytes::Bytes;
use pooltime::server::health::{check_bro_shadow, preflight, StartupError};
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
