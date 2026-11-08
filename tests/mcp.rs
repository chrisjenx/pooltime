//! Task F6: the MCP server at `/mcp` (JSON-RPC over HTTP POST), its token, its tools, and the
//! `.claude.json` registration written by `connect --mcp`.

mod support;

use std::net::SocketAddr;
use std::os::unix::fs::PermissionsExt;
use std::time::Duration;

use pooltime::accounts::AccountId;
use serde_json::{json, Value};
use support::fake_anthropic::{FakeAnthropic, Reply, TOKEN};
use support::sockets::{http_get, http_req, HttpResp};
use support::{Boot, Booted, Pool};
use time::OffsetDateTime;

const MCP_KEY: &str = "mcp-test-key";
const MGMT_KEY: &str = "mgmt-test-key";

fn id(s: &str) -> AccountId {
    s.parse().unwrap()
}

async fn post(addr: SocketAddr, path: &str, bearer: Option<&str>, body: &Value) -> HttpResp {
    let auth = bearer.map(|k| format!("Bearer {k}"));
    let mut h = vec![
        ("content-type", "application/json"),
        ("accept", "application/json, text/event-stream"),
    ];
    if let Some(a) = auth.as_deref() {
        h.push(("authorization", a));
    }
    http_req(addr, "POST", path, &h, body.to_string().as_bytes()).await
}

/// One JSON-RPC request to `/mcp` with the MCP token; the `result` (panics on an error).
async fn rpc(srv: &Booted, method: &str, params: Value) -> Value {
    let r = post(
        srv.addr,
        "/mcp",
        Some(MCP_KEY),
        &json!({"jsonrpc": "2.0", "id": 7, "method": method, "params": params}),
    )
    .await;
    assert_eq!(r.status, 200, "{}", r.text());
    assert_eq!(r.header("content-type"), Some("application/json"));
    let v = r.json();
    assert_eq!(v["jsonrpc"], "2.0");
    assert_eq!(v["id"], 7);
    assert!(v.get("error").is_none(), "{v}");
    v["result"].clone()
}

/// `tools/call`: `(is_error, text)`.
async fn call_tool(srv: &Booted, name: &str, args: Value) -> (bool, String) {
    let res = rpc(srv, "tools/call", json!({"name": name, "arguments": args})).await;
    let text = res["content"][0]["text"].as_str().unwrap().to_string();
    assert_eq!(res["content"][0]["type"], "text");
    (res["isError"] == true, text)
}

/// A tool call that must succeed; its JSON payload.
async fn tool_json(srv: &Booted, name: &str, args: Value) -> Value {
    let (err, text) = call_tool(srv, name, args).await;
    assert!(!err, "{name} failed: {text}");
    serde_json::from_str(&text).unwrap_or_else(|e| panic!("{name}: not JSON ({e}): {text}"))
}

fn account<'a>(v: &'a Value, name: &str) -> &'a Value {
    v["accounts"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["id"] == name)
        .unwrap_or_else(|| panic!("no account {name} in {v}"))
}

const TOOLS: [&str; 6] = [
    "pool_usage",
    "pool_health",
    "start_relogin",
    "set_weight",
    "recent_errors",
    "explain_request",
];

#[tokio::test]
async fn mcp_tools_listed() {
    let pool = Pool::new();
    let srv = pool.boot(Boot::default()).await;

    // The JSON-RPC round trip Claude Code's http transport makes.
    let init = rpc(
        &srv,
        "initialize",
        json!({
            "protocolVersion": "2025-06-18",
            "capabilities": {},
            "clientInfo": {"name": "claude-code", "version": "2.1.0"},
        }),
    )
    .await;
    assert_eq!(init["protocolVersion"], "2025-06-18");
    assert_eq!(init["serverInfo"]["name"], "pooltime");
    assert_eq!(init["serverInfo"]["version"], env!("CARGO_PKG_VERSION"));
    assert!(init["capabilities"]["tools"].is_object(), "{init}");

    let r = post(
        srv.addr,
        "/mcp",
        Some(MCP_KEY),
        &json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
    )
    .await;
    assert_eq!(r.status, 202, "{}", r.text());
    assert!(r.body.is_empty());

    let list = rpc(&srv, "tools/list", json!({})).await;
    let tools = list["tools"].as_array().unwrap();
    let mut names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
    names.sort_unstable();
    let mut want = TOOLS.to_vec();
    want.sort_unstable();
    assert_eq!(names, want);
    for t in tools {
        assert!(t["description"].as_str().is_some_and(|d| !d.is_empty()));
        assert_eq!(t["inputSchema"]["type"], "object", "{t}");
    }

    // pool_health: state, since, why and the login expiry per account; never an email.
    let mut meta = pool.reg.get(&id("work")).unwrap();
    meta.identity.email = Some("secret-person@example.com".into());
    pool.reg.upsert(meta).unwrap();
    let (err, text) = call_tool(&srv, "pool_health", json!({})).await;
    assert!(!err, "{text}");
    assert!(!text.contains("secret-person"), "{text}");
    assert!(!text.contains(support::ACCESS) && !text.contains(support::REFRESH));
    assert!(!text.contains(MGMT_KEY) && !text.contains(MCP_KEY));
    let v: Value = serde_json::from_str(&text).unwrap();
    let a = account(&v, "work");
    assert_eq!(a["status"], "ready");
    for k in ["since", "why", "login_expires_at", "login_expires_in_secs"] {
        assert!(a.get(k).is_some(), "{k} missing in {a}");
    }
    assert_eq!(v["pool"]["accounts"], 1);

    // pool_usage: per-account 7d left, pool total, this week's burn and the forecast.
    let v = tool_json(&srv, "pool_usage", json!({})).await;
    assert!(account(&v, "work").get("seven_day_left").is_some(), "{v}");
    assert!(v["pool"]["seven_day_left_total"].is_number(), "{v}");
    assert!(v["this_week"].is_object(), "{v}");
    assert!(v["forecast"].is_object(), "{v}");
    assert!(!v.to_string().contains("secret-person"));

    // Errors and request detail: an unknown path is journaled as an error.
    let miss = http_get(srv.addr, "/no/such/thing", &[]).await;
    let req_id = miss.header("x-pooltime-request-id").unwrap().to_string();
    let _ = pool.store.exec(|_| Ok(())).await;
    let v = tool_json(&srv, "recent_errors", json!({})).await;
    let rows = v["requests"].as_array().unwrap();
    assert!(rows.iter().any(|r| r["id"] == req_id.as_str()), "{v}");
    assert_eq!(v["limit"], 10, "default limit");
    let v = tool_json(&srv, "recent_errors", json!({"limit": 500})).await;
    assert_eq!(v["limit"], 50, "capped at 50");
    let (err, _) = call_tool(&srv, "recent_errors", json!({"limit": 0})).await;
    assert!(err);
    let v = tool_json(&srv, "explain_request", json!({"id": req_id})).await;
    assert_eq!(v["request"]["id"], req_id.as_str());
    assert_eq!(v["request"]["reason"], "unknown_path");
    assert!(v["attempts"].is_array());
    let (err, text) = call_tool(&srv, "explain_request", json!({"id": "nope"})).await;
    assert!(err && text.contains("nope"), "{text}");

    // Protocol edges: ping, unknown method, unknown tool, malformed JSON, GET.
    assert_eq!(rpc(&srv, "ping", json!({})).await, json!({}));
    let r = post(
        srv.addr,
        "/mcp",
        Some(MCP_KEY),
        &json!({"jsonrpc": "2.0", "id": 1, "method": "resources/list"}),
    )
    .await;
    assert_eq!(r.json()["error"]["code"], -32601);
    let r = http_req(
        srv.addr,
        "POST",
        "/mcp",
        &[
            ("content-type", "application/json"),
            ("authorization", "Bearer mcp-test-key"),
        ],
        b"{nope",
    )
    .await;
    assert_eq!(r.json()["error"]["code"], -32700);
    let r = http_get(
        srv.addr,
        "/mcp",
        &[("authorization", "Bearer mcp-test-key")],
    )
    .await;
    assert_eq!(r.status, 405);
}

#[tokio::test]
async fn mcp_token_cannot_call_mgmt_mutations_beyond_scope() {
    let pool = Pool::new();
    let srv = pool.boot(Boot::default()).await;
    let init = json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {}});

    // /mcp takes only the MCP token: not nothing, not a wrong one, not the management key.
    for bearer in [None, Some("wrong"), Some(MGMT_KEY)] {
        let r = post(srv.addr, "/mcp", bearer, &init).await;
        assert_eq!(r.status, 401, "{bearer:?}: {}", r.text());
    }
    assert_eq!(
        post(srv.addr, "/mcp", Some(MCP_KEY), &init).await.status,
        200
    );
    // DNS-rebinding guard: a foreign Host or Origin is refused before auth.
    let r = http_req(
        srv.addr,
        "POST",
        "/mcp",
        &[
            ("host", "evil.example:80"),
            ("authorization", "Bearer mcp-test-key"),
        ],
        init.to_string().as_bytes(),
    )
    .await;
    assert_eq!(r.status, 403);

    // The MCP token is not a management key: every /api/v1 mutation (and keyed read) is 401.
    for (method, path, body) in [
        ("DELETE", "/api/v1/accounts/work", json!({})),
        (
            "POST",
            "/api/v1/accounts",
            json!({"id": "x", "provider": "anthropic"}),
        ),
        (
            "POST",
            "/api/v1/accounts/work/routing",
            json!({"weight": 3}),
        ),
        ("POST", "/api/v1/accounts/work/login", json!({})),
        ("POST", "/api/v1/profiles/connect", json!({"all": true})),
        ("POST", "/api/v1/profiles/disconnect", json!({})),
        ("POST", "/api/v1/tuning", json!({"min_headroom": 0.1})),
        ("GET", "/api/v1/requests", json!({})),
    ] {
        let r = http_req(
            srv.addr,
            method,
            path,
            &[
                ("content-type", "application/json"),
                ("authorization", "Bearer mcp-test-key"),
            ],
            body.to_string().as_bytes(),
        )
        .await;
        assert_eq!(r.status, 401, "{method} {path}: {}", r.text());
    }

    // No tool removes accounts, reads tokens or changes connections.
    for name in [
        "remove_account",
        "delete_account",
        "connect",
        "disconnect",
        "connect_profiles",
        "read_token",
        "credentials",
    ] {
        let r = post(
            srv.addr,
            "/mcp",
            Some(MCP_KEY),
            &json!({"jsonrpc": "2.0", "id": 2, "method": "tools/call",
                "params": {"name": name, "arguments": {"account": "work"}}}),
        )
        .await;
        assert_eq!(r.status, 200);
        assert_eq!(r.json()["error"]["code"], -32602, "{name}: {}", r.text());
    }
    assert!(pool.reg.get(&id("work")).is_some(), "account still there");

    // In scope: a timed weight override that reverts on its own and shows in pool_health.
    let v = tool_json(
        &srv,
        "set_weight",
        json!({"account": "work", "weight": 5, "until": "2d"}),
    )
    .await;
    assert_eq!(v["effective_weight"], 5.0, "{v}");
    let until = v["weight_override"]["until"].as_str().unwrap().to_string();
    let t = OffsetDateTime::parse(&until, &time::format_description::well_known::Rfc3339).unwrap();
    let left = t - OffsetDateTime::now_utc();
    assert!(left > time::Duration::hours(47) && left <= time::Duration::hours(48));
    let meta = pool.reg.get(&id("work")).unwrap();
    assert_eq!(meta.weight, 1.0, "base weight untouched");
    let h = tool_json(&srv, "pool_health", json!({})).await;
    assert_eq!(account(&h, "work")["weight_override"]["weight"], 5.0, "{h}");
    assert_eq!(account(&h, "work")["effective_weight"], 5.0);

    // RFC 3339 works too; anything else is refused with a clear message, as are bad ranges.
    let soon = OffsetDateTime::now_utc() + time::Duration::hours(3);
    let rfc = soon
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap();
    tool_json(
        &srv,
        "set_weight",
        json!({"account": "work", "weight": 2, "until": rfc}),
    )
    .await;
    for (args, needle) in [
        (
            json!({"account": "work", "weight": 2, "until": "until friday"}),
            "until",
        ),
        (
            json!({"account": "work", "weight": 2, "until": "3w"}),
            "until",
        ),
        (
            json!({"account": "work", "weight": 2, "until": "0h"}),
            "until",
        ),
        (json!({"account": "work", "weight": 1000}), "weight"),
        (json!({"account": "work", "priority": -1}), "priority"),
        (json!({"account": "work"}), "priority"),
        (json!({"account": "ghost", "weight": 2}), "ghost"),
        (json!({"weight": 2}), "account"),
    ] {
        let (err, text) = call_tool(&srv, "set_weight", args.clone()).await;
        assert!(err, "{args} should fail");
        assert!(text.contains(needle), "{args}: {text}");
    }
    let (err, _) = call_tool(
        &srv,
        "set_weight",
        json!({"account": "work", "priority": 4}),
    )
    .await;
    assert!(!err);
    assert_eq!(pool.reg.get(&id("work")).unwrap().priority, 4);
}

#[tokio::test]
async fn start_relogin_returns_url_without_verifier() {
    let fake = FakeAnthropic::start().await;
    fake.route(TOKEN, |_, _| {
        Reply::json(500, &json!({"error": "server_error"}))
    });
    let pool = Pool::new();
    let url = fake.url();
    let srv = pool
        .boot(Boot {
            configure: Some(Box::new(move |cfg: &mut pooltime::config::Config| {
                cfg.anthropic.token_url = format!("{url}{TOKEN}");
                cfg.anthropic.api_base = url;
            })),
            ..Boot::default()
        })
        .await;

    let (err, text) = call_tool(&srv, "start_relogin", json!({"account": "work"})).await;
    assert!(!err, "{text}");
    let v: Value = serde_json::from_str(&text).unwrap();
    let mut keys: Vec<&str> = v.as_object().unwrap().keys().map(String::as_str).collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        ["account", "authorize_url", "expires_at", "instruction"]
    );
    let authorize = v["authorize_url"].as_str().unwrap();
    assert!(authorize.starts_with(&srv.app.cfg.anthropic.authorize_url));
    assert!(!text.contains("signin_id") && !text.contains("verifier"));

    // Complete the browser leg: the token exchange carries the verifier, which must not be in
    // anything the tool returned.
    let parsed = reqwest::Url::parse(authorize).unwrap();
    let q = |n: &str| {
        parsed
            .query_pairs()
            .find(|(k, _)| k == n)
            .map(|(_, v)| v.into_owned())
            .unwrap()
    };
    let redirect = reqwest::Url::parse(&q("redirect_uri")).unwrap();
    let port = redirect.port().unwrap();
    let _ = reqwest::get(format!(
        "http://127.0.0.1:{port}/callback?code=c0de&state={}",
        q("state")
    ))
    .await
    .unwrap();
    let mut verifier = None;
    for _ in 0..100 {
        if let Some(r) = fake.requests_to(TOKEN).first() {
            verifier = r.json()["code_verifier"].as_str().map(str::to_string);
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let verifier = verifier.expect("token exchange with a verifier");
    assert!(verifier.len() >= 32);
    assert!(!text.contains(&verifier), "verifier leaked: {text}");

    // Only existing accounts can be re-logged in; nothing is created.
    let (err, text) = call_tool(&srv, "start_relogin", json!({"account": "ghost"})).await;
    assert!(err && text.contains("ghost"), "{text}");
    assert!(pool.reg.get(&id("ghost")).is_none());
}

#[tokio::test]
async fn registration_added_and_removed() {
    let pool = Pool::new();
    let srv = pool.boot(Boot::default()).await;
    let home = tempfile::tempdir().unwrap();
    let dir = home.path().join(".claude-x");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("settings.json"), "{}").unwrap();
    let cj = dir.join(".claude.json");
    let original = json!({
        "numStartups": 3,
        "mcpServers": {"other": {"type": "stdio", "command": "other-mcp"}},
    });
    std::fs::write(&cj, serde_json::to_vec_pretty(&original).unwrap()).unwrap();
    std::fs::set_permissions(&cj, std::fs::Permissions::from_mode(0o600)).unwrap();
    let path = dir.to_str().unwrap().to_string();
    let mgmt = |body: Value| {
        let addr = srv.addr;
        async move {
            let auth = format!("Bearer {MGMT_KEY}");
            http_req(
                addr,
                "POST",
                "/api/v1/profiles/connect",
                &[
                    ("content-type", "application/json"),
                    ("authorization", &auth),
                ],
                body.to_string().as_bytes(),
            )
            .await
        }
    };
    let read = || -> Value { serde_json::from_slice(&std::fs::read(&cj).unwrap()).unwrap() };

    let r = mgmt(json!({"profiles": [path], "mcp": "yes"})).await;
    assert_eq!(r.status, 400, "{}", r.text());
    // Plain connect leaves mcpServers alone.
    let r = mgmt(json!({"profiles": [path]})).await;
    assert_eq!(r.status, 200, "{}", r.text());
    assert_eq!(read()["mcpServers"], original["mcpServers"]);

    let r = mgmt(json!({"profiles": [path], "mcp": true})).await;
    assert_eq!(r.status, 200, "{}", r.text());
    assert!(!r.text().contains(MCP_KEY), "the token is never echoed");
    let v = read();
    assert_eq!(
        v["mcpServers"]["pooltime"],
        json!({
            "type": "http",
            "url": format!("http://{}/mcp", srv.addr),
            "headers": {"Authorization": format!("Bearer {MCP_KEY}")},
        })
    );
    assert_eq!(v["mcpServers"]["other"], original["mcpServers"]["other"]);
    assert_eq!(v["numStartups"], 3);
    let mode = std::fs::metadata(&cj).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600, "existing mode kept");

    // Idempotent.
    let before = std::fs::read(&cj).unwrap();
    let r = mgmt(json!({"profiles": [path], "mcp": true})).await;
    assert_eq!(r.status, 200, "{}", r.text());
    assert_eq!(std::fs::read(&cj).unwrap(), before);

    // Disconnect removes only our entry.
    let auth = format!("Bearer {MGMT_KEY}");
    let r = http_req(
        srv.addr,
        "POST",
        "/api/v1/profiles/disconnect",
        &[
            ("content-type", "application/json"),
            ("authorization", &auth),
        ],
        json!({"profiles": [path]}).to_string().as_bytes(),
    )
    .await;
    assert_eq!(r.status, 200, "{}", r.text());
    let v = read();
    assert!(v["mcpServers"].get("pooltime").is_none(), "{v}");
    assert_eq!(v["mcpServers"]["other"], original["mcpServers"]["other"]);
    assert_eq!(v["numStartups"], 3);

    // Already absent: nothing changes.
    let before = std::fs::read(&cj).unwrap();
    let profile = pooltime::claude_code::Profile::from_dir(&dir, home.path());
    pooltime::claude_code::disconnect(&srv.app.paths, &[profile]).unwrap();
    assert_eq!(std::fs::read(&cj).unwrap(), before);
}
