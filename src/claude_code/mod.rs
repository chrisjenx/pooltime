//! Claude Code integration: profile discovery and guarded patching of the
//! JSON files Claude Code itself rewrites (`settings.json`, `.claude.json`),
//! plus the reversible `settings.json` `env` override.

mod profile;
mod safe_write;
mod scrub;
mod settings_env;
mod snapshot;

pub use profile::{discover, Profile};
pub use safe_write::patch_json;
pub use scrub::{scrub_legacy_pins, scrub_managed_context};
pub use settings_env::{apply, clear, is_connected, EnvBlock, Pin, Pins, MANAGED_KEYS};
pub use snapshot::snapshot_path;
