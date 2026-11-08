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
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::datetime;

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
