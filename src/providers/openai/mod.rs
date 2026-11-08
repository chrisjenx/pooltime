//! OpenAI (ChatGPT subscription / Codex) provider.
//!
//! Task 35 provides OAuth access-token handling ([`oauth`]: JSON refresh grant, dead-token
//! detection, `~/.codex/auth.json` import, id_token claims). The browser sign-in lives in
//! [`crate::signin::openai`]. Later tasks add request/stream translation and `impl Provider`.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::accounts::{AccountId, Registry, StateBook};
use crate::config::OpenAiCfg;
use crate::upstream::Upstream;

mod context;
mod oauth;
mod translate_req;
mod translate_stream;

pub use context::ContextCarry;
#[cfg(test)]
pub(crate) use oauth::testkit;
pub use oauth::{
    codex_auth_json_import, default_codex_auth_path, id_token_claims, CodexAuthImport,
    IdTokenClaims,
};
pub(crate) use oauth::{token_expiry, TokenResponse};
pub use translate_req::{clamp_effort, derive_effort_tier, to_codex_request};
pub use translate_stream::{anthropic_error_type, CodexToAnthropic, Finished, StreamError};

pub struct OpenAiProvider {
    cfg: OpenAiCfg,
    reg: Arc<Registry>,
    state: Arc<StateBook>,
    up: Arc<Upstream>,
    /// Refresh an access token this long before it expires (config default 300 s).
    skew: time::Duration,
    /// Bound for one token-endpoint call (config default 30 s).
    refresh_timeout: Duration,
    /// Serialises refresh tasks of one account within this process.
    gates: Mutex<HashMap<AccountId, Arc<tokio::sync::Mutex<()>>>>,
    /// Shared spawned refresh task per (account, force) (see [`oauth`]).
    inflight: oauth::InFlightMap,
}

impl OpenAiProvider {
    /// Defaults: 300 s skew, 30 s token timeout (the `Timeouts` defaults); override with
    /// [`OpenAiProvider::with_refresh_timing`].
    pub fn new(
        cfg: OpenAiCfg,
        reg: Arc<Registry>,
        state: Arc<StateBook>,
        up: Arc<Upstream>,
    ) -> Self {
        OpenAiProvider {
            cfg,
            reg,
            state,
            up,
            skew: time::Duration::seconds(300),
            refresh_timeout: Duration::from_secs(30),
            gates: Mutex::new(HashMap::new()),
            inflight: Arc::default(),
        }
    }

    /// Set the refresh skew and token-endpoint timeout (`Timeouts::token_refresh_skew` /
    /// `token_refresh_timeout`).
    pub fn with_refresh_timing(mut self, skew: Duration, timeout: Duration) -> Self {
        self.skew = time::Duration::try_from(skew).unwrap_or(time::Duration::MAX);
        self.refresh_timeout = timeout;
        self
    }
}
