//! Provider choice and cross-provider fallback for mapped Claude families (INV §2, §5; port of
//! bro `pickProvider` / `weightedProviderRanking` and `serveWithCrossProviderFallback`).

use std::future::Future;
use std::sync::atomic::{AtomicUsize, Ordering};

use axum::response::Response;
use time::OffsetDateTime;

use crate::accounts::AccountId;
use crate::errors::FailReason;
use crate::providers::anthropic::{RETRYABLE_TRANSPORT_HEADER, UPSTREAM_REJECTED_HEADER};
use crate::providers::ProviderId;
use crate::routing::{choose, score, Candidate, Tuning};

/// Statuses that mean "this provider's pool is exhausted or overloaded" (bro
/// `CROSS_PROVIDER_RETRY_STATUSES`).
pub const CROSS_PROVIDER_RETRY_STATUSES: [u16; 4] = [429, 500, 503, 529];

/// How one provider's serve ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// An attempt committed a response.
    Served,
    /// Terminal outcome, or exhaustion after retries (the last retry's reason).
    Failed(FailReason),
    /// No usable account was available (or the provider is not registered).
    NoAccount,
}

impl Verdict {
    /// Log label: `served`, `no_account`, or the [`FailReason::detail`].
    pub fn label(&self) -> String {
        match self {
            Verdict::Served => "served".into(),
            Verdict::Failed(r) => r.detail(),
            Verdict::NoAccount => "no_account".into(),
        }
    }
}

/// A provider's response plus why it ended that way.
pub struct Served {
    pub response: Response,
    pub verdict: Verdict,
    /// The provider whose response this is (journaled as the request's provider).
    pub provider: ProviderId,
}

impl Served {
    /// Whether retrying the whole request on the other provider is sensible (bro rule): the
    /// response status is 429/500/503/529 (whole-pool exhaustion, overload, or no account) or it
    /// is flagged `x-pool-retryable-transport: 1`. Never for an upstream per-request refusal
    /// (`x-pool-upstream-rejected`). The verdict is for logging only.
    pub fn cross_provider_retryable(&self) -> bool {
        let h = self.response.headers();
        if h.contains_key(UPSTREAM_REJECTED_HEADER) {
            return false;
        }
        CROSS_PROVIDER_RETRY_STATUSES.contains(&self.response.status().as_u16())
            || h.get(RETRYABLE_TRANSPORT_HEADER)
                .is_some_and(|v| v.as_bytes() == b"1")
    }
}

/// The other provider.
pub fn other(p: ProviderId) -> ProviderId {
    match p {
        ProviderId::Anthropic => ProviderId::OpenAi,
        ProviderId::OpenAi => ProviderId::Anthropic,
    }
}

/// Cancel a response nobody will read: dropping the body drops the upstream stream (a pumped
/// stream sees its channel close and releases its reservation). Done synchronously, before the
/// chosen response is returned.
fn discard(s: Served) {
    drop(s.response);
}

/// Serve on `first`; if that fails in a cross-provider-retryable way, retry once on the other
/// provider. When both fail the **first** response is returned (and both reasons are logged);
/// the response that is not returned is discarded.
pub async fn serve_with_cross_provider_fallback<F, Fut>(
    first: ProviderId,
    request_id: &str,
    mut serve: F,
) -> Served
where
    F: FnMut(ProviderId) -> Fut,
    Fut: Future<Output = Served>,
{
    let primary = serve(first).await;
    if !primary.cross_provider_retryable() {
        return primary;
    }
    let second = other(first);
    tracing::info!(
        request_id,
        from = %first,
        to = %second,
        reason = %primary.verdict.label(),
        "cross-provider fallback"
    );
    let retry = serve(second).await;
    if retry.cross_provider_retryable() {
        tracing::warn!(
            request_id,
            first_provider = %first,
            first_reason = %primary.verdict.label(),
            second_provider = %second,
            second_reason = %retry.verdict.label(),
            "both providers failed; returning the first provider's response"
        );
        discard(retry);
        primary
    } else {
        discard(primary);
        retry
    }
}

/// One provider's candidate accounts for [`pick_provider`].
pub struct ProviderPool<'a> {
    pub provider: ProviderId,
    /// Family used for this provider's windows (`None` for Codex: no per-family windows).
    pub family: Option<&'a str>,
    /// This provider's usable accounts (all tiers; the active tier is derived).
    pub cands: Vec<Candidate<'a>>,
}

fn active_tier<'p, 'a>(cands: &'p [Candidate<'a>]) -> Vec<&'p Candidate<'a>> {
    let Some(min) = cands.iter().map(|c| c.meta.priority).min() else {
        return Vec::new();
    };
    cands.iter().filter(|c| c.meta.priority == min).collect()
}

/// Which provider serves a mapped request first (bro `pickProvider`, weighted strategy).
///
/// 1. Pools without a usable account drop out; none -> `None`; one -> it.
/// 2. Sticky: a provider whose live session pin (`pins`) points at an account in its active
///    tier wins; if several do, `most_recent` (the provider that served the session last) wins,
///    else the first in `pools` order.
/// 3. Otherwise every pool's active tier is scored on **one** expiry scale, and each provider is
///    represented by the account its own `choose` would serve; the best of those (viable first,
///    then score) wins. Exact ties keep `pools` order (callers list Anthropic first). The
///    round-robin cursor is read, never advanced.
pub fn pick_provider(
    pools: &[ProviderPool<'_>],
    pins: &[(ProviderId, AccountId)],
    most_recent: Option<ProviderId>,
    tuning: &Tuning,
    cursor: &AtomicUsize,
    now: OffsetDateTime,
) -> Option<ProviderId> {
    let usable: Vec<&ProviderPool<'_>> = pools.iter().filter(|p| !p.cands.is_empty()).collect();
    match usable.len() {
        0 => return None,
        1 => return Some(usable[0].provider),
        _ => {}
    }

    let pinned: Vec<ProviderId> = usable
        .iter()
        .filter(|p| {
            active_tier(&p.cands).iter().any(|c| {
                pins.iter()
                    .any(|(pp, acct)| *pp == p.provider && *acct == c.meta.id)
            })
        })
        .map(|p| p.provider)
        .collect();
    if let Some(first) = pinned.first() {
        return Some(match most_recent {
            Some(m) if pinned.contains(&m) => m,
            _ => *first,
        });
    }

    // Global rows: every active-tier account of every pool, ranked together.
    struct Row<'a> {
        pool: usize,
        id: &'a AccountId,
        score: f64,
        viable: bool,
    }
    let tiers: Vec<Vec<&Candidate<'_>>> = usable.iter().map(|p| active_tier(&p.cands)).collect();
    let ranks: Vec<score::ExpiryRank> = usable
        .iter()
        .zip(&tiers)
        .flat_map(|(p, tier)| {
            tier.iter()
                .map(|c| score::expiry_rank(c.snapshot, p.family, now))
        })
        .collect();
    let shares = score::expiry_shares(&ranks);
    let mut rows: Vec<Row<'_>> = Vec::with_capacity(ranks.len());
    let mut i = 0;
    for (pool, (p, tier)) in usable.iter().zip(&tiers).enumerate() {
        for c in tier {
            let headroom = score::gate_headroom(c.snapshot, p.family, now);
            rows.push(Row {
                pool,
                id: &c.meta.id,
                score: score::score(
                    score::effective_weight(c.meta, now),
                    shares[i],
                    headroom,
                    c.active_sessions,
                    tuning,
                ),
                viable: headroom >= tuning.min_headroom,
            });
            i += 1;
        }
    }

    // Each provider's contender: the account its own `choose` would serve (cursor not advanced).
    let contenders: Vec<&Row<'_>> = usable
        .iter()
        .enumerate()
        .filter_map(|(pool, p)| {
            let preview = AtomicUsize::new(cursor.load(Ordering::SeqCst));
            let winner = choose(&p.cands, tuning, &preview, now, p.family)?;
            rows.iter().find(|r| r.pool == pool && r.id == winner)
        })
        .collect();
    let any_viable = contenders.iter().any(|r| r.viable);
    let mut best: Option<&Row<'_>> = None;
    for r in contenders.into_iter().filter(|r| r.viable || !any_viable) {
        if best.is_none_or(|b| r.score > b.score) {
            best = Some(r);
        }
    }
    best.map(|r| usable[r.pool].provider)
}
