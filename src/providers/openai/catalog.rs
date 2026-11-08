//! Codex model catalog discovery (INV §4 "Catalog discovery"; bro `codex-models.ts`).
//!
//! `GET {openai.api_base}/models?client_version=<CODEX_CLIENT_VERSION>` per signed-in OpenAI
//! account, bounded by [`CATALOG_TIMEOUT`], retried once with a refreshed token on 401/403.
//!
//! Validation is strict: one malformed entry rejects that account's whole catalog (a partial
//! read of a changed schema must not invent routes). The body must be `{"models": [...]}`; each
//! entry an object whose `visibility` is `list`, `hide` or `none`; only `list` entries become
//! routes, and those need a slug matching `^[a-z0-9][a-z0-9._-]*$` (case-insensitive) and a
//! `supported_reasoning_levels` array with at least one known Codex effort (`ultra` and other
//! unknown levels are dropped). `context_window` / `max_context_window` are kept when positive
//! integers, the max only when it is at least the context window.
//!
//! The merge into the live [`crate::models::ModelTable`] is additive ([`ModelTable::add_discovered`]):
//! an id already in the table (bundled, user-configured or discovered earlier) is never replaced
//! and nothing is ever removed. New rows are hot-applied (the next `/v1/models` listing and
//! request see them) and persisted to `models.toml` with the table's atomic save, the same file
//! and path the mapping editor uses, so a restart keeps them. [`refresh_codex_catalog`] then
//! re-syncs connected Claude Code profiles so their pins / context hints include the new routes.
//!
//! Runs at boot (after providers are registered), every [`CATALOG_REFRESH_EVERY`], and on
//! `POST /api/v1/models/refresh` (`pooltime models refresh`). Every account failing is an error;
//! some failing is a warning naming the account and a classified reason only. Tokens and upstream
//! bodies are never logged.

use std::sync::Arc;
use std::time::Duration;

use anyhow::anyhow;
use http::{HeaderName, HeaderValue};
use serde_json::Value;
use tokio::time::Instant;

use super::OpenAiProvider;
use crate::accounts::{AccountId, AccountMeta};
use crate::errors::TransportKind;
use crate::models::{ModelTable, RouteDef, CODEX_EFFORTS};
use crate::providers::ProviderId;
use crate::server::AppState;
use crate::upstream::{join_url, Limits, UpstreamResponse};

/// `client_version` the catalog is requested for (bro `CODEX_MODELS_URL`).
pub const CODEX_CLIENT_VERSION: &str = "0.200.0";
/// Path of the catalog under `openai.api_base`.
pub const MODELS_PATH: &str = "/models";
/// Bound for one catalog request.
pub const CATALOG_TIMEOUT: Duration = Duration::from_secs(8);
/// How often the daemon re-runs discovery after the boot run.
pub const CATALOG_REFRESH_EVERY: Duration = Duration::from_secs(60 * 60);

/// What one discovery run did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CodexDiscovery {
    /// Signed-in OpenAI accounts asked (0: nothing to ask, the run was skipped).
    pub accounts: usize,
    /// Ids newly added to the model table, in catalog order.
    pub added: Vec<String>,
    /// Accounts whose catalog was unavailable, with a classified reason.
    pub failed: Vec<(AccountId, String)>,
}

/// Routes of a catalog body, or a classified reason (`catalog:<class>`). See the module docs.
fn parse_catalog(body: &Value) -> Result<Vec<RouteDef>, String> {
    let bad = |class: &str| format!("catalog:{class}");
    let entries = body
        .get("models")
        .and_then(Value::as_array)
        .ok_or_else(|| bad("invalid_response"))?;
    let mut routes = Vec::new();
    for entry in entries {
        let m = entry.as_object().ok_or_else(|| bad("invalid_entry"))?;
        match m.get("visibility").and_then(Value::as_str) {
            Some("list") => {}
            Some("hide" | "none") => continue,
            _ => return Err(bad("invalid_visibility")),
        }
        let slug = m
            .get("slug")
            .and_then(Value::as_str)
            .filter(|s| valid_slug(s))
            .ok_or_else(|| bad("invalid_slug"))?;
        let positive = |k: &str| {
            m.get(k)
                .and_then(Value::as_u64)
                .filter(|n| *n > 0)
                .and_then(|n| u32::try_from(n).ok())
        };
        let context_window = positive("context_window");
        let max_context_window =
            positive("max_context_window").filter(|max| context_window.is_none_or(|c| *max >= c));
        let levels = m
            .get("supported_reasoning_levels")
            .and_then(Value::as_array)
            .ok_or_else(|| bad("invalid_reasoning_levels"))?;
        let mut supported_efforts: Vec<String> = Vec::new();
        for effort in levels
            .iter()
            .filter_map(|l| l.get("effort").and_then(Value::as_str))
        {
            if CODEX_EFFORTS.contains(&effort) && !supported_efforts.iter().any(|e| e == effort) {
                supported_efforts.push(effort.to_string());
            }
        }
        if supported_efforts.is_empty() {
            return Err(bad("no_reasoning_levels"));
        }
        routes.push(RouteDef {
            id: slug.to_string(),
            provider: ProviderId::OpenAi,
            upstream_model: slug.to_string(),
            context_window,
            max_context_window,
            supported_efforts,
            effort_map: None,
        });
    }
    Ok(routes)
}

/// `^[a-z0-9][a-z0-9._-]*$`, case-insensitive.
fn valid_slug(s: &str) -> bool {
    let mut chars = s.chars();
    chars.next().is_some_and(|c| c.is_ascii_alphanumeric())
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

impl OpenAiProvider {
    /// The validated catalog visible to `acct`, or a classified reason (`refresh:<kind>`,
    /// `transport:<kind>`, `status:<code>`, `parse:invalid_json`, `catalog:<class>`).
    pub async fn fetch_catalog(&self, acct: &AccountMeta) -> Result<Vec<RouteDef>, String> {
        let refresh = |k: crate::errors::RefreshErrorKind| format!("refresh:{}", k.detail());
        let transport = |k: TransportKind| format!("transport:{}", k.as_str());
        let mut token = self.access_token(acct, false).await.map_err(refresh)?;
        let account_id = self.chatgpt_account_id(&acct.id).await;
        let mut resp = self
            .catalog_request(&token, account_id.as_deref())
            .await
            .map_err(transport)?;
        if matches!(resp.status.as_u16(), 401 | 403) {
            drop(resp);
            token = self
                .token_after_reject(acct, &token)
                .await
                .map_err(refresh)?;
            resp = self
                .catalog_request(&token, account_id.as_deref())
                .await
                .map_err(transport)?;
        }
        let status = resp.status.as_u16();
        if !(200..300).contains(&status) {
            return Err(format!("status:{status}"));
        }
        let body = resp.body.text().await.map_err(transport)?;
        let json: Value =
            serde_json::from_str(&body).map_err(|_| "parse:invalid_json".to_string())?;
        parse_catalog(&json)
    }

    async fn catalog_request(
        &self,
        token: &str,
        account_id: Option<&str>,
    ) -> Result<UpstreamResponse, TransportKind> {
        let bad = |what: &str| {
            tracing::error!("{what} is not valid for the codex catalog request");
            TransportKind::Other
        };
        let url = format!(
            "{}?client_version={CODEX_CLIENT_VERSION}",
            join_url(&self.cfg.api_base, MODELS_PATH)
        );
        let url: reqwest::Url = url.parse().map_err(|_| bad("openai api_base"))?;
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
            http::header::ACCEPT,
            HeaderValue::from_static("application/json"),
        );
        if let Some(a) = account_id {
            h.insert(
                HeaderName::from_static("chatgpt-account-id"),
                HeaderValue::from_str(a).map_err(|_| bad("stored ChatGPT account id"))?,
            );
        }
        let lim = Limits {
            header_wait: CATALOG_TIMEOUT,
            idle: CATALOG_TIMEOUT,
            deadline: Instant::now() + CATALOG_TIMEOUT,
        };
        self.up.send(req, &lim).await
    }
}

/// Ask every signed-in OpenAI account for its catalog and merge the new ids into the live model
/// table (persisted). Does not re-sync profiles (see [`refresh_codex_catalog`]; the mgmt refresh
/// endpoint syncs once for both providers).
///
/// No OpenAI provider or no signed-in account: an empty report. Every account failing: `Err`
/// (classified reason, table unchanged).
pub async fn discover_codex_models(app: &Arc<AppState>) -> anyhow::Result<CodexDiscovery> {
    let Some(provider) = app.providers.get(&ProviderId::OpenAi).cloned() else {
        return Ok(CodexDiscovery::default());
    };
    let accounts: Vec<AccountMeta> = app
        .registry
        .list()
        .into_iter()
        .filter(|a| a.provider == ProviderId::OpenAi)
        .filter(
            |a| matches!(app.registry.read_creds(&a.id), Ok(Some(c)) if !c.access_token.is_empty()),
        )
        .collect();
    let mut report = CodexDiscovery {
        accounts: accounts.len(),
        ..CodexDiscovery::default()
    };
    if accounts.is_empty() {
        tracing::debug!("codex catalog: no signed-in OpenAI account; skipped");
        return Ok(report);
    }

    let mut found: Vec<RouteDef> = Vec::new();
    for acct in &accounts {
        match provider.catalog(acct).await {
            Ok(defs) => {
                for d in defs {
                    if found.iter().all(|f| f.id != d.id) {
                        found.push(d);
                    }
                }
            }
            Err(e) => {
                let reason = e.to_string();
                tracing::warn!(account = %acct.id, reason = %reason, "codex model catalog unavailable");
                report.failed.push((acct.id.clone(), reason));
            }
        }
    }
    if report.failed.len() == accounts.len() {
        let last = report.failed.last().map_or("", |(_, r)| r.as_str());
        return Err(anyhow!(
            "codex model catalog unavailable on every account ({last})"
        ));
    }
    if !report.failed.is_empty() {
        tracing::warn!(
            failed = report.failed.len(),
            accounts = accounts.len(),
            "some codex catalogs were unavailable; keeping models discovered from the others"
        );
    }

    let a = app.clone();
    report.added = tokio::task::spawn_blocking(move || merge(&a, found))
        .await
        .map_err(|_| anyhow!("codex catalog merge task failed"))?;
    if report.added.is_empty() {
        tracing::debug!("codex model catalog has no new model ids");
    } else {
        tracing::info!(
            added = report.added.len(),
            models = %report.added.join(","),
            "discovered new codex models"
        );
    }
    Ok(report)
}

/// Add the new ids to the live table (under its write lock) and persist; returns the added ids.
/// A failed save is logged and the rows still go live (they are re-discovered next run).
fn merge(app: &AppState, found: Vec<RouteDef>) -> Vec<String> {
    let mut live = app.models.write().unwrap_or_else(|e| e.into_inner());
    let new: Vec<RouteDef> = found
        .into_iter()
        .filter(|d| live.rows().iter().all(|r| r.id != d.id))
        .collect();
    if new.is_empty() {
        return Vec::new();
    }
    let ids: Vec<String> = new.iter().map(|d| d.id.clone()).collect();
    let mut next: ModelTable = live.clone();
    next.add_discovered(new);
    if let Err(e) = next.save(&app.paths) {
        tracing::error!(error = %format!("{e:#}"), "could not save discovered codex models to models.toml");
    }
    *live = next;
    ids
}

/// Discovery plus, when it added routes, a profile re-sync so connected Claude Code profiles see
/// them. For the boot / periodic runs: failures are logged, never returned.
pub async fn refresh_codex_catalog(app: &Arc<AppState>) {
    match discover_codex_models(app).await {
        Ok(r) if !r.added.is_empty() => {
            if let Err(e) = crate::claude_code::sync_once(app).await {
                tracing::warn!(error = %format!("{e:#}"), "profile sync after codex discovery failed");
            }
        }
        Ok(_) => {}
        Err(e) => tracing::warn!(error = %format!("{e:#}"), "codex model discovery failed"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn slug_rule() {
        for ok in ["gpt-5.5", "o3", "GPT_x-1.2", "9a"] {
            assert!(valid_slug(ok), "{ok}");
        }
        for bad in ["", "-x", ".x", "_x", "a b", "a/b", "a:b", "ä"] {
            assert!(!valid_slug(bad), "{bad}");
        }
    }

    #[test]
    fn non_object_entry_rejects_catalog() {
        let body = json!({"models": [
            {"slug": "a", "visibility": "list", "supported_reasoning_levels": [{"effort": "low"}]},
            "oops"
        ]});
        assert_eq!(parse_catalog(&body).unwrap_err(), "catalog:invalid_entry");
        assert_eq!(parse_catalog(&json!({"models": []})).unwrap(), vec![]);
    }
}
