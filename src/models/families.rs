//! Claude model families, matched as substrings of a model id or display name.

/// Families we route and rate-limit on, in match order (first hit wins, so `fable`
/// beats `opus` for an id mentioning both).
pub const MODEL_FAMILIES: [&str; 5] = ["fable", "mythos", "opus", "sonnet", "haiku"];

/// Family of `model` (case-insensitive substring, order `fable, mythos, opus, sonnet,
/// haiku`), or `None` for non-Claude ids such as `gpt-6-sol`.
pub fn family_of(model: &str) -> Option<&'static str> {
    let lower = model.to_ascii_lowercase();
    MODEL_FAMILIES.into_iter().find(|f| lower.contains(f))
}
