//! Bundled defaults (INV §5): the model table and the family mappings, plus the
//! effort vocabularies they are validated against. Refreshed at build time.

use super::mapping::Mapping;
use super::RouteDef;
use crate::providers::ProviderId;

/// Source (Claude Code) effort tiers accepted as keys of a mapping's effort overrides.
pub const SOURCE_EFFORT_TIERS: [&str; 6] = ["default", "low", "medium", "high", "xhigh", "max"];

/// Codex Responses API `reasoning.effort` values. `ultra` is a Codex-app-only delegated
/// mode, not an API value, and is never accepted.
pub const CODEX_EFFORTS: [&str; 6] = ["none", "low", "medium", "high", "xhigh", "max"];

/// Smallest verified Codex input/history ceiling that represents its opt-in 1M total
/// budget mode; such routes are listed as `<id>[1m]`.
pub const CODEX_EXTENDED_CONTEXT_MIN: u32 = 872_000;

const CODEX_DEFAULT_CONTEXT_WINDOW: u32 = 272_000;
const FRONTIER_EFFORTS: [&str; 5] = ["low", "medium", "high", "xhigh", "max"];
const STANDARD_EFFORTS: [&str; 5] = ["none", "low", "medium", "high", "xhigh"];
const FRONTIER_UPSTREAMS: [&str; 6] = [
    "gpt-6-astra",
    "gpt-6-sol",
    "gpt-6-luna",
    "gpt-5.6-sol",
    "gpt-5.6-terra",
    "gpt-5.6-luna",
];

fn owned(list: &[&str]) -> Vec<String> {
    list.iter().map(|s| (*s).to_string()).collect()
}

/// Efforts assumed for a route that does not list its own: frontier upstreams take
/// `low..max`, other OpenAI upstreams `none..xhigh`. Anthropic routes have none.
pub fn inferred_efforts(provider: ProviderId, upstream_model: &str) -> Vec<String> {
    match provider {
        ProviderId::Anthropic => Vec::new(),
        ProviderId::OpenAi if FRONTIER_UPSTREAMS.contains(&upstream_model) => {
            owned(&FRONTIER_EFFORTS)
        }
        ProviderId::OpenAi => owned(&STANDARD_EFFORTS),
    }
}

fn claude(id: &str) -> RouteDef {
    RouteDef {
        id: id.into(),
        provider: ProviderId::Anthropic,
        upstream_model: id.into(),
        context_window: None,
        max_context_window: None,
        supported_efforts: Vec::new(),
        effort_map: None,
    }
}

fn openai(id: &str, upstream: &str, max: u32) -> RouteDef {
    RouteDef {
        id: id.into(),
        provider: ProviderId::OpenAi,
        upstream_model: upstream.into(),
        context_window: Some(CODEX_DEFAULT_CONTEXT_WINDOW),
        max_context_window: Some(max),
        supported_efforts: inferred_efforts(ProviderId::OpenAi, upstream),
        effort_map: None,
    }
}

fn frontier(id: &str, upstream: &str) -> RouteDef {
    openai(id, upstream, CODEX_EXTENDED_CONTEXT_MIN)
}

/// The bundled model table. Codex values were checked against the authenticated catalog
/// (bro, 2026-09-22); GPT-6/GPT-5.6 872K maximums are the input side of the opt-in 1M
/// budget. GPT-6 ships Astra/Sol/Luna only (no Terra).
pub fn default_routes() -> Vec<RouteDef> {
    vec![
        claude("opus"),
        claude("sonnet"),
        claude("haiku"),
        claude("fable"),
        claude("claude-opus-5"),
        claude("claude-opus-4-8"),
        claude("claude-sonnet-5"),
        claude("claude-haiku-5-5"),
        claude("claude-haiku-4-5"),
        claude("claude-fable-5"),
        claude("claude-fable-5-1"),
        frontier("gpt-6-astra", "gpt-6-astra"),
        frontier("gpt-6-sol", "gpt-6-sol"),
        frontier("gpt-6-luna", "gpt-6-luna"),
        frontier("gpt-5.6-sol", "gpt-5.6-sol"),
        frontier("gpt-5.6-terra", "gpt-5.6-terra"),
        frontier("gpt-5.6-luna", "gpt-5.6-luna"),
        frontier("gpt-5.6", "gpt-5.6-sol"),
        openai("gpt-5.5", "gpt-5.5", 272_000),
        openai("gpt-5.4", "gpt-5.4", 1_000_000),
        openai("gpt-5.4-mini", "gpt-5.4-mini", 272_000),
    ]
}

/// Default family mappings (mapping itself defaults to off). GPT-6 has no Terra, so
/// Sonnet shares Sol with Opus rather than dropping to Luna (Haiku's role).
pub fn default_mappings() -> Vec<Mapping> {
    [
        ("fable", "gpt-6-astra"),
        ("opus", "gpt-6-sol"),
        ("sonnet", "gpt-6-sol"),
        ("haiku", "gpt-6-luna"),
    ]
    .into_iter()
    .map(|(from, to)| Mapping {
        from: from.into(),
        to: to.into(),
        effort: None,
    })
    .collect()
}
