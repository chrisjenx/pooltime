//! Row-level API for `usage_samples`: each account's 7-day window utilization over time (spec §5),
//! and the consumption math shared by the burn report (`analytics::burn`) and the retention
//! rollups (`store::retention`).
//!
//! # Rows
//! A **raw** row (`granularity = 'hourly'`, kept 30 days) is one sample: `ts`, `utilization`
//! (0-1) and `reset` (the window's `resets_at`, unix seconds). A **bucket** row (`daily`, then
//! `weekly`) stands for every sample of one account in one local day / ISO week: `ts` is the
//! bucket start, `consumed` the consumption *between* those samples, `first_*` its first sample
//! and `last_ts` + `utilization`/`reset` its last. Consumption between two rows is computed from
//! the earlier row's last sample and the later row's first, so totals do not depend on which
//! tier the history currently sits in.
//!
//! # Consumption
//! Between consecutive samples `a -> b`: `max(0, b - a)`, unless the window reset in between
//! (utilization dropped by more than [`RESET_DROP`], or `resets_at` moved forward by at least
//! [`RESET_ADVANCE_SECS`]); then the post-reset value counted from 0, i.e. `b`.

use std::collections::HashMap;

use anyhow::Context;
use rusqlite::{params, Connection, OptionalExtension, Row};

/// The `window` value of the account-wide 7-day window samples.
pub const WINDOW_7D: &str = "7d";
/// A utilization drop larger than this is a window reset (smaller dips are rounding noise between
/// header and poll readings: whole percents vs fractions).
pub const RESET_DROP: f64 = 0.02;
/// `resets_at` moving forward by at least this is a window reset (it jitters by seconds otherwise).
pub const RESET_ADVANCE_SECS: i64 = 3600;

pub const DAY: i64 = 86_400;
pub const WEEK: i64 = 7 * DAY;

/// One utilization reading.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Point {
    /// Unix seconds.
    pub ts: i64,
    /// 0-1 fraction.
    pub util: f64,
    /// The window's `resets_at`, unix seconds.
    pub reset: Option<i64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Granularity {
    /// A raw sample (stored as `hourly`, spec §6's 30-day tier).
    Raw,
    Daily,
    Weekly,
}

impl Granularity {
    pub fn as_str(self) -> &'static str {
        match self {
            Granularity::Raw => "hourly",
            Granularity::Daily => "daily",
            Granularity::Weekly => "weekly",
        }
    }

    fn parse(s: &str) -> Option<Granularity> {
        match s {
            "hourly" => Some(Granularity::Raw),
            "daily" => Some(Granularity::Daily),
            "weekly" => Some(Granularity::Weekly),
            _ => None,
        }
    }
}

/// One `usage_samples` row (a raw sample has `first == last` and `consumed == 0`).
#[derive(Debug, Clone, PartialEq)]
pub struct HistRow {
    pub account: String,
    pub window: String,
    pub gran: Granularity,
    /// Sample time (raw) or bucket start.
    pub ts: i64,
    pub consumed: f64,
    pub first: Point,
    pub last: Point,
}

impl HistRow {
    pub fn raw(account: &str, window: &str, p: Point) -> HistRow {
        HistRow {
            account: account.into(),
            window: window.into(),
            gran: Granularity::Raw,
            ts: p.ts,
            consumed: 0.0,
            first: p,
            last: p,
        }
    }

    /// A time inside the bucket (between its first and last sample).
    pub fn anchor(&self) -> i64 {
        self.first.ts + (self.last.ts - self.first.ts) / 2
    }
}

/// Whether the window reset between `a` and the later reading `b`.
pub fn is_reset(a: &Point, b: &Point) -> bool {
    b.util < a.util - RESET_DROP
        || matches!((a.reset, b.reset), (Some(ra), Some(rb)) if rb >= ra + RESET_ADVANCE_SECS)
}

/// Consumption between consecutive readings `a -> b`, and the time it started accruing: `a.ts`,
/// or after a reset the old `resets_at` when that falls between the two readings.
pub fn step(a: &Point, b: &Point) -> (f64, i64) {
    if is_reset(a, b) {
        let from = a.reset.filter(|r| *r > a.ts && *r < b.ts).unwrap_or(a.ts);
        (b.util.max(0.0), from)
    } else {
        ((b.util - a.util).max(0.0), a.ts)
    }
}

/// Fold time-ordered rows of one account into one bucket's `(consumed, first, last)`.
pub fn fold(rows: &[HistRow]) -> Option<(f64, Point, Point)> {
    let first = rows.first()?;
    let mut consumed = first.consumed;
    let mut last = first.last;
    for r in &rows[1..] {
        consumed += step(&last, &r.first).0 + r.consumed;
        last = r.last;
    }
    Some((consumed, first.first, last))
}

/// Start (unix seconds) of the local day holding `ts`, at UTC offset `off` seconds.
pub fn day_start(ts: i64, off: i64) -> i64 {
    (ts + off).div_euclid(DAY) * DAY - off
}

/// Start (unix seconds) of the local ISO week (Monday 00:00) holding `ts`, at UTC offset `off`.
pub fn week_start(ts: i64, off: i64) -> i64 {
    let day = (ts + off).div_euclid(DAY);
    // Epoch day 0 was a Thursday, so (day + 3) % 7 == 0 on Mondays.
    (day - (day + 3).rem_euclid(7)) * DAY - off
}

/// Queue-free insert of one raw sample (call on the store writer).
pub fn insert_sample(
    c: &Connection,
    account: &str,
    window: &str,
    p: &Point,
) -> rusqlite::Result<()> {
    c.execute(
        "INSERT INTO usage_samples(ts, account, window, utilization, reset, granularity)
         VALUES (?1, ?2, ?3, ?4, ?5, 'hourly')",
        params![p.ts, account, window, p.util, p.reset],
    )?;
    Ok(())
}

/// Insert one bucket row.
pub fn insert_bucket(c: &Connection, r: &HistRow) -> rusqlite::Result<()> {
    c.execute(
        "INSERT INTO usage_samples(ts, account, window, utilization, reset, granularity,
                                   consumed, first_ts, first_util, first_reset, last_ts)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
        params![
            r.ts,
            r.account,
            r.window,
            r.last.util,
            r.last.reset,
            r.gran.as_str(),
            r.consumed,
            r.first.ts,
            r.first.util,
            r.first.reset,
            r.last.ts
        ],
    )?;
    Ok(())
}

pub(crate) const COLUMNS: &str = "rowid, account, window, granularity, ts, utilization, reset, \
     consumed, first_ts, first_util, first_reset, last_ts";

/// Parse a row selected with [`COLUMNS`] into `(rowid, row)`; `None` for an unknown granularity.
/// Missing bucket columns (pre-v4 rows) fall back to the row's own `ts`/value.
pub(crate) fn from_row(r: &Row<'_>) -> rusqlite::Result<(i64, Option<HistRow>)> {
    let rowid: i64 = r.get(0)?;
    let gran: String = r.get(3)?;
    let Some(gran) = Granularity::parse(&gran) else {
        return Ok((rowid, None));
    };
    let ts: i64 = r.get(4)?;
    let util: f64 = r.get::<_, Option<f64>>(5)?.unwrap_or(0.0);
    let reset: Option<i64> = r.get(6)?;
    let last = Point {
        ts: r.get::<_, Option<i64>>(11)?.unwrap_or(ts),
        util,
        reset,
    };
    let first = match r.get::<_, Option<i64>>(8)? {
        Some(fts) => Point {
            ts: fts,
            util: r.get::<_, Option<f64>>(9)?.unwrap_or(util),
            reset: r.get(10)?,
        },
        None => last,
    };
    Ok((
        rowid,
        Some(HistRow {
            account: r.get(1)?,
            window: r.get(2)?,
            gran,
            ts,
            consumed: r.get::<_, Option<f64>>(7)?.unwrap_or(0.0),
            first,
            last,
        }),
    ))
}

/// Every `window` row an account needs to total burn from `since` on: the rows ending at or after
/// `since`, plus each account's latest row before them (the left end of a delta straddling
/// `since`). Ordered by account, then time.
pub fn rows_since(c: &Connection, window: &str, since: i64) -> anyhow::Result<Vec<HistRow>> {
    // A bucket starts at most a week before its last sample.
    let from = since - WEEK;
    let sql = format!(
        "SELECT {COLUMNS} FROM usage_samples u
         WHERE window = ?1 AND (ts >= ?2 OR rowid = (
             SELECT p.rowid FROM usage_samples p
             WHERE p.window = ?1 AND p.account = u.account AND p.ts < ?2
             ORDER BY p.ts DESC, p.rowid DESC LIMIT 1))"
    );
    let mut st = c.prepare(&sql).context("prepare usage history read")?;
    let mut rows: Vec<HistRow> = st
        .query_map(params![window, from], from_row)?
        .filter_map(|r| r.map(|(_, row)| row).transpose())
        .collect::<Result<_, _>>()?;
    rows.sort_by(|a, b| {
        a.account
            .cmp(&b.account)
            .then(a.first.ts.cmp(&b.first.ts))
            .then(a.last.ts.cmp(&b.last.ts))
    });
    Ok(rows)
}

/// The latest recorded reading of `window` per account (seeds the sampler after a restart).
pub fn latest_points(c: &Connection, window: &str) -> anyhow::Result<HashMap<String, Point>> {
    let mut st = c.prepare("SELECT DISTINCT account FROM usage_samples WHERE window = ?1")?;
    let accounts: Vec<String> = st
        .query_map([window], |r| r.get(0))?
        .collect::<Result<_, _>>()?;
    let sql = format!(
        "SELECT {COLUMNS} FROM usage_samples WHERE window = ?1 AND account = ?2
         ORDER BY COALESCE(last_ts, ts) DESC, rowid DESC LIMIT 1"
    );
    let mut out = HashMap::new();
    for a in accounts {
        let row = c
            .query_row(&sql, params![window, a], from_row)
            .optional()?
            .and_then(|(_, r)| r);
        if let Some(r) = row {
            out.insert(a, r.last);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(ts: i64, util: f64, reset: Option<i64>) -> Point {
        Point { ts, util, reset }
    }

    #[test]
    fn step_rules() {
        // plain increase
        assert_eq!(
            step(&p(0, 0.2, Some(100)), &p(10, 0.5, Some(100))).0,
            0.5 - 0.2
        );
        // rounding dip: no consumption, no reset
        assert_eq!(step(&p(0, 0.42, None), &p(10, 0.41, None)), (0.0, 0));
        // drop = reset: the new value counts from 0, from the old reset time when in between
        assert_eq!(
            step(&p(0, 0.9, Some(5)), &p(10, 0.1, Some(5 + WEEK))),
            (0.1, 5)
        );
        // reset moved forward while utilization rose
        assert_eq!(
            step(&p(0, 0.3, Some(50)), &p(100, 0.4, Some(50 + WEEK))),
            (0.4, 50)
        );
        // reset jitter is not a reset
        assert!(!is_reset(&p(0, 0.3, Some(50)), &p(100, 0.4, Some(80))));
    }

    #[test]
    fn week_and_day_starts() {
        // 2026-11-09 is a Monday. 1_794_182_400 = 2026-11-09T00:00:00Z.
        let monday = 1_794_182_400;
        assert_eq!(week_start(monday + 3 * DAY + 5, 0), monday);
        assert_eq!(week_start(monday - 1, 0), monday - WEEK);
        // UTC+8: local Monday 00:00 is Sunday 16:00 UTC.
        let off = 8 * 3600;
        assert_eq!(week_start(monday - 7 * 3600, off), monday - off);
        assert_eq!(week_start(monday - 9 * 3600, off), monday - off - WEEK);
        assert_eq!(day_start(monday + 3600, -5 * 3600), monday - DAY + 5 * 3600);
    }
}
