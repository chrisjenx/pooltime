//! `session_projects`: the project each client session works in (usage attribution), and the
//! per-request rows the attribution report totals. See [`crate::analytics::attribution`] for how
//! a project is found and [`crate::analytics::projects`] for the report.

use rusqlite::{params, Connection};

use crate::analytics::attribution::Project;

/// Record that session `session_hash` works in `project`, seen at `ts` (unix seconds). The latest
/// label wins; `first_seen` is kept.
pub fn upsert(
    conn: &Connection,
    session_hash: &str,
    project: &Project,
    ts: i64,
) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO session_projects(session_hash, project, path_hash, first_seen, last_seen)
         VALUES (?1, ?2, ?3, ?4, ?4)
         ON CONFLICT(session_hash) DO UPDATE SET
             project = excluded.project,
             path_hash = excluded.path_hash,
             first_seen = min(first_seen, excluded.first_seen),
             last_seen = max(last_seen, excluded.last_seen)",
        params![session_hash, project.name, project.path_hash, ts],
    )?;
    Ok(())
}

/// Drop the sessions the journal no longer holds a request of (after journal pruning).
pub fn prune_orphans(conn: &Connection) -> rusqlite::Result<usize> {
    conn.execute(
        "DELETE FROM session_projects WHERE session_hash NOT IN
             (SELECT session_hash FROM journal_requests WHERE session_hash IS NOT NULL)",
        [],
    )
}

/// One Messages request since the report's start, with what attribution needs.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UsageRow {
    pub ts: i64,
    pub session_hash: Option<String>,
    /// The session's project (`None`: unknown).
    pub project: Option<String>,
    pub path_hash: Option<String>,
    /// The account that served it (the last attempt's), if any attempt ran.
    pub account: Option<String>,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cache_read: i64,
    pub cache_write: i64,
}

/// Every `/v1/messages` request since `since` (unix seconds), oldest first.
pub fn usage_rows(conn: &Connection, since: i64) -> anyhow::Result<Vec<UsageRow>> {
    let mut st = conn.prepare(
        "SELECT r.ts, r.session_hash, sp.project, sp.path_hash,
            (SELECT a.account FROM journal_attempts a WHERE a.request_id = r.id
             ORDER BY a.n DESC LIMIT 1),
            coalesce(r.input_tokens, 0), coalesce(r.output_tokens, 0),
            coalesce(r.cache_read, 0), coalesce(r.cache_write, 0)
         FROM journal_requests r
         LEFT JOIN session_projects sp ON sp.session_hash = r.session_hash
         WHERE r.ts >= ?1 AND (r.path IS NULL OR r.path = '/v1/messages')
         ORDER BY r.ts, r.rowid",
    )?;
    let rows = st.query_map([since], |r| {
        Ok(UsageRow {
            ts: r.get(0)?,
            session_hash: r.get(1)?,
            project: r.get(2)?,
            path_hash: r.get(3)?,
            account: r.get(4)?,
            input_tokens: r.get(5)?,
            output_tokens: r.get(6)?,
            cache_read: r.get(7)?,
            cache_write: r.get(8)?,
        })
    })?;
    Ok(rows.collect::<Result<_, _>>()?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::journal::{self, AttemptRow, RequestRow};
    use crate::store::Store;

    fn project(name: &str, hash: &str) -> Project {
        Project {
            name: name.into(),
            path_hash: hash.into(),
        }
    }

    #[test]
    fn upsert_keeps_first_seen_and_latest_label() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("pooltime.db")).unwrap();
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        rt.block_on(store.exec(|c| {
            upsert(c, "s1", &project("acme", "h1"), 100)?;
            upsert(c, "s1", &project("acme2", "h2"), 200)?;
            upsert(c, "s1", &project("acme2", "h2"), 150)?;
            Ok(())
        }))
        .unwrap();
        let row: (String, String, i64, i64) = store
            .read(|c| {
                Ok(c.query_row(
                    "SELECT project, path_hash, first_seen, last_seen FROM session_projects",
                    [],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
                )?)
            })
            .unwrap();
        assert_eq!(row, ("acme2".into(), "h2".into(), 100, 200));
    }

    #[test]
    fn usage_rows_join_the_session_project_and_serving_account() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("pooltime.db")).unwrap();
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let req = |id: &str, ts: i64, path: &str, session: Option<&str>| RequestRow {
            id: id.into(),
            ts,
            path: Some(path.into()),
            outcome: "completed".into(),
            input_tokens: Some(10),
            output_tokens: Some(2),
            session_hash: session.map(str::to_string),
            ..RequestRow::default()
        };
        let att = |n: u32, account: &str| AttemptRow {
            n,
            account: account.into(),
            provider: "anthropic".into(),
            outcome: "completed".into(),
            ..AttemptRow::default()
        };
        rt.block_on(store.exec(move |c| {
            journal::insert(c, &req("old", 5, "/v1/messages", Some("s1")), &[])?;
            journal::insert(
                c,
                &req("a", 20, "/v1/messages", Some("s1")),
                &[att(1, "acct-a"), att(2, "acct-b")],
            )?;
            journal::insert(c, &req("b", 21, "/v1/messages", None), &[att(1, "acct-a")])?;
            journal::insert(
                c,
                &req("c", 22, "/v1/messages/count_tokens", Some("s1")),
                &[],
            )?;
            upsert(c, "s1", &project("acme", "h1"), 20)?;
            upsert(c, "gone", &project("old", "h0"), 1)?;
            Ok(())
        }))
        .unwrap();
        let rows = store.read(|c| usage_rows(c, 10)).unwrap();
        assert_eq!(rows.len(), 2, "{rows:?}");
        assert_eq!(rows[0].project.as_deref(), Some("acme"));
        assert_eq!(rows[0].account.as_deref(), Some("acct-b"), "last attempt");
        assert_eq!((rows[0].input_tokens, rows[0].cache_read), (10, 0));
        assert_eq!(rows[1].project, None);
        assert_eq!(rows[1].session_hash, None);

        let pruned = rt.block_on(store.exec(|c| Ok(prune_orphans(c)?))).unwrap();
        assert_eq!(pruned, 1, "the session without journal rows");
    }
}
