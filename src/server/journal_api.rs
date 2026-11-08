//! Debugging endpoints of the management API (spec §6): the request journal, the account
//! timeline, per-account stats and the time-boxed debug mode. Every route needs the management
//! key (bearer or dashboard session), like the mutations: the journal is local operational detail.
//!
//! | Route | Answer |
//! |---|---|
//! | `GET /api/v1/requests?errors_only=true&limit=N` | newest first; `limit` default 50, max 500 |
//! | `GET /api/v1/requests/{id}` | `{request, attempts}`, 404 when unknown |
//! | `GET /api/v1/timeline?account=&limit=` | account state transitions, newest first |
//! | `GET /api/v1/stats` | per-account error rate and attempt latency p50/p95 over 24 h, storage |
//! | `GET/POST/DELETE /api/v1/debug` | debug-mode state; `POST {"minutes": 1..=60}` turns it on |
//!
//! With `POOLTIME_FIXTURE_STATE` set, the reads answer from the fixture's `journal` object
//! (`requests`, `details`, `timeline`, `stats`) so screenshots can show these views.

// Parse errors are the response to send as-is (as in `mgmt`).
#![allow(clippy::result_large_err)]

use std::collections::HashMap;
use std::sync::Arc;

use axum::body::Body;
use axum::extract::{Path, Query, State};
use axum::response::Response;
use http::HeaderMap;
use serde_json::{json, Value};
use time::OffsetDateTime;

use super::mgmt::{bad, blocking, internal, ok, read_object, storage_json, ts};
use super::AppState;
use crate::logging::DEBUG_MAX_MINUTES;
use crate::providers::anthropic::anthropic_error;
use crate::store::{journal, timeline};

/// Default and maximum page sizes.
pub const DEFAULT_LIMIT: u32 = 50;
pub const MAX_LIMIT: u32 = 500;
/// Stats window.
const STATS_WINDOW_SECS: i64 = 24 * 3600;

type Params = Query<HashMap<String, String>>;

fn limit_of(q: &HashMap<String, String>) -> Result<u32, Response> {
    match q.get("limit").map(String::as_str) {
        None | Some("") => Ok(DEFAULT_LIMIT),
        Some(s) => match s.parse::<u32>() {
            Ok(n) if n >= 1 => Ok(n.min(MAX_LIMIT)),
            _ => Err(bad("limit must be a positive integer")),
        },
    }
}

fn flag(q: &HashMap<String, String>, name: &str) -> Result<bool, Response> {
    match q.get(name).map(String::as_str) {
        None | Some("") | Some("false") | Some("0") => Ok(false),
        Some("true") | Some("1") => Ok(true),
        Some(_) => Err(bad(format!("{name} must be true or false"))),
    }
}

fn time_of(unix: i64) -> String {
    OffsetDateTime::from_unix_timestamp(unix)
        .map(ts)
        .unwrap_or_default()
}

/// `v` (an object with a unix `ts`) plus its RFC 3339 `time`.
fn with_time(mut v: Value) -> Value {
    if let Some(t) = v.get("ts").and_then(Value::as_i64) {
        v["time"] = Value::String(time_of(t));
    }
    v
}

fn fixture<'a>(app: &'a AppState, key: &str) -> Option<&'a Value> {
    app.fixture_state.as_ref().map(|f| &f["journal"][key])
}

fn not_found(id: &str) -> Response {
    anthropic_error(404, "not_found_error", &format!("Unknown request: {id}"))
}

pub(super) async fn requests(State(app): State<Arc<AppState>>, Query(q): Params) -> Response {
    let (errors_only, limit) = match (flag(&q, "errors_only"), limit_of(&q)) {
        (Ok(e), Ok(l)) => (e, l),
        (Err(r), _) | (_, Err(r)) => return r,
    };
    if let Some(f) = fixture(&app, "requests") {
        let rows: Vec<Value> = f
            .as_array()
            .into_iter()
            .flatten()
            .filter(|r| {
                !errors_only
                    || !matches!(
                        r["outcome"].as_str(),
                        Some("completed" | "client_cancelled")
                    )
            })
            .take(limit as usize)
            .cloned()
            .collect();
        return ok(json!({ "errors_only": errors_only, "limit": limit, "requests": rows }));
    }
    let a = app.clone();
    match blocking(move || a.store.read(|c| journal::list(c, errors_only, limit))).await {
        Ok(Ok(rows)) => {
            let rows: Vec<Value> = rows
                .into_iter()
                .map(|r| with_time(serde_json::to_value(r).unwrap_or_default()))
                .collect();
            ok(json!({ "errors_only": errors_only, "limit": limit, "requests": rows }))
        }
        Ok(Err(e)) => {
            tracing::warn!(error = %format!("{e:#}"), "reading the request journal failed");
            internal("could not read the request journal")
        }
        Err(r) => r,
    }
}

pub(super) async fn request(State(app): State<Arc<AppState>>, Path(id): Path<String>) -> Response {
    if let Some(f) = fixture(&app, "details") {
        return match f.get(&id) {
            Some(v) => ok(v.clone()),
            None => not_found(&id),
        };
    }
    let (a, key) = (app.clone(), id.clone());
    match blocking(move || a.store.read(|c| journal::get(c, &key))).await {
        Ok(Ok(Some((r, attempts)))) => ok(json!({
            "request": with_time(serde_json::to_value(r).unwrap_or_default()),
            "attempts": attempts,
        })),
        Ok(Ok(None)) => not_found(&id),
        Ok(Err(e)) => {
            tracing::warn!(error = %format!("{e:#}"), "reading the request journal failed");
            internal("could not read the request journal")
        }
        Err(r) => r,
    }
}

pub(super) async fn timeline(State(app): State<Arc<AppState>>, Query(q): Params) -> Response {
    let limit = match limit_of(&q) {
        Ok(l) => l,
        Err(r) => return r,
    };
    let account = q.get("account").filter(|s| !s.is_empty()).cloned();
    if let Some(f) = fixture(&app, "timeline") {
        let rows: Vec<Value> = f
            .as_array()
            .into_iter()
            .flatten()
            .filter(|r| account.as_deref().is_none_or(|a| r["account"] == a))
            .take(limit as usize)
            .cloned()
            .collect();
        return ok(json!({ "events": rows }));
    }
    let a = app.clone();
    match blocking(move || {
        a.store
            .read(|c| timeline::list(c, account.as_deref(), limit))
    })
    .await
    {
        Ok(Ok(rows)) => ok(json!({
            "events": rows
                .into_iter()
                .map(|r| with_time(serde_json::to_value(r).unwrap_or_default()))
                .collect::<Vec<_>>(),
        })),
        Ok(Err(e)) => {
            tracing::warn!(error = %format!("{e:#}"), "reading the account timeline failed");
            internal("could not read the account timeline")
        }
        Err(r) => r,
    }
}

pub(super) async fn stats(State(app): State<Arc<AppState>>) -> Response {
    if let Some(f) = fixture(&app, "stats") {
        return ok(f.clone());
    }
    let now = OffsetDateTime::now_utc();
    let since = now.unix_timestamp() - STATS_WINDOW_SECS;
    let a = app.clone();
    let res = blocking(move || {
        let rows = a
            .store
            .read(|c| Ok((journal::account_stats(c, since)?, journal::count(c)?)));
        (rows, storage_json(&a.paths))
    })
    .await;
    match res {
        Ok((Ok((accounts, n)), storage)) => ok(json!({
            "window_secs": STATS_WINDOW_SECS,
            "since": time_of(since),
            "accounts": accounts,
            "journal_requests": n,
            "storage": storage,
        })),
        Ok((Err(e), _)) => {
            tracing::warn!(error = %format!("{e:#}"), "reading journal stats failed");
            internal("could not read journal stats")
        }
        Err(r) => r,
    }
}

fn debug_json(app: &AppState) -> Value {
    let until = app.debug.until();
    json!({
        "active": until.is_some(),
        "until": until.map(ts),
        "max_minutes": DEBUG_MAX_MINUTES,
    })
}

pub(super) async fn debug_get(State(app): State<Arc<AppState>>) -> Response {
    ok(debug_json(&app))
}

pub(super) async fn debug_post(
    State(app): State<Arc<AppState>>,
    headers: HeaderMap,
    body: Body,
) -> Response {
    let body = match read_object(&headers, body).await {
        Ok(b) => b,
        Err(r) => return r,
    };
    let minutes = body
        .get("minutes")
        .and_then(Value::as_u64)
        .and_then(|m| u32::try_from(m).ok());
    match minutes.and_then(|m| app.debug.enable(m)) {
        Some(_) => ok(debug_json(&app)),
        None => bad(format!(
            "minutes must be an integer from 1 to {DEBUG_MAX_MINUTES}"
        )),
    }
}

pub(super) async fn debug_delete(State(app): State<Arc<AppState>>) -> Response {
    app.debug.disable();
    ok(debug_json(&app))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn q(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn limit_defaults_and_caps() {
        assert_eq!(limit_of(&q(&[])).unwrap(), 50);
        assert_eq!(limit_of(&q(&[("limit", "10")])).unwrap(), 10);
        assert_eq!(limit_of(&q(&[("limit", "9999")])).unwrap(), 500);
        assert!(limit_of(&q(&[("limit", "0")])).is_err());
        assert!(limit_of(&q(&[("limit", "x")])).is_err());
        assert!(flag(&q(&[("errors_only", "true")]), "errors_only").unwrap());
        assert!(!flag(&q(&[]), "errors_only").unwrap());
        assert!(flag(&q(&[("errors_only", "maybe")]), "errors_only").is_err());
    }
}
