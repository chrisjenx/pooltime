//! Live Claude model catalog for `GET /v1/models` (INV §5).
//!
//! Anthropic's `GET /v1/models` accepts the same Claude Code OAuth token the pool already holds,
//! so the Claude side of the listing is read from upstream instead of a hand-kept table.
//!
//! Token choice (INV §10.8): any account that is not `NeedsLogin`, in a stable order (Ready
//! accounts first, then by id). Never `choose()` (this is not inference traffic and must not touch
//! affinity, reservations or scores), and never an account whose refresh token is known dead:
//! bro's lookup hammered those accounts' refresh endpoint on every listing. A not-yet-dead account
//! with an expiring token is refreshed through the normal [`AnthropicProvider::access_token`]
//! path (gated, single in-flight, cancel-safe); a rejected listing request never forces a refresh.
//!
//! Cache: 10 minutes, single-flight (concurrent callers share one fetch, failures included), and
//! a stale list is preferred over failure. Nothing here logs a token or an upstream message.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::anyhow;
use http::HeaderValue;
use serde_json::{json, Map, Value};
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;
use tokio::time::Instant as TokioInstant;

use super::AnthropicProvider;
use crate::accounts::AccountStatus;
use crate::providers::ProviderId;
use crate::upstream::{join_url, Limits};

/// `owned_by` of every Anthropic row (live and table).
pub const ANTHROPIC_OWNER: &str = "anthropic-claude-max-pool";

/// How long a successful upstream list is served before re-fetching.
pub const MODELS_CACHE_TTL: Duration = Duration::from_secs(10 * 60);

const MODELS_PATH: &str = "/v1/models?limit=1000";
/// Floor for the per-request bound (the shared `usage_fetch_timeout` knob, like bro).
const MIN_FETCH_TIMEOUT: Duration = Duration::from_millis(250);

/// Cache plus the single-flight gate. The async mutex is held across the fetch, so concurrent
/// callers queue behind one fetch; `attempts` lets a queued caller tell that an attempt finished
/// while it waited (and use its outcome instead of starting another).
#[derive(Default)]
pub(super) struct CatalogCache {
    state: tokio::sync::Mutex<Option<(Instant, Arc<Vec<Value>>)>>,
    attempts: AtomicU64,
}

impl AnthropicProvider {
    /// Anthropic's current model list as `/v1/models` rows (`id`, `object`, `created`, `owned_by`,
    /// optional `display_name` / `max_input_tokens`).
    ///
    /// Served from a fresh cache; otherwise fetched once for all concurrent callers. A failed
    /// fetch returns the stale cache when there is one; with none it is an `Err` carrying only a
    /// classified reason (callers fall back to the table).
    pub async fn live_models(&self) -> anyhow::Result<Vec<Value>> {
        let seen = self.catalog.attempts.load(Ordering::SeqCst);
        let mut cache = self.catalog.state.lock().await;
        let finished_meanwhile = self.catalog.attempts.load(Ordering::SeqCst) != seen;
        match &*cache {
            Some((at, models)) if at.elapsed() < self.catalog_ttl || finished_meanwhile => {
                return Ok(models.to_vec());
            }
            None if finished_meanwhile => {
                // The attempt we waited behind failed and nothing is cached.
                return Err(anyhow!("live model catalog unavailable"));
            }
            _ => {}
        }
        let fetched = self.fetch_models().await;
        self.catalog.attempts.fetch_add(1, Ordering::SeqCst);
        match fetched {
            Ok(models) => {
                let models = Arc::new(models);
                *cache = Some((Instant::now(), models.clone()));
                Ok(models.to_vec())
            }
            Err(reason) => {
                let stale = cache.as_ref().map(|(_, m)| m.clone());
                tracing::warn!(
                    reason = %reason,
                    stale_served = stale.is_some(),
                    "live model catalog fetch failed"
                );
                match stale {
                    Some(models) => Ok(models.to_vec()),
                    None => Err(anyhow!("live model catalog fetch failed: {reason}")),
                }
            }
        }
    }

    /// One fetch: try each eligible account until one answers. `Err` is a classified label.
    async fn fetch_models(&self) -> Result<Vec<Value>, String> {
        let url: reqwest::Url = join_url(&self.cfg.api_base, MODELS_PATH)
            .parse()
            .map_err(|_| "invalid_api_base".to_string())?;
        let accounts = self.catalog_accounts();
        if accounts.is_empty() {
            return Err("no_usable_account".to_string());
        }
        let mut last = "no_response".to_string();
        for acct in accounts {
            let token = match self.access_token(&acct, false).await {
                Ok(t) => t,
                Err(kind) => {
                    // access_token has already logged and recorded this refresh failure.
                    last = format!("refresh:{}", kind.detail());
                    continue;
                }
            };
            match self.fetch_from(&url, &token).await {
                Ok(models) => return Ok(models),
                Err(label) => {
                    tracing::debug!(account = %acct.id, reason = %label, "live model catalog attempt failed");
                    last = label;
                }
            }
        }
        Err(last)
    }

    /// Anthropic accounts whose token may be used: credentials readable, not `NeedsLogin`, refresh
    /// token not known dead. Ready accounts first, then registry (id) order.
    fn catalog_accounts(&self) -> Vec<crate::accounts::AccountMeta> {
        let now = OffsetDateTime::now_utc();
        let mut ready = Vec::new();
        let mut other = Vec::new();
        for acct in self.reg.list() {
            if acct.provider != ProviderId::Anthropic {
                continue;
            }
            let Ok(Some(creds)) = self.reg.read_creds(&acct.id) else {
                continue;
            };
            if creds
                .refresh_token
                .as_deref()
                .is_some_and(|rt| self.state.is_refresh_dead(&acct.id, rt))
            {
                continue;
            }
            match self.state.status(&acct.id, true, now) {
                AccountStatus::NeedsLogin => {}
                AccountStatus::Ready => ready.push(acct),
                _ => other.push(acct),
            }
        }
        ready.extend(other);
        ready
    }

    async fn fetch_from(&self, url: &reqwest::Url, token: &str) -> Result<Vec<Value>, String> {
        let bad = || "invalid_header".to_string();
        let mut auth = HeaderValue::from_str(&format!("Bearer {token}")).map_err(|_| bad())?;
        auth.set_sensitive(true);
        let ua = HeaderValue::from_str(&self.cfg.user_agent).map_err(|_| bad())?;
        let mut req = reqwest::Request::new(reqwest::Method::GET, url.clone());
        let h = req.headers_mut();
        h.insert(http::header::AUTHORIZATION, auth);
        h.insert("anthropic-version", HeaderValue::from_static("2023-06-01"));
        h.insert(
            "anthropic-beta",
            HeaderValue::from_static("oauth-2025-04-20"),
        );
        h.insert(http::header::USER_AGENT, ua);

        let timeout = self.cfg.usage_fetch_timeout.max(MIN_FETCH_TIMEOUT);
        let lim = Limits {
            header_wait: timeout,
            idle: timeout,
            deadline: TokioInstant::now() + timeout,
        };
        let resp = self
            .up
            .send(req, &lim)
            .await
            .map_err(|k| format!("transport:{}", k.as_str()))?;
        let status = resp.status.as_u16();
        if !(200..300).contains(&status) {
            return Err(format!("status:{status}"));
        }
        let body = resp
            .body
            .text()
            .await
            .map_err(|k| format!("transport:{}", k.as_str()))?;
        parse_models(&body)
    }
}

/// Rows of an upstream list body: entries without a string `id` are dropped; an empty result is a
/// failure (bro: `models.length ? models : null`).
fn parse_models(body: &str) -> Result<Vec<Value>, String> {
    let root: Value = serde_json::from_str(body).map_err(|_| "parse:invalid_json".to_string())?;
    let data = root
        .get("data")
        .and_then(Value::as_array)
        .ok_or_else(|| "parse:no_data".to_string())?;
    let rows: Vec<Value> = data.iter().filter_map(to_row).collect();
    if rows.is_empty() {
        return Err("parse:no_models".to_string());
    }
    Ok(rows)
}

fn to_row(raw: &Value) -> Option<Value> {
    let id = raw.get("id")?.as_str().filter(|s| !s.is_empty())?;
    let created = raw
        .get("created_at")
        .and_then(Value::as_str)
        .and_then(|s| OffsetDateTime::parse(s, &Rfc3339).ok())
        .map_or(0, OffsetDateTime::unix_timestamp);
    let mut row = Map::new();
    row.insert("id".into(), json!(id));
    row.insert("object".into(), json!("model"));
    row.insert("created".into(), json!(created));
    row.insert("owned_by".into(), json!(ANTHROPIC_OWNER));
    if let Some(name) = raw
        .get("display_name")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
    {
        row.insert("display_name".into(), json!(name));
    }
    if let Some(n) = raw.get("max_input_tokens").and_then(Value::as_u64) {
        row.insert("max_input_tokens".into(), json!(n));
    }
    Some(Value::Object(row))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rows_keep_known_fields_and_drop_malformed_entries() {
        let rows = parse_models(
            r#"{"data":[
                {"type":"model","id":"claude-a","display_name":"A","created_at":"2026-08-28T00:00:00Z","max_input_tokens":1000000,"extra":1},
                {"id":"claude-b","created_at":"not a date"},
                {"id":""}, {"id":5}, {"display_name":"no id"}, 7
            ]}"#,
        )
        .unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(
            rows[0],
            json!({"id":"claude-a","object":"model","created":1787875200,
                   "owned_by":ANTHROPIC_OWNER,"display_name":"A","max_input_tokens":1000000})
        );
        assert_eq!(
            rows[1],
            json!({"id":"claude-b","object":"model","created":0,"owned_by":ANTHROPIC_OWNER})
        );
    }

    #[test]
    fn unusable_bodies_are_classified() {
        assert_eq!(parse_models("nope").unwrap_err(), "parse:invalid_json");
        assert_eq!(parse_models("{}").unwrap_err(), "parse:no_data");
        assert_eq!(
            parse_models(r#"{"data":[{"id":""}]}"#).unwrap_err(),
            "parse:no_models"
        );
    }
}
