use std::ffi::OsString;
use std::fs::OpenOptions;
use std::os::unix::fs::OpenOptionsExt;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::sync::Mutex;
use std::time::Duration;

use anyhow::{anyhow, bail, Context};
use rusqlite::{Connection, OpenFlags};
use tokio::sync::oneshot;

use super::migrations::migrate;
use crate::fsutil::ensure_mode;

const BUSY_TIMEOUT: Duration = Duration::from_secs(5);
/// Idle read-only connections kept for reuse.
const READ_POOL_MAX: usize = 4;

type Job = Box<dyn FnOnce(&mut Connection) + Send>;

/// SQLite store. All writes go through one dedicated thread that owns the
/// writer connection; reads use separate read-only connections (WAL lets them
/// run concurrently with the writer).
pub struct Store {
    path: PathBuf,
    tx: mpsc::Sender<Job>,
    readers: Mutex<Vec<Connection>>,
}

fn sidecar(path: &Path, suffix: &str) -> PathBuf {
    let mut s: OsString = path.as_os_str().to_owned();
    s.push(suffix);
    PathBuf::from(s)
}

/// Tighten the db and its `-wal`/`-shm` files to 0600 (those that exist).
fn lock_down(path: &Path) -> anyhow::Result<()> {
    for p in [
        path.to_path_buf(),
        sidecar(path, "-wal"),
        sidecar(path, "-shm"),
    ] {
        match ensure_mode(&p, 0o600) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound && p != path => {}
            Err(e) => return Err(e).with_context(|| format!("chmod {}", p.display())),
        }
    }
    Ok(())
}

impl Store {
    /// Open (creating mode 0600 if needed) and migrate the database, then start
    /// the writer thread.
    pub fn open(path: &Path) -> anyhow::Result<Store> {
        // Create the file ourselves so it never exists with a wider mode.
        OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .open(path)
            .with_context(|| format!("create {}", path.display()))?;

        let mut conn =
            Connection::open(path).with_context(|| format!("open database {}", path.display()))?;
        conn.busy_timeout(BUSY_TIMEOUT)?;
        let mode: String =
            conn.pragma_update_and_check(None, "journal_mode", "WAL", |r| r.get(0))?;
        if !mode.eq_ignore_ascii_case("wal") {
            bail!("could not enable WAL journal mode (got {mode})");
        }
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        migrate(&mut conn)?;
        lock_down(path)?;

        let (tx, rx) = mpsc::channel::<Job>();
        std::thread::Builder::new()
            .name("pooltime-db".into())
            .spawn(move || {
                let mut conn = conn;
                for job in rx {
                    job(&mut conn);
                }
            })
            .context("spawn store writer thread")?;

        Ok(Store {
            path: path.to_path_buf(),
            tx,
            readers: Mutex::new(Vec::new()),
        })
    }

    /// Run `f` on the writer thread (strictly serialized with every other
    /// `exec`). A panic inside `f` is reported as an error; the thread survives.
    pub async fn exec<F, T>(&self, f: F) -> anyhow::Result<T>
    where
        F: FnOnce(&mut Connection) -> anyhow::Result<T> + Send + 'static,
        T: Send + 'static,
    {
        let (reply, rx) = oneshot::channel();
        let job: Job = Box::new(move |conn| {
            let r = catch_unwind(AssertUnwindSafe(|| f(conn)))
                .unwrap_or_else(|_| Err(anyhow!("store closure panicked")));
            let _ = reply.send(r);
        });
        self.tx
            .send(job)
            .map_err(|_| anyhow!("store writer thread has stopped"))?;
        rx.await
            .map_err(|_| anyhow!("store writer thread dropped the reply"))?
    }

    /// Queue `f` on the writer thread without waiting for it (fire-and-forget).
    /// Needs no async runtime. Jobs run in submission order, strictly serialized
    /// with every `exec`, so `exec(|_| Ok(()))` afterwards acts as a flush. A
    /// failing or panicking `f` is logged (`warn`), never propagated.
    pub fn submit<F>(&self, f: F) -> anyhow::Result<()>
    where
        F: FnOnce(&mut Connection) -> anyhow::Result<()> + Send + 'static,
    {
        let job: Job = Box::new(
            move |conn| match catch_unwind(AssertUnwindSafe(|| f(conn))) {
                Ok(Ok(())) => {}
                Ok(Err(e)) => {
                    tracing::warn!(error = %format!("{e:#}"), "queued store write failed")
                }
                Err(_) => tracing::warn!("queued store write panicked"),
            },
        );
        self.tx
            .send(job)
            .map_err(|_| anyhow!("store writer thread has stopped"))
    }

    /// Run `f` on a read-only connection. Blocking; keep it short.
    pub fn read<T>(&self, f: impl FnOnce(&Connection) -> anyhow::Result<T>) -> anyhow::Result<T> {
        let pooled = self.readers.lock().unwrap_or_else(|e| e.into_inner()).pop();
        let conn = match pooled {
            Some(c) => c,
            None => self.open_reader()?,
        };
        let r = f(&conn);
        let mut pool = self.readers.lock().unwrap_or_else(|e| e.into_inner());
        if pool.len() < READ_POOL_MAX {
            pool.push(conn);
        }
        r
    }

    fn open_reader(&self) -> anyhow::Result<Connection> {
        let conn = Connection::open_with_flags(
            &self.path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .with_context(|| format!("open read-only {}", self.path.display()))?;
        conn.busy_timeout(BUSY_TIMEOUT)?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        Ok(conn)
    }

    pub fn schema_version(&self) -> anyhow::Result<u32> {
        self.read(|c| Ok(c.query_row("PRAGMA user_version", [], |r| r.get(0))?))
    }

    /// Database file plus its write-ahead log, in bytes.
    pub fn size_bytes(&self) -> u64 {
        let len = |p: &Path| std::fs::metadata(p).map(|m| m.len()).unwrap_or(0);
        len(&self.path) + len(&sidecar(&self.path, "-wal"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::Arc;

    fn mode_of(p: &Path) -> u32 {
        std::fs::metadata(p).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn open_creates_current_schema_with_0600() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pooltime.db");
        let store = Store::open(&path).unwrap();
        assert_eq!(
            store.schema_version().unwrap(),
            super::super::SCHEMA_VERSION
        );
        assert_eq!(mode_of(&path), 0o600);
        for suffix in ["-wal", "-shm"] {
            let mut p = path.clone().into_os_string();
            p.push(suffix);
            let p = std::path::PathBuf::from(p);
            if p.exists() {
                assert_eq!(mode_of(&p), 0o600, "{suffix}");
            }
        }
        let tables: Vec<String> = store
            .read(|c| {
                let mut st =
                    c.prepare("SELECT name FROM sqlite_master WHERE type='table' ORDER BY name")?;
                let rows = st.query_map([], |r| r.get::<_, String>(0))?;
                Ok(rows.collect::<Result<_, _>>()?)
            })
            .unwrap();
        for t in [
            "account_state",
            "account_state_quarantine",
            "alerts_sent",
            "journal_attempts",
            "journal_requests",
            "sessions",
            "timeline",
            "usage_samples",
        ] {
            assert!(tables.iter().any(|x| x == t), "missing table {t}");
        }
        let (mode, sync): (String, i64) = store
            .read(|c| {
                Ok((
                    c.query_row("PRAGMA journal_mode", [], |r| r.get(0))?,
                    c.query_row("PRAGMA synchronous", [], |r| r.get(0))?,
                ))
            })
            .unwrap();
        assert_eq!(mode, "wal");
        assert_eq!(sync, 1); // NORMAL
        assert!(store.size_bytes() > 0);
    }

    #[test]
    fn reopen_keeps_version() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pooltime.db");
        drop(Store::open(&path).unwrap());
        let store = Store::open(&path).unwrap();
        assert_eq!(
            store.schema_version().unwrap(),
            super::super::SCHEMA_VERSION
        );
    }

    #[test]
    fn v1_database_upgrades_to_v2_keeping_rows() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pooltime.db");
        {
            let c = rusqlite::Connection::open(&path).unwrap();
            c.execute_batch(
                "CREATE TABLE account_state (account TEXT PRIMARY KEY, json TEXT);
                 INSERT INTO account_state VALUES ('a', '{}');
                 CREATE TABLE sessions (key_hash TEXT PRIMARY KEY, account TEXT, provider TEXT, last_seen INTEGER);
                 CREATE TABLE journal_requests (id TEXT PRIMARY KEY, ts INTEGER, model TEXT, provider TEXT, status INTEGER, outcome TEXT, ttfb_ms INTEGER, total_ms INTEGER, input_tokens INTEGER, output_tokens INTEGER, cache_read INTEGER, cache_write INTEGER, session_hash TEXT);
                 CREATE TABLE journal_attempts (request_id TEXT, n INTEGER, account TEXT, provider TEXT, outcome TEXT, reason TEXT, upstream_status INTEGER, upstream_request_id TEXT, ms INTEGER);
                 CREATE TABLE timeline (ts INTEGER, account TEXT, from_state TEXT, to_state TEXT, reason TEXT);
                 CREATE TABLE usage_samples (ts INTEGER, account TEXT, window TEXT, utilization REAL, reset INTEGER, granularity TEXT);
                 CREATE TABLE alerts_sent (key TEXT PRIMARY KEY, ts INTEGER);",
            )
            .unwrap();
            c.pragma_update(None, "user_version", 1).unwrap();
        }
        let store = Store::open(&path).unwrap();
        assert_eq!(
            store.schema_version().unwrap(),
            super::super::SCHEMA_VERSION
        );
        let (kept, quarantined): (i64, i64) = store
            .read(|c| {
                Ok((
                    c.query_row("SELECT count(*) FROM account_state", [], |r| r.get(0))?,
                    c.query_row("SELECT count(*) FROM account_state_quarantine", [], |r| {
                        r.get(0)
                    })?,
                ))
            })
            .unwrap();
        assert_eq!((kept, quarantined), (1, 0));
    }

    #[test]
    fn v2_database_upgrades_to_v3_journal_columns() {
        use crate::store::journal::{self, AttemptRow, RequestRow};
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pooltime.db");
        {
            let c = rusqlite::Connection::open(&path).unwrap();
            c.execute_batch(
                "CREATE TABLE journal_requests (id TEXT PRIMARY KEY, ts INTEGER, model TEXT, provider TEXT, status INTEGER, outcome TEXT, ttfb_ms INTEGER, total_ms INTEGER, input_tokens INTEGER, output_tokens INTEGER, cache_read INTEGER, cache_write INTEGER, session_hash TEXT);
                 CREATE TABLE journal_attempts (request_id TEXT, n INTEGER, account TEXT, provider TEXT, outcome TEXT, reason TEXT, upstream_status INTEGER, upstream_request_id TEXT, ms INTEGER);
                 CREATE TABLE account_state_quarantine (account TEXT, ts INTEGER, json TEXT);
                 CREATE TABLE usage_samples (ts INTEGER, account TEXT, window TEXT, utilization REAL, reset INTEGER, granularity TEXT);
                 INSERT INTO journal_requests(id, ts, outcome) VALUES ('old', 1, 'completed');",
            )
            .unwrap();
            c.pragma_update(None, "user_version", 2).unwrap();
        }
        let row = RequestRow {
            id: "new".into(),
            ts: 2,
            path: Some("/v1/messages/count_tokens".into()),
            outcome: "error".into(),
            reason: Some("unknown_path".into()),
            status: Some(404),
            ..RequestRow::default()
        };
        {
            let store = Store::open(&path).unwrap();
            assert_eq!(
                store.schema_version().unwrap(),
                super::super::SCHEMA_VERSION
            );
            let (r, a) = (row.clone(), Vec::<AttemptRow>::new());
            let rt = tokio::runtime::Builder::new_current_thread()
                .build()
                .unwrap();
            rt.block_on(store.exec(move |c| journal::insert(c, &r, &a)))
                .unwrap();
            let (got, _) = store.read(|c| journal::get(c, "new")).unwrap().unwrap();
            assert_eq!(got, row);
            let (old, _) = store.read(|c| journal::get(c, "old")).unwrap().unwrap();
            assert_eq!((old.path, old.reason), (None, None));
        }
        // A second open is a no-op: same version, rows intact.
        let store = Store::open(&path).unwrap();
        assert_eq!(
            store.schema_version().unwrap(),
            super::super::SCHEMA_VERSION
        );
        assert_eq!(store.read(journal::count).unwrap(), 2);
        assert_eq!(
            store.read(|c| journal::get(c, "new")).unwrap().unwrap().0,
            row
        );
    }

    #[test]
    fn open_rejects_newer_schema() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pooltime.db");
        {
            let c = rusqlite::Connection::open(&path).unwrap();
            c.pragma_update(None, "user_version", 99).unwrap();
        }
        assert!(Store::open(&path).is_err());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn exec_serializes_writes_from_many_tasks() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::open(&dir.path().join("pooltime.db")).unwrap());
        let mut tasks = Vec::new();
        for i in 0..100i64 {
            let s = store.clone();
            tasks.push(tokio::spawn(async move {
                s.exec(move |c| {
                    c.execute(
                        "INSERT INTO sessions(key_hash, account, provider, last_seen) VALUES (?1,'a','anthropic',?2)",
                        rusqlite::params![format!("k{i}"), i],
                    )?;
                    Ok(())
                })
                .await
            }));
        }
        for t in tasks {
            t.await.unwrap().unwrap();
        }
        let n: i64 = store
            .read(|c| Ok(c.query_row("SELECT count(*) FROM sessions", [], |r| r.get(0))?))
            .unwrap();
        assert_eq!(n, 100);
    }

    #[tokio::test]
    async fn submit_runs_in_order_without_waiting_and_exec_flushes() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("pooltime.db")).unwrap();
        for i in 0..50i64 {
            store
                .submit(move |c| {
                    c.execute(
                        "INSERT INTO alerts_sent(key, ts) VALUES ('k', ?1)
                         ON CONFLICT(key) DO UPDATE SET ts = excluded.ts",
                        [i],
                    )?;
                    Ok(())
                })
                .unwrap();
        }
        // failing and panicking jobs are swallowed and do not stop later ones
        store.submit(|_| anyhow::bail!("nope")).unwrap();
        store.submit(|_| panic!("boom")).unwrap();
        store.exec(|_| Ok(())).await.unwrap(); // flush
        let ts: i64 = store
            .read(
                |c| Ok(c.query_row("SELECT ts FROM alerts_sent WHERE key='k'", [], |r| r.get(0))?),
            )
            .unwrap();
        assert_eq!(ts, 49);
    }

    #[tokio::test]
    async fn exec_survives_closure_panic_and_error() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("pooltime.db")).unwrap();
        let r: anyhow::Result<()> = store.exec(|_| panic!("boom")).await;
        assert!(r.is_err());
        let r: anyhow::Result<()> = store.exec(|_| anyhow::bail!("nope")).await;
        assert!(r.is_err());
        let v = store.exec(|_| Ok(7)).await.unwrap();
        assert_eq!(v, 7);
    }
}
