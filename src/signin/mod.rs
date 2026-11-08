//! Interactive OAuth sign-in (runs inside the daemon, driven by the management API; an in-process
//! fallback serves `pooltime accounts login` when no daemon is running).
//!
//! A flow is started with [`SignInFlow::start`], which returns a [`PendingSignIn`]: the URL the user
//! opens, an expiry, and a completion that resolves to a [`SignInResult`]. Two ways to finish:
//!
//! - [`SignInMode::Callback`]: a one-shot listener on `127.0.0.1:<ephemeral>/callback` receives the
//!   browser redirect (see [`callback`]).
//! - [`SignInMode::PasteCode`]: the user pastes the `code#state` line shown on the manual redirect
//!   page into [`PendingSignIn::submit_code`] (also works in Callback mode, e.g. when the browser
//!   runs on another machine).
//!
//! After the code exchange the signed-in identity is fetched and checked against the account's
//! stored baseline (`accounts::identity`): same account -> `Saved`; another account -> `Mismatch`
//! (nothing saved); no baseline -> `NeedsBinding` (credentials held in memory until
//! [`SignInHandle::bind`] confirms, or five minutes pass).
//!
//! Keeping the in-flight flows addressable by id (for HTTP handlers that only have an id) is the
//! management API's job (Task 25): keep the [`SignInHandle`] from [`PendingSignIn::handle`] in a
//! map and drive [`PendingSignIn::wait`] from a spawned task. Nothing here logs a code, verifier or
//! token.

pub mod callback;
pub mod claude;
pub mod pkce;

use std::fmt;
use std::sync::Arc;

use futures_util::future::BoxFuture;
use time::OffsetDateTime;
use tokio::sync::oneshot;

use crate::accounts::{AccountId, Identity};

/// How long a started flow waits for the user (and how long `NeedsBinding` credentials are held).
pub const FLOW_TTL: std::time::Duration = std::time::Duration::from_secs(300);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignInMode {
    /// Localhost redirect listener (the browser is on this machine).
    Callback,
    /// Manual redirect; the user pastes `code#state`.
    PasteCode,
}

/// How a flow ended. Never carries a token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SignInResult {
    /// Credentials saved; `identity` is the account's (merged) identity now on file.
    Saved { identity: Identity },
    /// A different account signed in. Nothing was saved.
    Mismatch { expected: Identity, got: Identity },
    /// The account has no (comparable) identity on file. The credentials are held in memory until
    /// [`SignInHandle::bind`] confirms them.
    NeedsBinding { got: Identity },
    /// The flow failed or expired (a user-presentable reason, free of secrets).
    Failed(String),
}

/// Why [`PendingSignIn::submit_code`] refused its input (the flow stays usable except for
/// `AlreadyUsed`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum SubmitError {
    #[error("expected the pasted line to look like <code>#<state>")]
    Malformed,
    #[error("the pasted code belongs to a different sign-in (state mismatch)")]
    StateMismatch,
    #[error("this sign-in already received its code or has expired")]
    AlreadyUsed,
}

/// Starts sign-in flows for one provider.
pub trait SignInFlow: Send + Sync {
    /// Begin a flow for `account`. `hint` pre-populates the login page's email (`login_hint`);
    /// `None` falls back to the email on file. Must be called inside a tokio runtime.
    fn start(
        &self,
        account: &AccountId,
        hint: Option<&str>,
        mode: SignInMode,
    ) -> anyhow::Result<PendingSignIn>;
}

/// Provider-specific control surface of a running flow.
pub(crate) trait FlowControl: Send + Sync {
    fn submit_code(&self, pasted: &str) -> Result<(), SubmitError>;
    fn bind(&self) -> BoxFuture<'static, anyhow::Result<Identity>>;
}

/// A cheap, cloneable way to drive a running flow without owning its completion.
#[derive(Clone)]
pub struct SignInHandle {
    ctl: Arc<dyn FlowControl>,
}

impl SignInHandle {
    /// Hand the pasted `code#state` (or the full redirect URL) to the flow. Returns once the input
    /// is accepted; the outcome arrives through [`PendingSignIn::wait`].
    pub fn submit_code(&self, pasted: &str) -> Result<(), SubmitError> {
        self.ctl.submit_code(pasted)
    }

    /// Confirm a `NeedsBinding` result: save the held credentials and record the identity as the
    /// account's baseline. One-shot; errors when nothing is held (never needed, already bound, or
    /// the five-minute window passed).
    pub async fn bind(&self) -> anyhow::Result<Identity> {
        self.ctl.bind().await
    }
}

pub struct PendingSignIn {
    pub id: String,
    pub authorize_url: String,
    pub expires_at: OffsetDateTime,
    completion: oneshot::Receiver<SignInResult>,
    handle: SignInHandle,
}

impl fmt::Debug for PendingSignIn {
    /// The URL embeds the PKCE challenge and state, so only the id is shown.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PendingSignIn")
            .field("id", &self.id)
            .field("expires_at", &self.expires_at)
            .finish_non_exhaustive()
    }
}

impl PendingSignIn {
    pub(crate) fn new(
        id: String,
        authorize_url: String,
        expires_at: OffsetDateTime,
        completion: oneshot::Receiver<SignInResult>,
        ctl: Arc<dyn FlowControl>,
    ) -> PendingSignIn {
        PendingSignIn {
            id,
            authorize_url,
            expires_at,
            completion,
            handle: SignInHandle { ctl },
        }
    }

    /// A clone of the control handle (`submit_code`, `bind`).
    pub fn handle(&self) -> SignInHandle {
        self.handle.clone()
    }

    pub fn submit_code(&self, pasted: &str) -> Result<(), SubmitError> {
        self.handle.submit_code(pasted)
    }

    pub async fn bind(&self) -> anyhow::Result<Identity> {
        self.handle.bind().await
    }

    /// Resolves when the flow ends (always with a result: expiry is a `Failed`).
    pub async fn wait(self) -> SignInResult {
        self.completion
            .await
            .unwrap_or_else(|_| SignInResult::Failed("sign-in was abandoned".into()))
    }
}

/// Split what the user pasted into `(code, state)`: the manual page's `code#state` line, or (a
/// fallback when the localhost redirect could not be reached) the full redirect URL whose query
/// carries `code` and `state`. Both parts are required.
pub(crate) fn parse_pasted(raw: &str) -> Option<(String, String)> {
    let raw = raw.trim();
    if raw.starts_with("http://") || raw.starts_with("https://") {
        let url = reqwest::Url::parse(raw).ok()?;
        let get = |k: &str| {
            url.query_pairs()
                .find(|(key, v)| key == k && !v.is_empty())
                .map(|(_, v)| v.into_owned())
        };
        return Some((get("code")?, get("state")?));
    }
    let (code, state) = raw.split_once('#')?;
    let (code, state) = (code.trim(), state.trim());
    (!code.is_empty() && !state.is_empty()).then(|| (code.to_owned(), state.to_owned()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_code_hash_state_and_redirect_urls() {
        assert_eq!(
            parse_pasted("  abc#def\n"),
            Some(("abc".into(), "def".into()))
        );
        assert_eq!(
            parse_pasted("http://localhost:1234/callback?code=abc&state=def"),
            Some(("abc".into(), "def".into()))
        );
        for bad in [
            "",
            "abc",
            "#def",
            "abc#",
            "http://localhost:1/callback?code=abc",
            "http://localhost:1/callback?state=def",
        ] {
            assert_eq!(parse_pasted(bad), None, "{bad:?}");
        }
    }
}
