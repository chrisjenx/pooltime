//! A scripted in-process `Provider` (stands in for the Codex provider in routing tests).

use std::convert::Infallible;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use axum::body::Body;
use axum::response::Response;
use bytes::Bytes;
use pooltime::accounts::{AccountId, AccountMeta, RateLimitSnapshot, StateBook};
use pooltime::errors::{FailReason, RefreshErrorKind};
use pooltime::models::RouteDef;
use pooltime::providers::{
    AttemptCtx, AttemptOutcome, InboundRequest, Provider, ProviderId, UsageError,
};

type Script = Box<dyn Fn(&AccountMeta, usize) -> AttemptOutcome + Send + Sync>;

pub struct FakeProvider {
    id: ProviderId,
    calls: Mutex<Vec<AccountId>>,
    script: Script,
}

impl FakeProvider {
    /// `script(account, call_index)` produces each attempt's outcome.
    pub fn new(
        id: ProviderId,
        script: impl Fn(&AccountMeta, usize) -> AttemptOutcome + Send + Sync + 'static,
    ) -> Arc<FakeProvider> {
        Arc::new(FakeProvider {
            id,
            calls: Mutex::new(Vec::new()),
            script: Box::new(script),
        })
    }

    /// Accounts attempted, in order.
    pub fn calls(&self) -> Vec<AccountId> {
        self.calls.lock().unwrap().clone()
    }
}

#[async_trait]
impl Provider for FakeProvider {
    fn id(&self) -> ProviderId {
        self.id
    }

    async fn access_token(
        &self,
        _acct: &AccountMeta,
        _force: bool,
    ) -> Result<String, RefreshErrorKind> {
        Ok("fake-provider-token".into())
    }

    async fn attempt(
        &self,
        mut ctx: AttemptCtx<'_>,
        _req: &InboundRequest,
        _state: &StateBook,
    ) -> AttemptOutcome {
        drop(ctx.reservation.take());
        let n = {
            let mut calls = self.calls.lock().unwrap();
            calls.push(ctx.account.id.clone());
            calls.len() - 1
        };
        (self.script)(ctx.account, n)
    }

    async fn poll_usage(&self, _acct: &AccountMeta) -> Result<RateLimitSnapshot, UsageError> {
        Err(UsageError::NotSupported)
    }

    async fn catalog(&self, _acct: &AccountMeta) -> anyhow::Result<Vec<RouteDef>> {
        Ok(vec![])
    }
}

/// `200` JSON response.
pub fn ok_json(body: serde_json::Value) -> Response {
    let mut r = Response::new(Body::from(body.to_string()));
    r.headers_mut().insert(
        http::header::CONTENT_TYPE,
        http::HeaderValue::from_static("application/json"),
    );
    r
}

pub fn done(body: serde_json::Value) -> AttemptOutcome {
    AttemptOutcome::Done(ok_json(body), None)
}

/// Sets its flag when dropped.
pub struct DropFlag(pub Arc<AtomicBool>);

impl Drop for DropFlag {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

/// A streamed 429 error body that sets `dropped` once the body is dropped.
pub fn quota_retry_watched(dropped: Arc<AtomicBool>) -> AttemptOutcome {
    let guard = DropFlag(dropped);
    let body = futures_util::stream::once(async move {
        let _g = guard;
        Ok::<_, Infallible>(Bytes::from_static(
            br#"{"type":"error","error":{"type":"rate_limit_error","message":"codex quota"}}"#,
        ))
    });
    let mut r = Response::new(Body::from_stream(body));
    *r.status_mut() = http::StatusCode::TOO_MANY_REQUESTS;
    AttemptOutcome::Retry {
        reason: FailReason::Quota { reset: None },
        fallback_response: r,
    }
}
