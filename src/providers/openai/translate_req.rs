//! Anthropic Messages request -> Codex Responses request (INV §4 "Translation"). Pure: no IO.
//!
//! Ported from bro `codex-translate.ts` (`anthropicToCodexRequest`, `deriveEffortTier`,
//! `clampEffortToSupported`). The response/stream side is a separate task.
//!
//! # Output
//! Top-level fields, in this order: `model` (= `route.upstream_model`), `instructions`
//! (`system`; an array of blocks is joined with `\n`, empty when absent), `input`,
//! `stream:true`, `store:false`, `include:["reasoning.encrypted_content"]`, `reasoning`
//! (`{effort}`, only when an effort survives clamping), then only when the request has `tools`:
//! `tools` (`{type:function,name,description,strict:false,parameters}`), `tool_choice`
//! (`any`->`"required"`, `tool`+name->`{type:function,name}`, `none`->`"none"`, else `"auto"`),
//! `parallel_tool_calls:false`; finally `temperature` / `top_p` when numeric. **Never**
//! `max_output_tokens` (the ChatGPT Codex backend 400s on it); `max_tokens` is dropped.
//!
//! # Effort
//! `effort_override` is the already-resolved Codex effort for this request's source tier,
//! i.e. what the caller computes as
//! `effective_effort(&route, mapping, derive_effort_tier(body)).as_deref()` (a mapping row's
//! effort map replaces the route's `effort_map`; a tier with no override passes through 1:1;
//! tier `default` stays unset). `Some(e)` is used as-is; `None` means "no override", which falls
//! back to the request's own tier passed through 1:1 (`default` -> unset). The two readings agree
//! for what `effective_effort` returns: it yields `None` only for tier `default` with no override
//! entry, and `default` passes through as unset. The result is then clamped by [`clamp_effort`]
//! against `route.supported_efforts` (`max` -> `xhigh` when supported, anything else
//! unsupported is omitted; an empty list omits every effort). The caller needs the raw tier
//! (hence [`derive_effort_tier`] is public) but nothing else from the body.
//!
//! # Reasoning round-trip
//! A `thinking` block whose `signature` is the Codex stash (JSON object with string `id` and
//! `encrypted_content`, see `providers::anthropic::codex_stash_signature`) and which is followed
//! by a `text` / `tool_use` block in the same message becomes a `reasoning` input item (flushing
//! pending text parts first). Orphans (nothing follows) and foreign thinking are dropped.
//!
//! # Errors
//! Structurally malformed input (not an object, non-array `messages`/`tools`, non-object message
//! or block, unusable `content`, `tool_use` without string `id`/`name`, `tool_result` without
//! string `tool_use_id`) yields an `anyhow` error naming only the class of problem, never the
//! offending content. Unknown block types and unrecognized image sources degrade (dropped).

use anyhow::{bail, Result};
use serde_json::{json, Map, Value};

use crate::models::{RouteDef, SOURCE_EFFORT_TIERS};

/// Source effort tier of an Anthropic request: `output_config.effort` (what Claude Code sends
/// with adaptive thinking, unless `"default"` or unknown) > legacy `thinking.budget_tokens`
/// buckets (`<8192` low, `<16384` medium, `<32768` high, else xhigh) > `"default"`.
pub fn derive_effort_tier(body: &Value) -> &'static str {
    if let Some(e) = body
        .pointer("/output_config/effort")
        .and_then(Value::as_str)
        .filter(|e| *e != "default")
    {
        if let Some(tier) = SOURCE_EFFORT_TIERS.iter().find(|t| **t == e) {
            return tier;
        }
    }
    let thinking = body.get("thinking");
    if thinking.and_then(|t| t.get("type")).and_then(Value::as_str) == Some("enabled") {
        if let Some(budget) = thinking
            .and_then(|t| t.get("budget_tokens"))
            .and_then(Value::as_f64)
        {
            return if budget < 8192.0 {
                "low"
            } else if budget < 16384.0 {
                "medium"
            } else if budget < 32768.0 {
                "high"
            } else {
                "xhigh"
            };
        }
    }
    "default"
}

/// Normalise `effort` against a route's capabilities: supported values pass through, `max`
/// falls back to `xhigh` when that is supported, everything else is omitted (an unsupported
/// value would provoke an upstream 400).
pub fn clamp_effort(effort: Option<&str>, supported: &[String]) -> Option<String> {
    let effort = effort?;
    let has = |e: &str| supported.iter().any(|s| s == e);
    if has(effort) {
        Some(effort.to_string())
    } else if effort == "max" && has("xhigh") {
        Some("xhigh".to_string())
    } else {
        None
    }
}

/// Translate an Anthropic `/v1/messages` body into a Codex Responses request body. See the
/// module docs for the exact output and the meaning of `effort_override`.
pub fn to_codex_request(
    json: &Value,
    route: &RouteDef,
    effort_override: Option<&str>,
) -> Result<Value> {
    let Some(body) = json.as_object() else {
        bail!("codex request translation: request body is not a JSON object");
    };

    let input = translate_messages(body.get("messages"))?;

    let mut out = Map::new();
    out.insert("model".into(), json!(route.upstream_model));
    out.insert(
        "instructions".into(),
        json!(fold_system(body.get("system"))),
    );
    out.insert("input".into(), Value::Array(input));
    out.insert("stream".into(), json!(true));
    out.insert("store".into(), json!(false));
    out.insert("include".into(), json!(["reasoning.encrypted_content"]));

    // Deliberately no output-token cap: the Codex backend rejects `max_output_tokens`.
    let effort = match effort_override {
        Some(e) => Some(e),
        None => Some(derive_effort_tier(json)).filter(|t| *t != "default"),
    };
    if let Some(effort) = clamp_effort(effort, &route.supported_efforts) {
        out.insert("reasoning".into(), json!({ "effort": effort }));
    }

    match body.get("tools") {
        None | Some(Value::Null) => {}
        Some(Value::Array(tools)) if tools.is_empty() => {}
        Some(Value::Array(tools)) => {
            let mapped: Vec<Value> = tools
                .iter()
                .filter_map(|t| {
                    let name = t.get("name").and_then(Value::as_str)?;
                    Some(json!({
                        "type": "function",
                        "name": name,
                        "description": t.get("description").filter(|d| !d.is_null()).cloned().unwrap_or_else(|| json!("")),
                        "strict": false,
                        "parameters": t.get("input_schema").filter(|s| !s.is_null()).cloned().unwrap_or_else(|| json!({ "type": "object" })),
                    }))
                })
                .collect();
            out.insert("tools".into(), Value::Array(mapped));
            out.insert(
                "tool_choice".into(),
                map_tool_choice(body.get("tool_choice")),
            );
            out.insert("parallel_tool_calls".into(), json!(false));
        }
        Some(_) => bail!("codex request translation: tools is not an array"),
    }

    for key in ["temperature", "top_p"] {
        if let Some(v) = body.get(key).filter(|v| v.is_number()) {
            out.insert(key.into(), v.clone());
        }
    }
    Ok(Value::Object(out))
}

fn translate_messages(messages: Option<&Value>) -> Result<Vec<Value>> {
    let messages = match messages {
        None | Some(Value::Null) => return Ok(Vec::new()),
        Some(Value::Array(m)) => m,
        Some(_) => bail!("codex request translation: messages is not an array"),
    };
    let mut input = Vec::new();
    for m in messages {
        let Some(m) = m.as_object() else {
            bail!("codex request translation: message is not an object");
        };
        let role = if m.get("role").and_then(Value::as_str) == Some("assistant") {
            "assistant"
        } else {
            "user"
        };
        match m.get("content") {
            Some(Value::String(text)) => input.push(text_message(role, text)),
            Some(Value::Array(blocks)) => translate_blocks(role, blocks, &mut input)?,
            // bro skips a message with no content at all; anything else is malformed.
            None | Some(Value::Null) => {}
            Some(_) => {
                bail!("codex request translation: message content is neither a string nor an array")
            }
        }
    }
    Ok(input)
}

fn translate_blocks(role: &str, blocks: &[Value], input: &mut Vec<Value>) -> Result<()> {
    let text_kind = if role == "assistant" {
        "output_text"
    } else {
        "input_text"
    };
    // Text/image blocks accumulate as message parts; tool_use / tool_result / reasoning are
    // standalone items that flush any pending parts first.
    let mut parts: Vec<Value> = Vec::new();
    let flush = |parts: &mut Vec<Value>, input: &mut Vec<Value>| {
        if !parts.is_empty() {
            input
                .push(json!({ "type": "message", "role": role, "content": std::mem::take(parts) }));
        }
    };

    for (i, block) in blocks.iter().enumerate() {
        let Some(b) = block.as_object() else {
            bail!("codex request translation: content block is not an object");
        };
        let ty = b.get("type").and_then(Value::as_str);
        match ty {
            Some("text") if b.get("text").is_some_and(Value::is_string) => {
                let text = b["text"].as_str().unwrap_or_default();
                // Adjacent text folds into one part joined with "\n"; an image starts a new one.
                match parts.last_mut() {
                    Some(last) if last["type"] == text_kind => {
                        let joined =
                            format!("{}\n{text}", last["text"].as_str().unwrap_or_default());
                        last["text"] = Value::String(joined);
                    }
                    _ => parts.push(json!({ "type": text_kind, "text": text })),
                }
            }
            Some("image") if role == "user" => {
                if let Some(img) = image_part(block) {
                    parts.push(img);
                }
            }
            Some("tool_use") => {
                let Some(id) = b.get("id").and_then(Value::as_str) else {
                    bail!("codex request translation: tool_use block has no string id");
                };
                let Some(name) = b.get("name").and_then(Value::as_str) else {
                    bail!("codex request translation: tool_use block has no string name");
                };
                flush(&mut parts, input);
                let args = match b.get("input") {
                    None | Some(Value::Null) => "{}".to_string(),
                    Some(v) => v.to_string(),
                };
                input.push(json!({
                    "type": "function_call",
                    "call_id": id,
                    "name": name,
                    "arguments": args,
                }));
            }
            Some("thinking") => {
                // A reasoning item must be followed by another item of the same turn (Codex 400s
                // on a reasoning item "without its required following item"); an interrupted
                // turn can end on thinking.
                let has_following =
                    blocks[i + 1..]
                        .iter()
                        .any(|n| match n.get("type").and_then(Value::as_str) {
                            Some("text") => n.get("text").is_some_and(Value::is_string),
                            Some("tool_use") => true,
                            _ => false,
                        });
                let stash = has_following
                    .then(|| parse_reasoning_stash(b.get("signature")))
                    .flatten();
                if let Some((id, encrypted_content)) = stash {
                    flush(&mut parts, input);
                    let summary = match b.get("thinking").and_then(Value::as_str) {
                        Some(t) if !t.is_empty() => json!([{ "type": "summary_text", "text": t }]),
                        _ => json!([]),
                    };
                    input.push(json!({
                        "type": "reasoning",
                        "id": id,
                        "summary": summary,
                        "encrypted_content": encrypted_content,
                    }));
                }
            }
            Some("tool_result") => {
                let Some(call_id) = b.get("tool_use_id").and_then(Value::as_str) else {
                    bail!("codex request translation: tool_result block has no string tool_use_id");
                };
                flush(&mut parts, input);
                input.push(json!({
                    "type": "function_call_output",
                    "call_id": call_id,
                    "output": fold_tool_result_content(b.get("content")),
                }));
                // function_call_output.output is string-only, so images inside a tool_result
                // ride in a follow-up user message right after the tool output.
                if let Some(Value::Array(content)) = b.get("content") {
                    let images: Vec<Value> = content
                        .iter()
                        .filter(|c| c.get("type").and_then(Value::as_str) == Some("image"))
                        .filter_map(image_part)
                        .collect();
                    if !images.is_empty() {
                        input.push(json!({ "type": "message", "role": "user", "content": images }));
                    }
                }
            }
            // assistant images / unknown blocks: dropped (degrade gracefully)
            _ => {}
        }
    }
    flush(&mut parts, input);
    Ok(())
}

fn text_message(role: &str, text: &str) -> Value {
    let kind = if role == "assistant" {
        "output_text"
    } else {
        "input_text"
    };
    json!({ "type": "message", "role": role, "content": [{ "type": kind, "text": text }] })
}

/// An Anthropic image block as a Codex `input_image` part; `None` for unrecognised sources.
fn image_part(block: &Value) -> Option<Value> {
    let source = block.get("source")?;
    match source.get("type").and_then(Value::as_str)? {
        "base64" => {
            let data = source.get("data").and_then(Value::as_str)?;
            let media = source
                .get("media_type")
                .and_then(Value::as_str)
                .unwrap_or("image/png");
            Some(
                json!({ "type": "input_image", "image_url": format!("data:{media};base64,{data}") }),
            )
        }
        "url" => {
            let url = source.get("url").and_then(Value::as_str)?;
            Some(json!({ "type": "input_image", "image_url": url }))
        }
        _ => None,
    }
}

fn fold_system(system: Option<&Value>) -> String {
    match system {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(blocks)) => blocks
            .iter()
            .filter_map(|b| b.get("text").and_then(Value::as_str))
            .filter(|t| !t.is_empty())
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

fn fold_tool_result_content(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(blocks)) => blocks
            .iter()
            .filter_map(|b| b.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// Parse a thinking-block signature back into the Codex reasoning stash `(id,
/// encrypted_content)`; `None` for anything foreign.
fn parse_reasoning_stash(signature: Option<&Value>) -> Option<(String, String)> {
    let parsed: Value = serde_json::from_str(signature?.as_str()?).ok()?;
    let id = parsed.get("id")?.as_str()?;
    let enc = parsed.get("encrypted_content")?.as_str()?;
    Some((id.to_string(), enc.to_string()))
}

fn map_tool_choice(choice: Option<&Value>) -> Value {
    let c = choice.and_then(|c| c.get("type")).and_then(Value::as_str);
    match c {
        Some("any") => json!("required"),
        Some("tool") => match choice.and_then(|c| c.get("name")).and_then(Value::as_str) {
            Some(name) => json!({ "type": "function", "name": name }),
            None => json!("auto"),
        },
        Some("none") => json!("none"),
        _ => json!("auto"),
    }
}
