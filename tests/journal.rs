//! Task F4: the request journal, its management API, debug mode and journal retention.

mod support;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use pooltime::config::RetentionCfg;
use pooltime::logging::DebugMode;
use pooltime::providers::{Provider, ProviderId};
use pooltime::server::inference::REQUEST_ID_HEADER;
use pooltime::store::journal::{self, AttemptRow, RequestRow};
use pooltime::store::{prune, Store};
use pooltime::upstream::Upstream;
use serde_json::{json, Value};
use support::sockets::{contains, http_req, post_json, raw_client, HttpResp};
use support::*;
use time::OffsetDateTime;

const MGMT: (&str, &str) = ("authorization", "Bearer mgmt-test-key");

fn anthropic(pool: &Pool, fake: &FakeAnthropic) -> Arc<dyn Provider> {
    let (policy, _) = fast_policy();
    Arc::new(pool.provider(fake, policy))
}

fn ok_message(input: u64, output: u64) -> Value {
    json!({
        "id": "msg_1", "type": "message", "role": "assistant", "model": "claude-sonnet-5",
        "content": [{"type": "text", "text": "hello"}],
        "stop_reason": "end_turn",
        "usage": {"input_tokens": input, "output_tokens": output,
                  "cache_read_input_tokens": 5, "cache_creation_input_tokens": 6}
    })
}

fn sse(name: &str, data: Value) -> String {
    format!("event: {name}\ndata: {data}\n\n")
}

fn stream_frames(text: &str) -> Vec<(Duration, String)> {
    [
        sse(
            "message_start",
            json!({"type": "message_start", "message": {"id": "m", "usage": {"input_tokens": 7, "output_tokens": 1}}}),
        ),
        sse(
            "content_block_delta",
            json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": text}}),
        ),
        sse(
            "message_delta",
            json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"}, "usage": {"output_tokens": 9}}),
        ),
        sse("message_stop", json!({"type": "message_stop"})),
    ]
    .into_iter()
    .map(|f| (Duration::ZERO, f))
    .collect()
}

async fn post(addr: std::net::SocketAddr, body: &Value, extra: &[(&str, &str)]) -> HttpResp {
    let mut h = vec![("content-type", "application/json")];
    h.extend_from_slice(extra);
    http_req(
        addr,
        "POST",
        "/v1/messages",
        &h,
        &serde_json::to_vec(body).unwrap(),
    )
    .await
}

async fn mgmt(
    addr: std::net::SocketAddr,
    method: &str,
    path: &str,
    body: Option<Value>,
) -> HttpResp {
    let body = body.map(|b| b.to_string()).unwrap_or_default();
    let mut h = vec![MGMT];
    if !body.is_empty() {
        h.push(("content-type", "application/json"));
    }
    http_req(addr, method, path, &h, body.as_bytes()).await
}

/// The journal entry for `id`, once written (rows are queued, never written on the request path).
async fn journaled(addr: std::net::SocketAddr, id: &str) -> Value {
    for _ in 0..100 {
        let r = mgmt(addr, "GET", &format!("/api/v1/requests/{id}"), None).await;
        if r.status == 200 {
            return r.json();
        }
        assert_eq!(r.status, 404, "{}", r.text());
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("request {id} was never journaled");
}

fn header_value<'a>(raw: &'a str, name: &str) -> Option<&'a str> {
    raw.split("\r\n").find_map(|l| {
        let (k, v) = l.split_once(':')?;
        k.trim().eq_ignore_ascii_case(name).then(|| v.trim())
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn journal_row_per_request_with_attempt_chain() {
    let fake = FakeAnthropic::start().await;
    let pool = Pool::new().without_default_account();
    pool.add_account("a1", ProviderId::Anthropic, 1, "tok-a1");
    pool.add_account("a2", ProviderId::Anthropic, 2, "tok-a2");
    fake.route(MESSAGES, |r, _| {
        let stream = r.json()["stream"].as_bool().unwrap_or(false);
        match (r.header("authorization"), stream) {
            (Some("Bearer tok-a1"), _) => Reply::error(429, "rate_limit_error", "Rate limited")
                .header("retry-after", "120")
                .header("request-id", "req_up_a1"),
            (_, true) => Reply::sse(stream_frames("streamed")).header("request-id", "req_up_s"),
            _ => Reply::json(200, &ok_message(3, 4)).header("request-id", "req_up_a2"),
        }
    });
    let booted = pool
        .boot(Boot::default().provider(anthropic(&pool, &fake)))
        .await;

    // 1. Buffered request: a1 is quota-limited, a2 serves.
    let mut body = simple_body();
    body["metadata"] = json!({"user_id": "user-session-1"});
    let resp = post(booted.addr, &body, &[]).await;
    assert_eq!(resp.status, 200, "{}", resp.text());
    let rid = resp.header(REQUEST_ID_HEADER).unwrap().to_string();
    let j = journaled(booted.addr, &rid).await;
    let r = &j["request"];
    assert_eq!(r["id"], rid);
    assert_eq!(r["path"], "/v1/messages");
    assert_eq!(r["model"], "claude-sonnet-5");
    assert_eq!(r["provider"], "anthropic");
    assert_eq!(r["status"], 200);
    assert_eq!(r["outcome"], "completed");
    assert!(r["reason"].is_null(), "{r}");
    assert_eq!(
        (r["input_tokens"].as_i64(), r["output_tokens"].as_i64()),
        (Some(3), Some(4))
    );
    assert_eq!(
        (r["cache_read"].as_i64(), r["cache_write"].as_i64()),
        (Some(5), Some(6))
    );
    assert!(r["ttfb_ms"].as_i64().is_some() && r["total_ms"].as_i64().is_some());
    assert!(r["time"].as_str().unwrap().ends_with('Z'));
    let hash = r["session_hash"].as_str().unwrap();
    assert_eq!(hash.len(), 16);
    assert!(hash.bytes().all(|b| b.is_ascii_hexdigit()) && !hash.contains("user"));
    let attempts = j["attempts"].as_array().unwrap();
    assert_eq!(attempts.len(), 2, "{attempts:#?}");
    let a1 = &attempts[0];
    assert_eq!(
        (a1["n"].as_i64(), a1["account"].as_str()),
        (Some(1), Some("a1"))
    );
    assert_eq!(a1["provider"], "anthropic");
    assert_eq!(a1["outcome"], "failover");
    assert_eq!(a1["reason"], "quota");
    assert_eq!(a1["upstream_status"], 429);
    assert_eq!(a1["upstream_request_id"], "req_up_a1");
    assert!(a1["ms"].as_i64().is_some());
    let a2 = &attempts[1];
    assert_eq!(
        (a2["n"].as_i64(), a2["account"].as_str()),
        (Some(2), Some("a2"))
    );
    assert_eq!(a2["outcome"], "completed");
    assert!(a2["reason"].is_null());
    assert_eq!(a2["upstream_status"], 200);
    assert_eq!(a2["upstream_request_id"], "req_up_a2");

    // 2. Streamed request: the stream end and the streamed usage land on the row.
    let mut sbody = simple_body();
    sbody["stream"] = json!(true);
    let resp = post(booted.addr, &sbody, &[]).await;
    assert_eq!(resp.status, 200);
    assert!(contains(&resp.body, b"message_stop"));
    let sid = resp.header(REQUEST_ID_HEADER).unwrap().to_string();
    let j = journaled(booted.addr, &sid).await;
    assert_eq!(j["request"]["outcome"], "completed");
    assert_eq!(j["request"]["output_tokens"], 9);
    assert_eq!(j["request"]["input_tokens"], 7);
    let attempts = j["attempts"].as_array().unwrap();
    assert_eq!(attempts.last().unwrap()["upstream_request_id"], "req_up_s");
    assert_eq!(attempts.last().unwrap()["outcome"], "completed");

    // 3. Unknown inbound path: 404, journaled as an error with the path (query stripped).
    let r = http_req(
        booted.addr,
        "POST",
        "/v1/messages/count_tokens?code=SECRETCODE&beta=true",
        &[("content-type", "application/json")],
        b"{}",
    )
    .await;
    assert_eq!(r.status, 404);
    let nid = r.header(REQUEST_ID_HEADER).unwrap().to_string();
    let j = journaled(booted.addr, &nid).await;
    assert_eq!(j["request"]["outcome"], "error");
    assert_eq!(j["request"]["reason"], "unknown_path");
    assert_eq!(j["request"]["path"], "/v1/messages/count_tokens");
    assert_eq!(j["request"]["status"], 404);
    assert!(j["attempts"].as_array().unwrap().is_empty());

    // 4. The list: newest first, errors-only filter, limits; mgmt auth required.
    let all = mgmt(booted.addr, "GET", "/api/v1/requests", None)
        .await
        .json();
    let ids: Vec<&str> = all["requests"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, [nid.as_str(), sid.as_str(), rid.as_str()]);
    let first = &all["requests"][2];
    assert_eq!(
        (first["attempts"].as_i64(), first["account"].as_str()),
        (Some(2), Some("a2"))
    );
    let errs = mgmt(
        booted.addr,
        "GET",
        "/api/v1/requests?errors_only=true&limit=10",
        None,
    )
    .await
    .json();
    let errs: Vec<&str> = errs["requests"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["id"].as_str().unwrap())
        .collect();
    assert_eq!(errs, [nid.as_str()]);
    let one = mgmt(booted.addr, "GET", "/api/v1/requests?limit=1", None)
        .await
        .json();
    assert_eq!(one["requests"].as_array().unwrap().len(), 1);
    let anon = http_req(booted.addr, "GET", "/api/v1/requests", &[], b"").await;
    assert_eq!(anon.status, 401);
    let missing = mgmt(booted.addr, "GET", "/api/v1/requests/req_nope", None).await;
    assert_eq!(missing.status, 404);

    // 5. Stats: per-account attempts, error rate, latency percentiles, storage.
    let st = mgmt(booted.addr, "GET", "/api/v1/stats", None).await.json();
    let acct = |id: &str| {
        st["accounts"]
            .as_array()
            .unwrap()
            .iter()
            .find(|a| a["account"] == id)
            .cloned()
            .unwrap()
    };
    assert_eq!(acct("a1")["attempts"], 1);
    assert_eq!(acct("a1")["error_rate"], 1.0);
    // The class is the classified reason (the outcome was `failover`).
    assert_eq!(acct("a1")["top_error"], "quota", "{st}");
    assert!(acct("a2")["top_error"].is_null(), "{st}");
    // A failover hand-over is not held against the account's health rating.
    assert_eq!(acct("a1")["rated_attempts"], 0);
    assert_eq!(acct("a1")["rated_errors"], 0);
    assert_eq!(acct("a2")["rated_attempts"], 2);
    assert_eq!(acct("a2")["attempts"], 2);
    assert_eq!(acct("a2")["errors"], 0);
    assert!(acct("a2")["p50_ms"].as_i64().is_some() && acct("a2")["p95_ms"].as_i64().is_some());
    assert!(st["storage"]["db_bytes"].as_u64().unwrap() > 0);
    assert!(st["storage"]["logs_bytes"].is_u64());
    assert_eq!(st["journal_requests"], 3);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn client_cancel_outcome_recorded() {
    let fake = FakeAnthropic::start().await;
    let pool = Pool::new();
    fake.script(
        MESSAGES,
        vec![
            // Mid-stream: upstream goes silent after one delta, the client hangs up.
            Reply::sse(stream_frames("partial")[..2].to_vec())
                .header("request-id", "req_up_c")
                .hold(Hold::Forever),
            // Before any response: headers never come, the client hangs up.
            Reply::sse(stream_frames("never")).delay_headers(Duration::from_secs(60)),
        ],
    );
    let booted = pool
        .boot(Boot::default().provider(anthropic(&pool, &fake)))
        .await;
    let mut body = simple_body();
    body["stream"] = json!(true);
    let request = post_json("/v1/messages", body.to_string().as_bytes());

    let mut client = raw_client(booted.addr, &request).await;
    client.read_until(b"partial", Duration::from_secs(5)).await;
    let head = String::from_utf8_lossy(&client.received).into_owned();
    let rid = header_value(&head, REQUEST_ID_HEADER).unwrap().to_string();
    client.abort();
    let j = journaled(booted.addr, &rid).await;
    assert_eq!(j["request"]["outcome"], "client_cancelled");
    assert_eq!(j["request"]["status"], 200);
    let attempts = j["attempts"].as_array().unwrap();
    assert_eq!(attempts.len(), 1);
    assert_eq!(attempts[0]["outcome"], "client_cancelled");
    assert_eq!(attempts[0]["upstream_request_id"], "req_up_c");

    // Cancelled while the attempt waits on upstream headers: the dropped handler's guard writes it.
    let client = raw_client(booted.addr, &request).await;
    for _ in 0..100 {
        if fake.requests_to(MESSAGES).len() == 2 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(fake.requests_to(MESSAGES).len(), 2);
    client.abort();
    let mut row = Value::Null;
    for _ in 0..100 {
        let list = mgmt(booted.addr, "GET", "/api/v1/requests", None)
            .await
            .json();
        let rows = list["requests"].as_array().unwrap().clone();
        if rows.len() == 2 {
            row = rows[0].clone();
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(row["outcome"], "client_cancelled", "{row}");
    assert!(row["status"].is_null(), "no response was produced: {row}");
    let j = journaled(booted.addr, row["id"].as_str().unwrap()).await;
    assert_eq!(j["attempts"][0]["outcome"], "client_cancelled");
    assert_eq!(j["attempts"][0]["account"], "work");

    // Never an account error.
    let st = pool.state.state(&pool.meta.id, OffsetDateTime::now_utc());
    assert!(
        st.last_error.is_none(),
        "client cancel recorded on the account"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn no_bodies_or_messages_in_journal() {
    const PROMPT: &str = "PROMPT-FIXTURE-zebra-4242";
    const UPSTREAM_MSG: &str = "UPSTREAM-ERROR-MESSAGE-quokka-77";
    const RAW_USER: &str = "user_RAW-session-id-9f9f9f";
    const PROXY_SECRET: &str = "inbound-api-key-SECRET-31337";
    let fake = FakeAnthropic::start().await;
    let pool = Pool::new();
    fake.script(
        MESSAGES,
        vec![
            Reply::error(400, "invalid_request_error", UPSTREAM_MSG)
                .header("request-id", "req_up_e"),
            Reply::json(
                200,
                &json!({
                    "id": "m", "type": "message", "role": "assistant",
                    "content": [{"type": "text", "text": format!("echo {PROMPT}")}],
                    "usage": {"input_tokens": 1, "output_tokens": 2}
                }),
            ),
        ],
    );
    let booted = pool
        .boot(Boot::default().provider(anthropic(&pool, &fake)))
        .await;
    let body = json!({
        "model": "claude-sonnet-5",
        "max_tokens": 8,
        "messages": [{"role": "user", "content": PROMPT}],
        "metadata": {"user_id": RAW_USER}
    });
    let r1 = post(booted.addr, &body, &[("x-api-key", PROXY_SECRET)]).await;
    assert_eq!(r1.status, 400);
    assert!(
        r1.text().contains(UPSTREAM_MSG),
        "the client still gets the message"
    );
    let r2 = post(booted.addr, &body, &[("x-api-key", PROXY_SECRET)]).await;
    assert_eq!(r2.status, 200);
    let e = journaled(booted.addr, r1.header(REQUEST_ID_HEADER).unwrap()).await;
    assert_eq!(e["request"]["outcome"], "error");
    assert_eq!(e["request"]["reason"], "status:400");
    assert_eq!(e["attempts"][0]["upstream_request_id"], "req_up_e");
    journaled(booted.addr, r2.header(REQUEST_ID_HEADER).unwrap()).await;

    // Every byte of the database (and its WAL/SHM) is free of bodies, messages and secrets.
    booted.app.store.exec(|_| Ok(())).await.unwrap();
    let db = pool.dir.path().join("pooltime.db");
    let mut bytes = Vec::new();
    for suffix in ["", "-wal", "-shm"] {
        let mut p = db.clone().into_os_string();
        p.push(suffix);
        if let Ok(b) = std::fs::read(std::path::PathBuf::from(p)) {
            bytes.extend(b);
        }
    }
    assert!(bytes.len() > 4096, "database not found at {}", db.display());
    for needle in [
        PROMPT,
        UPSTREAM_MSG,
        RAW_USER,
        PROXY_SECRET,
        ACCESS,
        REFRESH,
    ] {
        assert!(
            !contains(&bytes, needle.as_bytes()),
            "{needle} found in the database"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn debug_mode_expires() {
    let fake = FakeAnthropic::start().await;
    let clock = Arc::new(Mutex::new(OffsetDateTime::now_utc()));
    let c = clock.clone();
    let debug = DebugMode::with_clock(Arc::new(move || *c.lock().unwrap()));
    let mut pool = Pool::new();
    pool.up = Arc::new(Upstream::new().with_debug(debug.clone()));
    fake.script(MESSAGES, vec![Reply::json(200, &ok_message(1, 1))]);
    let booted = pool
        .boot(Boot::default().provider(anthropic(&pool, &fake)))
        .await;
    let addr = booted.addr;

    // Validation and auth.
    for bad in [
        json!({"minutes": 0}),
        json!({"minutes": 61}),
        json!({}),
        json!({"minutes": "5"}),
    ] {
        let r = mgmt(addr, "POST", "/api/v1/debug", Some(bad.clone())).await;
        assert_eq!(r.status, 400, "{bad}");
    }
    let anon = http_req(
        addr,
        "POST",
        "/api/v1/debug",
        &[("content-type", "application/json")],
        br#"{"minutes":5}"#,
    )
    .await;
    assert_eq!(anon.status, 401);
    assert_eq!(
        mgmt(addr, "GET", "/api/v1/debug", None).await.json()["active"],
        false
    );

    let on = mgmt(addr, "POST", "/api/v1/debug", Some(json!({"minutes": 5}))).await;
    assert_eq!(on.status, 200, "{}", on.text());
    assert_eq!(on.json()["active"], true);
    assert!(on.json()["until"].is_string());

    let mut body = simple_body();
    body["messages"] = json!([{"role": "user", "content": "DEBUG-BODY-never-logged"}]);
    let hdrs = [
        ("x-debug-probe", "visible-1"),
        ("x-api-key", "inbound-SECRET-key"),
        ("cookie", "c=SECRETCOOKIE"),
    ];
    let r = post(addr, &body, &hdrs).await;
    assert_eq!(r.status, 200);
    let rid = r.header(REQUEST_ID_HEADER).unwrap().to_string();
    let lines: Vec<String> = captured_logs()
        .lines()
        .filter(|l| l.contains(&rid) && l.contains("debug headers"))
        .map(str::to_string)
        .collect();
    for dir in [
        "inbound_request",
        "upstream_request",
        "upstream_response",
        "inbound_response",
    ] {
        assert!(
            lines.iter().any(|l| l.contains(dir)),
            "no {dir} line: {lines:#?}"
        );
    }
    let all = lines.join("\n");
    assert!(all.contains("x-debug-probe: visible-1"));
    assert!(all.contains("x-api-key: <redacted>"));
    assert!(all.contains("authorization: <redacted>"));
    assert!(all.contains(r#""level":"INFO""#));
    for secret in [
        "inbound-SECRET-key",
        "SECRETCOOKIE",
        ACCESS,
        "DEBUG-BODY-never-logged",
    ] {
        assert!(!all.contains(secret), "{secret} logged in debug mode");
    }
    assert!(!captured_logs().contains("DEBUG-BODY-never-logged"));

    // The window ends on the (injected) clock: nothing more is captured.
    *clock.lock().unwrap() += time::Duration::minutes(5) + time::Duration::seconds(1);
    let st = mgmt(addr, "GET", "/api/v1/debug", None).await.json();
    assert_eq!(st["active"], false);
    assert!(st["until"].is_null());
    let r = post(addr, &body, &[("x-debug-probe", "visible-2")]).await;
    assert_eq!(r.status, 200);
    assert!(
        !captured_logs().contains("visible-2"),
        "headers logged after expiry"
    );

    // DELETE ends it early.
    mgmt(addr, "POST", "/api/v1/debug", Some(json!({"minutes": 60}))).await;
    assert!(debug.active());
    let off = mgmt(addr, "DELETE", "/api/v1/debug", None).await.json();
    assert_eq!(off["active"], false);
    assert!(!debug.active());
}

#[tokio::test]
async fn retention_prunes_journal() {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(Store::open(&dir.path().join("pooltime.db")).unwrap());
    // Injected clock: a fixed "now".
    let now = OffsetDateTime::from_unix_timestamp(1_794_000_000).unwrap();
    let day = 86_400;
    let row = |id: &str, ts: i64| RequestRow {
        id: id.into(),
        ts,
        outcome: "completed".into(),
        ..RequestRow::default()
    };
    let attempt = AttemptRow {
        n: 1,
        account: "a".into(),
        provider: "anthropic".into(),
        outcome: "completed".into(),
        ..AttemptRow::default()
    };
    let t = now.unix_timestamp();
    store
        .exec(move |c| {
            journal::insert(c, &row("old", t - 31 * day), std::slice::from_ref(&attempt))?;
            for i in 0..5 {
                journal::insert(
                    c,
                    &row(&format!("new{i}"), t - day + i),
                    std::slice::from_ref(&attempt),
                )?;
            }
            Ok(())
        })
        .await
        .unwrap();

    // Default 30 days: only the old request (and its attempt) goes.
    let rep = prune(&store, &RetentionCfg::default(), now).await.unwrap();
    assert_eq!(rep.journal_requests_by_age, 1);
    assert_eq!(store.read(|c| journal::get(c, "old")).unwrap(), None);
    assert_eq!(store.read(journal::count).unwrap(), 5);
    assert_eq!(RetentionCfg::default().journal_days, 30);
    assert_eq!(RetentionCfg::default().journal_max_requests, 100_000);

    // A ceiling of 0 means "no ceiling", not "delete everything".
    let unlimited = RetentionCfg {
        journal_max_requests: 0,
        ..RetentionCfg::default()
    };
    let rep = prune(&store, &unlimited, now).await.unwrap();
    assert_eq!(rep.journal_requests_by_size, 0);
    assert_eq!(store.read(journal::count).unwrap(), 5);

    // Row ceiling: the newest N survive, attempts follow their requests.
    let cfg = RetentionCfg {
        journal_max_requests: 3,
        ..RetentionCfg::default()
    };
    let rep = prune(&store, &cfg, now).await.unwrap();
    assert_eq!(rep.journal_requests_by_size, 2);
    let left: Vec<String> = store
        .read(|c| journal::list(c, false, 50))
        .unwrap()
        .into_iter()
        .map(|r| r.request.id)
        .collect();
    assert_eq!(left, ["new4", "new3", "new2"]);
    let attempts: i64 = store
        .read(|c| Ok(c.query_row("SELECT count(*) FROM journal_attempts", [], |r| r.get(0))?))
        .unwrap();
    assert_eq!(attempts, 3);

    // A clock 31 days later empties it.
    let later = now + time::Duration::days(31);
    prune(&store, &cfg, later).await.unwrap();
    assert_eq!(store.read(journal::count).unwrap(), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn journal_views_serve_the_fixture() {
    let raw = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/support/fixtures/dashboard.json"
    ))
    .unwrap();
    let fixture: Value = serde_json::from_str(&raw).unwrap();
    let pool = Pool::new();
    let booted = pool
        .boot(Boot {
            fixture_state: Some(fixture.clone()),
            ..Boot::default()
        })
        .await;
    let addr = booted.addr;
    let errs = mgmt(addr, "GET", "/api/v1/requests?errors_only=true", None)
        .await
        .json();
    let outcomes: Vec<&str> = errs["requests"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["outcome"].as_str().unwrap())
        .collect();
    assert_eq!(outcomes, ["idle_timeout", "error"]);
    // Every listed request has a detail document with its attempt chain.
    let all = mgmt(addr, "GET", "/api/v1/requests", None).await.json();
    for r in all["requests"].as_array().unwrap() {
        let id = r["id"].as_str().unwrap();
        let d = mgmt(addr, "GET", &format!("/api/v1/requests/{id}"), None).await;
        assert_eq!(d.status, 200, "{id}");
        let n = d.json()["attempts"].as_array().unwrap().len() as i64;
        assert_eq!(n, r["attempts"].as_i64().unwrap(), "{id}");
    }
    let tl = mgmt(addr, "GET", "/api/v1/timeline?account=team-2", None)
        .await
        .json();
    assert_eq!(tl["events"].as_array().unwrap().len(), 1);
    let st = mgmt(addr, "GET", "/api/v1/stats", None).await.json();
    assert_eq!(st, fixture["journal"]["stats"]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn lookup_flushes_a_still_queued_row_and_favicon_is_not_an_error() {
    let pool = Pool::new();
    let booted = pool.boot(Boot::default()).await;
    let addr = booted.addr;

    // Browsers' favicon probe: answered, never journaled as an unknown path.
    let fav = http_req(addr, "GET", "/favicon.ico", &[], b"").await;
    assert_eq!(fav.status, 204);

    // Keep the store writer busy so the next row waits in its queue.
    booted
        .app
        .store
        .submit(|_| {
            std::thread::sleep(Duration::from_millis(400));
            Ok(())
        })
        .unwrap();
    let r = http_req(addr, "GET", "/v1/nope", &[], b"").await;
    assert_eq!(r.status, 404);
    let id = r.header(REQUEST_ID_HEADER).unwrap().to_string();
    // One lookup, no polling: the server flushes the queue before answering 404.
    let d = mgmt(addr, "GET", &format!("/api/v1/requests/{id}"), None).await;
    assert_eq!(d.status, 200, "{}", d.text());
    assert_eq!(d.json()["request"]["reason"], "unknown_path");

    let all = mgmt(addr, "GET", "/api/v1/requests", None).await.json();
    let paths: Vec<&str> = all["requests"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["path"].as_str().unwrap_or(""))
        .collect();
    assert_eq!(paths, ["/v1/nope"], "favicon must not be journaled");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn body_hangup_is_client_cancelled() {
    let pool = Pool::new();
    let booted = pool.boot(Boot::default()).await;
    // Headers promise 1000 bytes; the client sends 10 and hangs up.
    let head =
        "POST /v1/messages HTTP/1.1\r\nhost: 127.0.0.1\r\ncontent-type: application/json\r\n\
                content-length: 1000\r\n\r\n{\"model\":";
    let client = raw_client(booted.addr, head.as_bytes()).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    client.abort();
    let mut row = Value::Null;
    for _ in 0..100 {
        let list = mgmt(booted.addr, "GET", "/api/v1/requests", None)
            .await
            .json();
        if let Some(r) = list["requests"].as_array().unwrap().first() {
            row = r.clone();
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(row["outcome"], "client_cancelled", "{row}");
    assert!(row["reason"].is_null(), "{row}");
    assert!(row["status"].is_null(), "{row}");
}
