//! Model routing: the table of public model ids and where they are served, id
//! resolution (`[1m]`-aware, Anthropic passthrough), family mappings to OpenAI routes,
//! and Claude family detection. Persisted as `models.toml` under the data dir.

mod defaults;
mod families;
mod mapping;
mod resolve;
mod table;
#[cfg(test)]
mod tests;

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::providers::ProviderId;

pub use defaults::{
    default_mappings, default_routes, inferred_efforts, CODEX_EFFORTS, CODEX_EXTENDED_CONTEXT_MIN,
    SOURCE_EFFORT_TIERS,
};
pub use families::{family_of, MODEL_FAMILIES};
pub use mapping::{Mapping, MappingError};
pub use table::ModelTable;

/// One row of the model table: a public model id and where it is served.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RouteDef {
    pub id: String,
    pub provider: ProviderId,
    pub upstream_model: String,
    pub context_window: Option<u32>,
    pub max_context_window: Option<u32>,
    pub supported_efforts: Vec<String>,
    /// Per-route source tier -> Codex effort overrides (bro `effortMap`), sanitised on load.
    /// See [`effective_effort`].
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        serialize_with = "mapping::sorted_effort"
    )]
    pub effort_map: Option<HashMap<String, String>>,
}

/// Codex effort to send for a source effort `tier` (`default`, `low`, ... `max`): the
/// mapping row's override wins, then the route's `effort_map`, else the tier passes
/// through 1:1 and `default` stays unset (Codex applies its own server default).
/// Clamping to `supported_efforts` is the caller's job.
pub fn effective_effort(
    def: &RouteDef,
    mapping_effort: Option<&HashMap<String, String>>,
    tier: &str,
) -> Option<String> {
    mapping_effort
        .and_then(|m| m.get(tier))
        .or_else(|| def.effort_map.as_ref().and_then(|m| m.get(tier)))
        .cloned()
        .or_else(|| (tier != "default").then(|| tier.to_string()))
}

/// A resolved request route.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Route {
    pub requested: String,
    /// Resolved row; for Anthropic passthrough `upstream_model` is `requested` minus `[1m]`.
    pub def: RouteDef,
    pub effort: Option<String>,
}
