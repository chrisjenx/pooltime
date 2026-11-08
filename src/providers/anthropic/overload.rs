//! Same-account backoff for transient upstream overload (529/500/503, INV §1).
//!
//! Full jitter over an exponential ceiling: delay for retry `n` (0-based) is
//! `rand[0,1) * min(cap, base * 2^n)`. A `retry-after` the upstream sent wins, capped at `cap`
//! so a huge value cannot stall the request. Sleeping and the jitter source are injectable so
//! tests are fast and deterministic.

use std::sync::Arc;
use std::time::Duration;

use futures_util::future::BoxFuture;
use http::HeaderMap;
use rand::RngExt;
use time::OffsetDateTime;

/// How the overload backoff sleeps. The default is [`tokio::time::sleep`].
pub trait Sleeper: Send + Sync {
    fn sleep(&self, d: Duration) -> BoxFuture<'static, ()>;
}

/// [`Sleeper`] backed by the tokio timer.
pub struct TokioSleeper;

impl Sleeper for TokioSleeper {
    fn sleep(&self, d: Duration) -> BoxFuture<'static, ()> {
        Box::pin(tokio::time::sleep(d))
    }
}

/// Overload retry policy (`timeouts.overload_retry_{max,base,cap}`).
#[derive(Clone)]
pub struct OverloadPolicy {
    /// Retries after the first attempt (4 => at most 5 upstream calls).
    pub max_retries: u32,
    /// Ceiling of the first retry's delay; doubles per retry.
    pub base: Duration,
    /// Hard cap on any single sleep (also caps `retry-after`).
    pub cap: Duration,
    pub sleeper: Arc<dyn Sleeper>,
    /// Uniform sample in `[0, 1]` for full jitter.
    pub jitter: Arc<dyn Fn() -> f64 + Send + Sync>,
}

impl Default for OverloadPolicy {
    /// 4 retries, 500 ms base, 8 s cap (the `Timeouts` defaults), tokio sleep, random jitter.
    fn default() -> Self {
        OverloadPolicy {
            max_retries: 4,
            base: Duration::from_millis(500),
            cap: Duration::from_secs(8),
            sleeper: Arc::new(TokioSleeper),
            jitter: Arc::new(|| rand::rng().random_range(0.0..1.0)),
        }
    }
}

impl OverloadPolicy {
    /// Delay before retry number `attempt` (0-based). `retry_after` (already relative) wins when
    /// positive, capped at `cap`; otherwise full jitter over `min(cap, base * 2^attempt)`.
    pub fn delay(&self, attempt: u32, retry_after: Option<Duration>) -> Duration {
        if let Some(ra) = retry_after.filter(|d| !d.is_zero()) {
            return ra.min(self.cap);
        }
        let ceiling = self
            .base
            .checked_mul(2u32.saturating_pow(attempt))
            .unwrap_or(self.cap)
            .min(self.cap);
        ceiling.mul_f64((self.jitter)().clamp(0.0, 1.0))
    }
}

/// `retry-after` as an absolute time: integer seconds (relative to `now`) or an HTTP-date.
pub fn retry_after_at(h: &HeaderMap, now: OffsetDateTime) -> Option<OffsetDateTime> {
    let raw = h.get(http::header::RETRY_AFTER)?.to_str().ok()?.trim();
    if let Ok(secs) = raw.parse::<i64>() {
        return now.checked_add(time::Duration::seconds(secs));
    }
    OffsetDateTime::parse(raw, &time::format_description::well_known::Rfc2822).ok()
}

/// Explicit reset time the response states: `retry-after`, else the first parseable
/// `anthropic-ratelimit-{requests,tokens,input-tokens,output-tokens}-reset` (RFC 3339), as bro's
/// `resetAtFromHeaders`.
pub fn explicit_reset(h: &HeaderMap, now: OffsetDateTime) -> Option<OffsetDateTime> {
    retry_after_at(h, now).or_else(|| {
        [
            "anthropic-ratelimit-requests-reset",
            "anthropic-ratelimit-tokens-reset",
            "anthropic-ratelimit-input-tokens-reset",
            "anthropic-ratelimit-output-tokens-reset",
        ]
        .iter()
        .find_map(|name| {
            let v = h.get(*name)?.to_str().ok()?;
            OffsetDateTime::parse(v.trim(), &time::format_description::well_known::Rfc3339).ok()
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::datetime;

    fn policy(j: f64) -> OverloadPolicy {
        OverloadPolicy {
            jitter: Arc::new(move || j),
            ..OverloadPolicy::default()
        }
    }

    #[test]
    fn full_jitter_doubles_and_caps() {
        let p = policy(1.0);
        let got: Vec<u128> = (0..7).map(|n| p.delay(n, None).as_millis()).collect();
        assert_eq!(got, vec![500, 1000, 2000, 4000, 8000, 8000, 8000]);
        assert_eq!(policy(0.5).delay(2, None), Duration::from_secs(1));
        assert_eq!(policy(0.0).delay(3, None), Duration::ZERO);
        // Huge attempt numbers saturate instead of overflowing.
        assert_eq!(p.delay(u32::MAX, None), Duration::from_secs(8));
    }

    #[test]
    fn retry_after_wins_but_is_capped() {
        let p = policy(0.0);
        assert_eq!(
            p.delay(0, Some(Duration::from_secs(3))),
            Duration::from_secs(3)
        );
        assert_eq!(
            p.delay(0, Some(Duration::from_secs(120))),
            Duration::from_secs(8)
        );
        // A zero retry-after falls back to jitter.
        assert_eq!(p.delay(0, Some(Duration::ZERO)), Duration::ZERO);
    }

    #[test]
    fn retry_after_forms() {
        let now = datetime!(2026-11-07 12:00 UTC);
        let mut h = HeaderMap::new();
        assert_eq!(retry_after_at(&h, now), None);
        h.insert("retry-after", "30".parse().unwrap());
        assert_eq!(
            retry_after_at(&h, now),
            Some(now + time::Duration::seconds(30))
        );
        h.insert(
            "retry-after",
            "Sat, 07 Nov 2026 12:05:00 GMT".parse().unwrap(),
        );
        assert_eq!(
            retry_after_at(&h, now),
            Some(datetime!(2026-11-07 12:05 UTC))
        );
        h.insert("retry-after", "soon".parse().unwrap());
        assert_eq!(retry_after_at(&h, now), None);

        let mut h = HeaderMap::new();
        h.insert(
            "anthropic-ratelimit-tokens-reset",
            "2026-11-07T13:00:00Z".parse().unwrap(),
        );
        assert_eq!(
            explicit_reset(&h, now),
            Some(datetime!(2026-11-07 13:00 UTC))
        );
        h.insert("retry-after", "60".parse().unwrap());
        assert_eq!(
            explicit_reset(&h, now),
            Some(now + time::Duration::seconds(60))
        );
    }
}
