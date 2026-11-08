//! Codex Responses SSE -> Anthropic Messages SSE (INV §4 "Stream translation"). Pure and
//! synchronous: no IO, no timers, no logging.
//!
//! Ported from bro `codex-translate.ts` (`CodexToAnthropicStream`); the proxy side (Task 38)
//! owns the transport, the 64 KiB pre-commit buffer, the timer-driven keep-alive and teardown.
//!
//! # Event mapping
//! The event type is `data.type`, else the SSE `event:` name; data that is not JSON is dropped.
//! - `response.created` -> `message_start` (id = `response.id`, else a generated `msg_…`;
//!   `usage = {input_tokens: seed, output_tokens: 0}`). Ignored once started.
//! - `response.output_item.added` -> `content_block_start` for `message` (text),
//!   `function_call` (tool_use: id = `call_id` / `id` / `tu_<index>`, `input: {}`) and
//!   `reasoning` (thinking). Other item types are dropped. Opening a block while one is still
//!   open closes the old one first (bro left it dangling).
//! - `response.output_text.delta` -> `text_delta`; `response.function_call_arguments.delta` ->
//!   `input_json_delta`; `response.reasoning_summary_text.delta` -> `thinking_delta`. Each only
//!   applies to an open block of the matching kind; otherwise it is dropped.
//! - `response.output_item.done` -> `content_block_stop`. For a thinking block whose done item
//!   carries string `id` and `encrypted_content`, a `signature_delta` precedes the stop; its
//!   signature is [`codex_stash_signature`] (`{"id":…,"encrypted_content":…}`), which
//!   `to_codex_request` turns back into a `reasoning` input item next turn.
//! - `response.completed` / `response.incomplete` -> terminal. `stop_reason`: `refusal` for an
//!   incomplete `content_filter`, `max_tokens` for any other incompleteness (status
//!   `incomplete` or the `response.incomplete` event), else `tool_use` if a function call was
//!   seen, else `end_turn`. `message_delta` + `message_stop` are produced by [`finish`].
//! - `response.failed` / `error` -> terminal upstream error ([`StreamError::Upstream`], type =
//!   `code` / `type` / `api_error`, message capped at 300 chars), reported by [`finish`].
//!
//! # Usage (two views)
//! Client view (`message_delta.usage`, [`to_message`]): `input_tokens = total - cached`,
//! `cache_read_input_tokens = cached` (present only when Codex reported `cached_tokens`; clamped
//! to `0..=total`), `output_tokens` as reported. Until terminal input usage arrives
//! `input_tokens` is the context-carry seed. Account view ([`usage`]): `input_tokens` is the
//! full Codex input (`total`, cached included; 0 when no terminal input usage arrived, so a
//! carried baseline is never recounted), `cache_read = cached`, `cache_write = 0`.
//!
//! # Errors ([`finish`])
//! In priority order: upstream error event; a tool call whose non-empty arguments are not a JSON
//! object ([`StreamError::MalformedToolArgs`], deferred to `finish` and excused when the
//! terminal was an incomplete/token-cap one); EOF without a terminal event; EOF without
//! `message_start`. Error values never carry prompt/response content.
//!
//! # Contract for the proxy (Task 38)
//! - Feed each parsed upstream event to [`on_event`] and forward the returned frames. When it
//!   returns no frames after `message_start` and before the terminal (a dropped reasoning /
//!   unknown event) it returns one `ping` frame instead, so the client sees activity. Nothing
//!   (not even a ping) is returned before `message_start` or after the terminal event.
//! - Frames other than `ping` are content: the first one commits the response.
//! - Pre-commit buffer cap reached: call [`force_message_start`] and send its frame first; the
//!   real `response.created` then becomes a no-op. It returns empty bytes once started.
//! - After every event check [`is_terminal`]; once true, stop reading upstream (tear down; do
//!   not wait for EOF) and call [`finish`]. At EOF call [`finish`] too.
//! - [`finish`] `Ok(frames)`: send them (closing stop for an open block, `message_delta`,
//!   `message_stop`); success. `Err(e)`: if committed send `e.to_frame()` (the only error frame
//!   — `on_event` never emits one); if not committed settle without streaming (never a lone
//!   error frame inside a 200). [`StreamError::Upstream`] -> classify `error_type()`/`message()`
//!   for rate limiting; the other variants are failover / terminal per Task 38.
//! - Read [`usage`] and [`context_tokens`] before `finish` (it consumes `self`; neither changes
//!   in `finish`). Record usage and `ContextCarry::record(context_tokens)` only when `finish`
//!   (or [`into_message`]) is `Ok` and `context_tokens()` is `Some`.
//! - Non-streaming clients: [`into_message`] = `finish` checks + the folded message.
//!
//! [`finish`]: CodexToAnthropic::finish
//! [`on_event`]: CodexToAnthropic::on_event
//! [`force_message_start`]: CodexToAnthropic::force_message_start
//! [`is_terminal`]: CodexToAnthropic::is_terminal
//! [`usage`]: CodexToAnthropic::usage
//! [`context_tokens`]: CodexToAnthropic::context_tokens
//! [`to_message`]: CodexToAnthropic::to_message
//! [`into_message`]: CodexToAnthropic::into_message

use bytes::Bytes;
use serde_json::{json, Map, Value};

use crate::accounts::TokenUsage;
use crate::providers::anthropic::codex_stash_signature;
use crate::sse::{encode, SseEvent};

/// Cap on an upstream error message carried to the client.
const MAX_ERROR_MESSAGE_CHARS: usize = 300;
/// Cap on an upstream error type/code.
const MAX_ERROR_TYPE_CHARS: usize = 64;

/// Why a Codex stream did not end cleanly. `Display` names only the class (and the upstream
/// error code), never upstream text.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum StreamError {
    #[error("codex stream ended before a terminal event")]
    MissingTerminal,
    #[error("codex stream ended without a message_start event")]
    NoMessageStart,
    #[error("codex returned malformed tool-call arguments")]
    MalformedToolArgs,
    /// `response.failed` / `error` event. `message` is upstream text (≤ 300 chars): it may go
    /// to the client in the error frame but must not be logged.
    #[error("codex upstream error ({type_})")]
    Upstream { type_: String, message: String },
}

impl StreamError {
    /// Anthropic error `type` for the client frame.
    pub fn error_type(&self) -> &str {
        match self {
            StreamError::Upstream { type_, .. } => type_,
            _ => "api_error",
        }
    }

    /// Anthropic error `message` for the client frame.
    pub fn message(&self) -> &str {
        match self {
            StreamError::MissingTerminal => "Codex stream ended before a terminal event",
            StreamError::NoMessageStart => "Codex stream ended without a message_start event",
            StreamError::MalformedToolArgs => "Codex returned malformed arguments for a tool call",
            StreamError::Upstream { message, .. } => message,
        }
    }

    /// The Anthropic `error` SSE frame.
    pub fn to_frame(&self) -> Bytes {
        frame(
            "error",
            json!({
                "type": "error",
                "error": { "type": self.error_type(), "message": self.message() },
            }),
        )
    }
}

#[derive(Debug, Clone)]
enum Block {
    Text(String),
    ToolUse {
        id: String,
        name: String,
        args: String,
        input: Value,
    },
    Thinking {
        thinking: String,
        signature: Option<String>,
    },
}

impl Block {
    fn to_json(&self) -> Value {
        match self {
            Block::Text(text) => json!({ "type": "text", "text": text }),
            Block::ToolUse {
                id, name, input, ..
            } => json!({ "type": "tool_use", "id": id, "name": name, "input": input }),
            Block::Thinking {
                thinking,
                signature,
            } => {
                let mut v = json!({ "type": "thinking", "thinking": thinking });
                if let Some(sig) = signature {
                    v["signature"] = json!(sig);
                }
                v
            }
        }
    }
}

/// Translator state for one Codex response. See the module docs.
#[derive(Debug, Clone)]
pub struct CodexToAnthropic {
    model: String,
    msg_id: Option<String>,
    started: bool,
    terminal: bool,
    /// Terminal was `incomplete` (token cap or content filter): malformed args are excused.
    incomplete: bool,
    stop_reason: Option<&'static str>,
    saw_tool_use: bool,
    blocks: Vec<Block>,
    open: bool,
    args_error: bool,
    upstream_error: Option<StreamError>,
    /// Client-view input tokens (seed until terminal input usage arrives).
    input_tokens: u64,
    output_tokens: u64,
    cache_read: Option<u64>,
    terminal_usage: bool,
}

impl CodexToAnthropic {
    /// `model` is echoed in `message_start` (the client-facing route id); `seed_input_tokens`
    /// is the previous turn's context from `ContextCarry::seed`.
    pub fn new(model: &str, seed_input_tokens: Option<u64>) -> Self {
        CodexToAnthropic {
            model: model.to_string(),
            msg_id: None,
            started: false,
            terminal: false,
            incomplete: false,
            stop_reason: None,
            saw_tool_use: false,
            blocks: Vec::new(),
            open: false,
            args_error: false,
            upstream_error: None,
            input_tokens: seed_input_tokens.unwrap_or(0),
            output_tokens: 0,
            cache_read: None,
            terminal_usage: false,
        }
    }

    /// Translate one upstream event into Anthropic frames (see the module docs for the ping rule).
    pub fn on_event(&mut self, ev: &SseEvent) -> Vec<Bytes> {
        if self.terminal {
            return Vec::new();
        }
        let frames = match serde_json::from_str::<Value>(&ev.data) {
            Ok(data) => self.translate(ev, &data),
            Err(_) => Vec::new(),
        };
        if frames.is_empty() && self.started && !self.terminal {
            return vec![ping()];
        }
        frames
    }

    /// Emit `message_start` without waiting for `response.created` (pre-commit buffer cap). Empty
    /// bytes when already started.
    pub fn force_message_start(&mut self) -> Bytes {
        if self.started {
            return Bytes::new();
        }
        self.message_start(None)
    }

    /// End of the response (terminal event seen or upstream EOF). See the module docs.
    pub fn finish(mut self) -> Result<Vec<Bytes>, StreamError> {
        let mut frames = self.close_block(None);
        self.check()?;
        frames.push(frame(
            "message_delta",
            json!({
                "type": "message_delta",
                "delta": { "stop_reason": self.final_stop_reason(), "stop_sequence": null },
                "usage": self.client_usage(),
            }),
        ));
        frames.push(frame("message_stop", json!({ "type": "message_stop" })));
        Ok(frames)
    }

    /// Account-view usage: input includes cached tokens (0 without terminal input usage).
    pub fn usage(&self) -> TokenUsage {
        let cached = self.cache_read.unwrap_or(0);
        TokenUsage {
            input_tokens: if self.terminal_usage {
                self.input_tokens + cached
            } else {
                0
            },
            output_tokens: self.output_tokens,
            cache_read: cached,
            cache_write: 0,
        }
    }

    /// The non-streaming Anthropic message folded from the accumulated blocks (bro
    /// `toAnthropicMessage`). Unchecked: prefer [`CodexToAnthropic::into_message`].
    pub fn to_message(mut self) -> Value {
        self.close_block(None);
        self.message_json()
    }

    /// [`CodexToAnthropic::to_message`] after the same checks as [`CodexToAnthropic::finish`].
    pub fn into_message(mut self) -> Result<Value, StreamError> {
        self.close_block(None);
        self.check()?;
        Ok(self.message_json())
    }

    /// `message_start` has been emitted (real or forced).
    pub fn is_started(&self) -> bool {
        self.started
    }

    /// A terminal event (`completed` / `incomplete` / `failed` / `error`) was seen.
    pub fn is_terminal(&self) -> bool {
        self.terminal
    }

    /// Stop reason decided by a `completed` / `incomplete` terminal, if any.
    pub fn stop_reason(&self) -> Option<&str> {
        self.stop_reason
    }

    /// Context to carry into the next turn: `input + cached + output`, only when terminal input
    /// usage arrived and no error is known (`finish` must still be `Ok` before recording).
    pub fn context_tokens(&self) -> Option<u64> {
        if !self.terminal_usage || self.upstream_error.is_some() || self.args_error {
            return None;
        }
        Some(self.input_tokens + self.cache_read.unwrap_or(0) + self.output_tokens)
    }

    // ---- internals ----------------------------------------------------------------

    fn translate(&mut self, ev: &SseEvent, data: &Value) -> Vec<Bytes> {
        let ty = data
            .get("type")
            .and_then(Value::as_str)
            .or(ev.event.as_deref())
            .unwrap_or("");
        match ty {
            "response.created" => {
                if self.started {
                    return Vec::new();
                }
                let id = data
                    .pointer("/response/id")
                    .and_then(Value::as_str)
                    .map(str::to_string);
                vec![self.message_start(id)]
            }
            "response.output_item.added" => {
                let item = data.get("item").unwrap_or(&Value::Null);
                let block = match item.get("type").and_then(Value::as_str) {
                    Some("message") => Block::Text(String::new()),
                    Some("function_call") => {
                        self.saw_tool_use = true;
                        let id = str_field(item, "call_id")
                            .or_else(|| str_field(item, "id"))
                            .unwrap_or_else(|| format!("tu_{}", self.blocks.len()));
                        Block::ToolUse {
                            id,
                            name: str_field(item, "name").unwrap_or_default(),
                            args: String::new(),
                            input: json!({}),
                        }
                    }
                    Some("reasoning") => Block::Thinking {
                        thinking: String::new(),
                        signature: None,
                    },
                    _ => return Vec::new(),
                };
                self.open_block(block)
            }
            "response.reasoning_summary_text.delta" => {
                let delta = delta_text(data);
                match self.open_mut() {
                    Some(Block::Thinking { thinking, .. }) => {
                        thinking.push_str(&delta);
                        vec![self
                            .block_delta(json!({ "type": "thinking_delta", "thinking": delta }))]
                    }
                    _ => Vec::new(),
                }
            }
            "response.output_text.delta" => {
                let delta = delta_text(data);
                match self.open_mut() {
                    Some(Block::Text(text)) => {
                        text.push_str(&delta);
                        vec![self.block_delta(json!({ "type": "text_delta", "text": delta }))]
                    }
                    _ => Vec::new(),
                }
            }
            "response.function_call_arguments.delta" => {
                let delta = delta_text(data);
                match self.open_mut() {
                    Some(Block::ToolUse { args, .. }) => {
                        args.push_str(&delta);
                        vec![self.block_delta(
                            json!({ "type": "input_json_delta", "partial_json": delta }),
                        )]
                    }
                    _ => Vec::new(),
                }
            }
            "response.output_item.done" => self.close_block(data.get("item")),
            "response.completed" | "response.incomplete" => {
                self.terminal = true;
                let response = data.get("response").unwrap_or(&Value::Null);
                self.read_usage(response.get("usage").unwrap_or(&Value::Null));
                let status_incomplete =
                    response.get("status").and_then(Value::as_str) == Some("incomplete");
                if status_incomplete || ty == "response.incomplete" {
                    // Truncation must not read as a clean finish; truncated tool args are
                    // expected under a token cap and the stop reason already says so.
                    self.incomplete = true;
                    self.args_error = false;
                    let reason = response
                        .pointer("/incomplete_details/reason")
                        .and_then(Value::as_str);
                    self.stop_reason = Some(if reason == Some("content_filter") {
                        "refusal"
                    } else {
                        "max_tokens"
                    });
                } else {
                    self.stop_reason = Some(if self.saw_tool_use {
                        "tool_use"
                    } else {
                        "end_turn"
                    });
                }
                Vec::new()
            }
            "response.failed" | "error" => {
                self.terminal = true;
                let err = data
                    .pointer("/response/error")
                    .filter(|v| v.is_object())
                    .or_else(|| data.get("error").filter(|v| v.is_object()))
                    .unwrap_or(data);
                let type_ = str_field(err, "code")
                    .or_else(|| str_field(err, "type"))
                    .unwrap_or_else(|| "api_error".into());
                let message =
                    str_field(err, "message").unwrap_or_else(|| "Codex backend error".into());
                self.upstream_error = Some(StreamError::Upstream {
                    type_: truncate_chars(&type_, MAX_ERROR_TYPE_CHARS),
                    message: truncate_chars(&message, MAX_ERROR_MESSAGE_CHARS),
                });
                Vec::new()
            }
            _ => Vec::new(),
        }
    }

    fn read_usage(&mut self, usage: &Value) {
        if let Some(total) = usage.get("input_tokens").and_then(non_negative) {
            let cached = usage
                .pointer("/input_tokens_details/cached_tokens")
                .and_then(finite)
                .map(|c| (c.max(0.0).floor() as u64).min(total));
            self.input_tokens = total - cached.unwrap_or(0);
            self.cache_read = cached;
            self.terminal_usage = true;
        }
        if let Some(out) = usage.get("output_tokens").and_then(non_negative) {
            self.output_tokens = out;
        }
    }

    fn message_start(&mut self, id: Option<String>) -> Bytes {
        self.started = true;
        let id = id.unwrap_or_else(generated_msg_id);
        self.msg_id = Some(id.clone());
        frame(
            "message_start",
            json!({
                "type": "message_start",
                "message": {
                    "id": id, "type": "message", "role": "assistant", "model": self.model,
                    "content": [], "stop_reason": null, "stop_sequence": null,
                    "usage": { "input_tokens": self.input_tokens, "output_tokens": 0 },
                },
            }),
        )
    }

    fn open_block(&mut self, block: Block) -> Vec<Bytes> {
        let mut frames = self.close_block(None);
        let index = self.blocks.len();
        frames.push(frame(
            "content_block_start",
            json!({ "type": "content_block_start", "index": index, "content_block": block.to_json() }),
        ));
        self.blocks.push(block);
        self.open = true;
        frames
    }

    fn open_mut(&mut self) -> Option<&mut Block> {
        if self.open {
            self.blocks.last_mut()
        } else {
            None
        }
    }

    fn block_delta(&self, delta: Value) -> Bytes {
        frame(
            "content_block_delta",
            json!({ "type": "content_block_delta", "index": self.blocks.len() - 1, "delta": delta }),
        )
    }

    /// Close the open block (if any). `item` is the `output_item.done` item.
    fn close_block(&mut self, item: Option<&Value>) -> Vec<Bytes> {
        if !self.open {
            return Vec::new();
        }
        self.open = false;
        let index = self.blocks.len() - 1;
        let incomplete = self.incomplete;
        let mut frames = Vec::new();
        match self.blocks.last_mut() {
            Some(Block::ToolUse { args, input, .. }) => {
                let raw = std::mem::take(args);
                if !raw.is_empty() {
                    match serde_json::from_str::<Value>(&raw) {
                        Ok(v @ Value::Object(_)) => *input = v,
                        // Running the tool with {} would fold a wrong result back into context:
                        // defer an error to `finish`, unless a token cap explains truncation.
                        _ => self.args_error |= !incomplete,
                    }
                }
            }
            Some(Block::Thinking { signature, .. }) => {
                let id = item.and_then(|i| i.get("id")).and_then(Value::as_str);
                let enc = item
                    .and_then(|i| i.get("encrypted_content"))
                    .and_then(Value::as_str);
                if let (Some(id), Some(enc)) = (id, enc) {
                    let sig = codex_stash_signature(id, enc);
                    *signature = Some(sig.clone());
                    frames.push(
                        self.block_delta(json!({ "type": "signature_delta", "signature": sig })),
                    );
                }
            }
            _ => {}
        }
        frames.push(frame(
            "content_block_stop",
            json!({ "type": "content_block_stop", "index": index }),
        ));
        frames
    }

    fn check(&self) -> Result<(), StreamError> {
        if let Some(e) = &self.upstream_error {
            return Err(e.clone());
        }
        if self.args_error {
            return Err(StreamError::MalformedToolArgs);
        }
        if !self.terminal {
            return Err(StreamError::MissingTerminal);
        }
        if !self.started {
            return Err(StreamError::NoMessageStart);
        }
        Ok(())
    }

    fn final_stop_reason(&self) -> &'static str {
        self.stop_reason.unwrap_or("end_turn")
    }

    fn client_usage(&self) -> Value {
        let mut u = Map::new();
        u.insert("input_tokens".into(), json!(self.input_tokens));
        u.insert("output_tokens".into(), json!(self.output_tokens));
        if let Some(c) = self.cache_read {
            u.insert("cache_read_input_tokens".into(), json!(c));
        }
        Value::Object(u)
    }

    fn message_json(&self) -> Value {
        json!({
            "id": self.msg_id.clone().unwrap_or_else(generated_msg_id),
            "type": "message",
            "role": "assistant",
            "model": self.model,
            "content": self.blocks.iter().map(Block::to_json).collect::<Vec<_>>(),
            "stop_reason": self.final_stop_reason(),
            "stop_sequence": null,
            "usage": self.client_usage(),
        })
    }
}

fn frame(event: &str, payload: Value) -> Bytes {
    encode(event, &payload.to_string())
}

fn ping() -> Bytes {
    frame("ping", json!({ "type": "ping" }))
}

fn generated_msg_id() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("msg_{nanos:x}")
}

/// A string field; numbers/bools are stringified (bro `String(x)`), null/absent is `None`.
fn str_field(v: &Value, key: &str) -> Option<String> {
    match v.get(key)? {
        Value::String(s) => Some(s.clone()),
        Value::Null => None,
        other @ (Value::Number(_) | Value::Bool(_)) => Some(other.to_string()),
        _ => None,
    }
}

fn delta_text(data: &Value) -> String {
    str_field(data, "delta").unwrap_or_default()
}

fn finite(v: &Value) -> Option<f64> {
    v.as_f64().filter(|f| f.is_finite())
}

/// A finite non-negative number, floored.
fn non_negative(v: &Value) -> Option<u64> {
    finite(v).filter(|f| *f >= 0.0).map(|f| f.floor() as u64)
}

fn truncate_chars(s: &str, max: usize) -> String {
    s.chars().take(max).collect()
}
