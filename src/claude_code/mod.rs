//! Claude Code integration: profile discovery and guarded patching of the
//! JSON files Claude Code itself rewrites (`settings.json`, `.claude.json`).

mod profile;
mod safe_write;

pub use profile::{discover, Profile};
pub use safe_write::patch_json;
