//! `GET /v1/models` (INV §5): the live Anthropic catalog merged with the model table.
//!
//! Unauthenticated, like bro's (the listing carries no account data). Order: live Claude rows,
//! then the table's Claude rows not listed live (aliases and user-added ids; the bundled full
//! Claude ids stay hidden), then the table's OpenAI rows with private `context_window` /
//! `max_context_window`. A configured OpenAI id replaces any live row with the same id, because
//! request routing gives a configured route precedence and discovery must agree.
//!
//! The listing never fails on upstream trouble: a stale cached live list is used, and with none the
//! table's Claude rows stand in alone.

use std::collections::HashSet;
use std::sync::Arc;

use axum::extract::State;
use axum::Json;
use serde_json::{json, Map, Value};

use super::AppState;
use crate::models::{ModelTable, RouteDef};
use crate::providers::anthropic::ANTHROPIC_OWNER;
use crate::providers::ProviderId;

/// `owned_by` of OpenAI (Codex) rows.
pub const OPENAI_OWNER: &str = "openai-chatgpt-pool";

/// `GET /v1/models`.
pub async fn models(State(app): State<Arc<AppState>>) -> Json<Value> {
    // `live_models` logs its own failures; any error just means "table only".
    let live = match app.providers.get(&ProviderId::Anthropic) {
        Some(p) => p.live_models().await.ok().filter(|v| !v.is_empty()),
        None => None,
    };
    let data = {
        let table = app.models.read().unwrap_or_else(|e| e.into_inner());
        build_listing(live.as_deref(), &table.rows_for_listing())
    };
    Json(envelope(data))
}

/// Anthropic list shape (`data`, `has_more`, `first_id`, `last_id`) plus bro's `object: "list"`.
fn envelope(data: Vec<Value>) -> Value {
    let id_at = |m: Option<&Value>| m.map_or(Value::Null, |m| m["id"].clone());
    json!({
        "object": "list",
        "first_id": id_at(data.first()),
        "last_id": id_at(data.last()),
        "has_more": false,
        "data": data,
    })
}

/// The merged rows (bro `buildModelListing`). `live` is `None` when no live list is available;
/// `rows` are the table's listable rows ([`ModelTable::rows_for_listing`]).
pub fn build_listing(live: Option<&[Value]>, rows: &[&RouteDef]) -> Vec<Value> {
    let live = live.unwrap_or_default();
    let live_ids: HashSet<&str> = live.iter().filter_map(|m| m["id"].as_str()).collect();
    let configured_openai: HashSet<&str> = rows
        .iter()
        .filter(|r| r.provider == ProviderId::OpenAi)
        .map(|r| r.id.as_str())
        .collect();
    // OpenAI rows always survive a live-id collision; Claude rows only when not listed live.
    let routes: Vec<&RouteDef> = rows
        .iter()
        .copied()
        .filter(|r| r.provider == ProviderId::OpenAi || !live_ids.contains(r.id.as_str()))
        .collect();
    // Ids that are their own selector: an explicitly configured `x[1m]` beats the synthesized
    // `x[1m]` of an `x` route.
    let exact: HashSet<String> = routes
        .iter()
        .filter(|r| ModelTable::selector_id(r) == r.id)
        .map(|r| r.id.clone())
        .collect();
    let mut listed: HashSet<String> = HashSet::new();
    let mut rest = Vec::new();
    // Table order within each provider, Claude rows first.
    for provider in [ProviderId::Anthropic, ProviderId::OpenAi] {
        for route in routes.iter().filter(|r| r.provider == provider) {
            let id = ModelTable::selector_id(route);
            if exact.contains(&id) && route.id != id {
                continue;
            }
            if listed.insert(id.clone()) {
                rest.push(table_row(route, id));
            }
        }
    }
    let mut out: Vec<Value> = live
        .iter()
        .filter(|m| {
            m["id"]
                .as_str()
                .is_some_and(|id| !listed.contains(id) && !configured_openai.contains(id))
        })
        .cloned()
        .collect();
    out.extend(rest);
    out
}

fn table_row(route: &RouteDef, id: String) -> Value {
    let mut row = Map::new();
    row.insert("id".into(), json!(id));
    row.insert("object".into(), json!("model"));
    row.insert("created".into(), json!(0));
    if route.provider == ProviderId::OpenAi {
        row.insert("owned_by".into(), json!(OPENAI_OWNER));
        // Pool-private Codex metadata.
        if let Some(n) = route.context_window {
            row.insert("context_window".into(), json!(n));
        }
        if let Some(n) = route.max_context_window {
            row.insert("max_context_window".into(), json!(n));
        }
    } else {
        row.insert("owned_by".into(), json!(ANTHROPIC_OWNER));
    }
    Value::Object(row)
}

#[cfg(test)]
mod tests {
    //! Ported from bro `models-list.test.ts` (`buildModelListing` and selector cases).
    use super::*;

    fn route(id: &str, provider: ProviderId, ctx: Option<u32>, max: Option<u32>) -> RouteDef {
        RouteDef {
            id: id.into(),
            provider,
            upstream_model: id.into(),
            context_window: ctx,
            max_context_window: max,
            supported_efforts: vec![],
            effort_map: None,
        }
    }

    fn openai(id: &str, ctx: u32, max: u32) -> RouteDef {
        route(id, ProviderId::OpenAi, Some(ctx), Some(max))
    }

    fn claude(id: &str) -> RouteDef {
        route(id, ProviderId::Anthropic, None, None)
    }

    fn live(ids: &[&str]) -> Vec<Value> {
        ids.iter()
            .map(|id| json!({"id": id, "object": "model", "created": 0, "owned_by": ANTHROPIC_OWNER}))
            .collect()
    }

    fn list(live: Option<&[Value]>, rows: &[RouteDef]) -> Vec<Value> {
        let refs: Vec<&RouteDef> = rows.iter().collect();
        build_listing(live, &refs)
    }

    fn ids(rows: &[Value]) -> Vec<&str> {
        rows.iter().map(|m| m["id"].as_str().unwrap()).collect()
    }

    fn sample() -> Vec<RouteDef> {
        vec![
            claude("opus"),
            claude("claude-opus-5"),
            openai("gpt-5.6-sol", 272_000, 872_000),
        ]
    }

    #[test]
    fn live_first_then_unlisted_table_claude_then_openai() {
        let l = live(&["claude-opus-5"]);
        let out = list(Some(&l), &sample());
        // The alias still routes so it stays listed; the live id is not duplicated.
        assert_eq!(ids(&out), ["claude-opus-5", "opus", "gpt-5.6-sol[1m]"]);
        assert_eq!(out[2]["owned_by"], OPENAI_OWNER);
    }

    #[test]
    fn falls_back_to_table_claude_rows_without_a_live_list() {
        let out = list(None, &sample());
        assert_eq!(ids(&out), ["opus", "claude-opus-5", "gpt-5.6-sol[1m]"]);
    }

    #[test]
    fn codex_rows_have_one_selector_and_private_context_metadata() {
        let rows = [
            openai("gpt-5.6-sol", 272_000, 872_000),
            openai("gpt-5.5", 272_000, 272_000),
            openai("gpt-5.4", 272_000, 1_000_000),
            openai("gpt-5.4-mini", 272_000, 272_000),
        ];
        let out = list(None, &rows);
        assert_eq!(
            ids(&out),
            ["gpt-5.6-sol[1m]", "gpt-5.5", "gpt-5.4[1m]", "gpt-5.4-mini"]
        );
        assert_eq!(out[0]["owned_by"], OPENAI_OWNER);
        assert_eq!(out[0]["context_window"], 272_000);
        assert_eq!(out[0]["max_context_window"], 872_000);
        assert!(out[0].get("max_input_tokens").is_none());
        assert_eq!(out[0]["created"], 0);
    }

    #[test]
    fn bundled_astra_is_listed_once_with_its_extended_selector() {
        let t = ModelTable::defaults();
        let out = build_listing(None, &t.rows_for_listing());
        let astra: Vec<&Value> = out
            .iter()
            .filter(|m| m["id"].as_str().unwrap().starts_with("gpt-6-astra"))
            .collect();
        assert_eq!(astra.len(), 1);
        assert_eq!(astra[0]["id"], "gpt-6-astra[1m]");
        assert_eq!(astra[0]["context_window"], 272_000);
        assert_eq!(astra[0]["max_context_window"], 872_000);
    }

    #[test]
    fn an_already_marked_custom_route_is_not_marked_twice() {
        let out = list(None, &[openai("custom[1m]", 272_000, 1_000_000)]);
        assert_eq!(ids(&out), ["custom[1m]"]);
    }

    #[test]
    fn an_explicit_selector_wins_a_synthesized_selector_collision() {
        let mut extended = openai("custom", 272_000, 1_000_000);
        extended.upstream_model = "custom-extended".into();
        let explicit = openai("custom[1m]", 128_000, 272_000);
        let out = list(None, &[extended, explicit]);
        assert_eq!(ids(&out), ["custom[1m]"]);
        assert_eq!(out[0]["context_window"], 128_000);
        assert_eq!(out[0]["max_context_window"], 272_000);
    }

    #[test]
    fn a_configured_openai_route_replaces_colliding_live_route_and_selector_ids() {
        let l = live(&["custom", "custom[1m]"]);
        let out = list(Some(&l), &[openai("custom", 272_000, 1_000_000)]);
        assert_eq!(ids(&out), ["custom[1m]"]);
        assert_eq!(out[0]["owned_by"], OPENAI_OWNER);
    }

    #[test]
    fn an_exact_configured_selector_replaces_a_colliding_live_selector() {
        let l = live(&["custom[1m]"]);
        let out = list(Some(&l), &[openai("custom[1m]", 128_000, 272_000)]);
        assert_eq!(ids(&out), ["custom[1m]"]);
        assert_eq!(out[0]["owned_by"], OPENAI_OWNER);
        assert_eq!(out[0]["context_window"], 128_000);
    }

    #[test]
    fn envelope_names_first_and_last_ids() {
        let e = envelope(live(&["a", "b", "c"]));
        assert_eq!(e["first_id"], "a");
        assert_eq!(e["last_id"], "c");
        assert_eq!(e["has_more"], false);
        let empty = envelope(vec![]);
        assert!(empty["first_id"].is_null() && empty["last_id"].is_null());
        assert_eq!(empty["data"], json!([]));
    }
}
