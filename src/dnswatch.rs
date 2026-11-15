//! Per-host DNS health, as seen by the upstream client's sends.
//!
//! A failed DNS lookup says nothing about the account that sent the request: every request to
//! that host fails the same way. So it is tracked per **host** (the API and token hosts differ
//! per provider, and pooled HTTP/2 connections keep working for a host whose lookups fail):
//!
//! - after a lookup of a host fails, the host is *half-open*: sends to it fail at once, without a
//!   network call (the failover loop skips the accounts behind it), except one real attempt at
//!   most every [`DNS_PROBE_EVERY`]; and the full skip never outlasts [`DNS_SKIP_CAP`] after the
//!   last failed lookup. So a resolver that is down for a moment (wake from sleep, a Wi-Fi
//!   change) costs a second or two, not a long refusal;
//! - any answer from the host clears its entry at once;
//! - one `WARN` when a host's lookups start failing and one when it answers again (per host, so
//!   other hosts answering in between never flap it; a host is never logged down twice without
//!   an answer in between);
//! - a host failing continuously for [`DNS_PERSISTENT_AFTER`] is an outage worth showing
//!   (`GET /api/v1/status` `network.dns_down`, `doctor`, the dashboard banner), without blaming
//!   any account.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use time::OffsetDateTime;

/// While a host is DNS-down, one real attempt goes through at most this often.
pub const DNS_PROBE_EVERY: Duration = Duration::from_secs(2);
/// Sends to a DNS-down host fail fast for at most this long after its last failed lookup.
pub const DNS_SKIP_CAP: Duration = Duration::from_secs(5);
/// A host whose lookups have failed for this long (and still fail) is shown as an outage.
pub const DNS_PERSISTENT_AFTER: Duration = Duration::from_secs(10 * 60);
/// A failing host nothing has tried for this long is not shown as an outage (its state is
/// unknown); its next failure starts a new outage period, without a second `WARN`.
const DNS_FORGET_AFTER: Duration = Duration::from_secs(15 * 60);

/// One host whose lookups fail.
#[derive(Debug, Clone)]
struct Down {
    since: Instant,
    since_wall: OffsetDateTime,
    last_fail: Instant,
    /// The last real attempt let through (a failed lookup counts as one).
    last_attempt: Instant,
}

/// A host shown as a DNS outage.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DnsOutage {
    pub host: String,
    /// The first failed lookup of the current outage.
    pub since: OffsetDateTime,
}

/// The hosts whose DNS lookups fail now.
#[derive(Debug)]
pub struct DnsWatch {
    hosts: Mutex<HashMap<String, Down>>,
    probe_every: Duration,
    skip_cap: Duration,
    persistent_after: Duration,
}

impl Default for DnsWatch {
    fn default() -> Self {
        DnsWatch::new(DNS_PROBE_EVERY, DNS_SKIP_CAP, DNS_PERSISTENT_AFTER)
    }
}

impl DnsWatch {
    pub fn new(probe_every: Duration, skip_cap: Duration, persistent_after: Duration) -> DnsWatch {
        DnsWatch {
            hosts: Mutex::default(),
            probe_every,
            skip_cap,
            persistent_after,
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Down>> {
        self.hosts.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Whether `d` fails a send fast at `now` (no probe due, still within the cap).
    fn skips(&self, d: &Down, now: Instant) -> bool {
        now.duration_since(d.last_fail) < self.skip_cap
            && now.duration_since(d.last_attempt) < self.probe_every
    }

    /// A real lookup of `host` failed.
    pub fn failed(&self, host: &str) {
        let now = Instant::now();
        let mut hosts = self.lock();
        match hosts.get_mut(host) {
            Some(d) => {
                if now.duration_since(d.last_fail) >= DNS_FORGET_AFTER {
                    // Untried for long: a new outage period (still unanswered, so no new WARN).
                    d.since = now;
                    d.since_wall = OffsetDateTime::now_utc();
                }
                d.last_fail = now;
                d.last_attempt = now;
            }
            None => {
                hosts.insert(
                    host.to_string(),
                    Down {
                        since: now,
                        since_wall: OffsetDateTime::now_utc(),
                        last_fail: now,
                        last_attempt: now,
                    },
                );
                tracing::warn!(
                    host,
                    "DNS lookup failed: requests to this host fail fast (retried every few \
                     seconds) and fail over to accounts on other hosts; no account is blamed"
                );
            }
        }
    }

    /// `host` answered: its lookups work.
    pub fn resolved(&self, host: &str) {
        let mut hosts = self.lock();
        if hosts.is_empty() {
            return;
        }
        if let Some(d) = hosts.remove(host) {
            tracing::warn!(
                host,
                down_secs = d.since.elapsed().as_secs(),
                "DNS lookups work again"
            );
        }
    }

    /// Whether a send to `host` would fail fast now. Read-only: it never takes the probe slot
    /// (the failover loop asks before sending; [`DnsWatch::admit`] decides the send itself).
    pub fn is_down(&self, host: &str) -> bool {
        let now = Instant::now();
        self.lock().get(host).is_some_and(|d| self.skips(d, now))
    }

    /// Whether a send to `host` may really go out now. For a DNS-down host, only the first send
    /// once a probe is due (or past the cap) is let through, and it takes the slot: concurrent
    /// sends keep failing fast until it fails again or the host answers.
    pub fn admit(&self, host: &str) -> bool {
        let now = Instant::now();
        let mut hosts = self.lock();
        let Some(d) = hosts.get_mut(host) else {
            return true;
        };
        if self.skips(d, now) {
            return false;
        }
        d.last_attempt = now;
        true
    }

    /// Hosts failing continuously for at least the persistent threshold, by host name.
    pub fn outages(&self) -> Vec<DnsOutage> {
        let mut out: Vec<DnsOutage> = self
            .lock()
            .iter()
            .filter(|(_, d)| {
                d.since.elapsed() >= self.persistent_after
                    && d.last_fail.elapsed() < DNS_FORGET_AFTER
            })
            .map(|(h, d)| DnsOutage {
                host: h.clone(),
                since: d.since_wall,
            })
            .collect();
        out.sort_by(|a, b| a.host.cmp(&b.host));
        out
    }
}

/// The host of `url` (`None` when it does not parse or has none).
pub fn host_of(url: &str) -> Option<String> {
    reqwest::Url::parse(url)
        .ok()?
        .host_str()
        .map(str::to_ascii_lowercase)
}

#[cfg(test)]
mod tests {
    use super::*;

    const MS: fn(u64) -> Duration = Duration::from_millis;

    #[test]
    fn half_open_lets_one_probe_through_per_interval() {
        let w = DnsWatch::new(MS(60), Duration::from_secs(60), Duration::from_secs(600));
        assert!(!w.is_down("a.example") && w.admit("a.example"));
        w.failed("a.example");
        assert!(w.is_down("a.example"));
        assert!(!w.admit("a.example"), "fails fast right after the failure");
        assert!(
            !w.is_down("b.example") && w.admit("b.example"),
            "another host"
        );
        std::thread::sleep(MS(90));
        // The probe is due: `is_down` does not take it, the first `admit` does.
        assert!(!w.is_down("a.example"));
        assert!(w.admit("a.example"), "one real attempt");
        assert!(!w.admit("a.example"), "concurrent sends keep failing fast");
        assert!(w.is_down("a.example"));
        // The probe answered: cleared at once.
        w.resolved("a.example");
        assert!(!w.is_down("a.example") && w.admit("a.example") && w.admit("a.example"));
    }

    #[test]
    fn the_full_skip_is_capped() {
        // Probes rare (10 s) but the cap short: past it, every send goes out.
        let w = DnsWatch::new(Duration::from_secs(10), MS(50), Duration::from_secs(600));
        w.failed("a.example");
        assert!(!w.admit("a.example"));
        std::thread::sleep(MS(80));
        assert!(w.admit("a.example") && w.admit("a.example"));
        assert!(!w.is_down("a.example"));
    }

    #[test]
    fn default_timing_is_short() {
        assert_eq!(DNS_PROBE_EVERY, Duration::from_secs(2));
        assert_eq!(DNS_SKIP_CAP, Duration::from_secs(5));
    }

    #[test]
    fn persistent_outages_are_listed_and_cleared_by_an_answer() {
        let w = DnsWatch::new(DNS_PROBE_EVERY, DNS_SKIP_CAP, Duration::ZERO);
        w.failed("auth.example");
        w.failed("api.example");
        let hosts: Vec<String> = w.outages().into_iter().map(|o| o.host).collect();
        assert_eq!(hosts, ["api.example", "auth.example"]);
        w.resolved("api.example");
        let hosts: Vec<String> = w.outages().into_iter().map(|o| o.host).collect();
        assert_eq!(hosts, ["auth.example"]);
        // Not yet persistent with the default threshold.
        let w = DnsWatch::default();
        w.failed("auth.example");
        assert!(w.outages().is_empty());
    }

    #[test]
    fn host_of_urls() {
        assert_eq!(
            host_of("https://API.anthropic.com/v1").as_deref(),
            Some("api.anthropic.com")
        );
        assert_eq!(
            host_of("http://127.0.0.1:9/x").as_deref(),
            Some("127.0.0.1")
        );
        assert_eq!(host_of("not a url"), None);
    }
}
