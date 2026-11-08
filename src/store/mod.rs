//! SQLite persistence: one writer thread, read-only side connections, schema
//! migrations and retention. Row-level APIs live in `journal`, `timeline` and
//! `usage_history` (filled in by later tasks).

mod db;
pub mod journal;
mod migrations;
mod retention;
pub mod timeline;
pub mod usage_history;

pub use db::Store;
pub use migrations::SCHEMA_VERSION;
pub use retention::{prune, PruneReport};
