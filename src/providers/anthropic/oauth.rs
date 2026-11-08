//! OAuth refresh for Claude subscription accounts (docs/spikes.md S1/S2).
//!
//! Exclusion layers, outermost first: a per-account async mutex (callers in this process share one
//! refresh), then the per-account `flock` (other pooltime processes), both held across the token
//! HTTP call. After each layer is acquired the credentials are re-read, because whoever held it
//! may have refreshed (refresh tokens rotate, so spending the old one again would kill the
//! account). Nothing here logs or persists a token or an upstream message: only the classified
//! [`RefreshErrorKind`], the account and the upstream `request-id`.

use std::sync::Arc;

use bytes::Bytes;
use serde::Deserialize;
use time::OffsetDateTime;
use tokio::time::Instant;

use super::AnthropicProvider;
use crate::accounts::{AccountId, AccountMeta, Credentials, ErrorRecord};
use crate::errors::{FailReason, RefreshErrorKind};
use crate::fsutil::FileLock;
use crate::upstream::Limits;

/// Used when the token response omits `expires_in` (bro's default).
const DEFAULT_EXPIRES_IN_SECS: i64 = 3600;
/// Upper bound on server-supplied lifetimes so date arithmetic can never overflow.
const MAX_LIFETIME_SECS: i64 = 10 * 365 * 86_400;

/// New `refresh_expires_at`: the earlier of the old value and `now + new_secs`. Either side may be
/// absent; both absent stays absent.
pub fn expiry_after_refresh(
    old: Option<OffsetDateTime>,
    new_secs: Option<i64>,
    now: OffsetDateTime,
) -> Option<OffsetDateTime> {
    let new = new_secs.map(|s| now + time::Duration::seconds(s.clamp(0, MAX_LIFETIME_SECS)));
    match (old, new) {
        (Some(o), Some(n)) => Some(o.min(n)),
        (o, n) => o.or(n),
    }
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: Option<String>,
    refresh_token: Option<String>,
    expires_in: Option<i64>,
    refresh_token_expires_in: Option<i64>,
    scope: Option<String>,
}

impl AnthropicProvider {
    /// A usable access token for `acct`, refreshing when `force` or when it expires within the
    /// skew. A token without an expiry counts as fresh; an account without a refresh token returns
    /// its stored token unless `force`. `refresh_expires_at` is never consulted: only the OAuth
    /// endpoint can declare a refresh token dead.
    pub async fn access_token(
        &self,
        acct: &AccountMeta,
        force: bool,
    ) -> Result<String, RefreshErrorKind> {
        let id = &acct.id;
        let creds = self.load(id)?;
        if let Some(r) = self.settled(&creds, force, None) {
            return r;
        }
        // The token this caller saw; a different one later means someone else refreshed.
        let seen = creds.access_token;

        let gate = self.gate(id);
        let _in_process = gate.lock().await;
        let creds = self.load(id)?;
        if let Some(r) = self.settled(&creds, force, Some(&seen)) {
            return r;
        }

        // flock blocks the thread: acquire it off the async workers.
        let flock = {
            let (reg, lock_id) = (self.reg.clone(), id.clone());
            tokio::task::spawn_blocking(move || reg.lock(&lock_id))
                .await
                .map_err(|_| RefreshErrorKind::Other)?
                .map_err(|e| {
                    tracing::warn!(account = %id, error_kind = ?e.kind(), "oauth refresh could not take the account lock");
                    RefreshErrorKind::Other
                })?
        };
        let creds = self.load(id)?;
        if let Some(r) = self.settled(&creds, force, Some(&seen)) {
            return r;
        }
        self.refresh(id, creds, &flock).await
    }

    /// `Some(result)` when no refresh is needed (or possible) for `creds`.
    fn settled(
        &self,
        creds: &Credentials,
        force: bool,
        seen: Option<&str>,
    ) -> Option<Result<String, RefreshErrorKind>> {
        let now = OffsetDateTime::now_utc();
        let fresh = creds.expires_at.is_none_or(|e| e - now > self.skew);
        // A forced refresh is satisfied by another caller's refresh (token changed since we looked).
        let satisfied = !force || seen.is_some_and(|s| s != creds.access_token);
        if fresh && satisfied {
            return Some(Ok(creds.access_token.clone()));
        }
        if creds.refresh_token.is_none() {
            return Some(if force {
                Err(RefreshErrorKind::Other)
            } else {
                Ok(creds.access_token.clone())
            });
        }
        None
    }

    fn load(&self, id: &AccountId) -> Result<Credentials, RefreshErrorKind> {
        match self.reg.read_creds(id) {
            Ok(Some(c)) => Ok(c),
            Ok(None) => {
                tracing::warn!(account = %id, "oauth refresh: account has no credentials");
                Err(RefreshErrorKind::Other)
            }
            // read_creds already logged the sanitized corruption reason.
            Err(_) => Err(RefreshErrorKind::Other),
        }
    }

    fn gate(&self, id: &AccountId) -> Arc<tokio::sync::Mutex<()>> {
        let mut m = self.gates.lock().unwrap_or_else(|e| e.into_inner());
        m.entry(id.clone()).or_default().clone()
    }

    /// One token-endpoint call. The caller holds the in-process gate and the account flock
    /// (`_flock` is the guard [`crate::accounts::Registry::lock`] returned).
    async fn refresh(
        &self,
        id: &AccountId,
        cur: Credentials,
        _flock: &FileLock,
    ) -> Result<String, RefreshErrorKind> {
        let old_rt = cur.refresh_token.clone().ok_or(RefreshErrorKind::Other)?;
        let req = self.build_request(&cur, &old_rt)?;
        let lim = Limits {
            header_wait: self.refresh_timeout,
            idle: self.refresh_timeout,
            deadline: Instant::now() + self.refresh_timeout,
        };

        let resp = match self.up.send(req, &lim).await {
            Ok(r) => r,
            Err(k) => return Err(self.fail(id, RefreshErrorKind::Network(k), None)),
        };
        let status = resp.status.as_u16();
        let request_id = resp
            .headers
            .get("request-id")
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        let body = match resp.body.text().await {
            Ok(b) => b,
            Err(k) => return Err(self.fail(id, RefreshErrorKind::Network(k), request_id)),
        };

        if !(200..300).contains(&status) {
            let kind = classify_failure(status, &body);
            match kind {
                RefreshErrorKind::InvalidGrant => {
                    self.state
                        .mark_refresh_dead(id, &old_rt, OffsetDateTime::now_utc())
                }
                RefreshErrorKind::AccountOnHold => {
                    self.state.mark_billing(id, OffsetDateTime::now_utc())
                }
                _ => {}
            }
            return Err(self.fail(id, kind, request_id));
        }

        let now = OffsetDateTime::now_utc();
        let parsed: Option<TokenResponse> = serde_json::from_str(&body).ok();
        let Some((access, parsed)) = parsed.and_then(|p| {
            let a = p.access_token.clone().filter(|a| !a.is_empty())?;
            Some((a, p))
        }) else {
            return Err(self.fail(id, RefreshErrorKind::Other, request_id));
        };

        let lifetime = parsed
            .expires_in
            .unwrap_or(DEFAULT_EXPIRES_IN_SECS)
            .clamp(1, MAX_LIFETIME_SECS);
        let scopes: Vec<String> = parsed
            .scope
            .as_deref()
            .map(|s| s.split_whitespace().map(str::to_owned).collect())
            .filter(|v: &Vec<String>| !v.is_empty())
            .unwrap_or_else(|| cur.scopes.clone());
        let next = Credentials {
            access_token: access.clone(),
            // Rotation: persist the new refresh token, else keep the old one.
            refresh_token: Some(
                parsed
                    .refresh_token
                    .filter(|t| !t.is_empty())
                    .unwrap_or(old_rt),
            ),
            expires_at: Some(now + time::Duration::seconds(lifetime)),
            refresh_expires_at: expiry_after_refresh(
                cur.refresh_expires_at,
                parsed.refresh_token_expires_in,
                now,
            ),
            scopes,
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
            tracing::error!(account = %id, "oauth refresh succeeded but persisting the rotated credentials failed");
            return Err(self.fail(id, RefreshErrorKind::Other, request_id));
        }

        // A successful refresh proves the token we spent was alive.
        if self.state.state(id, now).dead_refresh_fp.is_some() {
            self.state.clear_refresh_dead(id, now);
        }
        tracing::info!(account = %id, request_id = request_id.as_deref().unwrap_or("-"), "oauth refresh ok");
        Ok(access)
    }

    fn build_request(
        &self,
        cur: &Credentials,
        refresh_token: &str,
    ) -> Result<reqwest::Request, RefreshErrorKind> {
        let url: reqwest::Url = self.cfg.token_url.parse().map_err(|_| {
            tracing::error!("anthropic token_url is not a valid URL");
            RefreshErrorKind::Other
        })?;
        // Claude Code 2.1.319 sends `scope` (bro omits it). Prefer the scopes this credential was
        // granted (always requestable); fall back to the configured set.
        let scope = if cur.scopes.is_empty() {
            self.cfg.scope.clone()
        } else {
            cur.scopes.join(" ")
        };
        let mut body = serde_json::json!({
            "grant_type": "refresh_token",
            "refresh_token": refresh_token,
            "client_id": self.cfg.client_id,
        });
        if !scope.is_empty() {
            body["scope"] = scope.into();
        }
        let mut req = reqwest::Request::new(reqwest::Method::POST, url);
        req.headers_mut().insert(
            http::header::CONTENT_TYPE,
            http::HeaderValue::from_static("application/json"),
        );
        *req.body_mut() = Some(reqwest::Body::from(Bytes::from(body.to_string())));
        Ok(req)
    }

    /// Log the classified outcome, record it as the account's last error, return it.
    fn fail(
        &self,
        id: &AccountId,
        kind: RefreshErrorKind,
        request_id: Option<String>,
    ) -> RefreshErrorKind {
        tracing::warn!(
            account = %id,
            outcome = %kind.detail(),
            request_id = request_id.as_deref().unwrap_or("-"),
            "oauth refresh failed"
        );
        self.state.record_error(
            id,
            ErrorRecord {
                at: OffsetDateTime::now_utc(),
                reason: FailReason::Refresh(kind),
                upstream_request_id: request_id,
            },
        );
        kind
    }
}

/// Classify a non-2xx token response. Looks only at structure and the `account_on_hold` marker; the
/// message text is never kept.
fn classify_failure(status: u16, body: &str) -> RefreshErrorKind {
    if matches!(status, 400 | 401 | 403) && body.contains("account_on_hold") {
        return RefreshErrorKind::AccountOnHold;
    }
    let json: serde_json::Value = serde_json::from_str(body).unwrap_or_default();
    let is = |v: Option<&serde_json::Value>| v.and_then(|v| v.as_str()) == Some("invalid_grant");
    let error = json.get("error");
    // `{"error":"invalid_grant"}` (the endpoint's spec shape) or Anthropic's envelope
    // `{"error":{"type":"invalid_grant"}}`.
    if is(error) || is(error.and_then(|e| e.get("type"))) || is(error.and_then(|e| e.get("error")))
    {
        return RefreshErrorKind::InvalidGrant;
    }
    RefreshErrorKind::Server(status)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::convert::Infallible;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;
    use std::time::Duration;

    use http_body_util::{BodyExt, Full};
    use hyper::service::service_fn;
    use hyper::{Request, Response};
    use hyper_util::rt::TokioIo;
    use sha2::{Digest, Sha256};
    use time::macros::datetime;
    use tokio::net::TcpListener;

    use crate::accounts::Registry;
    use crate::accounts::{AccountStatus, StateBook};
    use crate::config::{AnthropicCfg, Paths};
    use crate::providers::ProviderId;
    use crate::store::Store;
    use crate::upstream::Upstream;

    const OLD_RT: &str = "old-refresh-token-SECRET";
    const OLD_AT: &str = "old-access-token-SECRET";

    /// Scripted reply for one token-endpoint hit.
    struct Reply {
        status: u16,
        body: String,
        delay: Duration,
    }

    fn reply(status: u16, body: &str) -> Reply {
        Reply {
            status,
            body: body.into(),
            delay: Duration::ZERO,
        }
    }

    fn ok_body(access: &str, refresh: Option<&str>, expires_in: i64) -> String {
        let mut v = serde_json::json!({"access_token": access, "expires_in": expires_in});
        if let Some(r) = refresh {
            v["refresh_token"] = r.into();
        }
        v.to_string()
    }

    struct Fake {
        url: String,
        hits: Arc<AtomicUsize>,
        bodies: Arc<Mutex<Vec<serde_json::Value>>>,
    }

    /// Token endpoint on 127.0.0.1:0; `script(hit_index)` decides each reply.
    async fn fake_token_endpoint(script: impl Fn(usize) -> Reply + Send + Sync + 'static) -> Fake {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/v1/oauth/token", listener.local_addr().unwrap());
        let hits = Arc::new(AtomicUsize::new(0));
        let bodies = Arc::new(Mutex::new(Vec::new()));
        let script = Arc::new(script);
        let (h, b) = (hits.clone(), bodies.clone());
        tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                let (script, h, b) = (script.clone(), h.clone(), b.clone());
                tokio::spawn(async move {
                    let svc = service_fn(move |req: Request<hyper::body::Incoming>| {
                        let (script, h, b) = (script.clone(), h.clone(), b.clone());
                        async move {
                            assert_eq!(req.method(), "POST");
                            assert_eq!(req.headers()["content-type"], "application/json");
                            let raw = req.into_body().collect().await.unwrap().to_bytes();
                            b.lock()
                                .unwrap()
                                .push(serde_json::from_slice(&raw).unwrap());
                            let r = script(h.fetch_add(1, Ordering::SeqCst));
                            tokio::time::sleep(r.delay).await;
                            Ok::<_, Infallible>(
                                Response::builder()
                                    .status(r.status)
                                    .header("request-id", "req_test123")
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
        Fake { url, hits, bodies }
    }

    struct Fx {
        _dir: tempfile::TempDir,
        reg: Arc<Registry>,
        state: Arc<StateBook>,
        meta: AccountMeta,
    }

    impl Fx {
        fn provider(&self, token_url: &str) -> AnthropicProvider {
            let cfg = AnthropicCfg {
                token_url: token_url.into(),
                ..AnthropicCfg::default()
            };
            AnthropicProvider::new(
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
    }

    fn creds_expiring_in(secs: i64) -> Credentials {
        Credentials {
            provider: ProviderId::Anthropic,
            access_token: OLD_AT.into(),
            refresh_token: Some(OLD_RT.into()),
            expires_at: Some(OffsetDateTime::now_utc() + time::Duration::seconds(secs)),
            refresh_expires_at: None,
            scopes: vec!["user:inference".into(), "user:profile".into()],
            plan: Some("max".into()),
            tier: None,
            openai_account_id: None,
        }
    }

    fn fx(c: Credentials) -> Fx {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::under(dir.path());
        paths.ensure().unwrap();
        let reg = Arc::new(Registry::load(&paths).unwrap());
        let store = Arc::new(Store::open(&dir.path().join("pooltime.db")).unwrap());
        let state = Arc::new(StateBook::load(store).unwrap());
        let meta = AccountMeta::new("work".parse().unwrap(), ProviderId::Anthropic);
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
    async fn refreshes_within_skew_and_persists_rotated_tokens() {
        let mut c = creds_expiring_in(60); // inside the 300 s skew
        c.refresh_expires_at = Some(OffsetDateTime::now_utc() + time::Duration::days(90));
        let fx = fx(c);
        let fake = fake_token_endpoint(|_| {
            let mut v: serde_json::Value =
                serde_json::from_str(&ok_body("new-access", Some("new-refresh"), 3600)).unwrap();
            v["refresh_token_expires_in"] = (30 * 86_400).into();
            v["scope"] = "user:inference user:profile user:file_upload".into();
            reply(200, &v.to_string())
        })
        .await;
        let p = fx.provider(&fake.url);

        let before = OffsetDateTime::now_utc();
        let tok = p.access_token(&fx.meta, false).await.unwrap();
        assert_eq!(tok, "new-access");
        assert_eq!(fake.hits.load(Ordering::SeqCst), 1);

        // Request body matches Claude Code's refresh JSON (scope = the credential's scopes).
        let body = fake.bodies.lock().unwrap()[0].clone();
        assert_eq!(
            body,
            serde_json::json!({
                "grant_type": "refresh_token",
                "refresh_token": OLD_RT,
                "client_id": "9d1c250a-e61b-44d9-88ed-5944d1962f5e",
                "scope": "user:inference user:profile",
            })
        );

        // Rotated credentials are on disk.
        let stored = fx.creds();
        assert_eq!(stored.access_token, "new-access");
        assert_eq!(stored.refresh_token.as_deref(), Some("new-refresh"));
        let exp = stored.expires_at.unwrap();
        assert!(exp >= before + time::Duration::seconds(3600));
        assert!(exp <= OffsetDateTime::now_utc() + time::Duration::seconds(3600));
        // min(old = +90 d, new = +30 d) = +30 d.
        let rexp = stored.refresh_expires_at.unwrap();
        assert!(rexp < before + time::Duration::days(31));
        assert_eq!(
            stored.scopes,
            ["user:inference", "user:profile", "user:file_upload"]
        );
        assert_eq!(stored.plan.as_deref(), Some("max"));

        // Now fresh: no second call.
        assert_eq!(p.access_token(&fx.meta, false).await.unwrap(), "new-access");
        assert_eq!(fake.hits.load(Ordering::SeqCst), 1);
        assert_eq!(fx.status(), AccountStatus::Ready);
    }

    #[tokio::test]
    async fn keeps_old_refresh_token_when_response_omits_it() {
        let fx = fx(creds_expiring_in(10));
        let fake = fake_token_endpoint(|_| reply(200, &ok_body("new-access", None, 3600))).await;
        let p = fx.provider(&fake.url);
        assert_eq!(p.access_token(&fx.meta, false).await.unwrap(), "new-access");
        assert_eq!(fx.creds().refresh_token.as_deref(), Some(OLD_RT));
    }

    #[tokio::test]
    async fn force_refreshes_a_fresh_token_and_scope_falls_back_to_config() {
        let mut c = creds_expiring_in(3600);
        c.scopes.clear();
        let fx = fx(c);
        let fake = fake_token_endpoint(|_| reply(200, &ok_body("forced", Some("rt2"), 3600))).await;
        let p = fx.provider(&fake.url);
        assert_eq!(p.access_token(&fx.meta, true).await.unwrap(), "forced");
        assert_eq!(
            fake.bodies.lock().unwrap()[0]["scope"],
            AnthropicCfg::default().scope
        );
    }

    #[tokio::test]
    async fn concurrent_callers_share_one_refresh() {
        let fx = fx(creds_expiring_in(10));
        let fake = fake_token_endpoint(|_| Reply {
            delay: Duration::from_millis(150),
            ..reply(200, &ok_body("shared", Some("rt2"), 3600))
        })
        .await;
        let p = Arc::new(fx.provider(&fake.url));
        let tasks: Vec<_> = (0..8)
            .map(|i| {
                let (p, meta) = (p.clone(), fx.meta.clone());
                // A mix of forced and unforced callers: a forced caller that waited behind
                // another's refresh must adopt it rather than spend the new token again.
                tokio::spawn(async move { p.access_token(&meta, i % 2 == 0).await })
            })
            .collect();
        for t in tasks {
            assert_eq!(t.await.unwrap().unwrap(), "shared");
        }
        assert_eq!(fake.hits.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn invalid_grant_both_shapes_marks_dead_with_fingerprint() {
        for body in [
            r#"{"error":"invalid_grant","error_description":"Refresh token not found or invalid"}"#,
            r#"{"error":{"type":"invalid_grant","message":"Refresh token expired"}}"#,
        ] {
            let fx = fx(creds_expiring_in(10));
            let fake = fake_token_endpoint(move |_| reply(400, body)).await;
            let p = fx.provider(&fake.url);
            let err = p.access_token(&fx.meta, false).await.unwrap_err();
            assert_eq!(err, RefreshErrorKind::InvalidGrant, "{body}");

            let st = fx.state.state(&fx.meta.id, OffsetDateTime::now_utc());
            assert_eq!(st.dead_refresh_fp, Some(fingerprint(OLD_RT)));
            assert_eq!(fx.status(), AccountStatus::NeedsLogin);
            let last = st.last_error.clone().unwrap();
            assert_eq!(
                last.reason,
                FailReason::Refresh(RefreshErrorKind::InvalidGrant)
            );
            assert_eq!(last.upstream_request_id.as_deref(), Some("req_test123"));
            // Credentials untouched.
            assert_eq!(fx.creds().refresh_token.as_deref(), Some(OLD_RT));
            assert!(!format!("{st:?}").contains(OLD_RT));
        }
    }

    #[tokio::test]
    async fn account_on_hold_marks_billing() {
        let fx = fx(creds_expiring_in(10));
        let fake = fake_token_endpoint(|_| {
            reply(
                403,
                r#"{"error":"invalid_grant","error_description":"account_on_hold","error_uri":"https://claude.ai/restricted"}"#,
            )
        })
        .await;
        let p = fx.provider(&fake.url);
        let err = p.access_token(&fx.meta, false).await.unwrap_err();
        assert_eq!(err, RefreshErrorKind::AccountOnHold);
        let st = fx.state.state(&fx.meta.id, OffsetDateTime::now_utc());
        assert!(st.billing_blocked_at.is_some());
        assert_eq!(st.dead_refresh_fp, None, "on hold is not a dead token");
        assert_eq!(fx.status(), AccountStatus::Billing);
    }

    #[tokio::test]
    async fn network_error_is_classified_not_dead() {
        let fx = fx(creds_expiring_in(10));
        // A port nobody listens on.
        let dead_url = {
            let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
            format!("http://{}/v1/oauth/token", l.local_addr().unwrap())
        };
        let p = fx.provider(&dead_url);
        let err = p.access_token(&fx.meta, false).await.unwrap_err();
        assert_eq!(
            err,
            RefreshErrorKind::Network(crate::errors::TransportKind::Connect)
        );
        let st = fx.state.state(&fx.meta.id, OffsetDateTime::now_utc());
        assert_eq!(st.dead_refresh_fp, None);
        assert_eq!(st.last_error.unwrap().reason, FailReason::Refresh(err));
        assert_eq!(fx.status(), AccountStatus::Ready);
        assert_eq!(fx.creds().access_token, OLD_AT);
    }

    #[tokio::test]
    async fn timeout_is_a_network_error_and_next_attempt_rereads_creds() {
        let fx = fx(creds_expiring_in(10));
        let fake = fake_token_endpoint(|_| Reply {
            delay: Duration::from_secs(5),
            ..reply(200, &ok_body("late", None, 3600))
        })
        .await;
        let p = fx
            .provider(&fake.url)
            .with_refresh_timing(Duration::from_secs(300), Duration::from_millis(150));
        let err = p.access_token(&fx.meta, false).await.unwrap_err();
        assert!(matches!(err, RefreshErrorKind::Network(_)), "{err:?}");
        assert_eq!(fx.status(), AccountStatus::Ready);

        // Another process refreshed meanwhile: the next attempt must use its result.
        let mut c = fx.creds();
        c.access_token = "from-other-process".into();
        c.expires_at = Some(OffsetDateTime::now_utc() + time::Duration::hours(1));
        fx.reg.write_creds(&fx.meta.id, &c).unwrap();
        assert_eq!(
            p.access_token(&fx.meta, false).await.unwrap(),
            "from-other-process"
        );
        assert_eq!(fake.hits.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn rereads_creds_after_lock_when_other_process_refreshed() {
        let fx = fx(creds_expiring_in(10));
        let fake =
            fake_token_endpoint(|_| reply(200, &ok_body("must-not-happen", None, 3600))).await;
        let p = Arc::new(fx.provider(&fake.url));

        // Another "process" holds the account flock while it refreshes.
        let guard = fx.reg.lock(&fx.meta.id).unwrap();
        let task = {
            let (p, meta) = (p.clone(), fx.meta.clone());
            tokio::spawn(async move { p.access_token(&meta, false).await })
        };
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(!task.is_finished(), "must wait for the account lock");

        let mut c = fx.creds();
        c.access_token = "refreshed-elsewhere".into();
        c.refresh_token = Some("rotated-elsewhere".into());
        c.expires_at = Some(OffsetDateTime::now_utc() + time::Duration::hours(1));
        fx.reg.write_creds_unlocked(&fx.meta.id, &c).unwrap();
        drop(guard);

        assert_eq!(task.await.unwrap().unwrap(), "refreshed-elsewhere");
        assert_eq!(fake.hits.load(Ordering::SeqCst), 0, "old token never spent");
    }

    #[tokio::test]
    async fn past_expiry_clock_without_invalid_grant_keeps_account_ready() {
        let mut c = creds_expiring_in(3600);
        c.refresh_expires_at = Some(datetime!(2020-01-01 0:00 UTC)); // long past
        let fx = fx(c);
        let fake =
            fake_token_endpoint(|_| reply(200, &ok_body("still-works", Some("rt2"), 3600))).await;
        let p = fx.provider(&fake.url);

        // Fresh access token: used as is, no sidelining, no call.
        assert_eq!(p.access_token(&fx.meta, false).await.unwrap(), OLD_AT);
        assert_eq!(fx.status(), AccountStatus::Ready);
        assert_eq!(fake.hits.load(Ordering::SeqCst), 0);

        // Forced refresh still tries, succeeds, and the account stays ready.
        assert_eq!(p.access_token(&fx.meta, true).await.unwrap(), "still-works");
        assert_eq!(fx.status(), AccountStatus::Ready);
        // The past refresh expiry is preserved (min rule), never acted on.
        assert_eq!(
            fx.creds().refresh_expires_at,
            Some(datetime!(2020-01-01 0:00 UTC))
        );
    }

    #[tokio::test]
    async fn no_refresh_token_returns_stale_unless_forced() {
        let mut c = creds_expiring_in(-100);
        c.refresh_token = None;
        let fx = fx(c);
        let fake = fake_token_endpoint(|_| reply(500, "{}")).await;
        let p = fx.provider(&fake.url);
        assert_eq!(p.access_token(&fx.meta, false).await.unwrap(), OLD_AT);
        assert_eq!(
            p.access_token(&fx.meta, true).await.unwrap_err(),
            RefreshErrorKind::Other
        );
        assert_eq!(fake.hits.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn server_error_and_garbage_are_classified_not_dead() {
        for (status, body, want) in [
            (503, "upstream connect error", RefreshErrorKind::Server(503)),
            (
                400,
                r#"{"error":"invalid_scope"}"#,
                RefreshErrorKind::Server(400),
            ),
            (200, "<html>not json</html>", RefreshErrorKind::Other),
            (200, r#"{"expires_in":10}"#, RefreshErrorKind::Other),
        ] {
            let fx = fx(creds_expiring_in(10));
            let fake = fake_token_endpoint(move |_| reply(status, body)).await;
            let p = fx.provider(&fake.url);
            assert_eq!(p.access_token(&fx.meta, false).await.unwrap_err(), want);
            assert_eq!(fx.status(), AccountStatus::Ready, "{status} {body}");
            assert_eq!(fx.creds().access_token, OLD_AT);
        }
    }

    #[tokio::test]
    async fn successful_refresh_clears_a_stale_dead_marker() {
        let fx = fx(creds_expiring_in(10));
        fx.state
            .mark_refresh_dead(&fx.meta.id, OLD_RT, OffsetDateTime::now_utc());
        assert_eq!(fx.status(), AccountStatus::NeedsLogin);
        let fake = fake_token_endpoint(|_| reply(200, &ok_body("alive", Some("rt2"), 3600))).await;
        let p = fx.provider(&fake.url);
        assert_eq!(p.access_token(&fx.meta, false).await.unwrap(), "alive");
        assert_eq!(fx.status(), AccountStatus::Ready);
    }

    #[test]
    fn expiry_after_refresh_is_min() {
        let now = datetime!(2026-11-07 12:00 UTC);
        let in_days = |d: i64| now + time::Duration::days(d);
        assert_eq!(
            expiry_after_refresh(Some(in_days(90)), Some(30 * 86_400), now),
            Some(in_days(30))
        );
        assert_eq!(
            expiry_after_refresh(Some(in_days(10)), Some(30 * 86_400), now),
            Some(in_days(10))
        );
        assert_eq!(
            expiry_after_refresh(Some(in_days(10)), None, now),
            Some(in_days(10))
        );
        assert_eq!(
            expiry_after_refresh(None, Some(86_400), now),
            Some(in_days(1))
        );
        assert_eq!(expiry_after_refresh(None, None, now), None);
        // Absurd server values cannot overflow date arithmetic.
        assert!(expiry_after_refresh(None, Some(i64::MAX), now).is_some());
    }
}
