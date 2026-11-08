//! Account identity, stored credentials and the on-disk registry.
//!
//! Layout: `accounts/<id>/account.toml` (meta), `credentials.json` (0600) and
//! `lock` (flock target for refresh/write serialization).

pub mod creds;
pub mod id;
pub mod registry;

pub use creds::{Credentials, CredsError};
pub use id::{AccountId, InvalidAccountId};
pub use registry::{AccountMeta, Identity, Registry, TimedOverride};
