//! Codex rate-limit windows from response headers (INV §4 "Codex responses"; bro
//! `parseCodexRateLimitSnapshot`, `durationToWindowKey`, `resetAtFromCodexHeaders`).
//!
//! Codex reports two rolling windows in `x-codex-{primary,secondary}-{used-percent,
//! window-minutes,reset-at}`. A window's key comes from its **duration**, not its slot: under
//! 24 h is `5h`, longer is `7d` (Codex ships the weekly window in the primary slot when the
//! session window is absent). Without a duration the slot's historical default applies
//! (primary `5h`, secondary `7d`). `used-percent` is 0–100 and is stored as a 0–1 fraction;
//! `reset-at` is an absolute unix timestamp in seconds.
//!
//! Status: a window is `allowed` even at 100% (a served response proves the limit is not
//! enforced right now) unless the response was a 429, where a full window is `rejected`.

use http::HeaderMap;
use time::OffsetDateTime;

use crate::accounts::{AccountId, RateLimitSnapshot, StateBook, Window, WindowStatus};
use crate::providers::anthropic::retry_after_at;

const DAY_MINUTES: f64 = 24.0 * 60.0;

/// Which header slot a window arrived in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Slot {
    Primary,
    Secondary,
}

impl Slot {
    fn name(self) -> &'static str {
        match self {
            Slot::Primary => "primary",
            Slot::Secondary => "secondary",
        }
    }
}

/// Canonical account-wide key for a window of `minutes` (`None` / non-finite: slot default).
pub fn window_key(minutes: Option<f64>, slot: Slot) -> &'static str {
    match minutes.filter(|m| m.is_finite()) {
        Some(m) if m < DAY_MINUTES => "5h",
        Some(_) => "7d",
        None if slot == Slot::Primary => "5h",
        None => "7d",
    }
}

fn header<'a>(h: &'a HeaderMap, slot: Slot, field: &str) -> Option<&'a str> {
    h.get(format!("x-codex-{}-{field}", slot.name()).as_str())
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|v| !v.is_empty())
}

fn num(h: &HeaderMap, slot: Slot, field: &str) -> Option<f64> {
    header(h, slot, field)?
        .parse::<f64>()
        .ok()
        .filter(|n| n.is_finite())
}

fn reset_at(h: &HeaderMap, slot: Slot) -> Option<OffsetDateTime> {
    let secs: i64 = header(h, slot, "reset-at")?.parse().ok()?;
    OffsetDateTime::from_unix_timestamp(secs).ok()
}

/// The windows the response reports, or `None` when it reports none (callers must then keep
/// the previous snapshot: an empty snapshot would wipe the known windows). `rejected` = the
/// response was a 429.
pub fn parse_codex_rate_limits(
    h: &HeaderMap,
    rejected: bool,
    now: OffsetDateTime,
) -> Option<RateLimitSnapshot> {
    let mut windows: Vec<Window> = Vec::new();
    for slot in [Slot::Primary, Slot::Secondary] {
        let used = num(h, slot, "used-percent");
        let reset = reset_at(h, slot);
        if used.is_none() && reset.is_none() {
            continue;
        }
        let mut key = window_key(num(h, slot, "window-minutes"), slot);
        // Two windows of the same scale (never seen today): keep both.
        if windows.iter().any(|w| w.key == key) {
            key = if key == "5h" { "7d" } else { "5h" };
        }
        let utilization = used.map_or(0.0, |u| (u / 100.0).max(0.0));
        let status = if rejected && utilization >= 1.0 {
            WindowStatus::Rejected
        } else {
            WindowStatus::Allowed
        };
        windows.push(Window {
            reset_synthetic: false,
            key: key.to_string(),
            model: None,
            status,
            utilization,
            reset,
        });
    }
    if windows.is_empty() {
        return None;
    }
    let any_rejected = windows.iter().any(|w| w.status == WindowStatus::Rejected);
    Some(RateLimitSnapshot {
        unified_status: Some(if any_rejected { "rejected" } else { "allowed" }.to_string()),
        windows,
        updated_at: now,
    })
}

/// When a Codex 429 benches the account: the soonest **future** reset among the reported
/// windows (with that window's key), else `retry-after`, else `None`.
pub fn codex_quota_reset(
    snap: Option<&RateLimitSnapshot>,
    h: &HeaderMap,
    now: OffsetDateTime,
) -> Option<(OffsetDateTime, Option<String>)> {
    let soonest = snap.and_then(|s| {
        s.windows
            .iter()
            .filter_map(|w| w.reset.filter(|r| *r > now).map(|r| (r, w.key.clone())))
            .min_by_key(|(r, _)| *r)
    });
    match soonest {
        Some((r, key)) => Some((r, Some(key))),
        None => retry_after_at(h, now)
            .filter(|r| *r > now)
            .map(|r| (r, None)),
    }
}

/// A rate-limit error *event* inside a 200 stream counts like a 429 (bro `markRateLimited` ->
/// `blockingWindowReset`): the response's windows are re-applied as rejected (a full window is
/// spent now), then the account is benched until the soonest spent window's future reset, else
/// for `cooldown`. Returns the bench end.
pub(crate) fn bench_for_rate_limit_event(
    state: &StateBook,
    id: &AccountId,
    headers: &HeaderMap,
    now: OffsetDateTime,
    cooldown: time::Duration,
) -> OffsetDateTime {
    if let Some(s) = parse_codex_rate_limits(headers, true, now) {
        state.replace_header_snapshot(id, s);
    }
    let (until, window) = match state.blocking_window(id, now) {
        Some((r, key)) => (r, Some(key)),
        None => (now + cooldown, None),
    };
    state.mark_rate_limited(id, until, window, now);
    until
}

#[cfg(test)]
mod tests {
    use super::*;
    use http::{HeaderName, HeaderValue};

    fn headers(pairs: &[(&str, String)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.insert(
                HeaderName::from_bytes(k.as_bytes()).unwrap(),
                HeaderValue::from_str(v).unwrap(),
            );
        }
        h
    }

    #[test]
    fn key_from_duration_with_slot_fallback() {
        assert_eq!(window_key(Some(300.0), Slot::Secondary), "5h");
        assert_eq!(window_key(Some(10080.0), Slot::Primary), "7d");
        assert_eq!(window_key(Some(1439.0), Slot::Primary), "5h");
        assert_eq!(window_key(Some(1440.0), Slot::Primary), "7d");
        assert_eq!(window_key(None, Slot::Primary), "5h");
        assert_eq!(window_key(None, Slot::Secondary), "7d");
        assert_eq!(window_key(Some(f64::NAN), Slot::Secondary), "7d");
    }

    #[test]
    fn percent_becomes_fraction_and_empty_is_none() {
        let now = OffsetDateTime::now_utc();
        assert!(parse_codex_rate_limits(&HeaderMap::new(), false, now).is_none());
        let reset = now.unix_timestamp() + 60;
        let h = headers(&[
            ("x-codex-primary-used-percent", "42.5".into()),
            ("x-codex-primary-window-minutes", "300".into()),
            ("x-codex-primary-reset-at", reset.to_string()),
        ]);
        let s = parse_codex_rate_limits(&h, false, now).unwrap();
        assert_eq!(s.windows.len(), 1);
        let w = &s.windows[0];
        assert_eq!(w.key, "5h");
        assert!((w.utilization - 0.425).abs() < 1e-9);
        assert_eq!(w.reset.unwrap().unix_timestamp(), reset);
        assert_eq!(s.unified_status.as_deref(), Some("allowed"));
    }

    #[test]
    fn quota_reset_skips_past_resets_then_retry_after() {
        let now = OffsetDateTime::now_utc();
        let past = (now.unix_timestamp() - 60).to_string();
        let future = (now.unix_timestamp() + 600).to_string();
        let h = headers(&[
            ("x-codex-primary-reset-at", past),
            ("x-codex-primary-window-minutes", "300".into()),
            ("x-codex-secondary-reset-at", future.clone()),
            ("x-codex-secondary-window-minutes", "10080".into()),
        ]);
        let s = parse_codex_rate_limits(&h, true, now);
        let (r, key) = codex_quota_reset(s.as_ref(), &h, now).unwrap();
        assert_eq!(r.unix_timestamp().to_string(), future);
        assert_eq!(key.as_deref(), Some("7d"));

        let h = headers(&[("retry-after", "30".into())]);
        let (r, key) = codex_quota_reset(None, &h, now).unwrap();
        assert_eq!(((r - now).whole_seconds(), key), (30, None));
        assert!(codex_quota_reset(None, &HeaderMap::new(), now).is_none());
    }
}
