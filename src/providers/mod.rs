//! Provider identity and the `Provider` trait every upstream (Anthropic, Codex)
//! implements, plus the shared entitlement-message classifier.
//! Task 24 adds `fn signin(&self) -> &dyn signin::SignInFlow` to the trait.

pub mod anthropic;

use std::fmt;
use std::str::FromStr;

use async_trait::async_trait;
use bytes::Bytes;
use http::HeaderMap;
use serde::{Deserialize, Serialize};
use tokio::sync::oneshot;

use crate::accounts::{AccountMeta, RateLimitSnapshot, StateBook, TokenUsage};
use crate::errors::{FailReason, RefreshErrorKind, TransportKind};
use crate::models::{Route, RouteDef};
use crate::routing::reserve::Reservation;
use crate::upstream::{Limits, Upstream};

/// Resolves once the response stream ends, with the final token usage. The
/// sender is dropped without a value if the stream aborts before usage is known.
pub type TokenUsageFuture = oneshot::Receiver<TokenUsage>;

/// A parsed inbound `/v1/messages` request.
#[derive(Debug, Clone)]
pub struct InboundRequest {
    pub raw_body: Bytes,
    pub json: serde_json::Value,
    pub headers: HeaderMap,
    pub model: String,
    pub stream: bool,
    pub session_user_id: Option<String>,
    pub request_id: String,
}

/// Everything one upstream attempt needs.
pub struct AttemptCtx<'a> {
    pub account: &'a AccountMeta,
    pub token: String,
    pub route: &'a Route,
    pub limits: Limits,
    pub upstream: &'a Upstream,
    /// The caller's in-flight slot for `account`. A committed streaming response moves it into
    /// the task that pumps the body, so the slot is held until the stream ends (completion,
    /// upstream failure or client disconnect) and released by `Drop`. For every other outcome it
    /// is dropped when the attempt returns. `None` = no slot to hold (tests, unreserved callers).
    pub reservation: Option<Reservation>,
}

/// Result of one upstream attempt.
pub enum AttemptOutcome {
    /// Committed response (streaming or buffered) plus eventual token usage.
    Done(axum::response::Response, Option<TokenUsageFuture>),
    /// Fatal for this request: send this response to the client; record the reason.
    Terminal(axum::response::Response, FailReason),
    /// Try another account/provider; `fallback_response` is what to send if none remains.
    Retry {
        reason: FailReason,
        fallback_response: axum::response::Response,
    },
}

/// Why a usage poll failed. Carries no upstream message text.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum UsageError {
    #[error("transport error: {0:?}")]
    Transport(TransportKind),
    #[error("upstream status {0}")]
    Status(u16),
    #[error("account lacks entitlement")]
    Entitlement,
    /// The response was not usable. Carries a static classification (`invalid_json`,
    /// `no_windows`), never upstream text.
    #[error("unparseable usage response: {0}")]
    Parse(String),
    /// No usable access token could be obtained for the poll.
    #[error("access token unavailable: {0:?}")]
    Refresh(RefreshErrorKind),
    #[error("usage polling not supported")]
    NotSupported,
}

impl UsageError {
    /// Classified label for `usage_check.error` and logs: `transport:<kind>`, `status:<code>`,
    /// `entitlement`, `parse:<class>`, `refresh:<kind>`, `not_supported`. Never upstream text.
    pub fn label(&self) -> String {
        match self {
            UsageError::Transport(k) => format!("transport:{}", k.as_str()),
            UsageError::Status(code) => format!("status:{code}"),
            UsageError::Entitlement => "entitlement".into(),
            UsageError::Parse(class) => format!("parse:{class}"),
            UsageError::Refresh(k) => format!("refresh:{}", k.detail()),
            UsageError::NotSupported => "not_supported".into(),
        }
    }
}

#[async_trait]
pub trait Provider: Send + Sync {
    fn id(&self) -> ProviderId;
    /// A usable access token for `acct`, refreshing when `force` or near expiry.
    async fn access_token(
        &self,
        acct: &AccountMeta,
        force: bool,
    ) -> Result<String, RefreshErrorKind>;
    async fn attempt(
        &self,
        ctx: AttemptCtx<'_>,
        req: &InboundRequest,
        state: &StateBook,
    ) -> AttemptOutcome;
    async fn poll_usage(&self, acct: &AccountMeta) -> Result<RateLimitSnapshot, UsageError>;
    async fn catalog(&self, acct: &AccountMeta) -> anyhow::Result<Vec<RouteDef>>;
    /// Live `/v1/models` rows for this provider's own listing (Anthropic: its upstream catalog,
    /// cached and single-flight). Providers whose rows come from the model table return none.
    async fn live_models(&self) -> anyhow::Result<Vec<serde_json::Value>> {
        Ok(Vec::new())
    }
    /// Like [`Provider::live_models`] but bypassing any cache (`pooltime models refresh`). A
    /// failure is an `Err` (no stale fallback), and must leave the last good list in place.
    async fn refresh_live_models(&self) -> anyhow::Result<Vec<serde_json::Value>> {
        self.live_models().await
    }
}

/// True when an upstream 403/error message indicates an entitlement problem
/// (organization / subscription / billing / credit balance) rather than a generic
/// refusal. Deliberately narrow, case-insensitive substring match (INV §1; bro
/// `isEntitlementRefusal`): anything unmatched stays a generic access denial.
pub fn is_entitlement_message(msg: &str) -> bool {
    let m = msg.to_lowercase();
    ["organization", "subscription", "billing", "credit balance"]
        .iter()
        .any(|k| m.contains(k))
}

/// Which upstream an account belongs to. Serialized as `"anthropic"` / `"openai"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProviderId {
    Anthropic,
    OpenAi,
}

impl ProviderId {
    pub fn as_str(self) -> &'static str {
        match self {
            ProviderId::Anthropic => "anthropic",
            ProviderId::OpenAi => "openai",
        }
    }
}

impl fmt::Display for ProviderId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unknown provider {0:?} (expected \"anthropic\" or \"openai\")")]
pub struct UnknownProvider(pub String);

impl FromStr for ProviderId {
    type Err = UnknownProvider;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "anthropic" => Ok(ProviderId::Anthropic),
            "openai" => Ok(ProviderId::OpenAi),
            other => Err(UnknownProvider(other.to_string())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_fromstr_and_serde_agree() {
        for (p, s) in [
            (ProviderId::Anthropic, "anthropic"),
            (ProviderId::OpenAi, "openai"),
        ] {
            assert_eq!(p.to_string(), s);
            assert_eq!(s.parse::<ProviderId>().unwrap(), p);
            assert_eq!(serde_json::to_string(&p).unwrap(), format!("\"{s}\""));
            assert_eq!(
                serde_json::from_str::<ProviderId>(&format!("\"{s}\"")).unwrap(),
                p
            );
        }
        assert!("OpenAI".parse::<ProviderId>().is_err());
    }

    #[test]
    fn provider_trait_is_object_safe() {
        fn _takes(_: std::sync::Arc<dyn Provider>) {}
    }

    #[test]
    fn entitlement_detection_is_narrow() {
        for (msg, want) in [
            // From bro's anthropic.test.ts.
            (
                "OAuth authentication is currently not allowed for this organization.",
                true,
            ),
            ("Request blocked by safety filter.", false),
            // One per keyword, case-insensitive.
            ("Your SUBSCRIPTION has ended", true),
            ("Billing issue on file", true),
            ("Your credit balance is too low to access the API", true),
            ("Organization disabled", true),
            // Narrow: near-misses and unrelated refusals stay generic access denials.
            ("credit is low", false),
            ("balance of credit", false),
            ("permission denied", false),
            ("rate limited", false),
            ("", false),
        ] {
            assert_eq!(is_entitlement_message(msg), want, "{msg:?}");
        }
    }
}
