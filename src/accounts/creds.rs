//! Stored OAuth credentials (`credentials.json`, 0600).

use std::fmt;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::providers::ProviderId;

#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub struct Credentials {
    pub provider: ProviderId,
    pub access_token: String,
    #[serde(default)]
    pub refresh_token: Option<String>,
    #[serde(default, with = "time::serde::rfc3339::option")]
    pub expires_at: Option<OffsetDateTime>,
    #[serde(default, with = "time::serde::rfc3339::option")]
    pub refresh_expires_at: Option<OffsetDateTime>,
    #[serde(default)]
    pub scopes: Vec<String>,
    #[serde(default)]
    pub plan: Option<String>,
    #[serde(default)]
    pub tier: Option<String>,
    #[serde(default)]
    pub openai_account_id: Option<String>,
}

/// Hand-written so tokens can never reach a log line via `{:?}`.
impl fmt::Debug for Credentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Credentials")
            .field("provider", &self.provider)
            .field("access_token", &"<redacted>")
            .field(
                "refresh_token",
                &self.refresh_token.as_ref().map(|_| "<redacted>"),
            )
            .field("expires_at", &self.expires_at)
            .field("refresh_expires_at", &self.refresh_expires_at)
            .field("scopes", &self.scopes)
            .field("plan", &self.plan)
            .field("tier", &self.tier)
            .field("openai_account_id", &self.openai_account_id)
            .finish()
    }
}

/// `credentials.json` exists but cannot be used. Never mapped to "logged out":
/// callers surface it as an account status reason (INV §10.9).
#[derive(Debug, thiserror::Error)]
pub enum CredsError {
    /// `error` is a sanitized description; it never echoes file content.
    #[error("credentials file {} is corrupt: {error}", path.display())]
    Corrupt { path: PathBuf, error: String },
}
