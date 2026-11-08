//! Account health (spec §4, §16): the display states ([`states`]) and the expiry probe.
//!
//! # Expiry probe
//! The recorded refresh-token expiry is only a clock. When it passes, [`ExpiryProbe::tick`] (run
//! from the daemon's health tick, every [`HEALTH_TICK`], never on the request path) spends one
//! *forced* refresh on the account, once per expiry value, through the provider's normal refresh
//! path (so the dead-marker and single-flight gates apply):
//!
//! - success: the account keeps working (`ready`/`expiring` by the clock);
//! - `invalid_grant` / `refresh_token_reused`: the refresh path marks the token dead, so the
//!   account is `needs_login` (confirmed, and sidelined);
//! - anything else (network, 5xx, ...): `erroring` until a success; never `needs_login`.
//!
//! An unknown expiry is never probed. Accounts already `needs_login` are skipped.

pub mod states;

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

use time::OffsetDateTime;

use crate::accounts::{AccountId, AccountStatus};
use crate::errors::{FailReason, RefreshErrorKind};
use crate::server::AppState;

/// Cadence of the daemon's health tick.
pub const HEALTH_TICK: Duration = Duration::from_secs(60);

/// What one expiry probe concluded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProbeOutcome {
    /// The refresh worked: the login is alive.
    Refreshed,
    /// The provider refused the refresh token: confirmed `needs_login`.
    NeedsLogin,
    /// Unconfirmed failure (class, e.g. `refresh:server:503`): `erroring`.
    Erroring(String),
}

/// Remembers which expiry value each account was probed for. In memory: a restart probes a
/// still-expired account once more.
#[derive(Default)]
pub struct ExpiryProbe {
    probed: Mutex<HashMap<AccountId, OffsetDateTime>>,
}

impl ExpiryProbe {
    /// Probe every account whose recorded expiry is at or before `now` and that was not probed
    /// for that expiry yet. Returns what each probe concluded (empty when nothing was due).
    pub async fn tick(
        &self,
        app: &AppState,
        now: OffsetDateTime,
    ) -> Vec<(AccountId, ProbeOutcome)> {
        let mut out = Vec::new();
        for (meta, expiry) in due(app, now).await {
            {
                let mut probed = self.probed.lock().unwrap_or_else(|e| e.into_inner());
                if probed.get(&meta.id) == Some(&expiry) {
                    continue;
                }
                probed.insert(meta.id.clone(), expiry);
            }
            let Some(provider) = app.providers.get(&meta.provider) else {
                continue;
            };
            let outcome = match provider.access_token(&meta, true).await {
                Ok(_) => ProbeOutcome::Refreshed,
                Err(kind) => outcome_of(kind),
            };
            if let ProbeOutcome::Erroring(class) = &outcome {
                app.state.record_probe_failure(&meta.id, class, now);
            }
            tracing::info!(
                account = %meta.id,
                outcome = ?outcome,
                "login expiry passed: probe refresh finished"
            );
            out.push((meta.id, outcome));
        }
        out
    }
}

/// Accounts whose recorded expiry is at or before `now` and that are not already `needs_login`,
/// with that expiry. Registry and credential reads run off the async workers.
async fn due(
    app: &AppState,
    now: OffsetDateTime,
) -> Vec<(crate::accounts::AccountMeta, OffsetDateTime)> {
    let (reg, state) = (app.registry.clone(), app.state.clone());
    tokio::task::spawn_blocking(move || {
        reg.list()
            .into_iter()
            .filter_map(|meta| {
                let expiry = reg.read_creds(&meta.id).ok()??.refresh_expires_at?;
                let live = state.status(&meta.id, true, now) != AccountStatus::NeedsLogin;
                (expiry <= now && live).then_some((meta, expiry))
            })
            .collect()
    })
    .await
    .unwrap_or_default()
}

/// Classify a refused forced refresh.
fn outcome_of(kind: RefreshErrorKind) -> ProbeOutcome {
    match kind {
        RefreshErrorKind::InvalidGrant | RefreshErrorKind::TokenReused => ProbeOutcome::NeedsLogin,
        other => ProbeOutcome::Erroring(FailReason::Refresh(other).detail()),
    }
}
