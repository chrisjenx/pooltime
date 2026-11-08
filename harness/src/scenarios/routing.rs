//! Routing across accounts and providers, and the merged model listing.

use std::time::Duration;

use serde_json::json;

use super::{claude, claude_body, codex, Scenario, Step};
use crate::fake::*;

/// Opening sentence of Claude Code's auto-mode classifier system prompt (bro `classifier.ts`).
const CLASSIFIER_MARKER: &str = "You are a security monitor for autonomous AI coding agents";

fn classifier_body() -> serde_json::Value {
    json!({
        "model": "claude-sonnet-5",
        "max_tokens": 64,
        "stream": false,
        "system": [{"type": "text", "text": format!("{CLASSIFIER_MARKER}. Decide.")}],
        "messages": [{"role": "user", "content": "tool call: ls"}],
        "metadata": {"user_id": "session-classifier"},
    })
}

pub fn scenarios() -> Vec<Scenario> {
    vec![
        Scenario::new(
            "cross_provider_fallback",
            "mapped Sonnet request: the only Claude account is quota-limited, so the turn is served by Codex",
            |up| {
                up.anthropic.route(
                    MESSAGES,
                    by_account(|_, _, _| quota_429(Duration::from_secs(3600))),
                );
                up.openai.script(RESPONSES, vec![codex_ok()]);
            },
        )
        .accounts(vec![claude("alpha", 1), codex("cx", 1)])
        .mapping()
        .steps(vec![Step::messages(claude_body(
            "claude-sonnet-5",
            true,
            Some("session-fallback"),
        ))]),
        Scenario::new(
            "classifier",
            "auto-mode classifier request with Sonnet mapped to Codex starts on Claude; when Claude is exhausted it falls back to Codex",
            |up| {
                up.anthropic.route(
                    MESSAGES,
                    by_account(|_, i, _| {
                        if i == 0 {
                            anthropic_ok_json("ALLOW")
                        } else {
                            quota_429(Duration::from_secs(3600))
                        }
                    }),
                );
                up.openai.script(RESPONSES, vec![codex_ok()]);
            },
        )
        .accounts(vec![claude("alpha", 1), codex("cx", 1)])
        .mapping()
        .steps(vec![
            Step::messages(classifier_body()),
            Step::messages(classifier_body()),
        ]),
        Scenario::new(
            "sticky_session",
            "two equal accounts: a session stays on its account; a second concurrent session goes to the other",
            |up| {
                up.anthropic
                    .route(MESSAGES, by_account(|_, _, _| anthropic_ok("ok")));
            },
        )
        .accounts(vec![claude("alpha", 100), claude("beta", 100)])
        .canonical()
        .steps(vec![
            Step::messages(claude_body("claude-sonnet-5", true, Some("session-one"))),
            Step::messages(claude_body("claude-sonnet-5", true, Some("session-one"))),
            Step::messages(claude_body("claude-sonnet-5", true, Some("session-two"))),
            Step::messages(claude_body("claude-sonnet-5", true, Some("session-one"))),
            Step::messages(claude_body("claude-sonnet-5", true, Some("session-two"))),
        ]),
        Scenario::new(
            "models_merge",
            "GET /v1/models: live Anthropic catalog first, then table aliases not listed, then Codex routes",
            |up| {
                up.anthropic.script(
                    ANTHROPIC_MODELS,
                    vec![Reply::json(
                        200,
                        &json!({
                            "data": [
                                {"type": "model", "id": "claude-opus-5", "display_name": "Claude Opus 5",
                                 "created_at": "2026-05-01T00:00:00Z"},
                                {"type": "model", "id": "claude-sonnet-5", "display_name": "Claude Sonnet 5",
                                 "created_at": "2026-05-01T00:00:00Z"},
                                {"type": "model", "id": "claude-harness-9", "display_name": "Claude Harness 9",
                                 "created_at": "2026-10-01T00:00:00Z", "max_input_tokens": 500000}
                            ],
                            "has_more": false,
                        }),
                    )],
                );
            },
        )
        .accounts(vec![claude("alpha", 1), codex("cx", 1)])
        .steps(vec![Step::get("/v1/models")]),
    ]
}
