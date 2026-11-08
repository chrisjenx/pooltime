//! The management API (`/api/v1/*`): the one layer the dashboard, the CLI and MCP all call.
//!
//! # Auth
//! Reads (`GET`) need nothing beyond the Host/Origin guard (spec §16); every mutation needs the
//! management key as `Authorization: Bearer <key>` or the dashboard session cookie. The key is 32
//! random bytes in hex at `run/mgmt.key` (0600, [`ensure_mgmt_key`]). `POST /api/v1/session`
//! exchanges the key for the cookie `pooltime_mgmt`, whose value is **not** the key but a token
//! derived from it ([`session_token`], SHA-256 over a fixed label and the key), so the cookie jar
//! never holds the key itself. Cookie: `HttpOnly; SameSite=Strict; Path=/`, no `Secure` (plain
//! http on localhost), no `Max-Age` (a browser session). There is no CORS: a foreign page cannot
//! read a response, and the Host/Origin guard refuses it before auth is even consulted.
//!
//! # Errors
//! Same envelope as the inference side, `{"type":"error","error":{"type","message"}}`, so
//! clients read `.error.message` everywhere. Status words are the five of spec §16
//! (`ready | needs_login | rate_limited | billing | access_denied`).
//!
//! [`build_status`] is a pure function of [`StatusInputs`] and a clock, which is what makes the
//! status shape snapshot-testable; [`gather`] does the I/O.

#![allow(clippy::result_large_err)]

use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};

use axum::body::Body;
use axum::extract::{Path, Request, State};
use axum::middleware::{from_fn_with_state, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post, MethodRouter};
use axum::{Json, Router};
use http::header::{AUTHORIZATION, CACHE_CONTROL, COOKIE, SET_COOKIE};
use http::{HeaderMap, HeaderValue, StatusCode};
use rand::RngExt;
use rusqlite::OptionalExtension;
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;

use super::auth::keys_match;
use super::router::{method_not_allowed, read_json_limited};
use super::AppState;
use crate::accounts::registry::{MAX_WEIGHT, MIN_WEIGHT};
use crate::accounts::status::window_duration;
use crate::accounts::{
    AccountId, AccountMeta, AccountState, AccountStatus, Identity, TimedOverride, Window,
};
use crate::config::{Config, Paths};
use crate::models::Mapping;
use crate::providers::anthropic::anthropic_error;
use crate::providers::ProviderId;
use crate::routing::Tuning;
use crate::signin::claude::ClaudeSignIn;
use crate::signin::{
    PendingSignIn, SignInFlow, SignInHandle, SignInMode, SignInResult, SubmitError,
};

/// Name of the dashboard session cookie.
pub const SESSION_COOKIE: &str = "pooltime_mgmt";

/// Management bodies are tiny; anything larger is refused.
const BODY_LIMIT: usize = 64 * 1024;

/// How long a finished or abandoned sign-in stays readable after its expiry.
const SIGNIN_RETENTION: time::Duration = time::Duration::minutes(10);

// ---- management key & session token -----------------------------------------------------------

/// `run/mgmt.key`.
pub fn mgmt_key_path(paths: &Paths) -> PathBuf {
    paths.run.join("mgmt.key")
}

fn is_valid_key(k: &str) -> bool {
    k.len() == 64 && k.bytes().all(|b| b.is_ascii_hexdigit())
}

/// The management key, created (32 random bytes, hex, mode 0600, atomically) when absent. A file
/// that is not 64 hex characters is replaced rather than used as a weak key. Call at serve start.
pub fn ensure_mgmt_key(paths: &Paths) -> anyhow::Result<String> {
    paths.ensure()?;
    let path = mgmt_key_path(paths);
    match fs::read_to_string(&path) {
        Ok(s) if is_valid_key(s.trim()) => {
            crate::fsutil::ensure_mode(&path, 0o600)?;
            return Ok(s.trim().to_string());
        }
        Ok(_) => tracing::warn!("run/mgmt.key is malformed; generating a new management key"),
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(anyhow::Error::new(e).context("reading run/mgmt.key")),
    }
    let key = hex::encode(rand::rng().random::<[u8; 32]>());
    crate::fsutil::atomic_write(&path, format!("{key}\n").as_bytes(), 0o600)?;
    Ok(key)
}

/// The cookie value that stands for `key`: hex SHA-256 of a fixed label and the key.
pub fn session_token(key: &str) -> String {
    let mut h = Sha256::new();
    h.update(b"pooltime-mgmt-session-v1\0");
    h.update(key.as_bytes());
    hex::encode(h.finalize())
}

/// The `Set-Cookie` value that opens a dashboard session for `key`.
pub fn session_cookie_header(key: &str) -> String {
    format!(
        "{SESSION_COOKIE}={}; HttpOnly; SameSite=Strict; Path=/",
        session_token(key)
    )
}

/// Whether the request carries the key (bearer) or the session cookie derived from it.
fn authorized(h: &HeaderMap, key: &str) -> bool {
    if key.is_empty() {
        return false; // never "open": an unset key authorizes nothing
    }
    let bearer = h
        .get(AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| {
            let (scheme, rest) = v.split_once(' ')?;
            scheme.eq_ignore_ascii_case("bearer").then_some(rest)
        })
        .is_some_and(|k| keys_match(k, key));
    if bearer {
        return true;
    }
    let token = session_token(key);
    h.get_all(COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .filter_map(|pair| pair.trim().split_once('='))
        .any(|(name, value)| name == SESSION_COOKIE && keys_match(value, &token))
}

async fn require_mgmt(State(app): State<Arc<AppState>>, req: Request, next: Next) -> Response {
    if authorized(req.headers(), &app.mgmt_key) {
        next.run(req).await
    } else {
        anthropic_error(
            401,
            "authentication_error",
            "pooltime: management key required (Authorization: Bearer <key> or a dashboard session)",
        )
    }
}

// ---- routes ----------------------------------------------------------------------------------

/// The `/api/v1/*` routes. Mutations sit behind [`require_mgmt`] as a route layer, so a wrong
/// verb answers 405 before any credential check (like the inference route).
pub fn routes(state: Arc<AppState>) -> Router<Arc<AppState>> {
    let read = |h: MethodRouter<Arc<AppState>>| h.fallback(|| async { method_not_allowed("GET") });
    let write = |h: MethodRouter<Arc<AppState>>, allow: &'static str| {
        h.route_layer(from_fn_with_state(state.clone(), require_mgmt))
            .fallback(move || async move { method_not_allowed(allow) })
    };
    Router::new()
        .route("/api/v1/status", read(get(status)))
        // The key is the credential here, so this one is not behind `require_mgmt`.
        .route(
            "/api/v1/session",
            post(session).fallback(|| async { method_not_allowed("POST") }),
        )
        .route("/api/v1/accounts", write(post(create_account), "POST"))
        .route(
            "/api/v1/accounts/{id}",
            write(delete(delete_account), "DELETE"),
        )
        .route("/api/v1/accounts/{id}/login", write(post(login), "POST"))
        .route(
            "/api/v1/accounts/{id}/routing",
            write(post(routing), "POST"),
        )
        .route(
            "/api/v1/accounts/{id}/recheck",
            write(post(recheck), "POST"),
        )
        .route("/api/v1/signin/{id}", read(get(signin_status)))
        .route("/api/v1/signin/{id}/code", write(post(signin_code), "POST"))
        .route("/api/v1/signin/{id}/bind", write(post(signin_bind), "POST"))
        .route("/api/v1/tuning", write(post(tuning), "POST"))
        .route("/api/v1/mappings", write(post(mappings), "POST"))
}

// ---- response helpers ------------------------------------------------------------------------

fn json_response(status: StatusCode, v: Value) -> Response {
    let mut r = (status, Json(v)).into_response();
    r.headers_mut()
        .insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
    r
}

fn ok(v: Value) -> Response {
    json_response(StatusCode::OK, v)
}

fn bad(msg: impl AsRef<str>) -> Response {
    anthropic_error(400, "invalid_request_error", msg.as_ref())
}

fn unknown_account(id: &AccountId) -> Response {
    anthropic_error(404, "not_found_error", &format!("Unknown account: {id}"))
}

fn internal(what: &str) -> Response {
    anthropic_error(500, "api_error", &format!("pooltime: {what}"))
}

fn parse_id(raw: &str) -> Result<AccountId, Response> {
    raw.parse::<AccountId>().map_err(|e| bad(e.to_string()))
}

async fn read_object(headers: &HeaderMap, body: Body) -> Result<Map<String, Value>, Response> {
    match read_json_limited(headers, body, BODY_LIMIT).await?.1 {
        Value::Object(m) => Ok(m),
        _ => Err(bad("request body must be a JSON object")),
    }
}

/// Run blocking work (disk, sqlite) off the async threads.
async fn blocking<T: Send + 'static>(
    f: impl FnOnce() -> T + Send + 'static,
) -> Result<T, Response> {
    tokio::task::spawn_blocking(f).await.map_err(|e| {
        tracing::error!(error = %e, "management task failed");
        internal("internal error")
    })
}

fn ts(t: OffsetDateTime) -> String {
    t.format(&Rfc3339).unwrap_or_default()
}

fn ts_opt(t: Option<OffsetDateTime>) -> Value {
    t.map_or(Value::Null, |t| Value::String(ts(t)))
}

fn identity_json(i: &Identity) -> Value {
    json!({ "email": i.email })
}

// ---- status ----------------------------------------------------------------------------------

/// What [`gather`] could say about an account's stored credentials (never the tokens).
#[derive(Debug, Clone, PartialEq)]
pub enum CredsInfo {
    /// No `credentials.json`: never logged in (or logged out).
    Missing,
    /// The file exists but cannot be used (corrupt).
    Unreadable,
    Present {
        expires_at: Option<OffsetDateTime>,
        refresh_expires_at: Option<OffsetDateTime>,
    },
}

/// Everything [`build_status`] shows for one account.
#[derive(Debug, Clone)]
pub struct AccountView {
    pub meta: AccountMeta,
    pub status: AccountStatus,
    /// When the account entered `status` (latest timeline row); `None` if never recorded.
    pub status_since: Option<OffsetDateTime>,
    pub state: AccountState,
    pub creds: CredsInfo,
    pub in_flight: u32,
    pub sessions: u32,
}

#[derive(Debug, Clone)]
pub struct StatusInputs {
    pub accounts: Vec<AccountView>,
    pub tuning: Tuning,
    pub mapping_enabled: bool,
    pub mappings: Vec<Mapping>,
}

fn round4(x: f64) -> f64 {
    (x * 10_000.0).round() / 10_000.0
}

/// Headroom left in the account-wide 7-day window (`1 - utilization`), `None` when unknown.
fn seven_day_left(windows: &[Window]) -> Option<f64> {
    let week = time::Duration::days(7);
    windows
        .iter()
        .find(|w| w.model.is_none() && window_duration(&w.key) == Some(week))
        .map(|w| round4(1.0 - w.utilization.clamp(0.0, 1.0)))
}

/// The classified reason behind a non-ready status (`None` when ready).
fn reason_of(v: &AccountView, now: OffsetDateTime) -> Option<String> {
    match &v.status {
        AccountStatus::Ready => None,
        AccountStatus::NeedsLogin => Some(
            match &v.creds {
                CredsInfo::Missing => "never_logged_in",
                CredsInfo::Unreadable => "credentials_unreadable",
                CredsInfo::Present { .. } if v.state.dead_refresh_fp.is_some() => {
                    "refresh_token_dead"
                }
                CredsInfo::Present {
                    refresh_expires_at: Some(t),
                    ..
                } if *t <= now => "refresh_token_expired",
                CredsInfo::Present { .. } => "credentials_unavailable",
            }
            .to_string(),
        ),
        AccountStatus::RateLimited {
            window: Some(w), ..
        } => Some(format!("quota:{w}")),
        AccountStatus::RateLimited { window: None, .. } => Some("quota".into()),
        AccountStatus::Billing => Some("billing".into()),
        AccountStatus::AccessDenied { .. } => Some("access_denied".into()),
    }
}

fn override_json(o: &TimedOverride) -> Value {
    json!({ "weight": o.weight, "until": ts(o.until) })
}

fn account_json(v: &AccountView, now: OffsetDateTime) -> Value {
    let dead = v.status == AccountStatus::NeedsLogin;
    // A dead account never shows leftover usage as headroom (INV §10).
    let windows: &[Window] = match (&v.state.snapshot, dead) {
        (Some(s), false) => &s.windows,
        _ => &[],
    };
    let until = match &v.status {
        AccountStatus::RateLimited { until, .. } | AccountStatus::AccessDenied { until } => {
            Some(*until)
        }
        _ => None,
    };
    let active = v.meta.weight_override.as_ref().filter(|o| o.until > now);
    let (expires_at, refresh_expires_at) = match &v.creds {
        CredsInfo::Present {
            expires_at,
            refresh_expires_at,
        } => (*expires_at, *refresh_expires_at),
        _ => (None, None),
    };
    json!({
        "id": v.meta.id.as_str(),
        "provider": v.meta.provider.as_str(),
        "status": v.status.label(),
        "status_since": ts_opt(v.status_since),
        "reason": reason_of(v, now),
        "until": ts_opt(until),
        "priority": v.meta.priority,
        "weight": v.meta.weight,
        "weight_override": active.map(override_json),
        "effective_weight": active.map_or(v.meta.weight, |o| o.weight),
        "windows": windows.iter().map(|w| json!({
            "key": w.key,
            "model": w.model,
            "utilization": w.utilization,
            "reset": ts_opt(w.reset),
        })).collect::<Vec<_>>(),
        "seven_day_left": seven_day_left(windows),
        "expires_at": ts_opt(expires_at),
        "refresh_expires_at": ts_opt(refresh_expires_at),
        "identity": identity_json(&v.meta.identity),
        "in_flight": v.in_flight,
        "sessions": v.sessions,
        "last_error": v.state.last_error.as_ref().map(|e| json!({
            "at": ts(e.at),
            "reason": e.reason.detail(),
        })),
    })
}

fn tuning_json(t: &Tuning) -> Value {
    json!({
        "five_hour_exp": t.five_hour_exp,
        "min_headroom": t.min_headroom,
        "headroom_taper_start": t.headroom_taper_start,
    })
}

fn mapping_json(enabled: bool, mappings: &[Mapping]) -> Value {
    json!({
        "enabled": enabled,
        "mappings": serde_json::to_value(mappings).unwrap_or(Value::Null),
    })
}

/// The `GET /api/v1/status` document at `now`. Pure: no clock, no I/O.
///
/// `pool.seven_day_left_total` counts one account as one subscription (spec §5): the sum of the
/// accounts' 7-day headroom, where an account with no known 7-day window (including every
/// account that needs login) contributes nothing.
pub fn build_status(inputs: &StatusInputs, now: OffsetDateTime) -> Value {
    let accounts: Vec<Value> = inputs
        .accounts
        .iter()
        .map(|v| account_json(v, now))
        .collect();
    let total: f64 = accounts
        .iter()
        .filter_map(|a| a["seven_day_left"].as_f64())
        .sum();
    let available = inputs
        .accounts
        .iter()
        .filter(|v| v.status == AccountStatus::Ready)
        .count();
    json!({
        "accounts": accounts,
        "pool": {
            "seven_day_left_total": round4(total),
            "accounts": inputs.accounts.len(),
            "available": available,
        },
        "tuning": tuning_json(&inputs.tuning),
        "mapping": mapping_json(inputs.mapping_enabled, &inputs.mappings),
        "version": env!("CARGO_PKG_VERSION"),
        "now": ts(now),
    })
}

/// Latest timeline row of `account`, when it is a move *into* `label`.
fn status_since(app: &AppState, account: &AccountId, label: &str) -> Option<OffsetDateTime> {
    let account = account.to_string();
    let row = app
        .store
        .read(|c| {
            Ok(c.query_row(
                "SELECT ts, to_state FROM timeline WHERE account = ?1 ORDER BY rowid DESC LIMIT 1",
                [account],
                |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)),
            )
            .optional()?)
        })
        .map_err(|e| tracing::warn!(error = %format!("{e:#}"), "timeline read failed"))
        .ok()??;
    (row.1 == label)
        .then(|| OffsetDateTime::from_unix_timestamp(row.0).ok())
        .flatten()
}

/// Read the live state `build_status` needs. Statuses are re-derived against the clock first, so
/// a cooldown that just ended (or credentials that just vanished) is reflected, and recorded in
/// the timeline, before `status_since` is read.
pub async fn gather(app: &Arc<AppState>, now: OffsetDateTime) -> Result<StatusInputs, Response> {
    let a = app.clone();
    let metas = blocking(move || {
        a.registry
            .list()
            .into_iter()
            .map(|meta| {
                let creds = match a.registry.read_creds(&meta.id) {
                    Ok(Some(c)) => CredsInfo::Present {
                        expires_at: c.expires_at,
                        refresh_expires_at: c.refresh_expires_at,
                    },
                    Ok(None) => CredsInfo::Missing,
                    Err(_) => CredsInfo::Unreadable,
                };
                a.state
                    .reevaluate(&meta.id, matches!(creds, CredsInfo::Present { .. }), now);
                (meta, creds)
            })
            .collect::<Vec<_>>()
    })
    .await?;
    if let Err(e) = app.state.flush().await {
        tracing::warn!(error = %format!("{e:#}"), "state flush failed before status read");
    }
    let a = app.clone();
    let accounts = blocking(move || {
        metas
            .into_iter()
            .map(|(meta, creds)| {
                let status =
                    a.state
                        .status(&meta.id, matches!(creds, CredsInfo::Present { .. }), now);
                AccountView {
                    status_since: status_since(&a, &meta.id, status.label()),
                    state: a.state.state(&meta.id, now),
                    in_flight: a.reservations.in_flight(&meta.id),
                    sessions: a.sessions.active_count(&meta.id, now),
                    meta,
                    status,
                    creds,
                }
            })
            .collect::<Vec<_>>()
    })
    .await?;
    let (mapping_enabled, mappings) = {
        let models = app.models.read().unwrap_or_else(|e| e.into_inner());
        (models.mapping_enabled(), models.mappings().to_vec())
    };
    Ok(StatusInputs {
        accounts,
        tuning: app.tuning(),
        mapping_enabled,
        mappings,
    })
}

async fn status(State(app): State<Arc<AppState>>) -> Response {
    let now = OffsetDateTime::now_utc();
    match gather(&app, now).await {
        Ok(inputs) => ok(build_status(&inputs, now)),
        Err(r) => r,
    }
}

// ---- session ---------------------------------------------------------------------------------

async fn session(State(app): State<Arc<AppState>>, headers: HeaderMap, body: Body) -> Response {
    let body = match read_object(&headers, body).await {
        Ok(b) => b,
        Err(r) => return r,
    };
    let presented = body.get("k").and_then(Value::as_str).unwrap_or("");
    if app.mgmt_key.is_empty() || !keys_match(presented, &app.mgmt_key) {
        return anthropic_error(
            401,
            "authentication_error",
            "pooltime: invalid management key",
        );
    }
    let mut r = ok(json!({ "ok": true }));
    if let Ok(v) = HeaderValue::from_str(&session_cookie_header(&app.mgmt_key)) {
        r.headers_mut().insert(SET_COOKIE, v);
    }
    r
}

// ---- accounts --------------------------------------------------------------------------------

fn parse_provider(raw: &str) -> Result<ProviderId, Response> {
    raw.parse::<ProviderId>().map_err(|e| bad(e.to_string()))
}

async fn create_account(
    State(app): State<Arc<AppState>>,
    headers: HeaderMap,
    body: Body,
) -> Response {
    let body = match read_object(&headers, body).await {
        Ok(b) => b,
        Err(r) => return r,
    };
    let Some(raw_id) = body.get("id").and_then(Value::as_str) else {
        return bad("id is required");
    };
    let id = match parse_id(raw_id) {
        Ok(id) => id,
        Err(r) => return r,
    };
    let Some(raw_provider) = body.get("provider").and_then(Value::as_str) else {
        return bad("provider is required (\"anthropic\" or \"openai\")");
    };
    let provider = match parse_provider(raw_provider) {
        Ok(p) => p,
        Err(r) => return r,
    };
    let (a, new_id) = (app.clone(), id.clone());
    let created = blocking(move || {
        if a.registry.get(&new_id).is_some() {
            return Err(anthropic_error(
                409,
                "invalid_request_error",
                &format!("account '{new_id}' already exists"),
            ));
        }
        a.registry
            .upsert(AccountMeta::new(new_id, provider))
            .map_err(|e| {
                tracing::error!(error = %format!("{e:#}"), "could not create account");
                internal("could not create the account")
            })
    })
    .await;
    match created {
        Ok(Ok(())) => json_response(
            StatusCode::CREATED,
            json!({ "ok": true, "id": id.as_str(), "provider": provider.as_str() }),
        ),
        Ok(Err(r)) | Err(r) => r,
    }
}

async fn delete_account(State(app): State<Arc<AppState>>, Path(raw): Path<String>) -> Response {
    let id = match parse_id(&raw) {
        Ok(id) => id,
        Err(r) => return r,
    };
    let (a, rid) = (app.clone(), id.clone());
    let removed = blocking(move || {
        if a.registry.get(&rid).is_none() {
            return Ok(false);
        }
        a.registry.remove(&rid).map(|()| true)
    })
    .await;
    match removed {
        Ok(Ok(true)) => {
            app.sessions.evict_account(&id);
            app.state.forget(&id);
            tracing::info!(account = %id, "account removed");
            ok(json!({ "ok": true, "id": id.as_str() }))
        }
        Ok(Ok(false)) => unknown_account(&id),
        Ok(Err(e)) => {
            tracing::error!(account = %id, error = %format!("{e:#}"), "could not remove account");
            internal("could not remove the account")
        }
        Err(r) => r,
    }
}

// ---- routing ---------------------------------------------------------------------------------

const PRIORITY_MSG: &str = "priority must be a non-negative integer (0..=4294967295)";

fn weight_msg() -> String {
    format!("weight must be a number between {MIN_WEIGHT} and {MAX_WEIGHT}")
}

/// A validated routing edit.
struct RoutingEdit {
    priority: Option<u32>,
    weight: Option<f64>,
    /// `Some(Some(t))` = timed override; `Some(None)` = `until: null`.
    until: Option<Option<OffsetDateTime>>,
}

fn parse_routing(body: &Map<String, Value>, now: OffsetDateTime) -> Result<RoutingEdit, String> {
    let priority = body
        .get("priority")
        .map(|v| {
            v.as_u64()
                .and_then(|n| u32::try_from(n).ok())
                .ok_or_else(|| PRIORITY_MSG.to_string())
        })
        .transpose()?;
    let weight = body
        .get("weight")
        .map(|v| {
            v.as_f64()
                .filter(|w| (MIN_WEIGHT..=MAX_WEIGHT).contains(w))
                .ok_or_else(weight_msg)
        })
        .transpose()?;
    let until = match body.get("until") {
        None => None,
        Some(Value::Null) => Some(None),
        Some(v) => {
            let t = v
                .as_str()
                .and_then(|s| OffsetDateTime::parse(s, &Rfc3339).ok())
                .ok_or("until must be an RFC 3339 timestamp, e.g. 2026-11-14T09:00:00Z")?;
            if t <= now {
                return Err("until must be in the future".into());
            }
            Some(Some(t))
        }
    };
    if priority.is_none() && weight.is_none() && until.is_none() {
        return Err("provide priority, weight and/or until".into());
    }
    if weight.is_none() && matches!(until, Some(Some(_))) {
        return Err("until requires weight (the temporary weight to apply until then)".into());
    }
    Ok(RoutingEdit {
        priority,
        weight,
        until,
    })
}

/// `{priority?, weight?, until?}`: `priority` and `weight` set the account's own values; with
/// `until` the weight is a temporary override that stops applying at that instant (routing
/// ignores an expired override, so it reverts with no further action). `until: null` alone
/// cancels an active override.
async fn routing(
    State(app): State<Arc<AppState>>,
    Path(raw): Path<String>,
    headers: HeaderMap,
    body: Body,
) -> Response {
    let id = match parse_id(&raw) {
        Ok(id) => id,
        Err(r) => return r,
    };
    let body = match read_object(&headers, body).await {
        Ok(b) => b,
        Err(r) => return r,
    };
    let now = OffsetDateTime::now_utc();
    let (a, rid) = (app.clone(), id.clone());
    let result = blocking(move || {
        let Some(mut meta) = a.registry.get(&rid) else {
            return Err(unknown_account(&rid));
        };
        let edit = parse_routing(&body, now).map_err(bad)?;
        if let Some(p) = edit.priority {
            meta.priority = p;
        }
        match (edit.weight, edit.until) {
            (Some(weight), Some(Some(until))) => {
                meta.weight_override = Some(TimedOverride { weight, until });
            }
            (Some(weight), _) => meta.weight = weight,
            (None, Some(None)) => meta.weight_override = None,
            (None, _) => {}
        }
        a.registry.upsert(meta.clone()).map_err(|e| {
            tracing::error!(account = %rid, error = %format!("{e:#}"), "could not save routing");
            internal("could not save the account")
        })?;
        Ok(meta)
    })
    .await;
    match result {
        Ok(Ok(meta)) => {
            let active = meta.weight_override.as_ref().filter(|o| o.until > now);
            tracing::info!(account = %id, priority = meta.priority, weight = meta.weight,
                override_active = active.is_some(), "routing updated");
            ok(json!({
                "ok": true,
                "id": id.as_str(),
                "priority": meta.priority,
                "weight": meta.weight,
                "weight_override": active.map(override_json),
                "effective_weight": active.map_or(meta.weight, |o| o.weight),
            }))
        }
        Ok(Err(r)) | Err(r) => r,
    }
}

/// Clear a billing block and re-poll usage now, so a reactivated subscription rejoins the
/// rotation without waiting for the sweeper. If it is still not entitled the block returns.
async fn recheck(State(app): State<Arc<AppState>>, Path(raw): Path<String>) -> Response {
    let id = match parse_id(&raw) {
        Ok(id) => id,
        Err(r) => return r,
    };
    let (a, rid) = (app.clone(), id.clone());
    let meta = match blocking(move || a.registry.get(&rid)).await {
        Ok(Some(m)) => m,
        Ok(None) => return unknown_account(&id),
        Err(r) => return r,
    };
    app.state.clear_billing(&id, OffsetDateTime::now_utc());
    let poll = match app.providers.get(&meta.provider) {
        None => "unavailable".to_string(),
        Some(p) => match p.poll_usage(&meta).await {
            Ok(_) => "ok".to_string(),
            Err(e) => e.label(),
        },
    };
    let (a, rid) = (app.clone(), id.clone());
    let status = blocking(move || {
        let creds_ok = matches!(a.registry.read_creds(&rid), Ok(Some(_)));
        a.state
            .reevaluate(&rid, creds_ok, OffsetDateTime::now_utc())
            .label()
    })
    .await;
    match status {
        Ok(status) => ok(json!({ "ok": true, "id": id.as_str(), "status": status, "poll": poll })),
        Err(r) => r,
    }
}

// ---- tuning & mappings -----------------------------------------------------------------------

/// Any subset of the three knobs. All-or-nothing: every supplied field is validated before
/// anything changes, and the result is saved to `config.toml` before it goes live, so a restart
/// keeps it.
async fn tuning(State(app): State<Arc<AppState>>, headers: HeaderMap, body: Body) -> Response {
    let body = match read_object(&headers, body).await {
        Ok(b) => b,
        Err(r) => return r,
    };
    let result = blocking(move || apply_tuning(&app, &body)).await;
    match result {
        Ok(Ok(t)) => ok(json!({ "ok": true, "tuning": tuning_json(&t) })),
        Ok(Err(r)) | Err(r) => r,
    }
}

fn apply_tuning(app: &AppState, body: &Map<String, Value>) -> Result<Tuning, Response> {
    if body.is_empty() {
        return Err(bad("provide at least one tuning field"));
    }
    let mut guard = app.tuning.write().unwrap_or_else(|e| e.into_inner());
    let mut next = *guard;
    for (key, value) in body {
        let slot = match key.as_str() {
            "five_hour_exp" => &mut next.five_hour_exp,
            "min_headroom" => &mut next.min_headroom,
            "headroom_taper_start" => &mut next.headroom_taper_start,
            other => {
                return Err(bad(format!(
                    "unknown tuning field '{other}' (expected five_hour_exp, min_headroom, headroom_taper_start)"
                )))
            }
        };
        *slot = value
            .as_f64()
            .ok_or_else(|| bad(format!("{key} must be a number")))?;
    }
    next.validate().map_err(|e| bad(e.to_string()))?;
    let persist = || -> anyhow::Result<()> {
        let mut cfg = Config::load_file(&app.paths)?;
        cfg.routing.five_hour_exp = next.five_hour_exp;
        cfg.routing.min_headroom = next.min_headroom;
        cfg.routing.headroom_taper_start = next.headroom_taper_start;
        cfg.save(&app.paths)
    };
    persist().map_err(|e| {
        tracing::error!(error = %format!("{e:#}"), "could not save tuning to config.toml");
        internal("could not save the tuning to config.toml")
    })?;
    *guard = next;
    tracing::info!(
        five_hour_exp = next.five_hour_exp,
        min_headroom = next.min_headroom,
        headroom_taper_start = next.headroom_taper_start,
        "tuning updated"
    );
    Ok(next)
}

/// `{enabled?, mappings?}`: the master switch and/or rows overlaid on the live set. Every row is
/// validated first; the table is saved to `models.toml` before it goes live.
async fn mappings(State(app): State<Arc<AppState>>, headers: HeaderMap, body: Body) -> Response {
    let body = match read_object(&headers, body).await {
        Ok(b) => b,
        Err(r) => return r,
    };
    let enabled = match body.get("enabled") {
        None => None,
        Some(Value::Bool(b)) => Some(*b),
        Some(_) => return bad("enabled must be a boolean"),
    };
    let rows = match body.get("mappings") {
        None => None,
        Some(Value::Array(items)) => {
            let mut rows = Vec::with_capacity(items.len());
            for item in items {
                match serde_json::from_value::<Mapping>(item.clone()) {
                    Ok(m) => rows.push(m),
                    Err(_) => {
                        return bad(
                            "each mapping needs string 'from' and 'to' (and an optional effort object of strings)",
                        )
                    }
                }
            }
            Some(rows)
        }
        Some(_) => return bad("mappings must be an array"),
    };
    if enabled.is_none() && rows.is_none() {
        return bad("provide enabled and/or mappings");
    }
    let result = blocking(move || {
        let mut live = app.models.write().unwrap_or_else(|e| e.into_inner());
        let mut next = live.clone();
        next.apply_mappings(enabled, rows)
            .map_err(|e| bad(e.to_string()))?;
        next.save(&app.paths).map_err(|e| {
            tracing::error!(error = %format!("{e:#}"), "could not save models.toml");
            internal("could not save models.toml")
        })?;
        let out = mapping_json(next.mapping_enabled(), next.mappings());
        *live = next;
        Ok(out)
    })
    .await;
    match result {
        Ok(Ok(m)) => {
            tracing::info!("model mappings updated");
            ok(json!({ "ok": true, "mapping": m }))
        }
        Ok(Err(r)) | Err(r) => r,
    }
}

// ---- sign-in ---------------------------------------------------------------------------------

/// The flows the API knows by id: a [`SignInHandle`] to drive each, and its result once known.
/// Entries are dropped [`SIGNIN_RETENTION`] after their expiry.
#[derive(Default)]
pub struct SignIns {
    inner: Arc<Mutex<HashMap<String, SignInEntry>>>,
}

struct SignInEntry {
    account: AccountId,
    handle: SignInHandle,
    expires_at: OffsetDateTime,
    result: Option<SignInResult>,
}

fn lock(m: &Mutex<HashMap<String, SignInEntry>>) -> MutexGuard<'_, HashMap<String, SignInEntry>> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl SignIns {
    /// Track `pending` and record its result when it ends. Needs a tokio runtime.
    fn register(&self, account: AccountId, pending: PendingSignIn, now: OffsetDateTime) {
        let id = pending.id.clone();
        {
            let mut map = lock(&self.inner);
            map.retain(|_, e| e.expires_at + SIGNIN_RETENTION > now);
            map.insert(
                id.clone(),
                SignInEntry {
                    account,
                    handle: pending.handle(),
                    expires_at: pending.expires_at,
                    result: None,
                },
            );
        }
        let inner = self.inner.clone();
        tokio::spawn(async move {
            let result = pending.wait().await;
            if let Some(e) = lock(&inner).get_mut(&id) {
                e.result = Some(result);
            }
        });
    }

    fn handle(&self, id: &str) -> Option<SignInHandle> {
        lock(&self.inner).get(id).map(|e| e.handle.clone())
    }

    fn set_result(&self, id: &str, result: SignInResult) {
        if let Some(e) = lock(&self.inner).get_mut(id) {
            e.result = Some(result);
        }
    }

    fn view(&self, id: &str) -> Option<Value> {
        let map = lock(&self.inner);
        let e = map.get(id)?;
        let mut out = json!({
            "signin_id": id,
            "account": e.account.as_str(),
            "expires_at": ts(e.expires_at),
        });
        let extra = match &e.result {
            None => json!({ "status": "pending" }),
            Some(SignInResult::Saved { identity }) => {
                json!({ "status": "saved", "identity": identity_json(identity) })
            }
            Some(SignInResult::NeedsBinding { got }) => {
                json!({ "status": "needs_binding", "got": identity_json(got) })
            }
            Some(SignInResult::Mismatch { expected, got }) => json!({
                "status": "mismatch",
                "expected": identity_json(expected),
                "got": identity_json(got),
                "message": mismatch_message(&e.account, expected, got),
            }),
            Some(SignInResult::Failed(message)) => {
                json!({ "status": "failed", "message": message })
            }
        };
        if let (Some(o), Value::Object(x)) = (out.as_object_mut(), extra) {
            o.extend(x);
        }
        Some(out)
    }
}

fn mismatch_message(account: &AccountId, expected: &Identity, got: &Identity) -> String {
    let who = |i: &Identity| {
        i.email
            .clone()
            .unwrap_or_else(|| "a different account".into())
    };
    format!(
        "signed in as {}, but account '{account}' belongs to {}: sign out of claude.ai and sign in as {} (nothing was saved)",
        who(got),
        who(expected),
        who(expected),
    )
}

/// Start a sign-in for `{id}`, creating the account first when it does not exist yet.
/// `{mode?: "callback"|"paste", provider?, hint?}`. Only Anthropic accounts can sign in so far.
async fn login(
    State(app): State<Arc<AppState>>,
    Path(raw): Path<String>,
    headers: HeaderMap,
    body: Body,
) -> Response {
    let id = match parse_id(&raw) {
        Ok(id) => id,
        Err(r) => return r,
    };
    let body = match read_object(&headers, body).await {
        Ok(b) => b,
        Err(r) => return r,
    };
    let mode = match body.get("mode").and_then(Value::as_str) {
        None | Some("callback") => SignInMode::Callback,
        Some("paste") => SignInMode::PasteCode,
        Some(_) => return bad("mode must be \"callback\" or \"paste\""),
    };
    let requested = match body.get("provider") {
        None => None,
        Some(v) => match v.as_str().map(parse_provider) {
            Some(Ok(p)) => Some(p),
            Some(Err(r)) => return r,
            None => return bad("provider must be a string"),
        },
    };
    let hint = body.get("hint").and_then(Value::as_str).map(str::to_owned);

    let (a, rid) = (app.clone(), id.clone());
    let existing = match blocking(move || a.registry.get(&rid)).await {
        Ok(m) => m,
        Err(r) => return r,
    };
    let provider = match (&existing, requested) {
        (Some(m), Some(p)) if m.provider != p => {
            return bad(format!(
                "account '{id}' is a {} account, not {p}",
                m.provider
            ))
        }
        (Some(m), _) => m.provider,
        (None, p) => p.unwrap_or(ProviderId::Anthropic),
    };
    if provider != ProviderId::Anthropic {
        return bad(format!(
            "sign-in for {provider} accounts is not yet supported"
        ));
    }

    let created = existing.is_none();
    if created {
        let (a, rid) = (app.clone(), id.clone());
        let saved = blocking(move || a.registry.upsert(AccountMeta::new(rid, provider))).await;
        match saved {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                tracing::error!(account = %id, error = %format!("{e:#}"), "could not create account");
                return internal("could not create the account");
            }
            Err(r) => return r,
        }
    }
    let flow = ClaudeSignIn::new(
        app.cfg.anthropic.clone(),
        app.registry.clone(),
        app.state.clone(),
        app.upstream.clone(),
    );
    match flow.start(&id, hint.as_deref(), mode) {
        Ok(pending) => {
            let out = json!({
                "signin_id": pending.id,
                "authorize_url": pending.authorize_url,
                "expires_at": ts(pending.expires_at),
            });
            tracing::info!(account = %id, signin = %pending.id, "sign-in started");
            app.signins.register(id, pending, OffsetDateTime::now_utc());
            ok(out)
        }
        Err(e) => {
            tracing::warn!(account = %id, error = %format!("{e:#}"), "could not start sign-in");
            if created {
                let (a, rid) = (app.clone(), id.clone());
                let _ = blocking(move || a.registry.remove(&rid)).await;
            }
            internal(&format!("could not start sign-in: {e:#}"))
        }
    }
}

async fn signin_status(State(app): State<Arc<AppState>>, Path(id): Path<String>) -> Response {
    match app.signins.view(&id) {
        Some(v) => ok(v),
        None => anthropic_error(404, "not_found_error", "unknown or expired sign-in"),
    }
}

/// `{code}`: the pasted `code#state` line (or the redirect URL). Returns once the input is
/// accepted; the outcome shows on `GET /api/v1/signin/{id}`.
async fn signin_code(
    State(app): State<Arc<AppState>>,
    Path(id): Path<String>,
    headers: HeaderMap,
    body: Body,
) -> Response {
    let body = match read_object(&headers, body).await {
        Ok(b) => b,
        Err(r) => return r,
    };
    let Some(handle) = app.signins.handle(&id) else {
        return anthropic_error(404, "not_found_error", "unknown or expired sign-in");
    };
    let Some(code) = body.get("code").and_then(Value::as_str) else {
        return bad("code is required (the <code>#<state> line from the sign-in page)");
    };
    match handle.submit_code(code) {
        Ok(()) => ok(json!({ "ok": true })),
        Err(e @ SubmitError::AlreadyUsed) => {
            anthropic_error(409, "invalid_request_error", &e.to_string())
        }
        Err(e) => bad(e.to_string()),
    }
}

/// Confirm a `needs_binding` sign-in: save the held credentials and record the identity.
async fn signin_bind(State(app): State<Arc<AppState>>, Path(id): Path<String>) -> Response {
    let Some(handle) = app.signins.handle(&id) else {
        return anthropic_error(404, "not_found_error", "unknown or expired sign-in");
    };
    match handle.bind().await {
        Ok(identity) => {
            let out = json!({ "ok": true, "identity": identity_json(&identity) });
            app.signins
                .set_result(&id, SignInResult::Saved { identity });
            ok(out)
        }
        Err(e) => anthropic_error(409, "invalid_request_error", &format!("{e:#}")),
    }
}
