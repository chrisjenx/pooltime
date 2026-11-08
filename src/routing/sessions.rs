//! Sticky sessions (INV §2): a client session (`metadata.user_id`) stays on the account
//! that served it, because switching accounts re-pays the prompt cache.
//!
//! Keys are `hash_session("<provider>:<user_id>")`, so the raw user id is never held or
//! persisted. State lives in memory and is written through to the `sessions` table with
//! [`Store::submit`] (ordered, fire-and-forget). Callers must still check that a pinned
//! account is usable and in the active tier before honoring a pin.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use time::OffsetDateTime;

use crate::accounts::AccountId;
use crate::logging::hash_session;
use crate::providers::ProviderId;
use crate::store::Store;

const PROVIDERS: [ProviderId; 2] = [ProviderId::Anthropic, ProviderId::OpenAi];

#[derive(Debug, Clone)]
struct Entry {
    account: AccountId,
    provider: ProviderId,
    /// Unix seconds (matches the table).
    last_seen: i64,
}

type SessionRow = (String, String, String, i64);

pub struct Sessions {
    store: Arc<Store>,
    idle_secs: i64,
    map: Mutex<HashMap<String, Entry>>,
}

fn key(provider: ProviderId, user_id: &str) -> String {
    hash_session(&format!("{provider}:{user_id}"))
}

impl Sessions {
    /// Load persisted pins. Rows with an unknown provider or invalid account id are skipped.
    /// Idle expiry is applied lazily by [`Sessions::pinned`] and [`Sessions::prune`].
    pub fn load(store: Arc<Store>, idle: Duration) -> Self {
        let rows: Vec<SessionRow> = store
            .read(|c| {
                let mut st =
                    c.prepare("SELECT key_hash, account, provider, last_seen FROM sessions")?;
                let rows =
                    st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?;
                Ok(rows.collect::<Result<_, _>>()?)
            })
            .unwrap_or_else(|e| {
                tracing::warn!(error = %format!("{e:#}"), "could not load sticky sessions");
                Vec::new()
            });
        let map = rows
            .into_iter()
            .filter_map(|(k, account, provider, last_seen)| {
                Some((
                    k,
                    Entry {
                        account: account.parse().ok()?,
                        provider: provider.parse().ok()?,
                        last_seen,
                    },
                ))
            })
            .collect();
        Sessions {
            store,
            idle_secs: i64::try_from(idle.as_secs()).unwrap_or(i64::MAX),
            map: Mutex::new(map),
        }
    }

    fn lock(&self) -> MutexGuard<'_, HashMap<String, Entry>> {
        self.map.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn expired(&self, e: &Entry, now: OffsetDateTime) -> bool {
        now.unix_timestamp().saturating_sub(e.last_seen) > self.idle_secs
    }

    fn write<F>(&self, f: F)
    where
        F: FnOnce(&mut rusqlite::Connection) -> anyhow::Result<()> + Send + 'static,
    {
        if let Err(e) = self.store.submit(f) {
            tracing::warn!(error = %format!("{e:#}"), "could not queue sticky session write");
        }
    }

    /// The account this session is pinned to, unless the pin has been idle longer than
    /// `session_idle`. Read-only: call [`Sessions::set`] to refresh `last_seen`.
    pub fn pinned(
        &self,
        provider: ProviderId,
        user_id: &str,
        now: OffsetDateTime,
    ) -> Option<AccountId> {
        let map = self.lock();
        let e = map.get(&key(provider, user_id))?;
        (!self.expired(e, now)).then(|| e.account.clone())
    }

    /// Pin (or re-pin / refresh) this session to `account`.
    pub fn set(
        &self,
        provider: ProviderId,
        user_id: &str,
        account: &AccountId,
        now: OffsetDateTime,
    ) {
        let k = key(provider, user_id);
        let last_seen = now.unix_timestamp();
        self.lock().insert(
            k.clone(),
            Entry {
                account: account.clone(),
                provider,
                last_seen,
            },
        );
        let account = account.to_string();
        self.write(move |c| {
            c.execute(
                "INSERT INTO sessions(key_hash, account, provider, last_seen) VALUES (?1,?2,?3,?4)
                 ON CONFLICT(key_hash) DO UPDATE SET
                    account = excluded.account,
                    provider = excluded.provider,
                    last_seen = excluded.last_seen",
                rusqlite::params![k, account, provider.as_str(), last_seen],
            )?;
            Ok(())
        });
    }

    /// Drop every pin for `account` (memory and database).
    pub fn evict_account(&self, account: &AccountId) {
        self.lock().retain(|_, e| &e.account != account);
        let account = account.to_string();
        self.write(move |c| {
            c.execute("DELETE FROM sessions WHERE account = ?1", [account])?;
            Ok(())
        });
    }

    /// Remove pins idle longer than `session_idle` (memory and database).
    pub fn prune(&self, now: OffsetDateTime) {
        self.lock().retain(|_, e| !self.expired(e, now));
        let cutoff = now.unix_timestamp().saturating_sub(self.idle_secs);
        self.write(move |c| {
            c.execute("DELETE FROM sessions WHERE last_seen < ?1", [cutoff])?;
            Ok(())
        });
    }

    /// The provider this user was most recently pinned under, across providers. Does not
    /// consider idle expiry (pruning handles that).
    pub fn last_provider(&self, user_id: &str) -> Option<ProviderId> {
        let map = self.lock();
        PROVIDERS
            .iter()
            .filter_map(|p| {
                map.get(&key(*p, user_id))
                    .map(|e| (e.last_seen, e.provider))
            })
            .max_by_key(|(seen, _)| *seen)
            .map(|(_, p)| p)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BASE: i64 = 1_800_000_000;
    const IDLE: Duration = Duration::from_secs(30 * 60);

    fn at(secs: i64) -> OffsetDateTime {
        OffsetDateTime::from_unix_timestamp(BASE + secs).unwrap()
    }

    fn acct(s: &str) -> AccountId {
        s.parse().unwrap()
    }

    fn open() -> (tempfile::TempDir, Arc<Store>) {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::open(&dir.path().join("pooltime.db")).unwrap());
        (dir, store)
    }

    async fn flush(store: &Store) {
        store.exec(|_| Ok(())).await.unwrap();
    }

    fn row_count(store: &Store) -> i64 {
        store
            .read(|c| Ok(c.query_row("SELECT count(*) FROM sessions", [], |r| r.get(0))?))
            .unwrap()
    }

    #[tokio::test]
    async fn pin_roundtrip_and_persisted() {
        let (_d, store) = open();
        let s = Sessions::load(store.clone(), IDLE);
        assert_eq!(s.pinned(ProviderId::Anthropic, "u1", at(0)), None);
        s.set(ProviderId::Anthropic, "u1", &acct("a1"), at(0));
        assert_eq!(
            s.pinned(ProviderId::Anthropic, "u1", at(10)),
            Some(acct("a1"))
        );
        // keyed by provider too
        assert_eq!(s.pinned(ProviderId::OpenAi, "u1", at(10)), None);
        // re-pin moves it
        s.set(ProviderId::Anthropic, "u1", &acct("a2"), at(20));
        assert_eq!(
            s.pinned(ProviderId::Anthropic, "u1", at(30)),
            Some(acct("a2"))
        );

        flush(&store).await;
        let reloaded = Sessions::load(store.clone(), IDLE);
        assert_eq!(
            reloaded.pinned(ProviderId::Anthropic, "u1", at(30)),
            Some(acct("a2"))
        );
        assert_eq!(row_count(&store), 1);
    }

    #[tokio::test]
    async fn idle_entries_pruned() {
        let (_d, store) = open();
        let s = Sessions::load(store.clone(), IDLE);
        s.set(ProviderId::Anthropic, "old", &acct("a1"), at(0));
        s.set(ProviderId::Anthropic, "fresh", &acct("a1"), at(1500));
        let now = at(1801);
        // idle > 30m: not honored even before pruning
        assert_eq!(s.pinned(ProviderId::Anthropic, "old", now), None);
        assert_eq!(
            s.pinned(ProviderId::Anthropic, "fresh", now),
            Some(acct("a1"))
        );
        // exactly at the limit is still honored
        assert_eq!(
            s.pinned(ProviderId::Anthropic, "old", at(1800)),
            Some(acct("a1"))
        );

        s.prune(now);
        flush(&store).await;
        assert_eq!(row_count(&store), 1);
        let reloaded = Sessions::load(store.clone(), IDLE);
        assert_eq!(reloaded.pinned(ProviderId::Anthropic, "old", at(0)), None);
        assert_eq!(
            reloaded.pinned(ProviderId::Anthropic, "fresh", now),
            Some(acct("a1"))
        );
    }

    #[tokio::test]
    async fn evict_account_removes_all_its_pins() {
        let (_d, store) = open();
        let s = Sessions::load(store.clone(), IDLE);
        s.set(ProviderId::Anthropic, "u1", &acct("a1"), at(0));
        s.set(ProviderId::Anthropic, "u2", &acct("a1"), at(0));
        s.set(ProviderId::OpenAi, "u3", &acct("a1"), at(0));
        s.set(ProviderId::Anthropic, "u4", &acct("a2"), at(0));
        s.evict_account(&acct("a1"));
        assert_eq!(s.pinned(ProviderId::Anthropic, "u1", at(1)), None);
        assert_eq!(s.pinned(ProviderId::Anthropic, "u2", at(1)), None);
        assert_eq!(s.pinned(ProviderId::OpenAi, "u3", at(1)), None);
        assert_eq!(
            s.pinned(ProviderId::Anthropic, "u4", at(1)),
            Some(acct("a2"))
        );
        flush(&store).await;
        assert_eq!(row_count(&store), 1);
        let reloaded = Sessions::load(store, IDLE);
        assert_eq!(
            reloaded.pinned(ProviderId::Anthropic, "u4", at(1)),
            Some(acct("a2"))
        );
        assert_eq!(reloaded.pinned(ProviderId::Anthropic, "u1", at(1)), None);
    }

    #[tokio::test]
    async fn raw_user_id_never_stored() {
        let (_d, store) = open();
        let s = Sessions::load(store.clone(), IDLE);
        let user = "user_SECRET-raw-id_0123456789";
        s.set(ProviderId::Anthropic, user, &acct("a1"), at(0));
        flush(&store).await;
        let rows: Vec<(String, String, String, i64)> = store
            .read(|c| {
                let mut st =
                    c.prepare("SELECT key_hash, account, provider, last_seen FROM sessions")?;
                let rows =
                    st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?;
                Ok(rows.collect::<Result<_, _>>()?)
            })
            .unwrap();
        assert_eq!(rows.len(), 1);
        for (k, a, p, t) in &rows {
            for field in [k.clone(), a.clone(), p.clone(), t.to_string()] {
                assert!(!field.contains(user), "raw user id leaked: {field}");
            }
        }
        assert_eq!(rows[0].0, hash_session(&format!("anthropic:{user}")));
    }

    #[test]
    fn last_provider_is_most_recent_pin() {
        let (_d, store) = open();
        let s = Sessions::load(store, IDLE);
        assert_eq!(s.last_provider("u"), None);
        s.set(ProviderId::Anthropic, "u", &acct("a1"), at(0));
        assert_eq!(s.last_provider("u"), Some(ProviderId::Anthropic));
        s.set(ProviderId::OpenAi, "u", &acct("o1"), at(5));
        assert_eq!(s.last_provider("u"), Some(ProviderId::OpenAi));
        s.set(ProviderId::Anthropic, "u", &acct("a1"), at(9));
        assert_eq!(s.last_provider("u"), Some(ProviderId::Anthropic));
        assert_eq!(s.last_provider("other"), None);
    }
}
