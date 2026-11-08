//! The live model table: bundled defaults overlaid by `models.toml`, plus family
//! mappings. A plain struct; the server wraps it in an `RwLock` for hot edits.

use std::collections::{HashMap, HashSet};
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Context;
use serde::Serialize;
use serde_json::{Map, Value};

use super::defaults::{default_mappings, default_routes, inferred_efforts, CODEX_EFFORTS};
use super::families::family_of;
use super::mapping::{
    drop_unsupported_efforts, merge_over, openai_target, sanitize_effort, validate, Mapping,
    MappingError,
};
use super::resolve::{find_row, resolve_in, selector_id};
use super::{Route, RouteDef};
use crate::config::Paths;
use crate::fsutil::atomic_write;
use crate::providers::ProviderId;

const FILE_NAME: &str = "models.toml";

#[derive(Debug, Clone, PartialEq)]
pub struct ModelTable {
    rows: Vec<RouteDef>,
    mapping_enabled: bool,
    mappings: Vec<Mapping>,
    /// Ids of rows added by runtime catalog discovery ([`Self::merge_discovered`]). Runtime-only:
    /// [`Self::save`] never writes them (the next discovery run re-adds them), and only these
    /// rows may have their metadata refreshed by a later discovery.
    discovered: HashSet<String>,
}

/// What [`ModelTable::merge_discovered`] changed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DiscoveredMerge {
    /// Ids newly added (as discovered rows).
    pub added: Vec<String>,
    /// Discovered rows whose context windows / efforts changed.
    pub updated: Vec<String>,
}

impl Default for ModelTable {
    fn default() -> Self {
        Self::defaults()
    }
}

impl ModelTable {
    /// Bundled routes and mappings, mapping off.
    pub fn defaults() -> Self {
        Self {
            rows: default_routes(),
            mapping_enabled: false,
            mappings: default_mappings(),
            discovered: HashSet::new(),
        }
    }

    /// `<home>/models.toml`.
    pub fn path(p: &Paths) -> PathBuf {
        p.home.join(FILE_NAME)
    }

    /// Parse `models.toml` text. Lenient like bro: unparseable text yields the defaults
    /// (with a warning), malformed rows/fields are skipped or sanitised.
    pub fn from_toml_str(s: &str) -> Self {
        match toml::from_str::<Value>(s) {
            Ok(v) => Self::from_value(Some(&v)),
            Err(e) => {
                tracing::warn!(error = %e, "models.toml failed to parse; using default model table");
                Self::defaults()
            }
        }
    }

    /// Import a bro `models.json` (`upstreamModel`, `mappingEnabled`, ... camelCase keys).
    pub fn from_bro_json(s: &str) -> Self {
        match serde_json::from_str::<Value>(s) {
            Ok(v) => Self::from_value(Some(&v)),
            Err(e) => {
                tracing::warn!(error = %e, "models.json failed to parse; using default model table");
                Self::defaults()
            }
        }
    }

    /// Load `<home>/models.toml`; a missing file means defaults. An unparseable file is
    /// quarantined (renamed to `models.toml.corrupt-<unix-ts>`, 0600, path logged) and
    /// defaults are returned, so the next `save` cannot silently destroy the user's bytes.
    /// If the quarantine rename itself fails that is an error.
    pub fn load(p: &Paths) -> anyhow::Result<Self> {
        let path = Self::path(p);
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Self::defaults()),
            Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
        };
        match toml::from_str::<Value>(&text) {
            Ok(v) => Ok(Self::from_value(Some(&v))),
            Err(e) => {
                let kept = quarantine(&path)?;
                tracing::warn!(
                    error = %e, quarantined = %kept.display(),
                    "models.toml failed to parse; moved aside and using default model table"
                );
                Ok(Self::defaults())
            }
        }
    }

    /// Atomically write `<home>/models.toml` (0600). Rows identical to a bundled default
    /// are not written, so default refreshes still reach existing installs, and neither are
    /// runtime-discovered rows ([`Self::merge_discovered`]); all mappings are written (persisted
    /// rows are authoritative, including opt-outs).
    pub fn save(&self, p: &Paths) -> anyhow::Result<()> {
        #[derive(Serialize)]
        struct Out<'a> {
            mapping_enabled: bool,
            models: Vec<&'a RouteDef>,
            mappings: &'a [Mapping],
        }
        let defaults = default_routes();
        let out = Out {
            mapping_enabled: self.mapping_enabled,
            models: self
                .rows
                .iter()
                .filter(|r| !defaults.contains(r) && !self.discovered.contains(&r.id))
                .collect(),
            mappings: &self.mappings,
        };
        let text = toml::to_string(&out).context("serializing models.toml")?;
        let path = Self::path(p);
        atomic_write(&path, text.as_bytes(), 0o600)
            .with_context(|| format!("writing {}", path.display()))
    }

    /// Exact id -> strip trailing `[1m]` (case-insensitive) -> Anthropic passthrough.
    pub fn resolve(&self, requested: &str) -> Route {
        resolve_in(&self.rows, requested)
    }

    /// The OpenAI route a Claude model id is mapped to, or `None` when mapping is off,
    /// the id has no family, the family's row is missing/inert, or the target is not an
    /// OpenAI route. The route's `def.id` is the caller's id (like bro); the effort
    /// overrides are on [`Self::mapping_for`].
    pub fn mapped_route(&self, model_id: &str) -> Option<Route> {
        let row = self.mapping_for(family_of(model_id)?)?;
        let target = find_row(&self.rows, &row.to)?;
        Some(Route {
            requested: model_id.to_string(),
            def: RouteDef {
                id: model_id.to_string(),
                ..target.clone()
            },
            effort: None,
        })
    }

    /// The *active* mapping row for `family`: mapping enabled, row present, not an
    /// opt-out (`to == from`), and the target resolves to an OpenAI route.
    pub fn mapping_for(&self, family: &str) -> Option<&Mapping> {
        if !self.mapping_enabled {
            return None;
        }
        let row = self.mappings.iter().find(|m| m.from == family)?;
        if row.to == row.from {
            return None;
        }
        openai_target(&self.rows, &row.to)?;
        Some(row)
    }

    pub fn mapping_enabled(&self) -> bool {
        self.mapping_enabled
    }

    /// Every mapping row, including inert/opt-out ones and while mapping is off.
    pub fn mappings(&self) -> &[Mapping] {
        &self.mappings
    }

    /// Apply an edit: the master switch and/or rows overlaid onto the **live** set
    /// (families not named keep their current row). Every row is validated first; on
    /// error nothing changes.
    pub fn apply_mappings(
        &mut self,
        enabled: Option<bool>,
        rows: Option<Vec<Mapping>>,
    ) -> Result<(), MappingError> {
        let rows = match rows {
            Some(rows) => {
                for m in &rows {
                    validate(&self.rows, m)?;
                }
                Some(rows)
            }
            None => None,
        };
        if let Some(rows) = rows {
            let rows = rows
                .into_iter()
                .map(|mut m| {
                    m.effort = m.effort.as_ref().and_then(sanitize_effort);
                    m
                })
                .collect();
            self.mappings = merge_over(std::mem::take(&mut self.mappings), rows);
        }
        if let Some(e) = enabled {
            self.mapping_enabled = e;
        }
        Ok(())
    }

    /// Mappings whose target is not a row and not a Claude id (an Anthropic opt-out), e.g. a Codex
    /// model not (yet) discovered. They fall back to passthrough until the target appears. Listed
    /// whether or not mapping is enabled; opt-outs (`to == from`) are never inert.
    pub fn inert_mappings(&self) -> Vec<&Mapping> {
        self.mappings
            .iter()
            .filter(|m| {
                m.to != m.from
                    && find_row(&self.rows, &m.to).is_none()
                    && family_of(&m.to).is_none()
            })
            .collect()
    }

    /// Add routes whose id is not yet present (never replaces); returns how many were added.
    /// Persisted by [`Self::save`] like user rows (test helper; discovery uses
    /// [`Self::merge_discovered`], whose rows are runtime-only).
    #[cfg(test)]
    pub(crate) fn add_discovered(&mut self, defs: Vec<RouteDef>) -> usize {
        let mut added = 0;
        for d in defs {
            if self.rows.iter().all(|r| r.id != d.id) {
                self.rows.push(d);
                added += 1;
            }
        }
        added
    }

    /// Merge a runtime catalog: an id not in the table is added and marked discovered (never
    /// saved); a row that was itself discovered gets its `context_window`, `max_context_window` and
    /// `supported_efforts` refreshed; bundled and user-configured rows are never touched. Nothing
    /// is removed. Duplicate ids in `defs` keep the first.
    pub fn merge_discovered(&mut self, defs: Vec<RouteDef>) -> DiscoveredMerge {
        let mut out = DiscoveredMerge::default();
        let mut seen: HashSet<String> = HashSet::new();
        for d in defs {
            if !seen.insert(d.id.clone()) {
                continue;
            }
            match self.rows.iter_mut().find(|r| r.id == d.id) {
                None => {
                    out.added.push(d.id.clone());
                    self.discovered.insert(d.id.clone());
                    self.rows.push(d);
                }
                Some(r) if self.discovered.contains(&r.id) => {
                    if (r.context_window, r.max_context_window, &r.supported_efforts)
                        != (d.context_window, d.max_context_window, &d.supported_efforts)
                    {
                        r.context_window = d.context_window;
                        r.max_context_window = d.max_context_window;
                        r.supported_efforts = d.supported_efforts;
                        out.updated.push(d.id);
                    }
                }
                Some(_) => {}
            }
        }
        out
    }

    /// Whether `id` is a runtime-discovered row (see [`Self::merge_discovered`]).
    pub fn is_discovered(&self, id: &str) -> bool {
        self.discovered.contains(id)
    }

    /// Client-facing id: `<id>[1m]` for OpenAI routes with `max_context_window >= 872000`.
    pub fn selector_id(def: &RouteDef) -> String {
        selector_id(def)
    }

    pub fn rows(&self) -> &[RouteDef] {
        &self.rows
    }

    /// Rows for `GET /v1/models`: the bundled full Claude ids (`claude-sonnet-5`, ...)
    /// are hidden so each family shows once as its alias; they still route. A user-added
    /// Claude id stays visible.
    pub fn rows_for_listing(&self) -> Vec<&RouteDef> {
        let hidden: HashSet<String> = default_routes()
            .into_iter()
            .filter(|r| r.provider == ProviderId::Anthropic && family_of(&r.id) != Some(&r.id))
            .map(|r| r.id)
            .collect();
        self.rows
            .iter()
            .filter(|r| !(r.provider == ProviderId::Anthropic && hidden.contains(&r.id)))
            .collect()
    }

    // ---- parsing ---------------------------------------------------------------------

    fn from_value(root: Option<&Value>) -> Self {
        let obj = root.and_then(Value::as_object);
        let rows = merge_rows(obj.and_then(|o| o.get("models")));
        let (mapping_enabled, file_mappings) = match obj {
            Some(o) => (
                parse_enabled(get(o, &["mapping_enabled", "mappingEnabled"])),
                parse_mappings(get(o, &["mappings"]), &rows),
            ),
            None => (false, Vec::new()),
        };
        Self {
            mappings: merge_over(default_mappings(), file_mappings),
            rows,
            mapping_enabled,
            discovered: HashSet::new(),
        }
    }
}

/// Rename a corrupt `path` to `<path>.corrupt-<unix-ts>` (never overwriting an earlier
/// quarantine) and tighten it to 0600.
fn quarantine(path: &Path) -> anyhow::Result<PathBuf> {
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let base = format!("{}.corrupt-{ts}", path.display());
    let mut dest = PathBuf::from(&base);
    let mut n = 1;
    while dest.exists() {
        dest = PathBuf::from(format!("{base}-{n}"));
        n += 1;
    }
    std::fs::rename(path, &dest)
        .with_context(|| format!("quarantining corrupt {}", path.display()))?;
    std::fs::set_permissions(&dest, std::fs::Permissions::from_mode(0o600))
        .with_context(|| format!("restricting {}", dest.display()))?;
    Ok(dest)
}

/// Recognised `(tier, value)` pairs of an effort map object; `None` when empty/not an object.
fn parse_effort_map(v: Option<&Value>) -> Option<HashMap<String, String>> {
    let raw: HashMap<String, String> = v?
        .as_object()?
        .iter()
        .filter_map(|(k, v)| Some((k.clone(), v.as_str()?.to_string())))
        .collect();
    sanitize_effort(&raw)
}

fn get<'a>(o: &'a Map<String, Value>, keys: &[&str]) -> Option<&'a Value> {
    keys.iter().find_map(|k| o.get(*k))
}

fn positive_u32(v: Option<&Value>) -> Option<u32> {
    v?.as_u64()
        .filter(|n| *n > 0)
        .and_then(|n| u32::try_from(n).ok())
}

/// Valid, de-duplicated (order kept) efforts; `None` when the value is not a list.
fn parse_efforts(v: Option<&Value>) -> Option<Vec<String>> {
    let list = v?.as_array()?;
    let mut out: Vec<String> = Vec::new();
    for e in list.iter().filter_map(Value::as_str) {
        if CODEX_EFFORTS.contains(&e) && !out.iter().any(|x| x == e) {
            out.push(e.to_string());
        }
    }
    Some(out)
}

struct FileRow {
    id: String,
    provider: ProviderId,
    upstream_model: String,
    context_window: Option<u32>,
    max_context_window: Option<u32>,
    supported_efforts: Option<Vec<String>>,
    effort_map: Option<HashMap<String, String>>,
}

fn parse_row(v: &Value) -> Option<FileRow> {
    let o = v.as_object()?;
    let id = o.get("id")?.as_str()?.to_string();
    let upstream_model = get(o, &["upstream_model", "upstreamModel"])?
        .as_str()?
        .to_string();
    let provider = ProviderId::from_str(o.get("provider")?.as_str()?).ok()?;
    let ctx = positive_u32(get(o, &["context_window", "contextWindow"]));
    let max = positive_u32(get(o, &["max_context_window", "maxContextWindow"]));
    // A window larger than its maximum is contradictory: trust neither.
    let contradictory = matches!((ctx, max), (Some(c), Some(m)) if c > m);
    Some(FileRow {
        id,
        provider,
        upstream_model,
        context_window: ctx.filter(|_| !contradictory),
        max_context_window: max.filter(|_| !contradictory),
        supported_efforts: parse_efforts(get(o, &["supported_efforts", "supportedEfforts"])),
        effort_map: parse_effort_map(get(o, &["effort_map", "effortMap"])),
    })
}

/// Defaults not shadowed by the file, then the file's rows (duplicate ids keep the
/// first). A file row with the same provider and upstream as the bundled row inherits
/// the bundled metadata it does not set itself; any other row gets only inferred efforts.
fn merge_rows(models: Option<&Value>) -> Vec<RouteDef> {
    let defaults = default_routes();
    let mut file: Vec<RouteDef> = Vec::new();
    for raw in models.and_then(Value::as_array).into_iter().flatten() {
        let Some(r) = parse_row(raw) else { continue };
        if file.iter().any(|f| f.id == r.id) {
            tracing::warn!(id = %r.id, "duplicate model id in models file; ignoring later entry");
            continue;
        }
        let bundled = defaults.iter().find(|d| {
            d.id == r.id && d.provider == r.provider && d.upstream_model == r.upstream_model
        });
        file.push(match bundled {
            Some(b) => RouteDef {
                context_window: r.context_window.or(b.context_window),
                max_context_window: r.max_context_window.or(b.max_context_window),
                supported_efforts: r
                    .supported_efforts
                    .unwrap_or_else(|| b.supported_efforts.clone()),
                effort_map: r.effort_map,
                id: r.id,
                provider: r.provider,
                upstream_model: r.upstream_model,
            },
            None => RouteDef {
                supported_efforts: r
                    .supported_efforts
                    .unwrap_or_else(|| inferred_efforts(r.provider, &r.upstream_model)),
                context_window: r.context_window,
                max_context_window: r.max_context_window,
                effort_map: r.effort_map,
                id: r.id,
                provider: r.provider,
                upstream_model: r.upstream_model,
            },
        });
    }
    let mut rows: Vec<RouteDef> = defaults
        .into_iter()
        .filter(|d| file.iter().all(|f| f.id != d.id))
        .collect();
    rows.extend(file);
    rows
}

fn parse_enabled(v: Option<&Value>) -> bool {
    match v {
        None => false,
        Some(Value::Bool(b)) => *b,
        Some(_) => {
            tracing::warn!(
                "models file: mapping_enabled is not a boolean; ignoring (mapping stays off)"
            );
            false
        }
    }
}

fn parse_mappings(v: Option<&Value>, rows: &[RouteDef]) -> Vec<Mapping> {
    let Some(v) = v else { return Vec::new() };
    let Some(list) = v.as_array() else {
        tracing::warn!("models file: mappings is not an array; ignoring (using defaults)");
        return Vec::new();
    };
    let mut out: Vec<Mapping> = Vec::new();
    for raw in list {
        let Some(o) = raw.as_object() else { continue };
        let (Some(from), Some(to)) = (
            o.get("from").and_then(Value::as_str),
            o.get("to").and_then(Value::as_str),
        ) else {
            continue;
        };
        if out.iter().any(|m| m.from == from) {
            continue;
        }
        let mut m = Mapping {
            from: from.to_string(),
            to: to.to_string(),
            effort: parse_effort_map(o.get("effort")),
        };
        drop_unsupported_efforts(rows, &mut m);
        out.push(m);
    }
    out
}
