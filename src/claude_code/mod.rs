//! Claude Code integration: profile discovery and guarded patching of the
//! JSON files Claude Code itself rewrites (`settings.json`, `.claude.json`),
//! plus the reversible `settings.json` `env` override.

mod connect;
mod fable_cache;
mod pins;
mod profile;
mod safe_write;
mod scrub;
mod settings_env;
mod snapshot;

pub use connect::{
    catalog, connect, disconnect, profile_of, spawn_profile_sync, sync_once, SYNC_INTERVAL,
};
pub use fable_cache::rewrite_fable_rows;
pub use pins::{derive_pins, max_context, newest_fable, newest_of_family, Newest};
pub use profile::{discover, Profile};
pub use safe_write::patch_json;
pub use scrub::{scrub_legacy_pins, scrub_managed_context};
pub use settings_env::{apply, clear, is_connected, EnvBlock, Pin, Pins, MANAGED_KEYS};
pub use snapshot::snapshot_path;
