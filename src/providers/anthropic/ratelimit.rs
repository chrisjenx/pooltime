//! Parsing of Anthropic's unified rate-limit response headers and quota-signal detection
//! (INV §1 429 handling, §3).
//!
//! Subscription (OAuth) responses carry `anthropic-ratelimit-unified-<key>-{status,utilization,
//! reset}` triples, parsed generically so model-scoped windows (`7d-fable`) and future ones need no
//! code change. Utilization in these headers is already a 0-1 fraction; reset is unix seconds.

use std::collections::BTreeMap;

use http::HeaderMap;
use time::OffsetDateTime;

use crate::accounts::status::window_duration;
use crate::accounts::{RateLimitSnapshot, Window, WindowStatus};

const UNIFIED_PREFIX: &str = "anthropic-ratelimit-unified-";

/// Model-scope of a window key: the tokens that are not a duration token, joined with `-`
/// (`7d-fable` -> `fable`, `5h` -> `None`). Tolerates either token order and `_` separators.
///
/// Like bro's `windowModelOf`, a key with no duration token at all (`overage`) is "scoped" to its
/// own name: it then never binds a request for a real model family. That is deliberate: Claude
/// responses routinely carry `overage-status: rejected` (extra usage not enabled), which must not
/// read as a spent account-wide window.
pub(crate) fn window_model_of(key: &str) -> Option<String> {
    let scopes: Vec<String> = key
        .split(['-', '_'])
        .filter(|t| !t.is_empty() && window_duration(t).is_none())
        .map(str::to_ascii_lowercase)
        .collect();
    (!scopes.is_empty()).then(|| scopes.join("-"))
}

fn window_status(raw: &str) -> WindowStatus {
    match raw.to_ascii_lowercase().as_str() {
        "allowed" => WindowStatus::Allowed,
        "allowed_warning" => WindowStatus::AllowedWarning,
        "rejected" => WindowStatus::Rejected,
        _ => WindowStatus::Unknown,
    }
}

/// Windows (plus the overall `unified-status`) reported by the response headers, or `None` when
/// the response carries no unified headers at all. A window missing its `utilization` reads 0.
/// Merge with [`crate::accounts::StateBook::merge_header_snapshot`].
pub fn parse_unified_headers(h: &HeaderMap, now: OffsetDateTime) -> Option<RateLimitSnapshot> {
    let mut unified_status = None;
    let mut windows: BTreeMap<String, Window> = BTreeMap::new();

    for (name, value) in h {
        let Some(rest) = name.as_str().strip_prefix(UNIFIED_PREFIX) else {
            continue;
        };
        let Ok(value) = value.to_str().map(str::trim) else {
            continue;
        };
        if rest == "status" {
            unified_status = Some(value.to_string());
            continue;
        }
        let Some((key, field)) = rest.rsplit_once('-') else {
            continue;
        };
        if key.is_empty() || !matches!(field, "status" | "utilization" | "reset") {
            continue;
        }
        let w = windows.entry(key.to_string()).or_insert_with(|| Window {
            key: key.to_string(),
            model: window_model_of(key),
            status: WindowStatus::Unknown,
            utilization: 0.0,
            reset: None,
        });
        match field {
            "status" => w.status = window_status(value),
            "utilization" => {
                if let Some(u) = value.parse::<f64>().ok().filter(|u| u.is_finite()) {
                    w.utilization = u.clamp(0.0, 1.0);
                }
            }
            _ => {
                w.reset = value
                    .parse::<i64>()
                    .ok()
                    .and_then(|s| OffsetDateTime::from_unix_timestamp(s).ok());
            }
        }
    }

    if unified_status.is_none() && windows.is_empty() {
        return None;
    }
    let mut windows: Vec<Window> = windows.into_values().collect();
    windows.sort_by(|a, b| {
        a.model
            .is_some()
            .cmp(&b.model.is_some())
            .then_with(|| a.key.cmp(&b.key))
    });
    Some(RateLimitSnapshot {
        unified_status,
        windows,
        updated_at: now,
    })
}

/// Whether a 429 is a per-account quota refusal rather than a per-request one (INV §1; bro
/// `classifyHttpError`): any `anthropic-ratelimit-unified-*` header, a `retry-after` header, or
/// rate-limit wording in the error message. The error *type* alone proves nothing (Anthropic also
/// answers `rate_limit_error` for requests it refuses outright), so only `error.message` - or, when
/// the body is not an error envelope, the first 500 characters of it - is sniffed.
pub fn has_quota_signal(h: &HeaderMap, body: &str) -> bool {
    h.keys().any(|k| k.as_str().starts_with(UNIFIED_PREFIX))
        || h.contains_key(http::header::RETRY_AFTER)
        || has_rate_limit_wording(&error_message(body))
}

fn error_message(body: &str) -> String {
    serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|v| v.get("error")?.get("message")?.as_str().map(str::to_owned))
        .unwrap_or_else(|| body.chars().take(500).collect())
}

fn has_rate_limit_wording(text: &str) -> bool {
    let t = text.to_lowercase();
    [
        "rate_limit",
        "rate limit",
        "usage limit",
        "limit reached",
        "too many requests",
    ]
    .iter()
    .any(|k| t.contains(k))
}

#[cfg(test)]
mod tests {
    use super::*;
    use http::{HeaderName, HeaderValue};
    use time::macros::datetime;

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.insert(
                HeaderName::from_bytes(k.as_bytes()).unwrap(),
                HeaderValue::from_str(v).unwrap(),
            );
        }
        h
    }

    #[test]
    fn parses_model_scoped_windows() {
        let now = datetime!(2026-07-13 00:00 UTC);
        let h = headers(&[
            ("anthropic-ratelimit-unified-status", "allowed_warning"),
            ("anthropic-ratelimit-unified-5h-status", "allowed"),
            ("anthropic-ratelimit-unified-5h-utilization", "0.39"),
            ("anthropic-ratelimit-unified-5h-reset", "1783900800"),
            ("anthropic-ratelimit-unified-7d-status", "allowed_warning"),
            ("anthropic-ratelimit-unified-7d-utilization", "0.81"),
            ("anthropic-ratelimit-unified-7d-reset", "1784160000"),
            ("anthropic-ratelimit-unified-7d-fable-status", "rejected"),
            ("anthropic-ratelimit-unified-7d-fable-utilization", "1"),
            ("anthropic-ratelimit-unified-7d-fable-reset", "1784160001"),
            // Not window triples: ignored.
            ("anthropic-ratelimit-unified-reset", "1783900800"),
            ("anthropic-ratelimit-unified-5h-surpassed-threshold", "0.9"),
            ("x-other", "1"),
        ]);
        let snap = parse_unified_headers(&h, now).unwrap();
        assert_eq!(snap.unified_status.as_deref(), Some("allowed_warning"));
        assert_eq!(snap.updated_at, now);
        let keys: Vec<(&str, Option<&str>)> = snap
            .windows
            .iter()
            .map(|w| (w.key.as_str(), w.model.as_deref()))
            .collect();
        // Account-wide first, then model-scoped.
        assert_eq!(
            keys,
            vec![("5h", None), ("7d", None), ("7d-fable", Some("fable"))]
        );
        let w5 = &snap.windows[0];
        assert_eq!(w5.status, WindowStatus::Allowed);
        assert_eq!(w5.utilization, 0.39);
        assert_eq!(
            w5.reset,
            Some(OffsetDateTime::from_unix_timestamp(1_783_900_800).unwrap())
        );
        let wf = &snap.windows[2];
        assert_eq!(wf.status, WindowStatus::Rejected);
        assert_eq!(wf.utilization, 1.0);
        assert_eq!(snap.windows[1].status, WindowStatus::AllowedWarning);
    }

    #[test]
    fn header_parsing_edge_cases() {
        let now = datetime!(2026-07-13 00:00 UTC);
        assert!(parse_unified_headers(&headers(&[("retry-after", "30")]), now).is_none());
        // Utilization is clamped to 0..=1; garbage values leave the defaults; unknown status.
        let snap = parse_unified_headers(
            &headers(&[
                ("anthropic-ratelimit-unified-5h-utilization", "7"),
                ("anthropic-ratelimit-unified-5h-reset", "soon"),
                ("anthropic-ratelimit-unified-5h-status", "weird"),
                ("anthropic-ratelimit-unified-opus_7d-utilization", "nan"),
            ]),
            now,
        )
        .unwrap();
        assert_eq!(snap.unified_status, None);
        let w5 = &snap.windows[0];
        assert_eq!(
            (w5.utilization, w5.reset, w5.status),
            (1.0, None, WindowStatus::Unknown)
        );
        // Either token order and `_` separators are model-scoped; `overage` is never account-wide.
        let opus = &snap.windows[1];
        assert_eq!(
            (opus.key.as_str(), opus.model.as_deref(), opus.utilization),
            ("opus_7d", Some("opus"), 0.0)
        );
        assert_eq!(window_model_of("overage").as_deref(), Some("overage"));
        assert_eq!(window_model_of("5h"), None);
    }

    #[test]
    fn quota_signal_detection() {
        let none = HeaderMap::new();
        let quota_msg = r#"{"type":"error","error":{"type":"rate_limit_error","message":"You have reached your usage limit"}}"#;
        // Per-request refusal: 429 with the rate_limit_error *type* but nothing else.
        let refusal = r#"{"type":"error","error":{"type":"rate_limit_error","message":"Error"}}"#;
        assert!(!has_quota_signal(&none, refusal));
        assert!(!has_quota_signal(&none, r#"{"message":"Error"}"#));
        assert!(!has_quota_signal(&none, ""));

        // Headers.
        let unified = headers(&[("anthropic-ratelimit-unified-5h-status", "rejected")]);
        assert!(has_quota_signal(&unified, refusal));
        let unified_overall = headers(&[("anthropic-ratelimit-unified-status", "rejected")]);
        assert!(has_quota_signal(&unified_overall, ""));
        assert!(has_quota_signal(
            &headers(&[("retry-after", "30")]),
            refusal
        ));
        // Non-unified rate-limit headers are not a signal.
        let other = headers(&[("anthropic-ratelimit-requests-remaining", "0")]);
        assert!(!has_quota_signal(&other, refusal));

        // Wording, case-insensitively.
        assert!(has_quota_signal(&none, quota_msg));
        for msg in [
            "Rate limit exceeded",
            "usage LIMIT hit",
            "Limit reached for this window",
            "Too Many Requests",
            "rate_limit",
        ] {
            let body = serde_json::json!({"error": {"type": "x", "message": msg}}).to_string();
            assert!(has_quota_signal(&none, &body), "{msg}");
        }
        // A body that is not an error envelope is sniffed as plain text.
        assert!(has_quota_signal(&none, "429 Too Many Requests"));
        assert!(!has_quota_signal(&none, "gateway timeout"));
    }
}
