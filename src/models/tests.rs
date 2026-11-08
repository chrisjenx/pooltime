//! Ported from bro `pool/src/models.test.ts` (+ the table/mapping validation of
//! `server/mappings-update.test.ts` and `models-update.test.ts`; bro's
//! `model-routing.test.ts` exercises provider selection and belongs to the routing
//! tasks). Test names, snake_case:
//!
//! model table: unknown_model_id_falls_through_to_anthropic_pass_through,
//! fable_5_1_routes_to_anthropic_under_its_own_id,
//! openai_models_route_to_openai_with_the_mapped_upstream_id,
//! gpt_5_6_family_routes_to_openai_with_gpt_5_6_alias_to_sol,
//! custom_aliases_inherit_capabilities_from_known_upstream_models,
//! gpt_6_sol_and_luna_route_directly_with_frontier_efforts,
//! gpt_6_astra_routes_directly_to_the_matching_codex_model,
//! rows_for_listing_shows_one_entry_per_claude_family_keeps_openai_and_custom_ids,
//! load_merges_file_entries_over_defaults_and_survives_a_missing_file,
//! duplicate_configured_model_ids_keep_the_first_route_consistently,
//! bundled_codex_routes_carry_their_verified_default_and_maximum_contexts,
//! legacy_bundled_rows_inherit_context_metadata_without_overriding_explicit_values,
//! repointed_and_custom_rows_do_not_inherit_unrelated_bundled_context,
//! loaded_routes_preserve_explicit_supported_efforts_and_drop_invalid_values,
//! a_trailing_1m_selector_resolves_to_the_bare_route_unless_configured_exactly.
//! config: missing_file_yields_defaults_with_mapping_off,
//! persisted_mappings_stay_authoritative_while_missing_families_get_current_defaults,
//! user_mappings_shadow_defaults_by_family_and_enabled_flag_round_trips,
//! default_mappings_target_gpt_6_tiers_by_claude_family,
//! malformed_mapping_fields_fall_back_to_defaults_with_mapping_off,
//! target_incompatible_effort_overrides_are_dropped_while_loading,
//! invalid_effort_values_are_dropped_from_an_otherwise_valid_row,
//! save_round_trips_models_and_mapping_state.
//! mapping_for: maps_a_family_alias_and_a_full_model_id_to_the_openai_route,
//! sonnet_extended_context_selector_preserves_its_caller_id_while_mapping_to_terra,
//! disabled_flag_unknown_family_and_missing_row_return_none,
//! inert_rows_return_none_identity_target_and_anthropic_target,
//! target_that_is_a_family_alias_of_gpt_resolves_through_the_table.
//! apply_mappings: toggles_enabled_and_persists, replaces_mappings_after_validation_unknown_target_rejected,
//! anthropic_target_is_accepted_as_a_claude_only_opt_out,
//! invalid_shapes_rejected_unknown_family_bad_effort_key_and_value,
//! rejects_target_unsupported_effort_values_for_astra,
//! partial_post_keeps_unlisted_families_and_stays_in_sync_with_disk,
//! partial_post_preserves_a_previously_customized_family,
//! failed_apply_leaves_the_table_untouched.
//! discovery: add_discovered_is_additive_and_counts_new_ids.
//! pooltime-only: selector_id_appends_1m_for_openai_routes_with_872k_max,
//! family_of_follows_fable_mythos_opus_sonnet_haiku_order_case_insensitively,
//! save_writes_models_toml_0600_and_only_non_default_rows,
//! from_bro_json_imports_camel_case_models_json, corrupt_file_falls_back_to_defaults.

use std::os::unix::fs::PermissionsExt;

use super::*;
use crate::config::Paths;

fn from_toml(s: &str) -> ModelTable {
    ModelTable::from_toml_str(s)
}

fn defaults() -> ModelTable {
    ModelTable::defaults()
}

fn row<'a>(t: &'a ModelTable, id: &str) -> &'a RouteDef {
    t.rows().iter().find(|r| r.id == id).expect("row present")
}

fn mapping(from: &str, to: &str) -> Mapping {
    Mapping {
        from: from.into(),
        to: to.into(),
        effort: None,
    }
}

fn with_effort(from: &str, to: &str, pairs: &[(&str, &str)]) -> Mapping {
    Mapping {
        effort: Some(
            pairs
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        ),
        ..mapping(from, to)
    }
}

fn enabled_with(rows: Vec<Mapping>) -> ModelTable {
    let mut t = defaults();
    t.apply_mappings(Some(true), Some(rows)).unwrap();
    t
}

fn mapping_to<'a>(t: &'a ModelTable, family: &str) -> &'a str {
    &t.mappings().iter().find(|m| m.from == family).unwrap().to
}

fn efforts(list: &[&str]) -> Vec<String> {
    list.iter().map(|s| s.to_string()).collect()
}

// ---- model table ---------------------------------------------------------------------

#[test]
fn unknown_model_id_falls_through_to_anthropic_pass_through() {
    let t = defaults();
    let r = t.resolve("claude-sonnet-5");
    assert_eq!(r.def.provider, ProviderId::Anthropic);
    assert_eq!(r.def.upstream_model, "claude-sonnet-5");
    let r = t.resolve("some-future-model");
    assert_eq!(r.def.provider, ProviderId::Anthropic);
    assert_eq!(r.def.upstream_model, "some-future-model");
    assert_eq!(r.requested, "some-future-model");
    assert_eq!(r.effort, None);
}

#[test]
fn fable_5_1_routes_to_anthropic_under_its_own_id() {
    let t = defaults();
    let r = t.resolve("claude-fable-5-1");
    assert_eq!(r.def.provider, ProviderId::Anthropic);
    assert_eq!(r.def.upstream_model, "claude-fable-5-1");
    // 5.1 must not be swallowed by the 5 route.
    assert_eq!(
        t.resolve("claude-fable-5").def.upstream_model,
        "claude-fable-5"
    );
    assert_eq!(family_of("claude-fable-5-1"), Some("fable"));
}

#[test]
fn openai_models_route_to_openai_with_the_mapped_upstream_id() {
    let t = from_toml(
        r#"[[models]]
id = "gpt"
provider = "openai"
upstream_model = "gpt-5.2-codex"
"#,
    );
    let r = t.resolve("gpt");
    assert_eq!(r.def.provider, ProviderId::OpenAi);
    assert_eq!(r.def.upstream_model, "gpt-5.2-codex");
}

#[test]
fn gpt_5_6_family_routes_to_openai_with_gpt_5_6_alias_to_sol() {
    let t = defaults();
    for id in ["gpt-5.6-sol", "gpt-5.6-terra", "gpt-5.6-luna"] {
        let r = t.resolve(id);
        assert_eq!(r.def.provider, ProviderId::OpenAi);
        assert_eq!(r.def.upstream_model, id);
    }
    let alias = t.resolve("gpt-5.6");
    assert_eq!(alias.def.provider, ProviderId::OpenAi);
    assert_eq!(alias.def.upstream_model, "gpt-5.6-sol");
}

#[test]
fn custom_aliases_inherit_capabilities_from_known_upstream_models() {
    let t = from_toml(
        r#"[[models]]
id = "sol-alias"
provider = "openai"
upstream_model = "gpt-5.6-sol"

[[models]]
id = "astra-alias"
provider = "openai"
upstream_model = "gpt-6-astra"

[[models]]
id = "plain-alias"
provider = "openai"
upstream_model = "gpt-5.5"
"#,
    );
    assert!(t
        .resolve("sol-alias")
        .def
        .supported_efforts
        .contains(&"max".to_string()));
    assert!(!t
        .resolve("astra-alias")
        .def
        .supported_efforts
        .contains(&"none".to_string()));
    assert_eq!(
        t.resolve("plain-alias").def.supported_efforts,
        efforts(&["none", "low", "medium", "high", "xhigh"])
    );
}

#[test]
fn gpt_6_sol_and_luna_route_directly_with_frontier_efforts() {
    let t = defaults();
    for id in ["gpt-6-sol", "gpt-6-luna"] {
        let d = t.resolve(id).def;
        assert_eq!(d.id, id);
        assert_eq!(d.provider, ProviderId::OpenAi);
        assert_eq!(d.upstream_model, id);
        assert_eq!(d.context_window, Some(272_000));
        assert_eq!(d.max_context_window, Some(872_000));
        assert_eq!(
            d.supported_efforts,
            efforts(&["low", "medium", "high", "xhigh", "max"])
        );
    }
}

#[test]
fn gpt_6_astra_routes_directly_to_the_matching_codex_model() {
    let d = defaults().resolve("gpt-6-astra").def;
    assert_eq!(d.id, "gpt-6-astra");
    assert_eq!(d.provider, ProviderId::OpenAi);
    assert_eq!(d.upstream_model, "gpt-6-astra");
    assert_eq!(
        (d.context_window, d.max_context_window),
        (Some(272_000), Some(872_000))
    );
    assert_eq!(
        d.supported_efforts,
        efforts(&["low", "medium", "high", "xhigh", "max"])
    );
}

#[test]
fn rows_for_listing_shows_one_entry_per_claude_family_keeps_openai_and_custom_ids() {
    let t = defaults();
    let ids: Vec<&str> = t.rows_for_listing().iter().map(|r| r.id.as_str()).collect();
    for alias in ["opus", "sonnet", "haiku", "fable"] {
        assert!(ids.contains(&alias), "{alias}");
    }
    for full in [
        "claude-opus-4-8",
        "claude-sonnet-5",
        "claude-haiku-5-5",
        "claude-haiku-4-5",
        "claude-fable-5",
        "claude-fable-5-1",
    ] {
        assert!(!ids.contains(&full), "{full}");
    }
    let sonnets: Vec<_> = ids
        .iter()
        .filter(|i| family_of(i) == Some("sonnet"))
        .collect();
    assert_eq!(sonnets, [&"sonnet"]);
    assert!(ids.contains(&"gpt-5.6-sol") && ids.contains(&"gpt-5.6"));

    // A user-added Claude id stays visible; routing of hidden ids is unaffected.
    let custom = from_toml(
        r#"[[models]]
id = "claude-sonnet-5-20991231"
provider = "anthropic"
upstream_model = "claude-sonnet-5-20991231"
"#,
    );
    assert!(custom
        .rows_for_listing()
        .iter()
        .any(|r| r.id == "claude-sonnet-5-20991231"));
    assert_eq!(
        t.resolve("claude-sonnet-5").def.upstream_model,
        "claude-sonnet-5"
    );
}

#[test]
fn load_merges_file_entries_over_defaults_and_survives_a_missing_file() {
    let dir = tempfile::tempdir().unwrap();
    let paths = Paths::under(dir.path());
    assert_eq!(ModelTable::load(&paths).unwrap(), defaults());
    std::fs::write(
        paths.home.join("models.toml"),
        "[[models]]\nid = \"gpt-x\"\nprovider = \"openai\"\nupstream_model = \"gpt-x\"\n",
    )
    .unwrap();
    let t = ModelTable::load(&paths).unwrap();
    assert_eq!(row(&t, "gpt-x").provider, ProviderId::OpenAi);
    assert_eq!(row(&t, "opus").provider, ProviderId::Anthropic); // defaults kept
}

#[test]
fn duplicate_configured_model_ids_keep_the_first_route_consistently() {
    let t = from_toml(
        r#"[[models]]
id = "duplicate"
provider = "openai"
upstream_model = "first"
supported_efforts = ["low"]

[[models]]
id = "duplicate"
provider = "openai"
upstream_model = "second"
supported_efforts = ["max"]
"#,
    );
    let d = t.resolve("duplicate").def;
    assert_eq!(d.upstream_model, "first");
    assert_eq!(d.supported_efforts, efforts(&["low"]));
    assert_eq!(t.rows().iter().filter(|r| r.id == "duplicate").count(), 1);
}

#[test]
fn bundled_codex_routes_carry_their_verified_default_and_maximum_contexts() {
    let t = defaults();
    let expected = [
        ("gpt-6-astra", 272_000, 872_000),
        ("gpt-6-sol", 272_000, 872_000),
        ("gpt-6-luna", 272_000, 872_000),
        ("gpt-5.6-sol", 272_000, 872_000),
        ("gpt-5.6-terra", 272_000, 872_000),
        ("gpt-5.6-luna", 272_000, 872_000),
        ("gpt-5.6", 272_000, 872_000),
        ("gpt-5.5", 272_000, 272_000),
        ("gpt-5.4", 272_000, 1_000_000),
        ("gpt-5.4-mini", 272_000, 272_000),
    ];
    assert_eq!(CODEX_EXTENDED_CONTEXT_MIN, 872_000);
    for (id, ctx, max) in expected {
        let d = t.resolve(id).def;
        assert_eq!(
            (d.context_window, d.max_context_window),
            (Some(ctx), Some(max)),
            "{id}"
        );
    }
}

#[test]
fn legacy_bundled_rows_inherit_context_metadata_without_overriding_explicit_values() {
    let t = from_toml(
        r#"[[models]]
id = "gpt-5.6-sol"
provider = "openai"
upstream_model = "gpt-5.6-sol"

[[models]]
id = "gpt-5.5"
provider = "openai"
upstream_model = "gpt-5.5"
context_window = 250000
max_context_window = 260000
"#,
    );
    let sol = t.resolve("gpt-5.6-sol").def;
    assert_eq!(
        (sol.context_window, sol.max_context_window),
        (Some(272_000), Some(872_000))
    );
    assert_eq!(
        sol.supported_efforts,
        efforts(&["low", "medium", "high", "xhigh", "max"])
    );
    let g55 = t.resolve("gpt-5.5").def;
    assert_eq!(
        (g55.context_window, g55.max_context_window),
        (Some(250_000), Some(260_000))
    );
}

#[test]
fn repointed_and_custom_rows_do_not_inherit_unrelated_bundled_context() {
    let t = from_toml(
        r#"[[models]]
id = "gpt-5.6-sol"
provider = "openai"
upstream_model = "custom-sol"

[[models]]
id = "custom"
provider = "openai"
upstream_model = "custom"
context_window = -1
max_context_window = "wide"

[[models]]
id = "contradictory"
provider = "openai"
upstream_model = "contradictory"
context_window = 1000000
max_context_window = 272000
"#,
    );
    for id in ["gpt-5.6-sol", "custom", "contradictory"] {
        let d = t.resolve(id).def;
        assert_eq!(
            (d.context_window, d.max_context_window),
            (None, None),
            "{id}"
        );
    }
    assert_eq!(t.resolve("gpt-5.6-sol").def.upstream_model, "custom-sol");
}

#[test]
fn loaded_routes_preserve_explicit_supported_efforts_and_drop_invalid_values() {
    let t = from_toml(
        r#"[[models]]
id = "frontier"
provider = "openai"
upstream_model = "frontier"
supported_efforts = ["low", "max", "ultra", "low"]

[[models]]
id = "limited"
provider = "openai"
upstream_model = "limited"
supported_efforts = ["none", "low"]

[[models]]
id = "empty"
provider = "openai"
upstream_model = "empty"
supported_efforts = []

[[models]]
id = "all-invalid"
provider = "openai"
upstream_model = "all-invalid"
supported_efforts = ["ultra"]

[[models]]
id = "invalid"
provider = "openai"
upstream_model = "invalid"
supported_efforts = "all"
"#,
    );
    let e = |id: &str| t.resolve(id).def.supported_efforts;
    assert_eq!(e("frontier"), efforts(&["low", "max"]));
    assert_eq!(e("limited"), efforts(&["none", "low"]));
    assert_eq!(e("empty"), efforts(&[]));
    assert_eq!(e("all-invalid"), efforts(&[]));
    // Not a list at all: treated as unspecified and inferred like any custom route.
    assert_eq!(
        e("invalid"),
        efforts(&["none", "low", "medium", "high", "xhigh"])
    );
}

#[test]
fn a_trailing_1m_selector_resolves_to_the_bare_route_unless_configured_exactly() {
    let t = defaults();
    let routed = t.resolve("gpt-5.6-sol[1m]");
    assert_eq!(routed.requested, "gpt-5.6-sol[1m]");
    assert_eq!(routed.def.id, "gpt-5.6-sol[1m]");
    assert_eq!(routed.def.provider, ProviderId::OpenAi);
    assert_eq!(routed.def.upstream_model, "gpt-5.6-sol");
    // case-insensitive
    assert_eq!(
        t.resolve("gpt-5.6-sol[1M]").def.upstream_model,
        "gpt-5.6-sol"
    );

    let exact = from_toml(
        r#"[[models]]
id = "literal[1m]"
provider = "openai"
upstream_model = "literal-upstream"
"#,
    );
    assert_eq!(
        exact.resolve("literal[1m]").def.upstream_model,
        "literal-upstream"
    );

    // Unknown ids pass through to Anthropic minus the selector (controller ruling).
    let unknown = t.resolve("unknown-anthropic[1m]");
    assert_eq!(unknown.def.provider, ProviderId::Anthropic);
    assert_eq!(unknown.def.upstream_model, "unknown-anthropic");
    assert_eq!(unknown.requested, "unknown-anthropic[1m]");
}

// ---- config load / save --------------------------------------------------------------

#[test]
fn missing_file_yields_defaults_with_mapping_off() {
    let dir = tempfile::tempdir().unwrap();
    let t = ModelTable::load(&Paths::under(dir.path())).unwrap();
    assert!(!t.mapping_enabled());
    assert_eq!(t.mappings(), default_mappings().as_slice());
    assert!(t.rows().iter().any(|r| r.id == "fable"));
}

#[test]
fn persisted_mappings_stay_authoritative_while_missing_families_get_current_defaults() {
    let t = from_toml(
        r#"mapping_enabled = true

[[mappings]]
from = "fable"
to = "gpt-5.6-sol"

[[mappings]]
from = "sonnet"
to = "gpt-5.5"
"#,
    );
    assert!(t.mapping_enabled());
    assert_eq!(mapping_to(&t, "fable"), "gpt-5.6-sol");
    assert_eq!(mapping_to(&t, "sonnet"), "gpt-5.5");
    assert_eq!(mapping_to(&t, "opus"), "gpt-6-sol");
    assert_eq!(mapping_to(&t, "haiku"), "gpt-6-luna");
}

#[test]
fn user_mappings_shadow_defaults_by_family_and_enabled_flag_round_trips() {
    let t = from_toml(
        r#"mapping_enabled = true

[[mappings]]
from = "fable"
to = "fable"

[[mappings]]
from = "haiku"
to = "gpt-5.4-mini"
[mappings.effort]
low = "medium"
"#,
    );
    assert!(t.mapping_enabled());
    assert_eq!(mapping_to(&t, "fable"), "fable");
    assert_eq!(mapping_to(&t, "opus"), "gpt-6-sol");
    let haiku = t.mappings().iter().find(|m| m.from == "haiku").unwrap();
    assert_eq!(
        haiku.effort,
        Some([("low".to_string(), "medium".to_string())].into())
    );
}

#[test]
fn default_mappings_target_gpt_6_tiers_by_claude_family() {
    // No GPT-6 Terra, so Sonnet shares Sol.
    let t = enabled_with(default_mappings());
    for (family, upstream) in [
        ("fable", "gpt-6-astra"),
        ("opus", "gpt-6-sol"),
        ("sonnet", "gpt-6-sol"),
        ("haiku", "gpt-6-luna"),
    ] {
        assert_eq!(t.mapped_route(family).unwrap().def.upstream_model, upstream);
        assert_eq!(t.mapping_for(family).unwrap().to, upstream);
    }
}

#[test]
fn malformed_mapping_fields_fall_back_to_defaults_with_mapping_off() {
    let t = from_toml("mapping_enabled = \"yes\"\nmappings = \"nope\"\n");
    assert!(!t.mapping_enabled());
    assert_eq!(t.mappings(), default_mappings().as_slice());
}

#[test]
fn target_incompatible_effort_overrides_are_dropped_while_loading() {
    let t = from_toml(
        r#"[[mappings]]
from = "fable"
to = "gpt-6-astra"
[mappings.effort]
low = "none"
high = "high"
"#,
    );
    let fable = t.mappings().iter().find(|m| m.from == "fable").unwrap();
    assert_eq!(
        fable.effort,
        Some([("high".to_string(), "high".to_string())].into())
    );
}

#[test]
fn invalid_effort_values_are_dropped_from_an_otherwise_valid_row() {
    let t = from_toml(
        r#"[[mappings]]
from = "opus"
to = "gpt-5.6-sol"
[mappings.effort]
low = "ultra"
high = "max"
bogus = "low"
"#,
    );
    let opus = t.mappings().iter().find(|m| m.from == "opus").unwrap();
    assert_eq!(opus.to, "gpt-5.6-sol");
    assert_eq!(
        opus.effort,
        Some([("high".to_string(), "max".to_string())].into())
    );
}

#[test]
fn save_round_trips_models_and_mapping_state() {
    let dir = tempfile::tempdir().unwrap();
    let paths = Paths::under(dir.path());
    let mut t = ModelTable::load(&paths).unwrap();
    t.apply_mappings(
        Some(true),
        Some(vec![with_effort(
            "haiku",
            "gpt-5.4-mini",
            &[("low", "medium")],
        )]),
    )
    .unwrap();
    t.add_discovered(vec![RouteDef {
        id: "gpt-7".into(),
        provider: ProviderId::OpenAi,
        upstream_model: "gpt-7".into(),
        context_window: Some(272_000),
        max_context_window: Some(872_000),
        supported_efforts: efforts(&["low", "high"]),
    }]);
    t.save(&paths).unwrap();
    assert_eq!(ModelTable::load(&paths).unwrap(), t);
}

// ---- mapping_for / mapped_route ------------------------------------------------------

#[test]
fn maps_a_family_alias_and_a_full_model_id_to_the_openai_route() {
    let t = enabled_with(vec![with_effort(
        "fable",
        "gpt-5.6-sol",
        &[("max", "xhigh")],
    )]);
    for id in ["fable", "claude-fable-5"] {
        let route = t.mapped_route(id).unwrap();
        assert_eq!(route.requested, id);
        assert_eq!(route.def.id, id);
        assert_eq!(route.def.provider, ProviderId::OpenAi);
        assert_eq!(route.def.upstream_model, "gpt-5.6-sol");
        assert_eq!(route.def.context_window, Some(272_000));
        assert_eq!(route.def.max_context_window, Some(872_000));
    }
    assert_eq!(
        t.mapping_for("fable").unwrap().effort,
        Some([("max".to_string(), "xhigh".to_string())].into())
    );
}

#[test]
fn sonnet_extended_context_selector_preserves_its_caller_id_while_mapping_to_terra() {
    let t = enabled_with(vec![mapping("sonnet", "gpt-5.6-terra")]);
    let route = t.mapped_route("claude-sonnet-5[1m]").unwrap();
    assert_eq!(route.def.id, "claude-sonnet-5[1m]");
    assert_eq!(route.def.provider, ProviderId::OpenAi);
    assert_eq!(route.def.upstream_model, "gpt-5.6-terra");
}

#[test]
fn disabled_flag_unknown_family_and_missing_row_return_none() {
    let mut t = defaults();
    t.apply_mappings(Some(false), Some(vec![mapping("fable", "gpt-5.6-sol")]))
        .unwrap();
    assert!(t.mapped_route("fable").is_none());
    assert!(t.mapping_for("fable").is_none());
    t.apply_mappings(Some(true), None).unwrap();
    assert!(t.mapped_route("fable").is_some());
    assert!(t.mapped_route("gpt-5.5").is_none()); // no family
    assert!(t.mapping_for("gpt").is_none()); // no such family row

    // A family with no row (mythos has no default) is not mapped.
    assert!(t.mapped_route("claude-mythos-1").is_none());
}

#[test]
fn inert_rows_return_none_identity_target_and_anthropic_target() {
    let t = enabled_with(vec![
        mapping("fable", "fable"),
        mapping("opus", "claude-opus-4-8"),
    ]);
    assert!(t.mapped_route("fable").is_none());
    assert!(t.mapped_route("opus").is_none());
    assert!(t.mapping_for("fable").is_none());
    assert!(t.mapping_for("opus").is_none());
    // The opt-out rows are kept (persisted) so a reload does not refill the default.
    assert_eq!(mapping_to(&t, "fable"), "fable");
    assert_eq!(mapping_to(&t, "opus"), "claude-opus-4-8");
}

#[test]
fn target_that_is_a_family_alias_of_gpt_resolves_through_the_table() {
    let t = enabled_with(vec![mapping("haiku", "gpt-5.6")]);
    assert_eq!(
        t.mapped_route("haiku").unwrap().def.upstream_model,
        "gpt-5.6-sol"
    );
}

// ---- apply_mappings ------------------------------------------------------------------

#[test]
fn toggles_enabled_and_persists() {
    let dir = tempfile::tempdir().unwrap();
    let paths = Paths::under(dir.path());
    let mut t = ModelTable::load(&paths).unwrap();
    assert!(!t.mapping_enabled());
    t.apply_mappings(Some(true), None).unwrap();
    assert!(t.mapping_enabled());
    t.save(&paths).unwrap();
    assert!(ModelTable::load(&paths).unwrap().mapping_enabled());
}

#[test]
fn replaces_mappings_after_validation_unknown_target_rejected() {
    let mut t = defaults();
    let err = t
        .apply_mappings(None, Some(vec![mapping("fable", "gpt-nonexistent")]))
        .unwrap_err();
    assert_eq!(err, MappingError::UnknownTarget("gpt-nonexistent".into()));
    assert!(err.to_string().contains("gpt-nonexistent"));

    t.apply_mappings(
        None,
        Some(vec![with_effort(
            "fable",
            "gpt-5.6-terra",
            &[("max", "xhigh")],
        )]),
    )
    .unwrap();
    assert_eq!(mapping_to(&t, "fable"), "gpt-5.6-terra");
    assert_eq!(mapping_to(&t, "opus"), "gpt-6-sol");
}

#[test]
fn anthropic_target_is_accepted_as_a_claude_only_opt_out() {
    let mut t = defaults();
    t.apply_mappings(None, Some(vec![mapping("fable", "fable")]))
        .unwrap();
    // A Claude id that is not a table row is still a valid (inert) target.
    t.apply_mappings(None, Some(vec![mapping("opus", "claude-opus-9-9")]))
        .unwrap();
}

#[test]
fn invalid_shapes_rejected_unknown_family_bad_effort_key_and_value() {
    let mut t = defaults();
    assert_eq!(
        t.apply_mappings(None, Some(vec![mapping("gpt", "gpt-5.5")])),
        Err(MappingError::UnknownFamily("gpt".into()))
    );
    assert_eq!(
        t.apply_mappings(
            None,
            Some(vec![with_effort("opus", "gpt-5.5", &[("low", "ultra")])])
        ),
        Err(MappingError::InvalidEffortValue {
            tier: "low".into(),
            value: "ultra".into()
        })
    );
    assert_eq!(
        t.apply_mappings(
            None,
            Some(vec![with_effort("opus", "gpt-5.5", &[("bogus", "low")])])
        ),
        Err(MappingError::InvalidEffortTier("bogus".into()))
    );
}

#[test]
fn rejects_target_unsupported_effort_values_for_astra() {
    let mut t = defaults();
    let err = t
        .apply_mappings(
            None,
            Some(vec![with_effort(
                "fable",
                "gpt-6-astra",
                &[("high", "none")],
            )]),
        )
        .unwrap_err();
    assert!(err.to_string().contains("does not support effort 'none'"));
    t.apply_mappings(
        None,
        Some(vec![with_effort(
            "fable",
            "gpt-6-astra",
            &[("high", "max")],
        )]),
    )
    .unwrap();
}

#[test]
fn partial_post_keeps_unlisted_families_and_stays_in_sync_with_disk() {
    let dir = tempfile::tempdir().unwrap();
    let paths = Paths::under(dir.path());
    let mut t = ModelTable::load(&paths).unwrap();
    t.apply_mappings(None, Some(vec![mapping("fable", "gpt-5.6-luna")]))
        .unwrap();
    assert_eq!(mapping_to(&t, "fable"), "gpt-5.6-luna");
    assert_eq!(mapping_to(&t, "opus"), "gpt-6-sol");
    assert_eq!(mapping_to(&t, "sonnet"), "gpt-6-sol");
    assert_eq!(mapping_to(&t, "haiku"), "gpt-6-luna");
    let mut families: Vec<_> = t.mappings().iter().map(|m| m.from.as_str()).collect();
    families.sort();
    assert_eq!(families, ["fable", "haiku", "opus", "sonnet"]);

    t.save(&paths).unwrap();
    assert_eq!(
        ModelTable::load(&paths).unwrap().mappings(),
        t.mappings(),
        "memory and disk agree (same rows, same order)"
    );
}

#[test]
fn partial_post_preserves_a_previously_customized_family() {
    let mut t = defaults();
    t.apply_mappings(
        None,
        Some(vec![
            mapping("opus", "gpt-5.6-terra"),
            mapping("mythos", "gpt-5.6-sol"),
        ]),
    )
    .unwrap();
    t.apply_mappings(None, Some(vec![mapping("fable", "gpt-5.6-luna")]))
        .unwrap();
    assert_eq!(mapping_to(&t, "fable"), "gpt-5.6-luna");
    assert_eq!(mapping_to(&t, "opus"), "gpt-5.6-terra");
    assert_eq!(mapping_to(&t, "mythos"), "gpt-5.6-sol");
}

#[test]
fn failed_apply_leaves_the_table_untouched() {
    let mut t = defaults();
    let before = t.clone();
    let res = t.apply_mappings(
        Some(true),
        Some(vec![
            mapping("fable", "gpt-5.6-luna"),
            mapping("opus", "gpt-nope"),
        ]),
    );
    assert!(res.is_err());
    assert_eq!(t, before);
}

// ---- discovery -----------------------------------------------------------------------

fn def(id: &str) -> RouteDef {
    RouteDef {
        id: id.into(),
        provider: ProviderId::OpenAi,
        upstream_model: id.into(),
        context_window: Some(272_000),
        max_context_window: Some(872_000),
        supported_efforts: efforts(&["low", "high"]),
    }
}

#[test]
fn add_discovered_is_additive_and_counts_new_ids() {
    let mut t = defaults();
    let sol_before = row(&t, "gpt-6-sol").clone();
    let n = t.add_discovered(vec![
        def("gpt-6.1-sol"),
        def("gpt-6-sol"),   // existing id: never replaced
        def("gpt-6.1-sol"), // duplicate in the batch
    ]);
    assert_eq!(n, 1);
    assert_eq!(row(&t, "gpt-6-sol"), &sol_before);
    assert_eq!(row(&t, "gpt-6.1-sol").upstream_model, "gpt-6.1-sol");
    assert_eq!(t.add_discovered(vec![def("gpt-6.1-sol")]), 0);
}

// ---- pooltime-only -------------------------------------------------------------------

#[test]
fn selector_id_appends_1m_for_openai_routes_with_872k_max() {
    let t = defaults();
    assert_eq!(
        ModelTable::selector_id(row(&t, "gpt-6-sol")),
        "gpt-6-sol[1m]"
    );
    assert_eq!(ModelTable::selector_id(row(&t, "gpt-5.4")), "gpt-5.4[1m]"); // 1M max
    assert_eq!(ModelTable::selector_id(row(&t, "gpt-5.5")), "gpt-5.5"); // 272K max
    assert_eq!(ModelTable::selector_id(row(&t, "sonnet")), "sonnet"); // Anthropic never
    let mut already = def("x[1m]");
    already.max_context_window = Some(872_000);
    assert_eq!(ModelTable::selector_id(&already), "x[1m]");
    let mut at_edge = def("edge");
    at_edge.max_context_window = Some(871_999);
    assert_eq!(ModelTable::selector_id(&at_edge), "edge");
    // The listed selector routes back to the row.
    assert_eq!(t.resolve("gpt-6-sol[1m]").def.upstream_model, "gpt-6-sol");
}

#[test]
fn family_of_follows_fable_mythos_opus_sonnet_haiku_order_case_insensitively() {
    assert_eq!(family_of("claude-opus-5"), Some("opus"));
    assert_eq!(family_of("Claude-SONNET-5[1m]"), Some("sonnet"));
    assert_eq!(family_of("haiku"), Some("haiku"));
    assert_eq!(family_of("claude-mythos-preview"), Some("mythos"));
    // First family in order wins when several appear.
    assert_eq!(family_of("fable-opus"), Some("fable"));
    assert_eq!(family_of("opus-sonnet-haiku"), Some("opus"));
    assert_eq!(family_of("sonnet-haiku"), Some("sonnet"));
    assert_eq!(family_of("mythos-opus"), Some("mythos"));
    assert_eq!(family_of("gpt-6-sol"), None);
    assert_eq!(family_of(""), None);
    assert_eq!(
        MODEL_FAMILIES,
        ["fable", "mythos", "opus", "sonnet", "haiku"]
    );
}

#[test]
fn save_writes_models_toml_0600_and_only_non_default_rows() {
    let dir = tempfile::tempdir().unwrap();
    let paths = Paths::under(dir.path());
    let t = defaults();
    t.save(&paths).unwrap();
    let file = dir.path().join("models.toml");
    assert_eq!(
        std::fs::metadata(&file).unwrap().permissions().mode() & 0o777,
        0o600
    );
    let text = std::fs::read_to_string(&file).unwrap();
    assert!(
        !text.contains("[[models]]"),
        "defaults are not frozen into the file:\n{text}"
    );
    assert!(text.contains("mapping_enabled = false"));
    assert!(text.contains("[[mappings]]"));

    let mut t = t;
    t.add_discovered(vec![def("gpt-9")]);
    t.save(&paths).unwrap();
    let text = std::fs::read_to_string(&file).unwrap();
    assert_eq!(text.matches("[[models]]").count(), 1);
    assert!(text.contains("id = \"gpt-9\""));
    assert!(text.contains("provider = \"openai\""));
    assert!(!text.contains("context_window = None"));
}

#[test]
fn from_bro_json_imports_camel_case_models_json() {
    let t = ModelTable::from_bro_json(
        r#"{
          "models": [
            {"id":"custom-sol","provider":"openai","upstreamModel":"private-sol",
             "contextWindow":272000,"maxContextWindow":872000,"supportedEfforts":["low","max"]},
            {"id":"bad","provider":"nope","upstreamModel":"x"}
          ],
          "mappingEnabled": true,
          "mappings": [{"from":"sonnet","to":"custom-sol","effort":{"high":"max","low":"ultra"}}]
        }"#,
    );
    let d = t.resolve("custom-sol").def;
    assert_eq!(d.upstream_model, "private-sol");
    assert_eq!(
        (d.context_window, d.max_context_window),
        (Some(272_000), Some(872_000))
    );
    assert_eq!(d.supported_efforts, efforts(&["low", "max"]));
    assert!(t.rows().iter().all(|r| r.id != "bad"));
    assert!(t.mapping_enabled());
    assert_eq!(
        t.mapping_for("sonnet").unwrap().effort,
        Some([("high".to_string(), "max".to_string())].into())
    );
}

#[test]
fn corrupt_file_falls_back_to_defaults() {
    let dir = tempfile::tempdir().unwrap();
    let paths = Paths::under(dir.path());
    std::fs::write(dir.path().join("models.toml"), "this is = = not toml [[").unwrap();
    assert_eq!(ModelTable::load(&paths).unwrap(), defaults());
    assert_eq!(ModelTable::from_bro_json("{nope"), defaults());
}
