//! `GET /api/v1/routing/preview`: management auth, the document's shape (over the dashboard
//! fixture), agreement with the account a real request is then served by, and no side effects
//! (the tie cursor, sticky pins and the journal are untouched).

use crate::support;

use std::sync::atomic::Ordering;
use std::sync::Arc;

use pooltime::providers::{Provider, ProviderId};
use serde_json::{json, Value};
use support::sockets::{http_get, http_req};
use support::*;
use time::OffsetDateTime;

const PREVIEW: &str = "/api/v1/routing/preview";
const MGMT: (&str, &str) = ("authorization", "Bearer mgmt-test-key");
const COUNT: &str = "/v1/messages/count_tokens";
const FIXTURE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/support/fixtures/dashboard.json"
);

fn fixture() -> Value {
    serde_json::from_str(&std::fs::read_to_string(FIXTURE).unwrap()).unwrap()
}

fn anthropic(pool: &Pool, fake: &FakeAnthropic) -> Arc<dyn Provider> {
    let (policy, _) = fast_policy();
    Arc::new(pool.provider(fake, policy))
}

fn row<'a>(v: &'a Value, id: &str) -> &'a Value {
    v["accounts"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["id"] == id)
        .unwrap_or_else(|| panic!("no row {id} in {v}"))
}

#[tokio::test]
async fn preview_needs_the_management_key() {
    let pool = Pool::new();
    let srv = pool.boot(Boot::default()).await;

    let r = http_get(srv.addr, PREVIEW, &[]).await;
    assert_eq!(r.status, 401, "{}", r.text());
    assert_eq!(r.json()["error"]["type"], "authentication_error");
    let r = http_get(srv.addr, PREVIEW, &[("authorization", "Bearer wrong")]).await;
    assert_eq!(r.status, 401);
    let r = http_req(srv.addr, "POST", PREVIEW, &[MGMT], b"").await;
    assert_eq!(r.status, 405);

    let r = http_get(srv.addr, &format!("{PREVIEW}?model=claude-opus-5"), &[MGMT]).await;
    assert_eq!(r.status, 200, "{}", r.text());
    assert_eq!(r.header("cache-control"), Some("no-store"));
    assert_eq!(r.json()["model"], "claude-opus-5");

    let long = "x".repeat(201);
    let r = http_get(srv.addr, &format!("{PREVIEW}?model={long}"), &[MGMT]).await;
    assert_eq!(r.status, 400);
}

/// The picker fields come only when asked for, and the model usage behind them is read from the
/// journal at most once per TTL (the dashboard polls every few seconds).
#[tokio::test]
async fn picker_list_is_opt_in_and_cached() {
    let pool = Pool::new();
    let srv = pool.boot(Boot::default()).await;
    let add = |model: &'static str, n: usize| {
        let now = OffsetDateTime::now_utc().unix_timestamp();
        pool.store.exec(move |c| {
            for i in 0..n {
                c.execute(
                    "INSERT INTO journal_requests(id, ts, model, outcome) VALUES (?1, ?2, ?3, 'completed')",
                    rusqlite::params![format!("{model}-{i}-{now}"), now, model],
                )?;
            }
            Ok(())
        })
    };
    add("claude-sonnet-5", 2).await.unwrap();

    // No model: default and list included.
    let v = http_get(srv.addr, PREVIEW, &[MGMT]).await.json();
    assert_eq!(v["default_model"], "claude-sonnet-5");
    assert_eq!(
        v["models"][0],
        json!({"id": "claude-sonnet-5", "requests": 2})
    );

    // A chosen model: no picker fields unless asked for.
    let v = http_get(srv.addr, &format!("{PREVIEW}?model=claude-opus-5"), &[MGMT])
        .await
        .json();
    assert!(
        v.get("models").is_none() && v.get("default_model").is_none(),
        "{v}"
    );
    let v = http_get(
        srv.addr,
        &format!("{PREVIEW}?model=claude-opus-5&models=true"),
        &[MGMT],
    )
    .await
    .json();
    assert!(v["models"].is_array(), "{v}");

    // Within the TTL the journal is not read again: new traffic does not show yet.
    add("claude-opus-5", 5).await.unwrap();
    let v = http_get(srv.addr, PREVIEW, &[MGMT]).await.json();
    assert_eq!(
        v["default_model"], "claude-sonnet-5",
        "cached usage expected"
    );
    assert_eq!(v["models"][0]["requests"], 2);
}

#[tokio::test]
async fn preview_shape_over_the_dashboard_fixture() {
    let pool = Pool::new();
    let srv = pool
        .boot(Boot {
            fixture_state: Some(fixture()),
            ..Boot::default()
        })
        .await;

    // No model: the fixture journal's most requested Claude model.
    let v = http_get(srv.addr, PREVIEW, &[MGMT]).await.json();
    assert_eq!(v["default_model"], "claude-opus-4-5");
    assert_eq!(v["model"], "claude-opus-4-5");
    assert_eq!(
        v["models"][0],
        json!({"id": "claude-opus-4-5", "requests": 3})
    );

    // personal (later reset, weight override 5, 3 sessions) outscores team-1 (soonest reset,
    // 1 session): 5 x 3 / 4 = 3.75 against 1 x 5 / 2 = 2.5.
    assert_eq!(v["next"]["account"], "personal");
    assert_eq!(v["next"]["reason"], "highest_score");
    assert_eq!(v["next"]["score"], 3.75);
    assert_eq!(
        v["next"]["runner_up"],
        json!({"account": "team-1", "score": 2.5})
    );
    assert_eq!(row(&v, "team-1")["eligible"], true);
    assert_eq!(row(&v, "org-seat")["factors"]["priority"], 200);
    assert_eq!(row(&v, "team-2")["excluded"]["reason"], "rate_limited");
    assert_eq!(row(&v, "old-laptop")["excluded"]["reason"], "needs_login");
    assert_eq!(row(&v, "lapsed")["excluded"]["reason"], "billing");
    assert_eq!(row(&v, "org-seat")["excluded"]["reason"], "access_denied");

    let mut shown = v.clone();
    shown.as_object_mut().unwrap().remove("models");
    insta::assert_snapshot!(serde_json::to_string_pretty(&shown).unwrap());
}

/// Two tied accounts: every preview names the account the next real request is served by, and
/// previewing changes nothing.
#[tokio::test]
async fn preview_matches_the_served_account_and_has_no_side_effects() {
    let fake = FakeAnthropic::start().await;
    fake.route(COUNT, |_, _| Reply::json(200, &json!({"input_tokens": 1})));
    let pool = Pool::new();
    pool.add_account("other", ProviderId::Anthropic, 100, "other-access-SECRET");
    let srv = pool
        .boot(Boot::default().provider(anthropic(&pool, &fake)))
        .await;
    let app = srv.app.clone();
    // A pinned session on `work`: the preview shows it and must leave it alone.
    let now = OffsetDateTime::now_utc();
    app.sessions
        .set(ProviderId::Anthropic, "user-pinned", &pool.meta.id, now);
    let journal_rows = || async {
        let _ = pool.store.exec(|_| Ok(())).await;
        pool.store
            .read(|c| {
                Ok(
                    c.query_row("SELECT count(*) FROM journal_requests", [], |r| {
                        r.get::<_, i64>(0)
                    })?,
                )
            })
            .unwrap()
    };

    let mut served = Vec::new();
    for _ in 0..4 {
        let cursor = app.cursor.load(Ordering::SeqCst);
        let rows = journal_rows().await;
        let mut picks = Vec::new();
        for _ in 0..3 {
            let r = http_get(
                srv.addr,
                &format!("{PREVIEW}?model=claude-sonnet-5"),
                &[MGMT],
            )
            .await;
            assert_eq!(r.status, 200, "{}", r.text());
            picks.push(r.json());
        }
        assert_eq!(app.cursor.load(Ordering::SeqCst), cursor, "cursor moved");
        assert_eq!(journal_rows().await, rows, "preview was journaled");
        let pinned = app.sessions.pinned(
            ProviderId::Anthropic,
            "user-pinned",
            OffsetDateTime::now_utc(),
        );
        assert_eq!(pinned.as_ref(), Some(&pool.meta.id), "pin changed");
        assert_eq!(
            app.sessions
                .active_count(&pool.meta.id, OffsetDateTime::now_utc()),
            1
        );
        let next = picks[0]["next"]["account"].as_str().unwrap().to_string();
        assert!(picks.iter().all(|p| p["next"]["account"] == next.as_str()));
        assert_eq!(picks[0]["sticky"]["pinned_sessions"], 1);
        assert_eq!(row(&picks[0], "work")["factors"]["sessions"], 1);

        let body = json!({"model": "claude-sonnet-5",
            "messages": [{"role": "user", "content": "hi"}]});
        let r = http_req(
            srv.addr,
            "POST",
            COUNT,
            &[("content-type", "application/json")],
            &serde_json::to_vec(&body).unwrap(),
        )
        .await;
        assert_eq!(r.status, 200, "{}", r.text());
        assert_eq!(r.header("x-pool-account"), Some(next.as_str()));
        served.push(next);
    }
    // `work` carries a pinned session, so `other` scores higher every time.
    assert_eq!(served, ["other", "other", "other", "other"]);

    // Without the pin the two tie: the preview follows the round-robin cursor.
    app.sessions.evict_account(&pool.meta.id);
    let mut served = Vec::new();
    for _ in 0..4 {
        let v = http_get(
            srv.addr,
            &format!("{PREVIEW}?model=claude-sonnet-5"),
            &[MGMT],
        )
        .await
        .json();
        assert_eq!(v["next"]["reason"], "tie_break", "{v}");
        let next = v["next"]["account"].as_str().unwrap().to_string();
        let body = json!({"model": "claude-sonnet-5",
            "messages": [{"role": "user", "content": "hi"}]});
        let r = http_req(
            srv.addr,
            "POST",
            COUNT,
            &[("content-type", "application/json")],
            &serde_json::to_vec(&body).unwrap(),
        )
        .await;
        assert_eq!(r.header("x-pool-account"), Some(next.as_str()));
        served.push(next);
    }
    assert_ne!(served[0], served[1], "ties alternate: {served:?}");
    assert_eq!(served[0], served[2]);
}
