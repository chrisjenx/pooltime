//! Codex ground-truth usage poll (`GET chatgpt.com/backend-api/wham/usage`, `openai.usage_url`)
//! and its sweeper (INV §4 "Codex responses"; bro `codex-usage.ts`).
//!
//! The endpoint is the ChatGPT-subscription analogue of Anthropic's `/api/oauth/usage`:
//! `rate_limit.{primary,secondary}_window` with `used_percent` (0-100, stored as a 0-1 fraction),
//! `limit_window_seconds`, `reset_at` (unix seconds) / `reset_after_seconds`, plus `allowed` /
//! `limit_reached`. A window without `used_percent` is skipped rather than shown as 0% used.
//! The window key comes from the duration like the response headers
//! ([`super::windows::window_key`]). A window is `rejected` only when the account is limited
//! (`limit_reached` or `!allowed`) **and** that window is full: marking an unfull session window
//! rejected under an enforced weekly limit would un-bench the account at the sooner session reset.
//!
//! # Gating
//!
//! Only on this poll's own last check (`usage_check.at`, stamped on success *and* failure), never
//! on the snapshot's `updated_at` (bumped by every response's `x-codex-*` headers, which cannot
//! report a lifted limit) and never on the account's state (a benched account is exactly the one
//! whose ground truth matters). A failing endpoint therefore backs off by a full TTL too. One poll
//! per account at a time (per-account async lock, held across the token refresh).
//!
//! Requests: `authorization: Bearer`, `chatgpt-account-id`, `originator`, `user-agent:
//! codex-cli` (`openai.user_agent`), `accept: application/json`; a 401/403 is retried once with a
//! refreshed token. Failures are classified ([`UsageError::label`]), recorded on the account's
//! `usage_check` and logged; nothing from the upstream body is kept or logged.

use std::sync::Arc;
use std::time::Duration;

use futures_util::future::join_all;
use http::{HeaderName, HeaderValue};
use serde_json::Value;
use time::OffsetDateTime;
use tokio::task::JoinHandle;
use tokio::time::{Instant, MissedTickBehavior};

use super::windows::{window_key, Slot};
use super::OpenAiProvider;
use crate::accounts::{AccountMeta, RateLimitSnapshot, Window, WindowStatus};
use crate::errors::TransportKind;
use crate::providers::{ProviderId, UsageError};
use crate::upstream::{Limits, UpstreamResponse};

/// Lower bound for the fetch timeout (same floor as the Anthropic poll).
const MIN_FETCH_TIMEOUT: Duration = Duration::from_millis(250);

/// Windows from a parsed `wham/usage` body, or `None` when it reports none (bro
/// `mapCodexUsageResponse`).
pub fn map_codex_usage(json: &Value, now: OffsetDateTime) -> Option<RateLimitSnapshot> {
    let rl = json.get("rate_limit").filter(|v| v.is_object())?;
    let limited = rl.get("limit_reached") == Some(&Value::Bool(true))
        || rl.get("allowed") == Some(&Value::Bool(false));
    let num = |w: &Value, k: &str| w.get(k).and_then(Value::as_f64).filter(|n| n.is_finite());

    let mut windows: Vec<Window> = Vec::new();
    for (slot, field) in [
        (Slot::Primary, "primary_window"),
        (Slot::Secondary, "secondary_window"),
    ] {
        let Some(w) = rl.get(field).filter(|v| v.is_object()) else {
            continue;
        };
        // A window without a usage figure is skipped (bro keeps it with `utilization: null`; our
        // `Window` has no "unknown" utilization, and 0% would claim headroom nobody reported).
        let Some(used) = num(w, "used_percent") else {
            continue;
        };
        let reset_at = num(w, "reset_at");
        let reset_after = num(w, "reset_after_seconds");
        let mut key = window_key(num(w, "limit_window_seconds").map(|s| s / 60.0), slot);
        // Two windows of the same scale (never seen today): keep both.
        if windows.iter().any(|w| w.key == key) {
            key = if key == "5h" { "7d" } else { "5h" };
        }
        let reset = match (reset_at, reset_after) {
            (Some(at), _) => OffsetDateTime::from_unix_timestamp(at as i64).ok(),
            (None, Some(after)) => {
                time::Duration::checked_seconds_f64(after).and_then(|d| now.checked_add(d))
            }
            (None, None) => None,
        };
        windows.push(Window {
            reset_synthetic: false,
            key: key.to_string(),
            model: None,
            status: if limited && used >= 100.0 {
                WindowStatus::Rejected
            } else {
                WindowStatus::Allowed
            },
            utilization: (used / 100.0).clamp(0.0, 1.0),
            reset,
        });
    }
    if windows.is_empty() {
        return None;
    }
    windows.sort_by(|a, b| a.key.cmp(&b.key));
    let rejected = windows.iter().any(|w| w.status == WindowStatus::Rejected);
    Some(RateLimitSnapshot {
        unified_status: Some(if rejected { "rejected" } else { "allowed" }.into()),
        windows,
        updated_at: now,
    })
}

impl OpenAiProvider {
    /// Ground-truth usage for `acct`, applied to the [`crate::accounts::StateBook`] (poll result
    /// replaces duration-keyed windows; a spent window benches until its reset, a clean poll lifts
    /// a stale bench).
    ///
    /// Within the TTL of the last check: the current snapshot when that check succeeded, else
    /// [`UsageError::Throttled`]; no request either way. Failures are logged and recorded on the
    /// account's `usage_check` as a classified label.
    pub async fn poll_usage(&self, acct: &AccountMeta) -> Result<RateLimitSnapshot, UsageError> {
        if !self.usage_refresh {
            return Err(UsageError::NotSupported);
        }
        let id = &acct.id;
        let gate = {
            let mut m = self.usage_gates.lock().unwrap_or_else(|e| e.into_inner());
            m.entry(id.clone()).or_default().clone()
        };
        let _held = gate.lock().await;

        let started_at = OffsetDateTime::now_utc();
        let st = self.state.state(id, started_at);
        if let Some(check) = &st.usage_check {
            let ttl = time::Duration::try_from(self.usage_ttl).unwrap_or(time::Duration::MAX);
            let age = started_at - check.at;
            if !age.is_negative() && age < ttl {
                return match st.snapshot {
                    Some(snap) if check.ok => Ok(snap),
                    _ => Err(UsageError::Throttled),
                };
            }
        }

        match self.fetch_usage(acct).await {
            Ok(snap) => {
                self.state
                    .replace_poll_snapshot(id, snap.clone(), started_at);
                Ok(snap)
            }
            Err((err, request_id)) => {
                let label = err.label();
                tracing::warn!(
                    account = %id,
                    outcome = %label,
                    request_id = request_id.as_deref().unwrap_or("-"),
                    "codex usage poll failed"
                );
                self.state
                    .record_usage_check_error(id, OffsetDateTime::now_utc(), &label);
                Err(err)
            }
        }
    }

    /// One poll (plus the one post-401/403 resend). `Err` carries the upstream request id, if any.
    async fn fetch_usage(
        &self,
        acct: &AccountMeta,
    ) -> Result<RateLimitSnapshot, (UsageError, Option<String>)> {
        let bare = |e: UsageError| (e, None);
        let mut token = self
            .access_token(acct, false)
            .await
            .map_err(|k| bare(UsageError::Refresh(k)))?;
        let account_id = self.chatgpt_account_id(&acct.id).await;
        let mut resp = self
            .usage_request(&token, account_id.as_deref())
            .await
            .map_err(|k| bare(UsageError::Transport(k)))?;
        if matches!(resp.status.as_u16(), 401 | 403) {
            drop(resp);
            token = self
                .token_after_reject(acct, &token)
                .await
                .map_err(|k| bare(UsageError::Refresh(k)))?;
            resp = self
                .usage_request(&token, account_id.as_deref())
                .await
                .map_err(|k| bare(UsageError::Transport(k)))?;
        }
        let request_id = ["x-request-id", "request-id"].iter().find_map(|h| {
            resp.headers
                .get(*h)
                .and_then(|v| v.to_str().ok())
                .map(str::to_owned)
        });
        let fail = |e: UsageError| (e, request_id.clone());
        let status = resp.status.as_u16();
        if !(200..300).contains(&status) {
            return Err(fail(UsageError::Status(status)));
        }
        let body = resp
            .body
            .text()
            .await
            .map_err(|k| fail(UsageError::Transport(k)))?;
        let json: Value = serde_json::from_str(&body)
            .map_err(|_| fail(UsageError::Parse("invalid_json".into())))?;
        map_codex_usage(&json, OffsetDateTime::now_utc())
            .ok_or_else(|| fail(UsageError::Parse("no_windows".into())))
    }

    async fn usage_request(
        &self,
        token: &str,
        account_id: Option<&str>,
    ) -> Result<UpstreamResponse, TransportKind> {
        let bad = |what: &str| {
            tracing::error!("{what} is not valid for the codex usage request");
            TransportKind::Other
        };
        let url: reqwest::Url = self
            .cfg
            .usage_url
            .parse()
            .map_err(|_| bad("openai usage_url"))?;
        let mut req = reqwest::Request::new(reqwest::Method::GET, url);
        let h = req.headers_mut();
        let mut auth =
            HeaderValue::from_str(&format!("Bearer {token}")).map_err(|_| bad("access token"))?;
        auth.set_sensitive(true);
        h.insert(http::header::AUTHORIZATION, auth);
        h.insert(
            HeaderName::from_static("originator"),
            HeaderValue::from_str(&self.cfg.originator).map_err(|_| bad("openai originator"))?,
        );
        h.insert(
            http::header::USER_AGENT,
            HeaderValue::from_str(&self.cfg.user_agent).map_err(|_| bad("openai user_agent"))?,
        );
        h.insert(
            http::header::ACCEPT,
            HeaderValue::from_static("application/json"),
        );
        if let Some(a) = account_id {
            h.insert(
                HeaderName::from_static("chatgpt-account-id"),
                HeaderValue::from_str(a).map_err(|_| bad("stored ChatGPT account id"))?,
            );
        }
        let timeout = self.usage_fetch_timeout.max(MIN_FETCH_TIMEOUT);
        let lim = Limits {
            header_wait: timeout,
            idle: timeout,
            deadline: Instant::now() + timeout,
        };
        self.up.send(req, &lim).await
    }
}

/// Poll every OpenAI account with stored credentials once at startup and then every `every`
/// (the Anthropic [`crate::providers::anthropic::SWEEP_INTERVAL`] in production). Account state is
/// not consulted (benched accounts need the ground truth most); the per-account TTL in
/// [`OpenAiProvider::poll_usage`] bounds the request rate. Returns at once if polling is off.
pub fn spawn_usage_sweeper(provider: Arc<OpenAiProvider>, every: Duration) -> JoinHandle<()> {
    tokio::spawn(async move {
        if !provider.usage_refresh {
            return;
        }
        let mut tick = tokio::time::interval(every);
        tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
        loop {
            tick.tick().await; // the first tick is immediate: the startup sweep
            let accounts: Vec<AccountMeta> = provider
                .reg
                .list()
                .into_iter()
                .filter(|a| a.provider == ProviderId::OpenAi)
                .filter(|a| matches!(provider.reg.read_creds(&a.id), Ok(Some(_))))
                .collect();
            // Failures are logged and recorded inside poll_usage.
            join_all(accounts.iter().map(|a| provider.poll_usage(a))).await;
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use time::macros::datetime;

    const NOW: OffsetDateTime = datetime!(2026-07-14 00:00 UTC);

    #[test]
    fn weekly_in_primary_is_7d_and_unenforced_100_is_allowed() {
        // Trimmed from a real capture (bro codex-usage.test.ts).
        let body = json!({"plan_type": "plus", "rate_limit": {
            "allowed": true, "limit_reached": false,
            "primary_window": {"used_percent": 100, "limit_window_seconds": 604800,
                               "reset_after_seconds": 583719, "reset_at": 1784666164},
            "secondary_window": null}});
        let s = map_codex_usage(&body, NOW).unwrap();
        assert_eq!(s.windows.len(), 1);
        let w = &s.windows[0];
        assert_eq!(
            (w.key.as_str(), w.status, w.utilization),
            ("7d", WindowStatus::Allowed, 1.0)
        );
        assert_eq!(w.reset.unwrap().unix_timestamp(), 1784666164);
        assert_eq!(s.unified_status.as_deref(), Some("allowed"));
    }

    #[test]
    fn limited_rejects_only_full_windows() {
        let body = json!({"rate_limit": {"allowed": false, "limit_reached": true,
            "primary_window": {"used_percent": 100, "limit_window_seconds": 604800, "reset_at": 1784666164},
            "secondary_window": {"used_percent": 12, "limit_window_seconds": 18000, "reset_at": 1784600000}}});
        let s = map_codex_usage(&body, NOW).unwrap();
        let w = |k: &str| s.windows.iter().find(|w| w.key == k).unwrap().status;
        assert_eq!(w("7d"), WindowStatus::Rejected);
        assert_eq!(w("5h"), WindowStatus::Allowed);
        assert_eq!(s.unified_status.as_deref(), Some("rejected"));
        // `allowed: false` alone also counts as limited.
        let body = json!({"rate_limit": {"allowed": false,
            "primary_window": {"used_percent": 100, "limit_window_seconds": 18000}}});
        assert_eq!(
            map_codex_usage(&body, NOW).unwrap().windows[0].status,
            WindowStatus::Rejected
        );
    }

    #[test]
    fn reset_after_fallback_fraction_and_duplicates() {
        let body = json!({"rate_limit": {"allowed": true, "limit_reached": false,
            "primary_window": {"used_percent": 40, "limit_window_seconds": 18000, "reset_after_seconds": 3600},
            "secondary_window": {"used_percent": 12, "limit_window_seconds": 604800, "reset_at": 1784666164}}});
        let s = map_codex_usage(&body, NOW).unwrap();
        let w = |k: &str| s.windows.iter().find(|w| w.key == k).unwrap().clone();
        assert_eq!(w("5h").reset, Some(NOW + time::Duration::hours(1)));
        assert!((w("5h").utilization - 0.4).abs() < 1e-9);
        assert!((w("7d").utilization - 0.12).abs() < 1e-9);
        // Two windows of the same scale keep both; no duration falls back to the slot default.
        let body = json!({"rate_limit": {
            "primary_window": {"used_percent": 1, "limit_window_seconds": 18000},
            "secondary_window": {"used_percent": 2, "limit_window_seconds": 18000}}});
        let keys: Vec<String> = map_codex_usage(&body, NOW)
            .unwrap()
            .windows
            .into_iter()
            .map(|w| w.key)
            .collect();
        assert_eq!(keys, vec!["5h", "7d"]);
    }

    #[test]
    fn unusable_bodies_yield_none() {
        for body in [
            json!({}),
            json!({"rate_limit": null}),
            json!({"rate_limit": {"primary_window": null, "secondary_window": {}}}),
            // A reset without a usage figure is not rendered as 0% used: no window.
            json!({"rate_limit": {"primary_window": {"limit_window_seconds": 18000,
                                                     "reset_at": 1784600000}}}),
        ] {
            assert!(map_codex_usage(&body, NOW).is_none(), "{body}");
        }
    }
}
