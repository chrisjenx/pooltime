//! Weighted account selection (INV §2; port of bro `AccountManager.pickByWeighted`).
//!
//! The caller filters accounts with [`usable`], builds a [`Candidate`] per survivor (with
//! its snapshot and live-session count) and asks [`choose`] for the winner. `choose` is
//! pure apart from the round-robin cursor, which advances only when the winning score is
//! tied.

pub mod reserve;
pub mod score;
pub mod tuning;

use std::collections::HashSet;
use std::sync::atomic::{AtomicUsize, Ordering};

use time::OffsetDateTime;

use crate::accounts::{AccountId, AccountMeta, AccountStatus, RateLimitSnapshot};
use crate::providers::ProviderId;

pub use score::{score, ExpiryRank};
pub use tuning::Tuning;

/// One selectable account with the live inputs scoring needs.
#[derive(Debug, Clone, Copy)]
pub struct Candidate<'a> {
    pub meta: &'a AccountMeta,
    pub snapshot: Option<&'a RateLimitSnapshot>,
    pub active_sessions: u32,
}

/// Can `meta` serve this request right now? Ready status, matching provider, not already
/// tried this request, and (when `family` is given) no spent model-scoped window for it.
/// `spent_families` are the families whose model-scoped window is currently spent for this
/// account. The soft in-flight cap is applied by the caller, not here.
pub fn usable(
    meta: &AccountMeta,
    status: &AccountStatus,
    provider: ProviderId,
    family: Option<&str>,
    spent_families: &[String],
    tried: &HashSet<AccountId>,
) -> bool {
    matches!(status, AccountStatus::Ready)
        && meta.provider == provider
        && !tried.contains(&meta.id)
        && family.is_none_or(|f| !spent_families.iter().any(|s| s == f))
}

/// Pick the account for a fresh placement. Only the lowest-priority-number tier among
/// `cands` competes; within it the highest [`score()`] among *viable* accounts (5h headroom
/// at least `min_headroom`) wins, falling back to all of the tier when none is viable. Exact
/// score ties are broken round-robin via `cursor` (advanced only on ties, like bro).
/// `now` drives `weight_override` expiry and which 7d resets are still in the future.
pub fn choose<'a>(
    cands: &[Candidate<'a>],
    tuning: &Tuning,
    cursor: &AtomicUsize,
    now: OffsetDateTime,
) -> Option<&'a AccountId> {
    let min_priority = cands.iter().map(|c| c.meta.priority).min()?;
    let tier: Vec<&Candidate<'a>> = cands
        .iter()
        .filter(|c| c.meta.priority == min_priority)
        .collect();

    let ranks: Vec<ExpiryRank> = tier
        .iter()
        .map(|c| score::expiry_rank(c.snapshot, now))
        .collect();
    let shares = score::expiry_shares(&ranks);

    struct Row<'a> {
        id: &'a AccountId,
        score: f64,
        viable: bool,
    }
    let mut rows: Vec<Row<'a>> = tier
        .iter()
        .zip(&shares)
        .map(|(c, share)| {
            let headroom = score::gate_headroom(c.snapshot, now);
            let meta: &'a AccountMeta = c.meta;
            Row {
                id: &meta.id,
                score: score::score(
                    score::effective_weight(meta, now),
                    *share,
                    headroom,
                    c.active_sessions,
                    tuning,
                ),
                viable: headroom >= tuning.min_headroom,
            }
        })
        .collect();
    if rows.iter().any(|r| r.viable) {
        rows.retain(|r| r.viable);
    }

    let best = rows
        .iter()
        .map(|r| r.score)
        .fold(f64::NEG_INFINITY, f64::max);
    let tied: Vec<&Row<'a>> = rows.iter().filter(|r| r.score == best).collect();
    match tied.len() {
        0 => rows.first().map(|r| r.id),
        1 => Some(tied[0].id),
        n => {
            let prev = cursor
                .try_update(Ordering::SeqCst, Ordering::SeqCst, |c| Some((c + 1) % n))
                .unwrap_or_default();
            Some(tied[prev % n].id)
        }
    }
}
