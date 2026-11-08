//! ChatGPT (Codex) PKCE sign-in, as the Codex CLI does it (bro `openai-login.ts`, codex-rs
//! `login/src/server.rs`): bind the localhost redirect listener on 1455, else 1457, **before**
//! building `redirect_uri = http://localhost:<port>/auth/callback` (only those ports are
//! registered for the client id); authorize with the Codex CLI's parameters; exchange the code
//! form-encoded; read the account from the id_token claims.
//!
//! Identity: the ChatGPT account id (`chatgpt_account_id`) names the workspace, shared by Team
//! members, so it fills the `org_uuid` slot of [`Identity`]; the user id (`chatgpt_user_id`)
//! fills `account_uuid`; plus the email claim. [`verify_openai`] compares them.
//!
//! Only one ChatGPT sign-in is pending per process (the ports are fixed): starting one
//! supersedes the previous (it fails "superseded by a new sign-in" and its listener closes).
//!
//! Paste mode: OpenAI has no manual "copy this code" page, so the user pastes the URL the browser
//! was redirected to (`parse_pasted` accepts it). The listener is still bound when possible (a
//! same-machine browser then finishes on its own); when both ports are busy, paste mode goes on
//! without a listener using the first port's redirect URI.
//!
//! Flow state machine as in [`super::claude`]: one claim of the code, expiry only from
//! `Awaiting`, `NeedsBinding` credentials held for one TTL.

use std::sync::{Arc, Mutex, MutexGuard};

use anyhow::{anyhow, bail, Context};
use futures_util::future::BoxFuture;
use futures_util::FutureExt;
use rand::RngExt;
use time::OffsetDateTime;
use tokio::sync::{oneshot, watch};
use tokio::time::Instant;

use super::callback::{self, page_html, CallbackEvent, Handler};
use super::pkce::{random_token, state_eq, Pkce};
use super::{
    parse_pasted, FlowControl, PendingSignIn, SignInFlow, SignInMode, SignInResult, SubmitError,
    FLOW_TTL,
};
use crate::accounts::{AccountId, Credentials, Identity, Registry, StateBook, Verify};
use crate::config::OpenAiCfg;
use crate::providers::openai::{id_token_claims, token_expiry, TokenResponse};
use crate::providers::ProviderId;
use crate::upstream::{Limits, Upstream};

/// Redirect path of the Codex CLI's login server.
pub const CALLBACK_PATH: &str = "/auth/callback";
/// Token-exchange timeout.
const TOKEN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);
/// How long [`OpenAiSignIn::start_superseding`] waits for a superseded listener to close.
const SUPERSEDE_WAIT: std::time::Duration = std::time::Duration::from_secs(2);
/// Token bodies are small; never accept more than this.
const MAX_TOKEN_BODY: usize = 256 * 1024;

/// Starts ChatGPT sign-ins.
pub struct OpenAiSignIn {
    cfg: OpenAiCfg,
    registry: Arc<Registry>,
    state: Arc<StateBook>,
    upstream: Arc<Upstream>,
    /// The pending flow a new start supersedes (shared by every `OpenAiSignIn` of a process
    /// through [`OpenAiSignIn::with_active`]).
    active: ActiveSignIn,
    /// Test seam: fixed `(verifier, state)` instead of random ones.
    #[cfg(test)]
    fixed: Option<(String, String)>,
}

/// The one ChatGPT sign-in a process may have pending: the callback ports are fixed, so a new
/// sign-in ends the previous one (and frees its port). Cheap to clone; clones share the slot.
#[derive(Clone, Default)]
pub struct ActiveSignIn(Arc<Mutex<Option<Active>>>);

struct Active {
    flow: std::sync::Weak<Flow>,
    /// The callback server task (ends once its listener is closed and connections drained).
    server: Option<tokio::task::JoinHandle<()>>,
}

/// Both callback ports are taken (mapped to 409 by the management API).
#[derive(Debug, thiserror::Error)]
#[error(
    "sign-in callback ports {0:?} are all in use (another Codex/pooltime login?); close it and retry"
)]
pub struct CallbackPortsBusy(pub Vec<u16>);

impl OpenAiSignIn {
    pub fn new(
        cfg: OpenAiCfg,
        registry: Arc<Registry>,
        state: Arc<StateBook>,
        upstream: Arc<Upstream>,
    ) -> OpenAiSignIn {
        OpenAiSignIn {
            cfg,
            registry,
            state,
            upstream,
            active: ActiveSignIn::default(),
            #[cfg(test)]
            fixed: None,
        }
    }

    /// Share the pending-flow slot with other instances (the management API keeps one).
    pub fn with_active(mut self, active: ActiveSignIn) -> OpenAiSignIn {
        self.active = active;
        self
    }

    /// Supersede the pending flow, wait (bounded) until its listener has let go of its port,
    /// then [`SignInFlow::start`]. Use this from async callers so the new flow gets the
    /// preferred port.
    pub async fn start_superseding(
        &self,
        account: &AccountId,
        hint: Option<&str>,
        mode: SignInMode,
    ) -> anyhow::Result<PendingSignIn> {
        if let Some(server) = self.supersede() {
            let _ = tokio::time::timeout(SUPERSEDE_WAIT, server).await;
        }
        self.start(account, hint, mode)
    }

    /// End the pending flow (if any): an unclaimed one fails as superseded; one already
    /// exchanging its code finishes on its own. Either way its listener closes. Returns the
    /// callback server task to await.
    fn supersede(&self) -> Option<tokio::task::JoinHandle<()>> {
        let old = lock(&self.active.0).take()?;
        if let Some(flow) = old.flow.upgrade() {
            flow.supersede();
        }
        old.server
    }

    #[cfg(test)]
    fn with_fixed_secrets(mut self, verifier: &str, state: &str) -> OpenAiSignIn {
        self.fixed = Some((verifier.into(), state.into()));
        self
    }

    fn secrets(&self) -> (Pkce, String) {
        #[cfg(test)]
        if let Some((v, s)) = &self.fixed {
            return (Pkce::from_verifier(v.clone()), s.clone());
        }
        (Pkce::generate(), random_token())
    }

    /// The authorize URL, parameters as the Codex CLI's `build_authorize_url` (bro
    /// `openai-login.ts`). No `login_hint`: the Codex CLI does not send one.
    fn authorize_url(
        &self,
        redirect_uri: &str,
        challenge: &str,
        state: &str,
    ) -> anyhow::Result<String> {
        let mut url = reqwest::Url::parse(&self.cfg.authorize_url)
            .context("openai authorize_url is not a valid URL")?;
        url.query_pairs_mut()
            .append_pair("response_type", "code")
            .append_pair("client_id", &self.cfg.client_id)
            .append_pair("redirect_uri", redirect_uri)
            .append_pair("scope", &self.cfg.login_scope)
            .append_pair("code_challenge", challenge)
            .append_pair("code_challenge_method", "S256")
            .append_pair("id_token_add_organizations", "true")
            .append_pair("codex_cli_simplified_flow", "true")
            .append_pair("state", state)
            .append_pair("originator", &self.cfg.originator);
        Ok(url.into())
    }

    /// Bind the redirect listener on the first free configured port.
    fn bind(&self) -> Result<callback::BoundCallback, CallbackPortsBusy> {
        callback::bind_first(&self.cfg.callback_ports, CALLBACK_PATH)
            .map_err(|_| CallbackPortsBusy(self.cfg.callback_ports.clone()))
    }
}

impl SignInFlow for OpenAiSignIn {
    /// `hint` is ignored (the ChatGPT login page takes none).
    fn start(
        &self,
        account: &AccountId,
        _hint: Option<&str>,
        mode: SignInMode,
    ) -> anyhow::Result<PendingSignIn> {
        let meta = self
            .registry
            .get(account)
            .ok_or_else(|| anyhow!("unknown account {account}"))?;
        if meta.provider != ProviderId::OpenAi {
            bail!("account {account} is not an OpenAI account");
        }
        // One pending ChatGPT sign-in per process (fixed ports); async callers use
        // `start_superseding`, which also waits for the old listener to close.
        drop(self.supersede());

        let bound = match mode {
            SignInMode::Callback => Some(self.bind()?),
            SignInMode::PasteCode => self.bind().ok(),
        };
        let port = match &bound {
            Some(b) => b.port(),
            None => *self
                .cfg
                .callback_ports
                .first()
                .context("no sign-in callback port configured")?,
        };
        let redirect_uri = format!("http://localhost:{port}{CALLBACK_PATH}");
        let (pkce, oauth_state) = self.secrets();
        let authorize_url = self.authorize_url(&redirect_uri, &pkce.challenge, &oauth_state)?;

        let (tx, rx) = oneshot::channel();
        let (done, _) = watch::channel(false);
        let flow = Arc::new(Flow {
            account: account.clone(),
            cfg: self.cfg.clone(),
            registry: self.registry.clone(),
            state: self.state.clone(),
            upstream: self.upstream.clone(),
            verifier: pkce.verifier,
            oauth_state: oauth_state.clone(),
            redirect_uri,
            phase: Mutex::new(Phase::Awaiting),
            tx: Mutex::new(Some(tx)),
            held: Mutex::new(None),
            done,
        });

        let server = bound.map(|bound| {
            let f = flow.clone();
            let handler: Handler = Arc::new(move |ev| {
                let f = f.clone();
                async move { f.on_callback(ev).await }.boxed()
            });
            let mut closed = flow.done.subscribe();
            bound.serve(oauth_state, handler, async move {
                let _ = closed.wait_for(|d| *d).await;
            })
        });
        // A flow started concurrently since the supersede above is superseded in turn.
        let displaced = lock(&self.active.0).replace(Active {
            flow: Arc::downgrade(&flow),
            server,
        });
        if let Some(old) = displaced.and_then(|a| a.flow.upgrade()) {
            old.supersede();
        }

        let f = flow.clone();
        let mut closed = flow.done.subscribe();
        tokio::spawn(async move {
            tokio::select! {
                _ = tokio::time::sleep(FLOW_TTL) => {
                    if f.claim() {
                        f.finish(SignInResult::Failed(
                            "sign-in expired (no code received within 5 minutes)".into(),
                        ));
                    }
                }
                _ = async { let _ = closed.wait_for(|d| *d).await; } => {}
            }
        });

        let id = hex::encode(rand::rng().random::<[u8; 16]>());
        Ok(PendingSignIn::new(
            id,
            authorize_url,
            OffsetDateTime::now_utc() + FLOW_TTL,
            rx,
            Arc::new(Ctl(flow)),
        ))
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    Awaiting,
    Exchanging,
    Done,
}

/// Credentials exchanged for a not-yet-confirmed identity (`NeedsBinding`).
struct Held {
    creds: Credentials,
    got: Identity,
}

struct Flow {
    account: AccountId,
    cfg: OpenAiCfg,
    registry: Arc<Registry>,
    state: Arc<StateBook>,
    upstream: Arc<Upstream>,
    verifier: String,
    oauth_state: String,
    redirect_uri: String,
    phase: Mutex<Phase>,
    tx: Mutex<Option<oneshot::Sender<SignInResult>>>,
    held: Mutex<Option<Held>>,
    done: watch::Sender<bool>,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl Flow {
    fn claim(&self) -> bool {
        let mut p = lock(&self.phase);
        if *p == Phase::Awaiting {
            *p = Phase::Exchanging;
            true
        } else {
            false
        }
    }

    /// A newer sign-in replaces this one: an unclaimed flow fails as superseded; one already
    /// exchanging its code finishes on its own. Either way the listener closes.
    fn supersede(self: &Arc<Self>) {
        if self.claim() {
            self.finish(SignInResult::Failed("superseded by a new sign-in".into()));
        } else {
            self.done.send_replace(true);
        }
    }

    fn finish(self: &Arc<Self>, r: SignInResult) {
        {
            let mut p = lock(&self.phase);
            if *p == Phase::Done {
                return;
            }
            *p = Phase::Done;
        }
        if matches!(r, SignInResult::NeedsBinding { .. }) {
            let f = self.clone();
            tokio::spawn(async move {
                tokio::time::sleep(FLOW_TTL).await;
                lock(&f.held).take();
            });
        }
        if let Some(tx) = lock(&self.tx).take() {
            let _ = tx.send(r);
        }
        self.done.send_replace(true);
    }

    async fn run(self: Arc<Self>, code: String) -> SignInResult {
        let r = self.exchange(&code).await;
        self.finish(r.clone());
        r
    }

    async fn on_callback(self: Arc<Self>, ev: CallbackEvent) -> (axum::http::StatusCode, String) {
        use axum::http::StatusCode;
        if !self.claim() {
            return (
                StatusCode::BAD_REQUEST,
                page_html("This sign-in already finished or expired."),
            );
        }
        let r = match ev {
            CallbackEvent::Code(code) => self.clone().run(code).await,
            CallbackEvent::Denied { error, .. } => {
                let r = SignInResult::Failed(format!("authorization was not granted ({error})"));
                self.finish(r.clone());
                r
            }
        };
        match r {
            SignInResult::Saved { .. } => (
                StatusCode::OK,
                page_html("Signed in. You can close this tab and return to pooltime."),
            ),
            SignInResult::NeedsBinding { .. } => (
                StatusCode::OK,
                page_html("Signed in. Return to pooltime to confirm which account this is."),
            ),
            SignInResult::Mismatch { .. } => (
                StatusCode::CONFLICT,
                page_html(
                    "This is a different account than the one being re-logged in. \
                     Nothing was saved; return to pooltime.",
                ),
            ),
            SignInResult::Failed(_) => (
                StatusCode::BAD_REQUEST,
                page_html("Sign-in failed. Return to pooltime for details."),
            ),
        }
    }

    /// The form-encoded code exchange (codex-rs `exchange_code_for_tokens`).
    fn token_request(&self, code: &str) -> anyhow::Result<reqwest::Request> {
        let url: reqwest::Url = self
            .cfg
            .token_url
            .parse()
            .context("openai token_url is not a valid URL")?;
        let body = reqwest::Url::parse_with_params(
            "http://form.invalid/",
            [
                ("grant_type", "authorization_code"),
                ("code", code),
                ("redirect_uri", self.redirect_uri.as_str()),
                ("client_id", self.cfg.client_id.as_str()),
                ("code_verifier", self.verifier.as_str()),
            ],
        )
        .context("encoding the token request")?
        .query()
        .unwrap_or_default()
        .to_owned();
        let mut req = reqwest::Request::new(reqwest::Method::POST, url);
        req.headers_mut().insert(
            http::header::CONTENT_TYPE,
            http::HeaderValue::from_static("application/x-www-form-urlencoded"),
        );
        *req.body_mut() = Some(reqwest::Body::from(bytes::Bytes::from(body)));
        Ok(req)
    }

    /// Code -> tokens -> id_token identity -> verify -> save/hold. Failure strings carry only the
    /// HTTP status and request id, never upstream text.
    async fn exchange(&self, code: &str) -> SignInResult {
        let failed = SignInResult::Failed;
        let req = match self.token_request(code) {
            Ok(r) => r,
            Err(e) => return failed(format!("{e:#}")),
        };
        let lim = Limits {
            header_wait: TOKEN_TIMEOUT,
            idle: TOKEN_TIMEOUT,
            deadline: Instant::now() + TOKEN_TIMEOUT,
        };
        let resp = match self.upstream.send(req, &lim).await {
            Ok(r) => r,
            Err(k) => return failed(format!("token exchange failed ({k:?})")),
        };
        let status = resp.status.as_u16();
        let rid = ["x-request-id", "request-id"]
            .iter()
            .find_map(|h| resp.headers.get(*h))
            .and_then(|v| v.to_str().ok())
            .map(|r| format!(", request-id {r}"))
            .unwrap_or_default();
        let body = match resp.body.text().await {
            Ok(b) => b,
            Err(k) => return failed(format!("token exchange failed ({k:?}{rid})")),
        };
        if !(200..300).contains(&status) {
            return failed(if matches!(status, 400 | 401) {
                format!("the authorization code was rejected (HTTP {status}{rid}); start the sign-in again")
            } else {
                format!("token exchange failed (HTTP {status}{rid})")
            });
        }
        if body.len() > MAX_TOKEN_BODY {
            return failed("token response too large".into());
        }
        let Ok(tok) = serde_json::from_str::<TokenResponse>(&body) else {
            return failed(format!("token response was not valid JSON{rid}"));
        };
        let (Some(access), Some(refresh)) = (
            tok.access_token.filter(|t| !t.is_empty()),
            tok.refresh_token.filter(|t| !t.is_empty()),
        ) else {
            return failed(format!(
                "token response lacked an access or refresh token{rid}"
            ));
        };
        let claims = tok
            .id_token
            .as_deref()
            .and_then(id_token_claims)
            .unwrap_or_default();
        let got = claims.identity();
        if got.account_uuid.is_none() && got.email.is_none() && got.org_uuid.is_none() {
            return failed("could not verify the signed-in account: no identity reported".into());
        }
        let prev = {
            let (reg, id) = (self.registry.clone(), self.account.clone());
            tokio::task::spawn_blocking(move || reg.read_creds(&id).ok().flatten())
                .await
                .ok()
                .flatten()
        };
        let now = OffsetDateTime::now_utc();
        let creds = Credentials {
            provider: ProviderId::OpenAi,
            expires_at: Some(token_expiry(tok.expires_in, &access, now)),
            access_token: access,
            refresh_token: Some(refresh),
            refresh_expires_at: None,
            scopes: tok
                .scope
                .as_deref()
                .unwrap_or(&self.cfg.login_scope)
                .split_whitespace()
                .map(str::to_owned)
                .collect(),
            plan: claims
                .plan
                .clone()
                .or_else(|| prev.as_ref().and_then(|c| c.plan.clone())),
            tier: None,
            // Only what this sign-in proved: a stale workspace id would misroute requests.
            openai_account_id: claims.account_id.clone(),
        };

        let Some(meta) = self.registry.get(&self.account) else {
            return failed("the account was removed during sign-in".into());
        };
        match verify_openai(&meta.identity, &got) {
            Verify::Match => {
                let identity = merge(&meta.identity, got);
                match self.save(creds, identity.clone()).await {
                    Ok(()) => SignInResult::Saved { identity },
                    Err(e) => failed(format!("could not save the credentials: {e:#}")),
                }
            }
            Verify::Mismatch => SignInResult::Mismatch {
                expected: meta.identity,
                got,
            },
            Verify::NoBaseline => {
                *lock(&self.held) = Some(Held {
                    creds,
                    got: got.clone(),
                });
                SignInResult::NeedsBinding { got }
            }
        }
    }

    /// Persist new credentials under the account lock, retire a dead-refresh marker, record the
    /// identity baseline.
    async fn save(&self, creds: Credentials, identity: Identity) -> anyhow::Result<()> {
        let (reg, id) = (self.registry.clone(), self.account.clone());
        tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
            // Never recreate a deleted account's dir: refuse before writing anything.
            let _guard = reg.lock_existing(&id)?;
            reg.write_creds_unlocked(&id, &creds)
        })
        .await
        .context("saving credentials panicked")??;
        self.state
            .clear_refresh_dead(&self.account, OffsetDateTime::now_utc());
        // A first login turns `needs_login` into `ready` without any state change.
        self.state.notify_changed();

        let (reg, id) = (self.registry.clone(), self.account.clone());
        let meta_saved = tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
            let mut m = reg
                .get(&id)
                .ok_or_else(|| anyhow!("account {id} no longer exists"))?;
            m.identity = identity;
            reg.upsert(m)
        })
        .await;
        if !matches!(meta_saved, Ok(Ok(()))) {
            tracing::warn!(account = %self.account, "signed in but could not update the account identity");
        }
        Ok(())
    }

    async fn bind(self: Arc<Self>) -> anyhow::Result<Identity> {
        let held = lock(&self.held).take().ok_or_else(|| {
            anyhow!("nothing to confirm: the sign-in did not need binding, was already bound, or the confirmation window passed")
        })?;
        match self.save(held.creds.clone(), held.got.clone()).await {
            Ok(()) => Ok(held.got),
            Err(e) => {
                *lock(&self.held) = Some(held);
                Err(e)
            }
        }
    }
}

/// Compare a ChatGPT sign-in with the account's baseline (identity slots per
/// `IdTokenClaims::identity`: `account_uuid` = user id, `org_uuid` = workspace account id).
/// `accounts::verify` (Claude) is not used: the workspace id is shared by Team members, so it can
/// only rule a login *out*.
///
/// 1. Both have a workspace id and they differ: another subscription, `Mismatch`.
/// 2. Both have a user id: it decides.
/// 3. Both have an email: it decides (case-insensitive, trimmed), even when the workspace
///    ids match.
/// 4. Only the (equal) workspace ids are comparable: `Match`.
/// 5. Nothing comparable: `NoBaseline`.
pub fn verify_openai(expected: &Identity, got: &Identity) -> Verify {
    fn present(s: &Option<String>) -> Option<&str> {
        s.as_deref().map(str::trim).filter(|s| !s.is_empty())
    }
    let both = |f: fn(&Identity) -> &Option<String>| present(f(expected)).zip(present(f(got)));
    let workspace = both(|i| &i.org_uuid);
    if workspace.is_some_and(|(e, g)| e != g) {
        return Verify::Mismatch;
    }
    let decide = |same: bool| {
        if same {
            Verify::Match
        } else {
            Verify::Mismatch
        }
    };
    if let Some((e, g)) = both(|i| &i.account_uuid) {
        return decide(e == g);
    }
    if let Some((e, g)) = both(|i| &i.email) {
        return decide(e.eq_ignore_ascii_case(g));
    }
    if workspace.is_some() {
        return Verify::Match;
    }
    Verify::NoBaseline
}

/// `got` over `old`: fresh values win, fields not reported keep their old value.
fn merge(old: &Identity, got: Identity) -> Identity {
    Identity {
        account_uuid: got.account_uuid.or_else(|| old.account_uuid.clone()),
        email: got.email.or_else(|| old.email.clone()),
        org_uuid: got.org_uuid.or_else(|| old.org_uuid.clone()),
    }
}

struct Ctl(Arc<Flow>);

impl FlowControl for Ctl {
    fn submit_code(&self, pasted: &str) -> Result<(), SubmitError> {
        let (code, state) = parse_pasted(pasted).ok_or(SubmitError::Malformed)?;
        if !state_eq(&state, &self.0.oauth_state) {
            return Err(SubmitError::StateMismatch);
        }
        if !self.0.claim() {
            return Err(SubmitError::AlreadyUsed);
        }
        let flow = self.0.clone();
        tokio::spawn(async move {
            flow.run(code).await;
        });
        Ok(())
    }

    fn bind(&self) -> BoxFuture<'static, anyhow::Result<Identity>> {
        // Its own task: a confirm request that goes away mid-save must not drop the held
        // credentials half-saved.
        let task = tokio::spawn(self.0.clone().bind());
        async move {
            task.await
                .map_err(|_| anyhow!("saving the confirmed sign-in failed"))?
        }
        .boxed()
    }

    fn supersede(&self) {
        self.0.supersede();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    use crate::accounts::AccountMeta;
    use crate::config::Paths;
    use crate::providers::openai::testkit::{
        fake_token_endpoint, id_token, id_token_with_user, reply, FakeToken,
    };
    use crate::store::Store;

    const VERIFIER: &str = "test-verifier-0123456789-abcdefghijklmnopqrstuvwxyz";
    const STATE: &str = "test-state-fixed";
    const OLD_RT: &str = "old-openai-refresh";
    const ACCT: &str = "acct-aaaa";

    fn token_body(account_id: &str) -> String {
        serde_json::json!({
            "id_token": id_token(account_id, "me@example.com", "pro"),
            "access_token": "at-new",
            "refresh_token": "rt-new",
            "expires_in": 3600,
        })
        .to_string()
    }

    struct Fx {
        _dir: tempfile::TempDir,
        reg: Arc<Registry>,
        state: Arc<StateBook>,
        fake: FakeToken,
        flow: OpenAiSignIn,
        id: AccountId,
    }

    impl Fx {
        fn creds(&self) -> Credentials {
            self.reg.read_creds(&self.id).unwrap().unwrap()
        }
        fn meta(&self) -> AccountMeta {
            self.reg.get(&self.id).unwrap()
        }
    }

    /// An OpenAI account with `baseline` identity; the fake token endpoint signs in `account_id`.
    /// The callback listener binds any free port (`[0]`).
    async fn fx(baseline: Identity, account_id: &'static str) -> Fx {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::under(dir.path());
        paths.ensure().unwrap();
        let reg = Arc::new(Registry::load(&paths).unwrap());
        let store = Arc::new(Store::open(&dir.path().join("pooltime.db")).unwrap());
        let state = Arc::new(StateBook::load(store).unwrap());
        let id: AccountId = "codex".parse().unwrap();
        let mut meta = AccountMeta::new(id.clone(), ProviderId::OpenAi);
        meta.identity = baseline;
        reg.upsert(meta).unwrap();
        reg.write_creds(
            &id,
            &Credentials {
                provider: ProviderId::OpenAi,
                access_token: "old-access".into(),
                refresh_token: Some(OLD_RT.into()),
                expires_at: None,
                refresh_expires_at: None,
                scopes: vec![],
                plan: None,
                tier: None,
                openai_account_id: None,
            },
        )
        .unwrap();
        state.mark_refresh_dead(&id, OLD_RT, OffsetDateTime::now_utc());
        let fake = fake_token_endpoint(move |_| reply(200, token_body(account_id))).await;
        let cfg = OpenAiCfg {
            token_url: fake.url.clone(),
            callback_ports: vec![0],
            ..OpenAiCfg::default()
        };
        let flow = OpenAiSignIn::new(cfg, reg.clone(), state.clone(), Arc::new(Upstream::new()))
            .with_fixed_secrets(VERIFIER, STATE);
        Fx {
            _dir: dir,
            reg,
            state,
            fake,
            flow,
            id,
        }
    }

    fn baseline(account_id: &str) -> Identity {
        Identity {
            account_uuid: None,
            email: Some("me@example.com".into()),
            org_uuid: Some(account_id.into()),
        }
    }

    fn param(url: &str, key: &str) -> String {
        reqwest::Url::parse(url)
            .unwrap()
            .query_pairs()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.into_owned())
            .unwrap_or_else(|| panic!("no {key} in the URL"))
    }

    async fn hit_callback(redirect_uri: &str, query: &str) -> u16 {
        let url = format!("{redirect_uri}?{query}").replace("localhost", "127.0.0.1");
        reqwest::get(url).await.unwrap().status().as_u16()
    }

    #[test]
    fn production_ports_and_constants() {
        let c = OpenAiCfg::default();
        assert_eq!(c.callback_ports, [1455, 1457]);
        assert_eq!(c.authorize_url, "https://auth.openai.com/oauth/authorize");
        assert_eq!(c.token_url, "https://auth.openai.com/oauth/token");
        assert_eq!(c.client_id, "app_EMoamEEZ73f0CkXaXp7hrann");
        assert_eq!(c.scope, "openid profile email");
        assert_eq!(
            c.login_scope,
            "openid profile email offline_access api.connectors.read api.connectors.invoke"
        );
        assert_eq!(c.originator, "codex_cli_rs");
        assert_eq!(CALLBACK_PATH, "/auth/callback");
    }

    #[tokio::test]
    async fn binds_1455_then_1457() {
        // Stand-ins for 1455/1457 (the real ports may be busy on this machine): the first
        // candidate is held, so the flow must fall back to the second (`0` = any free port).
        let held = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let held_port = held.local_addr().unwrap().port();
        let mut fx = fx(baseline(ACCT), ACCT).await;
        fx.flow.cfg.callback_ports = vec![held_port, 0];
        let p = fx.flow.start(&fx.id, None, SignInMode::Callback).unwrap();

        // The redirect URI was built from the port actually bound...
        let redirect = param(&p.authorize_url, "redirect_uri");
        let u = reqwest::Url::parse(&redirect).unwrap();
        assert_eq!(u.host_str(), Some("localhost"));
        assert_eq!(u.path(), "/auth/callback");
        assert_ne!(u.port().unwrap(), held_port);
        // ...and that listener serves the callback path.
        assert_eq!(hit_callback(&redirect, "code=c1&state=wrong").await, 400);
        assert_eq!(
            hit_callback(&redirect, &format!("code=c1&state={STATE}")).await,
            200
        );
        assert!(matches!(p.wait().await, SignInResult::Saved { .. }));

        // Both candidates busy: callback mode refuses to start (and says why).
        let held2 = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let held2_port = held2.local_addr().unwrap().port();
        fx.flow.cfg.callback_ports = vec![held_port, held2_port];
        let e = fx
            .flow
            .start(&fx.id, None, SignInMode::Callback)
            .unwrap_err();
        assert!(format!("{e:#}").contains("in use"), "{e:#}");
        // Paste mode goes on without a listener, on the first port's redirect URI.
        let p = fx.flow.start(&fx.id, None, SignInMode::PasteCode).unwrap();
        assert_eq!(
            param(&p.authorize_url, "redirect_uri"),
            format!("http://localhost:{held_port}/auth/callback")
        );
        drop((held, held2));
    }

    #[tokio::test]
    async fn authorize_url_has_codex_params() {
        let fx = fx(baseline(ACCT), ACCT).await;
        let p = fx
            .flow
            .start(&fx.id, Some("ignored@example.com"), SignInMode::Callback)
            .unwrap();
        let url = reqwest::Url::parse(&p.authorize_url).unwrap();
        assert_eq!(
            format!(
                "{}://{}{}",
                url.scheme(),
                url.host_str().unwrap(),
                url.path()
            ),
            "https://auth.openai.com/oauth/authorize"
        );
        let keys: Vec<String> = url.query_pairs().map(|(k, _)| k.into_owned()).collect();
        assert_eq!(
            keys,
            [
                "response_type",
                "client_id",
                "redirect_uri",
                "scope",
                "code_challenge",
                "code_challenge_method",
                "id_token_add_organizations",
                "codex_cli_simplified_flow",
                "state",
                "originator",
            ]
        );
        let get = |k: &str| param(&p.authorize_url, k);
        assert_eq!(get("response_type"), "code");
        assert_eq!(get("client_id"), "app_EMoamEEZ73f0CkXaXp7hrann");
        assert_eq!(
            get("scope"),
            "openid profile email offline_access api.connectors.read api.connectors.invoke"
        );
        assert_eq!(
            get("code_challenge"),
            super::super::pkce::challenge(VERIFIER)
        );
        assert_eq!(get("code_challenge_method"), "S256");
        assert_eq!(get("id_token_add_organizations"), "true");
        assert_eq!(get("codex_cli_simplified_flow"), "true");
        assert_eq!(get("state"), STATE);
        assert_eq!(get("originator"), "codex_cli_rs");
    }

    #[tokio::test]
    async fn code_exchange_form_encoded() {
        let fx = fx(baseline(ACCT), ACCT).await;
        let p = fx.flow.start(&fx.id, None, SignInMode::Callback).unwrap();
        let redirect = param(&p.authorize_url, "redirect_uri");
        assert_eq!(
            hit_callback(&redirect, &format!("code=the-code&state={STATE}")).await,
            200
        );
        assert!(matches!(p.wait().await, SignInResult::Saved { .. }));

        assert_eq!(fx.fake.hits(), 1);
        let seen = fx.fake.seen(0);
        assert_eq!(seen.content_type, "application/x-www-form-urlencoded");
        let form: Vec<(String, String)> = reqwest::Url::parse(&format!("http://x/?{}", seen.body))
            .unwrap()
            .query_pairs()
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect();
        let want: Vec<(String, String)> = [
            ("grant_type", "authorization_code"),
            ("code", "the-code"),
            ("redirect_uri", redirect.as_str()),
            ("client_id", "app_EMoamEEZ73f0CkXaXp7hrann"),
            ("code_verifier", VERIFIER),
        ]
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
        assert_eq!(form, want);
    }

    #[tokio::test]
    async fn saved_login_stores_account_id_and_clears_dead_marker() {
        let fx = fx(baseline(ACCT), ACCT).await;
        let p = fx.flow.start(&fx.id, None, SignInMode::PasteCode).unwrap();
        let redirect = param(&p.authorize_url, "redirect_uri");
        // Paste the URL the browser was redirected to.
        p.submit_code(&format!("{redirect}?code=pasted&state={STATE}"))
            .unwrap();
        let SignInResult::Saved { identity } = p.wait().await else {
            panic!("expected Saved")
        };
        assert_eq!(identity.org_uuid.as_deref(), Some(ACCT));
        let c = fx.creds();
        assert_eq!(c.provider, ProviderId::OpenAi);
        assert_eq!(c.access_token, "at-new");
        assert_eq!(c.refresh_token.as_deref(), Some("rt-new"));
        assert_eq!(c.openai_account_id.as_deref(), Some(ACCT));
        assert_eq!(c.plan.as_deref(), Some("pro"));
        assert!(c.expires_at.is_some());
        assert!(!fx.state.is_refresh_dead(&fx.id, OLD_RT));
    }

    #[tokio::test]
    async fn other_account_id_is_a_mismatch_and_saves_nothing() {
        let fx = fx(baseline(ACCT), "acct-bbbb").await;
        let p = fx.flow.start(&fx.id, None, SignInMode::Callback).unwrap();
        let redirect = param(&p.authorize_url, "redirect_uri");
        assert_eq!(
            hit_callback(&redirect, &format!("code=c&state={STATE}")).await,
            409
        );
        let SignInResult::Mismatch { got, .. } = p.wait().await else {
            panic!("expected Mismatch")
        };
        assert_eq!(got.org_uuid.as_deref(), Some("acct-bbbb"));
        assert_eq!(fx.creds().access_token, "old-access");
        assert!(fx.state.is_refresh_dead(&fx.id, OLD_RT));
    }

    #[tokio::test]
    async fn no_baseline_needs_binding_then_bind_saves() {
        let fx = fx(Identity::default(), ACCT).await;
        let p = fx.flow.start(&fx.id, None, SignInMode::Callback).unwrap();
        let h = p.handle();
        let redirect = param(&p.authorize_url, "redirect_uri");
        hit_callback(&redirect, &format!("code=c&state={STATE}")).await;
        assert!(matches!(p.wait().await, SignInResult::NeedsBinding { .. }));
        assert_eq!(fx.creds().access_token, "old-access");
        let got = h.bind().await.unwrap();
        assert_eq!(got.org_uuid.as_deref(), Some(ACCT));
        assert_eq!(fx.creds().access_token, "at-new");
        assert_eq!(fx.meta().identity.org_uuid.as_deref(), Some(ACCT));
    }

    #[tokio::test]
    async fn bind_after_account_delete_recreates_nothing() {
        let fx = fx(Identity::default(), ACCT).await;
        let p = fx.flow.start(&fx.id, None, SignInMode::Callback).unwrap();
        let h = p.handle();
        let redirect = param(&p.authorize_url, "redirect_uri");
        hit_callback(&redirect, &format!("code=c&state={STATE}")).await;
        assert!(matches!(p.wait().await, SignInResult::NeedsBinding { .. }));
        fx.reg.remove(&fx.id).unwrap();
        let err = h.bind().await.unwrap_err();
        assert!(format!("{err:#}").contains("no longer exists"), "{err:#}");
        let dir = Paths::under(fx._dir.path()).accounts.join(fx.id.as_str());
        assert!(!dir.exists(), "account dir recreated");
    }

    #[tokio::test]
    async fn token_failures_save_nothing_and_leak_nothing() {
        for (status, body) in [
            (
                400,
                r#"{"error":"invalid_grant","error_description":"SECRET-detail"}"#,
            ),
            (500, "SECRET upstream"),
            // No id_token: no identity to verify.
            (
                200,
                r#"{"access_token":"SECRET-a","refresh_token":"SECRET-r"}"#,
            ),
        ] {
            let mut fx = fx(baseline(ACCT), ACCT).await;
            let fake = fake_token_endpoint(move |_| reply(status, body)).await;
            fx.flow.cfg.token_url = fake.url.clone();
            let p = fx.flow.start(&fx.id, None, SignInMode::Callback).unwrap();
            let redirect = param(&p.authorize_url, "redirect_uri");
            hit_callback(&redirect, &format!("code=c&state={STATE}")).await;
            let SignInResult::Failed(m) = p.wait().await else {
                panic!("expected Failed for {status}")
            };
            assert!(!m.contains("SECRET"), "{m}");
            assert_eq!(fx.creds().access_token, "old-access");
        }
    }

    #[tokio::test]
    async fn start_rejects_unknown_and_non_openai_accounts() {
        let fx = fx(baseline(ACCT), ACCT).await;
        let e = fx
            .flow
            .start(&"nope".parse().unwrap(), None, SignInMode::Callback)
            .unwrap_err();
        assert!(e.to_string().contains("unknown account"));
        let claude: AccountId = "claude".parse().unwrap();
        fx.reg
            .upsert(AccountMeta::new(claude.clone(), ProviderId::Anthropic))
            .unwrap();
        let e = fx
            .flow
            .start(&claude, None, SignInMode::Callback)
            .unwrap_err();
        assert!(e.to_string().contains("not an OpenAI account"));
    }

    /// Run one callback-mode sign-in whose token endpoint returns `body`.
    async fn login_with(fx: &mut Fx, body: String) -> SignInResult {
        let fake = fake_token_endpoint(move |_| reply(200, body.clone())).await;
        fx.flow.cfg.token_url = fake.url.clone();
        let p = fx.flow.start(&fx.id, None, SignInMode::Callback).unwrap();
        let redirect = param(&p.authorize_url, "redirect_uri");
        hit_callback(&redirect, &format!("code=c&state={STATE}")).await;
        p.wait().await
    }

    fn body_with(id_token: String) -> String {
        serde_json::json!({
            "id_token": id_token,
            "access_token": "at-new",
            "refresh_token": "rt-new",
        })
        .to_string()
    }

    #[test]
    fn verify_openai_rules() {
        let i = |user: Option<&str>, email: Option<&str>, ws: Option<&str>| Identity {
            account_uuid: user.map(Into::into),
            email: email.map(Into::into),
            org_uuid: ws.map(Into::into),
        };
        let base = i(Some("u1"), Some("me@x.com"), Some("ws1"));
        for (got, want) in [
            (i(Some("u1"), Some("new@x.com"), Some("ws1")), Verify::Match),
            (
                i(Some("u2"), Some("me@x.com"), Some("ws1")),
                Verify::Mismatch,
            ),
            (
                i(Some("u1"), Some("me@x.com"), Some("ws2")),
                Verify::Mismatch,
            ),
            (i(None, Some(" ME@x.com "), Some("ws1")), Verify::Match),
            (i(None, Some("other@x.com"), Some("ws1")), Verify::Mismatch),
            (i(None, None, Some("ws1")), Verify::Match),
            (i(None, None, None), Verify::NoBaseline),
        ] {
            assert_eq!(verify_openai(&base, &got), want, "{got:?}");
        }
        assert_eq!(
            verify_openai(&Identity::default(), &base),
            Verify::NoBaseline
        );
    }

    #[tokio::test]
    async fn same_account_id_different_email_is_a_mismatch() {
        // Team members share the workspace account id: the email tells them apart.
        let mut fx = fx(baseline(ACCT), ACCT).await;
        let r = login_with(
            &mut fx,
            body_with(id_token(ACCT, " Other@Example.com ", "team")),
        )
        .await;
        let SignInResult::Mismatch { got, .. } = r else {
            panic!("expected Mismatch, got {r:?}")
        };
        assert_eq!(got.email.as_deref(), Some("Other@Example.com"));
        assert_eq!(fx.creds().access_token, "old-access", "nothing saved");
        assert_eq!(fx.meta().identity, baseline(ACCT));

        // Same email modulo case/whitespace: the same person.
        let r = login_with(
            &mut fx,
            body_with(id_token(ACCT, " ME@example.com", "team")),
        )
        .await;
        assert!(matches!(r, SignInResult::Saved { .. }), "{r:?}");
    }

    #[tokio::test]
    async fn user_id_decides_when_both_have_it() {
        let base = Identity {
            account_uuid: Some("user-1".into()),
            email: Some("me@example.com".into()),
            org_uuid: Some(ACCT.into()),
        };
        let mut fx = fx(base.clone(), ACCT).await;
        // Another member of the same workspace, same email on file or not: mismatch.
        let r = login_with(
            &mut fx,
            body_with(id_token_with_user(ACCT, "user-2", "me@example.com")),
        )
        .await;
        assert!(matches!(r, SignInResult::Mismatch { .. }), "{r:?}");
        // Same user with a changed email: match, and the new email is recorded.
        let r = login_with(
            &mut fx,
            body_with(id_token_with_user(ACCT, "user-1", "renamed@example.com")),
        )
        .await;
        let SignInResult::Saved { identity } = r else {
            panic!("expected Saved, got {r:?}")
        };
        assert_eq!(identity.account_uuid.as_deref(), Some("user-1"));
        assert_eq!(identity.email.as_deref(), Some("renamed@example.com"));
        assert_eq!(fx.creds().access_token, "at-new");
        // Same user signed into a different workspace: a different pooled subscription.
        let r = login_with(
            &mut fx,
            body_with(id_token_with_user(
                "acct-other-ws",
                "user-1",
                "renamed@example.com",
            )),
        )
        .await;
        assert!(matches!(r, SignInResult::Mismatch { .. }), "{r:?}");
    }

    #[tokio::test]
    async fn exchange_without_account_id_claim_stores_none() {
        let email_only = Identity {
            account_uuid: None,
            email: Some("me@example.com".into()),
            org_uuid: None,
        };
        let mut fx = fx(email_only, ACCT).await;
        let mut c = fx.creds();
        c.openai_account_id = Some("acct-previous".into());
        fx.reg.write_creds(&fx.id, &c).unwrap();
        let tok = crate::providers::openai::testkit::fake_jwt(
            &serde_json::json!({"email": "me@example.com"}),
        );
        let r = login_with(&mut fx, body_with(tok)).await;
        assert!(matches!(r, SignInResult::Saved { .. }), "{r:?}");
        assert_eq!(fx.creds().access_token, "at-new");
        assert_eq!(fx.creds().openai_account_id, None);
    }

    #[tokio::test]
    async fn new_signin_supersedes_the_pending_one_and_frees_its_port() {
        let mut fx = fx(baseline(ACCT), ACCT).await;
        let a = fx.flow.start(&fx.id, None, SignInMode::Callback).unwrap();
        let a_redirect = param(&a.authorize_url, "redirect_uri");
        let a_port = reqwest::Url::parse(&a_redirect).unwrap().port().unwrap();

        // B prefers A's port: it must get it once A is superseded.
        fx.flow.cfg.callback_ports = vec![a_port, 0];
        let b = fx
            .flow
            .start_superseding(&fx.id, None, SignInMode::Callback)
            .await
            .unwrap();
        assert_eq!(
            a.wait().await,
            SignInResult::Failed("superseded by a new sign-in".into())
        );
        let b_redirect = param(&b.authorize_url, "redirect_uri");
        assert_eq!(b_redirect, a_redirect, "B binds the freed port");
        assert_eq!(
            hit_callback(&b_redirect, &format!("code=c&state={STATE}")).await,
            200
        );
        assert!(matches!(b.wait().await, SignInResult::Saved { .. }));

        // The plain (sync) start supersedes too, even across instances sharing the slot.
        let other = OpenAiSignIn::new(
            fx.flow.cfg.clone(),
            fx.reg.clone(),
            fx.state.clone(),
            Arc::new(Upstream::new()),
        )
        .with_active(fx.flow.active.clone());
        fx.flow.cfg.callback_ports = vec![0];
        let c = fx.flow.start(&fx.id, None, SignInMode::PasteCode).unwrap();
        let _d = other.start(&fx.id, None, SignInMode::PasteCode).unwrap();
        assert_eq!(
            c.wait().await,
            SignInResult::Failed("superseded by a new sign-in".into())
        );
    }

    #[tokio::test(start_paused = true)]
    async fn flow_expires_after_5_minutes() {
        let fx = fx(baseline(ACCT), ACCT).await;
        let p = fx.flow.start(&fx.id, None, SignInMode::PasteCode).unwrap();
        let h = p.handle();
        tokio::time::advance(FLOW_TTL + Duration::from_secs(1)).await;
        let SignInResult::Failed(m) = p.wait().await else {
            panic!("expected expiry")
        };
        assert!(m.contains("expired"), "{m}");
        assert_eq!(
            h.submit_code(&format!("c#{STATE}")),
            Err(SubmitError::AlreadyUsed)
        );
    }
}
