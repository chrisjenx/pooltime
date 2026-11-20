//! One-time unlock codes: `pooltime unlock` turns an open read-only dashboard tab into a managing
//! one without opening a new tab.
//!
//! The CLI (holding the management key) asks `POST /api/v1/session/code` for a code; the daemon
//! keeps at most one, in memory only. The user types it into the dashboard's Unlock field, which
//! sends it to `POST /api/v1/session` (`{"code": ...}`) and gets the usual session cookie back.
//!
//! A code is 8 characters from an alphabet without look-alikes (`0/O`, `1/I/L`, `U`), about 39
//! bits, shown as `XXXX-XXXX`. It works once, for [`CODE_TTL`], and [`MAX_FAILURES`] wrong
//! guesses burn it; minting a new one replaces the old. Matching ignores case, spaces and dashes,
//! and is constant-time.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use rand::RngExt;
use subtle::ConstantTimeEq;

/// How long a code stays valid.
pub const CODE_TTL: Duration = Duration::from_secs(5 * 60);
/// Wrong guesses that burn the live code.
pub const MAX_FAILURES: u8 = 5;

const ALPHABET: &[u8] = b"23456789ABCDEFGHJKMNPQRSTVWXYZ";
const LEN: usize = 8;

struct Pending {
    code: String,
    expires: Instant,
    failures: u8,
}

/// The live unlock code, if any.
#[derive(Default)]
pub struct UnlockCodes {
    inner: Mutex<Option<Pending>>,
}

impl UnlockCodes {
    /// A fresh code (replacing any live one), formatted `XXXX-XXXX`.
    pub fn mint(&self, now: Instant) -> String {
        let mut rng = rand::rng();
        let code: String = (0..LEN)
            .map(|_| ALPHABET[rng.random_range(0..ALPHABET.len())] as char)
            .collect();
        let shown = format!("{}-{}", &code[..4], &code[4..]);
        *self.inner.lock().unwrap_or_else(|e| e.into_inner()) = Some(Pending {
            code,
            expires: now + CODE_TTL,
            failures: 0,
        });
        shown
    }

    /// Whether `presented` is the live code. A match consumes it; an expired code is dropped;
    /// the [`MAX_FAILURES`]th wrong guess drops it too.
    pub fn redeem(&self, presented: &str, now: Instant) -> bool {
        let given: String = presented
            .chars()
            .filter(|c| !c.is_whitespace() && *c != '-')
            .map(|c| c.to_ascii_uppercase())
            .collect();
        let mut slot = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let Some(p) = slot.as_mut() else {
            return false;
        };
        if now >= p.expires {
            *slot = None;
            return false;
        }
        let ok =
            given.len() == p.code.len() && bool::from(given.as_bytes().ct_eq(p.code.as_bytes()));
        if ok {
            *slot = None;
            return true;
        }
        p.failures += 1;
        if p.failures >= MAX_FAILURES {
            *slot = None;
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bare(shown: &str) -> String {
        shown.replace('-', "")
    }

    #[test]
    fn a_code_is_formatted_and_from_the_alphabet() {
        let c = UnlockCodes::default().mint(Instant::now());
        assert_eq!(c.len(), 9);
        assert_eq!(&c[4..5], "-");
        assert!(bare(&c).bytes().all(|b| ALPHABET.contains(&b)), "{c}");
    }

    #[test]
    fn a_code_works_once_ignoring_case_spaces_and_dashes() {
        let u = UnlockCodes::default();
        let now = Instant::now();
        let c = u.mint(now);
        let typed = format!(" {} {} ", c[..4].to_lowercase(), &c[5..]);
        assert!(u.redeem(&typed, now));
        assert!(!u.redeem(&c, now), "single use");
    }

    #[test]
    fn an_expired_code_is_refused() {
        let u = UnlockCodes::default();
        let now = Instant::now();
        let c = u.mint(now);
        assert!(!u.redeem(&c, now + CODE_TTL));
        assert!(!u.redeem(&c, now), "dropped once expired");
    }

    #[test]
    fn wrong_guesses_burn_the_code() {
        let u = UnlockCodes::default();
        let now = Instant::now();
        let c = u.mint(now);
        for _ in 0..MAX_FAILURES - 1 {
            assert!(!u.redeem("2222-2222", now));
        }
        assert!(
            u.redeem(&c, now),
            "still live after fewer than MAX_FAILURES misses"
        );
        let c = u.mint(now);
        for _ in 0..MAX_FAILURES {
            assert!(!u.redeem("2222-2222", now));
        }
        assert!(!u.redeem(&c, now), "burned");
    }

    #[test]
    fn nothing_minted_nothing_redeemed() {
        let u = UnlockCodes::default();
        assert!(!u.redeem("", Instant::now()));
        assert!(!u.redeem("ABCD-EFGH", Instant::now()));
    }

    #[test]
    fn a_new_code_replaces_the_old() {
        let u = UnlockCodes::default();
        let now = Instant::now();
        let old = u.mint(now);
        let new = u.mint(now);
        if old != new {
            assert!(!u.redeem(&old, now));
        }
        assert!(u.redeem(&new, now));
    }
}
