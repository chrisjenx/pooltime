//! Per-host DNS health, as seen by the upstream client's sends.
//!
//! A failed DNS lookup says nothing about the account that sent the request: every request to
//! that host fails the same way. So it is tracked per **host** (the API and token hosts differ
//! per provider, and pooled HTTP/2 connections keep working for a host whose lookups fail):
//!
//! - after a lookup of a host fails, sends to it fail at once, without a network call, for
//!   [`DNS_SKIP_TTL`] (the failover loop skips the accounts behind it); a later send tries again;
//! - any answer from the host clears its entry;
//! - one `WARN` when a host's lookups start failing and one when it answers again (per host, so
//!   other hosts answering in between never flap it);
//! - a host failing continuously for [`DNS_PERSISTENT_AFTER`] is an outage worth showing
//!   (`GET /api/v1/status` `network.dns_down`, `doctor`, the dashboard banner), without blaming
//!   any account.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use time::OffsetDateTime;

/// After a failed lookup, sends to the host fail fast for this long.
pub const DNS_SKIP_TTL: Duration = Duration::from_secs(30);
/// A host whose lookups have failed for this long (and still fail) is shown as an outage.
pub const DNS_PERSISTENT_AFTER: Duration = Duration::from_secs(10 * 60);
/// A failing host nothing has tried for this long is forgotten: its state is unknown.
const DNS_FORGET_AFTER: Duration = Duration::from_secs(15 * 60);

/// One host whose lookups fail.
#[derive(Debug, Clone)]
struct Down {
    since: Instant,
    since_wall: OffsetDateTime,
    last_fail: Instant,
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
    skip_ttl: Duration,
    persistent_after: Duration,
}

impl Default for DnsWatch {
    fn default() -> Self {
        DnsWatch::new(DNS_SKIP_TTL, DNS_PERSISTENT_AFTER)
    }
}

impl DnsWatch {
    pub fn new(skip_ttl: Duration, persistent_after: Duration) -> DnsWatch {
        DnsWatch {
            hosts: Mutex::default(),
            skip_ttl,
            persistent_after,
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Down>> {
        self.hosts.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// A real lookup of `host` failed.
    pub fn failed(&self, host: &str) {
        let now = Instant::now();
        let mut hosts = self.lock();
        match hosts.get_mut(host) {
            Some(d) if now.duration_since(d.last_fail) < DNS_FORGET_AFTER => d.last_fail = now,
            _ => {
                hosts.insert(
                    host.to_string(),
                    Down {
                        since: now,
                        since_wall: OffsetDateTime::now_utc(),
                        last_fail: now,
                    },
                );
                tracing::warn!(
                    host,
                    "DNS lookup failed: requests to this host fail fast and fail over to \
                     accounts on other hosts; no account is blamed"
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

    /// Whether a send to `host` should fail fast (a lookup failed within the skip TTL).
    pub fn is_down(&self, host: &str) -> bool {
        self.lock()
            .get(host)
            .is_some_and(|d| d.last_fail.elapsed() < self.skip_ttl)
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

    #[test]
    fn fails_fast_per_host_until_ttl_or_success() {
        let w = DnsWatch::new(Duration::from_millis(50), Duration::from_secs(600));
        assert!(!w.is_down("a.example"));
        w.failed("a.example");
        assert!(w.is_down("a.example"));
        assert!(!w.is_down("b.example"), "another host is unaffected");
        std::thread::sleep(Duration::from_millis(80));
        assert!(!w.is_down("a.example"), "tried again after the TTL");
        w.failed("a.example");
        assert!(w.is_down("a.example"));
        w.resolved("a.example");
        assert!(!w.is_down("a.example"));
    }

    #[test]
    fn persistent_outages_are_listed_and_cleared_by_an_answer() {
        let w = DnsWatch::new(DNS_SKIP_TTL, Duration::ZERO);
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
