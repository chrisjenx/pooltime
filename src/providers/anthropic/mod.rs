//! Anthropic (Claude Max) provider.
//!
//! Task 15 provides OAuth access-token handling ([`oauth`]); Task 16 adds rate-limit header
//! parsing ([`ratelimit`]) and the usage poll + sweeper ([`usage`]). `access_token` and
//! `poll_usage` are *inherent* methods; Task 18 adds `impl Provider for AnthropicProvider`
//! ([`proxy`]: non-stream attempt + classification, overload backoff in [`overload`]) whose trait
//! `access_token` / `poll_usage` delegate to them.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::accounts::{AccountId, Registry, StateBook};
use crate::config::AnthropicCfg;
use crate::upstream::Upstream;

mod oauth;
mod overload;
mod proxy;
mod ratelimit;
pub mod strip;
mod usage;

pub use oauth::expiry_after_refresh;
pub use overload::{explicit_reset, retry_after_at, OverloadPolicy, Sleeper, TokioSleeper};
pub use proxy::{
    anthropic_error, refresh_failure_outcome, POOL_ACCOUNT_HEADER, RETRYABLE_TRANSPORT_HEADER,
    UPSTREAM_REJECTED_HEADER,
};
pub use ratelimit::{has_quota_signal, parse_unified_headers};
pub use strip::{
    codex_stash_signature, is_codex_stash_signature, prepare_body, CODEX_STASH_PREFIX,
};
pub use usage::{spawn_usage_sweeper, SWEEP_INTERVAL};

pub struct AnthropicProvider {
    cfg: AnthropicCfg,
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
    /// One in-flight usage poll per account within this process (see [`usage`]).
    usage_gates: Mutex<HashMap<AccountId, Arc<tokio::sync::Mutex<()>>>>,
    /// Same-account backoff for 529/500/503.
    overload: overload::OverloadPolicy,
    /// Bench length for a quota 429 with no explicit or window reset (default 1 h).
    rate_limit_cooldown: time::Duration,
}

impl AnthropicProvider {
    /// Defaults: 300 s skew, 30 s token timeout (the `Timeouts` defaults); override with
    /// [`AnthropicProvider::with_refresh_timing`].
    pub fn new(
        cfg: AnthropicCfg,
        reg: Arc<Registry>,
        state: Arc<StateBook>,
        up: Arc<Upstream>,
    ) -> Self {
        AnthropicProvider {
            cfg,
            reg,
            state,
            up,
            skew: time::Duration::seconds(300),
            refresh_timeout: Duration::from_secs(30),
            gates: Mutex::new(HashMap::new()),
            inflight: Arc::default(),
            usage_gates: Mutex::new(HashMap::new()),
            overload: overload::OverloadPolicy::default(),
            rate_limit_cooldown: time::Duration::hours(1),
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
