//! Shared failure classification. These are persisted (inside `AccountState`) and
//! shown in the dashboard, so they carry only classified kinds, never upstream
//! messages or credentials.

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

/// Where a transport-level failure happened.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TransportKind {
    Dns,
    Connect,
    Tls,
    Reset,
    IdleTimeout,
    Deadline,
    Other,
}

/// Why an OAuth token refresh failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RefreshErrorKind {
    InvalidGrant,
    TokenReused,
    AccountOnHold,
    Network(TransportKind),
    Server(u16),
    Other,
}

/// Why an upstream attempt failed (one entry of a failover chain / `last_error`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailReason {
    Quota {
        #[serde(default, with = "time::serde::rfc3339::option")]
        reset: Option<OffsetDateTime>,
    },
    UpstreamRejected,
    Overloaded,
    AccessDenied,
    Billing,
    Transport(TransportKind),
    Refresh(RefreshErrorKind),
    Status(u16),
    /// The DNS lookup of the provider's API host failed: a fault of this machine's network (or
    /// of that one host), not of the account. Never recorded against an account; the provider's
    /// other accounts (same host) are skipped, the other provider (another host) may still serve.
    Network(TransportKind),
}

impl TransportKind {
    /// Stable snake_case name (matches the serde tag).
    pub fn as_str(self) -> &'static str {
        match self {
            TransportKind::Dns => "dns",
            TransportKind::Connect => "connect",
            TransportKind::Tls => "tls",
            TransportKind::Reset => "reset",
            TransportKind::IdleTimeout => "idle_timeout",
            TransportKind::Deadline => "deadline",
            TransportKind::Other => "other",
        }
    }

    /// A failed DNS lookup: it fails the same way for every request to that host, whichever
    /// account sends it, so it says nothing about the account.
    pub fn is_dns(self) -> bool {
        matches!(self, TransportKind::Dns)
    }
}

impl RefreshErrorKind {
    /// Stable detail string, e.g. `invalid_grant`, `network:dns`, `server:503`.
    pub fn detail(self) -> String {
        match self {
            RefreshErrorKind::InvalidGrant => "invalid_grant".into(),
            RefreshErrorKind::TokenReused => "token_reused".into(),
            RefreshErrorKind::AccountOnHold => "account_on_hold".into(),
            RefreshErrorKind::Network(k) => format!("network:{}", k.as_str()),
            RefreshErrorKind::Server(code) => format!("server:{code}"),
            RefreshErrorKind::Other => "other".into(),
        }
    }

    /// A refresh whose token-endpoint host did not resolve ([`TransportKind::is_dns`]).
    pub fn is_dns(self) -> bool {
        matches!(self, RefreshErrorKind::Network(k) if k.is_dns())
    }
}

impl FailReason {
    /// Static, low-cardinality label (metrics, log fields, dashboard grouping).
    /// Variants with a dynamic part (`Transport`, `Refresh`, `Status`) return just
    /// the variant name; use [`FailReason::detail`] for the full string.
    pub fn label(&self) -> &'static str {
        match self {
            FailReason::Quota { .. } => "quota",
            FailReason::UpstreamRejected => "upstream_rejected",
            FailReason::Overloaded => "overloaded",
            FailReason::AccessDenied => "access_denied",
            FailReason::Billing => "billing",
            FailReason::Transport(_) => "transport",
            FailReason::Refresh(_) => "refresh",
            FailReason::Status(_) => "status",
            FailReason::Network(_) => "network",
        }
    }

    /// `label()` plus the dynamic part: `transport:<kind>`, `refresh:<kind>`,
    /// `status:<code>`, `network:<kind>`. Never includes a quota reset time or upstream text.
    pub fn detail(&self) -> String {
        match self {
            FailReason::Transport(k) => format!("transport:{}", k.as_str()),
            FailReason::Refresh(k) => format!("refresh:{}", k.detail()),
            FailReason::Status(code) => format!("status:{code}"),
            FailReason::Network(k) => format!("network:{}", k.as_str()),
            other => other.label().to_string(),
        }
    }

    /// A failed DNS lookup, of the API host (`Network`, `Transport`) or of the token endpoint
    /// (`Refresh`): not the account's fault.
    pub fn is_dns(&self) -> bool {
        match self {
            FailReason::Network(k) | FailReason::Transport(k) => k.is_dns(),
            FailReason::Refresh(k) => k.is_dns(),
            _ => false,
        }
    }

    /// Whether failing over to the *other* provider is sensible (INV §5): quota,
    /// surfaced overload, and transport failures yes; `UpstreamRejected` never
    /// (the request itself is bad, the other provider would reject it too). A failed DNS lookup
    /// is (the other provider is another host).
    pub fn cross_provider_retryable(&self) -> bool {
        self.is_dns()
            || matches!(
                self,
                FailReason::Quota { .. } | FailReason::Overloaded | FailReason::Transport(_)
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::datetime;

    #[test]
    fn fail_reason_labels_stable() {
        let all = [
            FailReason::Quota { reset: None },
            FailReason::UpstreamRejected,
            FailReason::Overloaded,
            FailReason::AccessDenied,
            FailReason::Billing,
            FailReason::Transport(TransportKind::IdleTimeout),
            FailReason::Refresh(RefreshErrorKind::Network(TransportKind::Dns)),
            FailReason::Refresh(RefreshErrorKind::Server(503)),
            FailReason::Refresh(RefreshErrorKind::InvalidGrant),
            FailReason::Status(418),
            FailReason::Network(TransportKind::Dns),
        ];
        let rendered: Vec<String> = all
            .iter()
            .map(|r| format!("{} | {}", r.label(), r.detail()))
            .collect();
        insta::assert_snapshot!(rendered.join("\n"), @r"
        quota | quota
        upstream_rejected | upstream_rejected
        overloaded | overloaded
        access_denied | access_denied
        billing | billing
        transport | transport:idle_timeout
        refresh | refresh:network:dns
        refresh | refresh:server:503
        refresh | refresh:invalid_grant
        status | status:418
        network | network:dns
        ");
        let transports = [
            TransportKind::Dns,
            TransportKind::Connect,
            TransportKind::Tls,
            TransportKind::Reset,
            TransportKind::IdleTimeout,
            TransportKind::Deadline,
            TransportKind::Other,
        ];
        let rendered: Vec<String> = transports
            .iter()
            .map(|k| FailReason::Transport(*k).detail())
            .collect();
        insta::assert_snapshot!(rendered.join("\n"), @r"
        transport:dns
        transport:connect
        transport:tls
        transport:reset
        transport:idle_timeout
        transport:deadline
        transport:other
        ");
        let refreshes = [
            RefreshErrorKind::InvalidGrant,
            RefreshErrorKind::TokenReused,
            RefreshErrorKind::AccountOnHold,
            RefreshErrorKind::Network(TransportKind::Tls),
            RefreshErrorKind::Server(500),
            RefreshErrorKind::Other,
        ];
        let rendered: Vec<String> = refreshes
            .iter()
            .map(|k| FailReason::Refresh(*k).detail())
            .collect();
        insta::assert_snapshot!(rendered.join("\n"), @r"
        refresh:invalid_grant
        refresh:token_reused
        refresh:account_on_hold
        refresh:network:tls
        refresh:server:500
        refresh:other
        ");
    }

    #[test]
    fn label_ignores_dynamic_parts() {
        assert_eq!(
            FailReason::Status(404).label(),
            FailReason::Status(500).label()
        );
        assert_eq!(
            FailReason::Quota {
                reset: Some(datetime!(2026-11-07 12:00 UTC))
            }
            .detail(),
            "quota"
        );
    }

    #[test]
    fn upstream_rejected_is_not_cross_provider_retryable() {
        assert!(!FailReason::UpstreamRejected.cross_provider_retryable());
        assert!(FailReason::Quota { reset: None }.cross_provider_retryable());
        assert!(FailReason::Overloaded.cross_provider_retryable());
        assert!(FailReason::Transport(TransportKind::Reset).cross_provider_retryable());
        for r in [
            FailReason::AccessDenied,
            FailReason::Billing,
            FailReason::Refresh(RefreshErrorKind::Other),
            FailReason::Status(400),
            FailReason::Status(529),
        ] {
            assert!(!r.cross_provider_retryable(), "{r:?}");
        }
        // Another provider is another host.
        assert!(FailReason::Network(TransportKind::Dns).cross_provider_retryable());
        assert!(
            FailReason::Refresh(RefreshErrorKind::Network(TransportKind::Dns))
                .cross_provider_retryable()
        );
    }

    #[test]
    fn dns_failures_are_recognised() {
        use TransportKind::*;
        for k in [Connect, Tls, Reset, IdleTimeout, Deadline, Other] {
            assert!(!k.is_dns(), "{k:?}");
            assert!(!RefreshErrorKind::Network(k).is_dns(), "{k:?}");
            assert!(!FailReason::Transport(k).is_dns(), "{k:?}");
        }
        assert!(Dns.is_dns());
        assert!(RefreshErrorKind::Network(Dns).is_dns());
        assert!(!RefreshErrorKind::Server(503).is_dns());
        assert!(FailReason::Network(Dns).is_dns());
        assert!(FailReason::Refresh(RefreshErrorKind::Network(Dns)).is_dns());
        assert!(!FailReason::Quota { reset: None }.is_dns());
    }

    #[test]
    fn serde_uses_snake_case_tags_and_round_trips() {
        let cases = [
            (FailReason::UpstreamRejected, r#""upstream_rejected""#),
            (
                FailReason::Transport(TransportKind::IdleTimeout),
                r#"{"transport":"idle_timeout"}"#,
            ),
            (
                FailReason::Refresh(RefreshErrorKind::Server(503)),
                r#"{"refresh":{"server":503}}"#,
            ),
            (FailReason::Status(418), r#"{"status":418}"#),
            (
                FailReason::Network(TransportKind::Dns),
                r#"{"network":"dns"}"#,
            ),
            (
                FailReason::Quota {
                    reset: Some(datetime!(2026-11-07 12:00 UTC)),
                },
                r#"{"quota":{"reset":"2026-11-07T12:00:00Z"}}"#,
            ),
            (
                FailReason::Quota { reset: None },
                r#"{"quota":{"reset":null}}"#,
            ),
        ];
        for (v, json) in cases {
            assert_eq!(serde_json::to_string(&v).unwrap(), json);
            assert_eq!(serde_json::from_str::<FailReason>(json).unwrap(), v);
        }
        assert_eq!(
            serde_json::from_str::<FailReason>(r#"{"quota":{}}"#).unwrap(),
            FailReason::Quota { reset: None }
        );
    }
}
