//! The guard every committed streaming response is pumped under (Anthropic and Codex).

use crate::accounts::AccountId;
use crate::errors::TransportKind;
use crate::providers::ProviderId;
use crate::routing::reserve::Reservation;

/// How a committed stream ended (the `outcome` field of the `<provider> stream finished` event).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamOutcome {
    Completed,
    ClientCancelled,
    UpstreamTruncated,
    IdleTimeout,
    Reset,
    Error,
}

impl StreamOutcome {
    pub fn as_str(self) -> &'static str {
        match self {
            StreamOutcome::Completed => "completed",
            StreamOutcome::ClientCancelled => "client_cancelled",
            StreamOutcome::UpstreamTruncated => "upstream_truncated",
            StreamOutcome::IdleTimeout => "idle_timeout",
            StreamOutcome::Reset => "reset",
            StreamOutcome::Error => "error",
        }
    }

    pub(crate) fn of_transport(k: TransportKind) -> StreamOutcome {
        match k {
            TransportKind::IdleTimeout => StreamOutcome::IdleTimeout,
            TransportKind::Reset => StreamOutcome::Reset,
            _ => StreamOutcome::Error,
        }
    }
}

/// Owned by the task pumping a committed stream. Dropping it (normal end, failure, client
/// disconnect, task abort or panic) emits one structured `<label> stream finished` event (request
/// id, account, outcome; label `anthropic` / `codex`) and releases the in-flight reservation.
///
/// TODO(F4): also write the journal row here once the journal exists.
pub struct StreamGuard {
    reservation: Option<Reservation>,
    provider: ProviderId,
    account: AccountId,
    request_id: String,
    outcome: StreamOutcome,
}

impl StreamGuard {
    /// Outcome reported on drop until [`StreamGuard::set`] says otherwise (`error`).
    pub(crate) fn new(
        provider: ProviderId,
        reservation: Option<Reservation>,
        account: AccountId,
        request_id: String,
    ) -> Self {
        StreamGuard {
            reservation,
            provider,
            account,
            request_id,
            outcome: StreamOutcome::Error,
        }
    }

    pub(crate) fn set(&mut self, o: StreamOutcome) {
        self.outcome = o;
    }

    fn label(&self) -> &'static str {
        match self.provider {
            ProviderId::Anthropic => "anthropic",
            ProviderId::OpenAi => "codex",
        }
    }
}

impl Drop for StreamGuard {
    fn drop(&mut self) {
        let outcome = self.outcome.as_str();
        let label = self.label();
        match self.outcome {
            StreamOutcome::Completed | StreamOutcome::ClientCancelled => tracing::info!(
                request_id = %self.request_id,
                account = %self.account,
                outcome,
                "{label} stream finished"
            ),
            _ => tracing::warn!(
                request_id = %self.request_id,
                account = %self.account,
                outcome,
                "{label} stream finished"
            ),
        }
        drop(self.reservation.take());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn outcome_labels() {
        let all = [
            StreamOutcome::Completed,
            StreamOutcome::ClientCancelled,
            StreamOutcome::UpstreamTruncated,
            StreamOutcome::IdleTimeout,
            StreamOutcome::Reset,
            StreamOutcome::Error,
        ];
        let got: Vec<_> = all.iter().map(|o| o.as_str()).collect();
        assert_eq!(
            got,
            [
                "completed",
                "client_cancelled",
                "upstream_truncated",
                "idle_timeout",
                "reset",
                "error"
            ]
        );
        assert_eq!(
            StreamOutcome::of_transport(TransportKind::Deadline),
            StreamOutcome::Error
        );
    }
}
