//! The request journal (spec §6, §16): one `journal_requests` row plus its `journal_attempts`
//! chain per inbound request.
//!
//! [`Journal`] is the live, shared handle a request carries (`InboundRequest::journal`). The
//! handler, the stream pump's guard and the token-usage waiter each hold a clone; when the last
//! clone drops, the row is finalised and **queued** on the store writer ([`Store::submit`]), never
//! written on the request path. A journal write failure is logged once per failure streak and
//! never changes a response. If the handler future is cancelled (client gone) before anything set
//! an outcome, the row is finalised by `Drop` as `client_cancelled`, including an attempt that
//! was still in flight; that is never recorded on the account.
//!
//! Request outcome: `completed | client_cancelled | upstream_truncated | idle_timeout | reset |
//! error` (a streamed response takes its stream end; anything else is `completed` for a served
//! response and `error` otherwise). Attempt outcome: the same words for the serving attempt, and
//! `failover` for an attempt that handed over to the next account. `reason` is a classified
//! [`crate::errors::FailReason::detail`] (`quota`, `status:404`, `transport:reset`, ...) or a
//! fixed word (`no_account`, `unknown_path`, `invalid_request`).
//!
//! Privacy: no bodies, prompts, upstream error messages, tokens or raw `metadata.user_id` are
//! ever stored; the session is kept as [`crate::logging::hash_session`] (16 hex chars of
//! SHA-256), the upstream request id only when it is made of token-safe characters.
//!
//! Timing: `ts` is unix seconds at request start; `ttfb_ms` is the time until the response
//! headers were ready; `total_ms` runs until the row is finalised (the end of a streamed body);
//! an attempt's `ms` runs from the attempt start to its outcome (commit for a served attempt).

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Instant;

use anyhow::Context;
use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;
use time::OffsetDateTime;

use super::Store;
use crate::accounts::TokenUsage;

/// Request/stream outcomes (spec §6).
pub const OUTCOMES: [&str; 6] = [
    "completed",
    "client_cancelled",
    "upstream_truncated",
    "idle_timeout",
    "reset",
    "error",
];
/// Attempt outcome for an attempt that handed the request to the next account.
pub const FAILOVER: &str = "failover";
/// Outcomes that are not errors (the `errors_only` filter and the error rate exclude them).
const NOT_ERRORS: &str = "('completed','client_cancelled')";

/// One `journal_requests` row.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct RequestRow {
    pub id: String,
    /// Unix seconds at request start.
    pub ts: i64,
    pub path: Option<String>,
    pub model: Option<String>,
    /// The provider of the last attempt (the one that answered), else the routed provider.
    pub provider: Option<String>,
    /// HTTP status returned to the client.
    pub status: Option<u16>,
    pub outcome: String,
    pub reason: Option<String>,
    pub ttfb_ms: Option<i64>,
    pub total_ms: Option<i64>,
    pub input_tokens: Option<i64>,
    pub output_tokens: Option<i64>,
    pub cache_read: Option<i64>,
    pub cache_write: Option<i64>,
    pub session_hash: Option<String>,
}

/// One `journal_attempts` row.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct AttemptRow {
    /// 1-based position in the chain.
    pub n: u32,
    pub account: String,
    pub provider: String,
    pub outcome: String,
    pub reason: Option<String>,
    pub upstream_status: Option<u16>,
    pub upstream_request_id: Option<String>,
    pub ms: Option<i64>,
}

/// A list entry: the request plus a summary of its chain.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RequestSummary {
    #[serde(flatten)]
    pub request: RequestRow,
    pub attempts: u32,
    /// Account of the last attempt.
    pub account: Option<String>,
}

/// Per-account attempt statistics over a window.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct AccountStats {
    pub account: String,
    pub provider: String,
    pub attempts: u64,
    /// Attempts whose outcome is neither `completed` nor `client_cancelled`.
    pub errors: u64,
    pub error_rate: f64,
    /// Attempt latency (start to outcome) percentiles, over attempts that have one.
    pub p50_ms: Option<i64>,
    pub p95_ms: Option<i64>,
    /// The most frequent error outcome (ties: the alphabetically first); `None` without errors.
    pub top_error: Option<String>,
}

// ---- SQL ---------------------------------------------------------------------------------------

/// Insert (or replace) a request and its attempt chain in one transaction.
pub fn insert(
    conn: &mut Connection,
    r: &RequestRow,
    attempts: &[AttemptRow],
) -> anyhow::Result<()> {
    let tx = conn.transaction()?;
    tx.execute(
        "DELETE FROM journal_attempts WHERE request_id = ?1",
        [&r.id],
    )?;
    tx.execute(
        "INSERT OR REPLACE INTO journal_requests(id, ts, path, model, provider, status, outcome, reason,
            ttfb_ms, total_ms, input_tokens, output_tokens, cache_read, cache_write, session_hash)
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15)",
        params![
            r.id,
            r.ts,
            r.path,
            r.model,
            r.provider,
            r.status,
            r.outcome,
            r.reason,
            r.ttfb_ms,
            r.total_ms,
            r.input_tokens,
            r.output_tokens,
            r.cache_read,
            r.cache_write,
            r.session_hash
        ],
    )?;
    for a in attempts {
        tx.execute(
            "INSERT INTO journal_attempts(request_id, n, account, provider, outcome, reason,
                upstream_status, upstream_request_id, ms) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)",
            params![
                r.id,
                a.n,
                a.account,
                a.provider,
                a.outcome,
                a.reason,
                a.upstream_status,
                a.upstream_request_id,
                a.ms
            ],
        )?;
    }
    tx.commit()?;
    Ok(())
}

const REQUEST_COLS: &str = "r.id, r.ts, r.path, r.model, r.provider, r.status, r.outcome, r.reason,
    r.ttfb_ms, r.total_ms, r.input_tokens, r.output_tokens, r.cache_read, r.cache_write,
    r.session_hash";

fn request_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<RequestRow> {
    Ok(RequestRow {
        id: row.get(0)?,
        ts: row.get(1)?,
        path: row.get(2)?,
        model: row.get(3)?,
        provider: row.get(4)?,
        status: row.get(5)?,
        outcome: row.get::<_, Option<String>>(6)?.unwrap_or_default(),
        reason: row.get(7)?,
        ttfb_ms: row.get(8)?,
        total_ms: row.get(9)?,
        input_tokens: row.get(10)?,
        output_tokens: row.get(11)?,
        cache_read: row.get(12)?,
        cache_write: row.get(13)?,
        session_hash: row.get(14)?,
    })
}

/// The newest `limit` requests (newest first), optionally errors only.
pub fn list(
    conn: &Connection,
    errors_only: bool,
    limit: u32,
) -> anyhow::Result<Vec<RequestSummary>> {
    let filter = if errors_only {
        format!("WHERE r.outcome NOT IN {NOT_ERRORS}")
    } else {
        String::new()
    };
    let sql = format!(
        "SELECT {REQUEST_COLS},
            (SELECT count(*) FROM journal_attempts a WHERE a.request_id = r.id),
            (SELECT a.account FROM journal_attempts a WHERE a.request_id = r.id ORDER BY a.n DESC LIMIT 1)
         FROM journal_requests r {filter} ORDER BY r.ts DESC, r.rowid DESC LIMIT ?1"
    );
    let mut st = conn.prepare(&sql)?;
    let rows = st.query_map([limit], |row| {
        Ok(RequestSummary {
            request: request_row(row)?,
            attempts: row.get(15)?,
            account: row.get(16)?,
        })
    })?;
    Ok(rows.collect::<Result<_, _>>()?)
}

/// One request and its attempt chain (in order).
pub fn get(conn: &Connection, id: &str) -> anyhow::Result<Option<(RequestRow, Vec<AttemptRow>)>> {
    let sql = format!("SELECT {REQUEST_COLS} FROM journal_requests r WHERE r.id = ?1");
    let Some(r) = conn.query_row(&sql, [id], request_row).optional()? else {
        return Ok(None);
    };
    let mut st = conn.prepare(
        "SELECT n, account, provider, outcome, reason, upstream_status, upstream_request_id, ms
         FROM journal_attempts WHERE request_id = ?1 ORDER BY n",
    )?;
    let attempts = st
        .query_map([id], |row| {
            Ok(AttemptRow {
                n: row.get(0)?,
                account: row.get::<_, Option<String>>(1)?.unwrap_or_default(),
                provider: row.get::<_, Option<String>>(2)?.unwrap_or_default(),
                outcome: row.get::<_, Option<String>>(3)?.unwrap_or_default(),
                reason: row.get(4)?,
                upstream_status: row.get(5)?,
                upstream_request_id: row.get(6)?,
                ms: row.get(7)?,
            })
        })?
        .collect::<Result<_, _>>()?;
    Ok(Some((r, attempts)))
}

/// Per-account attempt counts, error rate and latency percentiles for requests since `since`
/// (unix seconds), ordered by account.
pub fn account_stats(conn: &Connection, since: i64) -> anyhow::Result<Vec<AccountStats>> {
    let mut st = conn.prepare(
        "SELECT a.account, a.provider, a.outcome, a.ms FROM journal_attempts a
         JOIN journal_requests r ON r.id = a.request_id
         WHERE r.ts >= ?1 ORDER BY a.account",
    )?;
    let rows = st.query_map([since], |row| {
        Ok((
            row.get::<_, Option<String>>(0)?.unwrap_or_default(),
            row.get::<_, Option<String>>(1)?.unwrap_or_default(),
            row.get::<_, Option<String>>(2)?.unwrap_or_default(),
            row.get::<_, Option<i64>>(3)?,
        ))
    })?;
    let mut out: Vec<(AccountStats, Vec<i64>, BTreeMap<String, u64>)> = Vec::new();
    for row in rows {
        let (account, provider, outcome, ms) = row?;
        if out.last().is_none_or(|(s, _, _)| s.account != account) {
            out.push((
                AccountStats {
                    account,
                    provider,
                    attempts: 0,
                    errors: 0,
                    error_rate: 0.0,
                    p50_ms: None,
                    p95_ms: None,
                    top_error: None,
                },
                Vec::new(),
                BTreeMap::new(),
            ));
        }
        let (s, lat, kinds) = out.last_mut().expect("pushed above");
        s.attempts += 1;
        if outcome != "completed" && outcome != "client_cancelled" {
            s.errors += 1;
            *kinds.entry(outcome).or_default() += 1;
        }
        lat.extend(ms);
    }
    Ok(out
        .into_iter()
        .map(|(mut s, mut lat, kinds)| {
            // `max_by_key` keeps the last maximum: reversed, that is the alphabetically first.
            s.top_error = kinds
                .into_iter()
                .rev()
                .max_by_key(|(_, n)| *n)
                .map(|(k, _)| k);
            s.error_rate = if s.attempts == 0 {
                0.0
            } else {
                s.errors as f64 / s.attempts as f64
            };
            lat.sort_unstable();
            s.p50_ms = percentile(&lat, 50);
            s.p95_ms = percentile(&lat, 95);
            s
        })
        .collect())
}

/// Nearest-rank percentile of sorted `v`.
fn percentile(v: &[i64], p: usize) -> Option<i64> {
    if v.is_empty() {
        return None;
    }
    let rank = (p * v.len()).div_ceil(100).max(1);
    v.get(rank - 1).copied()
}

/// Number of journal requests.
pub fn count(conn: &Connection) -> anyhow::Result<i64> {
    Ok(conn.query_row("SELECT count(*) FROM journal_requests", [], |r| r.get(0))?)
}

// ---- live handle -------------------------------------------------------------------------------

/// Set while journal writes are failing, so a failure streak logs once.
static WRITE_FAILING: AtomicBool = AtomicBool::new(false);

fn note_write(r: anyhow::Result<()>) {
    match r {
        Ok(()) => {
            if WRITE_FAILING.swap(false, Ordering::Relaxed) {
                tracing::info!("request journal writes recovered");
            }
        }
        Err(e) => {
            if !WRITE_FAILING.swap(true, Ordering::Relaxed) {
                tracing::warn!(error = %format!("{e:#}"), "request journal write failed; further failures are not logged until it recovers");
            }
        }
    }
}

/// Upstream request ids are kept only when token-safe (they are echoed into logs and the UI).
fn safe_upstream_id(id: &str) -> Option<String> {
    (!id.is_empty()
        && id.len() <= 128
        && id
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'_' | b'-' | b'.' | b':')))
    .then(|| id.to_string())
}

fn ms_since(t: Instant) -> i64 {
    i64::try_from(t.elapsed().as_millis()).unwrap_or(i64::MAX)
}

struct OpenAttempt {
    account: String,
    provider: String,
    started: Instant,
    upstream_status: Option<u16>,
    upstream_request_id: Option<String>,
}

#[derive(Default)]
struct Draft {
    row: RequestRow,
    attempts: Vec<AttemptRow>,
    open: Option<OpenAttempt>,
    /// Request-level outcome set when the response was produced.
    response_outcome: Option<&'static str>,
    /// End of the streamed body (wins over `response_outcome`).
    stream_outcome: Option<&'static str>,
    /// The provider whose response reached the client (wins over the last attempt's).
    served_provider: Option<String>,
}

struct Live {
    store: Arc<Store>,
    started: Instant,
    draft: Mutex<Draft>,
}

impl Live {
    fn lock(&self) -> MutexGuard<'_, Draft> {
        self.draft.lock().unwrap_or_else(|e| e.into_inner())
    }
}

impl Drop for Live {
    fn drop(&mut self) {
        let total = ms_since(self.started);
        let d = self.draft.get_mut().unwrap_or_else(|e| e.into_inner());
        if let Some(open) = d.open.take() {
            // The handler went away mid-attempt: the client disconnected.
            let n = d.attempts.len() as u32 + 1;
            d.attempts.push(AttemptRow {
                n,
                account: open.account,
                provider: open.provider,
                outcome: "client_cancelled".into(),
                reason: None,
                upstream_status: open.upstream_status,
                upstream_request_id: open.upstream_request_id,
                ms: Some(ms_since(open.started)),
            });
        }
        let outcome = d
            .stream_outcome
            .or(d.response_outcome)
            .unwrap_or("client_cancelled");
        let mut row = std::mem::take(&mut d.row);
        row.outcome = outcome.to_string();
        row.total_ms = Some(total);
        if let Some(p) = d.served_provider.take() {
            row.provider = Some(p);
        } else if let Some(last) = d.attempts.last() {
            row.provider = Some(last.provider.clone());
        }
        let attempts = std::mem::take(&mut d.attempts);
        let queued = self.store.submit(move |conn| {
            note_write(insert(conn, &row, &attempts).context("insert journal row"));
            Ok(())
        });
        if let Err(e) = queued {
            note_write(Err(e));
        }
    }
}

/// The live journal entry of one request (cheap to clone; a disabled handle does nothing).
#[derive(Clone, Default)]
pub struct Journal {
    live: Option<Arc<Live>>,
}

impl std::fmt::Debug for Journal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(if self.live.is_some() {
            "Journal(live)"
        } else {
            "Journal(off)"
        })
    }
}

impl Journal {
    /// Start the entry for request `id` at `now`. Written when the last clone drops.
    pub fn start(store: Arc<Store>, id: &str, now: OffsetDateTime) -> Journal {
        let draft = Draft {
            row: RequestRow {
                id: id.to_string(),
                ts: now.unix_timestamp(),
                ..RequestRow::default()
            },
            ..Draft::default()
        };
        Journal {
            live: Some(Arc::new(Live {
                store,
                started: Instant::now(),
                draft: Mutex::new(draft),
            })),
        }
    }

    /// A handle that records nothing (tests, internal callers).
    pub fn disabled() -> Journal {
        Journal::default()
    }

    fn with(&self, f: impl FnOnce(&mut Draft, Instant)) {
        if let Some(l) = &self.live {
            f(&mut l.lock(), l.started);
        }
    }

    /// The inbound path (callers strip the query string).
    pub fn set_path(&self, path: &str) {
        self.with(|d, _| d.row.path = Some(path.to_string()));
    }

    pub fn set_model(&self, model: &str) {
        self.with(|d, _| d.row.model = (!model.is_empty()).then(|| model.to_string()));
    }

    /// The routed provider (kept unless an attempt ran).
    pub fn set_provider(&self, provider: &str) {
        self.with(|d, _| d.row.provider = Some(provider.to_string()));
    }

    /// The provider whose response is returned (with cross-provider fallback this can be the
    /// first provider even though the last attempt ran on the other one).
    pub fn set_served_provider(&self, provider: &str) {
        self.with(|d, _| d.served_provider = Some(provider.to_string()));
    }

    /// `metadata.user_id`, stored only as its hash.
    pub fn set_session(&self, user_id: &str) {
        self.with(|d, _| d.row.session_hash = Some(crate::logging::hash_session(user_id)));
    }

    /// An attempt on `account` starts.
    pub fn begin_attempt(&self, account: &str, provider: &str) {
        self.with(|d, _| {
            d.open = Some(OpenAttempt {
                account: account.to_string(),
                provider: provider.to_string(),
                started: Instant::now(),
                upstream_status: None,
                upstream_request_id: None,
            })
        });
    }

    /// The current attempt's upstream answered with `status` (and maybe a request id).
    pub fn note_upstream(&self, status: u16, upstream_request_id: Option<&str>) {
        self.with(|d, _| {
            if let Some(a) = &mut d.open {
                a.upstream_status = Some(status);
                a.upstream_request_id = upstream_request_id.and_then(safe_upstream_id);
            }
        });
    }

    /// The current attempt ended with `outcome` (and a classified `reason`).
    pub fn end_attempt(&self, outcome: &str, reason: Option<String>) {
        self.with(|d, _| {
            let Some(open) = d.open.take() else { return };
            let n = d.attempts.len() as u32 + 1;
            // A committed stream may already have ended (its guard ran first): the serving
            // attempt takes the stream end, as `stream_end` would have given it.
            let outcome = match d.stream_outcome {
                Some(s) if outcome == "completed" => s,
                _ => outcome,
            };
            d.attempts.push(AttemptRow {
                n,
                account: open.account,
                provider: open.provider,
                outcome: outcome.to_string(),
                reason,
                upstream_status: open.upstream_status,
                upstream_request_id: open.upstream_request_id,
                ms: Some(ms_since(open.started)),
            });
        });
    }

    /// The response to the client is ready: its status, request outcome and reason. TTFB is
    /// measured here.
    pub fn respond(&self, status: u16, outcome: &'static str, reason: Option<String>) {
        self.with(|d, started| {
            d.row.status = Some(status);
            d.row.ttfb_ms.get_or_insert(ms_since(started));
            d.response_outcome = Some(outcome);
            if reason.is_some() {
                d.row.reason = reason;
            }
        });
    }

    /// A streamed body ended with `outcome`; it becomes the request's and the serving attempt's.
    pub fn stream_end(&self, outcome: &'static str) {
        self.with(|d, _| {
            d.stream_outcome = Some(outcome);
            if let Some(a) = d.attempts.last_mut() {
                if a.outcome == "completed" {
                    a.outcome = outcome.to_string();
                }
            }
            if outcome != "completed" && outcome != "client_cancelled" {
                d.row
                    .reason
                    .get_or_insert_with(|| format!("stream:{outcome}"));
            }
        });
    }

    pub fn tokens(&self, u: TokenUsage) {
        self.with(|d, _| {
            d.row.input_tokens = Some(i64::try_from(u.input_tokens).unwrap_or(i64::MAX));
            d.row.output_tokens = Some(i64::try_from(u.output_tokens).unwrap_or(i64::MAX));
            d.row.cache_read = Some(i64::try_from(u.cache_read).unwrap_or(i64::MAX));
            d.row.cache_write = Some(i64::try_from(u.cache_write).unwrap_or(i64::MAX));
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percentiles_nearest_rank() {
        assert_eq!(percentile(&[], 50), None);
        assert_eq!(percentile(&[7], 95), Some(7));
        let v: Vec<i64> = (1..=100).collect();
        assert_eq!(percentile(&v, 50), Some(50));
        assert_eq!(percentile(&v, 95), Some(95));
        assert_eq!(percentile(&[1, 2, 3], 50), Some(2));
    }

    #[test]
    fn upstream_ids_must_be_token_safe() {
        assert_eq!(
            safe_upstream_id("req_01-AB.c:d").as_deref(),
            Some("req_01-AB.c:d")
        );
        assert_eq!(safe_upstream_id("has spaces"), None);
        assert_eq!(safe_upstream_id(""), None);
        assert_eq!(safe_upstream_id(&"x".repeat(129)), None);
    }

    #[tokio::test]
    async fn stream_end_before_end_attempt_still_lands_on_the_attempt() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::open(&dir.path().join("pooltime.db")).unwrap());
        let j = Journal::start(store.clone(), "req_race", OffsetDateTime::now_utc());
        j.begin_attempt("a1", "anthropic");
        // The pump's guard drops before the handler records the attempt.
        j.stream_end("idle_timeout");
        j.end_attempt("completed", None);
        j.set_served_provider("openai");
        j.respond(200, "completed", None);
        drop(j);
        store.exec(|_| Ok(())).await.unwrap();
        let (r, a) = store.read(|c| get(c, "req_race")).unwrap().unwrap();
        assert_eq!(r.outcome, "idle_timeout");
        assert_eq!(a[0].outcome, "idle_timeout");
        // The served provider wins over the last attempt's.
        assert_eq!(r.provider.as_deref(), Some("openai"));
    }

    #[tokio::test]
    async fn dropped_mid_attempt_is_client_cancelled() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::open(&dir.path().join("pooltime.db")).unwrap());
        let j = Journal::start(store.clone(), "req_x", OffsetDateTime::now_utc());
        j.set_model("m");
        j.begin_attempt("a1", "anthropic");
        j.note_upstream(200, Some("req_up"));
        drop(j);
        store.exec(|_| Ok(())).await.unwrap();
        let (r, a) = store.read(|c| get(c, "req_x")).unwrap().unwrap();
        assert_eq!(r.outcome, "client_cancelled");
        assert_eq!(a.len(), 1);
        assert_eq!(a[0].outcome, "client_cancelled");
        assert_eq!(a[0].upstream_request_id.as_deref(), Some("req_up"));
    }
}
