//! OpenAI (ChatGPT subscription / Codex) provider.
//!
//! Task 35 provides OAuth access-token handling ([`oauth`]: JSON refresh grant, dead-token
//! detection, `~/.codex/auth.json` import, id_token claims). The browser sign-in lives in
//! [`crate::signin::openai`]. Task 36/37 add request ([`translate_req`]) and stream
//! ([`translate_stream`]) translation; Task 38 adds `impl Provider` ([`proxy`]: the upstream
//! call and response classification, [`windows`]: rate-limit headers, [`stream`]: pre-commit
//! buffering, the keep-alive pump and non-streaming folding).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::accounts::{AccountId, Registry, StateBook};
use crate::config::OpenAiCfg;
use crate::providers::anthropic::OverloadPolicy;
use crate::upstream::Upstream;

mod context;
mod oauth;
mod proxy;
mod stream;
mod translate_req;
mod translate_stream;
pub mod windows;

pub use context::ContextCarry;
#[cfg(test)]
pub(crate) use oauth::testkit;
pub use oauth::{
    codex_auth_json_import, default_codex_auth_path, id_token_claims, CodexAuthImport,
    IdTokenClaims,
};
pub(crate) use oauth::{token_expiry, TokenResponse};
pub use proxy::{describe_codex_error, RESPONSES_PATH};
pub use stream::PREFIX_CAP;
pub use translate_req::{clamp_effort, derive_effort_tier, to_codex_request};
pub use translate_stream::{anthropic_error_type, CodexToAnthropic, Finished, StreamError};

/// Default keep-alive cadence on a committed Codex stream (`timeouts.stream_keepalive`).
pub const DEFAULT_KEEPALIVE: Duration = Duration::from_secs(1);

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
    /// Same-account backoff for Codex 5xx (and pre-commit overload events).
    overload: OverloadPolicy,
    /// Bench length for a rate limit with no future window reset or `retry-after` (1 h).
    rate_limit_cooldown: time::Duration,
    /// A `ping` is sent on a committed stream after this much silence.
    keepalive: Duration,
    /// Previous-turn context per (session, route) (see [`context`]).
    carry: ContextCarry,
}

impl OpenAiProvider {
    /// Defaults: 300 s skew, 30 s token timeout (the `Timeouts` defaults), default overload
    /// policy, 1 h rate-limit cooldown, 1 s keep-alive; override with the `with_*` builders.
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
            overload: OverloadPolicy::default(),
            rate_limit_cooldown: time::Duration::hours(1),
            keepalive: DEFAULT_KEEPALIVE,
            carry: ContextCarry::new(),
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
