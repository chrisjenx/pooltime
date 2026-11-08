//! Pure scoring pieces of the weighted strategy (INV §2, port of bro `manager.ts`
//! `scoreWeightedRows` / `candidateExpiryReset` / `candidateGateHeadroom`).
//!
//! Only account-wide windows (`model == None`) are considered; model-scoped windows are
//! handled by `usable` (spent family) and never influence the score. Windows whose reset
//! is already in the past are treated as refreshed and ignored, as in bro.

use time::{Duration, OffsetDateTime};

use super::tuning::Tuning;
use crate::accounts::status::window_duration;
use crate::accounts::{AccountMeta, RateLimitSnapshot, Window, WindowStatus};

/// Where an account sits in the 7d-expiry ordering. Derived `Ord`: `Unknown` (probe it
/// first) < `At(reset)` (soonest first) < `Spent` (weekly allowance already burned, last).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ExpiryRank {
    Unknown,
    At(OffsetDateTime),
    Spent,
}

/// Mirrors the (private) `is_spent` in `accounts::status`: explicitly rejected, or fully
/// used without an explicit `allowed` (Codex reports 100% + allowed while unenforced).
fn is_spent(w: &Window) -> bool {
    match w.status {
        WindowStatus::Rejected => true,
        WindowStatus::Allowed => false,
        WindowStatus::AllowedWarning | WindowStatus::Unknown => w.utilization >= 1.0,
    }
}

/// Live account-wide windows: no model scope, and reset unknown or still in the future.
fn binding_windows(
    snap: &RateLimitSnapshot,
    now: OffsetDateTime,
) -> impl Iterator<Item = &Window> + '_ {
    snap.windows
        .iter()
        .filter(|w| w.model.is_none())
        .filter(move |w| w.reset.is_none_or(|r| r > now))
}

/// 5h gate headroom (0-1). Only live 5h windows count; a window that is `allowed` at
/// >=100% is unenforced and ignored; no data means full headroom (1.0).
pub fn gate_headroom(snap: Option<&RateLimitSnapshot>, now: OffsetDateTime) -> f64 {
    let Some(snap) = snap else { return 1.0 };
    let max_util = binding_windows(snap, now)
        .filter(|w| window_duration(&w.key) == Some(Duration::hours(5)))
        .filter(|w| !(w.status == WindowStatus::Allowed && w.utilization >= 1.0))
        .map(|w| w.utilization)
        .fold(None, |m: Option<f64>, u| Some(m.map_or(u, |m| m.max(u))));
    match max_util {
        Some(u) => (1.0 - u).max(0.0),
        None => 1.0,
    }
}

/// Expiry ordering key from the longest-duration live account-wide window(s) (normally
/// `7d`): the soonest future reset among those not spent; else `Spent` if any of them is
/// spent; else `Unknown` (no data, or only already-passed resets).
pub fn expiry_rank(snap: Option<&RateLimitSnapshot>, now: OffsetDateTime) -> ExpiryRank {
    let Some(snap) = snap else {
        return ExpiryRank::Unknown;
    };
    let timed: Vec<(Duration, &Window)> = binding_windows(snap, now)
        .filter_map(|w| window_duration(&w.key).map(|d| (d, w)))
        .collect();
    let Some(longest) = timed.iter().map(|(d, _)| *d).max() else {
        return ExpiryRank::Unknown;
    };
    let longest: Vec<&Window> = timed
        .into_iter()
        .filter(|(d, _)| *d == longest)
        .map(|(_, w)| w)
        .collect();
    let soonest = longest
        .iter()
        .filter(|w| !is_spent(w))
        .filter_map(|w| w.reset.filter(|r| *r > now))
        .min();
    match soonest {
        Some(r) => ExpiryRank::At(r),
        None if longest.iter().any(|w| is_spent(w)) => ExpiryRank::Spent,
        None => ExpiryRank::Unknown,
    }
}

/// Share for a 0-based distinct-rank position: 5, 3, 2, 1, then halving (0.5, 0.25, ...).
pub fn share_for_rank(rank: usize) -> f64 {
    match rank {
        0 => 5.0,
        1 => 3.0,
        2 => 2.0,
        3 => 1.0,
        n => 0.5f64.powi(n as i32 - 3),
    }
}

/// Expiry share per input, ranking over the *distinct* rank values so ties share a rank.
pub fn expiry_shares(ranks: &[ExpiryRank]) -> Vec<f64> {
    let mut distinct: Vec<ExpiryRank> = ranks.to_vec();
    distinct.sort();
    distinct.dedup();
    ranks
        .iter()
        .map(|r| share_for_rank(distinct.iter().position(|d| d == r).unwrap_or(0)))
        .collect()
}

/// `min(1, headroom / taper_start) ^ five_hour_exp`.
pub fn five_hour_factor(headroom: f64, tuning: &Tuning) -> f64 {
    (headroom / tuning.headroom_taper_start)
        .min(1.0)
        .powf(tuning.five_hour_exp)
}

/// `weight x expiry_share x five_hour_factor / (active_sessions + 1)`.
pub fn score(
    weight: f64,
    expiry_share: f64,
    headroom: f64,
    active_sessions: u32,
    tuning: &Tuning,
) -> f64 {
    weight * expiry_share * five_hour_factor(headroom, tuning) / (f64::from(active_sessions) + 1.0)
}

/// The weight scoring uses: an unexpired `weight_override` wins over the base weight.
pub fn effective_weight(meta: &AccountMeta, now: OffsetDateTime) -> f64 {
    match &meta.weight_override {
        Some(o) if o.until > now => o.weight,
        _ => meta.weight,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn share_sequence() {
        let got: Vec<f64> = (0..7).map(share_for_rank).collect();
        assert_eq!(got, vec![5.0, 3.0, 2.0, 1.0, 0.5, 0.25, 0.125]);
    }

    #[test]
    fn factor_is_neutral_above_taper_start() {
        let t = Tuning::default();
        assert_eq!(five_hour_factor(1.0, &t), 1.0);
        assert_eq!(five_hour_factor(0.2, &t), 1.0);
        assert!((five_hour_factor(0.15, &t) - 0.75).abs() < 1e-12);
        assert_eq!(five_hour_factor(0.0, &t), 0.0);
    }
}
