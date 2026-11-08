//! Anthropic Messages -> Codex Responses request translation (Task 36).
//!
//! Ported from bro `pool/src/upstream/codex-translate.test.ts` (request side). Name mapping
//! (bro test -> Rust test) is listed in the Task 36 report. Pure function: no network, no IO.

use pooltime::models::{effective_effort, Mapping, RouteDef};
use pooltime::providers::anthropic::codex_stash_signature;
use pooltime::providers::openai::{clamp_effort, derive_effort_tier, to_codex_request};
use pooltime::providers::ProviderId;
use serde_json::{json, Value};

fn efforts(list: &[&str]) -> Vec<String> {
    list.iter().map(|s| (*s).to_string()).collect()
}

fn route_with(upstream: &str, supported: &[&str]) -> RouteDef {
    RouteDef {
        id: upstream.into(),
        provider: ProviderId::OpenAi,
        upstream_model: upstream.into(),
        context_window: Some(272_000),
        max_context_window: None,
        supported_efforts: efforts(supported),
        effort_map: None,
    }
}

/// Conservative `none..xhigh` route (bro's DEFAULT_CODEX_SUPPORTED_EFFORTS).
fn route(upstream: &str) -> RouteDef {
    route_with(upstream, &["none", "low", "medium", "high", "xhigh"])
}

fn frontier(upstream: &str) -> RouteDef {
    route_with(upstream, &["low", "medium", "high", "xhigh", "max"])
}

fn tr(body: Value, upstream: &str) -> Value {
    to_codex_request(&body, &route(upstream), None).unwrap()
}

fn input_of(out: &Value) -> &Vec<Value> {
    out["input"].as_array().unwrap()
}

fn has_key(out: &Value, key: &str) -> bool {
    out.as_object().unwrap().contains_key(key)
}

// ---------------------------------------------------------------- anthropicToCodexRequest

#[test]
fn maps_system_text_turns_tools_and_forces_stream() {
    let out = tr(
        json!({
            "model": "gpt", "system": "be brief", "max_tokens": 100,
            "messages": [
                { "role": "user", "content": "hi" },
                { "role": "assistant", "content": [{ "type": "text", "text": "hello" }] },
                { "role": "user", "content": [{ "type": "text", "text": "use the tool" }] },
            ],
            "tools": [{ "name": "read_file", "description": "reads", "input_schema": { "type": "object" } }],
        }),
        "gpt-5.2-codex",
    );
    assert_eq!(out["model"], "gpt-5.2-codex");
    assert_eq!(out["instructions"], "be brief");
    assert_eq!(out["stream"], true);
    assert_eq!(out["store"], false);
    let input = input_of(&out);
    assert_eq!(input.len(), 3);
    assert_eq!(input[0]["type"], "message");
    assert_eq!(input[0]["role"], "user");
    assert_eq!(
        input[0]["content"],
        json!([{ "type": "input_text", "text": "hi" }])
    );
    assert_eq!(
        input[1]["content"],
        json!([{ "type": "output_text", "text": "hello" }])
    );
    assert_eq!(
        out["tools"],
        json!([{
            "type": "function", "name": "read_file", "description": "reads",
            "strict": false, "parameters": { "type": "object" }
        }])
    );
    assert_eq!(out["parallel_tool_calls"], false);
}

#[test]
fn maps_tool_choice_any_required_tool_function_none_none_default_auto() {
    let base = json!({
        "messages": [{ "role": "user", "content": "hi" }],
        "tools": [{ "name": "read_file", "description": "reads", "input_schema": { "type": "object" } }],
    });
    let with = |choice: Option<Value>| {
        let mut b = base.clone();
        if let Some(c) = choice {
            b["tool_choice"] = c;
        }
        tr(b, "m")["tool_choice"].clone()
    };
    assert_eq!(with(Some(json!({ "type": "any" }))), json!("required"));
    assert_eq!(
        with(Some(json!({ "type": "tool", "name": "read_file" }))),
        json!({ "type": "function", "name": "read_file" })
    );
    assert_eq!(with(Some(json!({ "type": "none" }))), json!("none"));
    assert_eq!(with(Some(json!({ "type": "auto" }))), json!("auto"));
    assert_eq!(with(None), json!("auto"));
}

#[test]
fn always_requests_encrypted_reasoning_content_store_false_statelessness() {
    let out = tr(
        json!({ "messages": [{ "role": "user", "content": "hi" }] }),
        "gpt-5.2-codex",
    );
    assert_eq!(out["include"], json!(["reasoning.encrypted_content"]));
}

#[test]
fn never_sends_an_output_token_cap_the_chatgpt_codex_backend_rejects_max_output_tokens() {
    for max_tokens in [4096, 1] {
        let out = tr(
            json!({ "max_tokens": max_tokens, "messages": [{ "role": "user", "content": "hi" }] }),
            "gpt-5.2-codex",
        );
        assert!(!has_key(&out, "max_output_tokens"));
        assert!(!has_key(&out, "max_tokens"));
    }
}

#[test]
fn adjacent_text_blocks_stay_joined_with_a_newline_in_a_single_part() {
    let out = tr(
        json!({ "messages": [{ "role": "user", "content": [
            { "type": "text", "text": "<system-reminder>x</system-reminder>" },
            { "type": "text", "text": "question" },
        ] }] }),
        "m",
    );
    let parts = input_of(&out)[0]["content"].as_array().unwrap();
    assert_eq!(parts.len(), 1);
    assert_eq!(
        parts[0]["text"],
        "<system-reminder>x</system-reminder>\nquestion"
    );
}

#[test]
fn an_intervening_image_splits_text_into_separate_parts() {
    let out = tr(
        json!({ "messages": [{ "role": "user", "content": [
            { "type": "text", "text": "before" },
            { "type": "image", "source": { "type": "base64", "media_type": "image/png", "data": "AA" } },
            { "type": "text", "text": "after" },
        ] }] }),
        "m",
    );
    let types: Vec<&str> = input_of(&out)[0]["content"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["type"].as_str().unwrap())
        .collect();
    assert_eq!(types, ["input_text", "input_image", "input_text"]);
}

#[test]
fn a_trailing_thinking_block_with_no_following_item_in_its_message_emits_no_orphan_reasoning_item()
{
    let signature = codex_stash_signature("rs_9", "enc");
    let out = tr(
        json!({ "messages": [
            { "role": "user", "content": "go" },
            // Interrupted turn: thinking is the last (only) block.
            { "role": "assistant", "content": [{ "type": "thinking", "thinking": "hmm", "signature": signature }] },
            { "role": "user", "content": "continue" },
        ] }),
        "m",
    );
    assert!(!input_of(&out).iter().any(|i| i["type"] == "reasoning"));
}

#[test]
fn reconstructs_reasoning_input_items_from_replayed_thinking_blocks() {
    let signature = codex_stash_signature("rs_1", "enc-blob");
    let out = tr(
        json!({ "messages": [
            { "role": "user", "content": "do it" },
            { "role": "assistant", "content": [
                { "type": "thinking", "thinking": "I should read the file", "signature": signature },
                { "type": "tool_use", "id": "call_1", "name": "read_file", "input": { "path": "a" } },
            ] },
            { "role": "user", "content": [{ "type": "tool_result", "tool_use_id": "call_1", "content": "data" }] },
        ] }),
        "gpt-5.2-codex",
    );
    let input = input_of(&out);
    // reasoning item must precede its paired function_call (Codex 400s otherwise)
    assert_eq!(
        input[1],
        json!({
            "type": "reasoning",
            "id": "rs_1",
            "summary": [{ "type": "summary_text", "text": "I should read the file" }],
            "encrypted_content": "enc-blob",
        })
    );
    assert_eq!(input[2]["type"], "function_call");
    assert_eq!(input[2]["call_id"], "call_1");
    assert_eq!(input[3]["type"], "function_call_output");
    assert_eq!(input[3]["call_id"], "call_1");
}

#[test]
fn drops_thinking_blocks_whose_signature_is_not_a_codex_reasoning_stash() {
    let out = tr(
        json!({ "messages": [{ "role": "assistant", "content": [
            { "type": "thinking", "thinking": "anthropic-native thought", "signature": "opaque-base64-not-json" },
            { "type": "text", "text": "hello" },
        ] }] }),
        "gpt-5.2-codex",
    );
    let input = input_of(&out);
    assert_eq!(input.len(), 1);
    assert_eq!(input[0]["type"], "message");
    assert_eq!(input[0]["role"], "assistant");
}

#[test]
fn maps_thinking_budget_tokens_onto_reasoning_effort_tiers() {
    let base = json!({ "messages": [{ "role": "user", "content": "hi" }] });
    let effort_of = |budget: u64| {
        let mut b = base.clone();
        b["thinking"] = json!({ "type": "enabled", "budget_tokens": budget });
        tr(b, "m")["reasoning"]["effort"].clone()
    };
    assert_eq!(effort_of(4000), "low"); // "think"
    assert_eq!(effort_of(10000), "medium"); // "megathink"
    assert_eq!(effort_of(31999), "high"); // "ultrathink"
                                          // No thinking requested: leave effort to the server default.
    assert!(!has_key(&tr(base, "m"), "reasoning"));
}

#[test]
fn user_image_blocks_become_input_image_parts_base64_and_url_sources() {
    let out = tr(
        json!({ "messages": [{ "role": "user", "content": [
            { "type": "text", "text": "what is this?" },
            { "type": "image", "source": { "type": "base64", "media_type": "image/png", "data": "AAAA" } },
            { "type": "image", "source": { "type": "url", "url": "https://x.test/pic.jpg" } },
        ] }] }),
        "m",
    );
    let input = input_of(&out);
    assert_eq!(input.len(), 1);
    let parts = input[0]["content"].as_array().unwrap();
    assert_eq!(
        parts[0],
        json!({ "type": "input_text", "text": "what is this?" })
    );
    assert_eq!(
        parts[1],
        json!({ "type": "input_image", "image_url": "data:image/png;base64,AAAA" })
    );
    assert_eq!(
        parts[2],
        json!({ "type": "input_image", "image_url": "https://x.test/pic.jpg" })
    );
}

#[test]
fn tool_result_image_blocks_are_re_injected_as_a_user_input_image_message() {
    let out = tr(
        json!({ "messages": [
            { "role": "assistant", "content": [{ "type": "tool_use", "id": "c1", "name": "screenshot", "input": {} }] },
            { "role": "user", "content": [{
                "type": "tool_result", "tool_use_id": "c1",
                "content": [
                    { "type": "text", "text": "captured" },
                    { "type": "image", "source": { "type": "base64", "media_type": "image/png", "data": "BBBB" } },
                ],
            }] },
        ] }),
        "m",
    );
    let input = input_of(&out);
    assert_eq!(input[1]["type"], "function_call_output");
    assert_eq!(input[1]["call_id"], "c1");
    assert_eq!(input[1]["output"], "captured");
    // function_call_output.output is string-only on Codex, so the image rides in a follow-up
    // user message right after the tool output.
    assert_eq!(input[2]["type"], "message");
    assert_eq!(input[2]["role"], "user");
    assert!(input[2]["content"]
        .as_array()
        .unwrap()
        .contains(&json!({ "type": "input_image", "image_url": "data:image/png;base64,BBBB" })));
}

#[test]
fn maps_tool_use_tool_result_round_trip() {
    let out = tr(
        json!({ "messages": [
            { "role": "assistant", "content": [{ "type": "tool_use", "id": "tu_1", "name": "read_file", "input": { "path": "a" } }] },
            { "role": "user", "content": [{ "type": "tool_result", "tool_use_id": "tu_1", "content": "file contents" }] },
        ] }),
        "gpt-5.2-codex",
    );
    let input = input_of(&out);
    assert_eq!(input[0]["type"], "function_call");
    assert_eq!(input[0]["call_id"], "tu_1");
    assert_eq!(input[0]["name"], "read_file");
    assert_eq!(input[0]["arguments"], r#"{"path":"a"}"#);
    assert_eq!(input[1]["type"], "function_call_output");
    assert_eq!(input[1]["call_id"], "tu_1");
    assert_eq!(input[1]["output"], "file contents");
}

// ---------------------------------------------------------------- deriveEffortTier

#[test]
fn output_config_effort_wins_over_thinking_budget() {
    assert_eq!(
        derive_effort_tier(&json!({
            "output_config": { "effort": "xhigh" },
            "thinking": { "type": "enabled", "budget_tokens": 4000 }
        })),
        "xhigh"
    );
    assert_eq!(
        derive_effort_tier(&json!({ "output_config": { "effort": "max" } })),
        "max"
    );
}

#[test]
fn output_config_effort_default_falls_through_to_thinking_budget() {
    assert_eq!(
        derive_effort_tier(&json!({
            "output_config": { "effort": "default" },
            "thinking": { "type": "enabled", "budget_tokens": 10000 }
        })),
        "medium"
    );
}

#[test]
fn legacy_budget_buckets_when_effort_absent() {
    let tier = |n: u64| {
        derive_effort_tier(&json!({ "thinking": { "type": "enabled", "budget_tokens": n } }))
    };
    assert_eq!(tier(4000), "low");
    assert_eq!(tier(10000), "medium");
    assert_eq!(tier(20000), "high");
    assert_eq!(tier(32768), "xhigh");
}

#[test]
fn bucket_boundaries_land_in_the_higher_tier() {
    let tier = |n: u64| {
        derive_effort_tier(&json!({ "thinking": { "type": "enabled", "budget_tokens": n } }))
    };
    assert_eq!(tier(8192), "medium");
    assert_eq!(tier(16384), "high");
}

#[test]
fn no_signal_and_junk_values_yield_default() {
    assert_eq!(derive_effort_tier(&json!({})), "default");
    assert_eq!(
        derive_effort_tier(&json!({ "output_config": { "effort": "ultra" } })),
        "default"
    );
    // budget ignored unless thinking is enabled and the budget is a number
    assert_eq!(
        derive_effort_tier(&json!({ "thinking": { "type": "disabled", "budget_tokens": 4000 } })),
        "default"
    );
    assert_eq!(
        derive_effort_tier(&json!({ "thinking": { "type": "enabled", "budget_tokens": "big" } })),
        "default"
    );
}

// ---------------------------------------------------------------- codexEffortFor (via effective_effort)

#[test]
fn explicit_mapping_entry_wins() {
    let mut def = route("m");
    def.effort_map = Some([("medium".to_string(), "high".to_string())].into());
    assert_eq!(
        effective_effort(&def, None, "medium").as_deref(),
        Some("high")
    );
    def.effort_map = Some([("default".to_string(), "low".to_string())].into());
    assert_eq!(
        effective_effort(&def, None, "default").as_deref(),
        Some("low")
    );
}

#[test]
fn pass_through_defaults_tiers_map_one_to_one_default_stays_unset() {
    let def = route("m");
    assert_eq!(effective_effort(&def, None, "low").as_deref(), Some("low"));
    assert_eq!(effective_effort(&def, None, "max").as_deref(), Some("max"));
    assert_eq!(effective_effort(&def, None, "default"), None);
}

// ---------------------------------------------------------------- clampEffortToSupported

#[test]
fn route_capabilities_determine_supported_efforts() {
    let frontier = efforts(&["low", "medium", "high", "xhigh", "max"]);
    let limited = efforts(&["none", "low", "medium", "high", "xhigh"]);
    assert_eq!(clamp_effort(Some("max"), &frontier).as_deref(), Some("max"));
    assert_eq!(
        clamp_effort(Some("max"), &limited).as_deref(),
        Some("xhigh")
    );
    assert_eq!(clamp_effort(Some("none"), &frontier), None);
    assert_eq!(
        clamp_effort(Some("none"), &limited).as_deref(),
        Some("none")
    );
    // Supported non-max efforts and None pass through untouched.
    assert_eq!(
        clamp_effort(Some("high"), &limited).as_deref(),
        Some("high")
    );
    assert_eq!(clamp_effort(None, &limited), None);
    // max without xhigh support -> omitted; empty capability list -> always omitted.
    assert_eq!(clamp_effort(Some("max"), &efforts(&["low"])), None);
    assert_eq!(clamp_effort(Some("low"), &[]), None);
}

// ---------------------------------------------------------------- reasoning.effort

fn msg() -> Value {
    json!({ "messages": [{ "role": "user", "content": "hi" }] })
}

fn with(extra: Value) -> Value {
    let mut b = msg();
    for (k, v) in extra.as_object().unwrap() {
        b[k] = v.clone();
    }
    b
}

/// What the caller (Task 38/39) does: tier -> effective_effort -> override.
fn translate_as_caller(body: &Value, def: &RouteDef, mapping: Option<&Mapping>) -> Value {
    let tier = derive_effort_tier(body);
    let ov = effective_effort(def, mapping, tier);
    to_codex_request(body, def, ov.as_deref()).unwrap()
}

#[test]
fn output_config_effort_flows_through_with_mapping_override() {
    let mut def = route("gpt-5.6-sol");
    def.effort_map = Some([("medium".to_string(), "high".to_string())].into());
    let out = translate_as_caller(
        &with(json!({ "output_config": { "effort": "medium" } })),
        &def,
        None,
    );
    assert_eq!(out["reasoning"], json!({ "effort": "high" }));
}

#[test]
fn astra_route_capability_preserves_max_reasoning() {
    let out = to_codex_request(
        &with(json!({ "output_config": { "effort": "max" } })),
        &frontier("gpt-6-astra"),
        None,
    )
    .unwrap();
    assert_eq!(out["reasoning"], json!({ "effort": "max" }));
}

#[test]
fn unsupported_none_from_a_persisted_astra_effort_map_is_omitted() {
    let mut def = frontier("gpt-6-astra");
    def.effort_map = Some([("high".to_string(), "none".to_string())].into());
    let out = translate_as_caller(
        &with(json!({ "output_config": { "effort": "high" } })),
        &def,
        None,
    );
    assert!(!has_key(&out, "reasoning"));
}

#[test]
fn no_effort_signal_leaves_reasoning_unset() {
    let out = tr(msg(), "gpt-5.6-sol");
    assert!(!has_key(&out, "reasoning"));
}

#[test]
fn legacy_thinking_budget_still_buckets_existing_behavior_preserved() {
    let out = tr(
        with(json!({ "thinking": { "type": "enabled", "budget_tokens": 10000 } })),
        "gpt-5.6-sol",
    );
    assert_eq!(out["reasoning"], json!({ "effort": "medium" }));
}

#[test]
fn mapping_override_applies_before_the_per_model_clamp_high_to_max_clamped_to_xhigh_on_gpt_5_5() {
    let mut def = route("gpt-5.5");
    def.effort_map = Some([("high".to_string(), "max".to_string())].into());
    let out = translate_as_caller(
        &with(json!({ "output_config": { "effort": "high" } })),
        &def,
        None,
    );
    assert_eq!(out["reasoning"], json!({ "effort": "xhigh" }));
}

#[test]
fn pass_through_max_on_a_limited_gpt_5_4_mini_route_clamps_to_xhigh() {
    let out = tr(
        with(json!({ "output_config": { "effort": "max" } })),
        "gpt-5.4-mini",
    );
    assert_eq!(out["reasoning"], json!({ "effort": "xhigh" }));
}

// ---------------------------------------------------------------- additional (not in bro)

#[test]
fn override_wins_over_the_requests_own_effort_signal() {
    let body = with(json!({ "output_config": { "effort": "low" } }));
    let out = to_codex_request(&body, &route("m"), Some("high")).unwrap();
    assert_eq!(out["reasoning"], json!({ "effort": "high" }));
}

#[test]
fn route_effort_map_for_the_default_tier_reaches_codex_via_the_override() {
    // No effort signal in the request (tier "default"); the route map's default entry applies.
    let mut def = route("m");
    def.effort_map = Some([("default".to_string(), "low".to_string())].into());
    let out = translate_as_caller(&msg(), &def, None);
    assert_eq!(out["reasoning"], json!({ "effort": "low" }));
}

#[test]
fn system_array_joins_text_blocks_with_newline_skipping_empty_and_non_text() {
    let out = tr(
        json!({
            "system": [
                { "type": "text", "text": "one" },
                { "type": "text", "text": "" },
                { "type": "image" },
                { "type": "text", "text": "two", "cache_control": { "type": "ephemeral" } },
            ],
            "messages": [{ "role": "user", "content": "hi" }],
        }),
        "m",
    );
    assert_eq!(out["instructions"], "one\ntwo");
}

#[test]
fn missing_system_yields_empty_instructions() {
    assert_eq!(tr(msg(), "m")["instructions"], "");
}

#[test]
fn temperature_and_top_p_pass_through_and_are_otherwise_absent() {
    let out = tr(with(json!({ "temperature": 0.25, "top_p": 0.9 })), "m");
    assert_eq!(out["temperature"], json!(0.25));
    assert_eq!(out["top_p"], json!(0.9));
    let out = tr(msg(), "m");
    assert!(!has_key(&out, "temperature"));
    assert!(!has_key(&out, "top_p"));
    let out = tr(with(json!({ "temperature": "hot" })), "m");
    assert!(!has_key(&out, "temperature"));
}

#[test]
fn no_tools_means_no_tool_fields() {
    let out = tr(msg(), "m");
    assert!(!has_key(&out, "tools"));
    assert!(!has_key(&out, "tool_choice"));
    assert!(!has_key(&out, "parallel_tool_calls"));
}

#[test]
fn tool_defaults_description_and_schema_and_skips_nameless_tools() {
    let out = tr(
        with(json!({ "tools": [
            { "name": "a" },
            { "description": "nameless" },
            { "name": "b", "description": "d", "input_schema": { "type": "object", "properties": { "x": {} } } },
        ] })),
        "m",
    );
    assert_eq!(
        out["tools"],
        json!([
            { "type": "function", "name": "a", "description": "", "strict": false, "parameters": { "type": "object" } },
            { "type": "function", "name": "b", "description": "d", "strict": false,
              "parameters": { "type": "object", "properties": { "x": {} } } },
        ])
    );
}

#[test]
fn tool_choice_tool_without_a_name_degrades_to_auto() {
    let out = tr(
        with(json!({ "tools": [{ "name": "a" }], "tool_choice": { "type": "tool" } })),
        "m",
    );
    assert_eq!(out["tool_choice"], "auto");
}

#[test]
fn reasoning_flushes_pending_text_parts_and_keeps_order() {
    let signature = codex_stash_signature("rs_2", "e2");
    let out = tr(
        json!({ "messages": [{ "role": "assistant", "content": [
            { "type": "text", "text": "first" },
            { "type": "thinking", "thinking": "", "signature": signature },
            { "type": "text", "text": "second" },
        ] }] }),
        "m",
    );
    let input = input_of(&out);
    assert_eq!(input.len(), 3);
    assert_eq!(
        input[0]["content"],
        json!([{ "type": "output_text", "text": "first" }])
    );
    // empty thinking text -> empty summary
    assert_eq!(
        input[1],
        json!({ "type": "reasoning", "id": "rs_2", "summary": [], "encrypted_content": "e2" })
    );
    assert_eq!(
        input[2]["content"],
        json!([{ "type": "output_text", "text": "second" }])
    );
}

#[test]
fn thinking_followed_only_by_foreign_blocks_is_still_an_orphan() {
    let signature = codex_stash_signature("rs_3", "e3");
    let out = tr(
        json!({ "messages": [{ "role": "assistant", "content": [
            { "type": "thinking", "thinking": "t", "signature": signature },
            { "type": "redacted_thinking", "data": "x" },
        ] }] }),
        "m",
    );
    assert!(input_of(&out).is_empty());
}

#[test]
fn signature_missing_non_string_or_partial_is_foreign_and_dropped() {
    for sig in [
        json!(null),
        json!(42),
        json!(r#"{"id":"rs"}"#),
        json!(r#"{"id":1,"encrypted_content":"e"}"#),
        json!(r#"["id","encrypted_content"]"#),
    ] {
        let out = tr(
            json!({ "messages": [{ "role": "assistant", "content": [
                { "type": "thinking", "thinking": "t", "signature": sig },
                { "type": "text", "text": "x" },
            ] }] }),
            "m",
        );
        assert_eq!(input_of(&out).len(), 1, "signature {sig}");
    }
    let out = tr(
        json!({ "messages": [{ "role": "assistant", "content": [
            { "type": "thinking", "thinking": "t" },
            { "type": "text", "text": "x" },
        ] }] }),
        "m",
    );
    assert_eq!(input_of(&out).len(), 1);
}

#[test]
fn tool_result_content_folds_text_blocks_and_tolerates_missing_content() {
    let out = tr(
        json!({ "messages": [{ "role": "user", "content": [
            { "type": "tool_result", "tool_use_id": "a", "content": [
                { "type": "text", "text": "l1" }, { "type": "text", "text": "l2" } ] },
            { "type": "tool_result", "tool_use_id": "b" },
            { "type": "text", "text": "after" },
        ] }] }),
        "m",
    );
    let input = input_of(&out);
    assert_eq!(input[0]["output"], "l1\nl2");
    assert_eq!(input[1]["output"], "");
    assert_eq!(input[2]["type"], "message");
}

#[test]
fn assistant_images_unknown_blocks_and_unrecognized_image_sources_are_dropped() {
    let out = tr(
        json!({ "messages": [
            { "role": "assistant", "content": [
                { "type": "image", "source": { "type": "url", "url": "https://x.test/a.png" } },
                { "type": "server_tool_use", "id": "x" },
                { "type": "text", "text": "ok" },
            ] },
            { "role": "user", "content": [
                { "type": "image", "source": { "type": "file", "file_id": "f" } },
                { "type": "text", "text": "q" },
            ] },
        ] }),
        "m",
    );
    let input = input_of(&out);
    assert_eq!(input.len(), 2);
    assert_eq!(input[0]["content"].as_array().unwrap().len(), 1);
    assert_eq!(input[1]["content"].as_array().unwrap().len(), 1);
}

#[test]
fn base64_image_without_media_type_defaults_to_png() {
    let out = tr(
        json!({ "messages": [{ "role": "user", "content": [
            { "type": "image", "source": { "type": "base64", "data": "Zg==" } } ] }] }),
        "m",
    );
    assert_eq!(
        input_of(&out)[0]["content"][0]["image_url"],
        "data:image/png;base64,Zg=="
    );
}

#[test]
fn tool_use_input_keeps_key_order_and_defaults_to_empty_object() {
    let out = tr(
        json!({ "messages": [{ "role": "assistant", "content": [
            { "type": "tool_use", "id": "t1", "name": "n", "input": { "z": 1, "a": [true, null] } },
            { "type": "tool_use", "id": "t2", "name": "n" },
        ] }] }),
        "m",
    );
    let input = input_of(&out);
    assert_eq!(input[0]["arguments"], r#"{"z":1,"a":[true,null]}"#);
    assert_eq!(input[1]["arguments"], "{}");
}

#[test]
fn emits_exactly_the_documented_top_level_field_set_in_order() {
    let out = to_codex_request(
        &json!({
            "model": "claude-x", "max_tokens": 5, "system": "s", "temperature": 1, "top_p": 0.5,
            "output_config": { "effort": "high" },
            "messages": [{ "role": "user", "content": "hi" }],
            "tools": [{ "name": "t" }],
            "metadata": { "user_id": "u" }, "stop_sequences": ["x"], "stream": false,
        }),
        &route("gpt-5.2-codex"),
        Some("high"),
    )
    .unwrap();
    let keys: Vec<&str> = out
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(
        keys,
        [
            "model",
            "instructions",
            "input",
            "stream",
            "store",
            "include",
            "reasoning",
            "tools",
            "tool_choice",
            "parallel_tool_calls",
            "temperature",
            "top_p"
        ]
    );
    assert_eq!(out["stream"], true);
}

#[test]
fn full_request_body_snapshot() {
    let signature = codex_stash_signature("rs_1", "enc-blob");
    let out = to_codex_request(
        &json!({
            "model": "gpt-sol", "max_tokens": 64000,
            "system": [{ "type": "text", "text": "You are helpful." }, { "type": "text", "text": "Be brief." }],
            "output_config": { "effort": "max" },
            "temperature": 0.5,
            "tool_choice": { "type": "any" },
            "tools": [{ "name": "read_file", "description": "reads a file",
                         "input_schema": { "type": "object", "properties": { "path": { "type": "string" } }, "required": ["path"] } }],
            "messages": [
                { "role": "user", "content": [
                    { "type": "text", "text": "look at this" },
                    { "type": "image", "source": { "type": "base64", "media_type": "image/jpeg", "data": "QUJD" } },
                ] },
                { "role": "assistant", "content": [
                    { "type": "thinking", "thinking": "need the file", "signature": signature },
                    { "type": "text", "text": "reading" },
                    { "type": "tool_use", "id": "call_1", "name": "read_file", "input": { "path": "a.rs" } },
                ] },
                { "role": "user", "content": [
                    { "type": "tool_result", "tool_use_id": "call_1", "content": [
                        { "type": "text", "text": "fn main() {}" },
                        { "type": "image", "source": { "type": "url", "url": "https://x.test/shot.png" } },
                    ] },
                    { "type": "text", "text": "thanks" },
                ] },
            ],
        }),
        &frontier("gpt-5.6-sol"),
        None,
    )
    .unwrap();
    insta::assert_snapshot!(serde_json::to_string_pretty(&out).unwrap());
}

// ---------------------------------------------------------------- errors

fn err_of(body: Value) -> String {
    to_codex_request(&body, &route("m"), None)
        .unwrap_err()
        .to_string()
}

#[test]
fn malformed_input_is_a_classified_error_that_never_echoes_content() {
    let secret = "SECRET-PROMPT-TEXT";
    let cases = [
        (json!([secret]), "request body is not a JSON object"),
        (json!({ "messages": secret }), "messages is not an array"),
        (json!({ "messages": [secret] }), "message is not an object"),
        (
            json!({ "messages": [{ "role": "user", "content": 7, "x": secret }] }),
            "message content is neither a string nor an array",
        ),
        (
            json!({ "messages": [{ "role": "user", "content": [secret] }] }),
            "content block is not an object",
        ),
        (
            json!({ "messages": [{ "role": "assistant", "content": [{ "type": "tool_use", "name": "n", "x": secret }] }] }),
            "tool_use block has no string id",
        ),
        (
            json!({ "messages": [{ "role": "assistant", "content": [{ "type": "tool_use", "id": "i", "x": secret }] }] }),
            "tool_use block has no string name",
        ),
        (
            json!({ "messages": [{ "role": "user", "content": [{ "type": "tool_result", "x": secret }] }] }),
            "tool_result block has no string tool_use_id",
        ),
        (
            json!({ "messages": [], "tools": secret }),
            "tools is not an array",
        ),
    ];
    for (body, expect) in cases {
        let e = err_of(body);
        assert!(e.contains(expect), "{e:?} should contain {expect:?}");
        assert!(!e.contains(secret), "error echoes content: {e}");
    }
}

#[test]
fn absent_messages_yield_an_empty_input() {
    let out = tr(json!({ "system": "s" }), "m");
    assert_eq!(out["input"], json!([]));
}
