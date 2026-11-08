//! Codex-thinking stripping with byte-exact request passthrough (Task 17).
//!
//! When a conversation hops from a Codex (OpenAI) route back to Anthropic, the history can
//! contain `thinking` blocks that pooltime itself fabricated while translating Codex reasoning
//! items into Anthropic stream events. Anthropic rejects them with a retry-stable 400
//! (fake signature) during thinking-enabled tool-use continuations, so they must be removed.
//! Every other request is forwarded **byte-identical** to what the client sent.
//!
//! # Stash-signature format (ported from bro `codex-translate.ts`; Task 37 must produce it)
//!
//! A Codex reasoning item `{id, encrypted_content}` is carried on the Anthropic side as a
//! `thinking` block whose `signature` is the compact JSON text
//! `{"id":"<reasoning item id>","encrypted_content":"<opaque>"}` (key order `id`,
//! `encrypted_content`; see [`codex_stash_signature`], [`CODEX_STASH_PREFIX`]). The producer
//! emits it as a `signature_delta` and the Anthropic→Codex request translation parses it back.
//! Detection ([`is_codex_stash_signature`]) is by *parsing*, not by prefix: a signature is ours
//! iff it is a JSON object whose `id` and `encrypted_content` are both strings (extra keys are
//! tolerated). Native Anthropic signatures are opaque base64 and never parse that way.
//!
//! # What is stripped
//!
//! A `thinking` block in a message's `content` array is "ours" if its `signature` is a Codex
//! stash **or is not a string at all** (absent/null/other: native thinking always carries an
//! opaque string signature). Messages (any role) emptied by the strip are dropped. A content
//! array that *arrived* empty is the caller's own shape (the effort-only mid-conversation
//! `role:"system"` message) and is kept untouched. Dropping a message can strand a
//! mid-conversation `role:"system"` message in front of a non-assistant turn, which Anthropic
//! rejects, so such system messages are removed too (a system message must precede an assistant
//! message or end the array; empty-content system messages are exempt).
//!
//! # Byte passthrough
//!
//! [`prepare_body`] returns `raw.clone()` (a refcount bump, zero copy) unless something was
//! stripped. Only then is the parsed [`Value`] re-serialized; the crate enables `serde_json`
//! `preserve_order` (key order kept) and `arbitrary_precision` (numbers keep their literal text,
//! e.g. `1.0`, integers beyond `u64`) so even the re-serialized body differs only by the removal
//! and by value-preserving normalisation (insignificant whitespace, string escapes, and the
//! exponent sign: `1e2` -> `1e+2`).
//!
//! Tests ported from bro `upstream/codex-translate.test.ts` `describe("stripCodexThinking")`:
//! - `removes_codex_stashed_and_signature_less_thinking_blocks_keeps_native_ones`
//! - `drops_an_assistant_message_emptied_by_stripping_interrupted_codex_turn`
//! - `does_not_strand_a_mid_conversation_system_message_before_a_user_turn`
//! - `keeps_a_system_message_that_still_precedes_an_assistant_turn`
//! - `keeps_a_system_message_that_ends_the_array`
//! - `keeps_an_effort_only_system_message_sitting_in_front_of_a_user_turn`
//! - `returns_non_object_and_thinking_free_bodies_unchanged`
//!
//! Additional: `untouched_body_is_byte_identical`, `stash_signature_roundtrip_and_detection`,
//! `stripped_body_preserves_key_order_and_number_literals`.

use bytes::Bytes;
use serde_json::{json, Value};

/// Leading bytes of every signature [`codex_stash_signature`] produces. Informational only:
/// detection parses the JSON ([`is_codex_stash_signature`]).
pub const CODEX_STASH_PREFIX: &str = r#"{"id":""#;

/// Build the stash signature for a Codex reasoning item (what Task 37 puts in the
/// `signature_delta` / `thinking.signature`). Compact JSON, `id` first.
pub fn codex_stash_signature(id: &str, encrypted_content: &str) -> String {
    json!({ "id": id, "encrypted_content": encrypted_content }).to_string()
}

/// True iff `sig` is a Codex reasoning stash: a JSON object whose `id` and `encrypted_content`
/// are both strings.
pub fn is_codex_stash_signature(sig: &str) -> bool {
    match serde_json::from_str::<Value>(sig) {
        Ok(Value::Object(o)) => {
            matches!(o.get("id"), Some(Value::String(_)))
                && matches!(o.get("encrypted_content"), Some(Value::String(_)))
        }
        _ => false,
    }
}

/// A thinking block pooltime fabricated: stash signature, or no string signature at all.
fn is_ours(block: &Value) -> bool {
    block.get("type").and_then(Value::as_str) == Some("thinking")
        && match block.get("signature") {
            Some(Value::String(s)) => is_codex_stash_signature(s),
            _ => true,
        }
}

fn content_array(msg: &Value) -> Option<&Vec<Value>> {
    msg.get("content").and_then(Value::as_array)
}

/// The request body to forward upstream: `raw.clone()` when there is nothing to strip,
/// otherwise the re-serialized body with Codex-fabricated thinking removed. `json` must be the
/// parse of `raw`.
pub fn prepare_body(raw: &Bytes, json: &Value) -> Bytes {
    let Some(messages) = json.get("messages").and_then(Value::as_array) else {
        return raw.clone();
    };
    if !messages
        .iter()
        .any(|m| content_array(m).is_some_and(|c| c.iter().any(is_ours)))
    {
        return raw.clone();
    }

    // Only messages *we* empty are dropped; content arrays that arrived empty are kept.
    let stripped: Vec<Value> = messages
        .iter()
        .filter_map(|m| match content_array(m) {
            Some(c) if !c.is_empty() => {
                let kept: Vec<Value> = c.iter().filter(|b| !is_ours(b)).cloned().collect();
                if kept.is_empty() {
                    None
                } else {
                    let mut m = m.clone();
                    m["content"] = Value::Array(kept);
                    Some(m)
                }
            }
            _ => Some(m.clone()),
        })
        .collect();

    // Build back to front: a non-exempt system message must be followed by an assistant message
    // or end the array. Going backwards collapses a run of stranded system messages in one pass.
    let mut repaired: Vec<Value> = Vec::with_capacity(stripped.len());
    for msg in stripped.into_iter().rev() {
        let exempt = content_array(&msg).is_some_and(|c| c.is_empty());
        let stranded = repaired
            .last()
            .is_some_and(|next| next.get("role").and_then(Value::as_str) != Some("assistant"));
        if msg.get("role").and_then(Value::as_str) == Some("system") && !exempt && stranded {
            continue;
        }
        repaired.push(msg);
    }
    repaired.reverse();

    let mut out = json.clone();
    out["messages"] = Value::Array(repaired);
    match serde_json::to_vec(&out) {
        Ok(v) => Bytes::from(v),
        // Serializing a parsed Value cannot realistically fail; forward untouched if it does.
        Err(_) => raw.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stash() -> String {
        codex_stash_signature("rs_1", "enc")
    }

    /// Run `prepare_body` on a JSON value; returns (output value, whether the bytes were the
    /// very same allocation as the input).
    fn run(v: &Value) -> (Value, bool) {
        let raw = Bytes::from(serde_json::to_vec(v).unwrap());
        let out = prepare_body(&raw, v);
        let same = out.as_ptr() == raw.as_ptr() && out.len() == raw.len();
        (serde_json::from_slice(&out).unwrap(), same)
    }

    fn roles(v: &Value) -> Vec<&str> {
        v["messages"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["role"].as_str().unwrap())
            .collect()
    }

    #[test]
    fn untouched_body_is_byte_identical() {
        let raw = Bytes::from_static(
            b"{ \"model\" :\"claude-sonnet-5\",\n\t\"zeta\":1.0 ,\"alpha\": 1e2, \"max_tokens\":1.50,\n \"s\":\"caf\\u00e9 \\ud83d\\ude00\",\n\"messages\":[{\"role\":\"user\",\"content\":\"hi\"},\n {\"role\":\"assistant\",\"content\":[ {\"type\":\"thinking\",\"thinking\":\"n\",\"signature\":\"opaque-base64-sig\"},{\"type\":\"text\",\"text\":\"a\"}]}  ] , \"stream\":true }  \n",
        );
        let json: Value = serde_json::from_slice(&raw).unwrap();
        let out = prepare_body(&raw, &json);
        assert_eq!(out, raw);
        // Zero copy: same backing allocation.
        assert_eq!(out.as_ptr(), raw.as_ptr());
    }

    #[test]
    fn removes_codex_stashed_and_signature_less_thinking_blocks_keeps_native_ones() {
        let body = json!({
            "model": "claude-sonnet-5",
            "messages": [
                {"role": "user", "content": "hi"},
                {"role": "assistant", "content": [
                    {"type": "thinking", "thinking": "codex thought", "signature": stash()},
                    {"type": "thinking", "thinking": "orphaned"},
                    {"type": "thinking", "thinking": "native", "signature": "opaque-base64-sig"},
                    {"type": "text", "text": "answer"},
                ]},
            ],
        });
        let (out, same) = run(&body);
        assert!(!same);
        assert_eq!(
            out["messages"][1]["content"],
            json!([
                {"type": "thinking", "thinking": "native", "signature": "opaque-base64-sig"},
                {"type": "text", "text": "answer"},
            ])
        );
        assert_eq!(out["model"], "claude-sonnet-5");
        // Input value is not mutated.
        assert_eq!(body["messages"][1]["content"].as_array().unwrap().len(), 4);
    }

    #[test]
    fn drops_an_assistant_message_emptied_by_stripping_interrupted_codex_turn() {
        let (out, _) = run(&json!({"messages": [
            {"role": "user", "content": "go"},
            {"role": "assistant", "content": [{"type": "thinking", "thinking": "hmm", "signature": stash()}]},
            {"role": "user", "content": "continue"},
        ]}));
        assert_eq!(out["messages"].as_array().unwrap().len(), 2);
        assert_eq!(roles(&out), ["user", "user"]);
    }

    #[test]
    fn does_not_strand_a_mid_conversation_system_message_before_a_user_turn() {
        let (out, _) = run(&json!({"messages": [
            {"role": "user", "content": "go"},
            {"role": "system", "content": "session title reminder"},
            {"role": "assistant", "content": [{"type": "thinking", "thinking": "", "signature": stash()}]},
            {"role": "user", "content": "next"},
        ]}));
        assert_eq!(roles(&out), ["user", "user"]);
    }

    #[test]
    fn keeps_a_system_message_that_still_precedes_an_assistant_turn() {
        let (out, _) = run(&json!({"messages": [
            {"role": "user", "content": "go"},
            {"role": "assistant", "content": [{"type": "thinking", "thinking": "", "signature": stash()}]},
            {"role": "system", "content": "operator instruction"},
            {"role": "assistant", "content": [{"type": "text", "text": "hi"}]},
        ]}));
        assert_eq!(roles(&out), ["user", "system", "assistant"]);
    }

    #[test]
    fn keeps_a_system_message_that_ends_the_array() {
        let (out, _) = run(&json!({"messages": [
            {"role": "user", "content": "go"},
            {"role": "assistant", "content": [{"type": "thinking", "thinking": "", "signature": stash()}]},
            {"role": "user", "content": "next"},
            {"role": "system", "content": "trailing directive"},
        ]}));
        assert_eq!(roles(&out), ["user", "user", "system"]);
    }

    #[test]
    fn keeps_an_effort_only_system_message_sitting_in_front_of_a_user_turn() {
        let (out, _) = run(&json!({"messages": [
            {"role": "user", "content": "go"},
            {"role": "assistant", "content": [{"type": "thinking", "thinking": "", "signature": stash()}]},
            {"role": "system", "content": [], "output_config": {"effort": "low"}},
            {"role": "user", "content": "next"},
        ]}));
        assert_eq!(roles(&out), ["user", "system", "user"]);
        assert_eq!(
            out["messages"][1]["output_config"],
            json!({"effort": "low"})
        );
    }

    #[test]
    fn returns_non_object_and_thinking_free_bodies_unchanged() {
        for raw in [
            &b"null"[..],
            b"[1,2]",
            b"\"str\"",
            b"{\"messages\":\"nope\"}",
            b"{\"stream\":true}",
            b"{ \"messages\": [ {\"role\":\"user\",\"content\":\"hi\"}, null, 7 ], \"stream\": true }",
        ] {
            let raw = Bytes::copy_from_slice(raw);
            let json: Value = serde_json::from_slice(&raw).unwrap();
            let out = prepare_body(&raw, &json);
            assert_eq!(out, raw);
            assert_eq!(out.as_ptr(), raw.as_ptr());
        }
    }

    #[test]
    fn stash_signature_roundtrip_and_detection() {
        let sig = codex_stash_signature("rs_1", "enc\"quoted");
        assert!(sig.starts_with(CODEX_STASH_PREFIX));
        assert_eq!(sig, r#"{"id":"rs_1","encrypted_content":"enc\"quoted"}"#);
        assert!(is_codex_stash_signature(&sig));
        // Extra keys tolerated, as in bro.
        assert!(is_codex_stash_signature(
            r#"{"encrypted_content":"e","id":"i","x":1}"#
        ));
        for no in [
            "",
            "opaque-base64-sig",
            "null",
            "[]",
            "12",
            r#"{"id":"a"}"#,
            r#"{"id":1,"encrypted_content":"e"}"#,
            r#"{"id":"a","encrypted_content":null}"#,
        ] {
            assert!(!is_codex_stash_signature(no), "{no}");
        }
        // Non-string signatures count as "ours".
        assert!(is_ours(&json!({"type": "thinking", "signature": null})));
        assert!(is_ours(&json!({"type": "thinking", "signature": 5})));
        assert!(!is_ours(&json!({"type": "text"})));
        assert!(!is_ours(&json!(null)));
    }

    #[test]
    fn stripped_body_preserves_key_order_and_number_literals() {
        let raw = Bytes::from_static(
            br#"{"zeta":1.0,"alpha":1e2,"temperature":0.10,"messages":[{"role":"user","content":"hi"},{"role":"assistant","content":[{"type":"thinking","thinking":"x"}]},{"role":"user","content":"again"}],"big":12345678901234567890123}"#,
        );
        let json: Value = serde_json::from_slice(&raw).unwrap();
        let out = prepare_body(&raw, &json);
        assert_eq!(
            std::str::from_utf8(&out).unwrap(),
            // serde_json canonicalises the exponent sign (`1e2` -> `1e+2`, value-equal); every
            // other literal (`1.0`, `0.10`, a >u64 integer) survives verbatim.
            r#"{"zeta":1.0,"alpha":1e+2,"temperature":0.10,"messages":[{"role":"user","content":"hi"},{"role":"user","content":"again"}],"big":12345678901234567890123}"#
        );
    }
}
