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
}

/// A resolved request route.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Route {
    pub requested: String,
    /// Resolved row; for Anthropic passthrough `upstream_model` is `requested` minus `[1m]`.
    pub def: RouteDef,
    pub effort: Option<String>,
}
