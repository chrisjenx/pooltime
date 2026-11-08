//! Row-level API for `usage_samples`: each account's 7-day window utilization over time (spec §5),
//! and the consumption math shared by the burn report (`analytics::burn`) and the retention
//! rollups (`store::retention`).
//!
//! # Rows
//! A **raw** row (`granularity = 'hourly'`, kept 30 days) is one sample: `ts`, `utilization`
//! (0-1) and `reset` (the window's `resets_at`, unix seconds). A **bucket** row (`daily`, then
//! `weekly`) stands for every sample of one account in one local day / ISO week: `ts` is the
//! bucket start, `first_*` its first sample, `last_ts` + `utilization`/`reset` its last, and
//! `consumed` the consumption *between* those samples assuming the high-water mark started at the
//! bucket's first reading. `seg_max` (the highest reading before the first reset inside the
//! bucket), `last_hw` (the high-water mark at its end) and `resets` (resets inside it) let a reader
//! re-base `consumed` on the high-water mark actually carried in ([`HistRow::inner`]).
//! Consumption between two rows is computed from the earlier row's last sample and the later
//! row's first, so totals do not depend on which tier the history currently sits in.
//!
//! # Consumption
//! Per window segment (between resets) consumption is the rise of the high-water mark: a reading
//! `b` after high-water `hw` consumes `max(0, b - hw)`, and a dip never lowers `hw`, so jitter
//! (0.42 -> 0.41 -> 0.42) consumes nothing. A **reset** starts a new segment whose value counts
//! from 0 (consumes `b`). When both readings carry `resets_at`, a reset is exactly `resets_at`
//! moving forward by at least [`RESET_ADVANCE_SECS`]; only when either lacks it does a drop of
//! more than [`RESET_DROP`] count as one.
//!
//! Post-reset consumption accrues from the old `resets_at` (when it falls between the readings)
//! and never earlier than the new window's start (`new resets_at - 7 d`). When several resets fall
//! inside one gap between readings, the use in the intermediate windows is unknowable and lost.

use std::collections::HashMap;

use anyhow::Context;
use rusqlite::{params, Connection, OptionalExtension, Row};

/// The `window` value of the account-wide 7-day window samples.
pub const WINDOW_7D: &str = "7d";
/// Without `resets_at` on both readings, a utilization drop larger than this is a window reset
/// (smaller dips are rounding noise between header and poll readings).
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

/// One `usage_samples` row (a raw sample has `first == last`, `consumed == 0` and
/// `seg_max == last_hw ==` its value).
#[derive(Debug, Clone, PartialEq)]
pub struct HistRow {
    pub account: String,
    pub window: String,
    pub gran: Granularity,
    /// Sample time (raw) or bucket start.
    pub ts: i64,
    /// Consumption between the row's readings, high-water mark starting at `first.util`.
    pub consumed: f64,
    pub first: Point,
    pub last: Point,
    /// Highest reading before the first reset inside the row (`>= first.util`).
    pub seg_max: f64,
    /// High-water mark after the last reading (high-water starting at `first.util`).
    pub last_hw: f64,
    /// Resets inside the row.
    pub resets: u32,
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
            seg_max: p.util,
            last_hw: p.util,
            resets: 0,
        }
    }

    /// A time inside the bucket (between its first and last sample).
    pub fn anchor(&self) -> i64 {
        self.first.ts + (self.last.ts - self.first.ts) / 2
    }

    /// Consumption inside the row when the high-water mark after entering its first reading is
    /// `h` (`>= first.util`), and the high-water mark after its last reading.
    pub fn inner(&self, h: f64) -> (f64, f64) {
        let consumed =
            self.consumed - (self.seg_max - self.first.util) + (self.seg_max - h).max(0.0);
        let end = if self.resets > 0 {
            self.last_hw
        } else {
            h.max(self.seg_max)
        };
        (consumed, end)
    }
}

/// Whether the window reset between `a` and the later reading `b` (see the module docs).
pub fn is_reset(a: &Point, b: &Point) -> bool {
    match (a.reset, b.reset) {
        (Some(ra), Some(rb)) => rb >= ra + RESET_ADVANCE_SECS,
        _ => b.util < a.util - RESET_DROP,
    }
}

/// One transition between consecutive readings.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Step {
    pub consumed: f64,
    /// When the consumption started accruing (it accrued linearly up to `b.ts`).
    pub from: i64,
    /// High-water mark after `b`.
    pub hw: f64,
    pub reset: bool,
}

/// The transition from reading `a` (with high-water mark `hw`) to the next reading `b`.
pub fn step(a: &Point, hw: f64, b: &Point) -> Step {
    if is_reset(a, b) {
        let mut from = a.reset.filter(|r| *r > a.ts && *r < b.ts).unwrap_or(a.ts);
        if let Some(rb) = b.reset {
            from = from.max(rb - WEEK); // never before the new window opened
        }
        Step {
            consumed: b.util.max(0.0),
            from: from.min(b.ts),
            hw: b.util,
            reset: true,
        }
    } else {
        Step {
            consumed: (b.util - hw).max(0.0),
            from: a.ts,
            hw: hw.max(b.util),
            reset: false,
        }
    }
}

/// Fold time-ordered rows of one account into one bucket (`account`, `window`, `gran` and `ts`
/// come from the first row; the caller sets the bucket's own).
pub fn fold(rows: &[HistRow]) -> Option<HistRow> {
    let mut acc = rows.first()?.clone();
    for r in &rows[1..] {
        let s = step(&acc.last, acc.last_hw, &r.first);
        let (inner, end) = r.inner(s.hw);
        acc.consumed += s.consumed + inner;
        if acc.resets == 0 && !s.reset {
            acc.seg_max = acc.seg_max.max(r.seg_max);
        }
        acc.resets += r.resets + u32::from(s.reset);
        acc.last_hw = end;
        acc.last = r.last;
    }
    Some(acc)
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

/// Insert one raw sample (call on the store writer).
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
                                   consumed, first_ts, first_util, first_reset, last_ts,
                                   seg_max, last_hw, resets)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
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
            r.last.ts,
            r.seg_max,
            r.last_hw,
            r.resets
        ],
    )?;
    Ok(())
}

pub(crate) const COLUMNS: &str = "rowid, account, window, granularity, ts, utilization, reset, \
     consumed, first_ts, first_util, first_reset, last_ts, seg_max, last_hw, resets";

/// Parse a row selected with [`COLUMNS`] into `(rowid, row)`; `None` for an unknown granularity.
/// Missing bucket columns (a raw row) fall back to the row's own `ts`/value.
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
    let seg_max = r
        .get::<_, Option<f64>>(12)?
        .unwrap_or(first.util.max(last.util));
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
            seg_max,
            last_hw: r.get::<_, Option<f64>>(13)?.unwrap_or(seg_max),
            resets: r.get::<_, Option<u32>>(14)?.unwrap_or(0),
        }),
    ))
}

/// Accounts with `window` history.
fn accounts(c: &Connection, window: &str) -> anyhow::Result<Vec<String>> {
    let mut st = c.prepare("SELECT DISTINCT account FROM usage_samples WHERE window = ?1")?;
    let out = st
        .query_map([window], |r| r.get(0))?
        .collect::<Result<_, _>>()?;
    Ok(out)
}

/// Every `window` row an account needs to total burn from `since` on: the rows ending at or after
/// `since`, plus each account's latest row before them (the left end of a delta straddling
/// `since`). Ordered by account, then time.
pub fn rows_since(c: &Connection, window: &str, since: i64) -> anyhow::Result<Vec<HistRow>> {
    // A bucket starts at most a week before its last sample.
    let from = since - WEEK;
    // Per account, so both halves are ranges on the (account, window, ts) index.
    let sql = format!(
        "SELECT {COLUMNS} FROM usage_samples WHERE account = ?1 AND window = ?2 AND ts >= ?3
         UNION ALL
         SELECT * FROM (SELECT {COLUMNS} FROM usage_samples
                        WHERE account = ?1 AND window = ?2 AND ts < ?3
                        ORDER BY ts DESC LIMIT 1)"
    );
    let mut st = c.prepare(&sql).context("prepare usage history read")?;
    let mut rows: Vec<HistRow> = Vec::new();
    for account in accounts(c, window)? {
        for r in st.query_map(params![account, window, from], from_row)? {
            rows.extend(r?.1);
        }
    }
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
    let sql = format!(
        "SELECT {COLUMNS} FROM usage_samples WHERE window = ?1 AND account = ?2
         ORDER BY COALESCE(last_ts, ts) DESC, rowid DESC LIMIT 1"
    );
    let mut out = HashMap::new();
    for a in accounts(c, window)? {
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
        let s = step(&p(0, 0.2, Some(100)), 0.2, &p(10, 0.5, Some(100)));
        assert_eq!(s.consumed, 0.5 - 0.2);
        // dip then recovery: nothing consumed, the high-water mark holds
        let s = step(&p(0, 0.42, None), 0.42, &p(10, 0.41, None));
        assert_eq!((s.consumed, s.hw, s.reset), (0.0, 0.42, false));
        let s = step(&p(10, 0.41, None), s.hw, &p(20, 0.42, None));
        assert_eq!(s.consumed, 0.0);
        // a drop with the same resets_at is never a reset
        let s = step(&p(0, 0.82, Some(9)), 0.82, &p(10, 0.79, Some(9)));
        assert_eq!((s.consumed, s.reset), (0.0, false));
        // reset: the new value counts from 0, from the old reset time when in between
        let s = step(&p(0, 0.9, Some(5)), 0.9, &p(10, 0.1, Some(5 + WEEK)));
        assert_eq!((s.consumed, s.from, s.hw, s.reset), (0.1, 5, 0.1, true));
        // reset moved forward while utilization rose
        let s = step(&p(0, 0.3, Some(50)), 0.3, &p(100, 0.4, Some(50 + WEEK)));
        assert_eq!((s.consumed, s.from), (0.4, 50));
        // accrual never starts before the new window opened (two resets in one gap)
        let s = step(
            &p(0, 0.5, Some(50)),
            0.5,
            &p(3 * WEEK, 0.2, Some(50 + 3 * WEEK)),
        );
        assert_eq!((s.consumed, s.from), (0.2, 50 + 2 * WEEK));
        // reset jitter is not a reset; without resets_at on both a big drop is
        assert!(!is_reset(&p(0, 0.3, Some(50)), &p(100, 0.4, Some(80))));
        assert!(is_reset(&p(0, 0.3, None), &p(100, 0.1, Some(80))));
    }

    /// Folding any split of a reading sequence gives the same result as folding it whole.
    #[test]
    fn fold_is_split_invariant() {
        let (r1, r2) = (Some(1000), Some(1000 + WEEK));
        let seq = [
            p(1, 0.42, r1),
            p(2, 0.41, r1),
            p(3, 0.45, r1),
            p(4, 0.44, r1),
            p(5, 0.30, r2),
            p(6, 0.29, r2),
            p(7, 0.33, r2),
        ];
        let raws: Vec<HistRow> = seq
            .iter()
            .map(|x| HistRow::raw("a", WINDOW_7D, *x))
            .collect();
        let whole = fold(&raws).unwrap();
        assert!(
            (whole.consumed - (0.03 + 0.30 + 0.03)).abs() < 1e-12,
            "{whole:?}"
        );
        for cut in 1..raws.len() {
            let both = fold(&[fold(&raws[..cut]).unwrap(), fold(&raws[cut..]).unwrap()]).unwrap();
            assert!((both.consumed - whole.consumed).abs() < 1e-12, "cut {cut}");
            assert!((both.last_hw - whole.last_hw).abs() < 1e-12, "cut {cut}");
            assert_eq!(both.resets, whole.resets);
            // re-based on a higher carried-in high-water mark, too
            let (a, ea) = whole.inner(0.44);
            let (b, eb) = both.inner(0.44);
            assert!(
                (a - b).abs() < 1e-12 && (ea - eb).abs() < 1e-12,
                "cut {cut}"
            );
        }
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
