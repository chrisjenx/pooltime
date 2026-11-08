//! Model-id resolution: exact -> strip a trailing `[1m]` -> Anthropic passthrough.

use super::defaults::CODEX_EXTENDED_CONTEXT_MIN;
use super::{Route, RouteDef};
use crate::providers::ProviderId;

/// `id` without a trailing `[1m]` (ASCII case-insensitive), or `None` when it has none.
pub(crate) fn strip_1m(id: &str) -> Option<&str> {
    let cut = id.len().checked_sub(4)?;
    let (head, tail) = (id.get(..cut)?, id.get(cut..)?);
    tail.eq_ignore_ascii_case("[1m]").then_some(head)
}

/// Row for `id`: exact match, else the row for `id` minus `[1m]`.
pub(crate) fn find_row<'a>(rows: &'a [RouteDef], id: &str) -> Option<&'a RouteDef> {
    rows.iter()
        .find(|r| r.id == id)
        .or_else(|| strip_1m(id).and_then(|bare| rows.iter().find(|r| r.id == bare)))
}

/// Resolve `requested` against `rows`. A `[1m]`-suffixed request that matched its bare
/// row keeps the caller's id (`def.id == requested`). An unknown id passes through to
/// Anthropic with `upstream_model` = `requested` minus `[1m]`.
pub(crate) fn resolve_in(rows: &[RouteDef], requested: &str) -> Route {
    let def = match find_row(rows, requested) {
        Some(row) if row.id == requested => row.clone(),
        Some(row) => RouteDef {
            id: requested.to_string(),
            ..row.clone()
        },
        None => RouteDef {
            id: requested.to_string(),
            provider: ProviderId::Anthropic,
            upstream_model: strip_1m(requested).unwrap_or(requested).to_string(),
            context_window: None,
            max_context_window: None,
            supported_efforts: Vec::new(),
        },
    };
    Route {
        requested: requested.to_string(),
        def,
        effort: None,
    }
}

/// Id shown to clients: OpenAI routes whose maximum context reaches 872K are listed as
/// `<id>[1m]` (the suffix Claude Code understands as "1M window").
pub(crate) fn selector_id(def: &RouteDef) -> String {
    let extended = def.provider == ProviderId::OpenAi
        && def.max_context_window.unwrap_or(0) >= CODEX_EXTENDED_CONTEXT_MIN;
    if extended && strip_1m(&def.id).is_none() {
        format!("{}[1m]", def.id)
    } else {
        def.id.clone()
    }
}
