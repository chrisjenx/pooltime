//! Anthropic-side failure handling: rate limits, overload, refusals, refresh death, stream cuts.

use std::time::Duration;

use super::{claude, claude_body, Scenario, Step};
use crate::fake::*;

const SONNET: &str = "claude-sonnet-5";

/// `alpha` (preferred) answers with `first`; `beta` always serves.
fn alpha_then_beta(up: &Upstreams, first: fn() -> Reply) {
    up.anthropic.route(
        MESSAGES,
        by_account(move |acct, _, _| match acct {
            "alpha" => first(),
            _ => anthropic_ok("from beta"),
        }),
    );
}

pub fn scenarios() -> Vec<Scenario> {
    let two = || vec![claude("alpha", 1), claude("beta", 2)];
    let one = || vec![claude("alpha", 1)];
    let stream = || Step::messages(claude_body(SONNET, true, None));
    vec![
        Scenario::new(
            "quota_429",
            "429 with unified rate-limit headers (5h rejected, reset in 1h) fails over and benches alpha until the reset",
            |up| alpha_then_beta(up, || quota_429(Duration::from_secs(3600))),
        )
        .accounts(two())
        .steps(vec![stream()]),
        Scenario::new(
            "headerless_429",
            "429 with no quota signal is a per-request refusal: passed through with x-pool-upstream-rejected, no bench, no failover",
            |up| {
                alpha_then_beta(up, || {
                    Reply::error(429, "rate_limit_error", "harness: slow down")
                })
            },
        )
        .accounts(two())
        .steps(vec![stream()]),
        Scenario::new(
            "overload_retry_after",
            "529 with retry-after: 1 is retried on the same account (retry-after wins, capped by the backoff cap)",
            |up| {
                up.anthropic.script(
                    MESSAGES,
                    vec![
                        Reply::error(529, "overloaded_error", "harness: overloaded")
                            .header("retry-after", "1"),
                        anthropic_ok("after overload"),
                    ],
                )
            },
        )
        .accounts(one())
        .steps(vec![stream()]),
        Scenario::new(
            "overload_no_retry_after",
            "529 twice without retry-after is retried with backoff on the same account, then served",
            |up| {
                let overloaded = Reply::error(529, "overloaded_error", "harness: overloaded");
                up.anthropic.script(
                    MESSAGES,
                    vec![
                        overloaded.clone(),
                        overloaded,
                        anthropic_ok("after overload"),
                    ],
                )
            },
        )
        .accounts(one())
        .steps(vec![stream()]),
        Scenario::new(
            "overload_exhausted",
            "529 on every attempt: same-account retries run out and the client gets the overload",
            |up| {
                up.anthropic.script(
                    MESSAGES,
                    vec![Reply::error(529, "overloaded_error", "harness: overloaded")],
                )
            },
        )
        .accounts(one())
        .steps(vec![stream()]),
        Scenario::new(
            "overload_sse",
            "200 whose SSE body opens with an overloaded_error event (before message_start) is retried, not streamed",
            |up| {
                up.anthropic.script(
                    MESSAGES,
                    vec![
                        Reply::sse(frames_now(vec![sse_error("overloaded_error")])),
                        anthropic_ok("after sse overload"),
                    ],
                )
            },
        )
        .accounts(one())
        .steps(vec![stream()]),
        Scenario::new(
            "ratelimit_sse",
            "200 whose SSE body opens with a rate_limit_error event benches alpha and fails over to beta",
            |up| {
                alpha_then_beta(up, || {
                    Reply::sse(frames_now(vec![sse_error("rate_limit_error")]))
                })
            },
        )
        .accounts(two())
        .steps(vec![stream()]),
        Scenario::new(
            "forbidden_billing",
            "403 whose message is an entitlement refusal benches alpha as billing-blocked and fails over",
            |up| {
                alpha_then_beta(up, || {
                    Reply::error(
                        403,
                        "permission_error",
                        "This organization has been disabled.",
                    )
                })
            },
        )
        .accounts(two())
        .steps(vec![stream()]),
        Scenario::new(
            "forbidden_generic",
            "403 with an unspecified refusal benches alpha as access-denied and fails over",
            |up| {
                alpha_then_beta(up, || {
                    Reply::error(403, "permission_error", "harness: request not allowed")
                })
            },
        )
        .accounts(two())
        .steps(vec![stream()]),
        Scenario::new(
            "invalid_grant",
            "expired access token whose refresh is rejected with invalid_grant: alpha needs login, beta serves",
            |up| {
                up.anthropic.route(
                    TOKEN,
                    by_refresh_account(|_, _| {
                        Reply::json(
                            400,
                            &serde_json::json!({"error": "invalid_grant",
                                                "error_description": "Refresh token not found or invalid"}),
                        )
                    }),
                );
                up.anthropic
                    .route(MESSAGES, by_account(|_, _, _| anthropic_ok("from beta")));
            },
        )
        .accounts(vec![claude("alpha", 1).expired(), claude("beta", 2)])
        .steps(vec![stream()]),
        Scenario::new(
            "pool_exhausted",
            "every account answers a quota 429: both are benched and the client gets the exhaustion error",
            |up| {
                up.anthropic.route(
                    MESSAGES,
                    by_account(|_, _, _| quota_429(Duration::from_secs(1800))),
                )
            },
        )
        .accounts(two())
        .steps(vec![stream(), stream()]),
        Scenario::new(
            "unauthorized_401",
            "401 forces a token refresh and one retry on the same account",
            |up| {
                up.anthropic.route(
                    MESSAGES,
                    by_account(|_, i, _| {
                        if i == 0 {
                            Reply::error(401, "authentication_error", "harness: token expired")
                        } else {
                            anthropic_ok("after refresh")
                        }
                    }),
                )
            },
        )
        .accounts(two())
        .steps(vec![stream()]),
        Scenario::new(
            "unknown_path",
            "an endpoint neither proxy implements (count_tokens) gets a 404",
            |_| {},
        )
        .accounts(one())
        .steps(vec![Step::post(
            "/v1/messages/count_tokens",
            claude_body(SONNET, false, None),
        )]),
        Scenario::new(
            "midstream_cut",
            "upstream connection resets after the first text delta: the client sees the partial stream end with an error",
            |up| {
                let fs = vec![message_start(), text_block_start(0), text_delta(0, "Hel")];
                up.anthropic.script(
                    MESSAGES,
                    vec![cut_after_frames(allowed_headers(Reply::sse(frames_now(fs))))],
                );
            },
        )
        .accounts(two())
        .steps(vec![stream().wait(Duration::from_millis(300))]),
        Scenario::new(
            "client_disconnect",
            "client hangs up mid-stream: the account is not blamed and the next request is served normally",
            |up| {
                up.anthropic.route(
                    MESSAGES,
                    by_account(|_, i, _| {
                        if i == 0 {
                            let mut fs = vec![message_start(), text_block_start(0)];
                            fs.extend((0..20).map(|n| text_delta(0, &format!("t{n} "))));
                            fs.push(block_stop(0));
                            fs.extend(message_end());
                            let mut r = Reply::sse(
                                fs.into_iter()
                                    .map(|f| (Duration::from_millis(150), f))
                                    .collect(),
                            );
                            r = allowed_headers(r);
                            r
                        } else {
                            anthropic_ok("second")
                        }
                    }),
                )
            },
        )
        .accounts(two())
        .steps(vec![
            stream()
                .disconnect_after(3)
                .wait(Duration::from_millis(800)),
            stream(),
        ]),
    ]
}
