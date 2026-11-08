//! OAuth for ChatGPT (Codex) accounts: the JSON refresh grant (INV §4), dead-refresh-token
//! detection (INV §10.4, new vs bro), id_token claims and `~/.codex/auth.json` import.
//!
//! The refresh mirrors the Anthropic one (`providers::anthropic::oauth`, Ruling T15): one shared
//! in-flight task per (account, force), a per-account async mutex, then the per-account `flock`,
//! the credentials re-read after each layer, and the critical section (flock, POST, persist, state
//! updates) running in a spawned task so a dropped caller can never lose a rotated refresh token.
//!
//! Dead detection: a 400/401 whose OAuth error code is `invalid_grant`, `refresh_token_reused`,
//! `refresh_token_expired` or `refresh_token_invalidated` marks the refresh token we sent dead (fingerprint only). Transport errors, 5xx and every other
//! status never do.
//!
//! The id_token is read without verifying its signature: it comes straight from the token
//! endpoint over TLS (or from the Codex CLI's own `auth.json`), and its claims are only used to
//! label the account (account id, email, plan), never to authorise anything.
//!
//! Nothing here logs or persists a token or an upstream message: only the classified
//! [`RefreshErrorKind`], the account and the upstream request id.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{anyhow, bail, Context};
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use bytes::Bytes;
use futures_util::future::{BoxFuture, FutureExt, Shared};
use serde::Deserialize;
use serde_json::Value;
use time::OffsetDateTime;
use tokio::time::Instant;

use super::OpenAiProvider;
use crate::accounts::{
    AccountId, AccountMeta, Credentials, ErrorRecord, Identity, Registry, StateBook,
};
use crate::config::OpenAiCfg;
use crate::errors::{FailReason, RefreshErrorKind};
use crate::fsutil::FileLock;
use crate::providers::ProviderId;
use crate::upstream::{Limits, Upstream};

/// Used when neither `expires_in` nor the access token's `exp` says (bro's default).
const DEFAULT_EXPIRES_IN_SECS: i64 = 3600;
/// Upper bound on server-supplied lifetimes so date arithmetic can never overflow.
const MAX_LIFETIME_SECS: i64 = 10 * 365 * 86_400;
/// The id_token claim object carrying the ChatGPT account (Codex CLI `IdTokenInfo`).
const AUTH_CLAIM: &str = "https://api.openai.com/auth";
/// Fallback location of the email claim.
const PROFILE_CLAIM: &str = "https://api.openai.com/profile";
/// `auth.json` is a few KB; refuse anything absurd.
const MAX_AUTH_JSON_BYTES: u64 = 1024 * 1024;

type RefreshResult = Result<String, RefreshErrorKind>;
/// A refresh running (or just finished) in a spawned task, awaitable by any number of callers.
pub(super) type InFlight = Shared<BoxFuture<'static, RefreshResult>>;
/// Keyed by (account, force): a forced caller must not be satisfied by an unforced run.
pub(super) type InFlightMap = Arc<Mutex<HashMap<(AccountId, bool), InFlight>>>;

/// Token endpoint response (code exchange and refresh).
#[derive(Deserialize)]
pub(crate) struct TokenResponse {
    pub access_token: Option<String>,
    pub refresh_token: Option<String>,
    pub id_token: Option<String>,
    pub expires_in: Option<i64>,
    pub scope: Option<String>,
}

/// What an id_token says about the signed-in ChatGPT account. Empty strings count as absent.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IdTokenClaims {
    /// `https://api.openai.com/auth`.`chatgpt_account_id` (the `chatgpt-account-id` header value).
    pub account_id: Option<String>,
    /// `https://api.openai.com/auth`.`chatgpt_user_id` (else `user_id`): the person.
    pub user_id: Option<String>,
    /// `email` (or `https://api.openai.com/profile`.`email`).
    pub email: Option<String>,
    /// `https://api.openai.com/auth`.`chatgpt_plan_type`.
    pub plan: Option<String>,
}

impl IdTokenClaims {
    /// The account identity. `chatgpt_account_id` names the *workspace* (Team members share it),
    /// so it fills the `org_uuid` slot; the user id (the person) fills `account_uuid`; the email
    /// sits alongside. Compared by `signin::openai::verify_openai`.
    pub fn identity(&self) -> Identity {
        Identity {
            account_uuid: self.user_id.clone(),
            email: self.email.clone(),
            org_uuid: self.account_id.clone(),
        }
    }
}

/// The JSON payload of a JWT (no signature check, see the module docs).
fn jwt_payload(jwt: &str) -> Option<serde_json::Map<String, Value>> {
    let mut parts = jwt.split('.');
    let (_header, payload, _sig) = (parts.next()?, parts.next()?, parts.next()?);
    let bytes = URL_SAFE_NO_PAD.decode(payload.trim_end_matches('=')).ok()?;
    match serde_json::from_slice(&bytes).ok()? {
        Value::Object(m) => Some(m),
        _ => None,
    }
}

fn nonempty(v: Option<&Value>) -> Option<String> {
    v.and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

/// Claims of an id_token; `None` when it is not a decodable JWT.
pub fn id_token_claims(id_token: &str) -> Option<IdTokenClaims> {
    let p = jwt_payload(id_token)?;
    let auth = p.get(AUTH_CLAIM);
    Some(IdTokenClaims {
        account_id: nonempty(auth.and_then(|a| a.get("chatgpt_account_id"))),
        user_id: nonempty(auth.and_then(|a| a.get("chatgpt_user_id")))
            .or_else(|| nonempty(auth.and_then(|a| a.get("user_id")))),
        email: nonempty(p.get("email"))
            .or_else(|| nonempty(p.get(PROFILE_CLAIM).and_then(|x| x.get("email")))),
        plan: nonempty(auth.and_then(|a| a.get("chatgpt_plan_type"))),
    })
}

/// The `exp` of a JWT access token, if it is one.
fn jwt_exp(jwt: &str) -> Option<OffsetDateTime> {
    let exp = jwt_payload(jwt)?.get("exp")?.as_i64()?;
    OffsetDateTime::from_unix_timestamp(exp).ok()
}

/// When a fresh access token expires: `expires_in`, else the token's own `exp`, else one hour.
pub(crate) fn token_expiry(
    expires_in: Option<i64>,
    access_token: &str,
    now: OffsetDateTime,
) -> OffsetDateTime {
    match expires_in {
        Some(s) => now + time::Duration::seconds(s.clamp(1, MAX_LIFETIME_SECS)),
        None => jwt_exp(access_token)
            .unwrap_or_else(|| now + time::Duration::seconds(DEFAULT_EXPIRES_IN_SECS)),
    }
}

/// A normalized `~/.codex/auth.json`.
#[derive(Debug, Clone)]
pub struct CodexAuthImport {
    pub creds: Credentials,
    /// From the id_token (account id + email); empty when the file has none.
    pub identity: Identity,
}

/// Read and normalize a Codex CLI `auth.json` (`tokens{id_token, access_token, refresh_token,
/// account_id}`, `last_refresh`, `OPENAI_API_KEY`; bro `normalizeCodexAuthJson`). The bare
/// `tokens.account_id` wins over the id_token claim. An API-key-only file is refused (pooltime
/// pools ChatGPT sign-ins). Errors never quote the file's content.
pub fn codex_auth_json_import(path: &Path) -> anyhow::Result<CodexAuthImport> {
    let f = std::fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let mut bytes = Vec::new();
    std::io::Read::read_to_end(
        &mut std::io::Read::take(f, MAX_AUTH_JSON_BYTES + 1),
        &mut bytes,
    )
    .with_context(|| format!("reading {}", path.display()))?;
    if bytes.len() as u64 > MAX_AUTH_JSON_BYTES {
        bail!("{} is too large to be a Codex auth.json", path.display());
    }
    normalize_codex_auth(&bytes).with_context(|| format!("importing {}", path.display()))
}

fn normalize_codex_auth(bytes: &[u8]) -> anyhow::Result<CodexAuthImport> {
    let root: Value =
        serde_json::from_slice(bytes).map_err(|_| anyhow!("auth.json is not valid JSON"))?;
    let obj = root
        .as_object()
        .ok_or_else(|| anyhow!("auth.json is not a JSON object"))?;
    let tokens = obj.get("tokens").and_then(Value::as_object);
    // `tokens.<k>`, else a top-level `<k>` (bro accepts both).
    let field = |k: &str| nonempty(tokens.and_then(|t| t.get(k))).or_else(|| nonempty(obj.get(k)));
    let Some(access_token) = field("access_token") else {
        if nonempty(obj.get("OPENAI_API_KEY")).is_some() {
            bail!(
                "auth.json holds an OpenAI API key, not a ChatGPT sign-in; sign in with \
                 `codex login` (ChatGPT) or use `pooltime accounts login`"
            );
        }
        bail!("auth.json has no ChatGPT access token");
    };
    let claims = field("id_token")
        .and_then(|t| id_token_claims(&t))
        .unwrap_or_default();
    let account_id = field("account_id").or(claims.account_id);
    Ok(CodexAuthImport {
        creds: Credentials {
            provider: ProviderId::OpenAi,
            expires_at: jwt_exp(&access_token),
            access_token,
            refresh_token: field("refresh_token"),
            refresh_expires_at: None,
            scopes: Vec::new(),
            plan: claims.plan,
            tier: None,
            openai_account_id: account_id.clone(),
        },
        identity: Identity {
            account_uuid: claims.user_id,
            email: claims.email,
            org_uuid: account_id,
        },
    })
}

/// `$CODEX_HOME/auth.json`, else `$HOME/.codex/auth.json` (`get` reads the environment).
pub fn default_codex_auth_path(get: impl Fn(&str) -> Option<String>) -> Option<PathBuf> {
    let var = |k: &str| get(k).filter(|v| !v.is_empty());
    if let Some(home) = var("CODEX_HOME") {
        return Some(PathBuf::from(home).join("auth.json"));
    }
    var("HOME").map(|h| PathBuf::from(h).join(".codex").join("auth.json"))
}

/// Everything the spawned refresh task needs, owned.
struct Ctx {
    cfg: OpenAiCfg,
    reg: Arc<Registry>,
    state: Arc<StateBook>,
    up: Arc<Upstream>,
    skew: time::Duration,
    timeout: Duration,
}

/// Removes the in-flight entry when the task ends, even by panic.
struct Cleanup {
    map: InFlightMap,
    key: (AccountId, bool),
}

impl Drop for Cleanup {
    fn drop(&mut self) {
        self.map
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&self.key);
    }
}

impl OpenAiProvider {
    /// A usable access token for `acct`, refreshing when `force` or when it expires within the
    /// skew. A token without an expiry counts as fresh; an account without a refresh token returns
    /// its stored token unless `force`. A refresh token already known dead is not re-POSTed
    /// (`InvalidGrant` straight away).
    ///
    /// Cancel-safe: dropping the returned future never loses an in-progress refresh.
    pub async fn access_token(&self, acct: &AccountMeta, force: bool) -> RefreshResult {
        let id = &acct.id;
        let creds = load(&self.reg, &self.state, id)?;
        if let Some(r) = settled(&self.state, self.skew, id, &creds, force, None) {
            return r;
        }
        self.in_flight(id, force, creds.access_token).await
    }

    /// The shared refresh task for `(id, force)`, starting it if none is running.
    fn in_flight(&self, id: &AccountId, force: bool, seen: String) -> InFlight {
        let key = (id.clone(), force);
        let mut map = self.inflight.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(f) = map.get(&key) {
            return f.clone();
        }
        let ctx = Ctx {
            cfg: self.cfg.clone(),
            reg: self.reg.clone(),
            state: self.state.clone(),
            up: self.up.clone(),
            skew: self.skew,
            timeout: self.refresh_timeout,
        };
        let gate = {
            let mut g = self.gates.lock().unwrap_or_else(|e| e.into_inner());
            g.entry(id.clone()).or_default().clone()
        };
        let cleanup = Cleanup {
            map: self.inflight.clone(),
            key: key.clone(),
        };
        let id = id.clone();
        // Spawned while holding the map lock: the task's `Cleanup` cannot run before the entry
        // below exists.
        let handle = tokio::spawn(async move {
            let _cleanup = cleanup;
            ctx.run(&id, force, &seen, gate).await
        });
        let fut: InFlight = async move { handle.await.unwrap_or(Err(RefreshErrorKind::Other)) }
            .boxed()
            .shared();
        map.insert(key, fut.clone());
        fut
    }
}

/// `Some(result)` when no refresh is needed (or possible) for `creds`.
fn settled(
    state: &StateBook,
    skew: time::Duration,
    id: &AccountId,
    creds: &Credentials,
    force: bool,
    seen: Option<&str>,
) -> Option<RefreshResult> {
    let now = OffsetDateTime::now_utc();
    let fresh = creds.expires_at.is_none_or(|e| e - now > skew);
    // A forced refresh is satisfied by another caller's refresh (token changed since we looked).
    let satisfied = !force || seen.is_some_and(|s| s != creds.access_token);
    if fresh && satisfied {
        return Some(Ok(creds.access_token.clone()));
    }
    if creds.refresh_token.is_none() {
        return Some(if force {
            Err(fail(state, id, RefreshErrorKind::Other, None))
        } else {
            Ok(creds.access_token.clone())
        });
    }
    None
}

fn load(
    reg: &Registry,
    state: &StateBook,
    id: &AccountId,
) -> Result<Credentials, RefreshErrorKind> {
    match reg.read_creds(id) {
        Ok(Some(c)) => Ok(c),
        // read_creds already logged the sanitized corruption reason (when corrupt).
        Ok(None) | Err(_) => Err(fail(state, id, RefreshErrorKind::Other, None)),
    }
}

/// Log the classified outcome, record it as the account's last error, return it.
fn fail(
    state: &StateBook,
    id: &AccountId,
    kind: RefreshErrorKind,
    request_id: Option<String>,
) -> RefreshErrorKind {
    tracing::warn!(
        account = %id,
        outcome = %kind.detail(),
        request_id = request_id.as_deref().unwrap_or("-"),
        "openai oauth refresh failed"
    );
    state.record_error(
        id,
        ErrorRecord {
            at: OffsetDateTime::now_utc(),
            reason: FailReason::Refresh(kind),
            upstream_request_id: request_id,
        },
    );
    kind
}

impl Ctx {
    /// The whole critical section; runs in a spawned task.
    async fn run(
        &self,
        id: &AccountId,
        force: bool,
        seen: &str,
        gate: Arc<tokio::sync::Mutex<()>>,
    ) -> RefreshResult {
        let _in_process = gate.lock_owned().await;
        let creds = load(&self.reg, &self.state, id)?;
        if let Some(r) = settled(&self.state, self.skew, id, &creds, force, Some(seen)) {
            return r;
        }
        // flock blocks the thread: acquire it off the async workers.
        let flock = {
            let (reg, lock_id) = (self.reg.clone(), id.clone());
            tokio::task::spawn_blocking(move || reg.lock(&lock_id))
                .await
                .map_err(|_| RefreshErrorKind::Other)?
                .map_err(|e| {
                    tracing::warn!(account = %id, error_kind = ?e.kind(), "openai oauth refresh could not take the account lock");
                    fail(&self.state, id, RefreshErrorKind::Other, None)
                })?
        };
        let creds = load(&self.reg, &self.state, id)?;
        if let Some(r) = settled(&self.state, self.skew, id, &creds, force, Some(seen)) {
            return r;
        }
        self.refresh(id, creds, &flock).await
    }

    /// One token-endpoint call. The caller holds the account flock.
    async fn refresh(&self, id: &AccountId, cur: Credentials, _flock: &FileLock) -> RefreshResult {
        let old_rt = cur
            .refresh_token
            .clone()
            .ok_or_else(|| fail(&self.state, id, RefreshErrorKind::Other, None))?;
        if self.state.is_refresh_dead(id, &old_rt) {
            tracing::debug!(account = %id, "openai oauth refresh skipped: refresh token known dead");
            return Err(RefreshErrorKind::InvalidGrant);
        }
        let req = self
            .build_request(&old_rt)
            .map_err(|k| fail(&self.state, id, k, None))?;
        let lim = Limits {
            header_wait: self.timeout,
            idle: self.timeout,
            deadline: Instant::now() + self.timeout,
        };
        let resp = match self.up.send(req, &lim).await {
            Ok(r) => r,
            Err(k) => return Err(fail(&self.state, id, RefreshErrorKind::Network(k), None)),
        };
        let status = resp.status.as_u16();
        let request_id = ["x-request-id", "request-id"]
            .iter()
            .find_map(|h| resp.headers.get(*h))
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        let body = match resp.body.text().await {
            Ok(b) => b,
            Err(k) => {
                return Err(fail(
                    &self.state,
                    id,
                    RefreshErrorKind::Network(k),
                    request_id,
                ))
            }
        };

        if !(200..300).contains(&status) {
            let kind = classify_failure(status, &body);
            if matches!(
                kind,
                RefreshErrorKind::InvalidGrant | RefreshErrorKind::TokenReused
            ) {
                self.state
                    .mark_refresh_dead(id, &old_rt, OffsetDateTime::now_utc());
            }
            return Err(fail(&self.state, id, kind, request_id));
        }

        let now = OffsetDateTime::now_utc();
        let parsed: Option<TokenResponse> = serde_json::from_str(&body).ok();
        let Some((access, parsed)) = parsed.and_then(|p| {
            let a = p.access_token.clone().filter(|a| !a.is_empty())?;
            Some((a, p))
        }) else {
            return Err(fail(&self.state, id, RefreshErrorKind::Other, request_id));
        };
        let claims = parsed
            .id_token
            .as_deref()
            .and_then(id_token_claims)
            .unwrap_or_default();
        let next = Credentials {
            expires_at: Some(token_expiry(parsed.expires_in, &access, now)),
            access_token: access.clone(),
            // Rotation: persist the new refresh token, else keep the old one.
            refresh_token: Some(
                parsed
                    .refresh_token
                    .filter(|t| !t.is_empty())
                    .unwrap_or_else(|| old_rt.clone()),
            ),
            plan: claims.plan.or(cur.plan.clone()),
            openai_account_id: match (cur.openai_account_id.clone(), claims.account_id) {
                // Never switch an account to another workspace behind the user's back.
                (Some(stored), Some(new)) if stored != new => {
                    tracing::warn!(account = %id, "openai oauth refresh returned a different ChatGPT account id; keeping the stored one");
                    Some(stored)
                }
                (stored, new) => stored.or(new),
            },
            // The token endpoint sends no refresh expiry; a recorded one that has passed was
            // disproven by this refresh succeeding (same rule as Anthropic).
            refresh_expires_at: crate::providers::anthropic::expiry_after_refresh(
                cur.refresh_expires_at,
                None,
                now,
            ),
            ..cur
        };
        // Persist (atomically, before anyone uses the new token). The old refresh token may now be
        // dead server-side, so a failed write is retried once and otherwise reported loudly.
        let persisted = {
            let (reg, pid) = (self.reg.clone(), id.clone());
            tokio::task::spawn_blocking(move || {
                reg.write_creds_unlocked(&pid, &next)
                    .or_else(|_| reg.write_creds_unlocked(&pid, &next))
            })
            .await
        };
        if !matches!(persisted, Ok(Ok(()))) {
            tracing::error!(account = %id, "openai oauth refresh succeeded but persisting the rotated credentials failed");
            return Err(fail(&self.state, id, RefreshErrorKind::Other, request_id));
        }
        if self.state.is_refresh_dead(id, &old_rt) {
            self.state.clear_refresh_dead(id, now);
        }
        // A refresh that works also settles an account whose stored state was unreadable.
        self.state.mark_verified(id, now);
        tracing::info!(account = %id, request_id = request_id.as_deref().unwrap_or("-"), "openai oauth refresh ok");
        Ok(access)
    }

    /// The JSON refresh grant (bro `refreshOpenAIToken`, INV §4).
    fn build_request(&self, refresh_token: &str) -> Result<reqwest::Request, RefreshErrorKind> {
        let url: reqwest::Url = self.cfg.token_url.parse().map_err(|_| {
            tracing::error!("openai token_url is not a valid URL");
            RefreshErrorKind::Other
        })?;
        let body = serde_json::json!({
            "grant_type": "refresh_token",
            "refresh_token": refresh_token,
            "client_id": self.cfg.client_id,
            "scope": self.cfg.scope,
        });
        let mut req = reqwest::Request::new(reqwest::Method::POST, url);
        req.headers_mut().insert(
            http::header::CONTENT_TYPE,
            http::HeaderValue::from_static("application/json"),
        );
        *req.body_mut() = Some(reqwest::Body::from(Bytes::from(body.to_string())));
        Ok(req)
    }
}

/// Classify a non-2xx token response by its OAuth error code only (the message is never kept).
/// Dead-token codes count only on the statuses the endpoint uses for them (400/401).
fn classify_failure(status: u16, body: &str) -> RefreshErrorKind {
    if !matches!(status, 400 | 401) {
        return RefreshErrorKind::Server(status);
    }
    let json: Value = serde_json::from_str(body).unwrap_or_default();
    let error = json.get("error");
    // `{"error":"<code>"}` (RFC 6749), `{"error":{"code"|"type":"<code>"}}` (OpenAI envelope),
    // or a top-level `{"code":"<code>"}`.
    let codes = [
        error,
        error.and_then(|e| e.get("code")),
        error.and_then(|e| e.get("type")),
        json.get("code"),
    ];
    let has = |want: &str| {
        codes
            .iter()
            .any(|c| c.and_then(Value::as_str) == Some(want))
    };
    if has("refresh_token_reused") {
        RefreshErrorKind::TokenReused
    } else if [
        "invalid_grant",
        "refresh_token_expired",
        "refresh_token_invalidated",
    ]
    .iter()
    .any(|c| has(c))
    {
        // codex-rs treats expired / invalidated refresh tokens as "sign in again" too.
        RefreshErrorKind::InvalidGrant
    } else {
        RefreshErrorKind::Server(status)
    }
}

/// Fakes shared by the OpenAI OAuth tests (refresh here, sign-in in `signin::openai`).
#[cfg(test)]
pub(crate) mod testkit {
    use std::convert::Infallible;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use base64::Engine;
    use bytes::Bytes;
    use http_body_util::{BodyExt, Full};
    use hyper::service::service_fn;
    use hyper::{Request, Response};
    use hyper_util::rt::TokioIo;
    use tokio::net::TcpListener;

    /// An unsigned JWT with `payload` (what the id_token parser needs).
    pub fn fake_jwt(payload: &serde_json::Value) -> String {
        let enc = |v: &serde_json::Value| URL_SAFE_NO_PAD.encode(v.to_string());
        format!(
            "{}.{}.fake-signature",
            enc(&serde_json::json!({"alg": "RS256", "typ": "JWT"})),
            enc(payload)
        )
    }

    /// An id_token for ChatGPT account `account_id`.
    pub fn id_token(account_id: &str, email: &str, plan: &str) -> String {
        fake_jwt(&serde_json::json!({
            "email": email,
            "https://api.openai.com/auth": {
                "chatgpt_account_id": account_id,
                "chatgpt_plan_type": plan,
            },
        }))
    }

    /// An id_token that also names the ChatGPT user (`chatgpt_user_id`).
    pub fn id_token_with_user(account_id: &str, user_id: &str, email: &str) -> String {
        fake_jwt(&serde_json::json!({
            "email": email,
            "https://api.openai.com/auth": {
                "chatgpt_account_id": account_id,
                "chatgpt_user_id": user_id,
                "chatgpt_plan_type": "team",
            },
        }))
    }

    /// Scripted reply for one token-endpoint hit.
    pub struct Reply {
        pub status: u16,
        pub body: String,
        pub delay: Duration,
    }

    pub fn reply(status: u16, body: impl Into<String>) -> Reply {
        Reply {
            status,
            body: body.into(),
            delay: Duration::ZERO,
        }
    }

    /// One request the fake received: content type and raw body.
    #[derive(Debug, Clone)]
    pub struct Seen {
        pub content_type: String,
        pub body: String,
    }

    pub struct FakeToken {
        pub url: String,
        pub hits: Arc<AtomicUsize>,
        pub seen: Arc<Mutex<Vec<Seen>>>,
    }

    impl FakeToken {
        pub fn hits(&self) -> usize {
            self.hits.load(Ordering::SeqCst)
        }
        pub fn seen(&self, i: usize) -> Seen {
            self.seen.lock().unwrap()[i].clone()
        }
    }

    /// Token endpoint on 127.0.0.1:0 (`/oauth/token`); `script(hit_index)` decides each reply.
    pub async fn fake_token_endpoint(
        script: impl Fn(usize) -> Reply + Send + Sync + 'static,
    ) -> FakeToken {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/oauth/token", listener.local_addr().unwrap());
        let hits = Arc::new(AtomicUsize::new(0));
        let seen = Arc::new(Mutex::new(Vec::new()));
        let script = Arc::new(script);
        let (h, s) = (hits.clone(), seen.clone());
        tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                let (script, h, s) = (script.clone(), h.clone(), s.clone());
                tokio::spawn(async move {
                    let svc = service_fn(move |req: Request<hyper::body::Incoming>| {
                        let (script, h, s) = (script.clone(), h.clone(), s.clone());
                        async move {
                            assert_eq!(req.method(), "POST");
                            let content_type = req
                                .headers()
                                .get("content-type")
                                .and_then(|v| v.to_str().ok())
                                .unwrap_or_default()
                                .to_string();
                            let raw = req.into_body().collect().await.unwrap().to_bytes();
                            s.lock().unwrap().push(Seen {
                                content_type,
                                body: String::from_utf8_lossy(&raw).into_owned(),
                            });
                            let r = script(h.fetch_add(1, Ordering::SeqCst));
                            tokio::time::sleep(r.delay).await;
                            Ok::<_, Infallible>(
                                Response::builder()
                                    .status(r.status)
                                    .header("x-request-id", "req_oa_test")
                                    .body(Full::new(Bytes::from(r.body)))
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
        FakeToken { url, hits, seen }
    }
}

#[cfg(test)]
mod tests {
    use super::testkit::*;
    use super::*;
    use std::sync::Arc;

    use sha2::{Digest, Sha256};
    use tokio::net::TcpListener;

    use crate::accounts::AccountStatus;
    use crate::config::Paths;
    use crate::store::Store;

    const OLD_RT: &str = "old-openai-refresh-SECRET";
    const OLD_AT: &str = "old-openai-access-SECRET";

    struct Fx {
        _dir: tempfile::TempDir,
        reg: Arc<Registry>,
        state: Arc<StateBook>,
        meta: AccountMeta,
    }

    impl Fx {
        fn provider(&self, token_url: &str) -> OpenAiProvider {
            let cfg = OpenAiCfg {
                token_url: token_url.into(),
                ..OpenAiCfg::default()
            };
            OpenAiProvider::new(
                cfg,
                self.reg.clone(),
                self.state.clone(),
                Arc::new(Upstream::new()),
            )
        }
        fn creds(&self) -> Credentials {
            self.reg.read_creds(&self.meta.id).unwrap().unwrap()
        }
        fn status(&self) -> AccountStatus {
            self.state
                .status(&self.meta.id, true, OffsetDateTime::now_utc())
        }
        fn dead_fp(&self) -> Option<String> {
            self.state
                .state(&self.meta.id, OffsetDateTime::now_utc())
                .dead_refresh_fp
        }
    }

    fn creds_expiring_in(secs: i64) -> Credentials {
        Credentials {
            provider: ProviderId::OpenAi,
            access_token: OLD_AT.into(),
            refresh_token: Some(OLD_RT.into()),
            expires_at: Some(OffsetDateTime::now_utc() + time::Duration::seconds(secs)),
            refresh_expires_at: None,
            scopes: Vec::new(),
            plan: Some("plus".into()),
            tier: None,
            openai_account_id: Some("acct-1".into()),
        }
    }

    fn fx(c: Credentials) -> Fx {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::under(dir.path());
        paths.ensure().unwrap();
        let reg = Arc::new(Registry::load(&paths).unwrap());
        let store = Arc::new(Store::open(&dir.path().join("pooltime.db")).unwrap());
        let state = Arc::new(StateBook::load(store).unwrap());
        let meta = AccountMeta::new("codex".parse().unwrap(), ProviderId::OpenAi);
        reg.upsert(meta.clone()).unwrap();
        reg.write_creds(&meta.id, &c).unwrap();
        Fx {
            _dir: dir,
            reg,
            state,
            meta,
        }
    }

    fn fingerprint(t: &str) -> String {
        hex::encode(Sha256::digest(t.as_bytes()))[..16].to_string()
    }

    #[tokio::test]
    async fn successful_refresh_heals_an_unverified_account_and_a_transient_failure_does_not() {
        let mut fx = fx(creds_expiring_in(3600));
        let store = Arc::new(Store::open(&fx._dir.path().join("unverified.db")).unwrap());
        store
            .exec(|c| {
                c.execute(
                    "INSERT INTO account_state(account, json) VALUES ('codex', '{corrupt')",
                    [],
                )?;
                Ok(())
            })
            .await
            .unwrap();
        fx.state = Arc::new(StateBook::load(store).unwrap());
        assert!(fx.state.is_unverified(&fx.meta.id));
        assert_eq!(fx.status(), AccountStatus::NeedsLogin);

        let down = fake_token_endpoint(|_| reply(503, "unavailable")).await;
        assert_eq!(
            fx.provider(&down.url)
                .access_token(&fx.meta, true)
                .await
                .unwrap_err(),
            RefreshErrorKind::Server(503)
        );
        assert!(fx.state.is_unverified(&fx.meta.id));

        let fake = fake_token_endpoint(|_| {
            reply(
                200,
                serde_json::json!({
                    "access_token": "healed",
                    "refresh_token": "rt2",
                    "expires_in": 3600,
                })
                .to_string(),
            )
        })
        .await;
        assert_eq!(
            fx.provider(&fake.url)
                .access_token(&fx.meta, true)
                .await
                .unwrap(),
            "healed"
        );
        assert!(!fx.state.is_unverified(&fx.meta.id));
        assert_eq!(fx.status(), AccountStatus::Ready);
    }

    #[test]
    fn account_id_from_id_token() {
        let tok = id_token("acct-123", "me@example.com", "pro");
        assert_eq!(
            id_token_claims(&tok),
            Some(IdTokenClaims {
                account_id: Some("acct-123".into()),
                user_id: None,
                email: Some("me@example.com".into()),
                plan: Some("pro".into()),
            })
        );
        // Identity: the workspace account id goes in the org slot (Team members share it), the
        // user id (absent here) in the uuid slot, the email alongside.
        let i = id_token_claims(&tok).unwrap().identity();
        assert_eq!(i.org_uuid.as_deref(), Some("acct-123"));
        assert_eq!(i.account_uuid, None);
        assert_eq!(i.email.as_deref(), Some("me@example.com"));

        // `chatgpt_user_id`, else `user_id`, names the person.
        let i = id_token_claims(&id_token_with_user("ws-1", "user-1", "a@b.c"))
            .unwrap()
            .identity();
        assert_eq!(
            (i.account_uuid.as_deref(), i.org_uuid.as_deref()),
            (Some("user-1"), Some("ws-1"))
        );
        let tok = fake_jwt(&serde_json::json!({
            "https://api.openai.com/auth": {"chatgpt_account_id": "ws-2", "user_id": "user-2"},
        }));
        assert_eq!(
            id_token_claims(&tok).unwrap().user_id.as_deref(),
            Some("user-2")
        );

        // Email from the profile claim; missing / empty claims are absent.
        let tok = fake_jwt(&serde_json::json!({
            "https://api.openai.com/profile": {"email": "p@example.com"},
            "https://api.openai.com/auth": {"chatgpt_account_id": ""},
        }));
        assert_eq!(
            id_token_claims(&tok),
            Some(IdTokenClaims {
                account_id: None,
                user_id: None,
                email: Some("p@example.com".into()),
                plan: None,
            })
        );
        // Base64 padding tolerated.
        let padded = {
            let mut parts: Vec<String> = id_token("acct-p", "x@y.z", "plus")
                .split('.')
                .map(str::to_owned)
                .collect();
            while !parts[1].len().is_multiple_of(4) {
                parts[1].push('=');
            }
            parts.join(".")
        };
        assert_eq!(
            id_token_claims(&padded).unwrap().account_id.as_deref(),
            Some("acct-p")
        );
        // Not a JWT.
        for bad in ["", "abc", "a.b", "a.!!!.c", "a.bm90IGpzb24.c"] {
            assert_eq!(id_token_claims(bad), None, "{bad}");
        }
    }

    #[test]
    fn codex_auth_json_import() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.json");
        let access = fake_jwt(&serde_json::json!({"exp": 2_000_000_000_i64}));
        std::fs::write(
            &path,
            serde_json::json!({
                "auth_mode": "ChatGPT",
                "OPENAI_API_KEY": null,
                "tokens": {
                    "id_token": id_token("acct-jwt", "me@example.com", "pro"),
                    "access_token": access,
                    "refresh_token": "fake-refresh",
                    "account_id": "acct-bare",
                },
                "last_refresh": "2026-11-01T00:00:00Z",
            })
            .to_string(),
        )
        .unwrap();
        let got = super::codex_auth_json_import(&path).unwrap();
        let c = &got.creds;
        assert_eq!(c.provider, ProviderId::OpenAi);
        assert_eq!(c.access_token, access);
        assert_eq!(c.refresh_token.as_deref(), Some("fake-refresh"));
        // The bare account id wins over the id_token claim (bro).
        assert_eq!(c.openai_account_id.as_deref(), Some("acct-bare"));
        assert_eq!(c.plan.as_deref(), Some("pro"));
        // Expiry from the access token's `exp`.
        assert_eq!(c.expires_at.unwrap().unix_timestamp(), 2_000_000_000);
        assert_eq!(got.identity.org_uuid.as_deref(), Some("acct-bare"));
        assert_eq!(got.identity.account_uuid, None, "no user id claim");
        assert_eq!(got.identity.email.as_deref(), Some("me@example.com"));

        // Browser-login shape: no bare account id, opaque access token.
        std::fs::write(
            &path,
            serde_json::json!({"tokens": {
                "id_token": id_token("acct-jwt", "me@example.com", "plus"),
                "access_token": "opaque-access",
                "refresh_token": "r",
            }})
            .to_string(),
        )
        .unwrap();
        let got = super::codex_auth_json_import(&path).unwrap();
        assert_eq!(got.creds.openai_account_id.as_deref(), Some("acct-jwt"));
        assert_eq!(got.creds.expires_at, None);
        assert_eq!(got.creds.plan.as_deref(), Some("plus"));

        // Refusals never echo the file content.
        for (raw, want) in [
            (r#"{"OPENAI_API_KEY":"sk-SECRET-KEY"}"#, "API key"),
            (
                r#"{"tokens":{"refresh_token":"SECRET-RT"}}"#,
                "no ChatGPT access token",
            ),
            ("not json SECRET", "not valid JSON"),
            ("[1,2]", "not a JSON object"),
        ] {
            std::fs::write(&path, raw).unwrap();
            let e = format!("{:#}", super::codex_auth_json_import(&path).unwrap_err());
            assert!(e.contains(want), "{e}");
            assert!(!e.contains("SECRET"), "{e}");
        }
        assert!(super::codex_auth_json_import(&dir.path().join("missing.json")).is_err());
    }

    #[test]
    fn default_codex_auth_path_prefers_codex_home() {
        let env = |pairs: &'static [(&'static str, &'static str)]| {
            move |k: &str| {
                pairs
                    .iter()
                    .find(|(n, _)| *n == k)
                    .map(|(_, v)| v.to_string())
            }
        };
        assert_eq!(
            default_codex_auth_path(env(&[("HOME", "/h"), ("CODEX_HOME", "/c")])),
            Some(PathBuf::from("/c/auth.json"))
        );
        assert_eq!(
            default_codex_auth_path(env(&[("HOME", "/h"), ("CODEX_HOME", "")])),
            Some(PathBuf::from("/h/.codex/auth.json"))
        );
        assert_eq!(default_codex_auth_path(env(&[])), None);
    }

    #[tokio::test]
    async fn refresh_posts_json_grant_and_persists_rotation() {
        let mut c = creds_expiring_in(60);
        // A recorded refresh expiry that has passed is disproven by the refresh succeeding.
        c.refresh_expires_at = Some(OffsetDateTime::now_utc() - time::Duration::days(1));
        let fx = fx(c);
        let fake = fake_token_endpoint(|_| {
            reply(
                200,
                serde_json::json!({
                    "access_token": "new-access",
                    "refresh_token": "new-refresh",
                    "id_token": id_token("acct-1", "me@example.com", "pro"),
                    "expires_in": 3600,
                })
                .to_string(),
            )
        })
        .await;
        let p = fx.provider(&fake.url);
        let before = OffsetDateTime::now_utc();
        assert_eq!(p.access_token(&fx.meta, false).await.unwrap(), "new-access");
        assert_eq!(fake.hits(), 1);

        let seen = fake.seen(0);
        assert_eq!(seen.content_type, "application/json");
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&seen.body).unwrap(),
            serde_json::json!({
                "grant_type": "refresh_token",
                "refresh_token": OLD_RT,
                "client_id": "app_EMoamEEZ73f0CkXaXp7hrann",
                "scope": "openid profile email",
            })
        );
        let c = fx.creds();
        assert_eq!(c.access_token, "new-access");
        assert_eq!(c.refresh_token.as_deref(), Some("new-refresh"));
        assert!(c.expires_at.unwrap() >= before + time::Duration::seconds(3600));
        assert_eq!(
            c.plan.as_deref(),
            Some("pro"),
            "plan follows the new id_token"
        );
        assert_eq!(c.openai_account_id.as_deref(), Some("acct-1"));
        assert_eq!(c.refresh_expires_at, None);

        // Fresh now: no second call.
        assert_eq!(p.access_token(&fx.meta, false).await.unwrap(), "new-access");
        assert_eq!(fake.hits(), 1);
        assert_eq!(fx.status(), AccountStatus::Ready);
    }

    #[tokio::test]
    async fn refresh_without_rotation_keeps_old_refresh_token_and_account_id() {
        let fx = fx(creds_expiring_in(10));
        let fake =
            fake_token_endpoint(|_| reply(200, r#"{"access_token":"a2","expires_in":60}"#)).await;
        let p = fx.provider(&fake.url);
        assert_eq!(p.access_token(&fx.meta, false).await.unwrap(), "a2");
        let c = fx.creds();
        assert_eq!(c.refresh_token.as_deref(), Some(OLD_RT));
        assert_eq!(c.openai_account_id.as_deref(), Some("acct-1"));
        assert_eq!(c.plan.as_deref(), Some("plus"));
    }

    async fn assert_marks_dead(status: u16, body: &'static str, want: RefreshErrorKind) {
        let fx = fx(creds_expiring_in(10));
        let fake = fake_token_endpoint(move |_| reply(status, body)).await;
        let p = fx.provider(&fake.url);
        assert_eq!(
            p.access_token(&fx.meta, false).await.unwrap_err(),
            want,
            "{status} {body}"
        );
        assert_eq!(fx.dead_fp(), Some(fingerprint(OLD_RT)), "{status} {body}");
        assert_eq!(fx.status(), AccountStatus::NeedsLogin, "{status} {body}");
        let st = fx.state.state(&fx.meta.id, OffsetDateTime::now_utc());
        let last = st.last_error.clone().unwrap();
        assert_eq!(last.reason, FailReason::Refresh(want));
        assert_eq!(last.upstream_request_id.as_deref(), Some("req_oa_test"));
        assert!(!format!("{st:?}").contains(OLD_RT));
        assert_eq!(fx.creds().refresh_token.as_deref(), Some(OLD_RT));

        // Known dead: never re-POSTed, forced or not.
        for force in [false, true] {
            assert!(p.access_token(&fx.meta, force).await.is_err());
        }
        assert_eq!(fake.hits(), 1);
    }

    #[tokio::test]
    async fn invalid_grant_marks_dead() {
        for (status, body) in [
            (400, r#"{"error":"invalid_grant","error_description":"x"}"#),
            (401, r#"{"error":"invalid_grant"}"#),
            (400, r#"{"error":{"code":"invalid_grant","message":"x"}}"#),
        ] {
            assert_marks_dead(status, body, RefreshErrorKind::InvalidGrant).await;
        }
    }

    #[tokio::test]
    async fn refresh_token_reused_marks_dead() {
        for (status, body) in [
            (
                401,
                r#"{"error":{"message":"Your refresh token has already been used","type":"invalid_request_error","code":"refresh_token_reused"}}"#,
            ),
            (400, r#"{"error":"refresh_token_reused"}"#),
            (401, r#"{"code":"refresh_token_reused"}"#),
        ] {
            assert_marks_dead(status, body, RefreshErrorKind::TokenReused).await;
        }
    }

    #[tokio::test]
    async fn refresh_token_expired_marks_dead() {
        for (status, body) in [
            (
                401,
                r#"{"error":{"message":"x","type":"invalid_request_error","code":"refresh_token_expired"}}"#,
            ),
            (400, r#"{"error":"refresh_token_expired"}"#),
        ] {
            assert_marks_dead(status, body, RefreshErrorKind::InvalidGrant).await;
        }
    }

    #[tokio::test]
    async fn refresh_token_invalidated_marks_dead() {
        for (status, body) in [
            (401, r#"{"error":{"code":"refresh_token_invalidated"}}"#),
            (400, r#"{"code":"refresh_token_invalidated"}"#),
        ] {
            assert_marks_dead(status, body, RefreshErrorKind::InvalidGrant).await;
        }
        // Only on 400/401.
        let fx = fx(creds_expiring_in(10));
        let fake =
            fake_token_endpoint(|_| reply(503, r#"{"error":"refresh_token_invalidated"}"#)).await;
        assert_eq!(
            fx.provider(&fake.url)
                .access_token(&fx.meta, false)
                .await
                .unwrap_err(),
            RefreshErrorKind::Server(503)
        );
        assert_eq!(fx.dead_fp(), None);
    }

    #[tokio::test]
    async fn refresh_never_replaces_the_stored_account_id() {
        let fx = fx(creds_expiring_in(10));
        let fake = fake_token_endpoint(|_| {
            reply(
                200,
                serde_json::json!({
                    "access_token": "a2",
                    "id_token": id_token("acct-OTHER", "me@example.com", "pro"),
                })
                .to_string(),
            )
        })
        .await;
        let p = fx.provider(&fake.url);
        assert_eq!(p.access_token(&fx.meta, false).await.unwrap(), "a2");
        assert_eq!(fx.creds().openai_account_id.as_deref(), Some("acct-1"));

        // An account without a stored id adopts the token's.
        let mut c = creds_expiring_in(10);
        c.openai_account_id = None;
        let fx = self::fx(c);
        let p = fx.provider(&fake.url);
        p.access_token(&fx.meta, false).await.unwrap();
        assert_eq!(fx.creds().openai_account_id.as_deref(), Some("acct-OTHER"));
    }

    #[tokio::test]
    async fn server_and_transport_errors_never_mark_dead() {
        for (status, body, want) in [
            (
                500,
                r#"{"error":"invalid_grant"}"#,
                RefreshErrorKind::Server(500),
            ),
            (
                503,
                r#"{"error":{"code":"refresh_token_reused"}}"#,
                RefreshErrorKind::Server(503),
            ),
            (
                403,
                r#"{"error":"invalid_grant"}"#,
                RefreshErrorKind::Server(403),
            ),
            (
                400,
                r#"{"error":"invalid_scope"}"#,
                RefreshErrorKind::Server(400),
            ),
            (200, "<html>", RefreshErrorKind::Other),
        ] {
            let fx = fx(creds_expiring_in(10));
            let fake = fake_token_endpoint(move |_| reply(status, body)).await;
            let p = fx.provider(&fake.url);
            assert_eq!(p.access_token(&fx.meta, false).await.unwrap_err(), want);
            assert_eq!(fx.dead_fp(), None, "{status} {body}");
            assert_eq!(fx.status(), AccountStatus::Ready, "{status} {body}");
            assert_eq!(fx.creds().access_token, OLD_AT);
        }
        // Connection refused.
        let fx = fx(creds_expiring_in(10));
        let dead_url = {
            let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
            format!("http://{}/oauth/token", l.local_addr().unwrap())
        };
        let err = fx
            .provider(&dead_url)
            .access_token(&fx.meta, false)
            .await
            .unwrap_err();
        assert!(matches!(err, RefreshErrorKind::Network(_)), "{err:?}");
        assert_eq!(fx.dead_fp(), None);
        assert_eq!(fx.status(), AccountStatus::Ready);
    }

    #[tokio::test]
    async fn concurrent_callers_share_one_refresh() {
        let fx = fx(creds_expiring_in(10));
        let fake = fake_token_endpoint(|_| Reply {
            delay: Duration::from_millis(150),
            ..reply(200, r#"{"access_token":"shared","refresh_token":"rt2"}"#)
        })
        .await;
        let p = Arc::new(fx.provider(&fake.url));
        let tasks: Vec<_> = (0..6)
            .map(|i| {
                let (p, meta) = (p.clone(), fx.meta.clone());
                tokio::spawn(async move { p.access_token(&meta, i % 2 == 0).await })
            })
            .collect();
        for t in tasks {
            assert_eq!(t.await.unwrap().unwrap(), "shared");
        }
        assert_eq!(fake.hits(), 1);
    }

    #[tokio::test]
    async fn cancelled_caller_does_not_lose_the_rotated_token() {
        let fx = fx(creds_expiring_in(10));
        let fake = fake_token_endpoint(|_| Reply {
            delay: Duration::from_millis(300),
            ..reply(200, r#"{"access_token":"rot-a","refresh_token":"rot-r"}"#)
        })
        .await;
        let p = Arc::new(fx.provider(&fake.url));
        let caller = {
            let (p, meta) = (p.clone(), fx.meta.clone());
            tokio::spawn(async move { p.access_token(&meta, false).await })
        };
        for _ in 0..200 {
            if fake.hits() == 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        caller.abort();
        let mut ok = false;
        for _ in 0..200 {
            if fx.creds().refresh_token.as_deref() == Some("rot-r") {
                ok = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(
            ok,
            "rotated refresh token must be persisted after caller abort"
        );
        assert_eq!(p.access_token(&fx.meta, false).await.unwrap(), "rot-a");
        assert_eq!(fake.hits(), 1);
    }

    #[test]
    fn token_expiry_prefers_expires_in_then_jwt_exp() {
        let now = OffsetDateTime::from_unix_timestamp(1_000_000).unwrap();
        let jwt = fake_jwt(&serde_json::json!({"exp": 1_500_000}));
        assert_eq!(
            token_expiry(Some(60), &jwt, now).unix_timestamp(),
            1_000_060
        );
        assert_eq!(token_expiry(None, &jwt, now).unix_timestamp(), 1_500_000);
        assert_eq!(
            token_expiry(None, "opaque", now).unix_timestamp(),
            1_003_600
        );
        assert!(token_expiry(Some(i64::MAX), "x", now) > now);
    }
}
