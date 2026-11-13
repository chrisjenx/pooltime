//! The six MCP tools. Each one calls the `/api/v1` handler the dashboard and CLI use (in
//! process, no auth layer: the MCP token already admitted the call) and reshapes its answer.
//! Outputs are JSON text and never carry tokens, refresh details, emails or keys; accounts
//! appear by id.
//!
//! | Tool | Management handler |
//! |---|---|
//! | `pool_health` | `GET /api/v1/status` |
//! | `pool_usage` | `GET /api/v1/status` + `GET /api/v1/analytics/burn?weeks=1` |
//! | `start_relogin {account}` | `POST /api/v1/accounts/{id}/login` (callback mode) |
//! | `set_weight {account, weight?, priority?, until?}` | `POST /api/v1/accounts/{id}/routing` |
//! | `recent_errors {limit?}` | `GET /api/v1/requests?errors_only=true` |
//! | `explain_request {id}` | `GET /api/v1/requests/{id}` |

use std::collections::HashMap;
use std::sync::Arc;

use axum::body::Body;
use axum::extract::{Path, Query, State};
use axum::response::Response;
use http::HeaderMap;
use serde_json::{json, Map, Value};
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;

use super::INVALID_PARAMS;
use crate::accounts::AccountId;
use crate::server::{analytics_api, journal_api, mgmt, AppState};

/// `recent_errors` page size: default and cap.
const ERRORS_DEFAULT: u64 = 10;
const ERRORS_MAX: u64 = 50;
/// Largest relative `until` accepted (one year).
const MAX_UNTIL_HOURS: i64 = 366 * 24;
/// Management answers are small; this only bounds a runaway.
const ANSWER_LIMIT: usize = 8 * 1024 * 1024;

const UNTIL_MSG: &str = "until must be an RFC 3339 timestamp (e.g. 2026-11-14T09:00:00Z) or a \
    duration from now in hours or days (e.g. 12h, 2d)";

/// The `tools/list` result.
pub(super) fn list() -> Value {
    let account = json!({"type": "string", "description": "Account id, as shown by pool_health"});
    json!({ "tools": [
        {
            "name": "pool_usage",
            "description": "Subscription usage: each account's 7-day usage as the share used (0-1, \
                seven_day_used; seven_day_left is its complement) and windows, the pool total \
                used (seven_day_used_total, in subscriptions), this week's burn and the \
                forecast for the week.",
            "inputSchema": {"type": "object", "properties": {}},
        },
        {
            "name": "pool_health",
            "description": "Account health: state (ready, rate_limited, needs_login, expiring, \
                erroring, ...), since when, why, when it clears (until), login expiry with a \
                countdown, and routing priority/weight including any timed override.",
            "inputSchema": {"type": "object", "properties": {}},
        },
        {
            "name": "start_relogin",
            "description": "Start a sign-in for an existing account. Returns the sign-in link \
                to open in a browser on this machine; pooltime completes it by itself.",
            "inputSchema": {
                "type": "object",
                "properties": {"account": account},
                "required": ["account"],
            },
        },
        {
            "name": "set_weight",
            "description": "Set an account's routing priority and/or weight. With until, the \
                weight is temporary and reverts automatically at that time (RFC 3339, or \
                12h / 2d from now); until: null cancels an active override.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "account": account,
                    "weight": {"type": "number", "description": "Routing weight"},
                    "priority": {"type": "integer", "minimum": 0,
                        "description": "Routing priority"},
                    "until": {"type": ["string", "null"],
                        "description": "When a temporary weight reverts: RFC 3339, or Nh / Nd"},
                },
                "required": ["account"],
            },
        },
        {
            "name": "recent_errors",
            "description": "The most recent failed requests, newest first: outcome, \
                classified reason, model, account and timing.",
            "inputSchema": {
                "type": "object",
                "properties": {"limit": {"type": "integer", "minimum": 1,
                    "maximum": ERRORS_MAX, "default": ERRORS_DEFAULT}},
            },
        },
        {
            "name": "explain_request",
            "description": "Explain one request by id (the x-pooltime-request-id header or an \
                id from recent_errors): its outcome and every attempt in its account chain.",
            "inputSchema": {
                "type": "object",
                "properties": {"id": {"type": "string", "description": "Request id"}},
                "required": ["id"],
            },
        },
    ]})
}

/// `tools/call`: a tool result (`isError` for a tool-level failure), or a JSON-RPC error for an
/// unknown tool or malformed params.
pub(super) async fn call(app: &Arc<AppState>, params: &Value) -> Result<Value, (i64, String)> {
    let Some(name) = params["name"].as_str() else {
        return Err((INVALID_PARAMS, "params.name is required".into()));
    };
    let args = match params.get("arguments") {
        None | Some(Value::Null) => Map::new(),
        Some(Value::Object(m)) => m.clone(),
        Some(_) => return Err((INVALID_PARAMS, "arguments must be an object".into())),
    };
    let out = match name {
        "pool_usage" => pool_usage(app).await,
        "pool_health" => pool_health(app).await,
        "start_relogin" => start_relogin(app, &args).await,
        "set_weight" => set_weight(app, &args).await,
        "recent_errors" => recent_errors(app, &args).await,
        "explain_request" => explain_request(app, &args).await,
        other => return Err((INVALID_PARAMS, format!("Unknown tool: {other}"))),
    };
    Ok(match out {
        Ok(v) => tool_result(serde_json::to_string_pretty(&v).unwrap_or_default(), false),
        Err(msg) => tool_result(msg, true),
    })
}

fn tool_result(text: String, is_error: bool) -> Value {
    json!({ "content": [{ "type": "text", "text": text }], "isError": is_error })
}

/// A management handler's answer: its JSON on success, its `error.message` otherwise.
async fn answer(resp: Response) -> Result<Value, String> {
    let ok = resp.status().is_success();
    let bytes = axum::body::to_bytes(resp.into_body(), ANSWER_LIMIT)
        .await
        .map_err(|_| "pooltime: could not read the management answer".to_string())?;
    let v: Value = serde_json::from_slice(&bytes)
        .map_err(|_| "pooltime: the management answer was not JSON".to_string())?;
    if ok {
        Ok(v)
    } else {
        Err(v["error"]["message"]
            .as_str()
            .unwrap_or("pooltime: request failed")
            .to_string())
    }
}

/// The `/api/v1/status` document as the unauthenticated view sees it (no local paths).
async fn status(app: &Arc<AppState>) -> Result<Value, String> {
    answer(mgmt::status(State(app.clone()), HeaderMap::new()).await).await
}

fn query(pairs: &[(&str, String)]) -> Query<HashMap<String, String>> {
    Query(
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.clone()))
            .collect(),
    )
}

fn str_arg<'a>(args: &'a Map<String, Value>, name: &str) -> Result<&'a str, String> {
    args.get(name)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| format!("{name} is required (a string)"))
}

fn account_arg(args: &Map<String, Value>) -> Result<AccountId, String> {
    str_arg(args, "account")?
        .parse::<AccountId>()
        .map_err(|e| e.to_string())
}

fn parse_ts(v: &Value) -> Option<OffsetDateTime> {
    v.as_str()
        .and_then(|s| OffsetDateTime::parse(s, &Rfc3339).ok())
}

/// Whole seconds from `now` to the timestamp in `v` (negative once passed), `null` if none.
fn countdown(v: &Value, now: Option<OffsetDateTime>) -> Value {
    match (parse_ts(v), now) {
        (Some(t), Some(now)) => json!((t - now).whole_seconds()),
        _ => Value::Null,
    }
}

fn accounts(doc: &Value) -> impl Iterator<Item = &Value> {
    doc["accounts"].as_array().into_iter().flatten()
}

async fn pool_health(app: &Arc<AppState>) -> Result<Value, String> {
    let doc = status(app).await?;
    let now = parse_ts(&doc["now"]);
    let rows: Vec<Value> = accounts(&doc)
        .map(|a| {
            json!({
                "id": a["id"],
                "provider": a["provider"],
                "status": a["status"],
                "since": a["status_since"],
                "why": a["reason"],
                "until": a["until"],
                "until_in_secs": countdown(&a["until"], now),
                "error_class": a["class"],
                "error_count": a["count"],
                "login_expires_at": a["refresh_expires_at"],
                "login_expires_in_secs": countdown(&a["refresh_expires_at"], now),
                "last_error": a["last_error"],
                "priority": a["priority"],
                "weight": a["weight"],
                "weight_override": a["weight_override"],
                "effective_weight": a["effective_weight"],
                "in_flight": a["in_flight"],
            })
        })
        .collect();
    Ok(json!({
        "now": doc["now"],
        "accounts": rows,
        "pool": doc["pool"],
        "counts": doc["counts"],
        "expiry_warn_secs": doc["expiry_warn_secs"],
    }))
}

async fn pool_usage(app: &Arc<AppState>) -> Result<Value, String> {
    let doc = status(app).await?;
    let burn =
        answer(analytics_api::burn(State(app.clone()), query(&[("weeks", "1".into())])).await)
            .await?;
    let rows: Vec<Value> = accounts(&doc)
        .map(|a| {
            json!({
                "id": a["id"],
                "provider": a["provider"],
                "status": a["status"],
                "seven_day_used": a["seven_day_used"],
                "seven_day_left": a["seven_day_left"],
                "windows": a["windows"],
            })
        })
        .collect();
    let this_week = burn["weeks"]
        .as_array()
        .and_then(|w| w.last())
        .cloned()
        .unwrap_or_else(|| json!({}));
    Ok(json!({
        "now": doc["now"],
        "accounts": rows,
        "pool": doc["pool"],
        "this_week": this_week,
        "forecast": burn["forecast"],
        "idle_accounts": burn["idle_accounts"],
    }))
}

async fn existing(app: &Arc<AppState>, id: &AccountId) -> Result<(), String> {
    let (a, rid) = (app.clone(), id.clone());
    match mgmt::blocking(move || a.registry.get(&rid).is_some()).await {
        Ok(true) => Ok(()),
        Ok(false) => Err(format!("Unknown account: {id}")),
        Err(r) => answer(r).await.map(|_| ()),
    }
}

/// Only existing accounts (adding one is not this tool's job). Callback mode: the browser's
/// redirect to the local listener finishes the flow, so nothing has to be pasted back. The
/// answer is the link and an instruction: never the sign-in id, verifier or any secret.
async fn start_relogin(app: &Arc<AppState>, args: &Map<String, Value>) -> Result<Value, String> {
    let id = account_arg(args)?;
    existing(app, &id).await?;
    let started = answer(
        mgmt::login(
            State(app.clone()),
            Path(id.to_string()),
            HeaderMap::new(),
            Body::from(json!({ "mode": "callback" }).to_string()),
        )
        .await,
    )
    .await?;
    let url = started["authorize_url"]
        .as_str()
        .ok_or("pooltime: sign-in started without a link")?;
    Ok(json!({
        "account": id.as_str(),
        "authorize_url": url,
        "expires_at": started["expires_at"],
        "instruction": format!(
            "Open authorize_url in a browser on this machine and sign in as the owner of account \
             '{id}'. pooltime finishes the sign-in by itself when the browser comes back; run \
             pool_health afterwards to confirm. The link stops working at expires_at."
        ),
    }))
}

/// `until` as RFC 3339 (passed through; the routing handler requires it to be in the future) or
/// `Nh` / `Nd` from `now`.
fn parse_until(raw: &str, now: OffsetDateTime) -> Result<OffsetDateTime, String> {
    let s = raw.trim();
    if let Ok(t) = OffsetDateTime::parse(s, &Rfc3339) {
        return Ok(t);
    }
    let (num, hours_per) = match (s.strip_suffix('h'), s.strip_suffix('d')) {
        (Some(n), _) => (n, 1),
        (_, Some(n)) => (n, 24),
        _ => return Err(UNTIL_MSG.into()),
    };
    // Overflow-safe: a huge count never multiplies, adds or builds a Duration out of range.
    let range = || {
        format!(
            "until must be between 1h and {}d from now",
            MAX_UNTIL_HOURS / 24
        )
    };
    let n = num
        .trim()
        .parse::<i64>()
        .map_err(|_| match num.trim().parse::<u128>() {
            Ok(_) => range(),
            Err(_) => UNTIL_MSG.to_string(),
        })?;
    let hours = n
        .checked_mul(hours_per)
        .filter(|h| (1..=MAX_UNTIL_HOURS).contains(h))
        .ok_or_else(range)?;
    now.checked_add(time::Duration::hours(hours))
        .ok_or_else(range)
}

/// Priority and weight pass through unchanged, so the routing handler validates them exactly as
/// for the CLI and dashboard; `until` is normalized to RFC 3339 first.
async fn set_weight(app: &Arc<AppState>, args: &Map<String, Value>) -> Result<Value, String> {
    let id = account_arg(args)?;
    let mut body = Map::new();
    for k in ["weight", "priority"] {
        if let Some(v) = args.get(k) {
            body.insert(k.into(), v.clone());
        }
    }
    match args.get("until") {
        None => {}
        Some(Value::Null) => {
            body.insert("until".into(), Value::Null);
        }
        Some(Value::String(s)) => {
            let t = parse_until(s, OffsetDateTime::now_utc())?;
            body.insert("until".into(), json!(mgmt::ts(t)));
        }
        Some(_) => return Err(UNTIL_MSG.into()),
    }
    let mut v = answer(
        mgmt::routing(
            State(app.clone()),
            Path(id.to_string()),
            HeaderMap::new(),
            Body::from(Value::Object(body).to_string()),
        )
        .await,
    )
    .await?;
    let note = match v["weight_override"]["until"].as_str() {
        Some(until) => format!(
            "weight {} applies until {until}, then reverts to {} automatically",
            v["effective_weight"], v["weight"]
        ),
        None => format!(
            "priority {}, weight {} (no timed override)",
            v["priority"], v["weight"]
        ),
    };
    if let Value::Object(o) = &mut v {
        o.remove("ok");
        o.insert("note".into(), json!(note));
    }
    Ok(v)
}

async fn recent_errors(app: &Arc<AppState>, args: &Map<String, Value>) -> Result<Value, String> {
    let limit = match args.get("limit") {
        None | Some(Value::Null) => ERRORS_DEFAULT,
        Some(v) => match v.as_u64() {
            Some(n) if n >= 1 => n.min(ERRORS_MAX),
            _ => return Err(format!("limit must be an integer from 1 to {ERRORS_MAX}")),
        },
    };
    answer(
        journal_api::requests(
            State(app.clone()),
            query(&[("errors_only", "true".into()), ("limit", limit.to_string())]),
        )
        .await,
    )
    .await
}

async fn explain_request(app: &Arc<AppState>, args: &Map<String, Value>) -> Result<Value, String> {
    let id = str_arg(args, "id")?;
    answer(journal_api::request(State(app.clone()), Path(id.to_string())).await).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::datetime;

    #[test]
    fn until_forms() {
        let now = datetime!(2026-11-08 10:00 UTC);
        assert_eq!(
            parse_until("12h", now).unwrap(),
            datetime!(2026-11-08 22:00 UTC)
        );
        assert_eq!(
            parse_until(" 2d ", now).unwrap(),
            datetime!(2026-11-10 10:00 UTC)
        );
        assert_eq!(
            parse_until("2026-11-14T09:00:00Z", now).unwrap(),
            datetime!(2026-11-14 09:00 UTC)
        );
        for bad in [
            "until friday",
            "friday",
            "3w",
            "0h",
            "-1d",
            "d",
            "",
            "1.5h",
            "9999d",
            "400000000000000000d",
            "9223372036854775807h",
            "99999999999999999999999h",
        ] {
            let e = parse_until(bad, now).unwrap_err();
            assert!(e.contains("until"), "{bad}: {e}");
        }
    }

    #[test]
    fn countdown_seconds() {
        let now = Some(datetime!(2026-11-08 10:00 UTC));
        assert_eq!(countdown(&json!("2026-11-08T10:01:00Z"), now), json!(60));
        assert_eq!(countdown(&Value::Null, now), Value::Null);
    }

    #[test]
    fn tool_list_is_exactly_the_scoped_six() {
        let v = list();
        let names: Vec<&str> = v["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect();
        assert_eq!(
            names,
            [
                "pool_usage",
                "pool_health",
                "start_relogin",
                "set_weight",
                "recent_errors",
                "explain_request"
            ]
        );
    }
}
