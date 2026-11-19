//! `cwd_projects`: the project of each working directory requests named (usage attribution), and
//! the per-request rows the attribution report totals. See [`crate::analytics::attribution`] for
//! how a project is found and [`crate::analytics::projects`] for the report.

use std::collections::HashMap;

use rusqlite::{params, Connection};

use crate::analytics::attribution::Project;

/// Record that working directory `cwd_hash` belongs to `project`, first seen at `ts` (unix
/// seconds). The latest label wins; `first_seen` is kept.
pub fn upsert(
    conn: &Connection,
    cwd_hash: &str,
    project: &Project,
    ts: i64,
) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO cwd_projects(cwd_hash, project, path_hash, first_seen)
         VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT(cwd_hash) DO UPDATE SET
             project = excluded.project,
             path_hash = excluded.path_hash,
             first_seen = min(first_seen, excluded.first_seen)",
        params![cwd_hash, project.name, project.path_hash, ts],
    )?;
    Ok(())
}

/// Drop the working directories the journal no longer holds a request of (after journal
/// pruning).
pub fn prune_orphans(conn: &Connection) -> rusqlite::Result<usize> {
    conn.execute(
        "DELETE FROM cwd_projects WHERE cwd_hash NOT IN
             (SELECT cwd_hash FROM journal_requests WHERE cwd_hash IS NOT NULL)",
        [],
    )
}

/// Every known working directory's project: `cwd_hash -> (project, path_hash)`.
pub fn projects(conn: &Connection) -> anyhow::Result<HashMap<String, (String, Option<String>)>> {
    let mut st = conn.prepare("SELECT cwd_hash, project, path_hash FROM cwd_projects")?;
    let rows = st.query_map([], |r| Ok((r.get(0)?, (r.get(1)?, r.get(2)?))))?;
    Ok(rows.collect::<Result<_, _>>()?)
}

/// One Messages request since the report's start, with what attribution needs.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UsageRow {
    pub ts: i64,
    pub session_hash: Option<String>,
    /// The working directory the request named (hash), if any.
    pub cwd_hash: Option<String>,
    /// The request's project (`None`: unknown); filled in by the report.
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
        "SELECT r.ts, r.session_hash, r.cwd_hash,
            (SELECT a.account FROM journal_attempts a WHERE a.request_id = r.id
             ORDER BY a.n DESC LIMIT 1),
            coalesce(r.input_tokens, 0), coalesce(r.output_tokens, 0),
            coalesce(r.cache_read, 0), coalesce(r.cache_write, 0)
         FROM journal_requests r
         WHERE r.ts >= ?1 AND (r.path IS NULL OR r.path = '/v1/messages')
         ORDER BY r.ts, r.rowid",
    )?;
    let rows = st.query_map([since], |r| {
        Ok(UsageRow {
            ts: r.get(0)?,
            session_hash: r.get(1)?,
            cwd_hash: r.get(2)?,
            account: r.get(3)?,
            input_tokens: r.get(4)?,
            output_tokens: r.get(5)?,
            cache_read: r.get(6)?,
            cache_write: r.get(7)?,
            ..UsageRow::default()
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
            upsert(c, "c1", &project("acme", "h1"), 100)?;
            upsert(c, "c1", &project("acme2", "h2"), 200)?;
            Ok(())
        }))
        .unwrap();
        let row: (String, String, i64) = store
            .read(|c| {
                Ok(c.query_row(
                    "SELECT project, path_hash, first_seen FROM cwd_projects",
                    [],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                )?)
            })
            .unwrap();
        assert_eq!(row, ("acme2".into(), "h2".into(), 100));
        let all = store.read(projects).unwrap();
        assert_eq!(all["c1"], ("acme2".to_string(), Some("h2".to_string())));
    }

    #[test]
    fn usage_rows_carry_the_cwd_and_serving_account() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("pooltime.db")).unwrap();
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let req = |id: &str, ts: i64, path: &str, cwd: Option<&str>| RequestRow {
            id: id.into(),
            ts,
            path: Some(path.into()),
            outcome: "completed".into(),
            input_tokens: Some(10),
            output_tokens: Some(2),
            session_hash: Some("s1".into()),
            cwd_hash: cwd.map(str::to_string),
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
            journal::insert(c, &req("old", 5, "/v1/messages", Some("c1")), &[])?;
            journal::insert(
                c,
                &req("a", 20, "/v1/messages", Some("c1")),
                &[att(1, "acct-a"), att(2, "acct-b")],
            )?;
            journal::insert(c, &req("b", 21, "/v1/messages", None), &[att(1, "acct-a")])?;
            journal::insert(c, &req("c", 22, "/v1/messages/count_tokens", None), &[])?;
            upsert(c, "c1", &project("acme", "h1"), 20)?;
            upsert(c, "gone", &project("old", "h0"), 1)?;
            Ok(())
        }))
        .unwrap();
        let rows = store.read(|c| usage_rows(c, 10)).unwrap();
        assert_eq!(rows.len(), 2, "{rows:?}");
        assert_eq!(rows[0].cwd_hash.as_deref(), Some("c1"));
        assert_eq!(rows[0].account.as_deref(), Some("acct-b"), "last attempt");
        assert_eq!((rows[0].input_tokens, rows[0].cache_read), (10, 0));
        assert_eq!(rows[1].cwd_hash, None);

        let pruned = rt.block_on(store.exec(|c| Ok(prune_orphans(c)?))).unwrap();
        assert_eq!(pruned, 1, "the directory without journal rows");
    }
}
