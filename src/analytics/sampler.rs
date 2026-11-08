//! Records each account's account-wide 7-day utilization into `usage_samples` (spec §5).
//!
//! A sample is taken when the window's utilization or `resets_at` changes, at most one row per
//! account per [`MIN_GAP_SECS`]. A detected reset ([`usage_history::is_reset`]) bypasses the
//! throttle: the last pre-reset reading (when it was throttled away) and the new one are both
//! recorded at once, so the consumption before the reset is not lost. The background task
//! A reading held back by the throttle is written (with its own time) once the gap has passed,
//! even with no further change. The background task ([`spawn_sampler`]) wakes on
//! [`StateBook::changes`] and at least every [`MIN_GAP_SECS`], and queues rows on the store
//! writer; the request path never waits on it.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use time::OffsetDateTime;
use tokio::task::JoinHandle;

use crate::accounts::status::window_duration;
use crate::accounts::{Registry, StateBook};
use crate::store::usage_history::{self, is_reset, Point, WINDOW_7D};
use crate::store::Store;

/// At most one sample per account per this many seconds (resets excepted).
pub const MIN_GAP_SECS: i64 = 300;
/// Coalesce bursts of state changes (every served request bumps the change counter).
const COALESCE: Duration = Duration::from_secs(1);
/// Wake at least this often without a state change, to flush a throttled final reading.
const FLUSH_EVERY: Duration = Duration::from_secs(MIN_GAP_SECS as u64);

#[derive(Debug, Clone, Copy)]
struct Track {
    last_seen: Point,
    /// Whether `last_seen` is on disk.
    recorded: bool,
    last_recorded_ts: i64,
}

/// Per-account change detection and throttling (pure; the caller writes what it returns).
#[derive(Debug, Default)]
pub struct Sampler {
    tracks: HashMap<String, Track>,
}

impl Sampler {
    /// Start from the latest recorded reading per account (so a restart does not re-record).
    pub fn seed(latest: HashMap<String, Point>) -> Sampler {
        let tracks = latest
            .into_iter()
            .map(|(a, p)| {
                (
                    a,
                    Track {
                        last_seen: p,
                        recorded: true,
                        last_recorded_ts: p.ts,
                    },
                )
            })
            .collect();
        Sampler { tracks }
    }

    /// Feed the account's current reading; returns the readings to record now, oldest first.
    pub fn observe(&mut self, account: &str, p: Point) -> Vec<Point> {
        self.observe_at(account, p, p.ts)
    }

    /// [`observe`](Self::observe) at clock `now`: an unchanged reading that was throttled away is
    /// written (with its own `ts`) once [`MIN_GAP_SECS`] have passed since the last write, so the
    /// final reading before a quiet spell is not held back indefinitely.
    pub fn observe_at(&mut self, account: &str, p: Point, now: i64) -> Vec<Point> {
        let Some(t) = self.tracks.get_mut(account) else {
            self.tracks.insert(
                account.to_string(),
                Track {
                    last_seen: p,
                    recorded: true,
                    last_recorded_ts: p.ts,
                },
            );
            return vec![p];
        };
        let prev = t.last_seen;
        let unchanged = (p.util - prev.util).abs() < 1e-9 && p.reset == prev.reset;
        if unchanged || p.ts < prev.ts {
            if !t.recorded && now.max(p.ts) - t.last_recorded_ts >= MIN_GAP_SECS {
                t.recorded = true;
                t.last_recorded_ts = prev.ts;
                return vec![prev];
            }
            return Vec::new();
        }
        let mut out = Vec::new();
        if is_reset(&prev, &p) {
            if !t.recorded {
                out.push(prev);
            }
            out.push(p);
        } else if p.ts - t.last_recorded_ts >= MIN_GAP_SECS {
            out.push(p);
        }
        t.last_seen = p;
        t.recorded = !out.is_empty();
        if t.recorded {
            t.last_recorded_ts = p.ts;
        }
        out
    }
}

/// The account-wide 7-day window reading in `state`, if known.
fn seven_day_point(state: &crate::accounts::AccountState) -> Option<Point> {
    let snap = state.snapshot.as_ref()?;
    let week = time::Duration::days(7);
    let w = snap
        .windows
        .iter()
        .find(|w| w.model.is_none() && window_duration(&w.key) == Some(week))?;
    Some(Point {
        ts: snap.updated_at.unix_timestamp(),
        util: w.utilization.clamp(0.0, 1.0),
        reset: w.reset.map(OffsetDateTime::unix_timestamp),
    })
}

/// Observe every configured account once and queue the resulting rows.
fn sample_all(sampler: &mut Sampler, registry: &Registry, state: &StateBook, store: &Store) {
    let now = OffsetDateTime::now_utc();
    for meta in registry.list() {
        let Some(p) = seven_day_point(&state.state(&meta.id, now)) else {
            continue;
        };
        let account = meta.id.to_string();
        let points = sampler.observe_at(&account, p, now.unix_timestamp());
        if points.is_empty() {
            continue;
        }
        let queued = store.submit(move |c| {
            for p in &points {
                usage_history::insert_sample(c, &account, WINDOW_7D, p)?;
            }
            Ok(())
        });
        if let Err(e) = queued {
            tracing::warn!(error = %e, "could not queue a usage sample");
        }
    }
}

/// Sample on every state change until aborted.
pub fn spawn_sampler(
    registry: Arc<Registry>,
    state: Arc<StateBook>,
    store: Arc<Store>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut changes = state.changes();
        let seed_store = store.clone();
        let latest = tokio::task::spawn_blocking(move || {
            seed_store.read(|c| usage_history::latest_points(c, WINDOW_7D))
        })
        .await;
        let latest = match latest {
            Ok(Ok(l)) => l,
            Ok(Err(e)) => {
                tracing::warn!(error = %format!("{e:#}"), "reading the latest usage samples failed");
                HashMap::new()
            }
            Err(_) => HashMap::new(),
        };
        let mut sampler = Sampler::seed(latest);
        loop {
            // The registry listing reads account files: keep it off the async threads.
            let (r, s, st) = (registry.clone(), state.clone(), store.clone());
            sampler = match tokio::task::spawn_blocking(move || {
                sample_all(&mut sampler, &r, &s, &st);
                sampler
            })
            .await
            {
                Ok(s) => s,
                Err(_) => {
                    tracing::warn!("usage sampling task failed; sampling stopped");
                    return;
                }
            };
            // Wake on a change, or after MIN_GAP_SECS anyway to flush a held reading.
            tokio::select! {
                r = changes.changed() => {
                    if r.is_err() {
                        return;
                    }
                    tokio::time::sleep(COALESCE).await;
                    changes.borrow_and_update();
                }
                _ = tokio::time::sleep(FLUSH_EVERY) => {}
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(ts: i64, util: f64, reset: i64) -> Point {
        Point {
            ts,
            util,
            reset: Some(reset),
        }
    }

    #[test]
    fn throttled_final_reading_is_flushed() {
        let r1 = 1_000_000;
        let mut s = Sampler::default();
        assert_eq!(s.observe("a", p(0, 0.1, r1)).len(), 1);
        assert!(s.observe("a", p(60, 0.5, r1)).is_empty(), "throttled");
        // No further change: once MIN_GAP has passed, the held reading is written as it was.
        assert!(
            s.observe_at("a", p(60, 0.5, r1), 200).is_empty(),
            "too early"
        );
        assert_eq!(s.observe_at("a", p(60, 0.5, r1), 300), [p(60, 0.5, r1)]);
        assert!(
            s.observe_at("a", p(60, 0.5, r1), 900).is_empty(),
            "only once"
        );
        // and the throttle restarts from it
        assert!(s.observe_at("a", p(100, 0.6, r1), 950).is_empty());
        assert_eq!(s.observe_at("a", p(400, 0.7, r1), 1000).len(), 1);
    }

    #[test]
    fn sampler_throttles_and_keeps_reset_edges() {
        let mut s = Sampler::default();
        let r1 = 1_000_000;
        assert_eq!(
            s.observe("a", p(0, 0.1, r1)),
            [p(0, 0.1, r1)],
            "first reading"
        );
        assert!(s.observe("a", p(10, 0.1, r1)).is_empty(), "unchanged");
        assert!(s.observe("a", p(60, 0.2, r1)).is_empty(), "throttled");
        assert!(s.observe("a", p(200, 0.3, r1)).is_empty(), "throttled");
        assert_eq!(
            s.observe("a", p(300, 0.35, r1)),
            [p(300, 0.35, r1)],
            "5 min passed"
        );
        assert!(s.observe("a", p(320, 0.9, r1)).is_empty(), "throttled");
        // Reset right away: the unrecorded 0.9 and the new value are both written.
        let r2 = r1 + 7 * 86_400;
        assert_eq!(
            s.observe("a", p(330, 0.05, r2)),
            [p(320, 0.9, r1), p(330, 0.05, r2)]
        );
        // A reset whose pre-reset reading is already on disk writes only the new value.
        assert_eq!(
            s.observe("a", p(340, 0.0, r2 + 7 * 86_400)),
            [p(340, 0.0, r2 + 7 * 86_400)]
        );
        // Other accounts are independent; out-of-order readings are ignored.
        assert_eq!(s.observe("b", p(5, 0.5, r1)).len(), 1);
        assert!(s.observe("a", p(100, 0.7, r2)).is_empty());

        // Seeded from disk: the same reading is not recorded again, and the throttle holds.
        let mut s = Sampler::seed(HashMap::from([("a".to_string(), p(1000, 0.4, r1))]));
        assert!(s.observe("a", p(1100, 0.4, r1)).is_empty());
        assert!(s.observe("a", p(1100, 0.45, r1)).is_empty());
        assert_eq!(s.observe("a", p(1300, 0.5, r1)).len(), 1);
    }
}
