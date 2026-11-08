//! Catalog-derived Claude Code pins (INV §6; port of bro `settings.js`
//! `newestOfFamily`, `sonnetPinFromCatalog`, `haikuPinFromCatalog` and
//! `codexDefaultContextFromCatalog`).
//!
//! All functions are pure over the rows `/v1/models` serves (`&[Value]`), so a
//! new Claude release reaches the picker without a pooltime release.

use std::cmp::Ordering;

use serde_json::Value;

use super::{Pin, Pins};
use crate::server::models_route::OPENAI_OWNER;

/// The newest model of a family: its bare id (any `[1m]` selector suffix
/// removed) and its picker name (`display_name` without the leading `Claude `,
/// falling back to the id).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Newest {
    pub id: String,
    pub name: String,
}

/// Newest `claude-<family>-<digit>...` row: highest `created`, then the
/// numeric-aware id compare (`claude-x-5-10` beats `claude-x-5-9`).
pub fn newest_of_family(catalog: &[Value], family: &str) -> Option<Newest> {
    let prefix = format!("claude-{family}-");
    let best = catalog
        .iter()
        .filter(|m| {
            id_of(m)
                .strip_prefix(&prefix)
                .is_some_and(|rest| rest.starts_with(|c: char| c.is_ascii_digit()))
        })
        .max_by(|a, b| {
            created(a)
                .cmp(&created(b))
                .then_with(|| numeric_cmp(id_of(a), id_of(b)))
        })?;
    let raw_id = id_of(best);
    let id = raw_id.strip_suffix("[1m]").unwrap_or(raw_id);
    let display = best
        .get("display_name")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .unwrap_or(id);
    Some(Newest {
        id: id.to_string(),
        name: strip_claude_prefix(display).to_string(),
    })
}

/// Newest Fable (used to rewrite the cached picker rows; never pinned).
pub fn newest_fable(catalog: &[Value]) -> Option<Newest> {
    newest_of_family(catalog, "fable")
}

fn id_of(m: &Value) -> &str {
    m.get("id").and_then(Value::as_str).unwrap_or_default()
}

fn created(m: &Value) -> i64 {
    let c = m.get("created");
    c.and_then(Value::as_i64)
        .or_else(|| c.and_then(Value::as_f64).map(|f| f as i64))
        .unwrap_or(0)
}

/// `^Claude\s+` removed (JS: `.replace(/^Claude\s+/, '')`).
fn strip_claude_prefix(s: &str) -> &str {
    match s.strip_prefix("Claude") {
        Some(rest) if rest.starts_with(char::is_whitespace) => rest.trim_start(),
        _ => s,
    }
}

/// Compare with digit runs as numbers (a stand-in for JS
/// `localeCompare(..., {numeric: true})` on ASCII model ids).
fn numeric_cmp(a: &str, b: &str) -> Ordering {
    let (mut a, mut b) = (chunks(a), chunks(b));
    loop {
        match (a.next(), b.next()) {
            (None, None) => return Ordering::Equal,
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(x), Some(y)) => {
                let digits = |s: &str| s.starts_with(|c: char| c.is_ascii_digit());
                let ord = if digits(x) && digits(y) {
                    let (x, y) = (x.trim_start_matches('0'), y.trim_start_matches('0'));
                    x.len().cmp(&y.len()).then_with(|| x.cmp(y))
                } else {
                    x.cmp(y)
                };
                if ord != Ordering::Equal {
                    return ord;
                }
            }
        }
    }
}

/// Maximal runs of ASCII digits / non-digits.
fn chunks(s: &str) -> impl Iterator<Item = &str> {
    let mut rest = s;
    std::iter::from_fn(move || {
        let first = rest.chars().next()?;
        let digit = first.is_ascii_digit();
        let end = rest
            .find(|c: char| c.is_ascii_digit() != digit)
            .unwrap_or(rest.len());
        let (head, tail) = rest.split_at(end);
        rest = tail;
        Some(head)
    })
}

/// Sonnet = newest `claude-sonnet-*` + `[1m]` (pinning replaces the built-in
/// row, collapsing the 200K/1M pair into one); Haiku = newest bare id (current
/// Haikus are native 1M). Wording is copied from Claude Code's own rows. A
/// family absent from the catalog gets no pin. Never a Fable pin: that would
/// add a second picker row (the cached row is rewritten instead).
pub fn derive_pins(catalog: &[Value]) -> Pins {
    Pins {
        sonnet: newest_of_family(catalog, "sonnet").map(|n| Pin {
            id: format!("{}[1m]", n.id),
            name: "Sonnet".into(),
            description: format!("{} with 1M context · Efficient for routine tasks", n.name),
        }),
        haiku: newest_of_family(catalog, "haiku").map(|n| Pin {
            id: n.id,
            name: "Haiku".into(),
            description: format!("{} · Fastest for quick answers", n.name),
        }),
    }
}

/// `CLAUDE_CODE_MAX_CONTEXT_TOKENS` fallback: the smallest valid Codex
/// `context_window` (the safest unsuffixed default). `None` when no OpenAI row
/// carries a positive integer window.
pub fn max_context(catalog: &[Value]) -> Option<u32> {
    catalog
        .iter()
        .filter(|m| m.get("owned_by").and_then(Value::as_str) == Some(OPENAI_OWNER))
        .filter_map(|m| m.get("context_window")?.as_u64())
        .filter_map(|n| u32::try_from(n).ok())
        .filter(|n| *n > 0)
        .min()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn pin(id: &str, name: &str, description: &str) -> Option<Pin> {
        Some(Pin {
            id: id.into(),
            name: name.into(),
            description: description.into(),
        })
    }

    // bro: sonnetPinFromCatalog pins the newest Sonnet 1M with Claude Code's row wording
    #[test]
    fn sonnet_pin_has_1m_and_copy() {
        let pins = derive_pins(&[
            json!({"id": "claude-sonnet-4-6", "display_name": "Claude Sonnet 4.6", "created": 100}),
            json!({"id": "claude-sonnet-5", "display_name": "Claude Sonnet 5", "created": 200}),
            json!({"id": "claude-fable-5-1", "display_name": "Claude Fable 5.1", "created": 300}),
        ]);
        assert_eq!(
            pins.sonnet,
            pin(
                "claude-sonnet-5[1m]",
                "Sonnet",
                "Sonnet 5 with 1M context · Efficient for routine tasks"
            )
        );
        // No Sonnet in the catalog: nothing pinned.
        assert_eq!(derive_pins(&[json!({"id": "claude-opus-5"})]).sonnet, None);
    }

    // bro: haikuPinFromCatalog pins the newest Haiku with Claude Code's row wording
    #[test]
    fn haiku_pin_bare_id() {
        let pins = derive_pins(&[
            json!({"id": "claude-haiku-4-5", "display_name": "Claude Haiku 4.5", "created": 100}),
            json!({"id": "claude-haiku-5-5", "display_name": "Claude Haiku 5.5", "created": 200}),
            json!({"id": "claude-sonnet-5-5", "display_name": "Claude Sonnet 5.5", "created": 300}),
        ]);
        assert_eq!(
            pins.haiku,
            pin(
                "claude-haiku-5-5",
                "Haiku",
                "Haiku 5.5 · Fastest for quick answers"
            )
        );
        assert_eq!(derive_pins(&[json!({"id": "claude-opus-5"})]).haiku, None);
    }

    #[test]
    fn no_fable_pin() {
        let pins = derive_pins(&[
            json!({"id": "claude-fable-5-1", "display_name": "Claude Fable 5.1", "created": 300}),
            json!({"id": "claude-sonnet-5", "created": 1}),
        ]);
        // `Pins` has no Fable slot; a Fable catalog never leaks into the other pins.
        let shown = format!("{pins:?}");
        assert!(!shown.to_lowercase().contains("fable"), "{shown}");
        assert_eq!(
            derive_pins(&[json!({"id": "claude-fable-5-1"})]),
            Pins::default()
        );
    }

    #[test]
    fn newest_by_created_then_numeric_id() {
        // bro: newestFable picks the newest claude-fable-* id from the catalog
        let models = [
            json!({"id": "claude-fable-5", "display_name": "Claude Fable 5", "created": 100}),
            json!({"id": "claude-opus-5", "display_name": "Claude Opus 5", "created": 300}),
            json!({"id": "claude-fable-5-1", "display_name": "Claude Fable 5.1", "created": 200}),
            json!({"id": "gpt-5.6-sol"}),
        ];
        assert_eq!(
            newest_fable(&models),
            Some(Newest {
                id: "claude-fable-5-1".into(),
                name: "Fable 5.1".into()
            })
        );
        // Equal `created` (here absent): numeric-aware id compare.
        let tie = [
            json!({"id": "claude-fable-5"}),
            json!({"id": "claude-fable-5-2"}),
        ];
        assert_eq!(newest_fable(&tie).unwrap().id, "claude-fable-5-2");
        let tie = [
            json!({"id": "claude-sonnet-5-9"}),
            json!({"id": "claude-sonnet-5-10"}),
            json!({"id": "claude-sonnet-5-2"}),
        ];
        assert_eq!(
            newest_of_family(&tie, "sonnet").unwrap().id,
            "claude-sonnet-5-10"
        );
        // `created` beats the id.
        let created_wins = [
            json!({"id": "claude-sonnet-9", "created": 1}),
            json!({"id": "claude-sonnet-5", "created": 2}),
        ];
        assert_eq!(
            newest_of_family(&created_wins, "sonnet").unwrap().id,
            "claude-sonnet-5"
        );
        assert_eq!(newest_fable(&[json!({"id": "claude-opus-5"})]), None);
        assert_eq!(newest_fable(&[]), None);
        // The family only matches followed by a digit; no display name falls back to the id.
        assert_eq!(
            newest_of_family(&[json!({"id": "claude-sonnet-x"})], "sonnet"),
            None
        );
        assert_eq!(
            newest_of_family(&[json!({"id": "claude-haiku-5"})], "haiku")
                .unwrap()
                .name,
            "claude-haiku-5"
        );
    }

    #[test]
    fn newest_strips_selector_suffix_from_ids() {
        // A table row listed as `<id>[1m]` must not yield `[1m][1m]` in the pin.
        let pins =
            derive_pins(&[json!({"id": "claude-sonnet-5[1m]", "display_name": "Claude Sonnet 5"})]);
        assert_eq!(pins.sonnet.unwrap().id, "claude-sonnet-5[1m]");
    }

    // bro: codexDefaultContextFromCatalog takes the safest valid unsuffixed default
    #[test]
    fn max_context_takes_safest_valid_unsuffixed_default() {
        let models = [
            json!({"id": "claude-sonnet-5", "owned_by": "anthropic-claude-max-pool", "context_window": 1000000}),
            json!({"id": "gpt-5.6-sol[1m]", "owned_by": OPENAI_OWNER, "context_window": 272000, "max_context_window": 872000}),
            json!({"id": "gpt-5.5", "owned_by": OPENAI_OWNER, "context_window": 272000, "max_context_window": 272000}),
            json!({"id": "broken", "owned_by": OPENAI_OWNER, "context_window": -1}),
            json!({"id": "zero", "owned_by": OPENAI_OWNER, "context_window": 0}),
        ];
        assert_eq!(max_context(&models), Some(272000));
        let claude_only = [
            json!({"id": "claude", "owned_by": "anthropic-claude-max-pool", "context_window": 200000}),
        ];
        assert_eq!(max_context(&claude_only), None);
        assert_eq!(max_context(&[]), None);
    }
}
