//! Weighted account selection (INV §2; port of bro `AccountManager.pickByWeighted`).
//!
//! The caller filters accounts with [`usable`], builds a [`Candidate`] per survivor (with
//! its snapshot and live-session count) and asks [`choose`] for the winner. `choose` is
//! [`rank`] (pure: every account's [`Factors`] and the tied top scores) plus the round-robin
//! tie-break, whose cursor advances only when the winning score is tied. The routing preview
//! (`GET /api/v1/routing/preview`) shows the same [`Ranking`].

pub mod reserve;
pub mod score;
pub mod sessions;
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

/// Every input and intermediate value behind one account's weighted score, as [`rank`]
/// computes it. The routing preview shows these; [`choose`] decides on them.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Factors<'a> {
    pub id: &'a AccountId,
    /// The weight scoring used ([`score::effective_weight`]: an unexpired override, else the
    /// base weight).
    pub weight: f64,
    /// Where the account sits in the 7d-expiry ordering.
    pub expiry: ExpiryRank,
    /// Share for that expiry rank (5, 3, 2, 1, 0.5, ...) on the ranked set's scale.
    pub expiry_share: f64,
    /// 5h gate headroom (0-1).
    pub headroom: f64,
    /// [`score::five_hour_factor`] of `headroom`: the taper (1 = neutral).
    pub five_hour_factor: f64,
    /// Live sticky sessions pinned to the account (the load term).
    pub active_sessions: u32,
    /// [`score::score`] of the above.
    pub score: f64,
    /// 5h headroom at least `min_headroom`.
    pub viable: bool,
}

/// [`Factors`] of `c` whose expiry rank is `expiry` with share `expiry_share` (one share scale
/// per ranked set). Used by [`rank`] and by the cross-provider ranking.
pub fn factors<'a>(
    c: &Candidate<'a>,
    expiry: ExpiryRank,
    expiry_share: f64,
    tuning: &Tuning,
    now: OffsetDateTime,
    family: Option<&str>,
) -> Factors<'a> {
    let meta: &'a AccountMeta = c.meta;
    let headroom = score::gate_headroom(c.snapshot, family, now);
    let weight = score::effective_weight(meta, now);
    Factors {
        id: &meta.id,
        weight,
        expiry,
        expiry_share,
        headroom,
        five_hour_factor: score::five_hour_factor(headroom, tuning),
        active_sessions: c.active_sessions,
        score: score::score(weight, expiry_share, headroom, c.active_sessions, tuning),
        viable: headroom >= tuning.min_headroom,
    }
}

/// The weighted ranking of a candidate set: the decision [`choose`] makes, minus the tie-break.
#[derive(Debug, Clone)]
pub struct Ranking<'a> {
    /// Priority number of the active tier (the lowest among the candidates).
    pub tier: u32,
    /// One row per active-tier candidate, in input order.
    pub rows: Vec<Factors<'a>>,
    /// No row is viable, so every row competes (best-effort fallback).
    pub best_effort: bool,
    /// Indices into `rows` of the competing rows with the top score, in input order. More
    /// than one means the round-robin cursor decides; empty only if no score is comparable.
    pub tied: Vec<usize>,
}

impl<'a> Ranking<'a> {
    /// Whether row `i` competes: viable, or nothing is (best effort).
    pub fn competes(&self, i: usize) -> bool {
        self.best_effort || self.rows[i].viable
    }

    /// The winner when the round-robin cursor reads `cursor` (only consulted on a tie).
    pub fn winner_at(&self, cursor: usize) -> Option<&'a AccountId> {
        match self.tied.len() {
            0 => (0..self.rows.len())
                .find(|&i| self.competes(i))
                .map(|i| self.rows[i].id),
            1 => Some(self.rows[self.tied[0]].id),
            n => Some(self.rows[self.tied[cursor % n]].id),
        }
    }
}

/// Rank `cands` as [`choose`] does: only the lowest-priority-number tier competes; each of its
/// accounts gets [`factors`] on one expiry-share scale; non-viable accounts drop out while any
/// account is viable. `None` when `cands` is empty. Pure.
pub fn rank<'a>(
    cands: &[Candidate<'a>],
    tuning: &Tuning,
    now: OffsetDateTime,
    family: Option<&str>,
) -> Option<Ranking<'a>> {
    let tier_no = cands.iter().map(|c| c.meta.priority).min()?;
    let tier: Vec<&Candidate<'a>> = cands
        .iter()
        .filter(|c| c.meta.priority == tier_no)
        .collect();
    let ranks: Vec<ExpiryRank> = tier
        .iter()
        .map(|c| score::expiry_rank(c.snapshot, family, now))
        .collect();
    let shares = score::expiry_shares(&ranks);
    let rows: Vec<Factors<'a>> = tier
        .iter()
        .zip(ranks.iter().zip(&shares))
        .map(|(c, (rank, share))| factors(c, *rank, *share, tuning, now, family))
        .collect();
    let best_effort = !rows.iter().any(|r| r.viable);
    let competing = |r: &Factors<'_>| best_effort || r.viable;
    let best = rows
        .iter()
        .filter(|r| competing(r))
        .map(|r| r.score)
        .fold(f64::NEG_INFINITY, f64::max);
    let tied = rows
        .iter()
        .enumerate()
        .filter(|(_, r)| competing(r) && r.score == best)
        .map(|(i, _)| i)
        .collect();
    Some(Ranking {
        tier: tier_no,
        rows,
        best_effort,
        tied,
    })
}

/// Pick the account for a fresh placement. Only the lowest-priority-number tier among
/// `cands` competes; within it the highest [`score()`] among *viable* accounts (5h headroom
/// at least `min_headroom`) wins, falling back to all of the tier when none is viable. Exact
/// score ties are broken round-robin via `cursor` (advanced only on ties, like bro).
/// `now` drives `weight_override` expiry and which 7d resets are still in the future;
/// `family` adds that family's model-scoped windows to the binding windows. The decision is
/// [`rank`]'s; this adds only the tie-break.
pub fn choose<'a>(
    cands: &[Candidate<'a>],
    tuning: &Tuning,
    cursor: &AtomicUsize,
    now: OffsetDateTime,
    family: Option<&str>,
) -> Option<&'a AccountId> {
    let ranking = rank(cands, tuning, now, family)?;
    let n = ranking.tied.len();
    let at = if n > 1 {
        cursor
            .try_update(Ordering::SeqCst, Ordering::SeqCst, |c| Some((c + 1) % n))
            .unwrap_or_default()
    } else {
        0
    };
    ranking.winner_at(at)
}

/// The account [`choose`] would return for the same inputs, reading `cursor` without advancing
/// it (the routing preview's tie-break).
pub fn peek<'a>(
    cands: &[Candidate<'a>],
    tuning: &Tuning,
    cursor: &AtomicUsize,
    now: OffsetDateTime,
    family: Option<&str>,
) -> Option<&'a AccountId> {
    choose(
        cands,
        tuning,
        &AtomicUsize::new(cursor.load(Ordering::SeqCst)),
        now,
        family,
    )
}
