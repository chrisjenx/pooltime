//! Claude Code auto-mode safety classifier detection (INV §5; port of bro `server/classifier.ts`).
//!
//! The classifier is a plain Sonnet request, so without detection it inherits a Sonnet family
//! mapping and lands on Codex. When `routing.classifier_route = "anthropic"` (the default) the
//! inference handler starts it on Claude accounts instead (see [`super::inference`]).

use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::config::ClassifierRoute;
use crate::models::family_of;

/// Opening sentence of the classifier's system prompt.
pub const MARKER: &str = "You are a security monitor for autonomous AI coding agents";

/// Observed `max_tokens` is 64; the margin tolerates modest growth.
pub const CLASSIFIER_MAX_TOKENS: u64 = 1024;

/// Minimum spacing between two marker-drift warnings.
pub const DRIFT_WARN_INTERVAL: Duration = Duration::from_secs(5 * 60);

/// The classifier's shape independent of its wording: non-streaming, no tools, `max_tokens`
/// at most [`CLASSIFIER_MAX_TOKENS`]. Too loose to route on by itself.
pub fn shape_matches(json: &Value) -> bool {
    let Some(obj) = json.as_object() else {
        return false;
    };
    if obj.get("stream") == Some(&Value::Bool(true)) {
        return false;
    }
    if obj
        .get("tools")
        .and_then(Value::as_array)
        .is_some_and(|t| !t.is_empty())
    {
        return false;
    }
    obj.get("max_tokens")
        .and_then(Value::as_f64)
        .is_some_and(|n| n <= CLASSIFIER_MAX_TOKENS as f64)
}

/// Text of every system block (a string system prompt is one block).
fn system_texts(json: &Value) -> Vec<&str> {
    match json.get("system") {
        Some(Value::String(s)) => vec![s.as_str()],
        Some(Value::Array(blocks)) => blocks
            .iter()
            .filter_map(|b| b.get("text").and_then(Value::as_str))
            .collect(),
        _ => Vec::new(),
    }
}

/// True iff `json` is the auto-mode classifier: [`shape_matches`] plus a system block that
/// starts with [`MARKER`]. The shape guard keeps a prompt that merely quotes the sentence out.
/// Whether the request is then routed specially also depends on `routing.classifier_route`
/// (see [`bypasses_mapping`]).
pub fn is_classifier(json: &Value) -> bool {
    shape_matches(json) && system_texts(json).iter().any(|t| t.starts_with(MARKER))
}

/// Whether the handler should serve this request with the classifier plan.
pub fn bypasses_mapping(route: ClassifierRoute, json: &Value) -> bool {
    route == ClassifierRoute::Anthropic && is_classifier(json)
}

/// Throttled warning for a classifier-shaped Sonnet request *without* the marker: the one signal
/// that Claude Code reworded its prompt (which silently disables the bypass). Other helper calls
/// share the shape, so it is rate-limited to one warning per [`DRIFT_WARN_INTERVAL`].
#[derive(Debug, Default)]
pub struct DriftWarner {
    last: Mutex<Option<Instant>>,
}

impl DriftWarner {
    /// Check `json` and warn if it looks like a drifted classifier. `now` is injectable for
    /// tests. Returns whether a warning was emitted.
    pub fn note(
        &self,
        route: ClassifierRoute,
        json: &Value,
        model: &str,
        request_id: &str,
        now: Instant,
    ) -> bool {
        if route != ClassifierRoute::Anthropic
            || family_of(model) != Some("sonnet")
            || !shape_matches(json)
            || is_classifier(json)
        {
            return false;
        }
        let mut last = self.last.lock().unwrap_or_else(|e| e.into_inner());
        if last.is_some_and(|t| now.saturating_duration_since(t) < DRIFT_WARN_INTERVAL) {
            return false;
        }
        *last = Some(now);
        tracing::warn!(
            request_id,
            model,
            "classifier-shaped Sonnet request without the auto-mode marker; if auto mode reports \
             timeouts, the classifier marker may be stale"
        );
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn classifier_body() -> Value {
        json!({
            "model": "claude-sonnet-5",
            "max_tokens": 64,
            "system": [
                {"type": "text", "text": "x-anthropic-billing-header: cc_version=2"},
                {"type": "text", "text": format!("{MARKER}. Your job is to ...")}
            ],
            "messages": [{"role": "user", "content": "tool call"}]
        })
    }

    fn with(mut v: Value, k: &str, x: Value) -> Value {
        v[k] = x;
        v
    }

    #[test]
    fn recognises_classifier_shape_and_marker() {
        assert!(is_classifier(&classifier_body()));
        // String system prompt.
        let s = with(
            classifier_body(),
            "system",
            json!(format!("{MARKER}. Rules")),
        );
        assert!(is_classifier(&s));
        // Marker only in the first (and only) block.
        let one = with(
            classifier_body(),
            "system",
            json!([{"type": "text", "text": format!("{MARKER}.")}]),
        );
        assert!(is_classifier(&one));
    }

    #[test]
    fn rejects_non_classifier_requests() {
        let no_marker = with(
            classifier_body(),
            "system",
            json!([{"type": "text", "text": "You are a Claude agent."}]),
        );
        assert!(!is_classifier(&no_marker));
        // Marker quoted in a user message only.
        let quoted = with(
            no_marker.clone(),
            "messages",
            json!([{"role": "user", "content": format!("Please quote: {MARKER}")}]),
        );
        assert!(!is_classifier(&quoted));
        // Marker not at the start of the block.
        let mid = with(
            classifier_body(),
            "system",
            json!(format!("Preamble. {MARKER}")),
        );
        assert!(!is_classifier(&mid));
        assert!(!is_classifier(&with(
            classifier_body(),
            "stream",
            json!(true)
        )));
        assert!(!is_classifier(&with(
            classifier_body(),
            "tools",
            json!([{"name": "Bash"}])
        )));
        assert!(!is_classifier(&with(
            classifier_body(),
            "max_tokens",
            json!(1025)
        )));
        assert!(!is_classifier(&with(
            classifier_body(),
            "max_tokens",
            json!(4096)
        )));
        assert!(!is_classifier(&json!(null)));
        assert!(!is_classifier(&json!("nope")));
        // Boundary and empty tools still match.
        assert!(is_classifier(&with(
            classifier_body(),
            "max_tokens",
            json!(1024)
        )));
        assert!(is_classifier(&with(classifier_body(), "tools", json!([]))));
    }

    #[test]
    fn classifier_route_default_disables_bypass() {
        assert!(bypasses_mapping(
            ClassifierRoute::Anthropic,
            &classifier_body()
        ));
        assert!(!bypasses_mapping(
            ClassifierRoute::Default,
            &classifier_body()
        ));
    }

    #[test]
    fn shape_matches_without_marker() {
        let drifted = with(
            classifier_body(),
            "system",
            json!([{"type": "text", "text": "You are a safety reviewer for coding agents."}]),
        );
        assert!(shape_matches(&drifted));
        assert!(!is_classifier(&drifted));
        assert!(!shape_matches(&with(drifted, "stream", json!(true))));
    }

    #[test]
    fn marker_drift_warning_throttled() {
        let w = DriftWarner::default();
        let drifted = with(
            classifier_body(),
            "system",
            json!([{"type": "text", "text": "You are a safety reviewer."}]),
        );
        let t0 = Instant::now();
        let r = ClassifierRoute::Anthropic;
        assert!(w.note(r, &drifted, "claude-sonnet-5", "req_a", t0));
        // Within five minutes: suppressed.
        assert!(!w.note(r, &drifted, "claude-sonnet-5", "req_b", t0));
        assert!(!w.note(
            r,
            &drifted,
            "claude-sonnet-5",
            "req_c",
            t0 + DRIFT_WARN_INTERVAL - Duration::from_millis(1)
        ));
        // Five minutes later: warns again.
        assert!(w.note(
            r,
            &drifted,
            "claude-sonnet-5",
            "req_d",
            t0 + DRIFT_WARN_INTERVAL
        ));
        let later = t0 + DRIFT_WARN_INTERVAL * 3;
        // Never for: the real classifier, non-Sonnet models, other shapes, or route=default.
        assert!(!w.note(r, &classifier_body(), "claude-sonnet-5", "req_e", later));
        assert!(!w.note(r, &drifted, "claude-opus-5", "req_f", later));
        assert!(!w.note(
            r,
            &with(drifted.clone(), "max_tokens", json!(64000)),
            "claude-sonnet-5",
            "req_g",
            later
        ));
        assert!(!w.note(
            ClassifierRoute::Default,
            &drifted,
            "claude-sonnet-5",
            "req_h",
            later
        ));
        // The window was not consumed by the ignored calls.
        assert!(w.note(r, &drifted, "sonnet", "req_i", later));
    }
}
