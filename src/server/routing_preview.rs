//! `GET /api/v1/routing/preview?model=<id>`: which account the next request for a model would
//! go to, and why (the dashboard's Routing view and the `explain_routing` MCP tool).
//!
//! The preview runs the live router's own steps over the live account view, not a copy of the
//! formula: [`inference::resolve_routes`] for the route, [`inference::mapped_provider`] (and so
//! [`fallback::pick_provider_explained`](super::fallback::pick_provider_explained)) for a mapped
//! family's provider, then [`inference::candidates`] + [`inference::pick_account`] (and so
//! [`crate::routing::choose`]) with the in-flight cap pass first, as `serve_provider`'s reserve
//! closure does. The per-account factors are [`crate::routing::rank`]'s, the ranking `choose`
//! itself decides on. It is a *new* session's view: no session pin applies.
//!
//! Read-only: account state, sessions and in-flight counts are only read; the round-robin
//! cursor is copied, never advanced; no pin is set and nothing is journaled.
//!
//! With `POOLTIME_FIXTURE_STATE` set, the accounts, tuning, in-flight counts and clock come from
//! the fixture document (its `accounts`, `tuning` and `now`), and the model list from its
//! `journal.requests`; the route and mapping come from the daemon's model table.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use axum::extract::{Query, State};
use axum::response::Response;
use serde_json::{json, Value};
use time::format_description::well_known::Rfc3339;
use time::{Duration, OffsetDateTime};

use super::fallback::{ProviderPick, ProviderWhy};
use super::inference::{self, Acct, ProviderEnv};
use super::mgmt::{bad, blocking, internal, ok, ts};
use super::AppState;
use crate::accounts::status::{is_spent, spent_families_of};
use crate::accounts::{
    AccountId, AccountMeta, AccountStatus, RateLimitSnapshot, TimedOverride, Window, WindowStatus,
};
use crate::models::family_of;
use crate::providers::ProviderId;
use crate::routing::{rank, score, usable, Candidate, ExpiryRank, Ranking, Tuning};
use crate::store::journal;

/// How far back the journal is read for the model picker.
const MODELS_WINDOW_SECS: i64 = 7 * 24 * 3600;
/// How long the picker's model usage is reused before the journal is read again.
const MODELS_TTL: std::time::Duration = std::time::Duration::from_secs(60);
/// Most-requested models offered in the picker.
const MODELS_LIMIT: u32 = 20;
/// Longest accepted `model` parameter.
const MODEL_MAX_LEN: usize = 200;

/// How a request for the model is routed before any account is chosen.
#[derive(Debug, Clone, PartialEq)]
pub(super) enum Plan {
    /// One provider only (an OpenAI route, or an unmapped Claude model).
    Direct(ProviderId),
    /// A mapped Claude family: the provider is chosen per request; `target` is the OpenAI route.
    Mapped { target: String },
}

/// Everything the preview reads.
pub(super) struct Inputs<'a> {
    pub accts: &'a [Acct],
    pub model: &'a str,
    pub plan: Plan,
    pub registered: &'a dyn Fn(ProviderId) -> bool,
    pub in_flight: &'a dyn Fn(&AccountId) -> u32,
    /// Max concurrent requests per account for a provider; 0 = unlimited.
    pub cap: &'a dyn Fn(ProviderId) -> u32,
    pub tuning: &'a Tuning,
    pub cursor: &'a AtomicUsize,
    pub now: OffsetDateTime,
}

/// The preview's answer: the JSON document and the pick it describes.
pub(super) struct Preview {
    pub doc: Value,
    /// The pick `doc` describes (the tests compare it with the live router's).
    #[cfg_attr(not(test), allow(dead_code))]
    pub next: Option<(ProviderId, AccountId)>,
}

/// One provider's account choice for a new session.
struct PoolPick<'a> {
    provider: ProviderId,
    family: Option<&'static str>,
    /// The candidate set the pick ran on (capped pass, or uncapped when every account is busy).
    cands: Vec<Candidate<'a>>,
    ranking: Option<Ranking<'a>>,
    pick: Option<AccountId>,
    /// Every usable account is at its in-flight cap: the request waits for a slot.
    waits: bool,
}

fn pool_pick<'a>(inp: &Inputs<'a>, provider: ProviderId) -> PoolPick<'a> {
    let family = inference::family_for(provider, inp.model);
    let none = HashSet::new();
    let cap = (inp.cap)(provider);
    let mine = || {
        inp.accts
            .iter()
            .filter(move |a| a.meta.provider == provider)
    };
    // `serve_provider`: pick among accounts with room; with none, the uncapped pick waits for
    // a slot. Each pass gets its own copy of the cursor, so nothing is advanced.
    let peek = || AtomicUsize::new(inp.cursor.load(Ordering::SeqCst));
    let capped = inference::candidates(
        mine().filter(|a| cap == 0 || (inp.in_flight)(&a.meta.id) < cap),
        provider,
        family,
        &none,
    );
    let capped_pick =
        inference::pick_account(&capped, None, inp.tuning, &peek(), inp.now, family).cloned();
    let (cands, pick, waits) = if capped_pick.is_some() {
        (capped, capped_pick, false)
    } else {
        let all = inference::candidates(mine(), provider, family, &none);
        let pick =
            inference::pick_account(&all, None, inp.tuning, &peek(), inp.now, family).cloned();
        let waits = pick.is_some();
        (all, pick, waits)
    };
    let ranking = rank(&cands, inp.tuning, inp.now, family);
    PoolPick {
        provider,
        family,
        cands,
        ranking,
        pick,
        waits,
    }
}

/// The routing preview over `inp`.
pub(super) fn preview(inp: &Inputs<'_>) -> Preview {
    let env = ProviderEnv {
        registered: inp.registered,
        has_room: &|p, id| {
            let cap = (inp.cap)(p);
            cap == 0 || (inp.in_flight)(id) < cap
        },
        tuning: inp.tuning,
        cursor: inp.cursor,
        now: inp.now,
    };
    // The provider the request is served on first (a new session: no pins).
    let (provider, provider_pick): (ProviderId, Option<ProviderPick>) = match &inp.plan {
        Plan::Direct(p) => (*p, None),
        Plan::Mapped { .. } => {
            match inference::mapped_provider(inp.accts, inp.model, &[], None, &env) {
                Some(pp) => (pp.provider, Some(pp)),
                // No usable account on either provider: the Anthropic path answers 503.
                None => (ProviderId::Anthropic, None),
            }
        }
    };
    let registered = (inp.registered)(provider);
    let providers: Vec<ProviderId> = match &inp.plan {
        Plan::Direct(p) => vec![*p],
        Plan::Mapped { .. } => [ProviderId::Anthropic, ProviderId::OpenAi]
            .into_iter()
            .filter(|p| (inp.registered)(*p))
            .collect(),
    };
    let pools: Vec<PoolPick<'_>> = providers.iter().map(|p| pool_pick(inp, *p)).collect();
    let chosen = pools.iter().find(|p| p.provider == provider);
    let next = chosen
        .filter(|_| registered)
        .and_then(|p| p.pick.clone().map(|id| (provider, id)));

    let mut rows: Vec<(Value, bool, Option<f64>)> = inp
        .accts
        .iter()
        .map(|a| account_row(inp, a, &pools, provider, next.as_ref()))
        .collect();
    // The pick first, then eligible accounts by score, then the rest; ids break ties.
    rows.sort_by(|(a, an, ascore), (b, bn, bscore)| {
        bn.cmp(an)
            .then_with(|| b["eligible"].as_bool().cmp(&a["eligible"].as_bool()))
            .then_with(|| {
                bscore
                    .unwrap_or(f64::NEG_INFINITY)
                    .total_cmp(&ascore.unwrap_or(f64::NEG_INFINITY))
            })
            .then_with(|| a["id"].as_str().cmp(&b["id"].as_str()))
    });

    let next_doc = match (&next, chosen) {
        (Some((_, id)), Some(pool)) => next_json(inp, id, pool),
        _ => Value::Null,
    };
    let summary = summary(inp, provider, registered, provider_pick.as_ref(), &next_doc);
    let pinned: u32 = inp.accts.iter().map(|a| a.active).sum();
    let doc = json!({
        "model": inp.model,
        "family": family_of(inp.model),
        "now": ts(inp.now),
        "provider": registered.then_some(provider.as_str()),
        "mapped_to": match &inp.plan {
            Plan::Mapped { target } => Some(target.as_str()),
            Plan::Direct(_) => None,
        },
        "provider_choice": provider_pick.as_ref().map(provider_choice_json),
        "active_tier": chosen.and_then(|p| p.ranking.as_ref()).map(|r| r.tier),
        "tuning": {
            "five_hour_exp": inp.tuning.five_hour_exp,
            "min_headroom": inp.tuning.min_headroom,
            "headroom_taper_start": inp.tuning.headroom_taper_start,
        },
        "next": next_doc,
        "summary": summary,
        "sticky": {
            "pinned_sessions": pinned,
            "note": "A request from an existing session stays on the account it is pinned to while \
                that account can serve and is in the active tier. This preview is for a new session.",
        },
        "accounts": rows.into_iter().map(|(v, _, _)| v).collect::<Vec<_>>(),
    });
    Preview { doc, next }
}

fn provider_choice_json(pp: &ProviderPick) -> Value {
    let (reason, contenders) = match &pp.why {
        ProviderWhy::Only => ("only", Vec::new()),
        ProviderWhy::Sticky => ("sticky", Vec::new()),
        ProviderWhy::Score(c) => (
            "score",
            c.iter()
                .map(|(p, id, score, viable)| {
                    json!({"provider": p.as_str(), "account": id.as_str(), "score": r4(*score),
                        "viable": viable})
                })
                .collect(),
        ),
    };
    json!({"provider": pp.provider.as_str(), "reason": reason, "contenders": contenders})
}

/// One account's row; also returns whether it is the pick and its score (for ordering).
fn account_row(
    inp: &Inputs<'_>,
    a: &Acct,
    pools: &[PoolPick<'_>],
    provider: ProviderId,
    next: Option<&(ProviderId, AccountId)>,
) -> (Value, bool, Option<f64>) {
    let p = a.meta.provider;
    let pool = pools.iter().find(|pool| pool.provider == p);
    let family = inference::family_for(p, inp.model);
    let snap = a.snapshot.as_ref();
    let headroom = score::gate_headroom(snap, family, inp.now);
    let expiry = score::expiry_rank(snap, family, inp.now);
    let ranked = pool.and_then(|pool| {
        let r = pool.ranking.as_ref()?;
        let i = r.rows.iter().position(|row| row.id == &a.meta.id)?;
        Some((r, i))
    });
    let is_next = next.is_some_and(|(np, id)| *np == p && *id == a.meta.id);
    let excluded = exclusion(inp, a, pool, provider, ranked, headroom);
    let score_v = ranked.map(|(r, i)| r.rows[i].score);
    let cap = (inp.cap)(p);
    let override_v = match &a.meta.weight_override {
        Some(TimedOverride { weight, until }) if *until > inp.now => {
            json!({"weight": weight, "until": ts(*until)})
        }
        _ => Value::Null,
    };
    let row = json!({
        "id": a.meta.id.as_str(),
        "provider": p.as_str(),
        "status": a.status.label(),
        "eligible": excluded.is_none(),
        "excluded": excluded,
        "factors": {
            // No usage data at all: unknown (the scorer treats it as full headroom).
            "five_hour_used": snap.map(|_| r4(1.0 - headroom)),
            "seven_day_used": score::seven_day_used(snap, family, inp.now).map(r4),
            "expiry": match expiry {
                ExpiryRank::Unknown => "unknown",
                ExpiryRank::At(_) => "at",
                ExpiryRank::Spent => "spent",
            },
            "expiry_reset": match expiry {
                ExpiryRank::At(r) => Some(ts(r)),
                _ => None,
            },
            "expiry_share": ranked.map(|(r, i)| r.rows[i].expiry_share),
            "taper": r4(score::five_hour_factor(headroom, inp.tuning)),
            "viable": headroom >= inp.tuning.min_headroom,
            "priority": a.meta.priority,
            "weight": a.meta.weight,
            "weight_override": override_v,
            "effective_weight": score::effective_weight(&a.meta, inp.now),
            "sessions": a.active,
            "in_flight": (inp.in_flight)(&a.meta.id),
            "cap": cap,
        },
        "score": score_v.map(r4),
        "tied": ranked.is_some_and(|(r, i)| r.tied.len() > 1 && r.tied.contains(&i)),
        "next": is_next,
    });
    (row, is_next, score_v)
}

fn excluded(reason: &str, detail: String, until: Option<OffsetDateTime>) -> Option<Value> {
    Some(json!({"reason": reason, "detail": detail, "until": until.map(ts)}))
}

/// Why `a` cannot take the next request (`None`: it competes).
fn exclusion(
    inp: &Inputs<'_>,
    a: &Acct,
    pool: Option<&PoolPick<'_>>,
    provider: ProviderId,
    ranked: Option<(&Ranking<'_>, usize)>,
    headroom: f64,
) -> Option<Value> {
    let p = a.meta.provider;
    let Some(pool) = pool else {
        let detail = if matches!(inp.plan, Plan::Direct(_)) {
            format!("{} is served by {provider} accounts only", inp.model)
        } else {
            format!("no {p} provider is running")
        };
        return excluded("provider_mismatch", detail, None);
    };
    if !(inp.registered)(p) {
        return excluded(
            "provider_unavailable",
            format!("no {p} provider is running"),
            None,
        );
    }
    if !usable(
        &a.meta,
        &a.status,
        p,
        pool.family,
        &a.spent,
        &HashSet::new(),
    ) {
        return Some(match &a.status {
            AccountStatus::NeedsLogin => {
                json!({"reason": "needs_login", "detail": "signed out: needs a re-login", "until": null})
            }
            AccountStatus::RateLimited { until, window } => json!({
                "reason": "rate_limited",
                "detail": match window {
                    Some(w) => format!("rate-limited ({w}) until {}", clock(*until)),
                    None => format!("rate-limited until {}", clock(*until)),
                },
                "until": ts(*until),
            }),
            AccountStatus::Billing => {
                json!({"reason": "billing", "detail": "billing problem on the subscription", "until": null})
            }
            AccountStatus::AccessDenied { until } => json!({
                "reason": "access_denied",
                "detail": format!("access denied; rechecked at {}", clock(*until)),
                "until": ts(*until),
            }),
            _ => {
                let fam = pool.family.unwrap_or_default();
                let until = spent_reset(a.snapshot.as_ref(), fam, inp.now);
                json!({
                    "reason": "family_limit",
                    "detail": match until {
                        Some(u) => format!("{fam} limit used up until {}", clock(u)),
                        None => format!("{fam} limit used up"),
                    },
                    "until": until.map(ts),
                })
            }
        });
    }
    if !pool.cands.iter().any(|c| c.meta.id == a.meta.id) {
        return excluded(
            "at_capacity",
            format!(
                "{} requests in flight (cap {})",
                (inp.in_flight)(&a.meta.id),
                (inp.cap)(p)
            ),
            None,
        );
    }
    let Some((r, i)) = ranked else {
        let tier = pool.ranking.as_ref().map_or(0, |r| r.tier);
        return excluded(
            "reserve_tier",
            format!(
                "priority {} is a reserve: priority {tier} accounts are used first",
                a.meta.priority
            ),
            None,
        );
    };
    if !r.competes(i) {
        return excluded(
            "below_min_headroom",
            format!(
                "5-hour window {} used, over the {} limit while other accounts have room",
                pct(1.0 - headroom),
                pct(1.0 - inp.tuning.min_headroom)
            ),
            None,
        );
    }
    if p != provider {
        return excluded(
            "fallback_provider",
            format!("{provider} serves this request; {p} takes it only if {provider} fails"),
            None,
        );
    }
    None
}

/// Soonest future reset of a spent window scoped to `family`.
fn spent_reset(
    snap: Option<&RateLimitSnapshot>,
    family: &str,
    now: OffsetDateTime,
) -> Option<OffsetDateTime> {
    snap?
        .windows
        .iter()
        .filter(|w| w.model.as_deref() == Some(family) && is_spent(w))
        .filter_map(|w| w.reset.filter(|r| *r > now))
        .min()
}

fn next_json(inp: &Inputs<'_>, id: &AccountId, pool: &PoolPick<'_>) -> Value {
    let Some(r) = pool.ranking.as_ref() else {
        return Value::Null;
    };
    let Some(i) = r.rows.iter().position(|row| row.id == id) else {
        return Value::Null;
    };
    let competing: Vec<usize> = (0..r.rows.len()).filter(|&j| r.competes(j)).collect();
    let reason = if competing.len() == 1 {
        "only_candidate"
    } else if r.tied.len() > 1 {
        "tie_break"
    } else if r.best_effort {
        "best_effort"
    } else {
        "highest_score"
    };
    let runner_up = competing
        .iter()
        .filter(|&&j| j != i)
        .max_by(|&&x, &&y| r.rows[x].score.total_cmp(&r.rows[y].score))
        .map(|&j| json!({"account": r.rows[j].id.as_str(), "score": r4(r.rows[j].score)}));
    let acct = inp.accts.iter().find(|a| &a.meta.id == id);
    let snap = acct.and_then(|a| a.snapshot.as_ref());
    json!({
        "account": id.as_str(),
        "provider": pool.provider.as_str(),
        "score": r4(r.rows[i].score),
        "reason": reason,
        "tied_with": r.tied.iter().filter(|&&j| j != i).map(|&j| r.rows[j].id.as_str()).collect::<Vec<_>>(),
        "runner_up": runner_up,
        "waits_for_slot": pool.waits,
        "seven_day_used": score::seven_day_used(snap, pool.family, inp.now).map(r4),
        "expiry_reset": match r.rows[i].expiry {
            ExpiryRank::At(t) => Some(ts(t)),
            _ => None,
        },
    })
}

/// The plain-English line on top of the Routing view.
fn summary(
    inp: &Inputs<'_>,
    provider: ProviderId,
    registered: bool,
    provider_pick: Option<&ProviderPick>,
    next: &Value,
) -> String {
    let model = inp.model;
    if !registered {
        return format!(
            "No {provider} provider is running, so a {model} request cannot be served."
        );
    }
    let Some(acct) = next["account"].as_str() else {
        return format!(
            "No account can take a new {model} request now: every {provider} account is excluded \
             (see the reasons below)."
        );
    };
    let score = next["score"].as_f64().unwrap_or(0.0);
    let why = match next["reason"].as_str() {
        Some("only_candidate") => format!("the only eligible account (score {score:.2})"),
        Some("tie_break") => {
            let others: Vec<&str> = next["tied_with"]
                .as_array()
                .map(|v| v.iter().filter_map(Value::as_str).collect())
                .unwrap_or_default();
            // The cursor is shared by all traffic, so whose turn it is can change any moment.
            format!(
                "tied with {} at {score:.2}; round-robin picks between them (this is the turn \
                 right now)",
                others.join(", ")
            )
        }
        Some("best_effort") => format!(
            "highest score ({score:.2}); every account is over the 5-hour limit, so this is best effort"
        ),
        _ => match (
            next["runner_up"]["account"].as_str(),
            next["runner_up"]["score"].as_f64(),
        ) {
            (Some(r), Some(rs)) => format!("highest score ({score:.2}, ahead of {r} at {rs:.2})"),
            _ => format!("highest score ({score:.2})"),
        },
    };
    let mut line = format!("Next request for {model} goes to {acct}");
    if let (Some(pp), Plan::Mapped { target }) = (provider_pick, &inp.plan) {
        let fam = family_of(model).unwrap_or("this family");
        let how = match &pp.why {
            ProviderWhy::Only => format!("only {} has an account free", pp.provider),
            ProviderWhy::Sticky => "the session is pinned there".to_string(),
            ProviderWhy::Score(_) => format!("{}'s best account scores highest", pp.provider),
        };
        line.push_str(&format!(
            " on {provider} ({fam} is mapped to {target}; {how})"
        ));
    }
    line.push_str(": ");
    line.push_str(&why);
    if let Some(u) = next["seven_day_used"].as_f64() {
        line.push_str(&format!(", 7-day {} used", pct(u)));
    }
    match next["expiry_reset"]
        .as_str()
        .and_then(|s| OffsetDateTime::parse(s, &Rfc3339).ok())
    {
        Some(r) => line.push_str(&format!(", resets in {}", eta(r - inp.now))),
        None => line.push_str(", 7-day reset not known yet"),
    }
    line.push('.');
    if next["waits_for_slot"].as_bool() == Some(true) {
        line.push_str(
            " Every account is at its in-flight cap, so the request waits for a free slot first, \
             then picks again: it may go to whichever account frees up.",
        );
    }
    line
}

/// Rounded to 4 decimals for display (decisions use the unrounded values).
fn r4(x: f64) -> f64 {
    (x * 10_000.0).round() / 10_000.0
}

fn pct(x: f64) -> String {
    format!("{}%", (x * 100.0).round() as i64)
}

/// `2d 3h`, `5h 12m`, `12m`, `under a minute`.
fn eta(d: Duration) -> String {
    let mins = d.whole_minutes().max(0);
    let (days, hours, m) = (mins / 1440, (mins % 1440) / 60, mins % 60);
    match (days, hours, m) {
        (0, 0, 0) => "under a minute".into(),
        (0, 0, m) => format!("{m}m"),
        (0, h, m) => format!("{h}h {m}m"),
        (d, 0, _) => format!("{d}d"),
        (d, h, _) => format!("{d}d {h}h"),
    }
}

/// `2026-11-07 15:00 UTC`.
fn clock(t: OffsetDateTime) -> String {
    let t = t.to_offset(time::UtcOffset::UTC);
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02} UTC",
        t.year(),
        u8::from(t.month()),
        t.day(),
        t.hour(),
        t.minute()
    )
}

// ---- handler ---------------------------------------------------------------------------------

/// The model-usage cache behind the picker: when it was read and what it said.
pub(crate) type ModelUsageCache =
    std::sync::Mutex<Option<(std::time::Instant, Vec<(String, u64)>)>>;

/// Most requested models of the past week, from the journal at most once per
/// [`MODELS_TTL`] (the query groups a week of journal rows; the dashboard polls every few
/// seconds). Fixture mode reads the fixture's `journal.requests` instead.
#[allow(clippy::result_large_err)] // the error is the ready-made HTTP answer, as in `mgmt`
async fn model_usage(app: &Arc<AppState>) -> Result<Vec<(String, u64)>, Response> {
    if let Some(doc) = &app.fixture_state {
        return Ok(fixture_models(doc));
    }
    {
        let cache = app.model_usage.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((_, v)) = cache.as_ref().filter(|(at, _)| at.elapsed() < MODELS_TTL) {
            return Ok(v.clone());
        }
    }
    let a = app.clone();
    let since = OffsetDateTime::now_utc().unix_timestamp() - MODELS_WINDOW_SECS;
    match blocking(move || {
        a.store
            .read(|c| journal::top_models(c, since, MODELS_LIMIT))
    })
    .await
    {
        Ok(Ok(v)) => {
            *app.model_usage.lock().unwrap_or_else(|e| e.into_inner()) =
                Some((std::time::Instant::now(), v.clone()));
            Ok(v)
        }
        Ok(Err(e)) => {
            tracing::warn!(error = %format!("{e:#}"), "reading model usage failed");
            Err(internal("could not read model usage"))
        }
        Err(r) => Err(r),
    }
}

/// `GET /api/v1/routing/preview?model=<id>[&models=true]` (management key or session
/// required). Without `model`, previews the most requested Claude model of the past week. The
/// picker fields (`default_model`, `models`) are only in the answer when `model` is absent or
/// `models=true`, so polling a chosen model reads no model usage at all.
pub(crate) async fn preview_handler(
    State(app): State<Arc<AppState>>,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    let asked = q.get("model").map(|m| m.trim()).filter(|m| !m.is_empty());
    if asked.is_some_and(|m| m.len() > MODEL_MAX_LEN || m.chars().any(char::is_control)) {
        return bad(format!(
            "model must be a model id of at most {MODEL_MAX_LEN} characters"
        ));
    }
    let want_models = asked.is_none()
        || q.get("models")
            .is_some_and(|v| matches!(v.as_str(), "true" | "1"));
    let listing: Vec<(String, ProviderId)> = {
        let table = app.models.read().unwrap_or_else(|e| e.into_inner());
        table
            .rows_for_listing()
            .into_iter()
            .map(|r| (r.id.clone(), r.provider))
            .collect()
    };
    let used = if want_models {
        match model_usage(&app).await {
            Ok(v) => v,
            Err(r) => return r,
        }
    } else {
        Vec::new()
    };
    let fixture = app.fixture_state.clone();
    let (accts, in_flight, now, tuning) = match &fixture {
        Some(doc) => {
            let now = doc["now"]
                .as_str()
                .and_then(|s| OffsetDateTime::parse(s, &Rfc3339).ok())
                .unwrap_or_else(OffsetDateTime::now_utc);
            let (accts, in_flight) = fixture_accounts(doc, now);
            (accts, in_flight, now, fixture_tuning(doc, app.tuning()))
        }
        None => {
            let now = OffsetDateTime::now_utc();
            // The same account view a request loads (registry, credential presence, state).
            let accts = inference::load_accounts(&app, now).await;
            let in_flight = accts
                .iter()
                .map(|a| (a.meta.id.clone(), app.reservations.in_flight(&a.meta.id)))
                .collect();
            (accts, in_flight, now, app.tuning())
        }
    };
    let default_model = used
        .iter()
        .map(|(m, _)| m.as_str())
        .find(|m| family_of(m).is_some())
        .or_else(|| {
            listing
                .iter()
                .find(|(id, p)| *p == ProviderId::Anthropic && family_of(id).is_some())
                .map(|(id, _)| id.as_str())
        })
        .unwrap_or("sonnet")
        .to_string();
    let model = asked.map_or_else(|| default_model.clone(), str::to_string);
    let plan = plan_for(&app, &model);
    let cursor = AtomicUsize::new(if fixture.is_some() {
        0
    } else {
        app.cursor.load(Ordering::SeqCst)
    });
    let in_flight_of = |id: &AccountId| in_flight.get(id).copied().unwrap_or(0);
    let registered = |p: ProviderId| fixture.is_some() || app.providers.contains_key(&p);
    let cap = |p: ProviderId| inference::cap_for(&app, p);
    let out = preview(&Inputs {
        accts: &accts,
        model: &model,
        plan,
        registered: &registered,
        in_flight: &in_flight_of,
        cap: &cap,
        tuning: &tuning,
        cursor: &cursor,
        now,
    });
    let mut doc = out.doc;
    if !want_models {
        return ok(doc);
    }
    let mut models: Vec<Value> = Vec::new();
    let mut seen = HashSet::new();
    for (m, n) in &used {
        if seen.insert(m.clone()) {
            models.push(json!({"id": m, "requests": n}));
        }
    }
    for (id, _) in &listing {
        if seen.insert(id.clone()) {
            models.push(json!({"id": id, "requests": 0}));
        }
    }
    if seen.insert(model.clone()) {
        models.push(json!({"id": model, "requests": 0}));
    }
    if let Value::Object(o) = &mut doc {
        o.insert("default_model".into(), json!(default_model));
        o.insert("models".into(), Value::Array(models));
    }
    ok(doc)
}

/// The route plan for `model` from the daemon's model table, as the inference path resolves it.
fn plan_for(app: &AppState, model: &str) -> Plan {
    let (route, mapped) = inference::resolve_routes(app, model, &Value::Null);
    if route.def.provider == ProviderId::OpenAi {
        return Plan::Direct(ProviderId::OpenAi);
    }
    if mapped.is_none() {
        return Plan::Direct(ProviderId::Anthropic);
    }
    let table = app.models.read().unwrap_or_else(|e| e.into_inner());
    let target = family_of(model)
        .and_then(|f| table.mapping_for(f))
        .map(|m| m.to.clone())
        .unwrap_or_default();
    Plan::Mapped { target }
}

// ---- fixture ---------------------------------------------------------------------------------

fn fixture_tuning(doc: &Value, fallback: Tuning) -> Tuning {
    let t = &doc["tuning"];
    match (
        t["five_hour_exp"].as_f64(),
        t["min_headroom"].as_f64(),
        t["headroom_taper_start"].as_f64(),
    ) {
        (Some(e), Some(m), Some(h)) => Tuning::new(e, m, h).unwrap_or(fallback),
        _ => fallback,
    }
}

fn fixture_time(v: &Value) -> Option<OffsetDateTime> {
    v.as_str()
        .and_then(|s| OffsetDateTime::parse(s, &Rfc3339).ok())
}

/// Routing views of the fixture's `/api/v1/status` accounts (display states mapped back to
/// routing ones) and their in-flight counts.
fn fixture_accounts(doc: &Value, now: OffsetDateTime) -> (Vec<Acct>, HashMap<AccountId, u32>) {
    let mut accts = Vec::new();
    let mut in_flight = HashMap::new();
    for a in doc["accounts"].as_array().into_iter().flatten() {
        let (Some(id), Some(provider)) = (
            a["id"].as_str().and_then(|s| s.parse::<AccountId>().ok()),
            a["provider"]
                .as_str()
                .and_then(|s| s.parse::<ProviderId>().ok()),
        ) else {
            continue;
        };
        let mut meta = AccountMeta::new(id.clone(), provider);
        if let Some(p) = a["priority"].as_u64().and_then(|p| u32::try_from(p).ok()) {
            meta.priority = p;
        }
        if let Some(w) = a["weight"].as_f64() {
            meta.weight = w;
        }
        meta.weight_override = match (
            a["weight_override"]["weight"].as_f64(),
            fixture_time(&a["weight_override"]["until"]),
        ) {
            (Some(weight), Some(until)) => Some(TimedOverride { weight, until }),
            _ => None,
        };
        let until = fixture_time(&a["until"]).unwrap_or(now + Duration::hours(1));
        let status = match a["status"].as_str().unwrap_or_default() {
            // Display-only states over a routable account.
            "ready" | "expiring" | "erroring" => AccountStatus::Ready,
            "rate_limited" => AccountStatus::RateLimited {
                until,
                window: None,
            },
            "billing" => AccountStatus::Billing,
            "access_denied" => AccountStatus::AccessDenied { until },
            _ => AccountStatus::NeedsLogin,
        };
        let windows: Vec<Window> = a["windows"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|w| {
                let utilization = w["utilization"].as_f64()?;
                Some(Window {
                    key: w["key"].as_str()?.to_string(),
                    model: w["model"].as_str().map(str::to_string),
                    status: if utilization >= 1.0 {
                        WindowStatus::Rejected
                    } else {
                        WindowStatus::Allowed
                    },
                    utilization,
                    reset: fixture_time(&w["reset"]),
                    reset_synthetic: false,
                })
            })
            .collect();
        let snapshot = (!windows.is_empty()).then(|| RateLimitSnapshot {
            unified_status: a["unified_status"].as_str().map(str::to_string),
            windows,
            updated_at: now,
        });
        let spent = snapshot
            .as_ref()
            .map_or_else(Vec::new, |s| spent_families_of(s, now));
        let active = a["sessions"]
            .as_u64()
            .and_then(|n| u32::try_from(n).ok())
            .unwrap_or(0);
        in_flight.insert(
            id,
            a["in_flight"]
                .as_u64()
                .and_then(|n| u32::try_from(n).ok())
                .unwrap_or(0),
        );
        accts.push(Acct {
            meta,
            creds_ok: true,
            status,
            spent,
            snapshot,
            active,
        });
    }
    (accts, in_flight)
}

/// Model usage from the fixture's `journal.requests`, most requested first.
fn fixture_models(doc: &Value) -> Vec<(String, u64)> {
    let mut counts: HashMap<String, u64> = HashMap::new();
    for r in doc["journal"]["requests"].as_array().into_iter().flatten() {
        if let Some(m) = r["model"].as_str().filter(|m| !m.is_empty()) {
            *counts.entry(m.to_string()).or_default() += 1;
        }
    }
    let mut v: Vec<(String, u64)> = counts.into_iter().collect();
    v.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    v
}

#[cfg(test)]
mod tests {
    //! The preview's pick must equal the live router's pick for the same state. "Live" here is
    //! the real decision path minus I/O: [`inference::mapped_provider`] for a mapped family, then
    //! [`Reservations::reserve`] with `serve_provider`'s pick ([`inference::pick_among`]) and the
    //! shared round-robin cursor, which it advances on ties.

    use super::*;
    use crate::routing::reserve::{Reservations, WaitBudget};
    use time::macros::datetime;

    const NOW: OffsetDateTime = datetime!(2026-11-07 12:00 UTC);
    const MODEL: &str = "claude-opus-5";

    fn win(key: &str, model: Option<&str>, utilization: f64, reset: Duration) -> Window {
        Window {
            key: key.into(),
            model: model.map(str::to_string),
            status: if utilization >= 1.0 {
                WindowStatus::Rejected
            } else {
                WindowStatus::Allowed
            },
            utilization,
            reset: Some(NOW + reset),
            reset_synthetic: false,
        }
    }

    /// A ready Anthropic account: 5h `u5` used, 7d 30% used resetting in `d7` days.
    fn acct(id: &str, u5: f64, d7: i64) -> Acct {
        acct_with(
            id,
            ProviderId::Anthropic,
            vec![
                win("5h", None, u5, Duration::hours(2)),
                win("7d", None, 0.3, Duration::days(d7)),
            ],
        )
    }

    fn acct_with(id: &str, provider: ProviderId, windows: Vec<Window>) -> Acct {
        let snapshot = RateLimitSnapshot {
            unified_status: None,
            windows,
            updated_at: NOW,
        };
        Acct {
            meta: AccountMeta::new(id.parse().unwrap(), provider),
            creds_ok: true,
            status: AccountStatus::Ready,
            spent: spent_families_of(&snapshot, NOW),
            snapshot: Some(snapshot),
            active: 0,
        }
    }

    fn caps() -> HashMap<ProviderId, u32> {
        HashMap::from([(ProviderId::Anthropic, 0), (ProviderId::OpenAi, 1)])
    }

    struct World {
        accts: Vec<Acct>,
        plan: Plan,
        res: Reservations,
        cursor: AtomicUsize,
        tuning: Tuning,
    }

    impl World {
        fn new(accts: Vec<Acct>) -> World {
            World {
                accts,
                plan: Plan::Direct(ProviderId::Anthropic),
                res: Reservations::new(caps()),
                cursor: AtomicUsize::new(0),
                tuning: Tuning::default(),
            }
        }

        fn preview(&self) -> Preview {
            let registered = |_: ProviderId| true;
            let in_flight = |id: &AccountId| self.res.in_flight(id);
            let cap = |p: ProviderId| caps()[&p];
            preview(&Inputs {
                accts: &self.accts,
                model: MODEL,
                plan: self.plan.clone(),
                registered: &registered,
                in_flight: &in_flight,
                cap: &cap,
                tuning: &self.tuning,
                cursor: &self.cursor,
                now: NOW,
            })
        }

        async fn live(&self) -> Option<(ProviderId, AccountId)> {
            let registered = |_: ProviderId| true;
            let has_room = |p: ProviderId, id: &AccountId| {
                let cap = caps()[&p];
                cap == 0 || self.res.in_flight(id) < cap
            };
            let provider = match &self.plan {
                Plan::Direct(p) => *p,
                Plan::Mapped { .. } => {
                    let env = ProviderEnv {
                        registered: &registered,
                        has_room: &has_room,
                        tuning: &self.tuning,
                        cursor: &self.cursor,
                        now: NOW,
                    };
                    inference::mapped_provider(&self.accts, MODEL, &[], None, &env)
                        .map_or(ProviderId::Anthropic, |p| p.provider)
                }
            };
            let family = inference::family_for(provider, MODEL);
            let tried = HashSet::new();
            // An expired budget: when every account is at cap, exceed it at once (the live
            // router waits first, then does the same).
            let budget = WaitBudget::new(std::time::Duration::ZERO);
            let pick = |has_room: &dyn Fn(&AccountId) -> bool| {
                inference::pick_among(
                    self.accts
                        .iter()
                        .filter(|a| a.meta.provider == provider && has_room(&a.meta.id)),
                    provider,
                    family,
                    &tried,
                    None,
                    &self.tuning,
                    &self.cursor,
                    NOW,
                )
            };
            let r = self.res.reserve(pick, provider, &budget).await?;
            Some((provider, r.account().clone()))
        }

        /// Preview, check it left the cursor alone, then the live pick; both must agree.
        async fn agree(&self) -> Preview {
            let before = self.cursor.load(Ordering::SeqCst);
            let p = self.preview();
            assert_eq!(
                self.cursor.load(Ordering::SeqCst),
                before,
                "preview moved the cursor"
            );
            let live = self.live().await;
            assert_eq!(
                p.next, live,
                "preview and live router disagree: {:#}",
                p.doc
            );
            assert_eq!(
                p.doc["next"]["account"].as_str(),
                live.as_ref().map(|(_, id)| id.as_str())
            );
            p
        }
    }

    fn row<'a>(doc: &'a Value, id: &str) -> &'a Value {
        doc["accounts"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["id"] == id)
            .unwrap_or_else(|| panic!("no row {id}"))
    }

    fn reason<'a>(doc: &'a Value, id: &str) -> &'a str {
        row(doc, id)["excluded"]["reason"].as_str().unwrap_or("")
    }

    #[tokio::test]
    async fn highest_score_wins() {
        let w = World::new(vec![acct("later", 0.2, 5), acct("soon", 0.2, 1)]);
        let p = w.agree().await;
        assert_eq!(p.doc["next"]["account"], "soon");
        assert_eq!(p.doc["next"]["reason"], "highest_score");
        assert_eq!(row(&p.doc, "soon")["factors"]["expiry_share"], 5.0);
        assert_eq!(row(&p.doc, "later")["factors"]["expiry_share"], 3.0);
        assert_eq!(
            p.doc["accounts"][0]["id"], "soon",
            "the pick is listed first"
        );
        let s = p.doc["summary"].as_str().unwrap();
        assert!(
            s.starts_with(
                "Next request for claude-opus-5 goes to soon: highest score (5.00, ahead of \
                 later at 3.00)"
            ),
            "{s}"
        );
        assert!(s.contains("7-day 30% used, resets in 1d"), "{s}");
    }

    #[tokio::test]
    async fn ties_follow_the_cursor_without_advancing_it() {
        let w = World::new(vec![
            acct("a", 0.2, 2),
            acct("b", 0.2, 2),
            acct("c", 0.2, 2),
        ]);
        let mut seen = Vec::new();
        for _ in 0..6 {
            // Several previews in a row change nothing.
            let first = w.preview().next;
            assert_eq!(w.preview().next, first);
            let p = w.agree().await;
            assert_eq!(p.doc["next"]["reason"], "tie_break");
            assert_eq!(p.doc["next"]["tied_with"].as_array().unwrap().len(), 2);
            seen.push(p.next.unwrap().1.to_string());
        }
        assert_eq!(seen, ["a", "b", "c", "a", "b", "c"]);
    }

    #[tokio::test]
    async fn exclusions_are_explained_and_skipped() {
        let mut limited = acct("limited", 0.2, 1);
        limited.status = AccountStatus::RateLimited {
            until: NOW + Duration::hours(3),
            window: Some("5h".into()),
        };
        let mut dead = acct("dead", 0.2, 1);
        dead.status = AccountStatus::NeedsLogin;
        let mut billing = acct("billing", 0.2, 1);
        billing.status = AccountStatus::Billing;
        let opus_spent = acct_with(
            "opus-spent",
            ProviderId::Anthropic,
            vec![
                win("5h", None, 0.1, Duration::hours(2)),
                win("7d-opus", Some("opus"), 1.0, Duration::days(2)),
            ],
        );
        let codex = acct_with(
            "codex",
            ProviderId::OpenAi,
            vec![win("7d", None, 0.1, Duration::days(1))],
        );
        let mut reserve = acct("reserve", 0.2, 1);
        reserve.meta.priority = 200;
        let hot = acct("hot", 0.95, 1);
        let w = World::new(vec![
            limited,
            dead,
            billing,
            opus_spent,
            codex,
            reserve,
            hot,
            acct("ok", 0.2, 4),
        ]);
        let p = w.agree().await;
        assert_eq!(p.doc["next"]["account"], "ok");
        assert_eq!(p.doc["next"]["reason"], "only_candidate");
        assert_eq!(reason(&p.doc, "limited"), "rate_limited");
        assert_eq!(
            row(&p.doc, "limited")["excluded"]["until"],
            "2026-11-07T15:00:00Z"
        );
        assert_eq!(reason(&p.doc, "dead"), "needs_login");
        assert_eq!(reason(&p.doc, "billing"), "billing");
        assert_eq!(reason(&p.doc, "opus-spent"), "family_limit");
        assert_eq!(
            row(&p.doc, "opus-spent")["excluded"]["until"],
            "2026-11-09T12:00:00Z"
        );
        assert_eq!(reason(&p.doc, "codex"), "provider_mismatch");
        assert_eq!(reason(&p.doc, "reserve"), "reserve_tier");
        assert_eq!(reason(&p.doc, "hot"), "below_min_headroom");
        assert_eq!(row(&p.doc, "ok")["eligible"], true);
        assert_eq!(row(&p.doc, "ok")["excluded"], Value::Null);
        assert_eq!(p.doc["active_tier"], 100);
    }

    #[tokio::test]
    async fn best_effort_when_nothing_is_viable() {
        let w = World::new(vec![acct("x", 0.95, 1), acct("y", 0.97, 3)]);
        let p = w.agree().await;
        assert_eq!(p.doc["next"]["reason"], "best_effort");
        assert_eq!(row(&p.doc, "x")["eligible"], true);
        assert_eq!(row(&p.doc, "y")["eligible"], true);
    }

    #[tokio::test]
    async fn timed_override_counts_only_until_it_expires() {
        let mut boosted = acct("boosted", 0.2, 5);
        boosted.meta.weight_override = Some(TimedOverride {
            weight: 10.0,
            until: NOW + Duration::hours(1),
        });
        let mut lapsed = acct("lapsed", 0.2, 6);
        lapsed.meta.weight_override = Some(TimedOverride {
            weight: 100.0,
            until: NOW - Duration::hours(1),
        });
        let w = World::new(vec![acct("soon", 0.2, 1), boosted, lapsed]);
        let p = w.agree().await;
        assert_eq!(p.doc["next"]["account"], "boosted");
        let f = &row(&p.doc, "boosted")["factors"];
        assert_eq!(f["effective_weight"], 10.0);
        assert_eq!(f["weight_override"]["until"], "2026-11-07T13:00:00Z");
        let f = &row(&p.doc, "lapsed")["factors"];
        assert_eq!(f["effective_weight"], 1.0);
        assert_eq!(f["weight_override"], Value::Null);
    }

    #[tokio::test]
    async fn pinned_sessions_lower_the_score() {
        let mut busy = acct("busy", 0.2, 1);
        busy.active = 4;
        let w = World::new(vec![busy, acct("idle", 0.2, 1)]);
        let p = w.agree().await;
        assert_eq!(p.doc["next"]["account"], "idle");
        assert_eq!(row(&p.doc, "busy")["factors"]["sessions"], 4);
        assert_eq!(row(&p.doc, "busy")["score"], 1.0);
        assert_eq!(p.doc["sticky"]["pinned_sessions"], 4);
    }

    #[tokio::test]
    async fn in_flight_caps_skip_busy_accounts_then_wait() {
        let codex = |id: &str, d7: i64| {
            acct_with(
                id,
                ProviderId::OpenAi,
                vec![win("7d", None, 0.1, Duration::days(d7))],
            )
        };
        let mut w = World::new(vec![codex("c1", 1), codex("c2", 3)]);
        w.plan = Plan::Direct(ProviderId::OpenAi);
        let budget = WaitBudget::new(std::time::Duration::from_secs(60));
        let only = |id: &'static str| {
            move |has_room: &dyn Fn(&AccountId) -> bool| {
                let id: AccountId = id.parse().unwrap();
                has_room(&id).then_some(id)
            }
        };
        let held = w
            .res
            .reserve(only("c1"), ProviderId::OpenAi, &budget)
            .await
            .unwrap();
        let p = w.agree().await;
        assert_eq!(p.doc["next"]["account"], "c2");
        assert_eq!(reason(&p.doc, "c1"), "at_capacity");
        assert_eq!(p.doc["next"]["waits_for_slot"], false);

        let held2 = w
            .res
            .reserve(only("c2"), ProviderId::OpenAi, &budget)
            .await
            .unwrap();
        let p = w.agree().await;
        assert_eq!(p.doc["next"]["account"], "c1");
        assert_eq!(p.doc["next"]["waits_for_slot"], true);
        assert!(p.doc["summary"]
            .as_str()
            .unwrap()
            .contains("waits for a free slot"));
        drop((held, held2));
    }

    #[tokio::test]
    async fn mapped_family_picks_the_provider_like_the_router() {
        let codex = acct_with(
            "codex",
            ProviderId::OpenAi,
            vec![win("7d", None, 0.1, Duration::days(1))],
        );
        let mut w = World::new(vec![acct("claude", 0.2, 5), codex]);
        w.plan = Plan::Mapped {
            target: "gpt-6-sol".into(),
        };
        let p = w.agree().await;
        assert_eq!(p.doc["provider"], "openai");
        assert_eq!(p.doc["mapped_to"], "gpt-6-sol");
        assert_eq!(p.doc["provider_choice"]["reason"], "score");
        assert_eq!(reason(&p.doc, "claude"), "fallback_provider");
        assert!(p.doc["summary"]
            .as_str()
            .unwrap()
            .contains("on openai (opus is mapped to gpt-6-sol"));

        // Only Claude left: the provider choice follows.
        w.accts[1].status = AccountStatus::NeedsLogin;
        let p = w.agree().await;
        assert_eq!(p.doc["provider"], "anthropic");
        assert_eq!(p.doc["provider_choice"]["reason"], "only");
    }

    #[tokio::test]
    async fn nothing_usable_previews_no_pick() {
        let mut dead = acct("dead", 0.2, 1);
        dead.status = AccountStatus::NeedsLogin;
        let w = World::new(vec![dead]);
        let p = w.agree().await;
        assert_eq!(p.next, None);
        assert_eq!(p.doc["next"], Value::Null);
        assert!(p.doc["summary"]
            .as_str()
            .unwrap()
            .starts_with("No account can take a new claude-opus-5 request"));
        let w = World::new(vec![]);
        assert_eq!(w.agree().await.next, None);
    }

    #[test]
    fn eta_forms() {
        assert_eq!(eta(Duration::seconds(30)), "under a minute");
        assert_eq!(eta(Duration::minutes(12)), "12m");
        assert_eq!(eta(Duration::minutes(312)), "5h 12m");
        assert_eq!(eta(Duration::days(2)), "2d");
        assert_eq!(eta(Duration::hours(51)), "2d 3h");
        assert_eq!(eta(-Duration::hours(1)), "under a minute");
    }
}
