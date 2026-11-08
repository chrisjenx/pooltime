//! Weekly subscription burn (spec §5): per local ISO week, the sum over accounts of the 7-day
//! window utilization consumed in that week (1 account = 1 sub).
//!
//! Per account, consumption is the rise of the window's high-water mark between consecutive
//! samples (dips never count); a window reset starts a new segment whose value counts from 0 (see
//! [`crate::store::usage_history`]). A delta whose two samples straddle a week boundary is split
//! linearly by time. The current week is extrapolated linearly from its elapsed fraction
//! (`projected = so_far / elapsed`), but only once an hour has passed (`null` before). Accounts
//! that consumed less than [`IDLE_BELOW`] this week are "idle".
//!
//! Accepted limitations:
//! - An account's **first sample consumes nothing**: the utilization it already shows was used
//!   at an unknown time before sampling began.
//! - **DST**: weeks are cut at the machine's *current* UTC offset ([`super::local_offset`]), also
//!   for historical weeks. Across a DST change a week boundary moves by an hour, so a delta near a
//!   boundary may land in the neighbouring week; buckets are assigned by their midpoint, so a
//!   downsampled bucket never changes week.
//! - Use in the windows between two resets that both fall inside one gap between samples is
//!   lost (unknowable).

use std::collections::{BTreeMap, BTreeSet, HashMap};

use serde::{Serialize, Serializer};
use time::{OffsetDateTime, UtcOffset};

use crate::store::usage_history::{self, step, week_start, HistRow, WEEK, WINDOW_7D};
use crate::store::Store;

pub const DEFAULT_WEEKS: u32 = 12;
pub const MAX_WEEKS: u32 = 104;
/// An account that consumed less than this (fraction of one sub) this week is idle.
pub const IDLE_BELOW: f64 = 0.10;
/// No projection until this much of the week has passed (division by a tiny fraction).
pub const MIN_ELAPSED_SECS: i64 = 3600;

fn round4<S: Serializer>(x: &f64, s: S) -> Result<S::Ok, S::Error> {
    s.serialize_f64((x * 10_000.0).round() / 10_000.0)
}

fn round4_opt<S: Serializer>(x: &Option<f64>, s: S) -> Result<S::Ok, S::Error> {
    match x {
        Some(x) => round4(x, s),
        None => s.serialize_none(),
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct AccountBurn {
    pub account: String,
    #[serde(serialize_with = "round4")]
    pub consumed: f64,
    /// The account's plan (e.g. `max`) when it is already on file.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub plan: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct WeekBurn {
    /// ISO week label, e.g. `2026-W45`.
    pub week: String,
    /// Local date of the week's Monday, `YYYY-MM-DD`.
    pub start: String,
    #[serde(serialize_with = "round4")]
    pub subs_used: f64,
    /// Accounts that consumed anything this week, by id.
    pub per_account: Vec<AccountBurn>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Forecast {
    pub week: String,
    #[serde(serialize_with = "round4")]
    pub subs_used_so_far: f64,
    #[serde(serialize_with = "round4")]
    pub elapsed_fraction: f64,
    /// `subs_used_so_far / elapsed_fraction`; `None` in the week's first hour.
    #[serde(serialize_with = "round4_opt")]
    pub projected: Option<f64>,
    /// Pool size in subs (the "of N").
    pub accounts: usize,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct BurnReport {
    /// Oldest first; the last entry is the current (partial) week.
    pub weeks: Vec<WeekBurn>,
    pub forecast: Forecast,
    pub idle_accounts: Vec<String>,
    /// Current-week consumption per account seen in the history.
    #[serde(skip)]
    this_week: BTreeMap<String, f64>,
}

/// `2026-W45` and `2026-11-02` for the week starting at unix `start`.
fn labels(start: i64, offset: UtcOffset) -> (String, String) {
    let date = OffsetDateTime::from_unix_timestamp(start)
        .map(|t| t.to_offset(offset).date())
        .unwrap_or(time::Date::MIN);
    let (iso_year, w, _) = date.to_iso_week_date();
    (
        format!("{iso_year}-W{w:02}"),
        format!(
            "{:04}-{:02}-{:02}",
            date.year(),
            u8::from(date.month()),
            date.day()
        ),
    )
}

/// Total the history `rows` (sorted by account, then time, as [`usage_history::rows_since`]
/// returns them) into the last `weeks` local ISO weeks ending with the one holding `now`.
pub fn compute(rows: &[HistRow], now: i64, offset: UtcOffset, weeks: u32) -> BurnReport {
    let weeks = weeks.clamp(1, MAX_WEEKS);
    let off = i64::from(offset.whole_seconds());
    let current = week_start(now, off);
    let first = current - i64::from(weeks - 1) * WEEK;

    // account -> week start -> consumed
    let mut per: BTreeMap<String, HashMap<i64, f64>> = BTreeMap::new();
    // (account, last reading, high-water mark after it)
    let mut prev: Option<(&str, usage_history::Point, f64)> = None;
    for r in rows {
        let acc = per.entry(r.account.clone()).or_default();
        let mut add = |ws: i64, c: f64| *acc.entry(ws).or_default() += c;
        let h = match prev.filter(|(pa, _, _)| *pa == r.account) {
            Some((_, last, hw)) => {
                let s = step(&last, hw, &r.first);
                let (c, mut t0, t1) = (s.consumed, s.from, r.first.ts);
                if c > 0.0 {
                    if t1 <= t0 {
                        add(week_start(t1, off), c);
                    } else {
                        // Split linearly by time across the week boundaries in [t0, t1].
                        let span = (t1 - t0) as f64;
                        while t0 < t1 {
                            let ws = week_start(t0, off);
                            let end = (ws + WEEK).min(t1);
                            add(ws, c * (end - t0) as f64 / span);
                            t0 = end;
                        }
                    }
                }
                s.hw
            }
            // The first reading of an account consumes nothing (when it was used is unknown).
            None => r.first.util,
        };
        let (inner, end_hw) = r.inner(h);
        if inner > 0.0 {
            add(week_start(r.anchor(), off), inner);
        }
        prev = Some((r.account.as_str(), r.last, end_hw));
    }

    let weeks: Vec<WeekBurn> = (0..i64::from(weeks))
        .map(|i| {
            let ws = first + i * WEEK;
            let per_account: Vec<AccountBurn> = per
                .iter()
                .filter_map(|(a, m)| {
                    m.get(&ws).filter(|c| **c > 1e-12).map(|c| AccountBurn {
                        account: a.clone(),
                        consumed: *c,
                        plan: None,
                    })
                })
                .collect();
            let (week, start) = labels(ws, offset);
            WeekBurn {
                week,
                start,
                subs_used: per_account.iter().map(|a| a.consumed).sum(),
                per_account,
            }
        })
        .collect();

    let this_week: BTreeMap<String, f64> = per
        .iter()
        .map(|(a, m)| (a.clone(), m.get(&current).copied().unwrap_or(0.0)))
        .collect();
    let so_far = weeks.last().map_or(0.0, |w| w.subs_used);
    let elapsed = (now - current).clamp(0, WEEK);
    let elapsed_fraction = elapsed as f64 / WEEK as f64;
    let forecast = Forecast {
        week: labels(current, offset).0,
        subs_used_so_far: so_far,
        elapsed_fraction,
        projected: (elapsed >= MIN_ELAPSED_SECS).then(|| so_far / elapsed_fraction),
        accounts: this_week.len(),
    };
    let idle_accounts = this_week
        .iter()
        .filter(|(_, c)| **c < IDLE_BELOW)
        .map(|(a, _)| a.clone())
        .collect();
    BurnReport {
        weeks,
        forecast,
        idle_accounts,
        this_week,
    }
}

/// The burn report over the store's 7-day history, scoped to the configured accounts `pool`
/// (`(id, plan)`, from [`super::pool_of`]; see [`BurnReport::with_pool`]). A blocking read: call
/// off the async threads.
pub fn report(
    store: &Store,
    now: OffsetDateTime,
    offset: UtcOffset,
    weeks: u32,
    pool: &[(String, Option<String>)],
) -> anyhow::Result<BurnReport> {
    let weeks = weeks.clamp(1, MAX_WEEKS);
    let now = now.unix_timestamp();
    let off = i64::from(offset.whole_seconds());
    let since = week_start(now, off) - i64::from(weeks - 1) * WEEK;
    let rows = store.read(|c| usage_history::rows_since(c, WINDOW_7D, since))?;
    Ok(compute(&rows, now, offset, weeks).with_pool(pool))
}

impl BurnReport {
    /// Scope the pool-level numbers to the configured accounts `(id, plan)`: the forecast's "of N"
    /// is the pool size, idle accounts are pool accounts (an account never sampled counts as
    /// idle; a removed one is not listed), and known plans are attached. Past weeks keep removed
    /// accounts' consumption.
    pub fn with_pool(mut self, pool: &[(String, Option<String>)]) -> BurnReport {
        let plans: HashMap<&str, &str> = pool
            .iter()
            .filter_map(|(id, plan)| plan.as_deref().map(|p| (id.as_str(), p)))
            .collect();
        for w in &mut self.weeks {
            for a in &mut w.per_account {
                a.plan = plans.get(a.account.as_str()).map(|p| p.to_string());
            }
        }
        let ids: BTreeSet<&str> = pool.iter().map(|(id, _)| id.as_str()).collect();
        self.idle_accounts = ids
            .iter()
            .filter(|id| self.this_week.get(**id).copied().unwrap_or(0.0) < IDLE_BELOW)
            .map(|id| id.to_string())
            .collect();
        self.forecast.accounts = ids.len();
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::RetentionCfg;
    use crate::store::usage_history::{insert_sample, Point, DAY};
    use std::sync::Arc;

    /// 2026-11-09T00:00:00Z, a Monday.
    const MON: i64 = 1_794_182_400;
    const H: i64 = 3600;

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

    fn sorted(mut rows: Vec<HistRow>) -> Vec<HistRow> {
        rows.sort_by(|a, b| a.account.cmp(&b.account).then(a.ts.cmp(&b.ts)));
        rows
    }

    fn of(w: &WeekBurn, account: &str) -> f64 {
        w.per_account
            .iter()
            .find(|a| a.account == account)
            .map_or(0.0, |a| a.consumed)
    }

    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-9
    }

    #[test]
    fn burn_sums_deltas_across_reset() {
        let reset1 = MON + 3 * DAY; // Thursday 00:00
        let reset2 = reset1 + WEEK;
        let rows = sorted(vec![
            // a: 0.1 -> 0.4 -> 0.9, window resets Thursday, then 0.2 -> 0.5 (+ a rounding dip)
            raw("a", MON + H, 0.1, reset1),
            raw("a", MON + DAY, 0.4, reset1),
            raw("a", MON + 2 * DAY, 0.9, reset1),
            raw("a", MON + 3 * DAY + 6 * H, 0.2, reset2),
            raw("a", MON + 4 * DAY, 0.5, reset2),
            raw("a", MON + 4 * DAY + H, 0.49, reset2 + 30),
            // b: 0 on Sunday noon (previous week), 0.24 on Monday noon: split half/half
            raw("b", MON - 12 * H, 0.0, reset1),
            raw("b", MON + 12 * H, 0.24, reset1),
        ]);
        let now = MON + 5 * DAY;
        let r = compute(&rows, now, UtcOffset::UTC, 2);
        assert_eq!(r.weeks.len(), 2);
        let (prev, cur) = (&r.weeks[0], &r.weeks[1]);
        assert_eq!(prev.week, "2026-W45");
        assert_eq!(prev.start, "2026-11-02");
        assert_eq!(cur.week, "2026-W46");
        assert_eq!(cur.start, "2026-11-09");
        // 0.3 + 0.5 + 0.2 (post-reset value counts from 0) + 0.3
        assert!(close(of(cur, "a"), 1.3), "{cur:?}");
        assert!(close(of(cur, "b"), 0.12), "{cur:?}");
        assert!(close(of(prev, "b"), 0.12), "{prev:?}");
        assert!(close(of(prev, "a"), 0.0), "{prev:?}");
        assert!(close(cur.subs_used, 1.42), "{cur:?}");
        assert!(close(prev.subs_used, 0.12), "{prev:?}");
        assert_eq!(
            prev.per_account
                .iter()
                .map(|a| a.account.as_str())
                .collect::<Vec<_>>(),
            ["b"],
            "accounts that consumed nothing are not listed"
        );

        // In UTC-5 the same Monday-noon-UTC sample falls on local Monday 07:00 and the Sunday
        // sample on local Sunday 07:00: the split moves with the local week boundary.
        let r = compute(&rows, now, UtcOffset::from_hms(-5, 0, 0).unwrap(), 2);
        let cur = r.weeks.last().unwrap();
        // boundary at MON + 5h: 7h of the 24h interval are in the new week
        assert!(close(of(cur, "b"), 0.24 * 7.0 / 24.0), "{cur:?}");
    }

    fn this_week(rows: Vec<HistRow>, account: &str) -> f64 {
        let r = compute(&sorted(rows), MON + 5 * DAY, UtcOffset::UTC, 1);
        of(r.weeks.last().unwrap(), account)
    }

    #[test]
    fn jitter_never_inflates_consumption() {
        let r = MON + 6 * DAY;
        // dip and recovery: 0.42 -> 0.41 -> 0.42 consumes nothing
        let dip = vec![
            raw("a", MON + H, 0.42, r),
            raw("a", MON + 2 * H, 0.41, r),
            raw("a", MON + 3 * H, 0.42, r),
        ];
        assert!(close(this_week(dip, "a"), 0.0));
        // a 3-point drop with the same resets_at is not a reset (and lowers no baseline)
        let drop = vec![
            raw("a", MON + H, 0.82, r),
            raw("a", MON + 2 * H, 0.79, r),
            raw("a", MON + 3 * H, 0.83, r),
        ];
        assert!(close(this_week(drop, "a"), 0.01), "high-water 0.82 -> 0.83");
        // without resets_at, a large drop still reads as a reset
        let none = |ts, util| {
            HistRow::raw(
                "a",
                WINDOW_7D,
                Point {
                    ts,
                    util,
                    reset: None,
                },
            )
        };
        let drop = vec![none(MON + H, 0.82), none(MON + 2 * H, 0.10)];
        assert!(close(this_week(drop, "a"), 0.10));
    }

    #[test]
    fn resets_at_move_without_drop_counts_from_zero() {
        let rows = vec![
            raw("a", MON + H, 0.3, MON + 2 * DAY),
            raw("a", MON + 3 * DAY, 0.4, MON + 9 * DAY),
        ];
        assert!(close(this_week(rows, "a"), 0.4));
    }

    #[test]
    fn two_resets_in_one_gap_accrue_only_in_the_last_window() {
        // 0.5 two weeks back (resets at MON - 7d); next reading on Wednesday after two resets
        // (resets_at now MON + 7d, so the current window started MON). The intermediate window's
        // use is unknowable and lost; the 0.3 accrues from MON, not from two weeks back.
        let rows = sorted(vec![
            raw("a", MON - 10 * DAY, 0.5, MON - 7 * DAY),
            raw("a", MON + 2 * DAY, 0.3, MON + 7 * DAY),
        ]);
        let r = compute(&rows, MON + 5 * DAY, UtcOffset::UTC, 3);
        assert!(close(of(&r.weeks[2], "a"), 0.3), "{r:?}");
        assert!(close(of(&r.weeks[1], "a"), 0.0), "{r:?}");
    }

    #[test]
    fn forecast_linear_from_elapsed() {
        let rows = sorted(vec![
            raw("a", MON + H, 0.0, MON + 5 * DAY),
            raw("a", MON + DAY, 0.6, MON + 5 * DAY),
            raw("b", MON + H, 0.1, MON + 6 * DAY),
            raw("b", MON + DAY, 0.5, MON + 6 * DAY),
        ]);
        let r = compute(&rows, MON + 2 * DAY, UtcOffset::UTC, 4);
        let f = &r.forecast;
        assert_eq!(f.week, "2026-W46");
        assert!(close(f.subs_used_so_far, 1.0), "{f:?}");
        assert!(close(f.elapsed_fraction, 2.0 / 7.0), "{f:?}");
        assert!(close(f.projected.unwrap(), 3.5), "{f:?}");
        assert_eq!(f.accounts, 2);
        assert_eq!(r.weeks.len(), 4);

        // Within the week's first hour there is no projection.
        let early = compute(&[], MON + 30 * 60, UtcOffset::UTC, 1);
        assert_eq!(early.forecast.projected, None);
        assert!(close(early.forecast.elapsed_fraction, 1800.0 / WEEK as f64));
        let json = serde_json::to_value(&early).unwrap();
        assert!(json["forecast"]["projected"].is_null(), "{json}");
    }

    #[test]
    fn idle_accounts_threshold() {
        let rows = sorted(vec![
            raw("a", MON + H, 0.0, MON + 5 * DAY),
            raw("a", MON + DAY, 0.09, MON + 5 * DAY),
            raw("b", MON + H, 0.0, MON + 5 * DAY),
            raw("b", MON + DAY, 0.10, MON + 5 * DAY),
            // removed from the pool: consumed last week only
            raw("gone", MON - 3 * DAY, 0.0, MON + 5 * DAY),
            raw("gone", MON - 2 * DAY, 0.5, MON + 5 * DAY),
        ]);
        let r = compute(&rows, MON + 3 * DAY, UtcOffset::UTC, 2);
        assert_eq!(r.idle_accounts, ["a", "gone"]);

        let pool = [
            ("a".to_string(), Some("max".to_string())),
            ("b".to_string(), None),
            ("never-sampled".to_string(), Some("pro".to_string())),
        ];
        let r = r.with_pool(&pool);
        assert_eq!(r.idle_accounts, ["a", "never-sampled"]);
        assert_eq!(r.forecast.accounts, 3);
        let cur = r.weeks.last().unwrap();
        let a = cur.per_account.iter().find(|x| x.account == "a").unwrap();
        assert_eq!(a.plan.as_deref(), Some("max"));
        let b = cur.per_account.iter().find(|x| x.account == "b").unwrap();
        assert_eq!(b.plan, None);
        // past weeks keep a removed account's consumption
        assert!(close(of(&r.weeks[0], "gone"), 0.5));
        let json = serde_json::to_value(&r).unwrap();
        assert!(
            json["weeks"][1]["per_account"][1].get("plan").is_none(),
            "{json}"
        );
    }

    /// Deterministic 7-day histories for two accounts over ~400 days, sampled every 6 hours:
    /// steady use, window resets at each account's own phase, rounding dips.
    fn synthetic(now: i64) -> Vec<(String, Point)> {
        let mut out = Vec::new();
        for (k, account) in ["alpha", "beta"].iter().enumerate() {
            let phase = 37 * H + k as i64 * 3 * DAY + 1234;
            let mut reset = now - 401 * DAY + phase;
            let mut util: f64 = 0.0;
            let mut seed: u64 = 17 + k as u64;
            let mut ts = now - 400 * DAY + k as i64 * 977;
            while ts < now {
                seed = seed.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
                let r = (seed >> 33) as f64 / (1u64 << 31) as f64; // 0..1
                if ts >= reset {
                    while reset <= ts {
                        reset += WEEK;
                    }
                    util = 0.0;
                }
                if r < 0.1 {
                    util = (util - 0.015 * (1.0 + r * 10.0)).max(0.0); // jitter dip (0.015..0.03)
                } else {
                    util = (util + r * 0.03).min(1.0);
                }
                out.push((
                    account.to_string(),
                    Point {
                        ts,
                        util,
                        // now and then a reading without resets_at
                        reset: (!seed.is_multiple_of(9)).then_some(reset),
                    },
                ));
                ts += 6 * H + (seed % 600) as i64;
            }
        }
        out
    }

    #[tokio::test]
    async fn downsampled_history_still_yields_weekly_totals() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::open(&dir.path().join("pooltime.db")).unwrap());
        let offset = UtcOffset::from_hms(8, 0, 0).unwrap();
        let now_ts = MON + 2 * DAY + 5 * H + 17;
        let now = OffsetDateTime::from_unix_timestamp(now_ts).unwrap();
        let samples = synthetic(now_ts);
        store
            .exec(move |c| {
                let tx = c.transaction()?;
                for (a, p) in &samples {
                    insert_sample(&tx, a, WINDOW_7D, p)?;
                }
                tx.commit()?;
                Ok(())
            })
            .await
            .unwrap();
        let weeks = 60;
        let pool = [("alpha".to_string(), None), ("beta".to_string(), None)];
        let before = report(&store, now, offset, weeks, &pool).unwrap();
        assert!(before.weeks.iter().filter(|w| w.subs_used > 0.5).count() > 50);

        // Retention at the local offset: raw > 30 d -> daily, daily > 1 y -> weekly.
        let cfg = RetentionCfg::default();
        crate::store::prune(&store, &cfg, now.to_offset(offset))
            .await
            .unwrap();
        // A later run (more raw rows roll up next to existing buckets).
        let rep = crate::store::prune(
            &store,
            &cfg,
            (now + time::Duration::days(10)).to_offset(offset),
        )
        .await
        .unwrap();
        assert!(rep.usage_hourly_rolled_up > 0, "{rep:?}");
        let tiers: Vec<(String, i64)> = store
            .read(|c| {
                let mut st = c.prepare(
                    "SELECT granularity, count(*) FROM usage_samples GROUP BY granularity ORDER BY 1",
                )?;
                let r = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
                Ok(r.collect::<Result<_, _>>()?)
            })
            .unwrap();
        let tier_names: Vec<&str> = tiers.iter().map(|t| t.0.as_str()).collect();
        assert_eq!(tier_names, ["daily", "hourly", "weekly"], "{tiers:?}");

        let after = report(&store, now, offset, weeks, &pool).unwrap();
        assert_eq!(before.weeks.len(), after.weeks.len());
        for (b, a) in before.weeks.iter().zip(&after.weeks) {
            assert_eq!(b.week, a.week);
            assert!(
                (b.subs_used - a.subs_used).abs() < 1e-9,
                "{}: raw {} vs downsampled {}",
                b.week,
                b.subs_used,
                a.subs_used
            );
            for acct in ["alpha", "beta"] {
                assert!(
                    (of(b, acct) - of(a, acct)).abs() < 1e-9,
                    "{} {acct}",
                    b.week
                );
            }
        }
        assert_eq!(before.forecast, after.forecast);
    }
}
