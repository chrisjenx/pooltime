//! Ground-truth usage poll (`GET {origin}/api/oauth/usage`) and the periodic sweeper (INV §3,
//! §10.2).
//!
//! The endpoint reports the same data as Claude Code's `/usage`. It needs a Claude Code `User-Agent`
//! and `anthropic-beta: oauth-2025-04-20` (anything else lands in a throttled bucket), and it is
//! *not* under `/v1`. Body utilization is 0-100 and is divided down to the pool's 0-1 fraction.
//!
//! # Gating
//!
//! [`AnthropicProvider::poll_usage`] holds a per-account async lock for the whole poll (token
//! refresh included) and, once it has the lock, skips the fetch when the account's last *successful*
//! check (`usage_check.ok`) is younger than `usage_ttl` and a snapshot exists: it then returns that
//! snapshot. So racing callers share one fetch, and the sweeper costs at most one request per
//! account per TTL. Failed checks are not gated (the sweeper retries them each interval).
//!
//! Every failure is classified ([`UsageError::label`]), recorded as the account's `usage_check`
//! error and logged with `tracing::warn!`; nothing from the upstream body is kept (INV §10.2).

use std::sync::Arc;
use std::time::Duration;

use futures_util::future::join_all;
use http::{HeaderValue, Method};
use serde_json::Value;
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;
use tokio::task::JoinHandle;
use tokio::time::{Instant, MissedTickBehavior};

use super::AnthropicProvider;
use crate::accounts::status::window_duration;
use crate::accounts::{AccountMeta, AccountStatus, RateLimitSnapshot, Window, WindowStatus};
// Families with a top-level `seven_day_<family>` object / scoped limits.
use crate::models::{family_of as model_family_of, MODEL_FAMILIES};
use crate::providers::{is_entitlement_message, ProviderId, UsageError};
use crate::upstream::Limits;

/// Sweep cadence in production (INV §3): also once at startup.
pub const SWEEP_INTERVAL: Duration = Duration::from_secs(60);
/// Lower bound for the fetch timeout (config floor).
const MIN_FETCH_TIMEOUT: Duration = Duration::from_millis(250);

fn parse_reset(raw: Option<&str>) -> Option<OffsetDateTime> {
    OffsetDateTime::parse(raw?, &Rfc3339).ok()
}

/// Windows from a parsed `/api/oauth/usage` body, or `None` when nothing usable is present.
///
/// Prefers `limits[]` (carries model-scoped windows), then backfills any account-wide or
/// family-scoped window it did not yield from the top-level `five_hour` / `seven_day` /
/// `seven_day_<family>` objects, so a partial `limits[]` never drops `5h`/`7d` (poll results
/// *replace* duration-keyed windows). Percent/utilization is 0-100 -> clamped 0-1 fraction. A
/// spent window without a usable reset is bounded by `now + window duration`.
fn map_usage_response(json: &Value, now: OffsetDateTime) -> Option<RateLimitSnapshot> {
    let mut windows: Vec<Window> = Vec::new();

    let mut push = |key: String,
                    model: Option<String>,
                    percent: Option<f64>,
                    resets_at: Option<&str>,
                    fallback_resets_at: Option<&str>| {
        let Some(percent) = percent else { return };
        if windows.iter().any(|w| w.key == key) {
            return;
        }
        let utilization = (percent / 100.0).clamp(0.0, 1.0);
        let mut reset = parse_reset(resets_at).or_else(|| parse_reset(fallback_resets_at));
        if reset.is_none() && utilization >= 1.0 {
            reset = window_duration(&key).map(|d| now + d);
        }
        windows.push(Window {
            reset_synthetic: false,
            key,
            model,
            status: if utilization >= 1.0 {
                WindowStatus::Rejected
            } else {
                WindowStatus::Allowed
            },
            utilization,
            reset,
        });
    };

    let str_at = |v: &'_ Value, k: &str| v.get(k).and_then(Value::as_str).map(str::to_owned);
    let top_resets = |k: &str| json.get(k).and_then(|o| str_at(o, "resets_at"));

    if let Some(limits) = json.get("limits").and_then(Value::as_array) {
        for lim in limits {
            let percent = lim.get("percent").and_then(Value::as_f64);
            let resets_at = str_at(lim, "resets_at");
            match lim.get("kind").and_then(Value::as_str) {
                Some("session") => push(
                    "5h".into(),
                    None,
                    percent,
                    resets_at.as_deref(),
                    top_resets("five_hour").as_deref(),
                ),
                Some("weekly_all") => push(
                    "7d".into(),
                    None,
                    percent,
                    resets_at.as_deref(),
                    top_resets("seven_day").as_deref(),
                ),
                Some("weekly_scoped") => {
                    let name = lim
                        .pointer("/scope/model/display_name")
                        .and_then(Value::as_str);
                    let Some(family) = name.and_then(model_family_of) else {
                        tracing::warn!("usage: skipping scoped limit with unrecognized model");
                        continue;
                    };
                    push(
                        format!("7d-{family}"),
                        Some(family.to_string()),
                        percent,
                        resets_at.as_deref(),
                        top_resets(&format!("seven_day_{family}")).as_deref(),
                    );
                }
                _ => {}
            }
        }
    }

    let mut top = |obj_key: &str, key: &str, model: Option<&str>| {
        let Some(obj) = json.get(obj_key).filter(|o| o.is_object()) else {
            return;
        };
        push(
            key.to_string(),
            model.map(str::to_owned),
            obj.get("utilization").and_then(Value::as_f64),
            str_at(obj, "resets_at").as_deref(),
            None,
        );
    };
    top("five_hour", "5h", None);
    top("seven_day", "7d", None);
    for f in MODEL_FAMILIES {
        top(&format!("seven_day_{f}"), &format!("7d-{f}"), Some(f));
    }

    if windows.is_empty() {
        return None;
    }
    windows.sort_by(|a, b| {
        a.model
            .is_some()
            .cmp(&b.model.is_some())
            .then_with(|| a.key.cmp(&b.key))
    });
    let unified = if windows.iter().any(|w| w.status == WindowStatus::Rejected) {
        "rejected"
    } else {
        "allowed"
    };
    Some(RateLimitSnapshot {
        unified_status: Some(unified.into()),
        windows,
        updated_at: now,
    })
}

/// `{origin of api_base}/api/oauth/usage` (never under `/v1`, whatever path the base carries).
fn usage_url(api_base: &str) -> Option<reqwest::Url> {
    reqwest::Url::parse(api_base)
        .ok()?
        .join("/api/oauth/usage")
        .ok()
}

/// A failed poll: the classification plus the upstream `request-id`, if any.
struct PollFailure {
    err: UsageError,
    request_id: Option<String>,
}

impl From<UsageError> for PollFailure {
    fn from(err: UsageError) -> Self {
        PollFailure {
            err,
            request_id: None,
        }
    }
}

impl AnthropicProvider {
    /// Ground-truth usage for `acct`, applied to the [`crate::accounts::StateBook`] (poll result
    /// replaces duration-keyed windows; an entitlement 403 marks the account billing-blocked).
    ///
    /// See the module docs for the lock and TTL gate. Never returns upstream text: failures are
    /// [`UsageError`]s, also logged and recorded on the account's `usage_check`.
    pub async fn poll_usage(&self, acct: &AccountMeta) -> Result<RateLimitSnapshot, UsageError> {
        if !self.cfg.usage_refresh {
            return Err(UsageError::NotSupported);
        }
        let id = &acct.id;
        let gate = self.usage_gate(&acct.id);
        let _held = gate.lock().await;

        // Stamped before the fetch: a snapshot that raced a serve-path 403 must not clear the
        // billing block that 403 set while this probe was in flight (StateBook enforces it).
        let started_at = OffsetDateTime::now_utc();
        let st = self.state.state(id, started_at);
        if let (Some(check), Some(snap)) = (st.usage_check, st.snapshot) {
            let ttl = time::Duration::try_from(self.cfg.usage_ttl).unwrap_or(time::Duration::MAX);
            let age = started_at - check.at;
            if check.ok && !age.is_negative() && age < ttl {
                return Ok(snap);
            }
        }

        match self.fetch_usage(acct, started_at).await {
            Ok(snap) => Ok(snap),
            Err(PollFailure { err, request_id }) => {
                let label = err.label();
                tracing::warn!(
                    account = %id,
                    outcome = %label,
                    request_id = request_id.as_deref().unwrap_or("-"),
                    "usage poll failed"
                );
                let now = OffsetDateTime::now_utc();
                if err == UsageError::Entitlement {
                    // Also stamps `usage_check` (ok), which throttles the next recheck to the TTL.
                    self.state.mark_billing(id, now);
                } else {
                    self.state.record_usage_check_error(id, now, &label);
                }
                Err(err)
            }
        }
    }

    fn usage_gate(&self, id: &crate::accounts::AccountId) -> Arc<tokio::sync::Mutex<()>> {
        let mut m = self.usage_gates.lock().unwrap_or_else(|e| e.into_inner());
        m.entry(id.clone()).or_default().clone()
    }

    async fn fetch_usage(
        &self,
        acct: &AccountMeta,
        started_at: OffsetDateTime,
    ) -> Result<RateLimitSnapshot, PollFailure> {
        let id = &acct.id;
        let token = self
            .access_token(acct, false)
            .await
            .map_err(UsageError::Refresh)?;
        let url = usage_url(&self.cfg.api_base).ok_or_else(|| {
            tracing::error!("anthropic api_base is not a valid URL");
            UsageError::Transport(crate::errors::TransportKind::Other)
        })?;
        let bad_header = || UsageError::Transport(crate::errors::TransportKind::Other);

        let mut req = reqwest::Request::new(Method::GET, url);
        let mut auth =
            HeaderValue::from_str(&format!("Bearer {token}")).map_err(|_| bad_header())?;
        auth.set_sensitive(true);
        let ua = HeaderValue::from_str(&self.cfg.user_agent).map_err(|_| bad_header())?;
        let h = req.headers_mut();
        h.insert(http::header::AUTHORIZATION, auth);
        h.insert("anthropic-version", HeaderValue::from_static("2023-06-01"));
        h.insert(
            "anthropic-beta",
            HeaderValue::from_static("oauth-2025-04-20"),
        );
        h.insert(http::header::USER_AGENT, ua);

        let timeout = self.cfg.usage_fetch_timeout.max(MIN_FETCH_TIMEOUT);
        let lim = Limits {
            header_wait: timeout,
            idle: timeout,
            deadline: Instant::now() + timeout,
        };
        let resp = self
            .up
            .send(req, &lim)
            .await
            .map_err(UsageError::Transport)?;
        let status = resp.status.as_u16();
        let request_id = resp
            .headers
            .get("request-id")
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        let fail = |err: UsageError| PollFailure {
            err,
            request_id: request_id.clone(),
        };

        if status == 403 {
            // Only an entitlement 403 blocks the account. Any other 403 here (a scope or beta
            // header change on this endpoint, a proxy refusing it) says nothing about
            // /v1/messages, and treating it as a block would bench every account with no way
            // back: the only healer is this same endpoint answering.
            let body = resp
                .body
                .text()
                .await
                .map_err(|k| fail(UsageError::Transport(k)))?;
            let message = serde_json::from_str::<Value>(&body)
                .ok()
                .and_then(|v| v.pointer("/error/message")?.as_str().map(str::to_owned));
            return Err(fail(match message {
                Some(m) if is_entitlement_message(&m) => UsageError::Entitlement,
                _ => UsageError::Status(403),
            }));
        }
        if !(200..300).contains(&status) {
            return Err(fail(UsageError::Status(status)));
        }
        let body = resp
            .body
            .text()
            .await
            .map_err(|k| fail(UsageError::Transport(k)))?;
        let json: Value = serde_json::from_str(&body)
            .map_err(|_| fail(UsageError::Parse("invalid_json".into())))?;
        let now = OffsetDateTime::now_utc();
        let snap = map_usage_response(&json, now)
            .ok_or_else(|| fail(UsageError::Parse("no_windows".into())))?;
        self.state
            .replace_poll_snapshot(id, snap.clone(), started_at);
        Ok(snap)
    }
}

/// Poll every Anthropic account once at startup and then every `every` ([`SWEEP_INTERVAL`] in
/// production), so accounts that fell out of rotation do not freeze at their 429-time snapshot.
/// Accounts in `NeedsLogin` are skipped (nothing to poll with). Failures are recorded and logged
/// by [`AnthropicProvider::poll_usage`]. Returns immediately if `usage_refresh` is off.
pub fn spawn_usage_sweeper(provider: Arc<AnthropicProvider>, every: Duration) -> JoinHandle<()> {
    tokio::spawn(async move {
        if !provider.cfg.usage_refresh {
            return;
        }
        let mut tick = tokio::time::interval(every);
        tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
        loop {
            tick.tick().await; // the first tick is immediate: the startup sweep
            crate::logging::catch_panic("anthropic usage sweep", sweep_once(&provider)).await;
        }
    })
}

async fn sweep_once(provider: &AnthropicProvider) {
    let now = OffsetDateTime::now_utc();
    let accounts: Vec<AccountMeta> = provider
        .reg
        .list()
        .into_iter()
        .filter(|a| a.provider == ProviderId::Anthropic)
        .filter(|a| {
            let creds_ok = matches!(provider.reg.read_creds(&a.id), Ok(Some(_)));
            // An account whose stored state was unreadable (INV §10.9) reads `NeedsLogin` but is
            // still polled: the poll or its token refresh is what proves it healthy again.
            let needs_login = provider.state.status(&a.id, creds_ok, now)
                == AccountStatus::NeedsLogin
                && !(creds_ok && provider.state.is_unverified(&a.id));
            if needs_login {
                tracing::debug!(account = %a.id, "usage sweep: skipping account that needs login");
            }
            !needs_login
        })
        .collect();
    // Failures are logged and recorded inside poll_usage.
    join_all(accounts.iter().map(|a| provider.poll_usage(a))).await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::convert::Infallible;
    use std::io;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;

    use bytes::Bytes;
    use http_body_util::Full;
    use hyper::service::service_fn;
    use hyper::{Request, Response};
    use hyper_util::rt::TokioIo;
    use serde_json::json;
    use time::macros::datetime;
    use tokio::net::TcpListener;

    use crate::accounts::{AccountId, Credentials, Registry, StateBook};
    use crate::config::{AnthropicCfg, Paths};
    use crate::store::Store;
    use crate::upstream::Upstream;

    const USAGE_UA: &str = "claude-code/9.9.9-test";

    // ---- mapper -------------------------------------------------------------

    /// Trimmed from a real /api/oauth/usage capture (bro `usage.test.ts`).
    fn sample() -> Value {
        json!({
            "five_hour": {"utilization": 39.0, "resets_at": "2026-07-13T07:59:59.883087+00:00"},
            "seven_day": {"utilization": 81.0, "resets_at": "2026-07-15T08:59:59.883113+00:00"},
            "seven_day_opus": null,
            "limits": [
                {"kind": "session", "percent": 39, "resets_at": "2026-07-13T07:59:59.883087+00:00", "scope": null},
                {"kind": "weekly_all", "percent": 81, "resets_at": "2026-07-15T08:59:59.883113+00:00", "scope": null},
                {"kind": "weekly_scoped", "percent": 68, "resets_at": "2026-07-15T08:59:59.883475+00:00",
                 "scope": {"model": {"id": null, "display_name": "Fable"}, "surface": null}}
            ]
        })
    }

    fn win<'a>(s: &'a RateLimitSnapshot, key: &str) -> &'a Window {
        s.windows.iter().find(|w| w.key == key).unwrap()
    }

    #[test]
    fn maps_limits_with_fraction_utilization_and_model_scope() {
        let now = datetime!(2026-07-13 00:00 UTC);
        let snap = map_usage_response(&sample(), now).unwrap();
        assert_eq!(snap.updated_at, now);
        assert_eq!(snap.unified_status.as_deref(), Some("allowed"));
        let keys: Vec<&str> = snap.windows.iter().map(|w| w.key.as_str()).collect();
        assert_eq!(keys, vec!["5h", "7d", "7d-fable"]);
        let (w5, w7, wf) = (win(&snap, "5h"), win(&snap, "7d"), win(&snap, "7d-fable"));
        assert_eq!((w5.model.as_deref(), w5.utilization), (None, 0.39));
        assert_eq!(w7.utilization, 0.81);
        assert_eq!((wf.model.as_deref(), wf.utilization), (Some("fable"), 0.68));
        assert_eq!(w5.status, WindowStatus::Allowed);
        assert_eq!(
            w5.reset,
            Some(datetime!(2026-07-13 07:59:59.883087 UTC)),
            "RFC 3339 with offset and microseconds"
        );
    }

    #[test]
    fn spent_window_without_reset_synthesized() {
        let now = datetime!(2026-07-13 00:00 UTC);
        let snap = map_usage_response(
            &json!({"limits": [{"kind": "weekly_all", "percent": 100, "scope": null}]}),
            now,
        )
        .unwrap();
        let w = win(&snap, "7d");
        assert_eq!(w.status, WindowStatus::Rejected);
        assert_eq!(w.utilization, 1.0);
        assert_eq!(w.reset, Some(now + time::Duration::days(7)));
        assert_eq!(snap.unified_status.as_deref(), Some("rejected"));
        // Over 100% clamps; a window that is not spent gets no synthesized reset.
        let snap = map_usage_response(
            &json!({"five_hour": {"utilization": 130}, "seven_day": {"utilization": 5}}),
            now,
        )
        .unwrap();
        assert_eq!(win(&snap, "5h").utilization, 1.0);
        assert_eq!(win(&snap, "5h").reset, Some(now + time::Duration::hours(5)));
        assert_eq!(win(&snap, "7d").reset, None);
    }

    #[test]
    fn partial_limits_backfilled() {
        let now = datetime!(2026-07-13 00:00 UTC);
        // session present but its percent is null; weekly_all fills 7d; the top-level five_hour
        // still carries the real 5h data and the scoped family backfills from its own object.
        let snap = map_usage_response(
            &json!({
                "five_hour": {"utilization": 95, "resets_at": "2026-07-13T07:00:00Z"},
                "seven_day_sonnet": {"utilization": 12, "resets_at": "2026-07-15T00:00:00Z"},
                "limits": [
                    {"kind": "session", "percent": null, "resets_at": "2026-07-13T07:00:00Z", "scope": null},
                    {"kind": "weekly_all", "percent": 40, "resets_at": "2026-07-15T00:00:00Z", "scope": null}
                ]
            }),
            now,
        )
        .unwrap();
        assert_eq!(win(&snap, "5h").utilization, 0.95);
        assert_eq!(win(&snap, "7d").utilization, 0.4);
        let s = win(&snap, "7d-sonnet");
        assert_eq!((s.model.as_deref(), s.utilization), (Some("sonnet"), 0.12));
        // limits[] wins over the top-level duplicate (no double window).
        let keys: Vec<&str> = snap.windows.iter().map(|w| w.key.as_str()).collect();
        assert_eq!(keys, vec!["5h", "7d", "7d-sonnet"]);
    }

    #[test]
    fn limit_resets_fall_back_to_top_level_and_unknown_models_are_skipped() {
        let now = datetime!(2026-07-13 00:00 UTC);
        let snap = map_usage_response(
            &json!({
                "five_hour": {"utilization": 5, "resets_at": "2026-07-13T07:00:00Z"},
                "limits": [
                    {"kind": "session", "percent": 5, "scope": null},
                    {"kind": "weekly_scoped", "percent": 10, "resets_at": "2026-07-15T00:00:00Z",
                     "scope": {"model": {"display_name": "Nonesuch 9"}}}
                ]
            }),
            now,
        )
        .unwrap();
        assert_eq!(snap.windows.len(), 1);
        assert_eq!(
            win(&snap, "5h").reset,
            Some(datetime!(2026-07-13 07:00 UTC))
        );
        for junk in [json!(null), json!({}), json!([1]), json!({"limits": "x"})] {
            assert!(map_usage_response(&junk, now).is_none(), "{junk}");
        }
    }

    // ---- fake usage server + fixture -------------------------------------------

    struct Reply {
        status: u16,
        body: String,
        delay: Duration,
    }

    fn reply(status: u16, body: impl Into<String>) -> Reply {
        Reply {
            status,
            body: body.into(),
            delay: Duration::ZERO,
        }
    }

    fn ok_usage(percent: u32) -> Reply {
        reply(
            200,
            json!({"limits": [{"kind": "session", "percent": percent,
                               "resets_at": "2099-07-13T07:00:00Z", "scope": null}]})
            .to_string(),
        )
    }

    #[derive(Default)]
    struct Seen {
        /// Hits per bearer token.
        by_token: HashMap<String, usize>,
        /// Headers of the most recent request, as (path, name -> value).
        last: Option<(String, HashMap<String, String>)>,
    }

    struct Fake {
        /// Origin only (no path).
        base: String,
        hits: Arc<AtomicUsize>,
        seen: Arc<Mutex<Seen>>,
    }

    impl Fake {
        fn hits_for(&self, token: &str) -> usize {
            *self.seen.lock().unwrap().by_token.get(token).unwrap_or(&0)
        }
    }

    /// Usage endpoint on 127.0.0.1:0; `script(hit_index)` decides each reply.
    async fn fake_usage(script: impl Fn(usize) -> Reply + Send + Sync + 'static) -> Fake {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let hits = Arc::new(AtomicUsize::new(0));
        let seen = Arc::new(Mutex::new(Seen::default()));
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
                            assert_eq!(req.method(), "GET");
                            let headers: HashMap<String, String> = req
                                .headers()
                                .iter()
                                .map(|(k, v)| {
                                    (k.to_string(), v.to_str().unwrap_or_default().to_string())
                                })
                                .collect();
                            let token = headers["authorization"]
                                .strip_prefix("Bearer ")
                                .unwrap_or_default()
                                .to_string();
                            {
                                let mut s = s.lock().unwrap();
                                *s.by_token.entry(token).or_default() += 1;
                                s.last = Some((req.uri().path().to_string(), headers));
                            }
                            let r = script(h.fetch_add(1, Ordering::SeqCst));
                            tokio::time::sleep(r.delay).await;
                            Ok::<_, Infallible>(
                                Response::builder()
                                    .status(r.status)
                                    .header("request-id", "req_usage123")
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
        Fake { base, hits, seen }
    }

    struct Fx {
        _dir: tempfile::TempDir,
        reg: Arc<Registry>,
        state: Arc<StateBook>,
    }

    fn fx() -> Fx {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::under(dir.path());
        paths.ensure().unwrap();
        let reg = Arc::new(Registry::load(&paths).unwrap());
        let store = Arc::new(Store::open(&dir.path().join("pooltime.db")).unwrap());
        let state = Arc::new(StateBook::load(store).unwrap());
        Fx {
            _dir: dir,
            reg,
            state,
        }
    }

    impl Fx {
        /// Add an account whose access token is `token-<name>` (fresh for an hour).
        fn account(&self, name: &str, provider: ProviderId) -> AccountMeta {
            let meta = AccountMeta::new(name.parse().unwrap(), provider);
            self.reg.upsert(meta.clone()).unwrap();
            self.reg
                .write_creds(
                    &meta.id,
                    &Credentials {
                        provider,
                        access_token: format!("token-{name}"),
                        refresh_token: Some(format!("refresh-{name}")),
                        expires_at: Some(OffsetDateTime::now_utc() + time::Duration::hours(1)),
                        refresh_expires_at: None,
                        scopes: vec!["user:inference".into()],
                        plan: Some("max".into()),
                        tier: None,
                        openai_account_id: None,
                    },
                )
                .unwrap();
            meta
        }

        fn provider(&self, base: &str) -> AnthropicProvider {
            let cfg = AnthropicCfg {
                // A base that carries `/v1` still polls under the origin.
                api_base: format!("{base}/v1"),
                user_agent: USAGE_UA.into(),
                usage_fetch_timeout: Duration::from_millis(400),
                ..AnthropicCfg::default()
            };
            AnthropicProvider::new(
                cfg,
                self.reg.clone(),
                self.state.clone(),
                Arc::new(Upstream::new()),
            )
        }

        fn usage_check(&self, id: &AccountId) -> Option<crate::accounts::UsageCheck> {
            self.state.state(id, OffsetDateTime::now_utc()).usage_check
        }
    }

    #[derive(Clone, Default)]
    struct SharedBuf(Arc<Mutex<Vec<u8>>>);

    impl io::Write for SharedBuf {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl SharedBuf {
        fn text(&self) -> String {
            String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned()
        }
    }

    // ---- poll -----------------------------------------------------------------

    #[tokio::test]
    async fn poll_normalizes_percent_to_fraction() {
        let fx = fx();
        let meta = fx.account("work", ProviderId::Anthropic);
        let fake = fake_usage(|_| ok_usage(88)).await;
        let p = fx.provider(&fake.base);

        let before = OffsetDateTime::now_utc();
        let snap = p.poll_usage(&meta).await.unwrap();
        assert_eq!(snap.windows.len(), 1);
        let w = &snap.windows[0];
        assert_eq!((w.key.as_str(), w.model.as_deref()), ("5h", None));
        assert!((w.utilization - 0.88).abs() < 1e-12, "{}", w.utilization);
        assert!(snap.updated_at >= before);

        // Request shape: origin path (not under /v1), required headers, configured UA.
        let (path, h) = fake.seen.lock().unwrap().last.clone().unwrap();
        assert_eq!(path, "/api/oauth/usage");
        assert_eq!(h["authorization"], "Bearer token-work");
        assert_eq!(h["anthropic-version"], "2023-06-01");
        assert_eq!(h["anthropic-beta"], "oauth-2025-04-20");
        assert_eq!(h["user-agent"], USAGE_UA);

        // Applied to the state book, and the check is recorded as ok.
        let st = fx.state.state(&meta.id, OffsetDateTime::now_utc());
        let stored = &st.snapshot.unwrap().windows[0];
        assert!((stored.utilization - 0.88).abs() < 1e-12);
        assert!(fx.usage_check(&meta.id).unwrap().ok);
    }

    #[tokio::test]
    async fn racing_polls_share_one_fetch_within_the_ttl() {
        let fx = fx();
        let meta = fx.account("work", ProviderId::Anthropic);
        let fake = fake_usage(|_| Reply {
            delay: Duration::from_millis(80),
            ..ok_usage(10)
        })
        .await;
        let p = fx.provider(&fake.base);

        let (a, b, c) = tokio::join!(
            p.poll_usage(&meta),
            p.poll_usage(&meta),
            p.poll_usage(&meta)
        );
        let (a, b, c) = (a.unwrap(), b.unwrap(), c.unwrap());
        assert_eq!(
            fake.hits.load(Ordering::SeqCst),
            1,
            "per-account lock + TTL gate"
        );
        assert_eq!(a.windows, b.windows);
        assert_eq!(b.windows, c.windows);
        // Still inside the TTL: still no second request.
        p.poll_usage(&meta).await.unwrap();
        assert_eq!(fake.hits.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn entitlement_403_marks_billing_but_other_403_does_not() {
        let fx = fx();
        let meta = fx.account("work", ProviderId::Anthropic);
        let fake = fake_usage(|n| {
            let msg = if n == 0 {
                "OAuth authentication is currently not allowed for this organization."
            } else {
                "Request blocked by safety filter."
            };
            reply(
                403,
                json!({"type": "error", "error": {"type": "permission_error", "message": msg}})
                    .to_string(),
            )
        })
        .await;
        let p = fx.provider(&fake.base);

        assert_eq!(p.poll_usage(&meta).await, Err(UsageError::Entitlement));
        let now = OffsetDateTime::now_utc();
        assert_eq!(fx.state.status(&meta.id, true, now), AccountStatus::Billing);

        // Second poll: a generic 403 says nothing about entitlement. It is a plain failure and
        // the billing block stays (only a successful poll clears it).
        let other = fx.account("other", ProviderId::Anthropic);
        assert_eq!(p.poll_usage(&other).await, Err(UsageError::Status(403)));
        assert_eq!(
            fx.state.status(&other.id, true, now),
            AccountStatus::Ready,
            "non-entitlement 403 must not bench the account"
        );
        let check = fx.usage_check(&other.id).unwrap();
        assert_eq!(
            (check.ok, check.error.as_deref()),
            (false, Some("status:403"))
        );
    }

    #[tokio::test]
    async fn poll_failure_is_logged_and_classified() {
        crate::test_support::ensure_callsites_stay_enabled();
        let fx = fx();
        let meta = fx.account("work", ProviderId::Anthropic);
        let secret = "UPSTREAM-MESSAGE-SECRET";
        let fake = fake_usage(move |n| match n {
            0 => reply(500, format!(r#"{{"error":{{"message":"{secret}"}}}}"#)),
            1 => reply(429, secret),
            2 => reply(200, format!("<html>{secret}")),
            3 => reply(200, r#"{"unrelated": true}"#),
            _ => Reply {
                delay: Duration::from_secs(5),
                ..ok_usage(1)
            },
        })
        .await;
        let p = fx.provider(&fake.base);

        let buf = SharedBuf::default();
        let sub = {
            let buf = buf.clone();
            tracing_subscriber::fmt()
                .json()
                .with_writer(move || buf.clone())
                .finish()
        };
        // Thread-local default held across awaits: fine on the current-thread test runtime.
        let _guard = tracing::subscriber::set_default(sub);

        let mut got = Vec::new();
        for _ in 0..5 {
            let err = p.poll_usage(&meta).await.unwrap_err();
            let check = fx.usage_check(&meta.id).unwrap();
            assert!(!check.ok);
            // The recorded label is exactly the classified error.
            assert_eq!(check.error.as_deref(), Some(err.label().as_str()));
            got.push(err);
        }
        assert_eq!(got[0], UsageError::Status(500));
        assert_eq!(got[1], UsageError::Status(429));
        assert_eq!(got[2], UsageError::Parse("invalid_json".into()));
        assert_eq!(got[3], UsageError::Parse("no_windows".into()));
        assert!(
            matches!(got[4], UsageError::Transport(_)),
            "slow upstream: {:?}",
            got[4]
        );
        // Failures are not TTL-gated and never touch the snapshot.
        assert_eq!(fake.hits.load(Ordering::SeqCst), 5);
        assert!(fx
            .state
            .state(&meta.id, OffsetDateTime::now_utc())
            .snapshot
            .is_none());

        // Each failure was logged (account + classified outcome + request id), with no upstream
        // text and no token.
        let logs: Vec<serde_json::Value> = buf
            .text()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        let failures: Vec<&serde_json::Value> = logs
            .iter()
            .filter(|l| l["fields"]["message"] == "usage poll failed")
            .collect();
        assert_eq!(failures.len(), 5, "{logs:?}");
        assert!(failures.iter().all(|l| l["level"] == "WARN"));
        assert!(failures.iter().all(|l| l["fields"]["account"] == "work"));
        let outcomes: Vec<&str> = failures
            .iter()
            .map(|l| l["fields"]["outcome"].as_str().unwrap())
            .collect();
        assert_eq!(
            &outcomes[..4],
            [
                "status:500",
                "status:429",
                "parse:invalid_json",
                "parse:no_windows"
            ]
        );
        assert!(outcomes[4].starts_with("transport:"));
        assert_eq!(failures[0]["fields"]["request_id"], "req_usage123");
        let all = buf.text();
        assert!(!all.contains(secret) && !all.contains("token-work"));
        let state_json =
            serde_json::to_string(&fx.state.state(&meta.id, OffsetDateTime::now_utc())).unwrap();
        assert!(!state_json.contains(secret));
    }

    #[tokio::test]
    async fn disabled_usage_refresh_is_not_supported() {
        let fx = fx();
        let meta = fx.account("work", ProviderId::Anthropic);
        let fake = fake_usage(|_| ok_usage(1)).await;
        let mut p = fx.provider(&fake.base);
        p.cfg.usage_refresh = false;
        assert_eq!(p.poll_usage(&meta).await, Err(UsageError::NotSupported));
        assert_eq!(fake.hits.load(Ordering::SeqCst), 0);
    }

    // ---- sweeper ----------------------------------------------------------------

    #[tokio::test]
    async fn sweeper_skips_needs_login() {
        let fx = fx();
        let ready = fx.account("ready", ProviderId::Anthropic);
        let dead = fx.account("dead", ProviderId::Anthropic);
        let no_creds = AccountMeta::new("nocreds".parse().unwrap(), ProviderId::Anthropic);
        fx.reg.upsert(no_creds).unwrap();
        // Another provider's account is never polled by the Anthropic sweeper.
        fx.account("codex", ProviderId::OpenAi);
        fx.state
            .mark_refresh_dead(&dead.id, "refresh-dead", OffsetDateTime::now_utc());
        assert_eq!(
            fx.state.status(&dead.id, true, OffsetDateTime::now_utc()),
            AccountStatus::NeedsLogin
        );

        let fake = fake_usage(|_| ok_usage(20)).await;
        let provider = Arc::new(fx.provider(&fake.base));
        let handle = spawn_usage_sweeper(provider, Duration::from_millis(40));

        // The startup sweep polls the ready account...
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while fake.hits_for("token-ready") == 0 {
            assert!(std::time::Instant::now() < deadline, "no startup sweep");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        // ...and several more sweeps later: the TTL gate keeps it at one hit, and the accounts
        // that need login (dead refresh token, no credentials) and the codex account got none.
        tokio::time::sleep(Duration::from_millis(250)).await;
        handle.abort();
        assert_eq!(fake.hits_for("token-ready"), 1);
        assert_eq!(fake.hits_for("token-dead"), 0);
        assert_eq!(fake.hits_for("token-codex"), 0);
        assert_eq!(
            fake.hits.load(Ordering::SeqCst),
            1,
            "{:?}",
            fake.seen.lock().unwrap().by_token
        );
        assert!(fx.usage_check(&ready.id).unwrap().ok);
        assert!(fx.usage_check(&dead.id).is_none());
    }

    /// INV §10.9 recovery: an account whose persisted state was corrupt reads `needs_login` but
    /// the sweeper still polls it, and the successful poll returns it to `ready`.
    #[tokio::test]
    async fn sweeper_polls_unverified_account_and_recovers_it() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::under(dir.path());
        paths.ensure().unwrap();
        let reg = Arc::new(Registry::load(&paths).unwrap());
        let store = Arc::new(Store::open(&dir.path().join("pooltime.db")).unwrap());
        store
            .exec(|c| {
                c.execute(
                    "INSERT INTO account_state(account, json) VALUES ('flaky', '{corrupt')",
                    [],
                )?;
                Ok(())
            })
            .await
            .unwrap();
        let state = Arc::new(StateBook::load(store).unwrap());
        let fx = Fx {
            _dir: dir,
            reg,
            state,
        };
        let flaky = fx.account("flaky", ProviderId::Anthropic);
        let now = OffsetDateTime::now_utc();
        assert_eq!(
            fx.state.status(&flaky.id, true, now),
            AccountStatus::NeedsLogin
        );

        let fake = fake_usage(|_| ok_usage(20)).await;
        let handle =
            spawn_usage_sweeper(Arc::new(fx.provider(&fake.base)), Duration::from_secs(60));
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while fake.hits_for("token-flaky") == 0 {
            assert!(
                std::time::Instant::now() < deadline,
                "unverified not polled"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        while fx.state.is_unverified(&flaky.id) {
            assert!(std::time::Instant::now() < deadline, "never verified");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        handle.abort();
        assert_eq!(
            fx.state.status(&flaky.id, true, OffsetDateTime::now_utc()),
            AccountStatus::Ready
        );
    }

    #[tokio::test]
    async fn sweeper_records_and_survives_failures() {
        let fx = fx();
        let a = fx.account("a", ProviderId::Anthropic);
        let fake = fake_usage(|n| if n < 2 { reply(503, "x") } else { ok_usage(5) }).await;
        let handle =
            spawn_usage_sweeper(Arc::new(fx.provider(&fake.base)), Duration::from_millis(40));
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        // Two failed sweeps are recorded, then the third succeeds and clears the error.
        while !fx.usage_check(&a.id).is_some_and(|c| c.ok) {
            assert!(
                std::time::Instant::now() < deadline,
                "sweeper never recovered"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        handle.abort();
        assert_eq!(fake.hits.load(Ordering::SeqCst), 3);
    }
}
