//! Runway forecast: will the pool's 7-day quota last until capacity comes back?
//!
//! # Model
//! - **Demand** is the pool's recent burn rate `R` (subscriptions per hour, 1 account = 1 sub, as
//!   in [`super::burn`]): the 7-day consumption of every account in the history over the trailing
//!   [`RATE_WINDOW_SECS`] (24 h: a full day, so a burst does not dominate and nights do not zero
//!   it), using the burn report's consumption math ([`usage_history::step`]: window resets count
//!   from 0, jitter never counts). A delta whose readings straddle the window start counts in
//!   proportion. When the history starts inside the window, the rate is over the span actually
//!   covered; with less than [`MIN_SPAN_SECS`] of history there is no forecast. Idle hours (no
//!   samples: the sampler records changes only) simply consume nothing.
//! - **Supply** is each *usable* account's 7-day headroom now (`1 - utilization`; accounts that
//!   need login, have a billing problem or are access-denied count for nothing, as does an account
//!   without a known 7-day window). An account's window resets at its own `resets_at`, restoring
//!   it to a full sub, then again every 7 days. A reset already in the past means the window is
//!   fresh. Synthetic resets (made up for a spent window) are unknown: no capacity comes back.
//! - **Simulation**: demand `R` drains the account that resets soonest first (the router's 7d
//!   expiry ordering: use-it-or-lose-it), stepping from reset to reset up to [`HORIZON_SECS`].
//!   The pool **runs out** when the total headroom of all usable accounts hits 0 before the next
//!   reset; how it is split between accounts does not change when that happens. The gap until the
//!   next reset after that moment is how long the pool is **short**.
//!
//! [`Runway::at_risk`] is set when the pool is short by at least `alerts.runway_warn_hours`
//! (`0` disables it), or out with no reset known. The daemon caches the forecast for
//! [`CACHE_TTL`] ([`current`]): the burn API, the alerts and the status cache share it.

use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::Serialize;
use time::{OffsetDateTime, UtcOffset};

use crate::accounts::status::window_duration;
use crate::accounts::AccountStatus;
use crate::server::mgmt::StatusInputs;
use crate::server::AppState;
use crate::store::usage_history::{self, step, HistRow, WEEK, WINDOW_7D};
use crate::store::Store;

/// Trailing window the burn rate is measured over.
pub const RATE_WINDOW_SECS: i64 = 24 * 3600;
/// Less history than this: no forecast.
pub const MIN_SPAN_SECS: i64 = 3 * 3600;
/// How far ahead the simulation looks (two full reset cycles).
pub const HORIZON_SECS: i64 = 2 * WEEK;
/// Default `alerts.runway_warn_hours`.
pub const DEFAULT_WARN_HOURS: u32 = 6;
/// How long the daemon reuses a computed forecast.
pub const CACHE_TTL: Duration = Duration::from_secs(30);

/// One account's 7-day window now, as the forecast sees it.
#[derive(Debug, Clone, PartialEq)]
pub struct AccountNow {
    pub id: String,
    /// Whether it can serve at all (not needs_login / billing / access_denied).
    pub usable: bool,
    /// 7-day utilization (0-1); `None` when unknown.
    pub used: Option<f64>,
    /// The window's real `resets_at` (unix seconds).
    pub reset: Option<i64>,
}

/// The accounts of a status read (the account-wide 7-day window of each).
pub fn accounts_of(inputs: &StatusInputs) -> Vec<AccountNow> {
    let week = time::Duration::days(7);
    inputs
        .accounts
        .iter()
        .map(|v| {
            let usable = !matches!(
                v.status,
                AccountStatus::NeedsLogin
                    | AccountStatus::Billing
                    | AccountStatus::AccessDenied { .. }
            );
            let w = v.state.snapshot.as_ref().and_then(|s| {
                s.windows
                    .iter()
                    .find(|w| w.model.is_none() && window_duration(&w.key) == Some(week))
            });
            AccountNow {
                id: v.meta.id.to_string(),
                usable,
                used: w.map(|w| w.utilization.clamp(0.0, 1.0)),
                reset: w
                    .filter(|w| !w.reset_synthetic)
                    .and_then(|w| w.reset)
                    .map(|r| r.unix_timestamp()),
            }
        })
        .collect()
}

/// The pool's burn rate over the trailing window.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rate {
    /// Subscriptions per second.
    pub per_sec: f64,
    /// Seconds of history the rate is measured over.
    pub span: i64,
}

/// The pool's consumption rate from `rows` (sorted by account, then time, as
/// [`usage_history::rows_since`] returns them) over `[now - window, now]`. `None` with less than
/// [`MIN_SPAN_SECS`] of history.
pub fn pool_rate(rows: &[HistRow], now: i64, window: i64) -> Option<Rate> {
    let start = now - window;
    let mut consumed = 0.0;
    let mut covered_from: Option<i64> = None;
    let mut prev: Option<(&str, usage_history::Point, f64)> = None;
    for r in rows {
        let from = if r.first.ts <= start {
            start
        } else {
            r.first.ts
        };
        covered_from = Some(covered_from.map_or(from, |c| c.min(from)));
        let h = match prev.filter(|(pa, _, _)| *pa == r.account) {
            Some((_, last, hw)) => {
                let s = step(&last, hw, &r.first);
                let (t0, t1) = (s.from, r.first.ts);
                if s.consumed > 0.0 && t1 > start {
                    consumed += if t1 <= t0 {
                        s.consumed
                    } else {
                        let lo = t0.max(start);
                        let hi = t1.min(now);
                        s.consumed * ((hi - lo).max(0) as f64 / (t1 - t0) as f64)
                    };
                }
                s.hw
            }
            None => r.first.util,
        };
        let (inner, end_hw) = r.inner(h);
        if inner > 0.0 && r.anchor() >= start {
            consumed += inner;
        }
        prev = Some((r.account.as_str(), r.last, end_hw));
    }
    let span = now - covered_from?.max(start);
    (span >= MIN_SPAN_SECS).then(|| Rate {
        per_sec: consumed.max(0.0) / span as f64,
        span,
    })
}

/// Where the simulation ends.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Projection {
    /// When the pool runs out (`now` when it already has).
    pub exhausts_at: Option<i64>,
    /// The first reset of a usable account after `exhausts_at` (`None`: none known).
    pub next_reset: Option<i64>,
}

/// Simulate `accounts` drained at `per_sec` from `now` (see the module docs). `None` when no
/// usable account has a known 7-day window.
pub fn project(accounts: &[AccountNow], per_sec: f64, now: i64) -> Option<Projection> {
    // (headroom, next reset)
    let mut pool: Vec<(f64, Option<i64>)> = accounts
        .iter()
        .filter(|a| a.usable)
        .filter_map(|a| {
            let used = a.used?;
            Some(match a.reset {
                // Already reset: a fresh window, next reset a whole number of weeks later.
                Some(r) if r <= now => (1.0, Some(r + ((now - r) / WEEK + 1) * WEEK)),
                r => ((1.0 - used).clamp(0.0, 1.0), r),
            })
        })
        .collect();
    if pool.is_empty() {
        return None;
    }
    let horizon = now + HORIZON_SECS;
    let per_sec = per_sec.max(0.0);
    let mut t = now;
    loop {
        let next = pool.iter().filter_map(|p| p.1).filter(|r| *r > t).min();
        let end = next.map_or(horizon, |n| n.min(horizon));
        let total: f64 = pool.iter().map(|p| p.0).sum();
        let need = per_sec * (end - t) as f64;
        if total <= 1e-9 || (per_sec > 0.0 && need >= total) {
            let at = if total <= 1e-9 {
                t
            } else {
                t + (total / per_sec).round() as i64
            };
            return Some(Projection {
                exhausts_at: Some(at),
                next_reset: next,
            });
        }
        if end >= horizon {
            return Some(Projection {
                exhausts_at: None,
                next_reset: next,
            });
        }
        // Soonest reset first; unknown resets last.
        let mut order: Vec<usize> = (0..pool.len()).collect();
        order.sort_by_key(|&i| pool[i].1.unwrap_or(i64::MAX));
        let mut left = need;
        for i in order {
            let take = pool[i].0.min(left);
            pool[i].0 -= take;
            left -= take;
        }
        for p in pool.iter_mut().filter(|p| p.1 == Some(end)) {
            *p = (1.0, Some(end + WEEK));
        }
        t = end;
    }
}

/// The forecast as the API, status and alerts show it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Runway {
    /// `unknown` (too little history, or no usable account with a known 7-day window), `lasts`
    /// (no shortfall within the horizon), `short` (runs out before capacity comes back) or `out`
    /// (no 7-day headroom left now).
    pub state: &'static str,
    /// The pool's burn rate in subscriptions per hour.
    pub rate_per_hour: Option<f64>,
    /// Hours of history the rate is measured over.
    pub rate_window_hours: Option<f64>,
    /// Usable 7-day headroom now, in subscriptions.
    pub headroom: f64,
    #[serde(with = "time::serde::rfc3339::option")]
    pub exhausts_at: Option<OffsetDateTime>,
    /// The next reset after `exhausts_at`.
    #[serde(with = "time::serde::rfc3339::option")]
    pub next_reset: Option<OffsetDateTime>,
    /// `next_reset - exhausts_at`.
    pub short_by_secs: Option<i64>,
    /// Short by at least `warn_hours` (or out with no reset known), with the warning enabled.
    pub at_risk: bool,
    /// `alerts.runway_warn_hours` (`0`: no warning).
    pub warn_hours: u32,
    /// The earliest upcoming reset of a usable account: identifies the reset cycle for alert
    /// dedupe.
    #[serde(skip)]
    pub cycle: Option<i64>,
}

fn round4(x: f64) -> f64 {
    // `+ 0.0`: an empty sum is -0.0, shown as such.
    (x * 10_000.0).round() / 10_000.0 + 0.0
}

fn at(ts: i64) -> Option<OffsetDateTime> {
    OffsetDateTime::from_unix_timestamp(ts).ok()
}

/// The forecast for `accounts` at `now` from a measured `rate`.
pub fn forecast(accounts: &[AccountNow], rate: Option<Rate>, now: i64, warn_hours: u32) -> Runway {
    let usable: Vec<&AccountNow> = accounts
        .iter()
        .filter(|a| a.usable && a.used.is_some())
        .collect();
    let headroom: f64 = usable
        .iter()
        .map(|a| match a.reset {
            Some(r) if r <= now => 1.0,
            _ => 1.0 - a.used.unwrap_or(1.0),
        })
        .sum();
    let cycle = usable
        .iter()
        .filter_map(|a| a.reset)
        .map(|r| {
            if r <= now {
                r + ((now - r) / WEEK + 1) * WEEK
            } else {
                r
            }
        })
        .min();
    let mut out = Runway {
        state: "unknown",
        rate_per_hour: rate.map(|r| round4(r.per_sec * 3600.0)),
        rate_window_hours: rate.map(|r| round4(r.span as f64 / 3600.0)),
        headroom: round4(headroom),
        exhausts_at: None,
        next_reset: None,
        short_by_secs: None,
        at_risk: false,
        warn_hours,
        cycle,
    };
    // Out of headroom is known without a rate.
    let per_sec = match rate {
        Some(r) => r.per_sec,
        None if !usable.is_empty() && headroom <= 1e-9 => 0.0,
        None => return out,
    };
    let Some(p) = project(accounts, per_sec, now) else {
        return out;
    };
    let Some(ex) = p.exhausts_at else {
        out.state = "lasts";
        return out;
    };
    out.state = if ex <= now { "out" } else { "short" };
    out.exhausts_at = at(ex);
    out.next_reset = p.next_reset.and_then(at);
    out.short_by_secs = p.next_reset.map(|r| (r - ex).max(0));
    out.at_risk = warn_hours > 0
        && out
            .short_by_secs
            .is_none_or(|s| s >= i64::from(warn_hours) * 3600);
    out
}

/// The forecast from the store's history and a status read. A blocking read: call off the async
/// threads.
pub fn report(
    store: &Store,
    accounts: &[AccountNow],
    now: OffsetDateTime,
    warn_hours: u32,
) -> anyhow::Result<Runway> {
    let now = now.unix_timestamp();
    let rows = store.read(|c| usage_history::rows_since(c, WINDOW_7D, now - RATE_WINDOW_SECS))?;
    let rate = pool_rate(&rows, now, RATE_WINDOW_SECS);
    Ok(forecast(accounts, rate, now, warn_hours))
}

/// The daemon's forecast cache ([`current`]).
pub(crate) type Cache = std::sync::Mutex<Option<(Instant, Runway)>>;

/// The daemon's forecast at `now`, computed at most once per [`CACHE_TTL`]. `None` when it cannot
/// be computed (logged) or in fixture mode.
pub async fn current(app: &Arc<AppState>, now: OffsetDateTime) -> Option<Runway> {
    if app.fixture_state.is_some() {
        return None;
    }
    {
        let cache = app.runway.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((_, r)) = cache.as_ref().filter(|(t, _)| t.elapsed() < CACHE_TTL) {
            return Some(r.clone());
        }
    }
    let inputs = match crate::server::mgmt::try_gather(app, now).await {
        Ok(i) => i,
        Err(e) => {
            tracing::warn!(error = %e, "runway: could not read the pool");
            return None;
        }
    };
    let accounts = accounts_of(&inputs);
    let (a, warn) = (app.clone(), app.cfg.alerts.runway_warn_hours);
    match crate::background::blocking(move || report(&a.store, &accounts, now, warn)).await {
        Ok(Ok(r)) => {
            *app.runway.lock().unwrap_or_else(|e| e.into_inner()) =
                Some((Instant::now(), r.clone()));
            Some(r)
        }
        Ok(Err(e)) => {
            tracing::warn!(error = %format!("{e:#}"), "runway: could not read the usage history");
            None
        }
        Err(e) => {
            tracing::warn!(error = %e, "runway: forecast task failed");
            None
        }
    }
}

/// `Thu 09:00` in `offset`.
pub fn fmt_when(t: OffsetDateTime, offset: UtcOffset) -> String {
    let fd = time::macros::format_description!("[weekday repr:short] [hour]:[minute]");
    t.to_offset(offset)
        .format(&fd)
        .unwrap_or_else(|_| "?".into())
}

/// `<1h`, `~14h`, `~3 days`.
pub fn fmt_span(secs: i64) -> String {
    let h = (secs as f64 / 3600.0).round() as i64;
    if secs < 3600 {
        "<1h".into()
    } else if h < 48 {
        format!("~{h}h")
    } else {
        format!("~{} days", (secs as f64 / 86_400.0).round() as i64)
    }
}

/// The one-sentence summary of a `short` / `out` forecast (`None` otherwise), times in `offset`:
/// "At the current rate the pool runs out ~14h before the next reset (Thu 09:00)".
pub fn message(r: &Runway, offset: UtcOffset) -> Option<String> {
    describe(
        r.state,
        r.exhausts_at,
        r.next_reset,
        r.short_by_secs,
        offset,
    )
}

/// [`message`] from a forecast's fields (the CLI reads them from the API's JSON).
pub fn describe(
    state: &str,
    exhausts_at: Option<OffsetDateTime>,
    next_reset: Option<OffsetDateTime>,
    short_by_secs: Option<i64>,
    offset: UtcOffset,
) -> Option<String> {
    let ex = exhausts_at?;
    Some(match (state, next_reset, short_by_secs) {
        ("out", Some(n), _) => format!(
            "The pool is out of 7-day quota until the next reset ({})",
            fmt_when(n, offset)
        ),
        ("out", None, _) => "The pool is out of 7-day quota (no reset known)".into(),
        ("short", Some(n), Some(s)) => format!(
            "At the current rate the pool runs out {} before the next reset ({})",
            fmt_span(s),
            fmt_when(n, offset)
        ),
        ("short", _, _) => format!(
            "At the current rate the pool runs out of 7-day quota {} (no reset known)",
            fmt_when(ex, offset)
        ),
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::usage_history::Point;

    /// 2026-11-09T00:00:00Z, a Monday.
    const MON: i64 = 1_794_182_400;
    const H: i64 = 3600;
    const DAY: i64 = 86_400;

    fn raw(account: &str, ts: i64, util: f64, reset: i64) -> HistRow {
        HistRow::raw(
            account,
            WINDOW_7D,
            Point {
                ts,
                util,
                reset: Some(reset),
            },
        )
    }

    fn acct(id: &str, used: f64, reset: Option<i64>) -> AccountNow {
        AccountNow {
            id: id.into(),
            usable: true,
            used: Some(used),
            reset,
        }
    }

    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-9
    }

    #[test]
    fn steady_burn_rate_over_the_trailing_day() {
        let now = MON + 2 * DAY;
        let reset = MON + 5 * DAY;
        // a: +0.02/h for 24 h (sampled every 2 h), b: +0.24 in one 24 h step that started 12 h
        // before the window (half of it counts).
        let mut rows: Vec<HistRow> = (0..=12)
            .map(|i| raw("a", now - DAY + i * 2 * H, 0.1 + 0.04 * i as f64, reset))
            .collect();
        rows.push(raw("b", now - DAY - 12 * H, 0.0, reset));
        rows.push(raw("b", now - 12 * H, 0.24, reset));
        let r = pool_rate(&rows, now, RATE_WINDOW_SECS).unwrap();
        assert_eq!(r.span, DAY);
        assert!(close(r.per_sec * 3600.0, (0.48 + 0.12) / 24.0), "{r:?}");
    }

    #[test]
    fn idle_history_short_history_and_resets() {
        let now = MON + 2 * DAY;
        let reset = MON + 5 * DAY;
        // Idle: the last reading is two days old (nothing changed since): a zero rate over 24 h.
        let idle = vec![raw("a", now - 2 * DAY, 0.5, reset)];
        let r = pool_rate(&idle, now, RATE_WINDOW_SECS).unwrap();
        assert_eq!((r.per_sec, r.span), (0.0, DAY));
        // No history, or under three hours of it: no rate.
        assert_eq!(pool_rate(&[], now, RATE_WINDOW_SECS), None);
        let young = vec![
            raw("a", now - 2 * H, 0.1, reset),
            raw("a", now - H, 0.3, reset),
        ];
        assert_eq!(pool_rate(&young, now, RATE_WINDOW_SECS), None);
        // Six hours of history: the rate is over those six hours (the first reading is free).
        let six = vec![
            raw("a", now - 6 * H, 0.1, reset),
            raw("a", now - H, 0.4, reset),
        ];
        let r = pool_rate(&six, now, RATE_WINDOW_SECS).unwrap();
        assert_eq!(r.span, 6 * H);
        assert!(close(r.per_sec * 3600.0, 0.05), "{r:?}");
        // A window reset counts from 0: 0.8 -> (reset) 0.1 consumed 0.1, not -0.7.
        let rows = vec![
            raw("a", now - 20 * H, 0.8, now - 10 * H),
            raw("a", now - 5 * H, 0.1, now - 10 * H + WEEK),
        ];
        let r = pool_rate(&rows, now, RATE_WINDOW_SECS).unwrap();
        assert!(close(r.per_sec * 3600.0, 0.1 / 20.0), "{r:?}");
    }

    #[test]
    fn steady_burn_runs_out_before_the_reset() {
        let now = MON;
        // Two accounts at 50% and 70%, both reset in 3 days; 0.8 subs left at 0.02 subs/h: out
        // in 40 h, 32 h before the reset.
        let pool = [
            acct("a", 0.5, Some(now + 3 * DAY)),
            acct("b", 0.7, Some(now + 3 * DAY)),
        ];
        let rate = Rate {
            per_sec: 0.02 / 3600.0,
            span: DAY,
        };
        let r = forecast(&pool, Some(rate), now, 6);
        assert_eq!(r.state, "short");
        assert_eq!(r.exhausts_at.unwrap().unix_timestamp(), now + 40 * H);
        assert_eq!(r.next_reset.unwrap().unix_timestamp(), now + 3 * DAY);
        assert_eq!(r.short_by_secs, Some(32 * H));
        assert!(r.at_risk);
        assert!(close(r.headroom, 0.8));
        assert_eq!(r.cycle, Some(now + 3 * DAY));
        assert_eq!(
            message(&r, UtcOffset::UTC).unwrap(),
            "At the current rate the pool runs out ~32h before the next reset (Thu 00:00)"
        );
        // Under the threshold: short, but not at risk; 0 disables the warning.
        assert!(!forecast(&pool, Some(rate), now, 33).at_risk);
        assert!(!forecast(&pool, Some(rate), now, 0).at_risk);
        // A slow burn lasts.
        let slow = Rate {
            per_sec: 0.001 / 3600.0,
            span: DAY,
        };
        let r = forecast(&pool, Some(slow), now, 6);
        assert_eq!((r.state, r.at_risk, r.exhausts_at), ("lasts", false, None));
        assert_eq!(message(&r, UtcOffset::UTC), None);
    }

    #[test]
    fn staggered_resets_add_capacity() {
        let now = MON;
        let rate = |per_hour: f64| {
            Some(Rate {
                per_sec: per_hour / 3600.0,
                span: DAY,
            })
        };
        // a: 0.1 left, resets in 10 h; b: 0.2 left, resets in 4 days. At 0.05/h, a's 0.1 goes
        // first (2 h), then b's 0.2 (4 h more): out at 6 h, 4 h before a resets.
        let pool = [
            acct("a", 0.9, Some(now + 10 * H)),
            acct("b", 0.8, Some(now + 4 * DAY)),
        ];
        let r = forecast(&pool, rate(0.05), now, 6);
        assert_eq!(r.state, "short");
        assert_eq!(r.exhausts_at.unwrap().unix_timestamp(), now + 6 * H);
        assert_eq!(r.next_reset.unwrap().unix_timestamp(), now + 10 * H);
        assert_eq!(r.short_by_secs, Some(4 * H));
        assert!(!r.at_risk, "4 h short is under the 6 h threshold");
        // At 0.02/h: by a's reset 0.2 of the 0.3 is used (a's 0.1 first, then 0.1 of b); a comes
        // back full, and the 1.1 left lasts 55 h more: out at 65 h, 31 h before b's reset (96 h).
        let r = forecast(&pool, rate(0.02), now, 6);
        assert_eq!(r.exhausts_at.unwrap().unix_timestamp(), now + 65 * H);
        assert_eq!(r.next_reset.unwrap().unix_timestamp(), now + 4 * DAY);
        assert_eq!(r.short_by_secs, Some(31 * H));
        assert!(r.at_risk);
        assert_eq!(r.cycle, Some(now + 10 * H));
        // At 0.01/h: a's reset at 10 h and b's at 96 h keep the pool going for two weeks.
        let r = forecast(&pool, rate(0.01), now, 6);
        assert_eq!(r.state, "lasts");
    }

    #[test]
    fn spent_and_unusable_accounts() {
        let now = MON;
        let rate = Some(Rate {
            per_sec: 0.05 / 3600.0,
            span: DAY,
        });
        // Every usable account spent: out now, until the first reset; a dead account with
        // headroom does not count.
        let mut dead = acct("dead", 0.0, Some(now + H));
        dead.usable = false;
        let pool = [
            acct("a", 1.0, Some(now + 20 * H)),
            acct("b", 1.0, Some(now + 30 * H)),
            dead,
        ];
        let r = forecast(&pool, rate, now, 6);
        assert_eq!(r.state, "out");
        assert_eq!(r.exhausts_at.unwrap().unix_timestamp(), now);
        assert_eq!(r.short_by_secs, Some(20 * H));
        assert!(r.at_risk);
        assert_eq!(
            r.cycle,
            Some(now + 20 * H),
            "the dead account's reset is not a cycle"
        );
        assert_eq!(
            message(&r, UtcOffset::UTC).unwrap(),
            "The pool is out of 7-day quota until the next reset (Mon 20:00)"
        );
        // Out is known even without a rate.
        assert_eq!(forecast(&pool, None, now, 6).state, "out");
        // A spent account whose reset already passed is fresh again.
        let pool = [acct("a", 1.0, Some(now - H))];
        let r = forecast(&pool, rate, now, 6);
        assert!(close(r.headroom, 1.0));
        assert_eq!(r.state, "short", "1.0 at 0.05/h is out in 20 h");
        assert_eq!(r.next_reset.unwrap().unix_timestamp(), now - H + WEEK);
        // Spent with no reset known: out, at risk, no reset to name.
        let r = forecast(&[acct("a", 1.0, None)], rate, now, 6);
        assert_eq!((r.state, r.short_by_secs, r.at_risk), ("out", None, true));
        assert_eq!(
            message(&r, UtcOffset::UTC).unwrap(),
            "The pool is out of 7-day quota (no reset known)"
        );
    }

    #[test]
    fn no_data() {
        let now = MON;
        // No rate yet: unknown (but the headroom is shown).
        let r = forecast(&[acct("a", 0.3, Some(now + DAY))], None, now, 6);
        assert_eq!((r.state, r.at_risk), ("unknown", false));
        assert!(close(r.headroom, 0.7));
        // No usable account with a known window: unknown, whatever the rate.
        let rate = Some(Rate {
            per_sec: 1.0,
            span: DAY,
        });
        let unknown = AccountNow {
            id: "a".into(),
            usable: true,
            used: None,
            reset: None,
        };
        assert_eq!(forecast(&[unknown], rate, now, 6).state, "unknown");
        assert_eq!(forecast(&[], rate, now, 6).state, "unknown");
        let json = serde_json::to_value(forecast(&[], None, now, 6)).unwrap();
        assert!(
            json["exhausts_at"].is_null() && json.get("cycle").is_none(),
            "{json}"
        );
    }

    #[test]
    fn spans_and_times() {
        assert_eq!(fmt_span(59 * 60), "<1h");
        assert_eq!(fmt_span(14 * H + 20 * 60), "~14h");
        assert_eq!(fmt_span(47 * H), "~47h");
        assert_eq!(fmt_span(3 * DAY + H), "~3 days");
        let t = OffsetDateTime::from_unix_timestamp(MON + 3 * DAY + 9 * H).unwrap();
        assert_eq!(fmt_when(t, UtcOffset::UTC), "Thu 09:00");
        assert_eq!(
            fmt_when(t, UtcOffset::from_hms(-10, 0, 0).unwrap()),
            "Wed 23:00"
        );
    }
}
