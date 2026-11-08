//! Codex Responses SSE -> Anthropic Messages SSE translation + context carry (Task 37).
//!
//! Ported from bro `pool/src/upstream/codex-translate.test.ts` (`CodexToAnthropicStream`) and
//! the translator-level parts of `openai-codex.test.ts`. Name mapping (bro test -> Rust test)
//! is listed in the Task 37 report. Pure state machine: no network, no IO.

use bytes::Bytes;
use pooltime::accounts::TokenUsage;
use pooltime::models::RouteDef;
use pooltime::providers::anthropic::codex_stash_signature;
use pooltime::providers::openai::{to_codex_request, CodexToAnthropic, ContextCarry, StreamError};
use pooltime::providers::ProviderId;
use pooltime::sse::{SseEvent, SseParser};
use serde_json::{json, Value};

// ---- helpers ----------------------------------------------------------------

/// A Codex event the way the upstream sends it: `type` inside `data`, no `event:` field.
fn ev(data: Value) -> SseEvent {
    SseEvent {
        event: None,
        data: data.to_string(),
        raw: Bytes::new(),
    }
}

/// Decode one emitted frame into `(event name, data JSON)`.
fn parse(frame: &Bytes) -> (String, Value) {
    let mut p = SseParser::default();
    let mut evs = p.push(frame).unwrap();
    evs.extend(p.finish().unwrap());
    assert_eq!(evs.len(), 1, "one frame = one event: {frame:?}");
    let e = &evs[0];
    let data: Value = serde_json::from_str(&e.data).unwrap();
    let name = e.event.clone().unwrap();
    assert_eq!(data["type"], json!(name), "event name matches data.type");
    (name, data)
}

fn types(frames: &[Bytes]) -> Vec<String> {
    frames.iter().map(|f| parse(f).0).collect()
}

fn datas(frames: &[Bytes]) -> Vec<Value> {
    frames.iter().map(|f| parse(f).1).collect()
}

fn created(id: &str) -> SseEvent {
    ev(json!({"type": "response.created", "response": {"id": id}}))
}

fn added_message() -> SseEvent {
    ev(json!({"type": "response.output_item.added", "item": {"type": "message"}}))
}

fn added_call(call_id: &str, name: &str) -> SseEvent {
    ev(json!({
        "type": "response.output_item.added",
        "item": {"type": "function_call", "call_id": call_id, "name": name}
    }))
}

fn text_delta(t: &str) -> SseEvent {
    ev(json!({"type": "response.output_text.delta", "delta": t}))
}

fn args_delta(t: &str) -> SseEvent {
    ev(json!({"type": "response.function_call_arguments.delta", "delta": t}))
}

fn done(item_type: &str) -> SseEvent {
    ev(json!({"type": "response.output_item.done", "item": {"type": item_type}}))
}

fn completed(usage: Value) -> SseEvent {
    ev(json!({"type": "response.completed", "response": {"usage": usage}}))
}

fn capped(event_type: &str, usage: Value) -> SseEvent {
    ev(json!({
        "type": event_type,
        "response": {
            "status": "incomplete",
            "incomplete_details": {"reason": "max_output_tokens"},
            "usage": usage
        }
    }))
}

/// Feed events; collect every emitted frame.
fn feed(s: &mut CodexToAnthropic, events: &[SseEvent]) -> Vec<Bytes> {
    events.iter().flat_map(|e| s.on_event(e)).collect()
}

/// bro's default success fixture: one text block `Hi`, usage 3 in / 1 out.
fn ok_events() -> Vec<SseEvent> {
    vec![
        created("r1"),
        added_message(),
        text_delta("Hi"),
        done("message"),
        completed(json!({"input_tokens": 3, "output_tokens": 1})),
    ]
}

fn opening_input(frames: &[Bytes]) -> Value {
    let (name, data) = parse(&frames[0]);
    assert_eq!(name, "message_start");
    data["message"]["usage"].clone()
}

// ---- codex-translate.test.ts: CodexToAnthropicStream --------------------------

#[test]
fn premature_eof_is_an_error_whether_or_not_message_start_was_emitted() {
    let before_start = CodexToAnthropic::new("gpt", None);
    let mut after_start = CodexToAnthropic::new("gpt", None);
    after_start.on_event(&created("r1"));
    for s in [before_start, after_start] {
        let err = s.finish().unwrap_err();
        assert_eq!(err, StreamError::MissingTerminal);
        assert!(err.message().contains("before a terminal event"));
        let (name, data) = parse(&err.to_frame());
        assert_eq!(name, "error");
        assert_eq!(data["error"]["type"], "api_error");
    }
}

#[test]
fn completion_without_usage_closes_cleanly_and_ignores_trailing_events() {
    let mut s = CodexToAnthropic::new("gpt", None);
    feed(
        &mut s,
        &[
            created("r1"),
            ev(json!({"type": "response.completed", "response": {"status": "completed"}})),
        ],
    );
    assert!(s.is_terminal());
    assert!(s.on_event(&added_message()).is_empty());
    assert_eq!(s.context_tokens(), None, "no terminal usage");
    assert_eq!(
        types(&s.finish().unwrap()),
        ["message_delta", "message_stop"]
    );
}

#[test]
fn text_turn_produces_well_formed_anthropic_sse_sequence() {
    let mut s = CodexToAnthropic::new("gpt", None);
    let mut frames = feed(
        &mut s,
        &[
            created("r1"),
            added_message(),
            text_delta("Hel"),
            text_delta("lo"),
            done("message"),
            completed(json!({"input_tokens": 10, "output_tokens": 5})),
        ],
    );
    assert_eq!(s.stop_reason(), Some("end_turn"));
    frames.extend(s.finish().unwrap());
    assert_eq!(
        types(&frames),
        [
            "message_start",
            "content_block_start",
            "content_block_delta",
            "content_block_delta",
            "content_block_stop",
            "message_delta",
            "message_stop"
        ]
    );
    let d = datas(&frames);
    assert_eq!(
        d[0],
        json!({
            "type": "message_start",
            "message": {
                "id": "r1", "type": "message", "role": "assistant", "model": "gpt",
                "content": [], "stop_reason": null, "stop_sequence": null,
                "usage": {"input_tokens": 0, "output_tokens": 0}
            }
        })
    );
    assert_eq!(
        d[1],
        json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}})
    );
    assert_eq!(
        d[2],
        json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": "Hel"}})
    );
    assert_eq!(d[4], json!({"type": "content_block_stop", "index": 0}));
    assert_eq!(
        d[5],
        json!({
            "type": "message_delta",
            "delta": {"stop_reason": "end_turn", "stop_sequence": null},
            "usage": {"input_tokens": 10, "output_tokens": 5}
        })
    );
    assert_eq!(d[6], json!({"type": "message_stop"}));
}

#[test]
fn message_start_carries_the_prior_turn_context_until_terminal_usage_arrives() {
    let mut s = CodexToAnthropic::new("gpt", Some(42));
    let frames = s.on_event(&created("r1"));
    assert_eq!(
        opening_input(&frames),
        json!({"input_tokens": 42, "output_tokens": 0})
    );
    assert_eq!(s.context_tokens(), None);
    s.on_event(&completed(json!({"input_tokens": 10, "output_tokens": 5})));
    assert_eq!(s.context_tokens(), Some(15));
    assert_eq!(
        s.usage(),
        TokenUsage {
            input_tokens: 10,
            output_tokens: 5,
            cache_read: 0,
            cache_write: 0
        }
    );
}

#[test]
fn forced_message_start_carries_the_same_prior_turn_context() {
    let mut s = CodexToAnthropic::new("gpt", Some(42));
    let start = s.force_message_start();
    assert_eq!(
        opening_input(&[start]),
        json!({"input_tokens": 42, "output_tokens": 0})
    );
    // No second message_start: the late response.created is dropped (so it reads as a ping).
    assert_eq!(types(&s.on_event(&created("r1"))), ["ping"]);
}

#[test]
fn missing_terminal_input_usage_is_not_authoritative() {
    let mut s = CodexToAnthropic::new("gpt", Some(42));
    feed(
        &mut s,
        &[created("r1"), completed(json!({"output_tokens": 5}))],
    );
    assert_eq!(s.context_tokens(), None);
    // Account view: the carried baseline is not recounted as input.
    assert_eq!(s.usage().input_tokens, 0);
    assert_eq!(s.usage().output_tokens, 5);
    // Client view keeps the seeded baseline.
    let frames = s.finish().unwrap();
    assert_eq!(
        parse(&frames[0]).1["usage"],
        json!({"input_tokens": 42, "output_tokens": 5})
    );
}

#[test]
fn function_call_maps_to_tool_use_block_and_stop_reason_tool_use() {
    let mut s = CodexToAnthropic::new("gpt", None);
    let mut frames = feed(
        &mut s,
        &[
            created("r1"),
            added_call("c1", "read_file"),
            args_delta(r#"{"path":"#),
            args_delta(r#""a"}"#),
            done("function_call"),
            completed(json!({"input_tokens": 1, "output_tokens": 2})),
        ],
    );
    assert_eq!(s.stop_reason(), Some("tool_use"));
    frames.extend(s.finish().unwrap());
    let d = datas(&frames);
    let start = d
        .iter()
        .find(|d| d["type"] == "content_block_start")
        .unwrap();
    assert_eq!(
        start["content_block"],
        json!({"type": "tool_use", "id": "c1", "name": "read_file", "input": {}})
    );
    let joined: String = d
        .iter()
        .filter(|d| d["type"] == "content_block_delta")
        .map(|d| {
            assert_eq!(d["delta"]["type"], "input_json_delta");
            d["delta"]["partial_json"].as_str().unwrap().to_string()
        })
        .collect();
    assert_eq!(joined, r#"{"path":"a"}"#);
    let delta = d.iter().find(|d| d["type"] == "message_delta").unwrap();
    assert_eq!(delta["delta"]["stop_reason"], "tool_use");
}

#[test]
fn to_anthropic_message_folds_structured_state_into_a_non_stream_message_text() {
    let mut s = CodexToAnthropic::new("gpt", None);
    feed(
        &mut s,
        &[
            created("r1"),
            added_message(),
            text_delta("Hi"),
            done("message"),
            completed(json!({"input_tokens": 3, "output_tokens": 1})),
        ],
    );
    assert_eq!(
        s.to_message(),
        json!({
            "id": "r1", "type": "message", "role": "assistant", "model": "gpt",
            "content": [{"type": "text", "text": "Hi"}],
            "stop_reason": "end_turn", "stop_sequence": null,
            "usage": {"input_tokens": 3, "output_tokens": 1}
        })
    );
}

#[test]
fn to_anthropic_message_parses_tool_use_input_from_accumulated_function_call_arguments() {
    let mut s = CodexToAnthropic::new("gpt", None);
    feed(
        &mut s,
        &[
            created("r1"),
            added_call("c1", "read_file"),
            args_delta(r#"{"path":"#),
            args_delta(r#""a"}"#),
            done("function_call"),
            completed(json!({"input_tokens": 1, "output_tokens": 2})),
        ],
    );
    let msg = s.into_message().unwrap();
    assert_eq!(msg["id"], "r1");
    assert_eq!(
        msg["content"],
        json!([{"type": "tool_use", "id": "c1", "name": "read_file", "input": {"path": "a"}}])
    );
    assert_eq!(msg["stop_reason"], "tool_use");
    assert_eq!(msg["usage"], json!({"input_tokens": 1, "output_tokens": 2}));
}

#[test]
fn force_message_start_emits_an_envelope_and_the_later_response_created_is_a_no_op() {
    let mut s = CodexToAnthropic::new("gpt", None);
    let forced = s.force_message_start();
    let (name, data) = parse(&forced);
    assert_eq!(name, "message_start");
    assert_eq!(data["message"]["type"], "message");
    assert_eq!(data["message"]["role"], "assistant");
    assert_eq!(data["message"]["model"], "gpt");
    assert!(data["message"]["id"].as_str().unwrap().starts_with("msg_"));
    assert!(s.is_started());

    // No second message_start: the late response.created is dropped (so it reads as a ping).
    assert_eq!(types(&s.on_event(&created("r1"))), ["ping"]);

    let mut rest = feed(
        &mut s,
        &[
            added_message(),
            text_delta("Hi"),
            done("message"),
            ev(json!({"type": "response.completed", "response": {"status": "completed"}})),
        ],
    );
    rest.extend(s.finish().unwrap());
    assert_eq!(
        types(&rest),
        [
            "content_block_start",
            "content_block_delta",
            "content_block_stop",
            "message_delta",
            "message_stop"
        ]
    );
}

#[test]
fn force_message_start_is_a_no_op_once_the_stream_has_already_started() {
    let mut s = CodexToAnthropic::new("gpt", None);
    s.on_event(&created("r1"));
    assert!(s.force_message_start().is_empty());
}

#[test]
fn reasoning_item_becomes_a_thinking_block_carrying_encrypted_content_in_its_signature() {
    let mut s = CodexToAnthropic::new("gpt", None);
    let mut frames = feed(
        &mut s,
        &[
            created("r1"),
            ev(
                json!({"type": "response.output_item.added", "item": {"type": "reasoning", "id": "rs_1"}}),
            ),
            ev(json!({"type": "response.reasoning_summary_text.delta", "delta": "planning "})),
            ev(json!({"type": "response.reasoning_summary_text.delta", "delta": "the read"})),
            ev(json!({
                "type": "response.output_item.done",
                "item": {"type": "reasoning", "id": "rs_1", "encrypted_content": "enc-blob", "summary": []}
            })),
            added_call("c1", "read_file"),
            args_delta("{}"),
            done("function_call"),
            completed(json!({"input_tokens": 1, "output_tokens": 2})),
        ],
    );
    // The translator is Clone: finish one copy for the stream, fold the other.
    frames.extend(s.clone().finish().unwrap());
    let d = datas(&frames);
    let start = d
        .iter()
        .find(|d| d["type"] == "content_block_start" && d["content_block"]["type"] == "thinking")
        .unwrap();
    assert_eq!(start["index"], 0);
    assert_eq!(
        start["content_block"],
        json!({"type": "thinking", "thinking": ""})
    );
    let thinking: String = d
        .iter()
        .filter(|d| d["type"] == "content_block_delta" && d["delta"]["type"] == "thinking_delta")
        .map(|d| d["delta"]["thinking"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(thinking, "planning the read");
    let sig = d
        .iter()
        .find(|d| d["type"] == "content_block_delta" && d["delta"]["type"] == "signature_delta")
        .unwrap();
    assert_eq!(sig["index"], 0);
    let sig_text = sig["delta"]["signature"].as_str().unwrap();
    assert_eq!(sig_text, codex_stash_signature("rs_1", "enc-blob"));
    let sig_json: Value = serde_json::from_str(sig_text).unwrap();
    assert_eq!(
        sig_json,
        json!({"id": "rs_1", "encrypted_content": "enc-blob"})
    );
    // signature_delta comes right before the thinking block's stop.
    let sig_pos = d.iter().position(|x| x == sig).unwrap();
    assert_eq!(
        d[sig_pos + 1],
        json!({"type": "content_block_stop", "index": 0})
    );

    let msg = s.into_message().unwrap();
    assert_eq!(
        msg["content"][0],
        json!({"type": "thinking", "thinking": "planning the read", "signature": sig_text})
    );
    assert_eq!(msg["content"][1]["type"], "tool_use");
    assert_eq!(msg["content"][1]["id"], "c1");
}

#[test]
fn reasoning_item_without_encrypted_content_still_closes_cleanly_with_no_signature() {
    let mut s = CodexToAnthropic::new("gpt", None);
    let frames = feed(
        &mut s,
        &[
            created("r1"),
            ev(
                json!({"type": "response.output_item.added", "item": {"type": "reasoning", "id": "rs_1"}}),
            ),
            ev(
                json!({"type": "response.output_item.done", "item": {"type": "reasoning", "id": "rs_1"}}),
            ),
            added_message(),
            text_delta("Hi"),
            done("message"),
            completed(json!({"input_tokens": 1, "output_tokens": 1})),
        ],
    );
    assert!(!datas(&frames)
        .iter()
        .any(|d| d["delta"]["type"] == "signature_delta"));
    let msg = s.into_message().unwrap();
    let content = msg["content"].as_array().unwrap();
    assert_eq!(content.len(), 2);
    assert_eq!(content[0], json!({"type": "thinking", "thinking": ""}));
    assert_eq!(content[1], json!({"type": "text", "text": "Hi"}));
}

#[test]
fn incomplete_response_due_to_max_output_tokens_maps_to_stop_reason_max_tokens() {
    let mut s = CodexToAnthropic::new("gpt", None);
    let mut frames = feed(
        &mut s,
        &[
            created("r1"),
            added_message(),
            text_delta("truncat"),
            capped(
                "response.completed",
                json!({"input_tokens": 5, "output_tokens": 100}),
            ),
        ],
    );
    assert_eq!(s.stop_reason(), Some("max_tokens"));
    frames.extend(s.finish().unwrap());
    let d = datas(&frames);
    // The still-open text block is closed before the message_delta.
    assert_eq!(
        types(&frames)[3..],
        ["content_block_stop", "message_delta", "message_stop"]
    );
    let delta = d.iter().find(|d| d["type"] == "message_delta").unwrap();
    assert_eq!(delta["delta"]["stop_reason"], "max_tokens");
}

#[test]
fn non_empty_malformed_tool_args_surface_as_an_error_not_a_silent_empty_tool_call() {
    let mut s = CodexToAnthropic::new("gpt", None);
    feed(
        &mut s,
        &[
            created("r1"),
            added_call("c1", "write_file"),
            args_delta(r#"{"path":"a","content":"trunca"#),
            done("function_call"),
        ],
    );
    let err = s.finish().unwrap_err();
    assert_eq!(err, StreamError::MalformedToolArgs);
    assert_eq!(parse(&err.to_frame()).0, "error");
    // The error names no tool and carries no argument text.
    assert!(!err.message().contains("write_file"));
    assert!(!err.message().contains("trunca"));
}

#[test]
fn malformed_tool_args_error_wins_over_a_clean_completed_terminal() {
    let mut s = CodexToAnthropic::new("gpt", None);
    feed(
        &mut s,
        &[
            created("r1"),
            added_call("c1", "write_file"),
            args_delta(r#"{"path":"#),
            done("function_call"),
            completed(json!({"input_tokens": 1, "output_tokens": 1})),
        ],
    );
    assert_eq!(
        s.context_tokens(),
        None,
        "a failing stream carries no context"
    );
    assert_eq!(s.finish().unwrap_err(), StreamError::MalformedToolArgs);
}

#[test]
fn empty_tool_args_still_fold_to_empty_object_without_error() {
    let mut s = CodexToAnthropic::new("gpt", None);
    feed(
        &mut s,
        &[
            created("r1"),
            added_call("c1", "list"),
            done("function_call"),
            completed(json!({"input_tokens": 1, "output_tokens": 1})),
        ],
    );
    let msg = s.into_message().unwrap();
    assert_eq!(msg["content"][0]["type"], "tool_use");
    assert_eq!(msg["content"][0]["input"], json!({}));
}

#[test]
fn cached_input_tokens_surface_as_cache_read_input_tokens() {
    let mut s = CodexToAnthropic::new("gpt", None);
    let mut events = ok_events();
    events.pop();
    events.push(completed(
        json!({"input_tokens": 100, "output_tokens": 5, "input_tokens_details": {"cached_tokens": 80}}),
    ));
    let mut frames = feed(&mut s, &events);
    let client = json!({"input_tokens": 20, "output_tokens": 5, "cache_read_input_tokens": 80});
    assert_eq!(s.context_tokens(), Some(105));
    let msg_view = s.clone().into_message().unwrap();
    frames.extend(s.finish().unwrap());
    let d = datas(&frames);
    let delta = d.iter().find(|d| d["type"] == "message_delta").unwrap();
    assert_eq!(delta["usage"], client);
    assert_eq!(msg_view["usage"], client);
}

#[test]
fn cached_input_tokens_are_clamped_inside_total_input() {
    for (cached, expected) in [(json!(-10), 0u64), (json!(150), 100), (json!(12.7), 12)] {
        let mut s = CodexToAnthropic::new("gpt", None);
        s.on_event(&completed(json!({
            "input_tokens": 100,
            "output_tokens": 5,
            "input_tokens_details": {"cached_tokens": cached}
        })));
        assert_eq!(
            s.usage(),
            TokenUsage {
                input_tokens: 100,
                output_tokens: 5,
                cache_read: expected,
                cache_write: 0
            }
        );
        let msg = s.to_message();
        assert_eq!(
            msg["usage"],
            json!({"input_tokens": 100 - expected, "output_tokens": 5, "cache_read_input_tokens": expected})
        );
    }
}

#[test]
fn no_events_are_translated_after_an_upstream_error() {
    let mut s = CodexToAnthropic::new("gpt", None);
    s.on_event(&created("r1"));
    let frames = s.on_event(&ev(
        json!({"type": "error", "error": {"code": "boom", "message": "backend broke"}}),
    ));
    assert!(
        frames.is_empty(),
        "the caller emits StreamError::to_frame once"
    );
    assert!(s.is_terminal());
    assert!(s.on_event(&added_message()).is_empty());
    assert!(s.on_event(&text_delta("late")).is_empty());
    let err = s.finish().unwrap_err();
    assert_eq!(
        err,
        StreamError::Upstream {
            type_: "boom".into(),
            message: "backend broke".into()
        }
    );
    assert_eq!(
        parse(&err.to_frame()).1,
        json!({"type": "error", "error": {"type": "boom", "message": "backend broke"}})
    );
}

#[test]
fn stream_ending_mid_args_finish_emits_the_error_and_no_clean_close() {
    let mut s = CodexToAnthropic::new("gpt", None);
    feed(
        &mut s,
        &[
            created("r1"),
            added_call("c1", "write_file"),
            args_delta(r#"{"path":"a","content":"trun"#),
        ],
    );
    // Malformed args (closed by finish) outrank the missing terminal, like bro.
    assert_eq!(s.finish().unwrap_err(), StreamError::MalformedToolArgs);
}

#[test]
fn args_truncated_by_max_output_tokens_fold_to_empty_with_stop_reason_max_tokens_not_an_error() {
    let mut s = CodexToAnthropic::new("gpt", None);
    feed(
        &mut s,
        &[
            created("r1"),
            added_call("c1", "write_file"),
            args_delta(r#"{"path":"a","content":"trun"#),
            done("function_call"),
            capped(
                "response.completed",
                json!({"input_tokens": 5, "output_tokens": 100}),
            ),
        ],
    );
    assert_eq!(s.stop_reason(), Some("max_tokens"));
    let frames = s.finish().unwrap();
    assert_eq!(types(&frames), ["message_delta", "message_stop"]);
}

#[test]
fn args_still_open_at_a_token_cap_terminal_are_not_an_error() {
    // Hardening over bro: the block was never closed by output_item.done before the cap.
    let mut s = CodexToAnthropic::new("gpt", None);
    feed(
        &mut s,
        &[
            created("r1"),
            added_call("c1", "write_file"),
            args_delta(r#"{"path":"#),
            capped(
                "response.incomplete",
                json!({"input_tokens": 5, "output_tokens": 100}),
            ),
        ],
    );
    let frames = s.finish().unwrap();
    assert_eq!(
        types(&frames),
        ["content_block_stop", "message_delta", "message_stop"]
    );
}

#[test]
fn response_incomplete_terminal_event_maps_like_completed_with_incomplete_status() {
    let mut s = CodexToAnthropic::new("gpt", None);
    feed(
        &mut s,
        &[
            created("r1"),
            added_message(),
            text_delta("partial"),
            capped(
                "response.incomplete",
                json!({"input_tokens": 5, "output_tokens": 100}),
            ),
        ],
    );
    assert_eq!(s.stop_reason(), Some("max_tokens"));
    assert!(s.is_terminal());
    assert_eq!(
        s.context_tokens(),
        Some(105),
        "incomplete usage is terminal usage"
    );
    assert_eq!(
        s.usage(),
        TokenUsage {
            input_tokens: 5,
            output_tokens: 100,
            cache_read: 0,
            cache_write: 0
        }
    );
    s.finish().unwrap();
}

#[test]
fn content_filter_incompleteness_maps_to_stop_reason_refusal_not_a_clean_finish() {
    let mut s = CodexToAnthropic::new("gpt", None);
    feed(
        &mut s,
        &[
            created("r1"),
            added_message(),
            done("message"),
            ev(json!({
                "type": "response.completed",
                "response": {"status": "incomplete", "incomplete_details": {"reason": "content_filter"}, "usage": {}}
            })),
        ],
    );
    assert_eq!(s.stop_reason(), Some("refusal"));
    let frames = s.finish().unwrap();
    assert_eq!(parse(&frames[0]).1["delta"]["stop_reason"], "refusal");
}

#[test]
fn mid_stream_error_is_terminal_no_message_delta_or_message_stop_follow_it() {
    let mut s = CodexToAnthropic::new("gpt", None);
    let frames = feed(
        &mut s,
        &[
            created("r1"),
            added_message(),
            text_delta("Hel"),
            ev(json!({
                "type": "response.failed",
                "response": {"error": {"code": "boom", "message": "backend broke"}}
            })),
        ],
    );
    assert!(!types(&frames).iter().any(|t| t == "error"));
    assert_eq!(s.context_tokens(), None);
    let err = s.finish().unwrap_err();
    assert_eq!(err.error_type(), "boom");
}

// ---- additional stream-side rules (INV §4) ------------------------------------

#[test]
fn upstream_error_without_code_falls_back_to_type_then_api_error() {
    // Codex `error` events may carry code/message at the top level.
    let mut s = CodexToAnthropic::new("gpt", None);
    s.on_event(&ev(
        json!({"type": "error", "code": "rate_limit_exceeded", "message": "slow down"}),
    ));
    assert_eq!(s.finish().unwrap_err().error_type(), "rate_limit_exceeded");

    let mut s = CodexToAnthropic::new("gpt", None);
    s.on_event(&ev(
        json!({"type": "response.failed", "response": {"error": {}}}),
    ));
    let err = s.finish().unwrap_err();
    assert_eq!(err.error_type(), "api_error");
    assert_eq!(err.message(), "Codex backend error");
}

#[test]
fn upstream_error_message_is_capped_and_kept_out_of_display() {
    let long = "x".repeat(1000);
    let mut s = CodexToAnthropic::new("gpt", None);
    s.on_event(&ev(
        json!({"type": "error", "error": {"code": "boom", "message": long}}),
    ));
    let err = s.finish().unwrap_err();
    assert_eq!(err.message().chars().count(), 300);
    let shown = err.to_string();
    assert!(shown.contains("boom"));
    assert!(
        !shown.contains("xxx"),
        "Display never carries upstream text: {shown}"
    );
}

#[test]
fn eof_after_terminal_without_message_start_is_an_error() {
    // bro openai-codex.test.ts: "a terminal event without response.created fails over".
    let mut s = CodexToAnthropic::new("gpt", None);
    s.on_event(&completed(json!({"input_tokens": 1, "output_tokens": 1})));
    assert!(s.is_terminal());
    assert!(!s.is_started());
    assert_eq!(s.finish().unwrap_err(), StreamError::NoMessageStart);

    let mut s = CodexToAnthropic::new("gpt", None);
    s.on_event(&completed(json!({"input_tokens": 1, "output_tokens": 1})));
    assert_eq!(s.into_message().unwrap_err(), StreamError::NoMessageStart);
}

#[test]
fn dropped_reasoning_events_become_pings_after_message_start() {
    let mut s = CodexToAnthropic::new("gpt", None);
    // Before message_start nothing is emitted, not even a ping.
    assert!(s
        .on_event(&ev(json!({"type": "response.in_progress"})))
        .is_empty());
    s.on_event(&created("r1"));
    for i in 0..3 {
        // Reasoning summary without an open thinking block translates to nothing -> ping.
        let frames = s.on_event(&ev(
            json!({"type": "response.reasoning_summary_text.delta", "delta": format!("thinking {i}")}),
        ));
        assert_eq!(frames.len(), 1);
        assert_eq!(
            parse(&frames[0]),
            ("ping".to_string(), json!({"type": "ping"}))
        );
    }
    // Unparseable data and unknown event types are dropped the same way.
    let junk = SseEvent {
        event: Some("x".into()),
        data: "not json".into(),
        raw: Bytes::new(),
    };
    assert_eq!(types(&s.on_event(&junk)), ["ping"]);
    // The terminal event itself is not a ping, and nothing follows it.
    assert!(s
        .on_event(&completed(json!({"input_tokens": 3, "output_tokens": 1})))
        .is_empty());
    assert!(s.on_event(&junk).is_empty());
    assert_eq!(
        types(&s.finish().unwrap()),
        ["message_delta", "message_stop"]
    );
}

#[test]
fn event_name_is_used_when_data_has_no_type() {
    let mut s = CodexToAnthropic::new("gpt", None);
    let frames = s.on_event(&SseEvent {
        event: Some("response.created".into()),
        data: json!({"response": {"id": "r9"}}).to_string(),
        raw: Bytes::new(),
    });
    assert_eq!(parse(&frames[0]).1["message"]["id"], "r9");
}

#[test]
fn cached_terminal_input_remains_split_for_clients_but_records_the_full_codex_input() {
    let events = [
        created("cached"),
        completed(
            json!({"input_tokens": 100, "output_tokens": 5, "input_tokens_details": {"cached_tokens": 80}}),
        ),
    ];
    let mut s = CodexToAnthropic::new("gpt", None);
    feed(&mut s, &events);
    assert_eq!(
        s.usage(),
        TokenUsage {
            input_tokens: 100,
            output_tokens: 5,
            cache_read: 80,
            cache_write: 0
        }
    );
    let frames = s.finish().unwrap();
    assert_eq!(
        parse(&frames[0]).1["usage"],
        json!({"input_tokens": 20, "output_tokens": 5, "cache_read_input_tokens": 80})
    );
}

#[test]
fn signature_round_trips_into_the_next_codex_request() {
    let mut s = CodexToAnthropic::new("gpt", None);
    feed(
        &mut s,
        &[
            created("r1"),
            ev(
                json!({"type": "response.output_item.added", "item": {"type": "reasoning", "id": "rs_7"}}),
            ),
            ev(json!({"type": "response.reasoning_summary_text.delta", "delta": "plan"})),
            ev(json!({
                "type": "response.output_item.done",
                "item": {"type": "reasoning", "id": "rs_7", "encrypted_content": "gAAA\"blob"}
            })),
            added_message(),
            text_delta("Done"),
            done("message"),
            completed(json!({"input_tokens": 1, "output_tokens": 1})),
        ],
    );
    let msg = s.into_message().unwrap();
    let body = json!({
        "model": "gpt",
        "messages": [
            {"role": "user", "content": "hi"},
            {"role": "assistant", "content": msg["content"]},
            {"role": "user", "content": "again"}
        ]
    });
    let route = RouteDef {
        id: "gpt".into(),
        provider: ProviderId::OpenAi,
        upstream_model: "gpt-5.5".into(),
        context_window: None,
        max_context_window: None,
        supported_efforts: vec![],
        effort_map: None,
    };
    let out = to_codex_request(&body, &route, None).unwrap();
    let input = out["input"].as_array().unwrap();
    let reasoning = input
        .iter()
        .find(|i| i["type"] == "reasoning")
        .expect("reasoning item reconstructed");
    assert_eq!(
        *reasoning,
        json!({
            "type": "reasoning",
            "id": "rs_7",
            "summary": [{"type": "summary_text", "text": "plan"}],
            "encrypted_content": "gAAA\"blob"
        })
    );
}

// ---- openai-codex.test.ts: context carry --------------------------------------

/// One streaming turn through the carry, the way Task 38 drives it: seed, translate, and
/// record only on a clean finish.
fn turn(carry: &ContextCarry, user: Option<&str>, route: &str, events: &[SseEvent]) -> Value {
    let mut s = CodexToAnthropic::new(route, carry.seed(user, route));
    let frames = feed(&mut s, events);
    let ctx = s.context_tokens();
    if s.finish().is_ok() {
        if let Some(tokens) = ctx {
            carry.record(user, route, tokens);
        }
    }
    opening_input(&frames)
}

#[test]
fn streaming_carries_and_replaces_terminal_context_for_the_same_session_and_route() {
    let carry = ContextCarry::new();
    let mut second = ok_events();
    *second.last_mut().unwrap() = completed(json!({"input_tokens": 7, "output_tokens": 2}));
    let a = Some("session-a");
    assert_eq!(turn(&carry, a, "gpt", &ok_events())["input_tokens"], 0);
    assert_eq!(
        turn(&carry, a, "gpt", &second),
        json!({"input_tokens": 4, "output_tokens": 0})
    );
    assert_eq!(
        turn(&carry, a, "gpt", &ok_events()),
        json!({"input_tokens": 9, "output_tokens": 0})
    );
}

#[test]
fn non_stream_terminal_usage_seeds_the_next_streaming_opening() {
    let carry = ContextCarry::new();
    let a = Some("session-a");
    let mut s = CodexToAnthropic::new("gpt", carry.seed(a, "gpt"));
    feed(&mut s, &ok_events());
    let ctx = s.context_tokens().unwrap();
    let msg = s.into_message().unwrap();
    assert_eq!(msg["content"], json!([{"type": "text", "text": "Hi"}]));
    carry.record(a, "gpt", ctx);
    assert_eq!(
        turn(&carry, a, "gpt", &ok_events()),
        json!({"input_tokens": 4, "output_tokens": 0})
    );
}

#[test]
fn carried_context_is_isolated_by_session_and_route() {
    let carry = ContextCarry::new();
    let open = |user: &str, route: &str| {
        turn(&carry, Some(user), route, &ok_events())["input_tokens"].clone()
    };
    assert_eq!(open("session-a", "gpt"), 0);
    assert_eq!(open("session-b", "gpt"), 0);
    assert_eq!(open("session-a", "gpt-other"), 0);
    assert_eq!(open("session-a", "gpt"), 4);
    // No session (affinity off / no metadata.user_id) never carries.
    assert_eq!(turn(&carry, None, "gpt", &ok_events())["input_tokens"], 0);
    assert_eq!(turn(&carry, None, "gpt", &ok_events())["input_tokens"], 0);
    assert_eq!(
        turn(&carry, Some(""), "gpt", &ok_events())["input_tokens"],
        0
    );
    assert_eq!(
        turn(&carry, Some(""), "gpt", &ok_events())["input_tokens"],
        0
    );
}

#[test]
fn a_failed_stream_does_not_replace_carried_context() {
    let carry = ContextCarry::new();
    let a = Some("session-a");
    let failed = [
        created("failed"),
        ev(
            json!({"type": "response.failed", "response": {"error": {"code": "boom", "message": "backend broke"}}}),
        ),
    ];
    turn(&carry, a, "gpt", &ok_events());
    turn(&carry, a, "gpt", &failed);
    assert_eq!(turn(&carry, a, "gpt", &ok_events())["input_tokens"], 4);
}

#[test]
fn a_cancelled_stream_does_not_replace_carried_context() {
    let carry = ContextCarry::new();
    let a = Some("session-a");
    turn(&carry, a, "gpt", &ok_events());
    // Cancelled after message_start: no terminal event, so no context.
    let mut s = CodexToAnthropic::new("gpt", carry.seed(a, "gpt"));
    s.on_event(&created("cancelled"));
    assert_eq!(s.context_tokens(), None);
    drop(s);
    assert_eq!(turn(&carry, a, "gpt", &ok_events())["input_tokens"], 4);
}

#[test]
fn missing_terminal_input_keeps_prior_context_without_recounting_its_carried_baseline() {
    let carry = ContextCarry::new();
    let a = Some("session-a");
    turn(&carry, a, "gpt", &ok_events());
    let missing = [
        created("missing-input"),
        completed(json!({"output_tokens": 5})),
    ];
    let mut s = CodexToAnthropic::new("gpt", carry.seed(a, "gpt"));
    feed(&mut s, &missing);
    assert_eq!(s.usage().input_tokens, 0);
    assert_eq!(s.usage().output_tokens, 5);
    assert_eq!(s.context_tokens(), None);
    s.finish().unwrap();
    assert_eq!(
        turn(&carry, a, "gpt", &ok_events()),
        json!({"input_tokens": 4, "output_tokens": 0})
    );
}

#[test]
fn context_carry_is_a_cheap_shared_handle() {
    let carry = ContextCarry::new();
    let clone = carry.clone();
    clone.record(Some("u"), "gpt", 11);
    assert_eq!(carry.seed(Some("u"), "gpt"), Some(11));
    let t = std::thread::spawn(move || clone.record(Some("u"), "gpt", 12));
    t.join().unwrap();
    assert_eq!(carry.seed(Some("u"), "gpt"), Some(12));
    assert_eq!(ContextCarry::CAPACITY, 1000);
}

#[test]
fn context_carry_evicts_the_least_recently_used_entry_at_capacity() {
    let carry = ContextCarry::with_capacity(2);
    carry.record(Some("a"), "r", 1);
    carry.record(Some("b"), "r", 2);
    // Reading `a` makes `b` the oldest.
    assert_eq!(carry.seed(Some("a"), "r"), Some(1));
    carry.record(Some("c"), "r", 3);
    assert_eq!(carry.len(), 2);
    assert_eq!(carry.seed(Some("b"), "r"), None);
    assert_eq!(carry.seed(Some("a"), "r"), Some(1));
    assert_eq!(carry.seed(Some("c"), "r"), Some(3));
    // Re-recording an existing key replaces it without growing.
    carry.record(Some("a"), "r", 10);
    assert_eq!(carry.len(), 2);
    assert_eq!(carry.seed(Some("a"), "r"), Some(10));
}
