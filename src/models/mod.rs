//! Minimal route types consumed by the `Provider` trait. Task 20 extends this
//! module with the model table, resolution, mapping and families.

use serde::{Deserialize, Serialize};

use crate::providers::ProviderId;

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
