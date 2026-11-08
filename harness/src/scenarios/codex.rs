//! Codex (ChatGPT backend) translation and stream behavior.

use std::time::Duration;

use serde_json::{json, Value};

use super::{claude, codex, Scenario, Step};
use crate::fake::*;

const GPT: &str = "gpt-5.5";

fn codex_body(model: &str, stream: bool) -> Value {
    json!({
        "model": model,
        "max_tokens": 2048,
        "stream": stream,
        "messages": [{"role": "user", "content": "hello"}],
    })
}

/// The second turn of a reasoning round-trip: the assistant turn carries the thinking block
/// (with the signature the first turn produced) back to the proxy.
fn roundtrip_body() -> Value {
    let signature = json!({"id": "rs_1", "encrypted_content": "enc-opaque-1"}).to_string();
    json!({
        "model": GPT,
        "max_tokens": 2048,
        "stream": true,
        "thinking": {"type": "enabled", "budget_tokens": 1024},
        "messages": [
            {"role": "user", "content": "hello"},
            {"role": "assistant", "content": [
                {"type": "thinking", "thinking": "plan", "signature": signature},
                {"type": "text", "text": "Hi there"}
            ]},
            {"role": "user", "content": "again"}
        ],
    })
}

/// Shape of a Codex request: model, effort, include, and the input item kinds in order.
fn responses_shape(rec: &Recorded) -> Option<String> {
    if rec.path != RESPONSES {
        return None;
    }
    let v: Value = serde_json::from_slice(&rec.body).ok()?;
    let input: Vec<String> = v["input"]
        .as_array()
        .map(|items| {
            items
                .iter()
                .map(|i| {
                    let ty = i["type"].as_str().unwrap_or("message");
                    match (ty, i["role"].as_str()) {
                        ("reasoning", _) => format!(
                            "reasoning(id={},enc={})",
                            i["id"].as_str().unwrap_or("-"),
                            i["encrypted_content"].as_str().unwrap_or("-")
                        ),
                        ("function_call", _) => format!(
                            "function_call(call_id={},name={},args={})",
                            i["call_id"].as_str().unwrap_or("-"),
                            i["name"].as_str().unwrap_or("-"),
                            i["arguments"].as_str().unwrap_or("-")
                        ),
                        ("function_call_output", _) => format!(
                            "function_call_output(call_id={},output={})",
                            i["call_id"].as_str().unwrap_or("-"),
                            i["output"]
                        ),
                        (t, Some(r)) => format!("{t}:{r}"),
                        (t, None) => t.to_string(),
                    }
                })
                .collect()
        })
        .unwrap_or_default();
    let tools: Vec<String> = v["tools"]
        .as_array()
        .map(|t| {
            t.iter()
                .map(|t| {
                    format!(
                        "{}:{}",
                        t["type"].as_str().unwrap_or("-"),
                        t["name"].as_str().unwrap_or("-")
                    )
                })
                .collect()
        })
        .unwrap_or_default();
    Some(format!(
        "model={} stream={} include={} tools=[{}] tool_choice={} input=[{}]",
        v["model"].as_str().unwrap_or("-"),
        v["stream"],
        v["include"],
        tools.join(","),
        v["tool_choice"],
        input.join(",")
    ))
}

fn tools() -> Value {
    json!([{
        "name": "ls",
        "description": "list a directory",
        "input_schema": {"type": "object", "properties": {"path": {"type": "string"}}}
    }])
}

fn tool_call_body() -> Value {
    json!({
        "model": GPT,
        "max_tokens": 2048,
        "stream": true,
        "tools": tools(),
        "messages": [{"role": "user", "content": "list it"}],
    })
}

/// The turn after a tool call: the tool_use goes back with its tool_result.
fn tool_result_body() -> Value {
    json!({
        "model": GPT,
        "max_tokens": 2048,
        "stream": true,
        "tools": tools(),
        "messages": [
            {"role": "user", "content": "list it"},
            {"role": "assistant", "content": [
                {"type": "tool_use", "id": "call_1", "name": "ls", "input": {"path": "."}}
            ]},
            {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "call_1", "content": "a.txt"}
            ]}
        ],
    })
}

fn codex_tool_call_frames() -> Vec<String> {
    vec![
        codex_created(None),
        codex_event(json!({"type": "response.output_item.added",
                           "item": {"type": "function_call", "id": "fc_1", "call_id": "call_1",
                                    "name": "ls", "arguments": ""}})),
        codex_event(
            json!({"type": "response.function_call_arguments.delta", "delta": "{\"path\":"}),
        ),
        codex_event(json!({"type": "response.function_call_arguments.delta", "delta": "\".\"}"})),
        codex_event(json!({"type": "response.output_item.done",
                           "item": {"type": "function_call", "id": "fc_1", "call_id": "call_1",
                                    "name": "ls", "arguments": "{\"path\":\".\"}"}})),
        codex_completed(),
    ]
}

pub fn scenarios() -> Vec<Scenario> {
    let one = || vec![codex("cx", 1)];
    vec![
        Scenario::new(
            "codex_text",
            "plain Codex text turn translated to an Anthropic stream",
            |up| up.openai.script(RESPONSES, vec![codex_ok()]),
        )
        .accounts(one())
        .steps(vec![
            Step::messages(codex_body(GPT, true)),
            Step::messages(codex_body(GPT, false)),
        ])
        .detail(responses_shape),
        Scenario::new(
            "codex_reasoning_roundtrip",
            "reasoning item becomes a thinking block whose signature stashes the encrypted content; the next turn sends it back as a reasoning input item",
            |up| {
                let mut fs = vec![codex_created(None)];
                fs.extend(codex_reasoning_item());
                fs.extend(codex_text_item());
                fs.push(codex_completed());
                up.openai.script(
                    RESPONSES,
                    vec![
                        codex_allowed(Reply::sse(frames_now(fs))),
                        codex_ok(),
                    ],
                );
            },
        )
        .accounts(one())
        .steps(vec![
            Step::messages(codex_body(GPT, true)),
            Step::messages(roundtrip_body()),
        ])
        .detail(responses_shape),
        Scenario::new(
            "codex_tool_call",
            "function_call becomes a tool_use block (stop_reason tool_use); the tool_result goes back as function_call_output",
            |up| {
                up.openai.script(
                    RESPONSES,
                    vec![
                        codex_allowed(Reply::sse(frames_now(codex_tool_call_frames()))),
                        codex_allowed(Reply::sse(frames_now(codex_tool_call_frames()))),
                        codex_ok(),
                    ],
                )
            },
        )
        .accounts(one())
        .steps(vec![
            Step::messages(tool_call_body()),
            Step::messages({
                let mut b = tool_call_body();
                b["stream"] = json!(false);
                b
            }),
            Step::messages(tool_result_body()),
        ])
        .detail(responses_shape),
        Scenario::new(
            "codex_unauthorized",
            "Codex 401 forces a token refresh and one retry on the same account",
            |up| {
                up.openai.route(
                    RESPONSES,
                    by_account(|_, i, _| {
                        if i == 0 {
                            Reply::json(
                                401,
                                &json!({"error": {"type": "invalid_request_error",
                                                  "code": "token_expired",
                                                  "message": "harness: token expired"}}),
                            )
                        } else {
                            codex_ok()
                        }
                    }),
                )
            },
        )
        .accounts(vec![codex("cx", 1), codex("cy", 2)])
        .steps(vec![Step::messages(codex_body(GPT, true))]),
        Scenario::new(
            "codex_oversized_created",
            "response.created echoing ~200 KiB of instructions still yields message_start and the text",
            |up| {
                // The created event arrives in 16 KiB pieces (as over a real network), so both
                // proxies cross their 64 KiB pre-commit cap before the event is complete.
                let big = "x".repeat(200 * 1024);
                let created = codex_created(Some(&big));
                let mut fs: Vec<(Duration, String)> = created
                    .as_bytes()
                    .chunks(16 * 1024)
                    .map(|c| {
                        (
                            Duration::from_millis(5),
                            String::from_utf8(c.to_vec()).expect("ascii"),
                        )
                    })
                    .collect();
                let mut rest = codex_text_item();
                rest.push(codex_completed());
                fs.extend(frames_now(rest));
                up.openai
                    .script(RESPONSES, vec![codex_allowed(Reply::sse(fs))]);
            },
        )
        .accounts(one())
        .steps(vec![Step::messages(codex_body(GPT, true))]),
        Scenario::new(
            "codex_oversized_created_single_read",
            "the same ~200 KiB response.created written in one piece (bro can parse it in one read and never needs the forced message_start)",
            |up| {
                let big = "x".repeat(200 * 1024);
                let mut fs = vec![codex_created(Some(&big))];
                fs.extend(codex_text_item());
                fs.push(codex_completed());
                up.openai
                    .script(RESPONSES, vec![codex_allowed(Reply::sse(frames_now(fs)))]);
            },
        )
        .accounts(one())
        .steps(vec![Step::messages(codex_body(GPT, true))]),
        Scenario::new(
            "codex_silent_keepalive",
            "Codex is silent for 2.5 s after the headers: the client stream stays alive (pings) and then completes",
            |up| {
                let mut fs: Vec<(Duration, String)> = frames_now(codex_turn());
                fs[0].0 = Duration::from_millis(2500);
                up.openai
                    .script(RESPONSES, vec![codex_allowed(Reply::sse(fs))]);
            },
        )
        .accounts(one())
        .steps(vec![Step::messages(codex_body(GPT, true))]),
        Scenario::new(
            "codex_failed_after_created",
            "response.created then response.failed before any output item (Task 38 intended change)",
            |up| {
                up.openai.script(
                    RESPONSES,
                    vec![codex_allowed(Reply::sse(frames_now(vec![
                        codex_created(None),
                        codex_failed("server_error"),
                    ])))],
                )
            },
        )
        .accounts(one())
        .steps(vec![Step::messages(codex_body(GPT, true))]),
        Scenario::new(
            "codex_midstream_cut",
            "Codex connection resets after the first text delta",
            |up| {
                let mut fs = vec![codex_created(None)];
                fs.extend(codex_text_item().into_iter().take(2));
                up.openai.script(
                    RESPONSES,
                    vec![cut_after_frames(codex_allowed(Reply::sse(frames_now(fs))))],
                );
            },
        )
        .accounts(one())
        .steps(vec![
            Step::messages(codex_body(GPT, true)).wait(Duration::from_millis(300))
        ]),
        Scenario::new(
            "codex_429",
            "Codex 429 with exhausted primary window fails over to the second Codex account",
            |up| {
                up.openai.route(
                    RESPONSES,
                    by_account(|acct, _, _| match acct {
                        "cx" => Reply::json(
                            429,
                            &json!({"error": {"type": "usage_limit_reached",
                                              "message": "harness: usage limit",
                                              "resets_in_seconds": 3600}}),
                        )
                        .header("x-codex-primary-used-percent", "100")
                        .header("x-codex-primary-window-minutes", "300")
                        .header(
                            "x-codex-primary-reset-at",
                            &unix_in(Duration::from_secs(3600)).to_string(),
                        ),
                        _ => codex_ok(),
                    }),
                )
            },
        )
        .accounts(vec![codex("cx", 1), codex("cy", 2)])
        .steps(vec![Step::messages(codex_body(GPT, true))]),
        Scenario::new(
            "codex_server_error",
            "Codex answers 500 on every attempt (second Codex account available)",
            |up| {
                up.openai.route(
                    RESPONSES,
                    by_account(|acct, _, _| match acct {
                        "cx" => Reply::json(
                            500,
                            &json!({"error": {"type": "server_error", "message": "harness: boom"}}),
                        ),
                        _ => codex_ok(),
                    }),
                )
            },
        )
        .accounts(vec![codex("cx", 1), codex("cy", 2)])
        .steps(vec![Step::messages(codex_body(GPT, true))]),
        Scenario::new(
            "codex_invalid_grant",
            "expired Codex token whose refresh is rejected with invalid_grant",
            |up| {
                up.openai.route(
                    OAUTH_TOKEN,
                    by_refresh_account(|_, _| {
                        Reply::json(
                            400,
                            &json!({"error": {"code": "invalid_grant",
                                              "message": "harness: refresh token revoked"}}),
                        )
                    }),
                );
                up.openai.script(RESPONSES, vec![codex_ok()]);
            },
        )
        .accounts(vec![codex("cx", 1).expired(), codex("cy", 2)])
        .steps(vec![Step::messages(codex_body(GPT, true))]),
        Scenario::new(
            "one_m_resolution",
            "a trailing [1m] on a Codex model id resolves to the base route (upstream model without the marker)",
            |up| {
                up.openai.script(RESPONSES, vec![codex_ok()]);
                up.anthropic
                    .route(MESSAGES, by_account(|_, _, _| anthropic_ok("claude")));
            },
        )
        .accounts(vec![codex("cx", 1), claude("alpha", 1)])
        .steps(vec![
            Step::messages(codex_body("gpt-6-sol[1m]", true)),
            Step::messages(codex_body("gpt-5.4[1m]", true)),
            Step::messages(codex_body("claude-opus-5[1m]", true)),
        ])
        .detail(|rec| match rec.path.as_str() {
            RESPONSES => responses_shape(rec),
            MESSAGES => serde_json::from_slice::<Value>(&rec.body)
                .ok()
                .map(|v| format!("model={}", v["model"].as_str().unwrap_or("-"))),
            _ => None,
        }),
    ]
}
