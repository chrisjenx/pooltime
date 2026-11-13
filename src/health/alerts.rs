//! Alert events and their notifications (spec §4 "Alerts", §16).
//!
//! Events:
//! - **Expiring** at each `alerts.warn_days` threshold N: the recorded refresh-token expiry is
//!   within N days and still ahead (`0 < expiry - now <= N days`). Key
//!   `<account>:expiring_<N>d:<refresh_expires_at>`. An unknown expiry never fires; nor does an
//!   account already `needs_login`. When several thresholds are crossed at once (e.g. the first
//!   start with 12 hours left) every key is recorded and one notification is shown.
//! - **Entering** `needs_login`, `billing` or `erroring`. Key `<account>:<state>:<since>`. Only a
//!   state the account was not in at the previous evaluation counts (in memory), so an
//!   `erroring` streak whose start drifts as old failures age out does not re-alert. After a
//!   restart a still-dead account is checked against the persisted keys again: a dead account
//!   notifies once ever per `since`, including on the very first start. An unknown `since`
//!   makes the key `<account>:<state>:unknown`; once recorded it suppresses every later entry
//!   into that state with an unknown `since`, in this process and after restarts alike, until
//!   retention prunes the row. That is intended: without a start time, re-entries cannot be told
//!   apart, and at most once beats spam.
//!
//! Each key is written to `alerts_sent` *before* the notification is shown, and a failed send
//! keeps the row: at most once beats spam. `alerts.notify = false` records and logs events but
//! shows nothing. Evaluation runs in the daemon only ([`spawn_alerts`]): every
//! [`super::HEALTH_TICK`] and, debounced by [`DEBOUNCE`], when the state book reports a change;
//! never per request. The CLI never notifies.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration as StdDuration;

use time::{Duration, OffsetDateTime};
use tokio::task::JoinHandle;

use super::notify::{Notification, Notifier};
use crate::accounts::{AccountId, AccountStatus};
use crate::server::mgmt::{AccountView, CredsInfo};
use crate::server::AppState;
use crate::store::Store;

/// Quiet time after a state-book change before evaluating (changes come in bursts).
pub const DEBOUNCE: StdDuration = StdDuration::from_secs(2);

const RELOGIN: &str = "open pooltime → Re-login";
const OPEN: &str = "open pooltime";

/// What alerts need to know about one account.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AlertInput {
    pub id: AccountId,
    /// The one state the account shows.
    pub status: AccountStatus,
    /// When it entered `status` (`None` = unknown).
    pub since: Option<OffsetDateTime>,
    /// Recorded refresh-token expiry (`None` = unknown).
    pub refresh_expires_at: Option<OffsetDateTime>,
}

impl From<&AccountView> for AlertInput {
    fn from(v: &AccountView) -> Self {
        AlertInput {
            id: v.meta.id.clone(),
            status: v.status.clone(),
            since: v.status_since,
            refresh_expires_at: match &v.creds {
                CredsInfo::Present {
                    refresh_expires_at, ..
                } => *refresh_expires_at,
                _ => None,
            },
        }
    }
}

/// One alert: the dedup keys it records and what it shows.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Event {
    account: AccountId,
    keys: Vec<String>,
    message: String,
    action: &'static str,
}

/// The alert evaluator: remembers the states seen at the previous evaluation.
pub struct Alerts {
    store: Arc<Store>,
    notifier: Arc<dyn Notifier>,
    warn_days: Vec<u32>,
    notify: bool,
    url: String,
    prev: HashMap<AccountId, &'static str>,
}

impl Alerts {
    /// `url` is what a click opens (the plain dashboard URL: never a key).
    pub fn new(
        store: Arc<Store>,
        notifier: Arc<dyn Notifier>,
        warn_days: Vec<u32>,
        notify: bool,
        url: String,
    ) -> Alerts {
        Alerts {
            store,
            notifier,
            warn_days,
            notify,
            url,
            prev: HashMap::new(),
        }
    }

    /// Fire every new event for `accounts` at `now`. Returns the notifications due (shown unless
    /// `alerts.notify` is off).
    pub async fn evaluate(
        &mut self,
        accounts: &[AlertInput],
        now: OffsetDateTime,
    ) -> Vec<Notification> {
        let mut seen: HashMap<AccountId, &'static str> = accounts
            .iter()
            .map(|a| (a.id.clone(), a.status.label()))
            .collect();
        let mut due = Vec::new();
        for ev in events(accounts, &self.warn_days, &self.prev, now) {
            let keys = ev.keys.clone();
            let ts = now.unix_timestamp();
            // Recorded before anything is shown; only a key new to the table fires.
            let fresh = self
                .store
                .exec(move |conn| {
                    let tx = conn.transaction()?;
                    let mut fresh = false;
                    for k in &keys {
                        fresh |= tx.execute(
                            "INSERT OR IGNORE INTO alerts_sent(key, ts) VALUES (?1, ?2)",
                            rusqlite::params![k, ts],
                        )? == 1;
                    }
                    tx.commit()?;
                    Ok(fresh)
                })
                .await;
            let fresh = match fresh {
                Ok(f) => f,
                Err(e) => {
                    // Not recorded: retried next time (the account is not marked as seen).
                    tracing::warn!(account = %ev.account, error = %format!("{e:#}"), "could not record alert");
                    match self.prev.get(&ev.account) {
                        Some(p) => seen.insert(ev.account.clone(), p),
                        None => seen.remove(&ev.account),
                    };
                    continue;
                }
            };
            if !fresh {
                continue;
            }
            let n = Notification {
                message: ev.message,
                action: ev.action.to_string(),
                url: self.url.clone(),
            };
            tracing::info!(
                account = %ev.account,
                event = %ev.keys.join(","),
                notify = self.notify,
                "alert: {}",
                n.message
            );
            if self.notify {
                let (notifier, note) = (self.notifier.clone(), n.clone());
                match crate::background::blocking(move || notifier.notify(&note)).await {
                    Ok(Ok(())) => {}
                    Ok(Err(e)) => {
                        tracing::warn!(account = %ev.account, error = %format!("{e:#}"), "alert notification failed")
                    }
                    Err(e) => {
                        tracing::warn!(account = %ev.account, error = %e, "alert notification task failed")
                    }
                }
            }
            due.push(n);
        }
        self.prev = seen;
        due
    }
}

/// The events `accounts` raise at `now`, given the states seen at the previous evaluation.
fn events(
    accounts: &[AlertInput],
    warn_days: &[u32],
    prev: &HashMap<AccountId, &'static str>,
    now: OffsetDateTime,
) -> Vec<Event> {
    let mut out = Vec::new();
    for a in accounts {
        let label = a.status.label();
        if matches!(
            a.status,
            AccountStatus::NeedsLogin | AccountStatus::Billing | AccountStatus::Erroring { .. }
        ) {
            if prev.get(&a.id) != Some(&label) {
                let since = a
                    .since
                    .map_or("unknown".to_string(), |s| s.unix_timestamp().to_string());
                let (message, action) = match a.status {
                    AccountStatus::NeedsLogin => (format!("{} needs login", a.id), RELOGIN),
                    AccountStatus::Billing => (format!("{} has a billing problem", a.id), OPEN),
                    _ => (format!("{} is erroring", a.id), OPEN),
                };
                out.push(Event {
                    account: a.id.clone(),
                    keys: vec![format!("{}:{label}:{since}", a.id)],
                    message,
                    action,
                });
            }
            if a.status == AccountStatus::NeedsLogin {
                continue;
            }
        }
        // Clock-driven: every crossed threshold, still ahead of the expiry.
        let Some(at) = a.refresh_expires_at else {
            continue;
        };
        let left = at - now;
        if left <= Duration::ZERO {
            continue;
        }
        let mut crossed: Vec<u32> = warn_days
            .iter()
            .copied()
            .filter(|n| left <= Duration::days(i64::from(*n)))
            .collect();
        if crossed.is_empty() {
            continue;
        }
        crossed.sort_unstable();
        crossed.dedup();
        let days = (left.whole_seconds() + 86_399) / 86_400;
        let unit = if days == 1 { "day" } else { "days" };
        out.push(Event {
            account: a.id.clone(),
            keys: crossed
                .iter()
                .map(|n| format!("{}:expiring_{n}d:{}", a.id, at.unix_timestamp()))
                .collect(),
            message: format!("{} login expires in {days} {unit}", a.id),
            action: RELOGIN,
        });
    }
    out
}

/// `http://<addr>/`: the dashboard as a click target (no key; the browser holds the session).
pub fn dashboard_url(app: &AppState) -> String {
    format!(
        "http://{}/",
        crate::server::health::probe_addr(app.cfg.host, app.cfg.port)
    )
}

/// Run alert evaluation for the daemon's lifetime (abort the handle to stop it).
pub fn spawn_alerts(app: Arc<AppState>, notifier: Arc<dyn Notifier>) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut alerts = Alerts::new(
            app.store.clone(),
            notifier,
            app.cfg.alerts.warn_days.clone(),
            app.cfg.alerts.notify,
            dashboard_url(&app),
        );
        let mut changes = Some(app.state.changes());
        let mut tick = tokio::time::interval(super::HEALTH_TICK);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            let changed = tokio::select! {
                _ = tick.tick() => false,
                r = async { changes.as_mut().expect("guarded").changed().await }, if changes.is_some() => {
                    if r.is_err() {
                        changes = None; // the book is gone: ticks only
                    }
                    r.is_ok()
                }
            };
            if changed {
                tokio::time::sleep(DEBOUNCE).await;
            }
            // Changes up to here are covered by this evaluation.
            if let Some(c) = changes.as_mut() {
                c.borrow_and_update();
            }
            let now = OffsetDateTime::now_utc();
            let round = async {
                match crate::server::mgmt::try_gather(&app, now).await {
                    Ok(inputs) => {
                        let accounts: Vec<AlertInput> =
                            inputs.accounts.iter().map(AlertInput::from).collect();
                        alerts.evaluate(&accounts, now).await;
                    }
                    Err(e) => tracing::warn!(error = %e, "alerts: could not read the pool"),
                }
            };
            crate::logging::catch_panic("alerts", round).await;
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::health::notify::RecordingNotifier;
    use time::macros::datetime;

    const NOW: OffsetDateTime = datetime!(2026-11-07 12:00 UTC);
    const URL: &str = "http://127.0.0.1:3460/";

    fn id(s: &str) -> AccountId {
        s.parse().unwrap()
    }

    fn acct(
        name: &str,
        status: AccountStatus,
        since: Option<OffsetDateTime>,
        exp: Option<OffsetDateTime>,
    ) -> AlertInput {
        AlertInput {
            id: id(name),
            status,
            since,
            refresh_expires_at: exp,
        }
    }

    fn alerts(store: &Arc<Store>, rec: &Arc<RecordingNotifier>, notify: bool) -> Alerts {
        Alerts::new(
            store.clone(),
            rec.clone(),
            vec![3, 1],
            notify,
            URL.to_string(),
        )
    }

    fn messages(rec: &RecordingNotifier) -> Vec<String> {
        rec.sent().into_iter().map(|n| n.message).collect()
    }

    fn rows(store: &Store) -> Vec<String> {
        store
            .read(|c| {
                let mut st = c.prepare("SELECT key FROM alerts_sent ORDER BY key")?;
                let keys = st
                    .query_map([], |r| r.get::<_, String>(0))?
                    .collect::<Result<Vec<_>, _>>()?;
                Ok(keys)
            })
            .unwrap()
    }

    #[tokio::test]
    async fn each_event_notifies_once_across_restarts() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("pooltime.db");
        let dead_since = NOW - Duration::hours(5);
        let exp_at = NOW + Duration::days(2);
        let pool = |now: OffsetDateTime| {
            vec![
                acct("dead", AccountStatus::NeedsLogin, Some(dead_since), None),
                acct(
                    "soon",
                    AccountStatus::Expiring { expires_at: exp_at },
                    Some(exp_at - Duration::days(3)),
                    Some(exp_at),
                ),
                acct(
                    "fine",
                    AccountStatus::Ready,
                    None,
                    Some(now + Duration::days(20)),
                ),
                // Unknown expiry: never alerts as expiring.
                acct("unknown", AccountStatus::Ready, None, None),
            ]
        };

        let rec = Arc::new(RecordingNotifier::default());
        {
            let store = Arc::new(Store::open(&db).unwrap());
            let mut a = alerts(&store, &rec, true);
            let fired = a.evaluate(&pool(NOW), NOW).await;
            assert_eq!(fired.len(), 2);
            assert_eq!(
                messages(&rec),
                vec!["dead needs login", "soon login expires in 2 days"]
            );
            let first = rec.sent();
            assert_eq!(first[0].action, "open pooltime → Re-login");
            assert_eq!(first[0].url, URL);
            assert!(first[0].text().ends_with("open pooltime → Re-login"));
            // Again, same process: nothing new.
            assert!(a.evaluate(&pool(NOW), NOW).await.is_empty());
            assert_eq!(
                rows(&store),
                vec![
                    format!("dead:needs_login:{}", dead_since.unix_timestamp()),
                    format!("soon:expiring_3d:{}", exp_at.unix_timestamp()),
                ]
            );
        }

        // Restart: a fresh evaluator over the reopened database remembers everything.
        let store = Arc::new(Store::open(&db).unwrap());
        let mut a = alerts(&store, &rec, true);
        assert!(a.evaluate(&pool(NOW), NOW).await.is_empty());
        assert_eq!(rec.sent().len(), 2);

        // The 1-day threshold is its own event, once.
        let later = NOW + Duration::hours(36);
        assert_eq!(a.evaluate(&pool(later), later).await.len(), 1);
        assert_eq!(messages(&rec)[2], "soon login expires in 1 day");
        assert!(a.evaluate(&pool(later), later).await.is_empty());
        // Past the expiry: no expiring alert (the probe decides what it is now).
        let past = exp_at + Duration::hours(1);
        assert!(a.evaluate(&pool(past), past).await.is_empty());

        // Re-login, then dead again later: a new event.
        let fixed = vec![acct("dead", AccountStatus::Ready, Some(NOW), None)];
        assert!(a.evaluate(&fixed, later).await.is_empty());
        let again = later + Duration::hours(1);
        let dead_again = vec![acct("dead", AccountStatus::NeedsLogin, Some(again), None)];
        assert_eq!(a.evaluate(&dead_again, again).await.len(), 1);
        assert_eq!(messages(&rec)[3], "dead needs login");
        assert_eq!(rec.sent().len(), 4);
    }

    #[tokio::test]
    async fn first_start_with_hours_left_records_every_threshold_once() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::open(&dir.path().join("p.db")).unwrap());
        let rec = Arc::new(RecordingNotifier::default());
        let mut a = alerts(&store, &rec, true);
        let at = NOW + Duration::hours(12);
        let pool = vec![acct(
            "soon",
            AccountStatus::Expiring { expires_at: at },
            None,
            Some(at),
        )];
        assert_eq!(a.evaluate(&pool, NOW).await.len(), 1);
        assert_eq!(messages(&rec), vec!["soon login expires in 1 day"]);
        assert_eq!(
            rows(&store),
            vec![
                format!("soon:expiring_1d:{}", at.unix_timestamp()),
                format!("soon:expiring_3d:{}", at.unix_timestamp()),
            ]
        );
    }

    #[tokio::test]
    async fn drifting_erroring_since_alerts_once_billing_and_erroring_texts() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::open(&dir.path().join("p.db")).unwrap());
        let rec = Arc::new(RecordingNotifier::default());
        let mut a = alerts(&store, &rec, true);
        let err = || AccountStatus::Erroring {
            class: "refresh:server:503".into(),
            count: 3,
        };
        let t1 = vec![
            acct("flaky", err(), Some(NOW), None),
            acct("held", AccountStatus::Billing, Some(NOW), None),
        ];
        assert_eq!(a.evaluate(&t1, NOW).await.len(), 2);
        // The streak start moves forward as old failures age out: still the same episode.
        let t2 = vec![acct(
            "flaky",
            err(),
            Some(NOW + Duration::minutes(10)),
            None,
        )];
        assert!(a.evaluate(&t2, NOW).await.is_empty());
        assert_eq!(
            messages(&rec),
            vec!["flaky is erroring", "held has a billing problem"]
        );
        assert!(rec.sent().iter().all(|n| n.action == "open pooltime"));
    }

    #[tokio::test]
    async fn notify_off_or_failing_send_still_records_once() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::open(&dir.path().join("p.db")).unwrap());
        let dead = vec![acct("dead", AccountStatus::NeedsLogin, Some(NOW), None)];

        // Off: the event is due and recorded, nothing is shown.
        let rec = Arc::new(RecordingNotifier::default());
        let mut off = alerts(&store, &rec, false);
        assert_eq!(off.evaluate(&dead, NOW).await.len(), 1);
        assert!(rec.sent().is_empty());
        assert_eq!(rows(&store).len(), 1);

        // A failing send keeps its row: no retry storm.
        let failing = Arc::new(RecordingNotifier::failing());
        let mut a = alerts(&store, &failing, true);
        let held = vec![acct("held", AccountStatus::Billing, Some(NOW), None)];
        assert_eq!(a.evaluate(&held, NOW).await.len(), 1);
        assert_eq!(failing.sent().len(), 1);
        let mut b = alerts(&store, &failing, true);
        assert!(b.evaluate(&held, NOW).await.is_empty());
        assert_eq!(failing.sent().len(), 1);
        assert_eq!(rows(&store).len(), 2);
    }
}
