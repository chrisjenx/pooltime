//! Per-account runtime state, status derivation and persistence (INV §2-§3, §10.1).
//!
//! [`StateBook`] is the in-memory map of [`AccountState`] with write-through to the
//! `account_state` table. Status is *derived* from state on demand ([`AccountStatus`]);
//! every mutator compares the derived status before and after the change and, when it
//! moved, emits a [`Transition`] (broadcast + `tracing` line + `timeline` row).
//!
//! # Persistence and ordering
//!
//! All methods are synchronous and need no async runtime. Writes go through
//! [`Store::submit`], a fire-and-forget enqueue onto the store's single writer thread:
//! the enqueue happens while the book's mutex is held, so database order always equals
//! in-memory mutation order. Tests (and shutdown) call [`StateBook::flush`], which waits
//! until everything queued so far has been written.
//!
//! # Time
//!
//! Callers pass `now` explicitly (deterministic tests). Snapshot mutators use the
//! snapshot's own `updated_at`, and `record_error` uses `ErrorRecord::at`, as "now".
//! Cooldowns that lapse with no mutation are surfaced by [`StateBook::reevaluate`]
//! (and implicitly at the start of the next mutation, reason `cooldown_expired`).

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use time::{Duration, OffsetDateTime};
use tokio::sync::broadcast;

use super::id::AccountId;
use crate::errors::FailReason;
use crate::store::Store;

/// Broadcast channel capacity for [`Transition`] subscribers.
const TRANSITION_CHANNEL_CAPACITY: usize = 256;
/// Default access-denied / billing recheck cooldown (config `timeouts.access_denied_cooldown`).
const DEFAULT_ACCESS_DENIED_COOLDOWN: Duration = Duration::hours(1);
/// Default rolling counter window (config `timeouts.usage_window`).
const DEFAULT_USAGE_WINDOW: Duration = Duration::hours(5);

// ---- rate-limit windows ---------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WindowStatus {
    Allowed,
    AllowedWarning,
    Rejected,
    Unknown,
}

/// One rate-limit window, e.g. `5h`, `7d`, `7d-fable` (model-scoped) or `overage`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Window {
    pub key: String,
    /// Model family for model-scoped windows (`7d-fable` -> `fable`); `None` = account-wide.
    pub model: Option<String>,
    pub status: WindowStatus,
    /// 0..=1 fraction.
    pub utilization: f64,
    #[serde(default, with = "time::serde::rfc3339::option")]
    pub reset: Option<OffsetDateTime>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RateLimitSnapshot {
    pub unified_status: Option<String>,
    pub windows: Vec<Window>,
    #[serde(with = "time::serde::rfc3339")]
    pub updated_at: OffsetDateTime,
}

fn is_duration_token(token: &str) -> Option<Duration> {
    let token = token.to_ascii_lowercase();
    let split = token.find(|c: char| !c.is_ascii_digit())?;
    if split == 0 {
        return None;
    }
    let n: i64 = token[..split].parse().ok()?;
    let unit = match &token[split..] {
        "min" | "m" => Duration::MINUTE,
        "h" | "hr" | "hrs" => Duration::HOUR,
        "d" | "day" | "days" => Duration::DAY,
        "w" | "wk" => Duration::WEEK,
        "mo" => Duration::days(30),
        _ => return None,
    };
    unit.checked_mul(i32::try_from(n).ok()?)
}

/// Duration encoded by a window key's duration token (`5h`, `7d-fable`, `fable_7d`), or
/// `None` for keys without one (`overage`). Units: min/m, h/hr/hrs, d/day/days, w/wk, mo (30 d).
pub fn window_duration(key: &str) -> Option<Duration> {
    key.split(['-', '_']).find_map(is_duration_token)
}

/// A window that will 429 if routed to: explicitly rejected, or fully used without an
/// explicit `allowed` (Codex reports 100% with `allowed` while a limit is unenforced).
pub(crate) fn is_spent(w: &Window) -> bool {
    match w.status {
        WindowStatus::Rejected => true,
        WindowStatus::Allowed => false,
        WindowStatus::AllowedWarning | WindowStatus::Unknown => w.utilization >= 1.0,
    }
}

/// Spent account-wide windows whose reset is still in the future, as `(reset, key)`.
fn spent_account_windows(
    snap: &RateLimitSnapshot,
    now: OffsetDateTime,
) -> impl Iterator<Item = (OffsetDateTime, &str)> {
    snap.windows
        .iter()
        .filter(|w| w.model.is_none() && is_spent(w))
        .filter_map(move |w| w.reset.filter(|r| *r > now).map(|r| (r, w.key.as_str())))
}

/// Account-wide windows first, then model-scoped; each group by key.
fn sort_windows(windows: &mut [Window]) {
    windows.sort_by(|a, b| {
        a.model
            .is_some()
            .cmp(&b.model.is_some())
            .then_with(|| a.key.cmp(&b.key))
    });
}

/// Per-key merge: every window `next` reports wins; windows it omits are kept.
fn merge_snapshot(prev: Option<&RateLimitSnapshot>, next: RateLimitSnapshot) -> RateLimitSnapshot {
    let mut windows: Vec<Window> = prev.map(|p| p.windows.clone()).unwrap_or_default();
    for w in next.windows {
        match windows.iter_mut().find(|x| x.key == w.key) {
            Some(slot) => *slot = w,
            None => windows.push(w),
        }
    }
    sort_windows(&mut windows);
    RateLimitSnapshot {
        unified_status: next
            .unified_status
            .or_else(|| prev.and_then(|p| p.unified_status.clone())),
        windows,
        updated_at: next.updated_at,
    }
}

/// `next` is whole-account ground truth: duration-keyed windows are replaced wholesale
/// (a window it no longer reports is dropped); non-duration windows (`overage`) carry over.
/// A spent window with no reset gets `updated_at + duration` so it blocks until it should.
fn replace_snapshot(
    prev: Option<&RateLimitSnapshot>,
    mut next: RateLimitSnapshot,
) -> RateLimitSnapshot {
    for w in &mut next.windows {
        if w.reset.is_none() && is_spent(w) {
            if let Some(d) = window_duration(&w.key) {
                w.reset = Some(next.updated_at + d);
            }
        }
    }
    let mut windows: Vec<Window> = prev
        .map(|p| {
            p.windows
                .iter()
                .filter(|w| window_duration(&w.key).is_none())
                .cloned()
                .collect()
        })
        .unwrap_or_default();
    for w in next.windows {
        windows.retain(|x| x.key != w.key);
        windows.push(w);
    }
    sort_windows(&mut windows);
    RateLimitSnapshot {
        unified_status: next.unified_status,
        windows,
        updated_at: next.updated_at,
    }
}

// ---- status ---------------------------------------------------------------

/// Derived availability of an account. Precedence: `NeedsLogin` > `Billing` >
/// `AccessDenied` > `RateLimited` > `Ready`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum AccountStatus {
    Ready,
    NeedsLogin,
    RateLimited {
        #[serde(with = "time::serde::rfc3339")]
        until: OffsetDateTime,
        window: Option<String>,
    },
    Billing,
    AccessDenied {
        #[serde(with = "time::serde::rfc3339")]
        until: OffsetDateTime,
    },
}

impl AccountStatus {
    /// Stable label (spec §16): `ready | needs_login | rate_limited | billing | access_denied`.
    pub fn label(&self) -> &'static str {
        match self {
            AccountStatus::Ready => "ready",
            AccountStatus::NeedsLogin => "needs_login",
            AccountStatus::RateLimited { .. } => "rate_limited",
            AccountStatus::Billing => "billing",
            AccountStatus::AccessDenied { .. } => "access_denied",
        }
    }
}

/// A change of [`AccountStatus`] (label level; a cooldown merely being extended is not one).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Transition {
    pub at: OffsetDateTime,
    pub account: AccountId,
    pub from: AccountStatus,
    pub to: AccountStatus,
    /// Short classified reason (never upstream text), e.g. `rate_limited`, `cooldown_expired`.
    pub reason: String,
}

// ---- state ------------------------------------------------------------------

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read: u64,
    pub cache_write: u64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Counters {
    /// Attempts that reached the upstream: `successes + errors`.
    pub requests: u64,
    pub successes: u64,
    pub errors: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read: u64,
    pub cache_write: u64,
}

impl Counters {
    fn add_success(&mut self, u: &TokenUsage) {
        self.requests = self.requests.saturating_add(1);
        self.successes = self.successes.saturating_add(1);
        self.input_tokens = self.input_tokens.saturating_add(u.input_tokens);
        self.output_tokens = self.output_tokens.saturating_add(u.output_tokens);
        self.cache_read = self.cache_read.saturating_add(u.cache_read);
        self.cache_write = self.cache_write.saturating_add(u.cache_write);
    }

    fn add_error(&mut self) {
        self.requests = self.requests.saturating_add(1);
        self.errors = self.errors.saturating_add(1);
    }
}

/// Outcome of the last ground-truth usage poll.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsageCheck {
    #[serde(with = "time::serde::rfc3339")]
    pub at: OffsetDateTime,
    pub ok: bool,
    /// Classified label (e.g. `timeout`, `http_429`); never an upstream message.
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ErrorRecord {
    #[serde(with = "time::serde::rfc3339")]
    pub at: OffsetDateTime,
    pub reason: FailReason,
    pub upstream_request_id: Option<String>,
}

/// Everything persisted per account (one JSON document in `account_state.json`).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AccountState {
    pub snapshot: Option<RateLimitSnapshot>,
    #[serde(with = "time::serde::rfc3339::option")]
    pub rate_limited_until: Option<OffsetDateTime>,
    pub rate_limited_window: Option<String>,
    #[serde(with = "time::serde::rfc3339::option")]
    pub access_denied_until: Option<OffsetDateTime>,
    #[serde(with = "time::serde::rfc3339::option")]
    pub billing_blocked_at: Option<OffsetDateTime>,
    /// `sha256(rejected refresh token)[..16]` (hex); a re-login clears it.
    pub dead_refresh_fp: Option<String>,
    /// Bumped on success only.
    #[serde(with = "time::serde::rfc3339::option")]
    pub last_used_at: Option<OffsetDateTime>,
    #[serde(with = "time::serde::rfc3339::option")]
    pub last_success_at: Option<OffsetDateTime>,
    pub last_error: Option<ErrorRecord>,
    pub usage_check: Option<UsageCheck>,
    /// Rolling-window counters; the window opened at `window_started_at`.
    pub window_counters: Counters,
    #[serde(with = "time::serde::rfc3339::option")]
    pub window_started_at: Option<OffsetDateTime>,
    pub lifetime: Counters,
}

/// `sha256(token)` hex, first 16 chars. Non-reversible; safe to persist.
fn fingerprint(token: &str) -> String {
    let mut s = hex::encode(Sha256::digest(token.as_bytes()));
    s.truncate(16);
    s
}

/// Derive the status of `s` at `now`.
fn derive(s: &AccountState, creds_ok: bool, now: OffsetDateTime) -> AccountStatus {
    if !creds_ok || s.dead_refresh_fp.is_some() {
        return AccountStatus::NeedsLogin;
    }
    if let Some(until) = s.access_denied_until.filter(|u| *u > now) {
        return if s.billing_blocked_at.is_some() {
            AccountStatus::Billing
        } else {
            AccountStatus::AccessDenied { until }
        };
    }
    // Rate limited while an explicit cooldown or any spent account-wide window is live;
    // report the latest of them (the account is usable only once all have passed).
    let explicit = s
        .rate_limited_until
        .filter(|u| *u > now)
        .map(|u| (u, s.rate_limited_window.clone()));
    let spent = s.snapshot.as_ref().and_then(|snap| {
        spent_account_windows(snap, now)
            .max_by_key(|(r, _)| *r)
            .map(|(r, k)| (r, Some(k.to_string())))
    });
    match [explicit, spent]
        .into_iter()
        .flatten()
        .max_by_key(|(u, _)| *u)
    {
        Some((until, window)) => AccountStatus::RateLimited { until, window },
        None => AccountStatus::Ready,
    }
}

/// Counters as of `now`: an expired rolling window reads as empty.
fn roll_view(s: &mut AccountState, now: OffsetDateTime, window: Duration) {
    if s.window_started_at.is_some_and(|st| now - st >= window) {
        s.window_counters = Counters::default();
        s.window_started_at = None;
    }
}

/// Open a fresh counter window when the current one is missing or has elapsed.
fn roll_window(s: &mut AccountState, now: OffsetDateTime, window: Duration) {
    if s.window_started_at.is_none_or(|st| now - st >= window) {
        s.window_counters = Counters::default();
        s.window_started_at = Some(now);
    }
}

// ---- book -------------------------------------------------------------------

struct Entry {
    state: AccountState,
    /// Status as of the last evaluation; the "before" of the next transition check.
    last_status: AccountStatus,
    /// Last credentials verdict from [`StateBook::reevaluate`] (mutators reuse it).
    creds_ok: bool,
}

impl Entry {
    fn new(state: AccountState, now: OffsetDateTime) -> Entry {
        let last_status = derive(&state, true, now);
        Entry {
            state,
            last_status,
            creds_ok: true,
        }
    }
}

/// In-memory account state with write-through persistence. Share via `Arc<StateBook>`.
pub struct StateBook {
    store: Arc<Store>,
    inner: Mutex<HashMap<AccountId, Entry>>,
    tx: broadcast::Sender<Transition>,
    access_denied_cooldown: Duration,
    usage_window: Duration,
}

impl StateBook {
    /// Load every persisted row. Rows that fail to parse are skipped with an `error` log
    /// (the account then starts from empty state) rather than failing startup.
    pub fn load(store: Arc<Store>) -> anyhow::Result<StateBook> {
        Self::load_at(store, OffsetDateTime::now_utc())
    }

    /// [`load`](Self::load) with an explicit clock (loading never emits transitions).
    pub fn load_at(store: Arc<Store>, now: OffsetDateTime) -> anyhow::Result<StateBook> {
        let rows: Vec<(String, Option<String>)> = store.read(|c| {
            let mut st = c.prepare("SELECT account, json FROM account_state")?;
            let rows = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
            Ok(rows.collect::<Result<_, _>>()?)
        })?;
        let mut map = HashMap::new();
        for (account, json) in rows {
            let Ok(id) = account.parse::<AccountId>() else {
                tracing::error!(account = %account, "skipping persisted state for invalid account id");
                continue;
            };
            let state = match json.as_deref().map(serde_json::from_str::<AccountState>) {
                Some(Ok(s)) => s,
                Some(Err(e)) => {
                    tracing::error!(account = %id, error = %e, "corrupt persisted account state; starting empty");
                    continue;
                }
                None => continue,
            };
            map.insert(id, Entry::new(state, now));
        }
        let (tx, _) = broadcast::channel(TRANSITION_CHANNEL_CAPACITY);
        Ok(StateBook {
            store,
            inner: Mutex::new(map),
            tx,
            access_denied_cooldown: DEFAULT_ACCESS_DENIED_COOLDOWN,
            usage_window: DEFAULT_USAGE_WINDOW,
        })
    }

    /// Cooldown applied by `mark_access_denied` / `mark_billing` (default 1 h).
    pub fn with_access_denied_cooldown(mut self, d: std::time::Duration) -> Self {
        self.access_denied_cooldown = Duration::try_from(d).unwrap_or(Duration::MAX);
        self
    }

    /// Length of the rolling counter window (default 5 h).
    pub fn with_usage_window(mut self, d: std::time::Duration) -> Self {
        self.usage_window = Duration::try_from(d).unwrap_or(Duration::MAX);
        self
    }

    /// Subscribe to status transitions (capacity 256; slow receivers see `Lagged`).
    pub fn transitions(&self) -> broadcast::Receiver<Transition> {
        self.tx.subscribe()
    }

    /// Wait until every queued write (state rows and timeline rows) is on disk.
    pub async fn flush(&self) -> anyhow::Result<()> {
        self.store.exec(|_| Ok(())).await
    }

    fn lock(&self) -> MutexGuard<'_, HashMap<AccountId, Entry>> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    // ---- queries (pure) ----

    /// Derived status at `now`. `creds_ok` is the caller's verdict on the stored credentials
    /// (missing/corrupt -> `false` -> `NeedsLogin`). Does not emit transitions.
    pub fn status(&self, id: &AccountId, creds_ok: bool, now: OffsetDateTime) -> AccountStatus {
        match self.lock().get(id) {
            Some(e) => derive(&e.state, creds_ok, now),
            None => derive(&AccountState::default(), creds_ok, now),
        }
    }

    /// Model families whose model-scoped window is spent with a future reset. They never
    /// change the account status; routing excludes the account for those families only.
    pub fn spent_families(&self, id: &AccountId, now: OffsetDateTime) -> Vec<String> {
        let map = self.lock();
        let Some(snap) = map.get(id).and_then(|e| e.state.snapshot.as_ref()) else {
            return Vec::new();
        };
        let mut fams: Vec<String> = snap
            .windows
            .iter()
            .filter(|w| is_spent(w) && w.reset.is_some_and(|r| r > now))
            .filter_map(|w| w.model.clone())
            .collect();
        fams.sort();
        fams.dedup();
        fams
    }

    /// Earliest future reset among the account-wide windows the stored snapshot reports spent,
    /// with its key (bro `blockingWindowReset`).
    pub fn blocking_window(
        &self,
        id: &AccountId,
        now: OffsetDateTime,
    ) -> Option<(OffsetDateTime, String)> {
        let map = self.lock();
        let snap = map.get(id)?.state.snapshot.as_ref()?;
        spent_account_windows(snap, now)
            .min_by_key(|(r, _)| *r)
            .map(|(r, key)| (r, key.to_string()))
    }

    /// Copy of the account's state (default for unknown accounts); an elapsed rolling
    /// counter window reads as empty.
    pub fn state(&self, id: &AccountId, now: OffsetDateTime) -> AccountState {
        let mut s = self
            .lock()
            .get(id)
            .map(|e| e.state.clone())
            .unwrap_or_default();
        roll_view(&mut s, now, self.usage_window);
        s
    }

    /// Re-derive the status with the given credentials verdict, emitting a transition if it
    /// moved (e.g. a cooldown ended, creds vanished or came back). Call periodically.
    pub fn reevaluate(&self, id: &AccountId, creds_ok: bool, now: OffsetDateTime) -> AccountStatus {
        let mut map = self.lock();
        let e = map
            .entry(id.clone())
            .or_insert_with(|| Entry::new(AccountState::default(), now));
        let reason = if e.creds_ok != creds_ok {
            if creds_ok {
                "credentials_restored"
            } else {
                "credentials_unavailable"
            }
        } else {
            "cooldown_expired"
        };
        e.creds_ok = creds_ok;
        self.settle(id, e, now, reason);
        e.last_status.clone()
    }

    // ---- mutators ----

    /// Drop everything known about a deleted account (memory and its persisted state row), so
    /// re-adding the same id starts clean. Timeline rows are history and stay.
    pub fn forget(&self, id: &AccountId) {
        self.lock().remove(id);
        let account = id.to_string();
        let queued = self.store.submit(move |c| {
            c.execute("DELETE FROM account_state WHERE account = ?1", [account])?;
            Ok(())
        });
        if let Err(err) = queued {
            tracing::error!(account = %id, error = %err, "could not queue account state delete");
        }
    }

    /// Sideline until `until` (explicit 429 cooldown). `window` names the binding window if known.
    pub fn mark_rate_limited(
        &self,
        id: &AccountId,
        until: OffsetDateTime,
        window: Option<String>,
        now: OffsetDateTime,
    ) {
        self.mutate(id, now, "rate_limited", |s| {
            s.rate_limited_until = Some(until);
            s.rate_limited_window = window;
        });
    }

    /// Non-entitlement refusal (403 / Codex 401-403 after refresh): cooldown, not billing.
    pub fn mark_access_denied(&self, id: &AccountId, now: OffsetDateTime) {
        let cooldown = self.access_denied_cooldown;
        self.mutate(id, now, "access_denied", |s| {
            s.billing_blocked_at = None;
            s.access_denied_until = Some(now + cooldown);
        });
    }

    /// Entitlement 403: billing block with a recheck cooldown. Re-marking keeps the
    /// original `billing_blocked_at` and refreshes the cooldown.
    pub fn mark_billing(&self, id: &AccountId, now: OffsetDateTime) {
        let cooldown = self.access_denied_cooldown;
        self.mutate(id, now, "billing", |s| {
            s.billing_blocked_at.get_or_insert(now);
            s.access_denied_until = Some(now + cooldown);
            s.usage_check = Some(UsageCheck {
                at: now,
                ok: true,
                error: None,
            });
        });
    }

    /// Manual recheck: drop a billing block and its cooldown. A plain access-denied
    /// cooldown is left alone.
    pub fn clear_billing(&self, id: &AccountId, now: OffsetDateTime) {
        self.mutate(id, now, "billing_cleared", |s| {
            if s.billing_blocked_at.is_some() {
                s.billing_blocked_at = None;
                s.access_denied_until = None;
                s.usage_check = None;
            }
        });
    }

    /// The OAuth endpoint permanently rejected `rejected_refresh_token`. Only its
    /// fingerprint is stored (takes the token, not the account's current creds, so a
    /// re-login that raced the rejection is not sidelined).
    pub fn mark_refresh_dead(
        &self,
        id: &AccountId,
        rejected_refresh_token: &str,
        now: OffsetDateTime,
    ) {
        let fp = fingerprint(rejected_refresh_token);
        self.mutate(id, now, "refresh_token_dead", |s| {
            s.dead_refresh_fp = Some(fp);
        });
    }

    /// Whether `token` is the refresh token last rejected for `id` (fingerprint compare), so
    /// callers can skip a POST that is known to fail.
    pub fn is_refresh_dead(&self, id: &AccountId, token: &str) -> bool {
        self.lock()
            .get(id)
            .and_then(|e| e.state.dead_refresh_fp.as_deref())
            .is_some_and(|fp| fp == fingerprint(token))
    }

    /// A fresh login replaced the dead token.
    pub fn clear_refresh_dead(&self, id: &AccountId, now: OffsetDateTime) {
        self.mutate(id, now, "refresh_token_replaced", |s| {
            s.dead_refresh_fp = None;
        });
    }

    /// Merge response-header windows per key (absent windows are presumed still in effect).
    pub fn merge_header_snapshot(&self, id: &AccountId, s: RateLimitSnapshot) {
        let now = s.updated_at;
        self.mutate(id, now, "header_snapshot", |st| {
            st.snapshot = Some(merge_snapshot(st.snapshot.as_ref(), s));
        });
    }

    /// Replace the duration-keyed windows with a whole-account header snapshot (Codex
    /// `x-codex-*`, bro replace mode): a window it no longer reports is dropped, non-duration
    /// windows carry over. Unlike [`StateBook::replace_poll_snapshot`] nothing but the snapshot
    /// changes. Callers skip empty snapshots (they would wipe the known windows).
    pub fn replace_header_snapshot(&self, id: &AccountId, s: RateLimitSnapshot) {
        let now = s.updated_at;
        self.mutate(id, now, "header_snapshot", |st| {
            st.snapshot = Some(replace_snapshot(st.snapshot.as_ref(), s));
        });
    }

    /// Apply a ground-truth usage poll that started at `started_at` (answered at `s.updated_at`).
    ///
    /// Replaces duration-keyed windows (keeps e.g. `overage`); a spent account-wide window
    /// extends `rate_limited_until` (extend-only); if nothing blocks, a stale cooldown is
    /// dropped; a billing block is cleared only by a poll that started at or after it.
    pub fn replace_poll_snapshot(
        &self,
        id: &AccountId,
        s: RateLimitSnapshot,
        started_at: OffsetDateTime,
    ) {
        let now = s.updated_at;
        self.mutate(id, now, "poll_snapshot", |st| {
            let snap = replace_snapshot(st.snapshot.as_ref(), s);
            st.usage_check = Some(UsageCheck {
                at: now,
                ok: true,
                error: None,
            });
            if st.billing_blocked_at.is_some_and(|b| b <= started_at) {
                st.billing_blocked_at = None;
                st.access_denied_until = None;
            }
            match spent_account_windows(&snap, now).min_by_key(|(r, _)| *r) {
                Some((reset, key)) => {
                    if st.rate_limited_until.is_none_or(|u| reset > u) {
                        st.rate_limited_until = Some(reset);
                        st.rate_limited_window = Some(key.to_string());
                    }
                }
                None => {
                    st.rate_limited_until = None;
                    st.rate_limited_window = None;
                    if matches!(
                        st.last_error.as_ref().map(|e| e.reason),
                        Some(FailReason::Quota { .. })
                    ) {
                        st.last_error = None;
                    }
                }
            }
            st.snapshot = Some(snap);
        });
    }

    /// Record that the usage poll failed (`label` is a classified reason, never upstream text).
    pub fn record_usage_check_error(&self, id: &AccountId, at: OffsetDateTime, label: &str) {
        let label = label.to_string();
        self.mutate(id, at, "usage_check_failed", |s| {
            s.usage_check = Some(UsageCheck {
                at,
                ok: false,
                error: Some(label),
            });
        });
    }

    /// A request was served: counts it, bumps `last_used_at`/`last_success_at`, clears
    /// `last_error`, the access-denied cooldown and any billing block (serving proves entitlement).
    pub fn record_success(&self, id: &AccountId, usage: TokenUsage, now: OffsetDateTime) {
        let window = self.usage_window;
        self.mutate(id, now, "success", |s| {
            roll_window(s, now, window);
            s.window_counters.add_success(&usage);
            s.lifetime.add_success(&usage);
            s.last_used_at = Some(now);
            s.last_success_at = Some(now);
            s.last_error = None;
            s.access_denied_until = None;
            s.billing_blocked_at = None;
        });
    }

    /// A failed attempt: counts it and records `last_error`. Does not bump `last_used_at`
    /// and does not change status by itself (use the `mark_*` calls for that).
    pub fn record_error(&self, id: &AccountId, e: ErrorRecord) {
        let window = self.usage_window;
        let now = e.at;
        self.mutate(id, now, "error", |s| {
            roll_window(s, now, window);
            s.window_counters.add_error();
            s.lifetime.add_error();
            s.last_error = Some(e);
        });
    }

    // ---- internals ----

    /// Apply `f` to the account's state under the lock: surface any time-based transition
    /// first, run `f`, surface the transition it caused (labelled `reason`), then persist
    /// the state if it changed.
    fn mutate(
        &self,
        id: &AccountId,
        now: OffsetDateTime,
        reason: &str,
        f: impl FnOnce(&mut AccountState),
    ) {
        let mut map = self.lock();
        let e = map
            .entry(id.clone())
            .or_insert_with(|| Entry::new(AccountState::default(), now));
        self.settle(id, e, now, "cooldown_expired");
        let before = e.state.clone();
        f(&mut e.state);
        self.settle(id, e, now, reason);
        if e.state != before {
            self.persist(id, &e.state);
        }
    }

    /// Compare the derived status with the last one seen; emit a transition if the label moved.
    fn settle(&self, id: &AccountId, e: &mut Entry, now: OffsetDateTime, reason: &str) {
        let to = derive(&e.state, e.creds_ok, now);
        if to.label() == e.last_status.label() {
            e.last_status = to;
            return;
        }
        let from = std::mem::replace(&mut e.last_status, to.clone());
        self.emit(Transition {
            at: now,
            account: id.clone(),
            from,
            to,
            reason: reason.to_string(),
        });
    }

    fn emit(&self, t: Transition) {
        if matches!(t.to, AccountStatus::Ready) {
            tracing::info!(account = %t.account, from = t.from.label(), to = t.to.label(), reason = %t.reason, "account status changed");
        } else {
            tracing::warn!(account = %t.account, from = t.from.label(), to = t.to.label(), reason = %t.reason, "account status changed");
        }
        let (ts, account, from, to, reason) = (
            t.at.unix_timestamp(),
            t.account.to_string(),
            t.from.label(),
            t.to.label(),
            t.reason.clone(),
        );
        let queued = self.store.submit(move |c| {
            c.execute(
                "INSERT INTO timeline(ts, account, from_state, to_state, reason) VALUES (?1,?2,?3,?4,?5)",
                rusqlite::params![ts, account, from, to, reason],
            )?;
            Ok(())
        });
        if let Err(err) = queued {
            tracing::error!(error = %err, "could not queue timeline row");
        }
        let _ = self.tx.send(t); // no subscribers is fine
    }

    fn persist(&self, id: &AccountId, state: &AccountState) {
        let json = match serde_json::to_string(state) {
            Ok(j) => j,
            Err(e) => {
                tracing::error!(account = %id, error = %e, "could not serialize account state");
                return;
            }
        };
        let account = id.to_string();
        let queued = self.store.submit(move |c| {
            c.execute(
                "INSERT INTO account_state(account, json) VALUES (?1, ?2)
                 ON CONFLICT(account) DO UPDATE SET json = excluded.json",
                rusqlite::params![account, json],
            )?;
            Ok(())
        });
        if let Err(err) = queued {
            tracing::error!(account = %id, error = %err, "could not queue account state write");
        }
    }
}

#[cfg(test)]
#[path = "status_tests.rs"]
mod tests;
