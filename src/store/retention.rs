//! Retention: age/size pruning of the journal, timeline and alerts, tiered
//! downsampling of `usage_samples` (hourly -> daily -> weekly -> deleted), and
//! a `VACUUM` when enough pages were freed.

use std::collections::BTreeMap;

use anyhow::Context;
use rusqlite::{params, Connection};
use time::OffsetDateTime;

use super::usage_history::{self, day_start, week_start, Granularity, HistRow};
use super::Store;
use crate::config::RetentionCfg;

const DAY: i64 = 86_400;
const MIB: i64 = 1024 * 1024;
/// Journal requests deleted per step while enforcing the size cap.
const SIZE_BATCH: i64 = 100;
/// `VACUUM` once more than this fraction of pages is on the freelist.
const VACUUM_FREE_FRACTION: f64 = 0.10;

/// What one [`prune`] run did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PruneReport {
    /// Journal requests deleted for exceeding `journal_days`.
    pub journal_requests_by_age: u64,
    /// Journal requests deleted (oldest first) to get under `journal_max_mb` and
    /// `journal_max_requests`.
    pub journal_requests_by_size: u64,
    /// Journal attempt rows deleted along with their requests.
    pub journal_attempts: u64,
    pub timeline: u64,
    pub alerts_sent: u64,
    /// Hourly rows consumed by the daily rollup.
    pub usage_hourly_rolled_up: u64,
    pub usage_daily_created: u64,
    /// Daily rows consumed by the weekly rollup.
    pub usage_daily_rolled_up: u64,
    pub usage_weekly_created: u64,
    /// Rows deleted for being older than `history_weekly_days`.
    pub usage_expired: u64,
    pub vacuumed: bool,
}

/// One retention run at `now`. Usage-history buckets are local days / ISO weeks at `now`'s UTC
/// offset (the daemon passes the local time, so they line up with the burn report's weeks).
pub async fn prune(
    store: &Store,
    cfg: &RetentionCfg,
    now: OffsetDateTime,
) -> anyhow::Result<PruneReport> {
    let cfg = cfg.clone();
    let off = i64::from(now.offset().whole_seconds());
    let now = now.unix_timestamp();
    store
        .exec(move |conn| prune_conn(conn, &cfg, now, off))
        .await
}

fn prune_conn(
    conn: &mut Connection,
    cfg: &RetentionCfg,
    now: i64,
    off: i64,
) -> anyhow::Result<PruneReport> {
    let mut rep = PruneReport::default();
    prune_journal(conn, cfg, now, &mut rep).context("prune journal")?;
    downsample_usage(conn, cfg, now, off, &mut rep).context("downsample usage history")?;

    let (pages, free) = page_counts(conn)?;
    if pages > 0 && (free as f64) / (pages as f64) > VACUUM_FREE_FRACTION {
        conn.execute_batch("VACUUM")?;
        rep.vacuumed = true;
    }
    // Fold the WAL back into the main file and shrink it. A busy reader makes
    // this a partial no-op (reported in the result row), not an error.
    conn.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()))?;
    Ok(rep)
}

fn page_counts(conn: &Connection) -> anyhow::Result<(i64, i64)> {
    let pages = conn.query_row("PRAGMA page_count", [], |r| r.get(0))?;
    let free = conn.query_row("PRAGMA freelist_count", [], |r| r.get(0))?;
    Ok((pages, free))
}

/// Bytes of the database file that hold data (file size minus freelist).
fn live_bytes(conn: &Connection) -> anyhow::Result<i64> {
    let (pages, free) = page_counts(conn)?;
    let page_size: i64 = conn.query_row("PRAGMA page_size", [], |r| r.get(0))?;
    Ok((pages - free) * page_size)
}

fn prune_journal(
    conn: &mut Connection,
    cfg: &RetentionCfg,
    now: i64,
    rep: &mut PruneReport,
) -> anyhow::Result<()> {
    let cutoff = now - i64::from(cfg.journal_days) * DAY;
    let tx = conn.transaction()?;
    rep.journal_attempts += tx.execute(
        "DELETE FROM journal_attempts WHERE request_id IN (SELECT id FROM journal_requests WHERE ts < ?1)",
        [cutoff],
    )? as u64;
    rep.journal_requests_by_age =
        tx.execute("DELETE FROM journal_requests WHERE ts < ?1", [cutoff])? as u64;
    rep.timeline = tx.execute("DELETE FROM timeline WHERE ts < ?1", [cutoff])? as u64;
    rep.alerts_sent = tx.execute("DELETE FROM alerts_sent WHERE ts < ?1", [cutoff])? as u64;
    tx.commit()?;

    // Row ceiling: drop everything older than the newest `journal_max_requests` rows (0 = no
    // ceiling).
    if cfg.journal_max_requests > 0 {
        let max_rows = i64::from(cfg.journal_max_requests);
        let keep_from =
            "SELECT id FROM journal_requests ORDER BY ts DESC, id DESC LIMIT -1 OFFSET ?1";
        let tx = conn.transaction()?;
        let attempts = tx.execute(
            &format!("DELETE FROM journal_attempts WHERE request_id IN ({keep_from})"),
            [max_rows],
        )?;
        let requests = tx.execute(
            &format!("DELETE FROM journal_requests WHERE id IN ({keep_from})"),
            [max_rows],
        )?;
        tx.commit()?;
        rep.journal_attempts += attempts as u64;
        rep.journal_requests_by_size += requests as u64;
    }

    let cap = i64::from(cfg.journal_max_mb) * MIB;
    while live_bytes(conn)? > cap {
        let tx = conn.transaction()?;
        // Same ordered subselect for both deletes; nothing else writes inside
        // this transaction so it picks the same ids.
        let oldest = "SELECT id FROM journal_requests ORDER BY ts, id LIMIT ?1";
        let attempts = tx.execute(
            &format!("DELETE FROM journal_attempts WHERE request_id IN ({oldest})"),
            [SIZE_BATCH],
        )?;
        let requests = tx.execute(
            &format!("DELETE FROM journal_requests WHERE id IN ({oldest})"),
            [SIZE_BATCH],
        )?;
        tx.commit()?;
        if requests == 0 {
            break; // nothing left to trim; the rest of the db is not ours to cap
        }
        rep.journal_attempts += attempts as u64;
        rep.journal_requests_by_size += requests as u64;
    }
    Ok(())
}

fn downsample_usage(
    conn: &mut Connection,
    cfg: &RetentionCfg,
    now: i64,
    off: i64,
    rep: &mut PruneReport,
) -> anyhow::Result<()> {
    // Cutoffs are aligned to a local day / ISO-week boundary so a bucket is
    // always rolled up whole (a bucket that already exists absorbs late rows).
    let hourly_cut = day_start(now - i64::from(cfg.history_hourly_days) * DAY, off);
    let weekly_cut = week_start(now - i64::from(cfg.history_daily_days) * DAY, off);
    let expire_cut = now - i64::from(cfg.history_weekly_days) * DAY;

    let tx = conn.transaction()?;
    let (rolled, created) = rollup(&tx, Granularity::Raw, Granularity::Daily, hourly_cut, |t| {
        day_start(t, off)
    })?;
    rep.usage_hourly_rolled_up = rolled;
    rep.usage_daily_created = created;
    let (rolled, created) = rollup(
        &tx,
        Granularity::Daily,
        Granularity::Weekly,
        weekly_cut,
        |t| week_start(t, off),
    )?;
    rep.usage_daily_rolled_up = rolled;
    rep.usage_weekly_created = created;

    rep.usage_expired = tx.execute(
        "DELETE FROM usage_samples WHERE ts < ?1",
        params![expire_cut],
    )? as u64;
    tx.commit()?;
    Ok(())
}

/// Fold every `src` row older than `cut` into one `dst` row per (account, window, bucket):
/// consumption between the folded readings is kept in `consumed`, the first and last readings
/// as the bucket's ends (see [`usage_history`]), so weekly burn totals come out the same. An
/// existing `dst` row for the bucket is folded in too. Returns `(rows rolled up, rows created)`.
fn rollup(
    tx: &Connection,
    src: Granularity,
    dst: Granularity,
    cut: i64,
    bucket_of: impl Fn(i64) -> i64,
) -> anyhow::Result<(u64, u64)> {
    let mut st = tx.prepare(&format!(
        "SELECT {} FROM usage_samples WHERE granularity = ?1 AND ts < ?2",
        usage_history::COLUMNS
    ))?;
    let src_rows: Vec<(i64, Option<HistRow>)> = st
        .query_map(params![src.as_str(), cut], usage_history::from_row)?
        .collect::<Result<_, _>>()?;
    if src_rows.is_empty() {
        return Ok((0, 0));
    }
    let mut groups: BTreeMap<(String, String, i64), Vec<HistRow>> = BTreeMap::new();
    let mut rolled = 0u64;
    for (rowid, row) in src_rows {
        let Some(row) = row else { continue };
        tx.execute("DELETE FROM usage_samples WHERE rowid = ?1", [rowid])?;
        rolled += 1;
        let key = (
            row.account.clone(),
            row.window.clone(),
            bucket_of(row.anchor()),
        );
        groups.entry(key).or_default().push(row);
    }
    let mut existing = tx.prepare(&format!(
        "SELECT {} FROM usage_samples WHERE granularity = ?1 AND account = ?2 AND window = ?3 AND ts = ?4",
        usage_history::COLUMNS
    ))?;
    let mut created = 0u64;
    for ((account, window, start), mut rows) in groups {
        let old: Vec<(i64, Option<HistRow>)> = existing
            .query_map(
                params![dst.as_str(), account, window, start],
                usage_history::from_row,
            )?
            .collect::<Result<_, _>>()?;
        if old.is_empty() {
            created += 1;
        }
        for (rowid, row) in old {
            tx.execute("DELETE FROM usage_samples WHERE rowid = ?1", [rowid])?;
            rows.extend(row);
        }
        rows.sort_by(|a, b| a.first.ts.cmp(&b.first.ts).then(a.last.ts.cmp(&b.last.ts)));
        let Some((consumed, first, last)) = usage_history::fold(&rows) else {
            continue;
        };
        usage_history::insert_bucket(
            tx,
            &HistRow {
                account,
                window,
                gran: dst,
                ts: start,
                consumed,
                first,
                last,
            },
        )?;
    }
    Ok((rolled, created))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::{params, Connection};
    use std::sync::Arc;

    const DAY: i64 = 86_400;
    // Midday of some day in Nov 2026.
    const NOW: i64 = 1_794_000_000 - (1_794_000_000 % DAY) + 12 * 3600;

    fn now() -> OffsetDateTime {
        OffsetDateTime::from_unix_timestamp(NOW).unwrap()
    }

    fn open() -> (tempfile::TempDir, Arc<Store>) {
        let dir = tempfile::tempdir().unwrap();
        let s = Store::open(&dir.path().join("pooltime.db")).unwrap();
        (dir, Arc::new(s))
    }

    fn insert_request(c: &Connection, id: &str, ts: i64, model: &str) {
        c.execute(
            "INSERT INTO journal_requests(id, ts, model, provider, status, outcome) VALUES (?1,?2,?3,'anthropic',200,'ok')",
            params![id, ts, model],
        )
        .unwrap();
        c.execute(
            "INSERT INTO journal_attempts(request_id, n, account, provider, outcome) VALUES (?1,1,'a','anthropic','ok')",
            params![id],
        )
        .unwrap();
    }

    fn count(store: &Store, sql: &'static str) -> i64 {
        store
            .read(|c| Ok(c.query_row(sql, [], |r| r.get(0))?))
            .unwrap()
    }

    #[tokio::test]
    async fn prune_drops_journal_older_than_30d() {
        let (_d, store) = open();
        store
            .exec(|c| {
                insert_request(c, "old", NOW - 31 * DAY, "m");
                insert_request(c, "new", NOW - 29 * DAY, "m");
                c.execute(
                    "INSERT INTO timeline(ts, account, from_state, to_state, reason) VALUES (?1,'a','ok','cooldown','rl'), (?2,'a','cooldown','ok','done')",
                    params![NOW - 31 * DAY, NOW - DAY],
                )?;
                c.execute(
                    "INSERT INTO alerts_sent(key, ts) VALUES ('old', ?1), ('new', ?2)",
                    params![NOW - 31 * DAY, NOW - DAY],
                )?;
                Ok(())
            })
            .await
            .unwrap();
        let rep = prune(&store, &RetentionCfg::default(), now())
            .await
            .unwrap();
        assert_eq!(rep.journal_requests_by_age, 1);
        assert_eq!(rep.journal_attempts, 1);
        assert_eq!(rep.timeline, 1);
        assert_eq!(rep.alerts_sent, 1);
        assert_eq!(count(&store, "SELECT count(*) FROM journal_requests"), 1);
        assert_eq!(
            count(
                &store,
                "SELECT count(*) FROM journal_requests WHERE id='new'"
            ),
            1
        );
        assert_eq!(count(&store, "SELECT count(*) FROM journal_attempts"), 1);
        assert_eq!(count(&store, "SELECT count(*) FROM timeline"), 1);
        assert_eq!(count(&store, "SELECT count(*) FROM alerts_sent"), 1);
    }

    #[tokio::test]
    async fn prune_enforces_journal_size_cap_oldest_first() {
        let (_d, store) = open();
        store
            .exec(|c| {
                let big = "x".repeat(10_000);
                let tx = c.transaction()?;
                for i in 0..400i64 {
                    // ts increases with i: i = 0 is the oldest. ~4 MB total.
                    insert_request(&tx, &format!("r{i:04}"), NOW - 20 * DAY + i, &big);
                }
                tx.commit()?;
                Ok(())
            })
            .await
            .unwrap();
        let cfg = RetentionCfg {
            journal_max_mb: 2,
            ..RetentionCfg::default()
        };
        let rep = prune(&store, &cfg, now()).await.unwrap();
        assert!(rep.journal_requests_by_size > 0, "{rep:?}");
        let left = count(&store, "SELECT count(*) FROM journal_requests");
        assert!(left > 0 && left < 400, "left={left}");
        // the survivors are the newest rows
        assert_eq!(
            count(
                &store,
                "SELECT count(*) FROM journal_requests WHERE id='r0399'"
            ),
            1
        );
        assert_eq!(
            count(
                &store,
                "SELECT count(*) FROM journal_requests WHERE id='r0000'"
            ),
            0
        );
        assert_eq!(
            count(&store, "SELECT count(*) FROM journal_attempts"),
            left,
            "attempts follow their requests"
        );
        assert!(store.size_bytes() <= 2 * 1024 * 1024 + 256 * 1024);
    }

    #[tokio::test]
    async fn prune_downsamples_usage_samples() {
        let (_d, store) = open();
        let day0 = (NOW - 40 * DAY) / DAY * DAY; // a day boundary 40-41 days back
        store
            .exec(move |c| {
                let ins = |ts: i64, acct: &str, win: &str, u: f64, reset: i64| {
                    c.execute(
                        "INSERT INTO usage_samples(ts, account, window, utilization, reset, granularity) VALUES (?1,?2,?3,?4,?5,'hourly')",
                        params![ts, acct, win, u, reset],
                    )
                    .unwrap();
                };
                // two days x 3 hourly rows each for (a, five_hour)
                for (i, u) in [0.1, 0.2, 0.3].iter().enumerate() {
                    ins(day0 + 3600 * i as i64, "a", "five_hour", *u, 1000 + i as i64);
                }
                for (i, u) in [0.5, 0.7, 0.9].iter().enumerate() {
                    ins(
                        day0 + DAY + 3600 * i as i64,
                        "a",
                        "five_hour",
                        *u,
                        2000 + i as i64,
                    );
                }
                // other window on day 0 stays separate
                ins(day0 + 60, "a", "seven_day", 0.4, 9);
                // recent hourly row is untouched
                ins(NOW - DAY, "a", "five_hour", 0.6, 5);
                // > 730 d: deleted
                c.execute(
                    "INSERT INTO usage_samples(ts, account, window, utilization, reset, granularity) VALUES (?1,'a','five_hour',0.5,1,'weekly')",
                    params![NOW - 800 * DAY],
                )?;
                Ok(())
            })
            .await
            .unwrap();
        let rep = prune(&store, &RetentionCfg::default(), now())
            .await
            .unwrap();
        assert_eq!(rep.usage_hourly_rolled_up, 7);
        assert_eq!(rep.usage_daily_created, 3);
        assert_eq!(rep.usage_expired, 1);

        type Row = (i64, String, String, f64, i64, String);
        let rows: Vec<Row> = store
            .read(|c| {
                let mut st = c.prepare(
                    "SELECT ts, account, window, utilization, reset, granularity FROM usage_samples ORDER BY granularity, window, ts",
                )?;
                let r = st.query_map([], |r| {
                    Ok((
                        r.get(0)?,
                        r.get(1)?,
                        r.get(2)?,
                        r.get(3)?,
                        r.get(4)?,
                        r.get(5)?,
                    ))
                })?;
                Ok(r.collect::<Result<_, _>>()?)
            })
            .unwrap();
        let daily: Vec<_> = rows.iter().filter(|r| r.5 == "daily").collect();
        assert_eq!(daily.len(), 3);
        let d0 = daily
            .iter()
            .find(|r| r.0 == day0 && r.2 == "five_hour")
            .unwrap();
        // A bucket keeps its last reading and the consumption between its readings.
        assert!((d0.3 - 0.3).abs() < 1e-9, "last {}", d0.3);
        assert_eq!(d0.4, 1002, "last reset");
        let d1 = daily.iter().find(|r| r.0 == day0 + DAY).unwrap();
        assert!((d1.3 - 0.9).abs() < 1e-9);
        let consumed = |ts: i64| -> f64 {
            store
                .read(move |c| {
                    Ok(c.query_row(
                        "SELECT consumed FROM usage_samples WHERE granularity = 'daily' AND window = 'five_hour' AND ts = ?1",
                        [ts],
                        |r| r.get(0),
                    )?)
                })
                .unwrap()
        };
        assert!((consumed(day0) - 0.2).abs() < 1e-9);
        assert!((consumed(day0 + DAY) - 0.4).abs() < 1e-9);
        assert_eq!(d1.4, 2002);
        assert!(daily.iter().any(|r| r.2 == "seven_day" && r.0 == day0));
        let hourly: Vec<_> = rows.iter().filter(|r| r.5 == "hourly").collect();
        assert_eq!(hourly.len(), 1);
        assert_eq!(hourly[0].0, NOW - DAY);
        assert!(rows.iter().all(|r| r.5 != "weekly"));

        // idempotent
        let rep2 = prune(&store, &RetentionCfg::default(), now())
            .await
            .unwrap();
        assert_eq!(rep2.usage_hourly_rolled_up, 0);
        assert_eq!(count(&store, "SELECT count(*) FROM usage_samples"), 4);
    }

    #[tokio::test]
    async fn prune_rolls_daily_into_iso_weeks() {
        let (_d, store) = open();
        // Find a Monday ~400 days back (epoch day 0 was a Thursday).
        let mut monday = (NOW - 400 * DAY) / DAY * DAY;
        while ((monday / DAY + 3) % 7) != 0 {
            monday += DAY;
        }
        store
            .exec(move |c| {
                for d in 0..9i64 {
                    // 7 days of week 1 (0.1..0.7), 2 days of week 2
                    let u = if d < 7 { 0.1 * (d + 1) as f64 } else { 0.5 };
                    c.execute(
                        "INSERT INTO usage_samples(ts, account, window, utilization, reset, granularity) VALUES (?1,'a','five_hour',?2,?3,'daily')",
                        params![monday + d * DAY, u, d],
                    )?;
                }
                // young daily row stays
                c.execute(
                    "INSERT INTO usage_samples(ts, account, window, utilization, reset, granularity) VALUES (?1,'a','five_hour',0.3,1,'daily')",
                    params![NOW - 100 * DAY],
                )?;
                Ok(())
            })
            .await
            .unwrap();
        let rep = prune(&store, &RetentionCfg::default(), now())
            .await
            .unwrap();
        assert_eq!(rep.usage_daily_rolled_up, 9);
        assert_eq!(rep.usage_weekly_created, 2);
        let weekly: Vec<(i64, f64, i64)> = store
            .read(|c| {
                let mut st = c.prepare(
                    "SELECT ts, utilization, reset FROM usage_samples WHERE granularity='weekly' ORDER BY ts",
                )?;
                let r = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
                Ok(r.collect::<Result<_, _>>()?)
            })
            .unwrap();
        assert_eq!(weekly.len(), 2);
        assert_eq!(weekly[0].0, monday);
        assert!((weekly[0].1 - 0.7).abs() < 1e-9, "last reading");
        assert_eq!(weekly[0].2, 6);
        assert_eq!(weekly[1].0, monday + 7 * DAY);
        assert!((weekly[1].1 - 0.5).abs() < 1e-9);
        assert_eq!(
            count(
                &store,
                "SELECT count(*) FROM usage_samples WHERE granularity='daily'"
            ),
            1
        );
    }

    #[tokio::test]
    async fn prune_vacuums_when_over_ten_percent_free() {
        let (_d, store) = open();
        store
            .exec(|c| {
                let big = "x".repeat(10_000);
                let tx = c.transaction()?;
                for i in 0..300i64 {
                    insert_request(&tx, &format!("r{i}"), NOW - 60 * DAY + i, &big);
                }
                tx.commit()?;
                Ok(())
            })
            .await
            .unwrap();
        let before = store.size_bytes();
        let rep = prune(&store, &RetentionCfg::default(), now())
            .await
            .unwrap();
        assert_eq!(rep.journal_requests_by_age, 300);
        assert!(rep.vacuumed);
        assert!(store.size_bytes() < before / 2);
    }

    #[tokio::test]
    async fn prune_on_empty_db_is_noop() {
        let (_d, store) = open();
        let rep = prune(&store, &RetentionCfg::default(), now())
            .await
            .unwrap();
        assert!(!rep.vacuumed);
        assert_eq!(rep.journal_requests_by_age, 0);
    }
}
