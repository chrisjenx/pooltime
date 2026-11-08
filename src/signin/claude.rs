//! Claude (claude.ai subscription) PKCE sign-in. Every URL, parameter, scope and body here follows
//! docs/spikes.md S1 (Claude Code 2.1.319); the endpoints, client id and scopes come from
//! [`AnthropicCfg`] because they go stale between Claude Code versions.
//!
//! One flow = one [`Flow`] shared by the expiry timer, the callback listener and the handle.
//! The code can be spent exactly once: [`Flow::claim`] moves `Awaiting -> Exchanging` atomically,
//! whichever source (callback or paste) gets there first wins, and expiry only fires from
//! `Awaiting` (an exchange already in flight is allowed to finish).

use std::sync::{Arc, Mutex, MutexGuard};

use anyhow::{anyhow, bail, Context};
use futures_util::future::BoxFuture;
use futures_util::FutureExt;
use rand::RngExt;
use serde::Deserialize;
use time::OffsetDateTime;
use tokio::sync::{oneshot, watch};
use tokio::time::Instant;

use super::callback::{self, page_html, CallbackEvent, Handler};
use super::pkce::{random_token, state_eq, Pkce};
use super::{
    parse_pasted, FlowControl, PendingSignIn, SignInFlow, SignInMode, SignInResult, SubmitError,
    FLOW_TTL,
};
use crate::accounts::{
    fetch_profile, verify, AccountId, Credentials, Identity, Registry, StateBook, Verify,
};
use crate::config::AnthropicCfg;
use crate::providers::anthropic::expiry_after_refresh;
use crate::providers::ProviderId;
use crate::upstream::{Limits, Upstream};

/// Claude Code's token-exchange timeout.
const TOKEN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);
/// Used when the token response omits `expires_in` (same default as refresh).
const DEFAULT_EXPIRES_IN_SECS: i64 = 3600;
/// Upper bound on server-supplied lifetimes so date arithmetic can never overflow.
const MAX_LIFETIME_SECS: i64 = 10 * 365 * 86_400;
/// Token bodies are small; never buffer more than this.
const MAX_TOKEN_BODY: usize = 256 * 1024;

/// Starts Claude sign-ins. Cheap to clone-share behind an `Arc`.
pub struct ClaudeSignIn {
    cfg: AnthropicCfg,
    registry: Arc<Registry>,
    state: Arc<StateBook>,
    upstream: Arc<Upstream>,
    /// Test seam: fixed `(verifier, state)` instead of random ones.
    #[cfg(test)]
    fixed: Option<(String, String)>,
}

impl ClaudeSignIn {
    pub fn new(
        cfg: AnthropicCfg,
        registry: Arc<Registry>,
        state: Arc<StateBook>,
        upstream: Arc<Upstream>,
    ) -> ClaudeSignIn {
        ClaudeSignIn {
            cfg,
            registry,
            state,
            upstream,
            #[cfg(test)]
            fixed: None,
        }
    }

    #[cfg(test)]
    fn with_fixed_secrets(mut self, verifier: &str, state: &str) -> ClaudeSignIn {
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

    /// The authorize URL, parameters in the order Claude Code appends them (spikes S1).
    fn authorize_url(
        &self,
        redirect_uri: &str,
        challenge: &str,
        state: &str,
        login_hint: Option<&str>,
    ) -> anyhow::Result<String> {
        let mut url = reqwest::Url::parse(&self.cfg.authorize_url)
            .context("anthropic authorize_url is not a valid URL")?;
        {
            let mut q = url.query_pairs_mut();
            q.append_pair("code", "true")
                .append_pair("client_id", &self.cfg.client_id)
                .append_pair("response_type", "code")
                .append_pair("redirect_uri", redirect_uri)
                .append_pair("scope", &self.cfg.scope)
                .append_pair("code_challenge", challenge)
                .append_pair("code_challenge_method", "S256")
                .append_pair("state", state);
            if let Some(h) = login_hint {
                q.append_pair("login_hint", h);
            }
        }
        Ok(url.into())
    }
}

impl SignInFlow for ClaudeSignIn {
    fn start(
        &self,
        account: &AccountId,
        hint: Option<&str>,
        mode: SignInMode,
    ) -> anyhow::Result<PendingSignIn> {
        let meta = self
            .registry
            .get(account)
            .ok_or_else(|| anyhow!("unknown account {account}"))?;
        if meta.provider != ProviderId::Anthropic {
            bail!("account {account} is not a Claude account");
        }
        let hint = hint.map(str::trim).filter(|h| !h.is_empty()).or(meta
            .identity
            .email
            .as_deref()
            .filter(|e| !e.is_empty()));

        let (bound, redirect_uri) = match mode {
            SignInMode::Callback => {
                let b = callback::bind().context("could not bind the localhost callback port")?;
                let uri = format!("http://localhost:{}/callback", b.port());
                (Some(b), uri)
            }
            SignInMode::PasteCode => (None, self.cfg.manual_redirect_url.clone()),
        };
        let (pkce, oauth_state) = self.secrets();
        let authorize_url =
            self.authorize_url(&redirect_uri, &pkce.challenge, &oauth_state, hint)?;

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

        if let Some(bound) = bound {
            let f = flow.clone();
            let handler: Handler = Arc::new(move |ev| {
                let f = f.clone();
                async move { f.on_callback(ev).await }.boxed()
            });
            let mut closed = flow.done.subscribe();
            bound.serve(oauth_state, handler, async move {
                let _ = closed.wait_for(|d| *d).await;
            });
        }

        // Expiry: only an unclaimed flow expires.
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
    /// Waiting for a code.
    Awaiting,
    /// A code was claimed; the exchange is running.
    Exchanging,
    /// Finished (result delivered).
    Done,
}

/// Credentials exchanged for a not-yet-confirmed identity (`NeedsBinding`).
struct Held {
    creds: Credentials,
    got: Identity,
}

struct Flow {
    account: AccountId,
    cfg: AnthropicCfg,
    registry: Arc<Registry>,
    state: Arc<StateBook>,
    upstream: Arc<Upstream>,
    verifier: String,
    oauth_state: String,
    /// The redirect URI of the authorize URL actually handed out (must be echoed to the token
    /// endpoint).
    redirect_uri: String,
    phase: Mutex<Phase>,
    tx: Mutex<Option<oneshot::Sender<SignInResult>>>,
    held: Mutex<Option<Held>>,
    /// Flips to true when the flow ends (stops the timer and the listener).
    done: watch::Sender<bool>,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: Option<String>,
    refresh_token: Option<String>,
    expires_in: Option<i64>,
    refresh_token_expires_in: Option<i64>,
    scope: Option<String>,
    account: Option<TokenAccount>,
    organization: Option<TokenOrg>,
}

/// The identity the token endpoint itself reports (docs/spikes.md S1: `account.{uuid,
/// email_address}`, `organization.uuid`); the fallback when the profile call fails.
#[derive(Deserialize)]
struct TokenAccount {
    uuid: Option<String>,
    email_address: Option<String>,
}

#[derive(Deserialize)]
struct TokenOrg {
    uuid: Option<String>,
}

impl TokenResponse {
    fn identity(&self) -> Identity {
        let nonempty = |s: &Option<String>| s.clone().filter(|s| !s.trim().is_empty());
        Identity {
            account_uuid: self.account.as_ref().and_then(|a| nonempty(&a.uuid)),
            email: self
                .account
                .as_ref()
                .and_then(|a| nonempty(&a.email_address)),
            org_uuid: self.organization.as_ref().and_then(|o| nonempty(&o.uuid)),
        }
    }
}

impl Flow {
    /// Spend the right to submit a code (once).
    fn claim(&self) -> bool {
        let mut p = lock(&self.phase);
        if *p == Phase::Awaiting {
            *p = Phase::Exchanging;
            true
        } else {
            false
        }
    }

    /// Deliver the result, stop the timer and the listener. Idempotent.
    fn finish(self: &Arc<Self>, r: SignInResult) {
        {
            let mut p = lock(&self.phase);
            if *p == Phase::Done {
                return;
            }
            *p = Phase::Done;
        }
        if matches!(r, SignInResult::NeedsBinding { .. }) {
            // Held credentials are only good for one confirmation window.
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

    /// Exchange the (already claimed) code, verify the identity, finish.
    async fn run(self: Arc<Self>, code: String) -> SignInResult {
        let r = self.exchange(&code).await;
        self.finish(r.clone());
        r
    }

    /// The localhost redirect arrived (state already validated by the listener).
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
            CallbackEvent::Denied { error, on_hold } => {
                let r = SignInResult::Failed(if on_hold {
                    "the account is on hold (see https://claude.ai/restricted)".to_string()
                } else {
                    format!("authorization was not granted ({error})")
                });
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

    fn token_request(&self, code: &str) -> anyhow::Result<reqwest::Request> {
        let url: reqwest::Url = self
            .cfg
            .token_url
            .parse()
            .context("anthropic token_url is not a valid URL")?;
        // Field set and order as Claude Code sends them (spikes S1, `HPs`).
        let body = serde_json::json!({
            "grant_type": "authorization_code",
            "code": code,
            "redirect_uri": self.redirect_uri,
            "client_id": self.cfg.client_id,
            "code_verifier": self.verifier,
            "state": self.oauth_state,
        });
        let mut req = reqwest::Request::new(reqwest::Method::POST, url);
        req.headers_mut().insert(
            http::header::CONTENT_TYPE,
            http::HeaderValue::from_static("application/json"),
        );
        *req.body_mut() = Some(reqwest::Body::from(bytes::Bytes::from(body.to_string())));
        Ok(req)
    }

    /// Code -> tokens -> identity -> verify -> save/hold. Failure strings never carry upstream
    /// message text.
    async fn exchange(&self, code: &str) -> SignInResult {
        let failed = |m: String| SignInResult::Failed(m);
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
        let request_id = resp
            .headers
            .get("request-id")
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        let rid = request_id
            .map(|r| format!(", request-id {r}"))
            .unwrap_or_default();
        let body = match resp.body.text().await {
            Ok(b) => b,
            Err(k) => return failed(format!("token exchange failed ({k:?}{rid})")),
        };
        if !(200..300).contains(&status) {
            return failed(if body.contains("account_on_hold") {
                "the account is on hold (see https://claude.ai/restricted)".to_string()
            } else if matches!(status, 400 | 401) {
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
        let from_token = tok.identity();
        let (Some(access), Some(refresh)) = (
            tok.access_token.filter(|t| !t.is_empty()),
            tok.refresh_token.filter(|t| !t.is_empty()),
        ) else {
            return failed(format!(
                "token response lacked an access or refresh token{rid}"
            ));
        };

        let now = OffsetDateTime::now_utc();
        let lifetime = tok
            .expires_in
            .unwrap_or(DEFAULT_EXPIRES_IN_SECS)
            .clamp(1, MAX_LIFETIME_SECS);
        let scopes: Vec<String> = tok
            .scope
            .as_deref()
            .unwrap_or(&self.cfg.scope)
            .split_whitespace()
            .map(str::to_owned)
            .collect();
        // Prefer the profile; the token response's own identity covers a failed profile call
        // (the code is already spent, so failing here would waste the user's sign-in).
        let (got, plan, tier) = match fetch_profile(
            &self.upstream,
            &self.cfg.api_base,
            &access,
            &self.cfg.user_agent,
        )
        .await
        {
            Ok(p) => (merge(&from_token, p.identity), p.plan, p.tier),
            Err(e) => {
                tracing::warn!(account = %self.account, "profile fetch failed during sign-in; using the token response identity");
                if from_token.account_uuid.is_none() && from_token.email.is_none() {
                    return failed(format!("could not verify the signed-in account: {e:#}"));
                }
                (from_token, None, None)
            }
        };
        if got.account_uuid.is_none() && got.email.is_none() {
            return failed("could not verify the signed-in account: no identity reported".into());
        }
        // A re-login keeps what we already knew about the plan when the profile did not say.
        let prev = {
            let (reg, id) = (self.registry.clone(), self.account.clone());
            tokio::task::spawn_blocking(move || reg.read_creds(&id).ok().flatten())
                .await
                .ok()
                .flatten()
        };
        let creds = Credentials {
            provider: ProviderId::Anthropic,
            access_token: access,
            refresh_token: Some(refresh),
            expires_at: Some(now + time::Duration::seconds(lifetime)),
            // A new login starts a new token chain: no older expiry to take the min against.
            refresh_expires_at: expiry_after_refresh(None, tok.refresh_token_expires_in, now),
            scopes,
            plan: plan.or_else(|| prev.as_ref().and_then(|c| c.plan.clone())),
            tier: tier.or_else(|| prev.as_ref().and_then(|c| c.tier.clone())),
            openai_account_id: None,
        };

        let Some(meta) = self.registry.get(&self.account) else {
            return failed("the account was removed during sign-in".into());
        };
        match verify(&meta.identity, &got) {
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

    /// Persist new credentials (under the account lock), retire a stale dead-refresh marker and
    /// record the identity baseline.
    async fn save(&self, creds: Credentials, identity: Identity) -> anyhow::Result<()> {
        let (reg, id) = (self.registry.clone(), self.account.clone());
        tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
            let _guard = reg.lock(&id)?;
            reg.write_creds_unlocked(&id, &creds)
        })
        .await
        .context("saving credentials panicked")??;
        // The binding constraint from Task 15: refresh never clears markers for tokens it did not
        // spend, so a successful login must.
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
            // The credentials are saved and usable; a missing baseline only means the next login
            // asks for confirmation again.
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

/// `got` over `old`: fresh values win, fields the profile did not report keep their old value.
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
        self.0.clone().bind().boxed()
    }

    /// A newer sign-in for the account replaces this one: an unclaimed flow fails as superseded
    /// (which closes its listener); one already exchanging its code finishes on its own.
    fn supersede(&self) {
        if self.0.claim() {
            self.0
                .finish(SignInResult::Failed("superseded by a new sign-in".into()));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::convert::Infallible;
    use std::net::SocketAddr;
    use std::sync::Mutex;
    use std::time::Duration;

    use bytes::Bytes;
    use http_body_util::{BodyExt, Full};
    use hyper::service::service_fn;
    use hyper::{Request, Response};
    use hyper_util::rt::TokioIo;
    use time::OffsetDateTime;
    use tokio::net::TcpListener;

    use crate::accounts::{AccountMeta, Credentials, Identity};
    use crate::config::Paths;
    use crate::providers::ProviderId;
    use crate::store::Store;

    const VERIFIER: &str = "test-verifier-0123456789-abcdefghijklmnopqrstuvwxyz";
    const STATE: &str = "test-state-fixed";
    const OLD_RT: &str = "old-refresh-token";
    const UUID_A: &str = "aaaaaaaa-0000-0000-0000-000000000001";
    const UUID_B: &str = "bbbbbbbb-0000-0000-0000-000000000002";
    const ORG: &str = "cccccccc-0000-0000-0000-000000000003";

    struct Script {
        token_status: u16,
        token_body: String,
        profile_status: u16,
        profile_body: String,
    }

    struct Fake {
        addr: SocketAddr,
        script: Arc<Mutex<Script>>,
        token_bodies: Arc<Mutex<Vec<serde_json::Value>>>,
        profile_headers: Arc<Mutex<Vec<http::HeaderMap>>>,
    }

    fn token_body() -> String {
        serde_json::json!({
            "access_token": "at-new",
            "refresh_token": "rt-new",
            "expires_in": 28800,
            "refresh_token_expires_in": 30 * 86_400,
            "scope": "user:inference user:profile",
            "account": {"uuid": UUID_A, "email_address": "me@example.com"},
            "organization": {"uuid": ORG, "name": "Org"},
        })
        .to_string()
    }

    fn profile_body(uuid: &str, email: &str) -> String {
        serde_json::json!({
            "account": {"uuid": uuid, "email": email},
            "organization": {"uuid": ORG},
        })
        .to_string()
    }

    /// Token + profile endpoints on 127.0.0.1:0.
    async fn fake(profile_uuid: &str) -> Fake {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let script = Arc::new(Mutex::new(Script {
            token_status: 200,
            token_body: token_body(),
            profile_status: 200,
            profile_body: profile_body(profile_uuid, "me@example.com"),
        }));
        let token_bodies = Arc::new(Mutex::new(Vec::new()));
        let profile_headers = Arc::new(Mutex::new(Vec::new()));
        let (s, tb, ph) = (
            script.clone(),
            token_bodies.clone(),
            profile_headers.clone(),
        );
        tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                let (s, tb, ph) = (s.clone(), tb.clone(), ph.clone());
                tokio::spawn(async move {
                    let svc = service_fn(move |req: Request<hyper::body::Incoming>| {
                        let (s, tb, ph) = (s.clone(), tb.clone(), ph.clone());
                        async move {
                            let path = req.uri().path().to_string();
                            let method = req.method().clone();
                            let headers = req.headers().clone();
                            let raw = req.into_body().collect().await.unwrap().to_bytes();
                            let (status, body) = {
                                let s = s.lock().unwrap();
                                match (method.as_str(), path.as_str()) {
                                    ("POST", "/v1/oauth/token") => {
                                        assert_eq!(headers["content-type"], "application/json");
                                        tb.lock()
                                            .unwrap()
                                            .push(serde_json::from_slice(&raw).unwrap());
                                        (s.token_status, s.token_body.clone())
                                    }
                                    ("GET", "/api/oauth/profile") => {
                                        ph.lock().unwrap().push(headers);
                                        (s.profile_status, s.profile_body.clone())
                                    }
                                    _ => (404, "{}".to_string()),
                                }
                            };
                            Ok::<_, Infallible>(
                                Response::builder()
                                    .status(status)
                                    .header("request-id", "req_test")
                                    .body(Full::new(Bytes::from(body)))
                                    .unwrap(),
                            )
                        }
                    });
                    let _ = hyper::server::conn::http1::Builder::new()
                        .serve_connection(TokioIo::new(stream), svc)
                        .await;
                });
            }
        });
        Fake {
            addr,
            script,
            token_bodies,
            profile_headers,
        }
    }

    struct Fx {
        _dir: tempfile::TempDir,
        reg: Arc<Registry>,
        state: Arc<StateBook>,
        fake: Fake,
        flow: ClaudeSignIn,
        id: AccountId,
    }

    fn old_creds() -> Credentials {
        Credentials {
            provider: ProviderId::Anthropic,
            access_token: "old-access-token".into(),
            refresh_token: Some(OLD_RT.into()),
            expires_at: None,
            refresh_expires_at: None,
            scopes: vec![],
            plan: None,
            tier: None,
            openai_account_id: None,
        }
    }

    fn baseline_a() -> Identity {
        Identity {
            account_uuid: Some(UUID_A.into()),
            email: Some("me@example.com".into()),
            org_uuid: Some(ORG.into()),
        }
    }

    /// `baseline`: the account's stored identity. The fake profile reports `profile_uuid`.
    async fn fx(baseline: Identity, profile_uuid: &str) -> Fx {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::under(dir.path());
        paths.ensure().unwrap();
        let reg = Arc::new(Registry::load(&paths).unwrap());
        let store = Arc::new(Store::open(&dir.path().join("pooltime.db")).unwrap());
        let state = Arc::new(StateBook::load(store).unwrap());
        let id: AccountId = "work".parse().unwrap();
        let mut meta = AccountMeta::new(id.clone(), ProviderId::Anthropic);
        meta.identity = baseline;
        reg.upsert(meta).unwrap();
        reg.write_creds(&id, &old_creds()).unwrap();
        state.mark_refresh_dead(&id, OLD_RT, OffsetDateTime::now_utc());
        let fake = fake(profile_uuid).await;
        let cfg = AnthropicCfg {
            token_url: format!("http://{}/v1/oauth/token", fake.addr),
            api_base: format!("http://{}", fake.addr),
            ..AnthropicCfg::default()
        };
        let flow = ClaudeSignIn::new(cfg, reg.clone(), state.clone(), Arc::new(Upstream::new()))
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

    impl Fx {
        fn creds(&self) -> Credentials {
            self.reg.read_creds(&self.id).unwrap().unwrap()
        }
        fn meta(&self) -> AccountMeta {
            self.reg.get(&self.id).unwrap()
        }
        fn token_hits(&self) -> usize {
            self.fake.token_bodies.lock().unwrap().len()
        }
    }

    fn params(url: &str) -> Vec<(String, String)> {
        reqwest::Url::parse(url)
            .unwrap()
            .query_pairs()
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect()
    }

    fn param(url: &str, key: &str) -> String {
        params(url).into_iter().find(|(k, _)| k == key).unwrap().1
    }

    fn assert_dead_marker_still_set(fx: &Fx) {
        assert!(fx.state.is_refresh_dead(&fx.id, OLD_RT));
    }

    #[tokio::test]
    async fn authorize_url_has_expected_params_and_login_hint() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::under(dir.path());
        paths.ensure().unwrap();
        let reg = Arc::new(Registry::load(&paths).unwrap());
        let store = Arc::new(Store::open(&dir.path().join("pooltime.db")).unwrap());
        let state = Arc::new(StateBook::load(store).unwrap());
        let id: AccountId = "work".parse().unwrap();
        reg.upsert(AccountMeta::new(id.clone(), ProviderId::Anthropic))
            .unwrap();
        let flow = ClaudeSignIn::new(
            AnthropicCfg::default(),
            reg,
            state,
            Arc::new(Upstream::new()),
        )
        .with_fixed_secrets(VERIFIER, STATE);

        // Paste-code mode: the manual redirect makes the whole URL deterministic.
        let p = flow
            .start(&id, Some("me+tag@example.com"), SignInMode::PasteCode)
            .unwrap();
        insta::assert_snapshot!(p.authorize_url, @"https://claude.com/cai/oauth/authorize?code=true&client_id=9d1c250a-e61b-44d9-88ed-5944d1962f5e&response_type=code&redirect_uri=https%3A%2F%2Fplatform.claude.com%2Foauth%2Fcode%2Fcallback&scope=org%3Acreate_api_key+user%3Aprofile+user%3Ainference+user%3Asessions%3Aclaude_code+user%3Amcp_servers+user%3Afile_upload+user%3Aplugins&code_challenge=nw4Cig7sk_DT0QSX9Qn-3UKs33fmbbp8bRuv1b5-wXw&code_challenge_method=S256&state=test-state-fixed&login_hint=me%2Btag%40example.com");

        // No hint and no baseline email: no login_hint at all.
        let p = flow.start(&id, None, SignInMode::PasteCode).unwrap();
        assert!(!params(&p.authorize_url)
            .iter()
            .any(|(k, _)| k == "login_hint"));

        // Callback mode: localhost redirect on an ephemeral port, same order otherwise.
        let p = flow
            .start(&id, Some("a@b.c"), SignInMode::Callback)
            .unwrap();
        let keys: Vec<String> = params(&p.authorize_url)
            .into_iter()
            .map(|(k, _)| k)
            .collect();
        assert_eq!(
            keys,
            [
                "code",
                "client_id",
                "response_type",
                "redirect_uri",
                "scope",
                "code_challenge",
                "code_challenge_method",
                "state",
                "login_hint"
            ]
        );
        let redirect = param(&p.authorize_url, "redirect_uri");
        let r = reqwest::Url::parse(&redirect).unwrap();
        assert_eq!(r.scheme(), "http");
        assert_eq!(r.host_str(), Some("localhost"));
        assert_eq!(r.path(), "/callback");
        assert!(r.port().is_some());
    }

    #[tokio::test]
    async fn mismatch_does_not_save() {
        let fx = fx(baseline_a(), UUID_B).await;
        let p = fx.flow.start(&fx.id, None, SignInMode::PasteCode).unwrap();
        p.submit_code(&format!("the-code#{STATE}")).unwrap();
        match p.wait().await {
            SignInResult::Mismatch { expected, got } => {
                assert_eq!(expected, baseline_a());
                assert_eq!(got.account_uuid.as_deref(), Some(UUID_B));
            }
            other => panic!("{other:?}"),
        }
        // Nothing saved: old credentials, old identity, dead marker untouched.
        assert_eq!(fx.creds(), old_creds());
        assert_eq!(fx.meta().identity, baseline_a());
        assert_dead_marker_still_set(&fx);
    }

    #[tokio::test]
    async fn needs_binding_when_no_baseline() {
        let fx = fx(Identity::default(), UUID_A).await;
        let p = fx
            .flow
            .start(&fx.id, Some("me@example.com"), SignInMode::PasteCode)
            .unwrap();
        let handle = p.handle();
        p.submit_code(&format!("the-code#{STATE}")).unwrap();
        let got = match p.wait().await {
            SignInResult::NeedsBinding { got } => got,
            other => panic!("{other:?}"),
        };
        assert_eq!(got.account_uuid.as_deref(), Some(UUID_A));
        assert_eq!(got.email.as_deref(), Some("me@example.com"));
        // Held in memory only: nothing on disk changed yet.
        assert_eq!(fx.creds(), old_creds());
        assert_eq!(fx.meta().identity, Identity::default());

        // The caller confirms: creds and baseline are saved, the dead marker is retired.
        assert_eq!(handle.bind().await.unwrap(), got);
        assert_eq!(fx.creds().access_token, "at-new");
        assert_eq!(fx.meta().identity, got);
        assert!(!fx.state.is_refresh_dead(&fx.id, OLD_RT));
        // One-shot: the held credentials are gone.
        assert!(handle.bind().await.is_err());
    }

    #[tokio::test]
    async fn saved_clears_dead_marker() {
        let fx = fx(baseline_a(), UUID_A).await;
        assert_dead_marker_still_set(&fx);
        let p = fx.flow.start(&fx.id, None, SignInMode::PasteCode).unwrap();
        p.submit_code(&format!("the-code#{STATE}")).unwrap();
        let got = match p.wait().await {
            SignInResult::Saved { identity } => identity,
            other => panic!("{other:?}"),
        };
        assert_eq!(got.account_uuid.as_deref(), Some(UUID_A));

        let c = fx.creds();
        assert_eq!(c.provider, ProviderId::Anthropic);
        assert_eq!(c.access_token, "at-new");
        assert_eq!(c.refresh_token.as_deref(), Some("rt-new"));
        assert_eq!(c.scopes, ["user:inference", "user:profile"]);
        let now = OffsetDateTime::now_utc();
        let near = |at: Option<OffsetDateTime>, secs: i64| {
            let d = (at.unwrap() - (now + time::Duration::seconds(secs))).whole_seconds();
            d.abs() < 60
        };
        assert!(near(c.expires_at, 28_800), "{:?}", c.expires_at);
        assert!(near(c.refresh_expires_at, 30 * 86_400));
        assert_eq!(fx.meta().identity, got);
        assert!(
            !fx.state.is_refresh_dead(&fx.id, OLD_RT),
            "a fresh login retires the dead marker"
        );
    }

    #[tokio::test]
    async fn saved_merges_identity_and_adopts_uuid_from_email_baseline() {
        // An imported account whose baseline only knows the email.
        let baseline = Identity {
            account_uuid: None,
            email: Some("ME@example.com".into()),
            org_uuid: None,
        };
        let fx = fx(baseline, UUID_A).await;
        let p = fx.flow.start(&fx.id, None, SignInMode::PasteCode).unwrap();
        p.submit_code(&format!("c#{STATE}")).unwrap();
        assert!(matches!(p.wait().await, SignInResult::Saved { .. }));
        let m = fx.meta();
        assert_eq!(m.identity.account_uuid.as_deref(), Some(UUID_A));
        assert_eq!(m.identity.org_uuid.as_deref(), Some(ORG));
    }

    #[tokio::test]
    async fn callback_mode_end_to_end() {
        let fx = fx(baseline_a(), UUID_A).await;
        let p = fx.flow.start(&fx.id, None, SignInMode::Callback).unwrap();
        let redirect = param(&p.authorize_url, "redirect_uri");
        let port = reqwest::Url::parse(&redirect).unwrap().port().unwrap();

        // Wrong state: rejected, flow still armed.
        let r = reqwest::get(format!(
            "http://127.0.0.1:{port}/callback?code=x&state=nope"
        ))
        .await
        .unwrap();
        assert_eq!(r.status(), 400);
        assert_eq!(fx.token_hits(), 0);

        let r = reqwest::get(format!(
            "http://127.0.0.1:{port}/callback?code=the-code&state={STATE}"
        ))
        .await
        .unwrap();
        assert_eq!(r.status(), 200);
        assert!(matches!(p.wait().await, SignInResult::Saved { .. }));

        // The exchange body: exactly the spike's fields, redirect_uri as in the authorize URL.
        let bodies = fx.fake.token_bodies.lock().unwrap().clone();
        assert_eq!(
            bodies,
            vec![serde_json::json!({
                "grant_type": "authorization_code",
                "code": "the-code",
                "redirect_uri": redirect,
                "client_id": "9d1c250a-e61b-44d9-88ed-5944d1962f5e",
                "code_verifier": VERIFIER,
                "state": STATE,
            })]
        );
        // The profile call: bearer of the NEW access token, Claude Code UA, no-cache.
        let h = fx.fake.profile_headers.lock().unwrap().clone();
        assert_eq!(h.len(), 1);
        assert_eq!(h[0]["authorization"], "Bearer at-new");
        assert_eq!(h[0]["user-agent"], "claude-code/2.1.319");
        assert_eq!(h[0]["cache-control"], "no-cache");
        assert_eq!(h[0]["content-type"], "application/json");
        assert_eq!(fx.creds().access_token, "at-new");
    }

    #[tokio::test]
    async fn paste_code_uses_manual_redirect_and_validates_input() {
        let fx = fx(baseline_a(), UUID_A).await;
        let p = fx.flow.start(&fx.id, None, SignInMode::PasteCode).unwrap();
        for bad in ["", "just-a-code", "#state-only", "code#"] {
            assert_eq!(
                p.submit_code(bad).unwrap_err(),
                SubmitError::Malformed,
                "{bad:?}"
            );
        }
        // A wrong state is refused without spending the flow.
        assert_eq!(
            p.submit_code("c#wrong").unwrap_err(),
            SubmitError::StateMismatch
        );
        assert_eq!(fx.token_hits(), 0);

        // Surrounding whitespace from a terminal paste is tolerated.
        p.submit_code(&format!("  the-code#{STATE}\n")).unwrap();
        // Second submission: the flow is spent.
        assert_eq!(
            p.submit_code(&format!("the-code#{STATE}")).unwrap_err(),
            SubmitError::AlreadyUsed
        );
        assert!(matches!(p.wait().await, SignInResult::Saved { .. }));
        let bodies = fx.fake.token_bodies.lock().unwrap().clone();
        assert_eq!(bodies.len(), 1);
        assert_eq!(
            bodies[0]["redirect_uri"],
            "https://platform.claude.com/oauth/code/callback"
        );
    }

    #[tokio::test]
    async fn token_and_profile_failures_save_nothing() {
        for (token_status, profile_status) in [(400, 200), (401, 200), (200, 500)] {
            let fx = fx(baseline_a(), UUID_A).await;
            {
                let mut s = fx.fake.script.lock().unwrap();
                s.token_status = token_status;
                s.profile_status = profile_status;
                if profile_status != 200 {
                    // No identity in the token response either: nothing to fall back to.
                    s.token_body = serde_json::json!({
                        "access_token": "at-new", "refresh_token": "rt-new", "expires_in": 3600
                    })
                    .to_string();
                }
                if token_status != 200 {
                    s.token_body =
                        r#"{"error":"invalid_grant","error_description":"secret-ish"}"#.into();
                }
            }
            let p = fx.flow.start(&fx.id, None, SignInMode::PasteCode).unwrap();
            p.submit_code(&format!("the-code#{STATE}")).unwrap();
            match p.wait().await {
                SignInResult::Failed(msg) => {
                    assert!(!msg.contains("secret-ish"), "{msg}");
                    assert!(!msg.contains("at-new"), "{msg}");
                }
                other => panic!("{other:?}"),
            }
            assert_eq!(fx.creds(), old_creds());
            assert_dead_marker_still_set(&fx);
        }
    }

    #[tokio::test]
    async fn provider_error_on_callback_fails_the_flow() {
        let fx = fx(baseline_a(), UUID_A).await;
        let p = fx.flow.start(&fx.id, None, SignInMode::Callback).unwrap();
        let port = reqwest::Url::parse(&param(&p.authorize_url, "redirect_uri"))
            .unwrap()
            .port()
            .unwrap();
        let r = reqwest::get(format!(
            "http://127.0.0.1:{port}/callback?error=access_denied&state={STATE}"
        ))
        .await
        .unwrap();
        assert!(r.status().is_client_error());
        match p.wait().await {
            SignInResult::Failed(m) => assert!(m.contains("access_denied"), "{m}"),
            other => panic!("{other:?}"),
        }
        assert_eq!(fx.token_hits(), 0);
    }

    #[tokio::test]
    async fn start_rejects_unknown_account_and_non_anthropic() {
        let fx = fx(baseline_a(), UUID_A).await;
        let ghost: AccountId = "ghost".parse().unwrap();
        assert!(fx.flow.start(&ghost, None, SignInMode::PasteCode).is_err());
        let oa: AccountId = "codex".parse().unwrap();
        fx.reg
            .upsert(AccountMeta::new(oa.clone(), ProviderId::OpenAi))
            .unwrap();
        assert!(fx.flow.start(&oa, None, SignInMode::PasteCode).is_err());
    }

    #[tokio::test(start_paused = true)]
    async fn flow_expires_after_5_minutes() {
        let fx = fx(baseline_a(), UUID_A).await;
        let p = fx.flow.start(&fx.id, None, SignInMode::Callback).unwrap();
        let port = reqwest::Url::parse(&param(&p.authorize_url, "redirect_uri"))
            .unwrap()
            .port()
            .unwrap();
        let ttl = (p.expires_at - OffsetDateTime::now_utc()).whole_seconds();
        assert!((298..=300).contains(&ttl), "{ttl}");

        let mut wait = Box::pin(p.wait());
        tokio::select! {
            _ = &mut wait => panic!("expired early"),
            _ = tokio::time::sleep(Duration::from_secs(299)) => {}
        }
        match wait.await {
            SignInResult::Failed(m) => assert!(m.contains("expired"), "{m}"),
            other => panic!("{other:?}"),
        }
        assert_eq!(fx.token_hits(), 0);
        assert_eq!(fx.creds(), old_creds());

        // The callback listener is closed once the flow ends.
        for _ in 0..100 {
            if tokio::net::TcpStream::connect(("127.0.0.1", port))
                .await
                .is_err()
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("listener still open after expiry");
    }

    #[tokio::test(start_paused = true)]
    async fn code_after_expiry_is_refused() {
        let fx = fx(baseline_a(), UUID_A).await;
        let p = fx.flow.start(&fx.id, None, SignInMode::PasteCode).unwrap();
        let handle = p.handle();
        tokio::time::sleep(Duration::from_secs(301)).await;
        assert_eq!(
            handle.submit_code(&format!("c#{STATE}")).unwrap_err(),
            SubmitError::AlreadyUsed
        );
        assert!(matches!(p.wait().await, SignInResult::Failed(_)));
    }

    #[tokio::test]
    async fn profile_failure_falls_back_to_token_response_identity() {
        // Saved: the token response's account matches the baseline.
        let fx1 = fx(baseline_a(), UUID_A).await;
        fx1.fake.script.lock().unwrap().profile_status = 500;
        let p = fx1
            .flow
            .start(&fx1.id, None, SignInMode::PasteCode)
            .unwrap();
        p.submit_code(&format!("c#{STATE}")).unwrap();
        assert!(matches!(p.wait().await, SignInResult::Saved { .. }));
        assert_eq!(fx1.creds().access_token, "at-new");
        assert_eq!(fx1.meta().identity, baseline_a());
        assert!(!fx1.state.is_refresh_dead(&fx1.id, OLD_RT));

        // Mismatch: the token response says another account signed in.
        let fx2 = fx(baseline_a(), UUID_A).await;
        {
            let mut s = fx2.fake.script.lock().unwrap();
            s.profile_status = 500;
            s.token_body = serde_json::json!({
                "access_token": "at-new", "refresh_token": "rt-new", "expires_in": 3600,
                "account": {"uuid": UUID_B, "email_address": "other@example.com"},
                "organization": {"uuid": ORG},
            })
            .to_string();
        }
        let p = fx2
            .flow
            .start(&fx2.id, None, SignInMode::PasteCode)
            .unwrap();
        p.submit_code(&format!("c#{STATE}")).unwrap();
        match p.wait().await {
            SignInResult::Mismatch { got, .. } => {
                assert_eq!(got.account_uuid.as_deref(), Some(UUID_B));
                assert_eq!(got.email.as_deref(), Some("other@example.com"));
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(fx2.creds(), old_creds());
    }

    #[tokio::test]
    async fn plan_and_tier_come_from_the_profile() {
        let fx = fx(baseline_a(), UUID_A).await;
        fx.fake.script.lock().unwrap().profile_body = serde_json::json!({
            "account": {"uuid": UUID_A, "email": "me@example.com"},
            "organization": {
                "uuid": ORG,
                "organization_type": "claude_max",
                "rate_limit_tier": "default_claude_max_20x",
            },
        })
        .to_string();
        let mut prev = old_creds();
        prev.plan = Some("pro".into());
        prev.tier = Some("old-tier".into());
        fx.reg.write_creds(&fx.id, &prev).unwrap();
        let p = fx.flow.start(&fx.id, None, SignInMode::PasteCode).unwrap();
        p.submit_code(&format!("c#{STATE}")).unwrap();
        assert!(matches!(p.wait().await, SignInResult::Saved { .. }));
        let c = fx.creds();
        assert_eq!(c.plan.as_deref(), Some("max"));
        assert_eq!(c.tier.as_deref(), Some("default_claude_max_20x"));
    }

    #[tokio::test]
    async fn plan_and_tier_are_kept_when_the_profile_omits_them() {
        // Profile without organization_type/rate_limit_tier, and a failed profile call.
        for profile_status in [200, 500] {
            let fx = fx(baseline_a(), UUID_A).await;
            fx.fake.script.lock().unwrap().profile_status = profile_status;
            let mut prev = old_creds();
            prev.plan = Some("max".into());
            prev.tier = Some("default_claude_max_5x".into());
            fx.reg.write_creds(&fx.id, &prev).unwrap();
            let p = fx.flow.start(&fx.id, None, SignInMode::PasteCode).unwrap();
            p.submit_code(&format!("c#{STATE}")).unwrap();
            assert!(matches!(p.wait().await, SignInResult::Saved { .. }));
            let c = fx.creds();
            assert_eq!(c.access_token, "at-new");
            assert_eq!(c.plan.as_deref(), Some("max"), "profile {profile_status}");
            assert_eq!(c.tier.as_deref(), Some("default_claude_max_5x"));
        }
    }
}
