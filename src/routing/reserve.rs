//! In-flight reservations, soft per-account concurrency caps and the shared wait budget
//! (INV §2 "soft concurrency"; port of bro's in-flight reservation logic).
//!
//! A request calls [`Reservations::reserve`] with a *pick closure* that chooses an account
//! (auth, cooldown, exclusion and scoring are the closure's business). Pick and reserve
//! happen atomically under one lock, so two requests never take the same last slot. When
//! every usable account is at its cap, `reserve` waits for a release until the request's
//! [`WaitBudget`] expires, then exceeds the cap rather than failing. The held
//! [`Reservation`] releases its slot on drop (completion, failure or client abort).
//!
//! Pick closure contract: `Fn(&dyn Fn(&AccountId) -> bool) -> Option<AccountId>`. The
//! argument is `has_room`; the closure must only return accounts for which `has_room(id)`
//! is true (and which pass its own usability filters). It is called with the counts lock
//! held, so it must not call back into [`Reservations`] (use `has_room` instead of
//! `in_flight`).

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use tokio::sync::Notify;
use tokio::time::Instant;

use crate::accounts::AccountId;
use crate::providers::ProviderId;

/// Safety-net re-check interval while waiting. A release wakes exactly one waiter, but that
/// waiter's pick filter may not accept the freed account (e.g. it already tried it); the
/// poll bounds how long another waiter that could use the slot would otherwise sleep.
const RECHECK: Duration = Duration::from_millis(500);

/// Wait allowance shared by every reserve attempt of one request (retries, provider
/// fallbacks). Uses tokio's clock so tests can pause and advance time.
#[derive(Debug, Clone, Copy)]
pub struct WaitBudget {
    deadline: Instant,
}

impl WaitBudget {
    pub fn new(d: Duration) -> Self {
        Self {
            deadline: Instant::now() + d,
        }
    }

    pub fn remaining(&self) -> Duration {
        self.deadline.saturating_duration_since(Instant::now())
    }

    pub fn expired(&self) -> bool {
        Instant::now() >= self.deadline
    }
}

#[derive(Default)]
struct State {
    counts: HashMap<AccountId, u32>,
    notifies: HashMap<ProviderId, Arc<Notify>>,
}

struct Inner {
    /// 0 = unlimited; a provider with no entry is unlimited too.
    caps: HashMap<ProviderId, u32>,
    state: Mutex<State>,
}

impl Inner {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// Shared in-flight counts. Cheap to clone (all clones share state); `Send + Sync`.
#[derive(Clone)]
pub struct Reservations {
    inner: Arc<Inner>,
}

/// One held in-flight slot. Dropping it releases the slot and wakes at most one waiter.
pub struct Reservation {
    inner: Arc<Inner>,
    id: AccountId,
    provider: ProviderId,
}

impl Reservation {
    pub fn account(&self) -> &AccountId {
        &self.id
    }
}

impl std::fmt::Debug for Reservation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Reservation")
            .field("account", &self.id)
            .field("provider", &self.provider)
            .finish()
    }
}

impl Drop for Reservation {
    fn drop(&mut self) {
        let notify = {
            let mut st = self.inner.lock();
            if let Some(n) = st.counts.get_mut(&self.id) {
                *n = n.saturating_sub(1);
                if *n == 0 {
                    st.counts.remove(&self.id);
                }
            }
            st.notifies.get(&self.provider).cloned()
        };
        // One freed slot wakes one waiter (a permit is stored if nobody waits yet; the
        // spurious wake-up that causes is harmless because waiters re-check under the lock).
        if let Some(n) = notify {
            n.notify_one();
        }
    }
}

impl Reservations {
    /// `caps`: max concurrent requests per account, per provider; 0 = unlimited.
    pub fn new(caps: HashMap<ProviderId, u32>) -> Self {
        Self {
            inner: Arc::new(Inner {
                caps,
                state: Mutex::new(State::default()),
            }),
        }
    }

    pub fn in_flight(&self, id: &AccountId) -> u32 {
        self.inner.lock().counts.get(id).copied().unwrap_or(0)
    }

    /// Pick and reserve an account of `provider`.
    ///
    /// 1. `pick` runs with `has_room` = "this account is under the provider cap"; a result
    ///    is reserved in the same critical section.
    /// 2. If it finds nothing, `pick` runs again with `has_room` always true. `None` there
    ///    means no usable account exists, and `reserve` returns `None` immediately.
    /// 3. Otherwise every usable account is at cap: wait for a release (or `budget`
    ///    expiry), re-picking on each wake-up. Once `budget` has expired the cap is
    ///    exceeded using the uncapped pick, so only the cap is ever bypassed.
    pub async fn reserve(
        &self,
        pick: impl Fn(&dyn Fn(&AccountId) -> bool) -> Option<AccountId>,
        provider: ProviderId,
        budget: &WaitBudget,
    ) -> Option<Reservation> {
        let cap = self.inner.caps.get(&provider).copied().unwrap_or(0);
        let notify = self
            .inner
            .lock()
            .notifies
            .entry(provider)
            .or_insert_with(|| Arc::new(Notify::new()))
            .clone();
        loop {
            let notified = notify.notified();
            tokio::pin!(notified);
            {
                let mut st = self.inner.lock();
                let capped = pick(&|id| cap == 0 || st.counts.get(id).copied().unwrap_or(0) < cap);
                if let Some(id) = capped {
                    return Some(self.take(&mut st, id, provider));
                }
                // All usable accounts (if any) are at cap.
                let uncapped = pick(&|_| true)?;
                if budget.expired() {
                    return Some(self.take(&mut st, uncapped, provider));
                }
                // Register for wake-ups while still holding the lock: a release either
                // happened before this check (we saw the free slot) or notifies us.
                notified.as_mut().enable();
            }
            let _ = tokio::time::timeout(budget.remaining().min(RECHECK), notified).await;
        }
    }

    fn take(&self, st: &mut State, id: AccountId, provider: ProviderId) -> Reservation {
        *st.counts.entry(id.clone()).or_insert(0) += 1;
        Reservation {
            inner: self.inner.clone(),
            id,
            provider,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(s: &str) -> AccountId {
        s.parse().unwrap()
    }

    /// Pick the first candidate (in order) that has room.
    fn first_with_room(
        cands: &[&str],
    ) -> impl Fn(&dyn Fn(&AccountId) -> bool) -> Option<AccountId> {
        let cands: Vec<AccountId> = cands.iter().map(|s| id(s)).collect();
        move |has_room| cands.iter().find(|c| has_room(c)).cloned()
    }

    fn pool(cap: u32) -> Arc<Reservations> {
        Arc::new(Reservations::new(HashMap::from([(
            ProviderId::OpenAi,
            cap,
        )])))
    }

    const WAIT: Duration = Duration::from_secs(90);

    #[tokio::test(start_paused = true)]
    async fn reserve_skips_accounts_at_cap() {
        let r = pool(1);
        let budget = WaitBudget::new(WAIT);
        let pick = first_with_room(&["a", "b"]);
        let r1 = r.reserve(&pick, ProviderId::OpenAi, &budget).await.unwrap();
        assert_eq!(r1.account(), &id("a"));
        let r2 = r.reserve(&pick, ProviderId::OpenAi, &budget).await.unwrap();
        assert_eq!(r2.account(), &id("b"));
        assert_eq!((r.in_flight(&id("a")), r.in_flight(&id("b"))), (1, 1));
        assert!(!budget.expired());
    }

    #[tokio::test(start_paused = true)]
    async fn unlimited_cap_never_blocks_and_missing_provider_cap_is_unlimited() {
        let r = pool(0);
        let budget = WaitBudget::new(WAIT);
        let pick = first_with_room(&["a"]);
        let held: Vec<_> = futures_collect(&r, &pick, &budget, 10).await;
        assert_eq!(r.in_flight(&id("a")), 10);
        assert!(!budget.expired());
        drop(held);
        // Anthropic has no cap entry at all: treated as unlimited.
        let held = r.reserve(&pick, ProviderId::Anthropic, &budget).await;
        let held2 = r.reserve(&pick, ProviderId::Anthropic, &budget).await;
        assert!(held.is_some() && held2.is_some());
        assert!(!budget.expired());
    }

    async fn futures_collect(
        r: &Arc<Reservations>,
        pick: &impl Fn(&dyn Fn(&AccountId) -> bool) -> Option<AccountId>,
        budget: &WaitBudget,
        n: usize,
    ) -> Vec<Reservation> {
        let mut v = Vec::new();
        for _ in 0..n {
            v.push(r.reserve(pick, ProviderId::OpenAi, budget).await.unwrap());
        }
        v
    }

    #[tokio::test(start_paused = true)]
    async fn returns_none_only_when_no_usable_account() {
        let r = pool(1);
        let budget = WaitBudget::new(WAIT);
        let none = |_: &dyn Fn(&AccountId) -> bool| None;
        assert!(r.reserve(none, ProviderId::OpenAi, &budget).await.is_none());
        // Returned immediately, without burning the budget.
        assert!(!budget.expired());
        assert_eq!(budget.remaining(), WAIT);
    }

    #[tokio::test(start_paused = true)]
    async fn waits_for_release_then_proceeds() {
        let r = pool(1);
        let budget = Arc::new(WaitBudget::new(WAIT));
        let pick = first_with_room(&["a"]);
        let held = r.reserve(&pick, ProviderId::OpenAi, &budget).await.unwrap();

        let waiter = {
            let (r, budget) = (r.clone(), budget.clone());
            tokio::spawn(async move {
                let pick = first_with_room(&["a"]);
                r.reserve(&pick, ProviderId::OpenAi, &budget).await
            })
        };
        tokio::time::sleep(Duration::from_secs(5)).await;
        assert!(!waiter.is_finished());
        assert_eq!(r.in_flight(&id("a")), 1);

        let released = Instant::now();
        drop(held);
        let got = waiter.await.unwrap().unwrap();
        assert!(
            released.elapsed() < RECHECK,
            "woken by the release, not by the safety-net poll"
        );
        assert_eq!(got.account(), &id("a"));
        assert_eq!(r.in_flight(&id("a")), 1, "cap respected, not exceeded");
        assert!(!budget.expired(), "proceeded well before the budget");
    }

    #[tokio::test(start_paused = true)]
    async fn after_budget_exceeds_cap_rather_than_error() {
        let r = pool(1);
        let budget = WaitBudget::new(WAIT);
        let pick = first_with_room(&["a"]);
        let _held = r.reserve(&pick, ProviderId::OpenAi, &budget).await.unwrap();

        let start = Instant::now();
        let over = r.reserve(&pick, ProviderId::OpenAi, &budget).await.unwrap();
        assert!(start.elapsed() >= WAIT, "waited the whole budget first");
        assert!(budget.expired());
        assert_eq!(over.account(), &id("a"));
        assert_eq!(r.in_flight(&id("a")), 2, "cap exceeded as a last resort");

        // Budget is shared: the next request does not wait again.
        let again = r.reserve(&pick, ProviderId::OpenAi, &budget).await.unwrap();
        assert_eq!(again.account(), &id("a"));
        assert_eq!(r.in_flight(&id("a")), 3);
    }

    #[tokio::test(start_paused = true)]
    async fn exceeding_cap_still_honours_the_pick_filter() {
        let r = pool(1);
        let budget = WaitBudget::new(WAIT);
        let a = first_with_room(&["a"]);
        let _held = r.reserve(&a, ProviderId::OpenAi, &budget).await.unwrap();
        // The only account at cap is "a"; the filter (e.g. excluded/tried) allows nothing else.
        let only_other = first_with_room(&[]);
        let start = Instant::now();
        assert!(r
            .reserve(&only_other, ProviderId::OpenAi, &budget)
            .await
            .is_none());
        assert!(start.elapsed() < Duration::from_secs(1));
    }

    #[tokio::test(start_paused = true)]
    async fn one_release_wakes_one_waiter() {
        let r = pool(1);
        let budget = Arc::new(WaitBudget::new(WAIT));
        let pick = first_with_room(&["a"]);
        let held = r.reserve(&pick, ProviderId::OpenAi, &budget).await.unwrap();

        let spawn_waiter = || {
            let (r, budget) = (r.clone(), budget.clone());
            tokio::spawn(async move {
                let pick = first_with_room(&["a"]);
                r.reserve(&pick, ProviderId::OpenAi, &budget).await
            })
        };
        let (w1, w2) = (spawn_waiter(), spawn_waiter());
        tokio::time::sleep(Duration::from_secs(1)).await;
        assert!(!w1.is_finished() && !w2.is_finished());

        drop(held);
        tokio::time::sleep(Duration::from_secs(10)).await;
        assert!(!budget.expired());
        let done = [&w1, &w2].iter().filter(|w| w.is_finished()).count();
        assert_eq!(done, 1, "exactly one waiter takes the freed slot");
        assert_eq!(r.in_flight(&id("a")), 1);

        // Releasing the winner's reservation lets the other proceed.
        let (winner, loser) = if w1.is_finished() { (w1, w2) } else { (w2, w1) };
        drop(winner.await.unwrap().unwrap());
        let got = loser.await.unwrap().unwrap();
        assert_eq!(got.account(), &id("a"));
        assert_eq!(r.in_flight(&id("a")), 1);
        assert!(!budget.expired());
    }

    #[tokio::test(start_paused = true)]
    async fn drop_releases_once() {
        let r = pool(0);
        let budget = WaitBudget::new(WAIT);
        let pick = first_with_room(&["a"]);
        let r1 = r.reserve(&pick, ProviderId::OpenAi, &budget).await.unwrap();
        let r2 = r.reserve(&pick, ProviderId::OpenAi, &budget).await.unwrap();
        assert_eq!(r.in_flight(&id("a")), 2);
        drop(r1);
        assert_eq!(r.in_flight(&id("a")), 1, "one drop frees exactly one slot");
        // Dropping from another task works too (Send + 'static).
        tokio::spawn(async move { drop(r2) }).await.unwrap();
        assert_eq!(r.in_flight(&id("a")), 0);
    }

    #[test]
    fn reservations_are_send_and_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        fn assert_send<T: Send>() {}
        assert_send_sync::<Reservations>();
        assert_send::<Reservation>();
    }
}
