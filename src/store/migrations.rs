use anyhow::{bail, Context};
use rusqlite::Connection;

pub const SCHEMA_VERSION: u32 = 1;

/// Schema v1. Timestamps are unix seconds. `utilization` is a 0-1 fraction.
/// `usage_samples.granularity` is `hourly` (raw) | `daily` | `weekly`.
const V1: &str = "
CREATE TABLE account_state (
    account TEXT PRIMARY KEY,
    json    TEXT
);
CREATE TABLE sessions (
    key_hash  TEXT PRIMARY KEY,
    account   TEXT,
    provider  TEXT,
    last_seen INTEGER
);
CREATE TABLE journal_requests (
    id            TEXT PRIMARY KEY,
    ts            INTEGER,
    model         TEXT,
    provider      TEXT,
    status        INTEGER,
    outcome       TEXT,
    ttfb_ms       INTEGER,
    total_ms      INTEGER,
    input_tokens  INTEGER,
    output_tokens INTEGER,
    cache_read    INTEGER,
    cache_write   INTEGER,
    session_hash  TEXT
);
CREATE INDEX journal_requests_ts ON journal_requests(ts);
CREATE TABLE journal_attempts (
    request_id          TEXT,
    n                   INTEGER,
    account             TEXT,
    provider            TEXT,
    outcome             TEXT,
    reason              TEXT,
    upstream_status     INTEGER,
    upstream_request_id TEXT,
    ms                  INTEGER
);
CREATE INDEX journal_attempts_request ON journal_attempts(request_id);
CREATE TABLE timeline (
    ts         INTEGER,
    account    TEXT,
    from_state TEXT,
    to_state   TEXT,
    reason     TEXT
);
CREATE INDEX timeline_ts ON timeline(ts);
CREATE TABLE usage_samples (
    ts          INTEGER,
    account     TEXT,
    window      TEXT,
    utilization REAL,
    reset       INTEGER,
    granularity TEXT
);
CREATE INDEX usage_samples_account_window_ts ON usage_samples(account, window, ts);
CREATE TABLE alerts_sent (
    key TEXT PRIMARY KEY,
    ts  INTEGER
);
";

/// Bring the database to [`SCHEMA_VERSION`]. Each step runs in one transaction
/// together with its `user_version` bump.
pub(super) fn migrate(conn: &mut Connection) -> anyhow::Result<()> {
    let current: u32 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    if current > SCHEMA_VERSION {
        bail!("database schema v{current} is newer than this pooltime (v{SCHEMA_VERSION})");
    }
    if current < 1 {
        let tx = conn.transaction()?;
        tx.execute_batch(V1).context("apply schema v1")?;
        tx.pragma_update(None, "user_version", 1)?;
        tx.commit()?;
    }
    Ok(())
}
