//! Task F5: `GET /api/v1/analytics/burn` (weekly subscription burn) over the real server.

use crate::support;

use pooltime::store::usage_history::{insert_sample, Point, WINDOW_7D};
use serde_json::Value;
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
    // The runway forecast rides along: `gone`'s old reading means the history covers the whole
    // trailing day, so `work`'s 0.3 is a rate over 24 h; no account has a known 7-day window
    // (no usage snapshot), so there is nothing to project.
    let runway = &v["runway"];
    assert_eq!(runway["state"], "unknown", "{v}");
    assert_eq!(runway["rate_per_hour"], 0.0125, "{v}");
    assert_eq!(runway["rate_window_hours"], 24.0, "{v}");
    assert_eq!(runway["headroom"], 0.0, "{v}");
    assert_eq!(runway["at_risk"], false, "{v}");
    assert_eq!(runway["warn_hours"], 6, "{v}");

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
