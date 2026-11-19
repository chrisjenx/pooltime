//! Task F5: `GET /api/v1/analytics/burn` (weekly subscription burn) over the real server, and
//! `GET /api/v1/analytics/projects` (usage by project and session).

use crate::support;

use std::sync::Arc;
use std::time::Duration;

use pooltime::providers::Provider;
use pooltime::store::usage_history::{insert_sample, Point, WINDOW_7D};
use serde_json::{json, Value};
use support::fake_anthropic::{FakeAnthropic, Reply, MESSAGES};
use support::sockets::http_req;
use support::*;
use time::OffsetDateTime;

const MGMT: (&str, &str) = ("authorization", "Bearer mgmt-test-key");

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn burn_endpoint_reports_pool_weeks_and_needs_key() {
    let pool = Pool::new();
    pool.add_account(
        "idle",
        pooltime::providers::ProviderId::Anthropic,
        0,
        "tok-b",
    );
    let now = OffsetDateTime::now_utc().unix_timestamp();
    let reset = Some(now + 3 * 86_400);
    // `work` used 0.3 of its window in the last minutes; `gone` (removed) has old history.
    pool.store
        .exec(move |c| {
            for (a, ts, util) in [
                ("work", now - 120, 0.1),
                ("work", now - 60, 0.4),
                ("gone", now - 200 * 86_400, 0.0),
                ("gone", now - 199 * 86_400, 0.5),
            ] {
                insert_sample(c, a, WINDOW_7D, &Point { ts, util, reset })?;
            }
            Ok(())
        })
        .await
        .unwrap();
    let booted = pool.boot(Boot::default()).await;

    let anon = http_req(booted.addr, "GET", "/api/v1/analytics/burn", &[], b"").await;
    assert_eq!(anon.status, 401, "{}", anon.text());

    let r = http_req(booted.addr, "GET", "/api/v1/analytics/burn", &[MGMT], b"").await;
    assert_eq!(r.status, 200, "{}", r.text());
    let v = r.json();
    let weeks = v["weeks"].as_array().unwrap();
    assert_eq!(weeks.len(), 12, "default weeks");
    let week_re = |s: &str| s.len() == 8 && &s[4..6] == "-W";
    assert!(
        weeks.iter().all(|w| week_re(w["week"].as_str().unwrap())),
        "{v}"
    );
    let total: f64 = weeks.iter().map(|w| w["subs_used"].as_f64().unwrap()).sum();
    assert!((total - 0.3).abs() < 1e-3, "{v}");
    let work: Vec<&Value> = weeks
        .iter()
        .flat_map(|w| w["per_account"].as_array().unwrap())
        .collect();
    // (two entries only if a week boundary fell in the last two minutes)
    assert!(
        !work.is_empty() && work.iter().all(|a| a["account"] == "work"),
        "{v}"
    );
    assert_eq!(work[0]["plan"], "max", "plan from the stored credentials");
    let f = &v["forecast"];
    assert_eq!(f["accounts"], 2, "pool size, not history size: {v}");
    assert!(f["elapsed_fraction"].as_f64().unwrap() <= 1.0);
    assert!(v["idle_accounts"]
        .as_array()
        .unwrap()
        .iter()
        .any(|a| a == "idle"));
    assert!(!v["idle_accounts"]
        .as_array()
        .unwrap()
        .iter()
        .any(|a| a == "gone"));

    let long = http_req(
        booted.addr,
        "GET",
        "/api/v1/analytics/burn?weeks=500",
        &[MGMT],
        b"",
    )
    .await;
    assert_eq!(long.json()["weeks"].as_array().unwrap().len(), 104);
    let bad = http_req(
        booted.addr,
        "GET",
        "/api/v1/analytics/burn?weeks=0",
        &[MGMT],
        b"",
    )
    .await;
    assert_eq!(bad.status, 400);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn burn_endpoint_serves_the_fixture() {
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
    let r = http_req(booted.addr, "GET", "/api/v1/analytics/burn", &[MGMT], b"").await;
    assert_eq!(r.status, 200);
    let v = r.json();
    assert_eq!(v, fixture["analytics"]["burn"]);
    assert!(
        v["weeks"].as_array().unwrap().len() >= 8,
        "chart shows >= 8 weeks"
    );
}

// ---- usage by project ----------------------------------------------------------------------

fn ok_message(input: u64, output: u64) -> Value {
    json!({
        "id": "msg_1", "type": "message", "role": "assistant", "model": "claude-sonnet-5",
        "content": [{"type": "text", "text": "hello"}],
        "stop_reason": "end_turn",
        "usage": {"input_tokens": input, "output_tokens": output,
                  "cache_read_input_tokens": 100, "cache_creation_input_tokens": 10}
    })
}

/// A Claude Code-like body: a session id and an environment block naming `cwd`.
fn session_body(session: &str, cwd: Option<&str>) -> Value {
    let mut b = simple_body();
    b["metadata"] = json!({ "user_id": session });
    if let Some(cwd) = cwd {
        b["system"] = json!([
            {"type": "text", "text": "You are Claude Code."},
            {"type": "text", "text": format!(
                "# Environment\n - Primary working directory: {cwd}\n - Is a git repository: true\n")},
        ]);
    }
    b
}

async fn send(addr: std::net::SocketAddr, b: &Value) {
    let r = http_req(
        addr,
        "POST",
        "/v1/messages",
        &[("content-type", "application/json")],
        &serde_json::to_vec(b).unwrap(),
    )
    .await;
    assert_eq!(r.status, 200, "{}", r.text());
}

async fn projects(addr: std::net::SocketAddr, query: &str) -> support::sockets::HttpResp {
    let path = format!("/api/v1/analytics/projects{query}");
    http_req(addr, "GET", &path, &[MGMT], b"").await
}

/// The 24-hour report once the journal holds `n` requests (rows are written off the request path).
async fn report_with(addr: std::net::SocketAddr, n: u64) -> Value {
    for _ in 0..100 {
        let r = projects(addr, "?range=24h").await;
        assert_eq!(r.status, 200, "{}", r.text());
        let v = r.json();
        if v["totals"]["requests"] == n {
            return v;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("the journal never held {n} requests");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn projects_endpoint_attributes_sessions_to_their_repository() {
    let fake = FakeAnthropic::start().await;
    fake.route(MESSAGES, |_, _| Reply::json(200, &ok_message(30, 20)));
    let pool = Pool::new();
    let (policy, _) = fast_policy();
    let provider: Arc<dyn Provider> = Arc::new(pool.provider(&fake, policy));
    let booted = pool.boot(Boot::default().provider(provider)).await;

    // Session one in acme (twice; the second request has no environment block and still counts
    // for acme), session two in a worktree of widgets, session three without a directory.
    let a = booted.addr;
    send(a, &session_body("session-one", Some("/home/user/src/acme"))).await;
    send(a, &session_body("session-one", None)).await;
    let worktree = "/home/user/src/widgets/.claude/worktrees/agent-1";
    send(a, &session_body("session-two", Some(worktree))).await;
    send(a, &session_body("session-three", None)).await;

    let v = report_with(a, 4).await;
    assert_eq!(v["range"], "24h");
    // No usage history in the test pool: tokens only, no invented quota.
    assert!(v["quota"].is_null(), "{v}");
    let list = v["projects"].as_array().unwrap();
    let names: Vec<&str> = list
        .iter()
        .map(|p| p["project"].as_str().unwrap())
        .collect();
    // Most tokens first; ties by name.
    assert_eq!(names, ["acme", "unknown", "widgets"], "{v}");
    let acme = &list[0];
    assert_eq!(acme["requests"], 2);
    assert_eq!(acme["tokens"], 2 * (30 + 20 + 100 + 10));
    assert_eq!(acme["sessions"], 1);
    assert_eq!(acme["accounts"][0]["account"], "work");
    assert!(acme["est_subs"].is_null());
    assert!(
        (acme["share"].as_f64().unwrap() - 0.5).abs() < 1e-9,
        "{acme}"
    );
    assert_eq!(
        list[2]["requests"], 1,
        "a worktree counts as its repository"
    );
    assert_eq!(list[1]["key"], "unknown");
    assert_eq!(v["sessions_total"], 3);
    assert_eq!(v["sessions"][0]["project"], "acme", "most usage first");

    // Privacy: neither paths nor raw session ids reach the answer or the database.
    let text = v.to_string();
    for leak in ["/home/user", "session-one", "worktrees"] {
        assert!(!text.contains(leak), "{leak} in {text}");
    }
    let stored: Vec<String> = pool
        .store
        .read(|c| {
            let mut st =
                c.prepare("SELECT session_hash || project || path_hash FROM session_projects")?;
            let rows = st.query_map([], |r| r.get(0))?;
            Ok(rows.collect::<Result<_, _>>()?)
        })
        .unwrap();
    assert_eq!(stored.len(), 2, "{stored:?}");
    assert!(
        stored
            .iter()
            .all(|s| !s.contains('/') && !s.contains("session")),
        "{stored:?}"
    );

    // The answer's shape (times and hashes masked).
    let mut shown = v.clone();
    for k in ["since", "now"] {
        shown[k] = json!("<time>");
    }
    for list in ["projects", "sessions"] {
        for l in shown[list].as_array_mut().unwrap() {
            l["first_seen"] = json!("<time>");
            l["last_seen"] = json!("<time>");
            if l["key"] != "unknown" {
                l["key"] = json!("<hash>");
            }
        }
    }
    insta::assert_snapshot!(serde_json::to_string_pretty(&shown).unwrap());

    // Key required; ranges checked.
    let anon = http_req(a, "GET", "/api/v1/analytics/projects", &[], b"").await;
    assert_eq!(anon.status, 401);
    assert_eq!(projects(a, "?range=1y").await.status, 400);
    let week = projects(a, "?range=7d").await.json();
    assert_eq!(week["range"], "7d");
    assert_eq!(week["totals"]["requests"], 4);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn projects_endpoint_serves_the_fixture() {
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
    let r = projects(booted.addr, "?range=7d").await;
    assert_eq!(r.status, 200);
    let v = r.json();
    assert_eq!(v["range"], "7d", "the asked range");
    assert_eq!(v["projects"], fixture["analytics"]["projects"]["projects"]);
}
