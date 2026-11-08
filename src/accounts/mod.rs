//! Account identity, stored credentials, the on-disk registry and runtime state.
//!
//! Layout: `accounts/<id>/account.toml` (meta), `credentials.json` (0600) and
//! `lock` (flock target for refresh/write serialization).

pub mod creds;
pub mod id;
pub mod registry;
pub mod status;

pub use creds::{Credentials, CredsError};
pub use id::{AccountId, InvalidAccountId};
pub use registry::{AccountMeta, Identity, Registry, TimedOverride};
pub use status::{
    AccountState, AccountStatus, Counters, ErrorRecord, RateLimitSnapshot, StateBook, TokenUsage,
    Transition, UsageCheck, Window, WindowStatus,
};
