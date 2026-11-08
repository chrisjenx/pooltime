//! Recognising values an earlier bro or pooltime wrote into a settings `env`
//! block (port of bro `settings.js` `scrubLegacyPins` / `isBroPinValue` /
//! `scrubManagedContext`), so they are never mistaken for the user's own.

use serde_json::{Map, Value};

pub(crate) const CONTEXT_KEY: &str = "CLAUDE_CODE_MAX_CONTEXT_TOKENS";
pub(crate) const MARKER_KEY: &str = "POOLTIME_MANAGED_MAX_CONTEXT_TOKENS";
/// bro's marker: never written by pooltime, but cleaned up like our own.
pub(crate) const BRO_MARKER_KEY: &str = "BRO_POOL_MANAGED_MAX_CONTEXT_TOKENS";
pub(crate) const AUTO_COMPACT_KEY: &str = "CLAUDE_CODE_AUTO_COMPACT_WINDOW";

/// `ANTHROPIC_DEFAULT_{SONNET,OPUS,FABLE,HAIKU}_MODEL{,_NAME,_DESCRIPTION}`.
pub(crate) const PIN_KEYS: &[&str] = &[
    "ANTHROPIC_DEFAULT_SONNET_MODEL",
    "ANTHROPIC_DEFAULT_SONNET_MODEL_NAME",
    "ANTHROPIC_DEFAULT_SONNET_MODEL_DESCRIPTION",
    "ANTHROPIC_DEFAULT_OPUS_MODEL",
    "ANTHROPIC_DEFAULT_OPUS_MODEL_NAME",
    "ANTHROPIC_DEFAULT_OPUS_MODEL_DESCRIPTION",
    "ANTHROPIC_DEFAULT_FABLE_MODEL",
    "ANTHROPIC_DEFAULT_FABLE_MODEL_NAME",
    "ANTHROPIC_DEFAULT_FABLE_MODEL_DESCRIPTION",
    "ANTHROPIC_DEFAULT_HAIKU_MODEL",
    "ANTHROPIC_DEFAULT_HAIKU_MODEL_NAME",
    "ANTHROPIC_DEFAULT_HAIKU_MODEL_DESCRIPTION",
];

const HAIKU_DESCRIPTION_KEY: &str = "ANTHROPIC_DEFAULT_HAIKU_MODEL_DESCRIPTION";

/// Every literal value an earlier bro wrote for a pin key. Closed, historical
/// list: pins written today are recognised by shape in [`is_pin_value`].
const LEGACY_PIN_VALUES: &[&str] = &[
    "claude-sonnet-5[1m]",
    "claude-opus-5[1m]",
    "claude-fable-5-1[1m]",
    "Sonnet",
    "Opus",
    "Fable",
    "Sonnet 5 with 1M context · Efficient for routine tasks",
    "Opus 5 with 1M context · Best for everyday, complex tasks",
    "Fable 5.1 · Most capable for your hardest and longest-running tasks",
];

/// `claude-<family>-<digit><[\w.-]*>[1m]` (bro's `^claude-sonnet-\d[\w.-]*\[1m\]$`).
fn is_family_1m_id(value: &str, family: &str) -> bool {
    let Some(rest) = value
        .strip_prefix("claude-")
        .and_then(|r| r.strip_prefix(family))
        .and_then(|r| r.strip_prefix('-'))
    else {
        return false;
    };
    let Some(body) = rest.strip_suffix("[1m]") else {
        return false;
    };
    body.chars().next().is_some_and(|c| c.is_ascii_digit())
        && body
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'))
}

/// bro's `^Haiku [\d.]+ · Fastest for quick answers$`.
fn is_haiku_description(value: Option<&Value>) -> bool {
    let Some(s) = value.and_then(Value::as_str) else {
        return false;
    };
    let Some(version) = s
        .strip_prefix("Haiku ")
        .and_then(|r| r.strip_suffix(" · Fastest for quick answers"))
    else {
        return false;
    };
    !version.is_empty() && version.chars().all(|c| c.is_ascii_digit() || c == '.')
}

/// Whether `value` of pin key `k` is one we (or an earlier bro) wrote: a
/// historical literal, or the shape a catalog-derived pin has for any model
/// version. `env` is the block the value sits in, for keys only recognisable by
/// a sibling (Haiku id/name alone are indistinguishable from a user pinning
/// `claude-haiku-4-5` themselves; our description beside them is not). Keys
/// that are not pin keys never match.
pub(crate) fn is_pin_value(k: &str, value: &Value, env: &Map<String, Value>) -> bool {
    if !PIN_KEYS.contains(&k) {
        return false;
    }
    let Some(s) = value.as_str() else {
        return false;
    };
    if LEGACY_PIN_VALUES.contains(&s) {
        return true;
    }
    match k {
        "ANTHROPIC_DEFAULT_FABLE_MODEL" => is_family_1m_id(s, "fable"),
        "ANTHROPIC_DEFAULT_FABLE_MODEL_DESCRIPTION" => {
            s.ends_with(" · Most capable for your hardest and longest-running tasks")
        }
        "ANTHROPIC_DEFAULT_SONNET_MODEL" => is_family_1m_id(s, "sonnet"),
        "ANTHROPIC_DEFAULT_SONNET_MODEL_DESCRIPTION" => {
            s.ends_with(" with 1M context · Efficient for routine tasks")
        }
        HAIKU_DESCRIPTION_KEY => is_haiku_description(Some(value)),
        "ANTHROPIC_DEFAULT_HAIKU_MODEL" | "ANTHROPIC_DEFAULT_HAIKU_MODEL_NAME" => {
            is_haiku_description(env.get(HAIKU_DESCRIPTION_KEY))
        }
        _ => false,
    }
}

/// Pin keys in `env` holding a recognisable pooltime/bro value. Decided against
/// the untouched block, since Haiku keys depend on their sibling.
pub(crate) fn own_pin_keys(env: &Map<String, Value>) -> Vec<&'static str> {
    PIN_KEYS
        .iter()
        .copied()
        .filter(|k| env.get(*k).is_some_and(|v| is_pin_value(k, v, env)))
        .collect()
}

/// Delete pin values an earlier bro/pooltime wrote; user values stay.
pub fn scrub_legacy_pins(env: &mut Map<String, Value>) {
    for k in own_pin_keys(env) {
        env.remove(k);
    }
}

/// Whether a context marker in `env` proves `CLAUDE_CODE_MAX_CONTEXT_TOKENS` is
/// the value we (or bro) wrote rather than the user's own.
pub(crate) fn context_is_managed(env: &Map<String, Value>) -> bool {
    let Some(ctx) = env.get(CONTEXT_KEY) else {
        return false;
    };
    [MARKER_KEY, BRO_MARKER_KEY]
        .iter()
        .any(|m| env.get(*m) == Some(ctx))
}

/// A marker carrying the exact written value lets inherited managed context be
/// discarded without clobbering a user's own export. Handles both pooltime's and
/// bro's marker; markers are always removed.
pub fn scrub_managed_context(env: &mut Map<String, Value>) {
    if context_is_managed(env) {
        env.remove(CONTEXT_KEY);
    }
    env.remove(MARKER_KEY);
    env.remove(BRO_MARKER_KEY);
}
