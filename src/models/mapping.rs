//! Family mappings: which Claude family is served by which route, with per-tier effort
//! overrides. Rows are always kept as given (an opt-out row must persist, otherwise a
//! reload would refill the default); whether a row is *active* is decided by
//! [`super::ModelTable::mapping_for`].

use std::collections::{BTreeMap, HashMap};

use serde::{Deserialize, Serialize, Serializer};

use super::defaults::{CODEX_EFFORTS, SOURCE_EFFORT_TIERS};
use super::families::{family_of, MODEL_FAMILIES};
use super::resolve::find_row;
use super::RouteDef;
use crate::providers::ProviderId;

/// One family mapping. `to` is a route id; a Claude (Anthropic) target, or `to == from`,
/// is an opt-out that keeps the family on Anthropic.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Mapping {
    /// Claude family (`fable`, `mythos`, `opus`, `sonnet`, `haiku`).
    pub from: String,
    /// Target route id.
    pub to: String,
    /// Source tier -> Codex effort overrides; omitted tiers pass through 1:1.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        serialize_with = "sorted_effort"
    )]
    pub effort: Option<HashMap<String, String>>,
}

pub(crate) fn sorted_effort<S: Serializer>(
    v: &Option<HashMap<String, String>>,
    s: S,
) -> Result<S::Ok, S::Error> {
    match v {
        Some(m) => m.iter().collect::<BTreeMap<_, _>>().serialize(s),
        None => s.serialize_none(),
    }
}

/// Why a mapping edit was rejected. Messages are safe to return in a 400 body.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum MappingError {
    #[error("unknown family '{0}' (expected one of fable, mythos, opus, sonnet, haiku)")]
    UnknownFamily(String),
    #[error("unknown target model '{0}'")]
    UnknownTarget(String),
    #[error("unknown effort tier '{0}'")]
    InvalidEffortTier(String),
    #[error("invalid effort value '{value}' for tier '{tier}'")]
    InvalidEffortValue { tier: String, value: String },
    #[error("target model '{target}' does not support effort '{value}'")]
    UnsupportedEffort { target: String, value: String },
}

/// Keep only recognised `(tier, value)` pairs; `None` when nothing is left.
pub(crate) fn sanitize_effort(m: &HashMap<String, String>) -> Option<HashMap<String, String>> {
    let kept: HashMap<String, String> = m
        .iter()
        .filter(|(k, v)| {
            SOURCE_EFFORT_TIERS.contains(&k.as_str()) && CODEX_EFFORTS.contains(&v.as_str())
        })
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    (!kept.is_empty()).then_some(kept)
}

/// The OpenAI row `to` points at, if any (exact or `[1m]`-stripped).
pub(crate) fn openai_target<'a>(rows: &'a [RouteDef], to: &str) -> Option<&'a RouteDef> {
    find_row(rows, to).filter(|r| r.provider == ProviderId::OpenAi)
}

/// Load-time cleanup: drop effort values the target cannot accept (bro warns and ignores).
pub(crate) fn drop_unsupported_efforts(rows: &[RouteDef], m: &mut Mapping) {
    let Some(target) = openai_target(rows, &m.to) else {
        return;
    };
    let Some(effort) = m.effort.as_mut() else {
        return;
    };
    effort.retain(|tier, value| {
        let ok = target.supported_efforts.contains(value);
        if !ok {
            tracing::warn!(
                to = %m.to, %tier, %value,
                "model mapping target does not support this effort; ignoring override"
            );
        }
        ok
    });
    if effort.is_empty() {
        m.effort = None;
    }
}

/// Strict validation for an API edit (bro `validateMapping`). Effort is checked
/// (sorted, so the first error is deterministic) against the target's supported efforts.
pub(crate) fn validate(rows: &[RouteDef], m: &Mapping) -> Result<(), MappingError> {
    if !MODEL_FAMILIES.contains(&m.from.as_str()) {
        return Err(MappingError::UnknownFamily(m.from.clone()));
    }
    let target = find_row(rows, &m.to);
    // A Claude-looking id that is not a row is a valid (inert) Anthropic opt-out.
    if target.is_none() && family_of(&m.to).is_none() {
        return Err(MappingError::UnknownTarget(m.to.clone()));
    }
    if let Some(effort) = &m.effort {
        let sorted: BTreeMap<_, _> = effort.iter().collect();
        for (tier, value) in sorted {
            if !SOURCE_EFFORT_TIERS.contains(&tier.as_str()) {
                return Err(MappingError::InvalidEffortTier(tier.clone()));
            }
            if !CODEX_EFFORTS.contains(&value.as_str()) {
                return Err(MappingError::InvalidEffortValue {
                    tier: tier.clone(),
                    value: value.clone(),
                });
            }
            if let Some(t) = target.filter(|t| t.provider == ProviderId::OpenAi) {
                if !t.supported_efforts.contains(value) {
                    return Err(MappingError::UnsupportedEffort {
                        target: m.to.clone(),
                        value: value.clone(),
                    });
                }
            }
        }
    }
    Ok(())
}

/// Overlay `rows` onto `base`: a row replaces the base row for its family, families not
/// in `rows` keep their base value. Duplicate families within either list keep the first.
pub(crate) fn merge_over(base: Vec<Mapping>, rows: Vec<Mapping>) -> Vec<Mapping> {
    let mut out: Vec<Mapping> = Vec::with_capacity(base.len() + rows.len());
    let overridden: Vec<&str> = rows.iter().map(|m| m.from.as_str()).collect();
    for m in base {
        if !overridden.contains(&m.from.as_str()) && !out.iter().any(|o| o.from == m.from) {
            out.push(m);
        }
    }
    for m in rows {
        if !out.iter().any(|o| o.from == m.from) {
            out.push(m);
        }
    }
    out
}
