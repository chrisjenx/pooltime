//! Context carry (INV §4): the previous Codex turn's context size seeds the next turn's
//! `message_start.usage.input_tokens`, so Claude Code's context meter does not read 0 while a
//! Codex response streams (Codex reports usage only at the terminal event).
//!
//! Ported from bro `openai-codex.ts` (`ContextUsageCache`). An LRU of [`ContextCarry::CAPACITY`]
//! entries keyed by `(session hash, route id)`. The session is the request's
//! `metadata.user_id`; it is hashed on the way in (`hash_session("openai:<user_id>")`, the same
//! scheme as sticky sessions) so raw user ids are never held. No session (absent or empty) never
//! carries. The value is `input + cached + output` of the last *successful* turn with terminal
//! input usage ([`CodexToAnthropic::context_tokens`]); `completed` and `incomplete` terminals
//! both count, failed / cancelled / truncated streams never update it (bro parity).
//!
//! The handle is `Clone` (shared `Arc<Mutex<_>>`); in-memory only.
//!
//! [`CodexToAnthropic::context_tokens`]: super::CodexToAnthropic::context_tokens

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex, MutexGuard};

use crate::logging::hash_session;

type Key = (String, String);

#[derive(Debug, Default)]
struct Lru {
    capacity: usize,
    tick: u64,
    /// key -> (tokens, last-use tick)
    values: HashMap<Key, (u64, u64)>,
    /// last-use tick -> key (oldest first)
    order: BTreeMap<u64, Key>,
}

impl Lru {
    fn touch(&mut self, key: &Key) -> Option<u64> {
        let (value, old_tick) = *self.values.get(key)?;
        self.order.remove(&old_tick);
        self.tick += 1;
        self.order.insert(self.tick, key.clone());
        self.values.insert(key.clone(), (value, self.tick));
        Some(value)
    }

    fn insert(&mut self, key: Key, value: u64) {
        if let Some((_, old_tick)) = self.values.remove(&key) {
            self.order.remove(&old_tick);
        }
        self.tick += 1;
        self.order.insert(self.tick, key.clone());
        self.values.insert(key, (value, self.tick));
        while self.values.len() > self.capacity {
            let Some((_, oldest)) = self.order.pop_first() else {
                break;
            };
            self.values.remove(&oldest);
        }
    }
}

/// Shared LRU of carried context sizes. See the module docs.
#[derive(Debug, Clone)]
pub struct ContextCarry {
    inner: Arc<Mutex<Lru>>,
}

impl Default for ContextCarry {
    fn default() -> Self {
        Self::new()
    }
}

impl ContextCarry {
    /// bro `MAX_CONTEXT_USAGE_ENTRIES`.
    pub const CAPACITY: usize = 1000;

    pub fn new() -> Self {
        Self::with_capacity(Self::CAPACITY)
    }

    pub fn with_capacity(capacity: usize) -> Self {
        ContextCarry {
            inner: Arc::new(Mutex::new(Lru {
                capacity,
                ..Lru::default()
            })),
        }
    }

    /// The carried context for `(user_id, route_id)` (marks it recently used). `None` when
    /// nothing is carried or there is no session.
    pub fn seed(&self, user_id: Option<&str>, route_id: &str) -> Option<u64> {
        let key = key(user_id, route_id)?;
        self.lock().touch(&key)
    }

    /// Replace the carried context for `(user_id, route_id)`. No-op without a session.
    pub fn record(&self, user_id: Option<&str>, route_id: &str, tokens: u64) {
        if let Some(key) = key(user_id, route_id) {
            self.lock().insert(key, tokens);
        }
    }

    pub fn len(&self) -> usize {
        self.lock().values.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn lock(&self) -> MutexGuard<'_, Lru> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }
}

fn key(user_id: Option<&str>, route_id: &str) -> Option<Key> {
    let user_id = user_id.filter(|u| !u.is_empty())?;
    Some((
        hash_session(&format!("openai:{user_id}")),
        route_id.to_string(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_hold_the_session_hash_never_the_raw_user_id() {
        let carry = ContextCarry::new();
        carry.record(Some("user-secret-id"), "gpt", 5);
        let inner = carry.lock();
        let (k, _) = inner.values.iter().next().unwrap();
        assert_eq!(k.0, hash_session("openai:user-secret-id"));
        assert!(!k.0.contains("user-secret-id"));
        assert_eq!(k.1, "gpt");
        assert!(!format!("{inner:?}").contains("user-secret-id"));
    }

    #[test]
    fn capacity_bound_holds_at_default_size() {
        let carry = ContextCarry::new();
        for i in 0..(ContextCarry::CAPACITY + 50) {
            carry.record(Some(&format!("u{i}")), "r", i as u64);
        }
        assert_eq!(carry.len(), ContextCarry::CAPACITY);
        assert_eq!(carry.lock().order.len(), ContextCarry::CAPACITY);
        assert_eq!(carry.seed(Some("u0"), "r"), None);
        assert_eq!(carry.seed(Some("u1049"), "r"), Some(1049));
    }
}
