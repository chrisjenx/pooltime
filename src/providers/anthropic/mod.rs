//! Anthropic (Claude Max) provider.
//!
//! Task 15 provides OAuth access-token handling ([`oauth`]). `access_token` is an *inherent*
//! method for now; Task 18 adds `impl Provider for AnthropicProvider` (attempt / usage / catalog)
//! and its trait `access_token` simply delegates here.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::accounts::{AccountId, Registry, StateBook};
use crate::config::AnthropicCfg;
use crate::upstream::Upstream;

mod oauth;

pub use oauth::expiry_after_refresh;

pub struct AnthropicProvider {
    cfg: AnthropicCfg,
    reg: Arc<Registry>,
    state: Arc<StateBook>,
    up: Arc<Upstream>,
    /// Refresh an access token this long before it expires (config default 300 s).
    skew: time::Duration,
    /// Bound for one token-endpoint call (config default 30 s).
    refresh_timeout: Duration,
    /// One in-flight refresh per account within this process.
    gates: Mutex<HashMap<AccountId, Arc<tokio::sync::Mutex<()>>>>,
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
