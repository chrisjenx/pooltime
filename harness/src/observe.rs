//! Client side: send a scenario's steps to one daemon and reduce what came back to comparable
//! observations (status, selected headers, semantic SSE sequence or JSON projection).
//!
//! Nothing here keeps response text beyond what a projection needs (event kinds, delta text the
//! fakes produced, error *types*); upstream/daemon error messages are dropped.

use std::time::Duration;

use futures_util::StreamExt;
use serde_json::Value;

use crate::scenarios::{Read, Step};

/// Response headers that are compared (or allowlisted).
pub const SELECTED_HEADERS: &[&str] = &[
    "content-type",
    "retry-after",
    "x-pool-account",
    "x-pool-route",
    "x-pool-upstream-rejected",
    "x-pool-retryable-transport",
    "request-id",
];

/// Per-step budget (covers overload backoffs and the silent-upstream scenario).
const STEP_TIMEOUT: Duration = Duration::from_secs(45);

/// One step's observation, already flattened to `(field, value)` pairs (field relative to the
/// step, e.g. `status`, `header.content-type`, `sse`, `end`, `json`).
pub type Fields = Vec<(String, String)>;

pub async fn run_step(client: &reqwest::Client, base: &str, step: &Step) -> Fields {
    let url = format!("{base}{}", step.path);
    let mut req = match step.method {
        "GET" => client.get(&url),
        _ => client.post(&url),
    };
    req = req.header("anthropic-version", "2023-06-01");
    if let Some(b) = &step.body {
        req = req
            .header("content-type", "application/json")
            .body(b.to_string());
    }
    match tokio::time::timeout(STEP_TIMEOUT, observe(req, step.read)).await {
        Ok(f) => f,
        Err(_) => vec![("end".into(), "harness-timeout".into())],
    }
}

async fn observe(req: reqwest::RequestBuilder, read: Read) -> Fields {
    let mut out: Fields = Vec::new();
    let resp = match req.send().await {
        Ok(r) => r,
        Err(e) => {
            out.push(("end".into(), format!("send-error:{}", error_kind(&e))));
            return out;
        }
    };
    out.push(("status".into(), resp.status().as_u16().to_string()));
    for h in SELECTED_HEADERS {
        let v = resp
            .headers()
            .get(*h)
            .and_then(|v| v.to_str().ok())
            .map(|v| {
                if *h == "content-type" {
                    v.split(';').next().unwrap_or(v).trim().to_string()
                } else {
                    v.to_string()
                }
            });
        if let Some(v) = v {
            out.push((format!("header.{h}"), v));
        }
    }
    let sse = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.starts_with("text/event-stream"));
    if sse {
        let (events, end) = read_sse(resp, read).await;
        out.push(("sse".into(), summarize(&events).join(" | ")));
        // Ping *presence* is compared (keep-alive is behavior); count and timing are not.
        let pings = events.iter().any(|(e, _)| e == "ping");
        out.push(("pings".into(), if pings { "some" } else { "none" }.into()));
        out.push(("end".into(), end));
    } else {
        match resp.bytes().await {
            Ok(b) => out.push(("json".into(), project_json(&b))),
            Err(e) => out.push(("end".into(), format!("body-error:{}", error_kind(&e)))),
        }
    }
    out
}

fn error_kind(e: &reqwest::Error) -> &'static str {
    if e.is_timeout() {
        "timeout"
    } else if e.is_connect() {
        "connect"
    } else if e.is_body() || e.is_decode() {
        "body"
    } else {
        "request"
    }
}

/// Read SSE frames until the body ends (or the client hangs up per `read`). Returns the parsed
/// `(event, data)` frames and how the stream ended: `clean`, `reset` (body error) or
/// `client-hangup`.
async fn read_sse(resp: reqwest::Response, read: Read) -> (Vec<(String, Value)>, String) {
    let mut stream = resp.bytes_stream();
    let mut buf: Vec<u8> = Vec::new();
    let mut events = Vec::new();
    loop {
        match stream.next().await {
            Some(Ok(chunk)) => {
                buf.extend_from_slice(&chunk);
                for f in drain_frames(&mut buf) {
                    events.push(f);
                    if let Read::DisconnectAfter(n) = read {
                        let real = events.iter().filter(|(e, _)| e != "ping").count();
                        if real >= n {
                            return (events, "client-hangup".into());
                        }
                    }
                }
            }
            Some(Err(_)) => return (events, "reset".into()),
            None => {
                buf.extend_from_slice(b"\n\n");
                events.extend(drain_frames(&mut buf));
                return (events, "clean".into());
            }
        }
    }
}

/// Split complete SSE frames off the front of `buf`.
pub fn drain_frames(buf: &mut Vec<u8>) -> Vec<(String, Value)> {
    let mut out = Vec::new();
    loop {
        let lf = find(buf, b"\n\n").map(|i| (i, 2));
        let crlf = find(buf, b"\r\n\r\n").map(|i| (i, 4));
        let Some((end, sep)) = [lf, crlf].into_iter().flatten().min() else {
            break;
        };
        let frame = String::from_utf8_lossy(&buf[..end]).replace("\r\n", "\n");
        buf.drain(..end + sep);
        let mut event = String::new();
        let mut data = String::new();
        for line in frame.lines() {
            if let Some(v) = line.strip_prefix("event:") {
                event = v.trim().to_string();
            } else if let Some(v) = line.strip_prefix("data:") {
                if !data.is_empty() {
                    data.push('\n');
                }
                data.push_str(v.strip_prefix(' ').unwrap_or(v));
            }
        }
        if event.is_empty() && data.is_empty() {
            continue; // comment / keep-alive line
        }
        let value = serde_json::from_str(&data).unwrap_or(Value::Null);
        if event.is_empty() {
            event = value["type"].as_str().unwrap_or("message").to_string();
        }
        out.push((event, value));
    }
    out
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

/// Semantic SSE sequence: pings dropped, consecutive deltas of one block coalesced (so chunking
/// differences do not matter), error events reduced to their type.
pub fn summarize(events: &[(String, Value)]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    // (index, delta type, accumulated text) of the delta run being coalesced.
    let mut run: Option<(i64, String, String)> = None;
    let flush = |run: &mut Option<(i64, String, String)>, out: &mut Vec<String>| {
        if let Some((_, ty, text)) = run.take() {
            out.push(format!("delta:{ty}:{text:?}"));
        }
    };
    for (event, data) in events {
        if event == "ping" {
            continue;
        }
        if event == "content_block_delta" {
            let idx = data["index"].as_i64().unwrap_or(-1);
            let ty = data["delta"]["type"].as_str().unwrap_or("?").to_string();
            let piece = match ty.as_str() {
                "text_delta" => data["delta"]["text"].as_str(),
                "thinking_delta" => data["delta"]["thinking"].as_str(),
                "input_json_delta" => data["delta"]["partial_json"].as_str(),
                "signature_delta" => data["delta"]["signature"].as_str(),
                _ => None,
            }
            .unwrap_or("")
            .to_string();
            match &mut run {
                Some((i, t, text)) if *i == idx && *t == ty => text.push_str(&piece),
                _ => {
                    flush(&mut run, &mut out);
                    run = Some((idx, ty, piece));
                }
            }
            continue;
        }
        flush(&mut run, &mut out);
        out.push(match event.as_str() {
            "content_block_start" => {
                let b = &data["content_block"];
                let ty = b["type"].as_str().unwrap_or("?");
                match b["name"].as_str() {
                    Some(n) => format!("block_start:{ty}:{n}"),
                    None => format!("block_start:{ty}"),
                }
            }
            "content_block_stop" => "block_stop".into(),
            "message_delta" => format!(
                "message_delta:{}",
                data["delta"]["stop_reason"].as_str().unwrap_or("null")
            ),
            "error" => format!("error:{}", data["error"]["type"].as_str().unwrap_or("?")),
            other => other.to_string(),
        });
    }
    flush(&mut run, &mut out);
    out
}

/// Comparable projection of a non-SSE body.
pub fn project_json(body: &[u8]) -> String {
    let Ok(v) = serde_json::from_slice::<Value>(body) else {
        return format!("non-json({} bytes)", body.len());
    };
    if v["object"] == "list" {
        let ids: Vec<String> = v["data"]
            .as_array()
            .map(|d| {
                d.iter()
                    .map(|m| {
                        let mut s = format!(
                            "{}<{}>",
                            m["id"].as_str().unwrap_or("?"),
                            m["owned_by"].as_str().unwrap_or("-")
                        );
                        for k in ["context_window", "max_context_window", "max_input_tokens"] {
                            if let Some(n) = m[k].as_u64() {
                                s.push_str(&format!(" {k}={n}"));
                            }
                        }
                        s
                    })
                    .collect()
            })
            .unwrap_or_default();
        return format!("list[{}]", ids.join(", "));
    }
    match v["type"].as_str() {
        Some("error") => format!("error:{}", v["error"]["type"].as_str().unwrap_or("?")),
        Some("message") => {
            let content: Vec<String> = v["content"]
                .as_array()
                .map(|c| {
                    c.iter()
                        .map(|b| {
                            let ty = b["type"].as_str().unwrap_or("?");
                            match ty {
                                "text" => format!("text:{:?}", b["text"].as_str().unwrap_or("")),
                                "thinking" => format!(
                                    "thinking:{:?}:sig={}",
                                    b["thinking"].as_str().unwrap_or(""),
                                    b["signature"].as_str().is_some()
                                ),
                                t => t.to_string(),
                            }
                        })
                        .collect()
                })
                .unwrap_or_default();
            format!(
                "message stop={} content=[{}]",
                v["stop_reason"].as_str().unwrap_or("null"),
                content.join(", ")
            )
        }
        _ => {
            let mut keys: Vec<&str> = v
                .as_object()
                .map(|o| o.keys().map(String::as_str).collect())
                .unwrap_or_default();
            keys.sort_unstable();
            format!("json{{{}}}", keys.join(","))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn frames(s: &str) -> Vec<(String, Value)> {
        let mut b = s.as_bytes().to_vec();
        drain_frames(&mut b)
    }

    #[test]
    fn pings_dropped_and_deltas_coalesced() {
        let ev = frames(concat!(
            "event: message_start\ndata: {\"type\":\"message_start\"}\n\n",
            "event: ping\ndata: {\"type\":\"ping\"}\n\n",
            "event: content_block_start\ndata: {\"index\":0,\"content_block\":{\"type\":\"text\"}}\n\n",
            "event: content_block_delta\ndata: {\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Hi\"}}\n\n",
            "event: ping\ndata: {\"type\":\"ping\"}\n\n",
            "event: content_block_delta\ndata: {\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\" there\"}}\n\n",
            "event: content_block_stop\ndata: {\"index\":0}\n\n",
            "event: message_delta\ndata: {\"delta\":{\"stop_reason\":\"end_turn\"}}\n\n",
            "event: error\ndata: {\"type\":\"error\",\"error\":{\"type\":\"api_error\",\"message\":\"secret prompt\"}}\n\n",
        ));
        assert_eq!(
            summarize(&ev),
            vec![
                "message_start",
                "block_start:text",
                "delta:text_delta:\"Hi there\"",
                "block_stop",
                "message_delta:end_turn",
                "error:api_error",
            ]
        );
    }

    #[test]
    fn partial_frames_wait_for_terminator_and_crlf_is_accepted() {
        let mut b = b"event: a\r\ndata: {}\r\n\r\nevent: b\ndata: {".to_vec();
        let got = drain_frames(&mut b);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].0, "a");
        assert_eq!(b, b"event: b\ndata: {".to_vec());
    }

    #[test]
    fn json_projection_drops_messages() {
        let e =
            json!({"type": "error", "error": {"type": "rate_limit_error", "message": "secret"}});
        assert_eq!(
            project_json(e.to_string().as_bytes()),
            "error:rate_limit_error"
        );
        let l =
            json!({"object": "list", "data": [{"id": "a", "owned_by": "x", "context_window": 5}]});
        assert_eq!(
            project_json(l.to_string().as_bytes()),
            "list[a<x> context_window=5]"
        );
    }
}
