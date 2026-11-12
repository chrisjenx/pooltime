//! pooltime's integration tests, linked into one test binary (`cargo test --test it`) instead of
//! one binary per file: far less link time and disk. Each former `tests/<name>.rs` is the module
//! `<name>`; filter with `cargo test --test it <name>::`.
//!
//! The modules share one process, so tests that spawn children or need a free port go through
//! [`support::procs`] (one spawn/port gate for the whole binary), and nothing here mutates the
//! process environment: child commands get their environment via `Command::env`.

#[path = "../support/mod.rs"]
mod support;

mod analytics;
mod claude_code;
mod claude_code_connect;
mod claude_code_settings;
mod cli;
mod codex_discovery;
mod codex_translate_req;
mod codex_translate_stream;
mod docs;
mod import_bro;
mod journal;
mod mcp;
mod mgmt_api;
mod models_route;
mod proxy_anthropic;
mod proxy_openai;
mod routing;
mod service;
mod setup_doctor;
