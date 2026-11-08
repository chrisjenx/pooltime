//! The §4 health states layered on top of the routing availability (spec §4, §16).
//!
//! [`crate::accounts::StateBook::status`] derives *availability* (`ready | needs_login |
//! rate_limited | billing | access_denied`), which routing acts on. The one state an account
//! *shows* adds two clock/health-driven states on top ([`derive_health`]):
//!
//! - `expiring`: usable, but the recorded refresh-token expiry is within the warn window
//!   (`alerts.warn_days[0]`, default 3 days) or already passed without a refused refresh. Routes
//!   exactly like `ready`. An unknown expiry is never `expiring` (§16).
//! - `erroring`: [`ERRORING_THRESHOLD`] or more non-quota failures ([`erroring_class`]) within
//!   [`ERRORING_WINDOW`] with no success in between, or a failed expiry probe. Display only: the
//!   per-request failover still decides routing, so a blip never cuts capacity.
//!
//! Precedence, most severe first ([`severity`]): `needs_login > billing > access_denied >
//! erroring > rate_limited > expiring > ready`.

use std::collections::VecDeque;

use time::{Duration, OffsetDateTime};

use crate::accounts::AccountStatus;
use crate::errors::{FailReason, RefreshErrorKind};

/// Failures older than this no longer count towards `erroring`.
pub const ERRORING_WINDOW: Duration = Duration::minutes(15);
/// Failures within [`ERRORING_WINDOW`] that make an account `erroring`.
pub const ERRORING_THRESHOLD: u32 = 3;
/// How long a failed expiry probe keeps an account `erroring` without a success: the probe's
/// retry interval ([`ERRORING_WINDOW`]) plus two health ticks of slack, so the state stays up
/// between retries but lapses when probing stops.
pub const PROBE_FAILURE_TTL: Duration = Duration::minutes(17);
/// Default warn window when `alerts.warn_days` is empty.
pub const DEFAULT_WARN_DAYS: u32 = 3;
/// Cap on remembered failures per account (only the window's worth matters).
const MAX_FAILURES: usize = 32;

/// The expiry warn window from `alerts.warn_days` (its first entry).
pub fn warn_window(warn_days: &[u32]) -> Duration {
    Duration::days(i64::from(
        warn_days.first().copied().unwrap_or(DEFAULT_WARN_DAYS),
    ))
}

/// Sort key, most severe first (0 = `needs_login`).
pub fn severity(s: &AccountStatus) -> u8 {
    match s {
        AccountStatus::NeedsLogin => 0,
        AccountStatus::Billing => 1,
        AccountStatus::AccessDenied { .. } => 2,
        AccountStatus::Erroring { .. } => 3,
        AccountStatus::RateLimited { .. } => 4,
        AccountStatus::Expiring { .. } => 5,
        AccountStatus::Ready => 6,
    }
}

/// The class a failure counts under for `erroring`, `None` if it does not count. Counted: refresh
/// network / 5xx failures, a 401 that persisted after a refresh, access denied. Quota, overload,
/// a dead refresh token (that is `needs_login`) and per-request rejections do not.
pub fn erroring_class(reason: &FailReason) -> Option<String> {
    match reason {
        FailReason::Refresh(RefreshErrorKind::Network(_) | RefreshErrorKind::Server(_))
        | FailReason::Status(401)
        | FailReason::AccessDenied => Some(reason.detail()),
        _ => None,
    }
}

/// An account's live `erroring` verdict.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Erroring {
    /// Class of the latest failure, e.g. `refresh:server:503`.
    pub class: String,
    pub count: u32,
    /// The first failure of the current streak.
    pub since: OffsetDateTime,
}

/// Recent counted failures of one account (in memory only; a restart starts clean).
#[derive(Debug, Clone, Default)]
pub struct ErrorStreak {
    failures: VecDeque<(OffsetDateTime, String)>,
    /// A failed expiry probe: `erroring` on its own until a success clears it or
    /// [`PROBE_FAILURE_TTL`] passes (each failed retry renews it).
    probe_failed: Option<(OffsetDateTime, String)>,
}

impl ErrorStreak {
    /// Count one failure of `class` at `at`.
    pub fn record(&mut self, class: String, at: OffsetDateTime) {
        self.failures.retain(|(t, _)| at - *t < ERRORING_WINDOW);
        if self.failures.len() >= MAX_FAILURES {
            self.failures.pop_front();
        }
        self.failures.push_back((at, class));
    }

    /// The expiry probe failed without confirming a dead login.
    pub fn record_probe_failure(&mut self, class: String, at: OffsetDateTime) {
        self.probe_failed = Some((at, class));
    }

    /// A served request or a successful usage poll: everything is disproven.
    pub fn clear(&mut self) {
        self.failures.clear();
        self.probe_failed = None;
    }

    /// A successful token refresh disproves the refresh failures (and a failed probe), not the
    /// request-level ones (a persistent 401 keeps refreshing fine).
    pub fn clear_refresh_failures(&mut self) {
        self.failures.retain(|(_, c)| !c.starts_with("refresh:"));
        self.probe_failed = None;
    }

    pub fn is_empty(&self) -> bool {
        self.failures.is_empty() && self.probe_failed.is_none()
    }

    /// `Some` when the account is erroring at `now`.
    pub fn current(&self, now: OffsetDateTime) -> Option<Erroring> {
        let live: Vec<&(OffsetDateTime, String)> = self
            .failures
            .iter()
            .filter(|(t, _)| now - *t < ERRORING_WINDOW)
            .collect();
        let count = u32::try_from(live.len()).unwrap_or(u32::MAX);
        let latest = live.last().map(|(t, c)| (*t, c.clone()));
        let first = live.first().map(|(t, _)| *t);
        let probe = self
            .probe_failed
            .as_ref()
            .filter(|(pt, _)| now - *pt < PROBE_FAILURE_TTL);
        match probe {
            Some((pt, pc)) => {
                let class = match &latest {
                    Some((t, c)) if t > pt => c.clone(),
                    _ => pc.clone(),
                };
                Some(Erroring {
                    class,
                    count: count.max(1),
                    since: first.map_or(*pt, |f| f.min(*pt)),
                })
            }
            None if count >= ERRORING_THRESHOLD => Some(Erroring {
                class: latest.map(|(_, c)| c).unwrap_or_default(),
                count,
                since: first.unwrap_or(now),
            }),
            None => None,
        }
    }
}

/// The one state an account shows, from its routing availability, its live `erroring` verdict
/// and its recorded refresh-token expiry (`None` = unknown: never `expiring`).
pub fn derive_health(
    availability: AccountStatus,
    erroring: Option<&Erroring>,
    refresh_expires_at: Option<OffsetDateTime>,
    warn: Duration,
    now: OffsetDateTime,
) -> AccountStatus {
    if matches!(
        availability,
        AccountStatus::NeedsLogin | AccountStatus::Billing | AccountStatus::AccessDenied { .. }
    ) {
        return availability;
    }
    if let Some(e) = erroring {
        return AccountStatus::Erroring {
            class: e.class.clone(),
            count: e.count,
        };
    }
    if availability != AccountStatus::Ready {
        return availability;
    }
    match refresh_expires_at {
        Some(at) if at - now <= warn => AccountStatus::Expiring { expires_at: at },
        _ => AccountStatus::Ready,
    }
}

/// When an account entered the clock-driven `expiring` state (the start of its warn window).
pub fn expiring_since(expires_at: OffsetDateTime, warn: Duration) -> OffsetDateTime {
    expires_at - warn
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::errors::TransportKind;
    use time::macros::datetime;

    const NOW: OffsetDateTime = datetime!(2026-11-07 12:00 UTC);

    fn warn() -> Duration {
        warn_window(&[3, 1])
    }

    #[test]
    fn expiring_within_warn_window() {
        let ready = || AccountStatus::Ready;
        let h = |avail, exp| derive_health(avail, None, exp, warn(), NOW);
        let in_2d = NOW + Duration::days(2);
        // Inside the window (and exactly at its edge): expiring, carrying the expiry.
        assert_eq!(
            h(ready(), Some(in_2d)),
            AccountStatus::Expiring { expires_at: in_2d }
        );
        let edge = NOW + Duration::days(3);
        assert_eq!(
            h(ready(), Some(edge)),
            AccountStatus::Expiring { expires_at: edge }
        );
        // Outside the window: ready.
        assert_eq!(
            h(ready(), Some(NOW + Duration::days(3) + Duration::SECOND)),
            AccountStatus::Ready
        );
        // Passed but never refused: still expiring (only a probe confirms needs_login).
        let past = NOW - Duration::hours(1);
        assert_eq!(
            h(ready(), Some(past)),
            AccountStatus::Expiring { expires_at: past }
        );
        // Unknown expiry: never expiring (spec §16).
        assert_eq!(h(ready(), None), AccountStatus::Ready);
        // warn_days[0] drives the window; an empty list falls back to 3 days.
        assert_eq!(
            derive_health(ready(), None, Some(in_2d), warn_window(&[1]), NOW),
            AccountStatus::Ready
        );
        assert_eq!(warn_window(&[]), Duration::days(3));
        // More severe availability wins over expiring.
        let limited = AccountStatus::RateLimited {
            until: NOW + Duration::hours(2),
            window: Some("5h".into()),
        };
        assert_eq!(h(limited.clone(), Some(in_2d)), limited);
        assert_eq!(
            h(AccountStatus::NeedsLogin, Some(in_2d)),
            AccountStatus::NeedsLogin
        );
        assert_eq!(
            h(AccountStatus::Billing, Some(in_2d)),
            AccountStatus::Billing
        );
        // Severity order: needs_login > billing > access_denied > erroring > rate_limited >
        // expiring > ready.
        let order = [
            AccountStatus::NeedsLogin,
            AccountStatus::Billing,
            AccountStatus::AccessDenied { until: NOW },
            AccountStatus::Erroring {
                class: "x".into(),
                count: 3,
            },
            limited,
            AccountStatus::Expiring { expires_at: in_2d },
            AccountStatus::Ready,
        ];
        let sev: Vec<u8> = order.iter().map(severity).collect();
        assert!(sev.windows(2).all(|w| w[0] < w[1]), "{sev:?}");
        assert_eq!(expiring_since(in_2d, warn()), in_2d - Duration::days(3));
    }

    #[test]
    fn erroring_precedence_and_classes() {
        let e = Erroring {
            class: "refresh:server:503".into(),
            count: 3,
            since: NOW,
        };
        let erroring = AccountStatus::Erroring {
            class: "refresh:server:503".into(),
            count: 3,
        };
        let limited = AccountStatus::RateLimited {
            until: NOW + Duration::hours(1),
            window: None,
        };
        let soon = Some(NOW + Duration::days(1));
        assert_eq!(
            derive_health(limited, Some(&e), soon, warn(), NOW),
            erroring
        );
        assert_eq!(
            derive_health(AccountStatus::Ready, Some(&e), soon, warn(), NOW),
            erroring
        );
        let denied = AccountStatus::AccessDenied { until: NOW };
        assert_eq!(
            derive_health(denied.clone(), Some(&e), soon, warn(), NOW),
            denied
        );

        let counted = [
            FailReason::Refresh(RefreshErrorKind::Server(503)),
            FailReason::Refresh(RefreshErrorKind::Network(TransportKind::Dns)),
            FailReason::Status(401),
            FailReason::AccessDenied,
        ];
        for r in counted {
            assert_eq!(erroring_class(&r), Some(r.detail()), "{r:?}");
        }
        let ignored = [
            FailReason::Quota { reset: None },
            FailReason::Overloaded,
            FailReason::UpstreamRejected,
            FailReason::Billing,
            FailReason::Refresh(RefreshErrorKind::InvalidGrant),
            FailReason::Status(404),
        ];
        for r in ignored {
            assert_eq!(erroring_class(&r), None, "{r:?}");
        }
    }
}
