//! Field-by-field comparison of two observations, and the allowlist of intended differences.
//!
//! An observation is a flat `field -> value` map (see `main.rs` for the field names). Every field
//! present on either side is compared; a missing field reads as `<absent>`. A differing field is
//! *allowed* when an allowlist entry matches the scenario, the field and both values (all regexes,
//! anchored). Every entry must say why and cite where the difference comes from (an INV item,
//! spec §16 or a ledger ruling), so an allowlist entry can never be a silent pass.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use anyhow::{bail, Context};
use regex::Regex;
use serde::Deserialize;

pub type Observation = BTreeMap<String, String>;

pub const ABSENT: &str = "<absent>";

/// Fields with this prefix are informational (e.g. step timings) and never compared.
pub const INFO: &str = "info.";

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AllowEntry {
    /// Regex over the scenario name.
    pub scenario: String,
    /// Regex over the field name.
    pub field: String,
    /// Regex bro's value must match.
    pub bro: String,
    /// Regex pooltime's value must match.
    pub pooltime: String,
    pub reason: String,
    /// `INV §<n>...`, `spec §16 ...`, `docs/parity.md ...` or `ruling: ...`.
    pub source: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct AllowFile {
    #[serde(default)]
    allow: Vec<AllowEntry>,
}

pub struct Allowlist {
    pub entries: Vec<AllowEntry>,
    compiled: Vec<[Regex; 4]>,
}

fn anchored(p: &str) -> anyhow::Result<Regex> {
    Regex::new(&format!("^(?:{p})$")).with_context(|| format!("bad allowlist regex {p:?}"))
}

/// Accepted citation prefixes for `source`.
const SOURCES: &[&str] = &["INV §", "spec §16", "docs/parity.md", "ruling:"];

impl Allowlist {
    pub fn parse(text: &str) -> anyhow::Result<Allowlist> {
        let f: AllowFile = toml::from_str(text).context("parsing the allowlist")?;
        let mut compiled = Vec::new();
        for (i, e) in f.allow.iter().enumerate() {
            if e.reason.trim().is_empty() {
                bail!("allowlist entry {i} has no reason");
            }
            if !SOURCES.iter().any(|s| e.source.starts_with(s)) {
                bail!(
                    "allowlist entry {i} source {:?} must start with one of {SOURCES:?}",
                    e.source
                );
            }
            compiled.push([
                anchored(&e.scenario)?,
                anchored(&e.field)?,
                anchored(&e.bro)?,
                anchored(&e.pooltime)?,
            ]);
        }
        Ok(Allowlist {
            entries: f.allow,
            compiled,
        })
    }

    pub fn load(path: &Path) -> anyhow::Result<Allowlist> {
        let text =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        Self::parse(&text)
    }

    /// Index of the first entry that allows this difference.
    pub fn find(&self, scenario: &str, field: &str, bro: &str, pooltime: &str) -> Option<usize> {
        self.compiled.iter().position(|[s, f, b, p]| {
            s.is_match(scenario) && f.is_match(field) && b.is_match(bro) && p.is_match(pooltime)
        })
    }
}

#[derive(Debug, Clone)]
pub struct Diff {
    pub field: String,
    pub bro: String,
    pub pooltime: String,
    /// Allowlist entry that covers it.
    pub allowed: Option<usize>,
}

/// Every differing field, in field order.
pub fn diff(scenario: &str, bro: &Observation, pt: &Observation, allow: &Allowlist) -> Vec<Diff> {
    let fields: BTreeSet<&String> = bro
        .keys()
        .chain(pt.keys())
        .filter(|k| !k.starts_with(INFO))
        .collect();
    fields
        .into_iter()
        .filter_map(|f| {
            let b = bro.get(f).map_or(ABSENT, String::as_str);
            let p = pt.get(f).map_or(ABSENT, String::as_str);
            (b != p).then(|| Diff {
                field: f.clone(),
                bro: b.to_string(),
                pooltime: p.to_string(),
                allowed: allow.find(scenario, f, b, p),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn obs(pairs: &[(&str, &str)]) -> Observation {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    const LIST: &str = r#"
[[allow]]
scenario = ".*"
field = 'step\d+\.header\.x-pool-account'
bro = ".*"
pooltime = ".*"
reason = "account header differs by design"
source = "spec §16 allowlist"

[[allow]]
scenario = "codex_failed_after_created"
field = 'step0\.status'
bro = "200"
pooltime = "5\\d\\d"
reason = "pooltime settles a pre-output failure instead of streaming 200+error"
source = "docs/parity.md Decisions"
"#;

    #[test]
    fn equal_fields_are_not_diffs_and_absent_is_explicit() {
        let a = Allowlist::parse(LIST).unwrap();
        let d = diff(
            "s",
            &obs(&[
                ("step0.status", "200"),
                ("step0.header.x-pool-account", "alpha"),
            ]),
            &obs(&[("step0.status", "200")]),
            &a,
        );
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].pooltime, ABSENT);
        assert_eq!(d[0].allowed, Some(0));
    }

    #[test]
    fn allow_needs_scenario_field_and_both_values_to_match() {
        let a = Allowlist::parse(LIST).unwrap();
        let bro = obs(&[("step0.status", "200")]);
        let pt = obs(&[("step0.status", "503")]);
        assert_eq!(
            diff("codex_failed_after_created", &bro, &pt, &a)[0].allowed,
            Some(1)
        );
        assert_eq!(diff("other", &bro, &pt, &a)[0].allowed, None);
        // pooltime's value must match its pattern too (anchored: 429 is not `5\d\d`).
        let pt429 = obs(&[("step0.status", "429")]);
        assert_eq!(
            diff("codex_failed_after_created", &bro, &pt429, &a)[0].allowed,
            None
        );
    }

    #[test]
    fn entries_without_reason_or_citation_are_rejected() {
        let no_source = LIST.replace("source = \"spec §16 allowlist\"", "source = \"because\"");
        assert!(Allowlist::parse(&no_source).is_err());
        let no_reason = LIST.replace("account header differs by design", " ");
        assert!(Allowlist::parse(&no_reason).is_err());
    }

    #[test]
    fn shipped_allowlist_is_valid() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("allowlist.toml");
        Allowlist::load(&path).unwrap();
    }
}
