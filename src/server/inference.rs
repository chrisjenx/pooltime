//! `POST /v1/messages`: route resolution, the per-provider failover loop, the classifier plan
//! and cross-provider fallback (INV §1 exhaustion, §2, §5; port of bro `handleAnthropic` and
//! `proxyAnthropicMessages`' account loop).
//!
//! Flow: request id (set by the router's middleware) -> parse -> resolve route ->
//! * OpenAI route -> that provider only;
//! * classifier (`routing.classifier_route = "anthropic"`) -> Anthropic first, mapped fallback
//!   kept, session affinity never read or written, `X-Pool-Route: classifier`;
//! * unmapped Claude model -> Anthropic only;
//! * mapped family -> [`fallback::pick_provider`] then
//!   [`fallback::serve_with_cross_provider_fallback`].
//!
//! Per provider ([`serve_provider`]): loop { sticky pin if usable and in the active tier, else
//! `choose` -> reserve -> token -> `attempt` } until a response commits, an outcome is terminal,
//! or no account is left. Providers record success/errors themselves; this module only logs each
//! attempt (classified reason, never bodies or upstream text), evicts pins on quota/access
//! denials, and sets affinity on success.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Instant as StdInstant;

use axum::extract::{Request, State};
use axum::response::Response;
use http::HeaderValue;
use serde_json::Value;
use time::OffsetDateTime;

use super::classifier;
use super::fallback::{self, ProviderPool, Served, Verdict};
use super::router::read_json;
use super::AppState;
use crate::accounts::{AccountId, AccountMeta, AccountStatus, RateLimitSnapshot};
use crate::errors::{FailReason, RefreshErrorKind};
use crate::logging::new_request_id;
use crate::models::{family_of, Route};
use crate::providers::anthropic::{
    anthropic_error, refresh_failure_outcome, POOL_ACCOUNT_HEADER, RETRYABLE_TRANSPORT_HEADER,
};
use crate::providers::{AttemptCtx, AttemptOutcome, InboundRequest, ProviderId};
use crate::routing::reserve::WaitBudget;
use crate::routing::{choose, usable, Candidate, Tuning};
use crate::upstream::Limits;

/// Response header carrying the inbound request id (every response).
pub const REQUEST_ID_HEADER: &str = "x-pooltime-request-id";
/// Response header marking a classifier-routed request.
pub const POOL_ROUTE_HEADER: &str = "x-pool-route";

/// The inbound request id, placed in the request extensions by the router middleware.
#[derive(Debug, Clone)]
pub struct RequestId(pub String);

/// Exhaustion message when no account exists at all (bro wording, pooltime command).
pub const NO_ACCOUNTS_MESSAGE: &str =
    "No Claude accounts configured. Add one with: pooltime accounts add <name>";
/// Exhaustion message when accounts exist but none could serve (bro wording verbatim).
pub const ALL_UNAVAILABLE_MESSAGE: &str =
    "All Claude accounts are currently unavailable (logged out or rate limited). Check the dashboard.";

/// `POST /v1/messages`.
pub async fn messages(State(app): State<Arc<AppState>>, req: Request) -> Response {
    let request_id = req
        .extensions()
        .get::<RequestId>()
        .map(|r| r.0.clone())
        .unwrap_or_else(new_request_id);
    let (parts, body) = req.into_parts();
    let (raw_body, json) = match read_json(&parts.headers, body).await {
        Ok(v) => v,
        Err(resp) => return resp,
    };
    let model = json
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let stream = json.get("stream").and_then(Value::as_bool).unwrap_or(false);
    let session_user_id = json
        .pointer("/metadata/user_id")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let req = InboundRequest {
        raw_body,
        json,
        headers: parts.headers,
        model,
        stream,
        session_user_id,
        request_id,
    };
    handle(&app, &req).await
}

async fn handle(app: &Arc<AppState>, req: &InboundRequest) -> Response {
    let (route, mapped) = {
        let table = app.models.read().unwrap_or_else(|e| e.into_inner());
        (table.resolve(&req.model), table.mapped_route(&req.model))
    };
    // One slot-wait budget for the whole request, across retries and providers.
    let budget = WaitBudget::new(app.cfg.timeouts.inflight_wait);

    if route.def.provider == ProviderId::OpenAi {
        return serve_provider(app, ProviderId::OpenAi, &route, req, true, &budget)
            .await
            .response;
    }

    let cr = app.cfg.routing.classifier_route;
    app.drift.note(
        cr,
        &req.json,
        &req.model,
        &req.request_id,
        StdInstant::now(),
    );
    let is_classifier = classifier::bypasses_mapping(cr, &req.json);
    let (first, use_fallback, affinity) = if is_classifier {
        (Some(ProviderId::Anthropic), mapped.is_some(), false)
    } else if mapped.is_none() {
        (Some(ProviderId::Anthropic), false, true)
    } else {
        (choose_mapped_provider(app, req).await, true, true)
    };

    let route_for = |svc: ProviderId| match (svc, mapped.as_ref()) {
        (ProviderId::OpenAi, Some(m)) => m,
        _ => &route,
    };
    let served = match first {
        // Neither provider has a usable account: let the Anthropic path produce its 503.
        None => serve_provider(app, ProviderId::Anthropic, &route, req, affinity, &budget).await,
        Some(first) if use_fallback => {
            fallback::serve_with_cross_provider_fallback(first, &req.request_id, |svc| {
                serve_provider(app, svc, route_for(svc), req, affinity, &budget)
            })
            .await
        }
        Some(first) => serve_provider(app, first, route_for(first), req, affinity, &budget).await,
    };
    let mut resp = served.response;
    if is_classifier {
        resp.headers_mut()
            .insert(POOL_ROUTE_HEADER, HeaderValue::from_static("classifier"));
    }
    resp
}

/// View of one account at a point in time. `creds_ok` comes from the per-pass registry read;
/// the state fields are re-read live by [`live_view`] on every pick.
struct Acct {
    meta: AccountMeta,
    creds_ok: bool,
    status: AccountStatus,
    spent: Vec<String>,
    snapshot: Option<RateLimitSnapshot>,
    active: u32,
}

/// Current state of `meta` from the StateBook and Sessions (cheap, in-memory; safe to call from
/// a `Reservations::reserve` pick closure).
fn live_view(app: &AppState, meta: &AccountMeta, creds_ok: bool, now: OffsetDateTime) -> Acct {
    Acct {
        meta: meta.clone(),
        creds_ok,
        status: app.state.status(&meta.id, creds_ok, now),
        spent: app.state.spent_families(&meta.id, now),
        snapshot: app.state.state(&meta.id, now).snapshot,
        active: app.sessions.active_count(&meta.id, now),
    }
}

/// Every registered account (all providers) with its state at `now`. The registry and
/// credential files are read off the async runtime.
async fn load_accounts(app: &Arc<AppState>, now: OffsetDateTime) -> Vec<Acct> {
    let app = app.clone();
    let job = tokio::task::spawn_blocking(move || {
        app.registry
            .list()
            .into_iter()
            .map(|meta| {
                let creds_ok = matches!(app.registry.read_creds(&meta.id), Ok(Some(_)));
                live_view(&app, &meta, creds_ok, now)
            })
            .collect()
    });
    match job.await {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(error = %e, "account snapshot task failed");
            Vec::new()
        }
    }
}

fn candidate(a: &Acct) -> Candidate<'_> {
    Candidate {
        meta: &a.meta,
        snapshot: a.snapshot.as_ref(),
        active_sessions: a.active,
    }
}

/// Pick within one provider's usable `cands`: a sticky `pinned` account wins while it is among
/// the candidates (usable, not tried, has room) **and** in the active (lowest priority number)
/// tier; otherwise [`choose`].
pub fn pick_account<'a>(
    cands: &[Candidate<'a>],
    pinned: Option<&AccountId>,
    tuning: &Tuning,
    cursor: &std::sync::atomic::AtomicUsize,
    now: OffsetDateTime,
    family: Option<&str>,
) -> Option<&'a AccountId> {
    if let Some(pin) = pinned {
        let min = cands.iter().map(|c| c.meta.priority).min()?;
        if let Some(c) = cands
            .iter()
            .find(|c| &c.meta.id == pin && c.meta.priority == min)
        {
            let meta: &'a AccountMeta = c.meta;
            return Some(&meta.id);
        }
    }
    choose(cands, tuning, cursor, now, family)
}

fn family_for(provider: ProviderId, model: &str) -> Option<&'static str> {
    match provider {
        ProviderId::Anthropic => family_of(model),
        // Codex accounts have no per-family windows.
        ProviderId::OpenAi => None,
    }
}

fn cap_for(app: &AppState, provider: ProviderId) -> u32 {
    match provider {
        ProviderId::Anthropic => app.cfg.routing.anthropic_max_inflight,
        ProviderId::OpenAi => app.cfg.routing.codex_max_inflight,
    }
}

/// Provider for a mapped request (bro `chooseMappedService`): Anthropic listed first so exact
/// ties keep Claude primary; capacity is a preference, so a second pass ignores in-flight caps.
async fn choose_mapped_provider(app: &Arc<AppState>, req: &InboundRequest) -> Option<ProviderId> {
    let now = OffsetDateTime::now_utc();
    let accts = load_accounts(app, now).await;
    let tuning = Tuning::from(&app.cfg.routing);
    let user = req.session_user_id.as_deref();
    let order = [ProviderId::Anthropic, ProviderId::OpenAi];
    let pins: Vec<(ProviderId, AccountId)> = user
        .map(|u| {
            order
                .iter()
                .filter_map(|p| app.sessions.pinned(*p, u, now).map(|a| (*p, a)))
                .collect()
        })
        .unwrap_or_default();
    let most_recent = user.and_then(|u| app.sessions.last_provider(u));
    let no_tried = HashSet::new();
    for respect_cap in [true, false] {
        let pools: Vec<ProviderPool<'_>> = order
            .iter()
            .filter(|p| app.providers.contains_key(p))
            .map(|&p| {
                let family = family_for(p, &req.model);
                let cap = cap_for(app, p);
                let cands = accts
                    .iter()
                    .filter(|a| usable(&a.meta, &a.status, p, family, &a.spent, &no_tried))
                    .filter(|a| {
                        !respect_cap || cap == 0 || app.reservations.in_flight(&a.meta.id) < cap
                    })
                    .map(candidate)
                    .collect();
                ProviderPool {
                    provider: p,
                    family,
                    cands,
                }
            })
            .collect();
        if let Some(p) =
            fallback::pick_provider(&pools, &pins, most_recent, &tuning, &app.cursor, now)
        {
            return Some(p);
        }
    }
    None
}

fn set_account_header(resp: &mut Response, acct: &AccountId) {
    if let Ok(v) = HeaderValue::from_str(acct.as_str()) {
        resp.headers_mut().insert(POOL_ACCOUNT_HEADER, v);
    }
}

/// Serve `req` on one provider, failing over across its accounts.
async fn serve_provider(
    app: &Arc<AppState>,
    pid: ProviderId,
    route: &Route,
    req: &InboundRequest,
    affinity: bool,
    budget: &WaitBudget,
) -> Served {
    let rid = req.request_id.as_str();
    let Some(provider) = app.providers.get(&pid) else {
        tracing::warn!(request_id = rid, provider = %pid, "no provider registered for route");
        return Served {
            response: anthropic_error(
                503,
                "api_error",
                &format!("pooltime: no {pid} provider is available to serve this request"),
            ),
            verdict: Verdict::NoAccount,
        };
    };
    let family = family_for(pid, &req.model);
    let user = if affinity {
        req.session_user_id.as_deref()
    } else {
        None
    };
    let tuning = Tuning::from(&app.cfg.routing);
    let idle = match pid {
        ProviderId::Anthropic => app.cfg.timeouts.anthropic_idle,
        ProviderId::OpenAi => app.cfg.timeouts.codex_idle,
    };
    let mut tried: HashSet<AccountId> = HashSet::new();
    let mut last: Option<(FailReason, Response)> = None;
    let mut total_accounts;

    loop {
        let now = OffsetDateTime::now_utc();
        let accts = load_accounts(app, now).await;
        total_accounts = accts.len();
        // Registry metadata and credential presence come from this pass; account state, load
        // and the session pin are re-read live on every pick, because `reserve` may re-pick
        // many times while it waits for a slot (a bench or an expired cooldown must count).
        let pick = |has_room: &dyn Fn(&AccountId) -> bool| {
            let now = OffsetDateTime::now_utc();
            let live: Vec<Acct> = accts
                .iter()
                .filter(|a| a.meta.provider == pid && !tried.contains(&a.meta.id))
                .filter(|a| has_room(&a.meta.id))
                .map(|a| live_view(app, &a.meta, a.creds_ok, now))
                .collect();
            let cands: Vec<Candidate<'_>> = live
                .iter()
                .filter(|a| usable(&a.meta, &a.status, pid, family, &a.spent, &tried))
                .map(candidate)
                .collect();
            let pin = user.and_then(|u| app.sessions.pinned(pid, u, now));
            pick_account(&cands, pin.as_ref(), &tuning, &app.cursor, now, family).cloned()
        };
        let Some(reservation) = app.reservations.reserve(pick, pid, budget).await else {
            break;
        };
        let acct_id = reservation.account().clone();
        let Some(meta) = accts.iter().find(|a| a.meta.id == acct_id).map(|a| &a.meta) else {
            // Unreachable: `pick` only returns ids from `accts`.
            tried.insert(acct_id);
            continue;
        };
        let outcome = match provider.access_token(meta, false).await {
            Ok(token) => {
                let limits = Limits {
                    header_wait: idle,
                    idle,
                    // Caps the streamed body too, so it is the whole-request timeout.
                    deadline: tokio::time::Instant::now() + app.cfg.timeouts.request_timeout,
                };
                let ctx = AttemptCtx {
                    account: meta,
                    token,
                    route,
                    limits,
                    upstream: &app.upstream,
                    reservation: Some(reservation),
                };
                provider.attempt(ctx, req, &app.state).await
            }
            Err(kind) => {
                drop(reservation);
                refresh_failure_outcome(kind)
            }
        };
        match outcome {
            AttemptOutcome::Done(mut resp, _usage) => {
                tracing::info!(
                    request_id = rid,
                    account = %acct_id,
                    provider = %pid,
                    outcome = "done",
                    status = resp.status().as_u16(),
                    "attempt served"
                );
                if let Some(u) = user {
                    app.sessions
                        .set(pid, u, &acct_id, OffsetDateTime::now_utc());
                }
                set_account_header(&mut resp, &acct_id);
                return Served {
                    response: resp,
                    verdict: Verdict::Served,
                };
            }
            AttemptOutcome::Terminal(mut resp, reason) => {
                tracing::warn!(
                    request_id = rid,
                    account = %acct_id,
                    provider = %pid,
                    outcome = "terminal",
                    reason = %reason.detail(),
                    status = resp.status().as_u16(),
                    "attempt failed"
                );
                set_account_header(&mut resp, &acct_id);
                return Served {
                    response: resp,
                    verdict: Verdict::Failed(reason),
                };
            }
            AttemptOutcome::Retry {
                reason,
                fallback_response,
            } => {
                tracing::warn!(
                    request_id = rid,
                    account = %acct_id,
                    provider = %pid,
                    outcome = "retry",
                    reason = %reason.detail(),
                    "attempt failed; trying another account"
                );
                if matches!(
                    reason,
                    FailReason::Quota { .. } | FailReason::AccessDenied | FailReason::Billing
                ) {
                    app.sessions.evict_account(&acct_id);
                }
                tried.insert(acct_id);
                last = Some((reason, fallback_response));
            }
        }
    }

    tracing::warn!(
        request_id = rid,
        provider = %pid,
        tried = tried.len(),
        last_reason = %last.as_ref().map_or_else(|| "no_account".into(), |(r, _)| r.detail()),
        "no account left to serve the request"
    );
    match last {
        Some((reason, mut resp)) => {
            // A refresh that failed on the network is a transport failure too (bro).
            if matches!(reason, FailReason::Refresh(RefreshErrorKind::Network(_))) {
                resp.headers_mut()
                    .insert(RETRYABLE_TRANSPORT_HEADER, HeaderValue::from_static("1"));
            }
            Served {
                response: resp,
                verdict: Verdict::Failed(reason),
            }
        }
        None => Served {
            response: anthropic_error(
                503,
                "overloaded_error",
                if total_accounts == 0 {
                    NO_ACCOUNTS_MESSAGE
                } else {
                    ALL_UNAVAILABLE_MESSAGE
                },
            ),
            verdict: Verdict::NoAccount,
        },
    }
}
