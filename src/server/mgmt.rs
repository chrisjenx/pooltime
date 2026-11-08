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
//! status shape snapshot-testable; [`gather`] does the I/O. For authorized requests (bearer or session cookie) the HTTP
//! handler adds `profiles` (connected Claude Code profiles from `config.toml`; `null` if the file
//! is unreadable) and `storage` (data dir, database and log bytes) for the Settings view. With `POOLTIME_FIXTURE_STATE` set (dev/screenshots
//! only, see [`super::daemon`]) the handler serves that document verbatim instead.

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
use super::models_route::build_listing;
use super::router::{method_not_allowed, read_json_limited};
use super::AppState;
use crate::accounts::registry::{MAX_WEIGHT, MIN_WEIGHT};
use crate::accounts::status::window_duration;
use crate::accounts::{
    AccountId, AccountMeta, AccountState, AccountStatus, Identity, TimedOverride, Window,
};
use crate::claude_code::{discover, profile_of, Profile};
use crate::config::{Config, Paths, ProfileRef};
use crate::fsutil::WriteOutcome;
use crate::models::Mapping;
use crate::providers::anthropic::anthropic_error;
use crate::providers::ProviderId;
use crate::routing::Tuning;
use crate::signin::claude::ClaudeSignIn;
use crate::signin::openai::{ActiveSignIn, CallbackPortsBusy, OpenAiSignIn};
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
pub(crate) fn authorized(h: &HeaderMap, key: &str) -> bool {
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
        .route(
            "/api/v1/profiles/connect",
            write(post(profiles_connect), "POST"),
        )
        .route(
            "/api/v1/profiles/disconnect",
            write(post(profiles_disconnect), "POST"),
        )
        .route(
            "/api/v1/models/refresh",
            write(post(models_refresh), "POST"),
        )
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
    /// `from` families of mappings whose target is missing ([`crate::models::ModelTable::inert_mappings`]).
    pub inert_mappings: Vec<String>,
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

/// The mapping block; a row whose target is missing (`inert`, by `from` family) carries
/// `"inert": true` (it falls back to passthrough until the target exists).
fn mapping_json(enabled: bool, mappings: &[Mapping], inert: &[String]) -> Value {
    let rows: Vec<Value> = mappings
        .iter()
        .map(|m| {
            let mut v = serde_json::to_value(m).unwrap_or(Value::Null);
            if inert.contains(&m.from) {
                v["inert"] = json!(true);
            }
            v
        })
        .collect();
    json!({ "enabled": enabled, "mappings": rows })
}

/// `from` families of the table's inert mappings.
fn inert_families(t: &crate::models::ModelTable) -> Vec<String> {
    t.inert_mappings()
        .into_iter()
        .map(|m| m.from.clone())
        .collect()
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
        "mapping": mapping_json(inputs.mapping_enabled, &inputs.mappings, &inputs.inert_mappings),
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
    let (mapping_enabled, mappings, inert_mappings) = {
        let models = app.models.read().unwrap_or_else(|e| e.into_inner());
        (
            models.mapping_enabled(),
            models.mappings().to_vec(),
            inert_families(&models),
        )
    };
    Ok(StatusInputs {
        accounts,
        tuning: app.tuning(),
        mapping_enabled,
        mappings,
        inert_mappings,
    })
}

async fn status(State(app): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    if let Some(fixture) = &app.fixture_state {
        return ok(fixture.clone());
    }
    let now = OffsetDateTime::now_utc();
    let inputs = match gather(&app, now).await {
        Ok(inputs) => inputs,
        Err(r) => return r,
    };
    let mut v = build_status(&inputs, now);
    // Local paths (data dir, profile dirs) are for the key holder only; the pool view stays open.
    if authorized(&headers, &app.mgmt_key) {
        let paths = app.paths.clone();
        let (profiles, storage) =
            match blocking(move || (profiles_on_file(&paths), storage_json(&paths))).await {
                Ok(x) => x,
                Err(r) => return r,
            };
        if let Value::Object(o) = &mut v {
            o.insert("profiles".into(), profiles);
            o.insert("storage".into(), storage);
        }
    }
    ok(v)
}

/// The connected Claude Code profiles as recorded in `config.toml` (`connect` keeps it current;
/// the startup copy in `AppState::cfg` would go stale). An unreadable config is `null` (unknown),
/// distinct from `[]` (none connected). Only the path is logged: parse errors quote file content.
fn profiles_on_file(paths: &Paths) -> Value {
    match Config::load_file(paths) {
        Ok(c) => Value::Array(
            c.profiles
                .iter()
                .map(|p| json!({ "id": p.id, "settings_dir": p.settings_dir.to_string_lossy() }))
                .collect(),
        ),
        Err(_) => {
            tracing::warn!(path = %paths.config.display(), "could not read config.toml for status");
            Value::Null
        }
    }
}

/// Data dir and the bytes held by the database (with its WAL/SHM files) and the logs.
fn storage_json(paths: &Paths) -> Value {
    let db: u64 = ["", "-wal", "-shm"]
        .iter()
        .map(|suffix| {
            let mut p = paths.db.clone().into_os_string();
            p.push(suffix);
            fs::metadata(PathBuf::from(p)).map_or(0, |m| m.len())
        })
        .sum();
    json!({
        "home": paths.home.to_string_lossy(),
        "db_bytes": db,
        "logs_bytes": dir_size(&paths.logs),
    })
}

fn dir_size(p: &std::path::Path) -> u64 {
    let Ok(rd) = fs::read_dir(p) else {
        return 0;
    };
    rd.flatten()
        .map(|e| match e.file_type() {
            Ok(t) if t.is_dir() => dir_size(&e.path()),
            Ok(t) if t.is_file() => e.metadata().map_or(0, |m| m.len()),
            _ => 0,
        })
        .sum()
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
    // `email` (optional) is the expected identity: the first sign-in must match it (email is
    // the fallback match when no account uuid is on file) and it pre-fills the login page.
    let email = match body.get("email") {
        None | Some(Value::Null) => None,
        Some(v) => match v.as_str().map(str::trim) {
            Some(e) if e.len() > 2 && e.contains('@') => Some(e.to_string()),
            _ => return bad("email must be an email address, e.g. you@example.com"),
        },
    };
    let (a, new_id, mail) = (app.clone(), id.clone(), email.clone());
    let created = blocking(move || {
        if a.registry.get(&new_id).is_some() {
            return Err(anthropic_error(
                409,
                "invalid_request_error",
                &format!("account '{new_id}' already exists"),
            ));
        }
        let mut meta = AccountMeta::new(new_id, provider);
        meta.identity.email = mail;
        a.registry.upsert(meta).map_err(|e| {
            tracing::error!(error = %format!("{e:#}"), "could not create account");
            internal("could not create the account")
        })
    })
    .await;
    match created {
        Ok(Ok(())) => json_response(
            StatusCode::CREATED,
            json!({ "ok": true, "id": id.as_str(), "provider": provider.as_str(), "email": email }),
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
        let out = mapping_json(
            next.mapping_enabled(),
            next.mappings(),
            &inert_families(&next),
        );
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

// ---- Claude Code profiles & models -----------------------------------------------------------

fn outcome_label(o: WriteOutcome) -> &'static str {
    match o {
        WriteOutcome::Written => "written",
        WriteOutcome::Unchanged => "unchanged",
        WriteOutcome::GaveUp => "gave_up",
    }
}

/// The daemon's `$HOME`, where `~/.claude*` profiles are discovered (never `$CLAUDE_CONFIG_DIR`).
fn user_home() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .filter(|h| !h.is_empty())
        .map(PathBuf::from)
}

const PROFILES_MSG: &str =
    "profiles must be an array of profile ids or absolute settings-directory paths";

/// The optional `profiles` array of strings (absent = empty).
fn profile_names(body: &Map<String, Value>) -> Result<Vec<String>, Response> {
    match body.get("profiles") {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::Array(items)) => items
            .iter()
            .map(|v| {
                v.as_str()
                    .map(str::to_owned)
                    .ok_or_else(|| bad(PROFILES_MSG))
            })
            .collect(),
        Some(_) => Err(bad(PROFILES_MSG)),
    }
}

/// The profile id an argument without `/` stands for: leading dots are dropped (`.claude-work`
/// -> `claude-work`) and `claude` / `.claude` mean `default` (`~/.claude`). Such an argument is
/// never a path, so `.claude` typed inside a repository cannot pick that repository's
/// `.claude/` directory.
pub fn profile_id_arg(name: &str) -> String {
    match name.trim_start_matches('.') {
        "" => name.to_string(),
        "claude" => "default".to_string(),
        id => id.to_string(),
    }
}

/// `p` with symlinks resolved when it exists (macOS temp dirs live behind `/var` ->
/// `/private/var`); a missing file is resolved through its parent.
fn canonical(p: &std::path::Path) -> PathBuf {
    if let Ok(c) = std::fs::canonicalize(p) {
        return c;
    }
    match (p.parent(), p.file_name()) {
        (Some(dir), Some(name)) => std::fs::canonicalize(dir)
            .map(|d| d.join(name))
            .unwrap_or_else(|_| p.to_path_buf()),
        _ => p.to_path_buf(),
    }
}

/// Resolve profile names against the daemon's `$HOME` ([`resolve_profiles_in`]).
pub fn resolve_profiles(
    names: &[String],
    configured: &[ProfileRef],
) -> Result<Vec<Profile>, String> {
    resolve_profiles_in(names, configured, user_home().as_deref())
}

/// Resolve profile names. A name containing `/` is a settings directory ([`profile_at`]).
/// Anything else is an id ([`profile_id_arg`]), looked up among the configured profiles first
/// and then the `~/.claude*` directories discovered under `home` (only read when needed). A
/// profile whose `.claude.json` would be another profile's is refused. Duplicates (same
/// settings dir) collapse.
pub fn resolve_profiles_in(
    names: &[String],
    configured: &[ProfileRef],
    home: Option<&std::path::Path>,
) -> Result<Vec<Profile>, String> {
    let mut discovered: Option<Vec<Profile>> = None;
    let mut out: Vec<Profile> = Vec::new();
    for name in names {
        let p = if name.contains('/') {
            profile_at(name, home)?
        } else {
            let id = profile_id_arg(name);
            if let Some(r) = configured.iter().find(|r| r.id == id) {
                profile_of(r)
            } else {
                let found =
                    discovered.get_or_insert_with(|| home.map(discover).unwrap_or_default());
                match found.iter().find(|p| p.id == id) {
                    Some(p) => p.clone(),
                    None => {
                        let mut known: Vec<&str> =
                            configured.iter().map(|r| r.id.as_str()).collect();
                        for p in found.iter() {
                            if !known.contains(&p.id.as_str()) {
                                known.push(&p.id);
                            }
                        }
                        let known = if known.is_empty() {
                            "none found".to_string()
                        } else {
                            known.join(", ")
                        };
                        return Err(format!("unknown profile '{name}' (known: {known})"));
                    }
                }
            }
        };
        let (dir, state) = (canonical(&p.settings_dir), canonical(&p.claude_json));
        let others = configured.iter().map(profile_of).chain(out.iter().cloned());
        for q in others {
            if canonical(&q.settings_dir) != dir && canonical(&q.claude_json) == state {
                return Err(format!(
                    "{} would share {} with profile '{}'",
                    p.settings_dir.display(),
                    p.claude_json.display(),
                    q.id
                ));
            }
        }
        if !out.iter().any(|q| canonical(&q.settings_dir) == dir) {
            out.push(p);
        }
    }
    Ok(out)
}

/// The profile at directory `name`: absolute, holding a `settings.json` (the discovery rule),
/// never `home` itself, and never a directory whose `.claude.json` is `~/.claude.json` other than
/// `~/.claude` (however spelled, that one is the default profile).
fn profile_at(name: &str, home: Option<&std::path::Path>) -> Result<Profile, String> {
    let dir = std::path::Path::new(name);
    if !dir.is_absolute() {
        return Err(format!("profile path must be absolute: {name}"));
    }
    if !dir.is_dir() {
        return Err(format!("profile directory not found: {name}"));
    }
    if !dir.join("settings.json").is_file() {
        return Err(format!(
            "{name} has no settings.json, so it is not a Claude Code profile directory"
        ));
    }
    let Some(home) = home else {
        return Ok(Profile::from_dir(dir, std::path::Path::new("")));
    };
    let dir_c = canonical(dir);
    if dir_c == canonical(home) {
        return Err(format!(
            "{name} is your home directory, not a Claude Code profile (did you mean {}?)",
            home.join(".claude").display()
        ));
    }
    if dir_c == canonical(&home.join(".claude")) {
        return Ok(Profile::from_dir(&home.join(".claude"), home));
    }
    let p = Profile::from_dir(dir, home);
    if canonical(&p.claude_json) == canonical(&home.join(".claude.json")) {
        return Err(format!(
            "{name} would share ~/.claude.json with the default profile"
        ));
    }
    Ok(p)
}

/// Response rows for `profiles` and the `(id, outcome)` pairs `connect` / `disconnect` returned.
/// Those come back in `profiles` order (failed ones skipped), so each pair is matched to the next
/// profile with its id: two directories sharing an id keep their own directories.
pub fn profiles_json(profiles: &[Profile], outcomes: &[(String, WriteOutcome)]) -> Value {
    let mut rest = profiles.iter();
    Value::Array(
        outcomes
            .iter()
            .map(|(id, o)| {
                let dir = rest
                    .by_ref()
                    .find(|p| p.id == *id)
                    .map(|p| p.settings_dir.to_string_lossy().into_owned());
                json!({ "id": id, "settings_dir": dir, "outcome": outcome_label(*o) })
            })
            .collect(),
    )
}

/// `{profiles: [id | absolute dir, ...]}` or `{all: true}` (every `~/.claude*` profile found under
/// the daemon's `$HOME`): point them at this pool ([`crate::claude_code::connect`]). Naming
/// nothing is refused rather than read as "all", so no profile is ever connected by accident.
async fn profiles_connect(
    State(app): State<Arc<AppState>>,
    headers: HeaderMap,
    body: Body,
) -> Response {
    let body = match read_object(&headers, body).await {
        Ok(b) => b,
        Err(r) => return r,
    };
    let names = match profile_names(&body) {
        Ok(n) => n,
        Err(r) => return r,
    };
    let all = match body.get("all") {
        None | Some(Value::Bool(false)) => false,
        Some(Value::Bool(true)) => true,
        Some(_) => return bad("all must be a boolean"),
    };
    if names.is_empty() && !all {
        return bad(
            "name the profiles to connect (ids or absolute settings-directory paths), or pass all: true",
        );
    }
    let paths = app.paths.clone();
    let resolved = blocking(move || -> Result<Vec<Profile>, Response> {
        let cfg = Config::load_file(&paths).map_err(|e| {
            tracing::error!(error = %format!("{e:#}"), "could not read config.toml");
            internal("could not read config.toml")
        })?;
        let mut profiles = resolve_profiles(&names, &cfg.profiles).map_err(bad)?;
        if all {
            for p in user_home().map(|h| discover(&h)).unwrap_or_default() {
                if !profiles.iter().any(|q| q.settings_dir == p.settings_dir) {
                    profiles.push(p);
                }
            }
            if profiles.is_empty() {
                return Err(bad(
                    "no Claude Code profiles found (~/.claude*/settings.json)",
                ));
            }
        }
        Ok(profiles)
    })
    .await;
    let profiles = match resolved {
        Ok(Ok(p)) => p,
        Ok(Err(r)) | Err(r) => return r,
    };
    match crate::claude_code::connect(&app, &profiles).await {
        Ok(outcomes) => {
            tracing::info!(profiles = outcomes.len(), "profiles connected");
            ok(json!({ "ok": true, "profiles": profiles_json(&profiles, &outcomes) }))
        }
        Err(e) => {
            tracing::warn!(error = %format!("{e:#}"), "connect failed");
            internal(&format!("connect failed: {e:#}"))
        }
    }
}

/// `{profiles?: [id | absolute dir, ...]}`: restore the named profiles
/// ([`crate::claude_code::disconnect`]); none named = every connected (configured) profile.
async fn profiles_disconnect(
    State(app): State<Arc<AppState>>,
    headers: HeaderMap,
    body: Body,
) -> Response {
    let body = match read_object(&headers, body).await {
        Ok(b) => b,
        Err(r) => return r,
    };
    let names = match profile_names(&body) {
        Ok(n) => n,
        Err(r) => return r,
    };
    let paths = app.paths.clone();
    let result = blocking(move || -> Result<Value, Response> {
        let cfg = Config::load_file(&paths).map_err(|e| {
            tracing::error!(error = %format!("{e:#}"), "could not read config.toml");
            internal("could not read config.toml")
        })?;
        let profiles = if names.is_empty() {
            cfg.profiles.iter().map(profile_of).collect()
        } else {
            resolve_profiles(&names, &cfg.profiles).map_err(bad)?
        };
        let outcomes = crate::claude_code::disconnect(&paths, &profiles).map_err(|e| {
            tracing::warn!(error = %format!("{e:#}"), "disconnect failed");
            internal(&format!("disconnect failed: {e:#}"))
        })?;
        Ok(profiles_json(&profiles, &outcomes))
    })
    .await;
    match result {
        Ok(Ok(list)) => {
            tracing::info!("profiles disconnected");
            ok(json!({ "ok": true, "profiles": list }))
        }
        Ok(Err(r)) | Err(r) => r,
    }
}

/// Re-fetch the live Claude catalog now (bypassing the listing cache) and run Codex catalog
/// discovery (runtime rows, applied live), independently of each other, then re-derive the pins
/// of every connected profile.
///
/// A failed Claude fetch keeps the last good list for the listing (`models` is what `/v1/models`
/// serves) and is reported as a classified `live_error` with `live: 0`. The Codex outcome is
/// under `codex`: `{accounts, added, updated, failed: [{account, reason}]}`, or `{error}` when
/// every account failed. Codex counts as answered only with `accounts > 0`. 502 (classified
/// reasons only) when no catalog answered; 503 when neither provider runs.
async fn models_refresh(State(app): State<Arc<AppState>>) -> Response {
    let anthropic = app.providers.get(&ProviderId::Anthropic).cloned();
    let has_openai = app.providers.contains_key(&ProviderId::OpenAi);
    if anthropic.is_none() && !has_openai {
        return anthropic_error(503, "api_error", "pooltime: no model provider is running");
    }
    let live = match &anthropic {
        Some(p) => Some(p.refresh_live_models().await),
        None => None,
    };
    let discovery = if has_openai {
        let d = crate::providers::openai::discover_codex_models(&app).await;
        crate::providers::openai::warn_inert_mappings(&app);
        Some(d)
    } else {
        None
    };
    let live_err = live
        .as_ref()
        .and_then(|r| r.as_ref().err())
        .map(|e| format!("{e:#}"));
    let codex_err = discovery
        .as_ref()
        .and_then(|r| r.as_ref().err())
        .map(|e| format!("{e:#}"));
    let live_ok = live.as_ref().is_some_and(|r| r.is_ok());
    // Codex counts as answered only when at least one ChatGPT account was asked.
    let codex_ok = matches!(&discovery, Some(Ok(d)) if d.accounts > 0);
    if !live_ok && !codex_ok {
        let mut reasons: Vec<String> = [live_err, codex_err].into_iter().flatten().collect();
        if reasons.is_empty() {
            reasons.push("no model catalog could be asked".into());
        }
        return anthropic_error(
            502,
            "api_error",
            &format!("pooltime: {}", reasons.join("; ")),
        );
    }
    let live_n = live
        .as_ref()
        .and_then(|r| r.as_ref().ok())
        .map_or(0, Vec::len);
    // What `/v1/models` serves: after a failed refresh, the last good (cached) live list.
    let listed_live = match (live.and_then(Result::ok), &anthropic) {
        (Some(l), _) => Some(l),
        (None, Some(p)) => p.live_models().await.ok(),
        (None, None) => None,
    }
    .filter(|v| !v.is_empty());
    let codex = discovery.map(codex_json);
    let ids: Vec<Value> = {
        let table = app.models.read().unwrap_or_else(|e| e.into_inner());
        build_listing(listed_live.as_deref(), &table.rows_for_listing())
            .into_iter()
            .map(|m| m["id"].clone())
            .collect()
    };
    let mut out = json!({ "ok": true, "live": live_n, "models": ids });
    if let Some(e) = live_err {
        out["live_error"] = json!(e);
    }
    if let Some(c) = codex {
        out["codex"] = c;
    }
    match crate::claude_code::sync_once(&app).await {
        Ok(v) => out["profiles_synced"] = json!(v.len()),
        Err(e) => {
            // The chain may quote config.toml; the response carries only a fixed reason.
            tracing::warn!(error = %format!("{e:#}"), "profile sync after models refresh failed");
            out["profiles_synced"] = json!(0);
            out["sync_error"] = json!("profile_sync_failed");
        }
    }
    tracing::info!(live = live_n, "model catalog refreshed");
    ok(out)
}

/// The `codex` part of the refresh response (classified reasons only).
fn codex_json(r: anyhow::Result<crate::providers::openai::CodexDiscovery>) -> Value {
    match r {
        Ok(d) => json!({
            "accounts": d.accounts,
            "added": d.added,
            "updated": d.updated,
            "failed": d.failed.iter()
                .map(|(a, reason)| json!({ "account": a.as_str(), "reason": reason }))
                .collect::<Vec<_>>(),
        }),
        Err(e) => json!({ "error": format!("{e:#}") }),
    }
}

// ---- sign-in ---------------------------------------------------------------------------------

/// The flows the API knows by id: a [`SignInHandle`] to drive each, and its result once known.
/// Entries are dropped [`SIGNIN_RETENTION`] after their expiry.
#[derive(Default)]
pub struct SignIns {
    inner: Arc<Mutex<HashMap<String, SignInEntry>>>,
    /// The pending ChatGPT sign-in, superseded by the next one.
    openai: ActiveSignIn,
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

pub fn mismatch_message(account: &AccountId, expected: &Identity, got: &Identity) -> String {
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
    let started = match provider {
        ProviderId::Anthropic => ClaudeSignIn::new(
            app.cfg.anthropic.clone(),
            app.registry.clone(),
            app.state.clone(),
            app.upstream.clone(),
        )
        .start(&id, hint.as_deref(), mode),
        // One pending ChatGPT sign-in at a time (fixed callback ports): a new one supersedes it.
        ProviderId::OpenAi => {
            OpenAiSignIn::new(
                app.cfg.openai.clone(),
                app.registry.clone(),
                app.state.clone(),
                app.upstream.clone(),
            )
            .with_active(app.signins.openai.clone())
            .start_superseding(&id, hint.as_deref(), mode)
            .await
        }
    };
    match started {
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
            if let Some(busy) = e.downcast_ref::<CallbackPortsBusy>() {
                return anthropic_error(409, "invalid_request_error", &busy.to_string());
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

#[cfg(test)]
mod tests {
    use super::*;

    fn profile_dir(home: &std::path::Path, name: &str) -> PathBuf {
        let d = home.join(name);
        fs::create_dir_all(&d).unwrap();
        fs::write(d.join("settings.json"), "{}").unwrap();
        d
    }

    fn resolve(names: &[&str], home: &std::path::Path) -> Result<Vec<Profile>, String> {
        let names: Vec<String> = names.iter().map(|s| s.to_string()).collect();
        resolve_profiles_in(&names, &[], Some(home))
    }

    #[test]
    fn bare_names_are_ids_never_paths() {
        assert_eq!(profile_id_arg(".claude"), "default");
        assert_eq!(profile_id_arg("claude"), "default");
        assert_eq!(profile_id_arg(".claude-work"), "claude-work");
        assert_eq!(profile_id_arg("default"), "default");
        assert_eq!(profile_id_arg("."), ".");

        let home = tempfile::tempdir().unwrap();
        let h = home.path();
        profile_dir(h, ".claude");
        profile_dir(h, ".claude-work");
        let p = resolve(&[".claude", ".claude-work"], h).unwrap();
        assert_eq!(p[0].id, "default");
        assert_eq!(p[0].claude_json, h.join(".claude.json"));
        assert_eq!(p[1].id, "claude-work");
        assert_eq!(p[1].claude_json, h.join(".claude-work/.claude.json"));
    }

    #[test]
    fn path_rules() {
        let home = tempfile::tempdir().unwrap();
        let h = home.path();
        let s = |p: PathBuf| p.to_string_lossy().into_owned();

        // $HOME itself, even with a settings.json, is refused (it maps to ~/.claude.json).
        fs::write(h.join("settings.json"), "{}").unwrap();
        let e = resolve(&[&s(h.to_path_buf())], h).unwrap_err();
        assert!(e.contains("home directory"), "{e}");
        let e = resolve(&[&format!("{}/", s(h.to_path_buf()))], h).unwrap_err();
        assert!(e.contains("home directory"), "{e}");

        // A directory without settings.json is not a profile.
        fs::create_dir_all(h.join("plain")).unwrap();
        let e = resolve(&[&s(h.join("plain"))], h).unwrap_err();
        assert!(e.contains("settings.json"), "{e}");

        // ~/.claude however spelled is the default profile.
        profile_dir(h, ".claude");
        let p = resolve(&[&format!("{}/", s(h.join(".claude")))], h).unwrap();
        assert_eq!(p[0].id, "default");
        assert_eq!(p[0].claude_json, h.join(".claude.json"));

        // Another dir is fine; the same dir twice collapses.
        let other = s(profile_dir(h, "elsewhere"));
        let p = resolve(&[&other, &other], h).unwrap();
        assert_eq!(p.len(), 1);
        assert_eq!(p[0].claude_json, h.join("elsewhere/.claude.json"));
    }

    #[test]
    fn shared_claude_json_is_refused() {
        let home = tempfile::tempdir().unwrap();
        let h = home.path();
        let a = profile_dir(h, "a");
        // A configured profile pointing at `a` under another id (e.g. via a symlinked dir).
        let link = h.join("link");
        std::os::unix::fs::symlink(&a, &link).unwrap();
        let configured = [ProfileRef {
            id: "a".into(),
            settings_dir: a.clone(),
        }];
        // The same directory through a symlink collapses rather than conflicting.
        let names = vec![link.to_string_lossy().into_owned()];
        assert!(resolve_profiles_in(&names, &configured, Some(h)).is_ok());
        // A profile recorded at $HOME (hand-edited config) shares ~/.claude.json with ~/.claude.
        profile_dir(h, ".claude");
        let configured = [ProfileRef {
            id: "weird".into(),
            settings_dir: h.to_path_buf(),
        }];
        let e = resolve_profiles_in(&[".claude".into()], &configured, Some(h)).unwrap_err();
        assert!(e.contains("would share"), "{e}");
    }
}
