//! Row-level reads of the account `timeline` (every state transition with time and reason, spec
//! §6). Rows are written by [`crate::accounts::StateBook`] on each transition.

use rusqlite::Connection;
use serde::Serialize;

/// One transition. `ts` is unix seconds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TimelineRow {
    pub ts: i64,
    pub account: String,
    pub from_state: Option<String>,
    pub to_state: Option<String>,
    pub reason: Option<String>,
}

/// The newest `limit` transitions (newest first), optionally for one account.
pub fn list(
    conn: &Connection,
    account: Option<&str>,
    limit: u32,
) -> anyhow::Result<Vec<TimelineRow>> {
    let mut st = conn.prepare(
        "SELECT ts, account, from_state, to_state, reason FROM timeline
         WHERE ?1 IS NULL OR account = ?1 ORDER BY ts DESC, rowid DESC LIMIT ?2",
    )?;
    let rows = st.query_map(rusqlite::params![account, limit], |r| {
        Ok(TimelineRow {
            ts: r.get(0)?,
            account: r.get::<_, Option<String>>(1)?.unwrap_or_default(),
            from_state: r.get(2)?,
            to_state: r.get(3)?,
            reason: r.get(4)?,
        })
    })?;
    Ok(rows.collect::<Result<_, _>>()?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Store;

    #[tokio::test]
    async fn newest_first_and_filtered() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("pooltime.db")).unwrap();
        store
            .exec(|c| {
                c.execute_batch(
                    "INSERT INTO timeline VALUES (1,'a','ready','rate_limited','quota');
                     INSERT INTO timeline VALUES (2,'b','ready','erroring','transport:reset');
                     INSERT INTO timeline VALUES (3,'a','rate_limited','ready',NULL);",
                )?;
                Ok(())
            })
            .await
            .unwrap();
        let all = store.read(|c| list(c, None, 10)).unwrap();
        assert_eq!(all.iter().map(|r| r.ts).collect::<Vec<_>>(), [3, 2, 1]);
        let a = store.read(|c| list(c, Some("a"), 1)).unwrap();
        assert_eq!(a.len(), 1);
        assert_eq!(a[0].to_state.as_deref(), Some("ready"));
    }
}
