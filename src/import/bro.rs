//! `pooltime import bro`: migrate a bro-cli install (spec §10, §16 "Import & shadow run",
//! INV §8) into the pooltime data dir.
//!
//! Two phases. [`plan_import`] only reads bro's tree (`~/.claude-max-pool`, `~/.bro`) and the
//! Claude Code profiles under `home`; the plan holds no tokens (credentials are re-read at apply
//! time) and is what `--dry-run` prints. [`apply_import`] writes into pooltime's data dir only.
//! bro's files and Keychain items are never modified; the cleanup commands are printed for the
//! user to run.
//!
//! On-disk mapping:
//! - `accounts/<name>/` → pooltime account `<name>` (bro's name rule equals [`AccountId`]'s;
//!   invalid names are reported and skipped). An existing pooltime account with the same id is
//!   never overwritten (reported; remove it first to re-import). Imported accounts get
//!   `imported = true`.
//! - `.credentials.json` `claudeAiOauth{accessToken, refreshToken, expiresAt ms,
//!   refreshTokenExpiresAt ms, scopes, subscriptionType, rateLimitTier}` → [`Credentials`]
//!   (`plan` = subscriptionType, `tier` = rateLimitTier). Without the file, the macOS Keychain
//!   item `Claude Code-credentials-<sha256(config dir)[:8]>` is read (see [`keychain_service`]).
//! - `openai-auth.json` `{accessToken, refreshToken, accountId, expiresAt ms, planType}` (its
//!   presence makes a Codex account, as in bro) → [`Credentials`] with provider `openai`,
//!   `openai_account_id` = accountId, `plan` = planType, written to the same 0600
//!   `credentials.json` as every other account.
//! - `routing.json` `{priority, weight}` → account priority/weight (invalid values fall back to
//!   the defaults, exactly like bro's `routingFileFor`).
//! - `usage.json` `usage.<name>`: `rateLimitStatus` (utilization is already a 0-1 fraction in
//!   bro, kept as is), a future `rateLimitedUntil`, a future `accessDeniedUntil` (+
//!   `billingBlockedAt`), and `deadRefreshToken`, which seeds `needs_login` only when it equals
//!   `sha256(current refresh token)[:16]`. Request counters are not imported.
//! - `sessions.json` `"<provider>:<metadata.user_id>"` → sticky sessions (stored hashed).
//! - `models.json` → `models.toml` (skipped if pooltime already has one).
//! - `tuning.json` `{fiveHourExp, minHeadroom, headroomTaperStart}` → `config.toml` `[routing]`.
//! - `~/.bro/pool-settings.json` ↔ `~/.claude`; `pool-settings.<stem>.<sha256(dir)[:8]>.json` ↔
//!   the discovered profile it names (bro `src/settings.js` `stateFileFor`). Unmatched files are
//!   orphans (reported, not applied); an existing pooltime snapshot is never overwritten.

use std::collections::{BTreeMap, HashSet};
use std::fmt::{self, Write as _};
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, bail, Context};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use time::OffsetDateTime;

use crate::accounts::registry::{DEFAULT_PRIORITY, DEFAULT_WEIGHT, MAX_WEIGHT, MIN_WEIGHT};
use crate::accounts::{
    fetch_profile, AccountId, AccountMeta, Credentials, RateLimitSnapshot, Registry, StateBook,
    Window, WindowStatus,
};
use crate::claude_code::snapshot::{self, Snapshot, SnapshotState};
use crate::claude_code::{discover, snapshot_path, Profile};
use crate::config::{AnthropicCfg, Config, Paths, RoutingCfg};
use crate::logging::hash_session;
use crate::models::ModelTable;
use crate::providers::ProviderId;
use crate::routing::sessions::Sessions;
use crate::store::Store;
use crate::upstream::Upstream;

/// Claude Code's Keychain service name for the default `~/.claude` dir; other config dirs append
/// `-<sha256(dir)[:8]>`.
pub const KEYCHAIN_SERVICE_PREFIX: &str = "Claude Code-credentials";
const CLAUDE_CREDS_FILE: &str = ".credentials.json";
const OPENAI_CREDS_FILE: &str = "openai-auth.json";
const DEFAULT_SNAPSHOT_FILE: &str = "pool-settings.json";
const PROFILE_PATH: &str = "/api/oauth/profile";

// ---- Keychain -----------------------------------------------------------------------------------

/// Read-only access to generic-password items, injectable for tests.
pub trait Keychain {
    /// The item's secret for `service`, `Ok(None)` when there is no such item.
    fn read(&self, service: &str) -> anyhow::Result<Option<String>>;
}

/// The login Keychain via `security find-generic-password -s <service> -w` (no `-a`, like bro and
/// Claude Code). May show a macOS access prompt. On other platforms there is no Keychain: always
/// `Ok(None)`.
pub struct SystemKeychain;

impl Keychain for SystemKeychain {
    #[cfg(target_os = "macos")]
    fn read(&self, service: &str) -> anyhow::Result<Option<String>> {
        use std::process::{Command, Stdio};
        let out = Command::new("/usr/bin/security")
            .args(["find-generic-password", "-s", service, "-w"])
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
            .context("running /usr/bin/security")?;
        match out.status.code() {
            Some(0) => {
                let s = String::from_utf8(out.stdout)
                    .map_err(|_| anyhow!("the Keychain item is not UTF-8"))?;
                Ok(Some(s.trim().to_string()))
            }
            // errSecItemNotFound
            Some(44) => Ok(None),
            code => bail!("security exited with status {code:?}"),
        }
    }

    #[cfg(not(target_os = "macos"))]
    fn read(&self, _service: &str) -> anyhow::Result<Option<String>> {
        Ok(None)
    }
}

/// `Claude Code-credentials-<sha256(dir)[:8]>`, where `dir` is the account's config dir string as
/// bro computes it: `join(poolDir, "accounts", name)` (bro `pool/src/accounts/manager.ts`
/// `configDirFor`, hashed by `keychainServiceForConfigDir` in `pool/src/accounts/keychain.ts`).
/// Node's `join` normalises lexically and does not resolve symlinks; so does this.
pub fn keychain_service(config_dir: &Path) -> String {
    let dir = node_resolve(config_dir);
    format!(
        "{KEYCHAIN_SERVICE_PREFIX}-{}",
        sha_hex(&dir.to_string_lossy(), 8)
    )
}

// ---- plan ---------------------------------------------------------------------------------------

/// Where an account's credentials come from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CredsSource {
    ClaudeFile(PathBuf),
    Keychain { service: String },
    OpenAiFile(PathBuf),
}

impl CredsSource {
    fn describe(&self) -> String {
        match self {
            CredsSource::ClaudeFile(_) => CLAUDE_CREDS_FILE.into(),
            CredsSource::OpenAiFile(_) => OPENAI_CREDS_FILE.into(),
            CredsSource::Keychain { service } => format!("Keychain \"{service}\""),
        }
    }
}

/// The parts of a bro `usage.json` record pooltime keeps.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct BroUsage {
    pub snapshot: Option<RateLimitSnapshot>,
    pub rate_limited_until: Option<OffsetDateTime>,
    pub access_denied_until: Option<OffsetDateTime>,
    pub billing_blocked_at: Option<OffsetDateTime>,
    /// `sha256(rejected refresh token)[:16]` hex.
    pub dead_refresh_fp: Option<String>,
}

#[derive(Debug, Clone)]
pub struct PlannedAccount {
    pub name: String,
    pub id: AccountId,
    pub provider: ProviderId,
    pub source: CredsSource,
    pub priority: u32,
    pub weight: f64,
    pub usage: Option<BroUsage>,
}

/// A bro sticky session. `Debug` shows only the hashed user id.
#[derive(Clone)]
pub struct PlannedSession {
    pub provider: ProviderId,
    /// The full `metadata.user_id` (hashed when stored).
    pub user_id: String,
    pub account: String,
    pub last_seen: OffsetDateTime,
}

impl fmt::Debug for PlannedSession {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PlannedSession")
            .field("provider", &self.provider)
            .field("user_id_hash", &hash_session(&self.user_id))
            .field("account", &self.account)
            .field("last_seen", &self.last_seen)
            .finish()
    }
}

/// A bro `pool-settings*.json` matched to a profile. `prior` is `None` when the file is
/// unreadable. `Debug` shows only the prior's keys (values may be the user's own token).
#[derive(Clone)]
pub struct PlannedSnapshot {
    pub source: PathBuf,
    pub profile: Profile,
    pub prior: Option<Map<String, Value>>,
}

impl fmt::Debug for PlannedSnapshot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PlannedSnapshot")
            .field("source", &self.source)
            .field("profile", &self.profile.id)
            .field(
                "prior_keys",
                &self.prior.as_ref().map(|p| p.keys().collect::<Vec<_>>()),
            )
            .finish()
    }
}

/// Valid fields of bro's `tuning.json` (bro's own bounds; invalid fields are dropped like bro).
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct TuningImport {
    pub five_hour_exp: Option<f64>,
    pub min_headroom: Option<f64>,
    pub headroom_taper_start: Option<f64>,
}

impl TuningImport {
    fn is_empty(&self) -> bool {
        *self == TuningImport::default()
    }
}

#[derive(Debug, Clone)]
pub struct ImportPlan {
    pub bro_pool: PathBuf,
    pub bro_home: PathBuf,
    /// Sorted by name.
    pub accounts: Vec<PlannedAccount>,
    /// `(dir name, reason)` for account dirs that cannot be imported.
    pub invalid: Vec<(String, String)>,
    /// Sessions whose account is in [`ImportPlan::accounts`].
    pub sessions: Vec<PlannedSession>,
    /// Sessions pinned to accounts outside the import (or with an unknown provider).
    pub sessions_skipped: usize,
    pub models_json: Option<String>,
    pub tuning: Option<TuningImport>,
    pub snapshots: Vec<PlannedSnapshot>,
    /// bro snapshots that match no Claude Code profile under `home`.
    pub orphans: Vec<PathBuf>,
    /// Non-fatal problems reading bro's files.
    pub notes: Vec<String>,
}

/// Read bro's state under `bro_pool` (`~/.claude-max-pool`) and `bro_home` (`~/.bro`), matching
/// bro's settings snapshots against the Claude Code profiles under `home`. Reads only; never
/// consults the Keychain.
pub fn plan_import(bro_pool: &Path, bro_home: &Path, home: &Path) -> anyhow::Result<ImportPlan> {
    if !bro_pool.is_dir() {
        bail!("no bro pool directory at {}", bro_pool.display());
    }
    let mut notes = Vec::new();
    let usage = read_json(&bro_pool.join("usage.json"), &mut notes);
    let usage = usage
        .as_ref()
        .and_then(|u| u.get("usage"))
        .and_then(Value::as_object);

    let mut accounts = Vec::new();
    let mut invalid = Vec::new();
    let accounts_dir = bro_pool.join("accounts");
    for name in subdirs(&accounts_dir) {
        let id = match name.parse::<AccountId>() {
            Ok(id) => id,
            Err(_) => {
                invalid.push((
                    name,
                    "invalid account name (must match [A-Za-z0-9._-]{1,64})".into(),
                ));
                continue;
            }
        };
        let dir = accounts_dir.join(&name);
        let (provider, source) = if dir.join(OPENAI_CREDS_FILE).is_file() {
            (
                ProviderId::OpenAi,
                CredsSource::OpenAiFile(dir.join(OPENAI_CREDS_FILE)),
            )
        } else if dir.join(CLAUDE_CREDS_FILE).is_file() {
            (
                ProviderId::Anthropic,
                CredsSource::ClaudeFile(dir.join(CLAUDE_CREDS_FILE)),
            )
        } else {
            (
                ProviderId::Anthropic,
                CredsSource::Keychain {
                    service: keychain_service(&dir),
                },
            )
        };
        let (priority, weight) = routing(&dir.join("routing.json"));
        accounts.push(PlannedAccount {
            usage: usage.and_then(|u| u.get(&name)).map(parse_usage),
            name,
            id,
            provider,
            source,
            priority,
            weight,
        });
    }

    let names: HashSet<&str> = accounts.iter().map(|a| a.name.as_str()).collect();
    let (sessions, sessions_skipped) =
        parse_sessions(&bro_pool.join("sessions.json"), &names, &mut notes);

    let models_json = fs::read_to_string(bro_pool.join("models.json")).ok();
    let tuning = read_json(&bro_pool.join("tuning.json"), &mut notes).map(|v| parse_tuning(&v));
    let (snapshots, orphans) = plan_snapshots(bro_home, home);

    Ok(ImportPlan {
        bro_pool: bro_pool.to_path_buf(),
        bro_home: bro_home.to_path_buf(),
        accounts,
        invalid,
        sessions,
        sessions_skipped,
        models_json,
        tuning,
        snapshots,
        orphans,
        notes,
    })
}

/// Sorted names of the directories in `dir` (missing dir = none).
fn subdirs(dir: &Path) -> Vec<String> {
    let Ok(rd) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut names: Vec<String> = rd
        .flatten()
        .filter(|e| e.path().is_dir())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

/// Parsed JSON, or `None` when missing; unparseable files add a note (never their content).
fn read_json(path: &Path, notes: &mut Vec<String>) -> Option<Value> {
    let bytes = fs::read(path).ok()?;
    match serde_json::from_slice(&bytes) {
        Ok(v) => Some(v),
        Err(_) => {
            notes.push(format!("{} is not valid JSON; ignored", path.display()));
            None
        }
    }
}

/// bro `routingFileFor`: priority is a non-negative integer, weight within 0.1..=10.
fn routing(path: &Path) -> (u32, f64) {
    let v: Value = fs::read(path)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or(Value::Null);
    let priority = v
        .get("priority")
        .and_then(Value::as_u64)
        .and_then(|p| u32::try_from(p).ok())
        .unwrap_or(DEFAULT_PRIORITY);
    let weight = v
        .get("weight")
        .and_then(Value::as_f64)
        .filter(|w| (MIN_WEIGHT..=MAX_WEIGHT).contains(w))
        .unwrap_or(DEFAULT_WEIGHT);
    (priority, weight)
}

/// Epoch milliseconds -> time.
fn ms(v: Option<&Value>) -> Option<OffsetDateTime> {
    let n = v?.as_f64().filter(|n| n.is_finite())?;
    OffsetDateTime::from_unix_timestamp_nanos(n as i128 * 1_000_000).ok()
}

fn window_status(s: Option<&str>) -> WindowStatus {
    match s {
        Some("allowed") => WindowStatus::Allowed,
        Some("allowed_warning") => WindowStatus::AllowedWarning,
        Some("rejected") => WindowStatus::Rejected,
        _ => WindowStatus::Unknown,
    }
}

/// One bro window. bro already stores utilization as a 0-1 fraction (header values are
/// fractions; `/api/oauth/usage` and Codex percentages are divided by 100 before storing), so it
/// is kept verbatim. A window without a utilization is kept only when rejected (as fully used).
fn window(
    key: &str,
    model: Option<&str>,
    status: Option<&str>,
    util: Option<&Value>,
    reset: Option<&Value>,
) -> Option<Window> {
    let status = window_status(status);
    let utilization = match util.and_then(Value::as_f64) {
        Some(u) if u.is_finite() && u >= 0.0 => u,
        _ if status == WindowStatus::Rejected => 1.0,
        _ => return None,
    };
    Some(Window {
        reset_synthetic: false,
        key: key.to_string(),
        model: model.map(str::to_string),
        status,
        utilization,
        reset: ms(reset),
    })
}

/// bro `normalizeRateLimitSnapshot`, including the legacy `fiveHour*`/`sevenDay*` fields.
fn parse_snapshot(v: &Value) -> Option<RateLimitSnapshot> {
    fn str_of(v: Option<&Value>) -> Option<&str> {
        v.and_then(Value::as_str)
    }
    let o = v.as_object()?;
    let windows: Vec<Window> = match o.get("windows").and_then(Value::as_array) {
        Some(ws) => ws
            .iter()
            .filter_map(|w| {
                window(
                    str_of(w.get("key"))?,
                    str_of(w.get("model")),
                    str_of(w.get("status")),
                    w.get("utilization"),
                    w.get("reset"),
                )
            })
            .collect(),
        None => [("5h", "fiveHour"), ("7d", "sevenDay")]
            .into_iter()
            .filter_map(|(key, p)| {
                window(
                    key,
                    None,
                    str_of(o.get(&format!("{p}Status"))),
                    o.get(&format!("{p}Utilization")),
                    o.get(&format!("{p}Reset")),
                )
            })
            .collect(),
    };
    let unified_status = str_of(o.get("unifiedStatus")).map(str::to_string);
    if windows.is_empty() && unified_status.is_none() {
        return None;
    }
    Some(RateLimitSnapshot {
        unified_status,
        windows,
        updated_at: ms(o.get("updatedAt")).unwrap_or_else(OffsetDateTime::now_utc),
    })
}

fn parse_usage(v: &Value) -> BroUsage {
    BroUsage {
        snapshot: v.get("rateLimitStatus").and_then(parse_snapshot),
        rate_limited_until: ms(v.get("rateLimitedUntil")),
        access_denied_until: ms(v.get("accessDeniedUntil")),
        billing_blocked_at: ms(v.get("billingBlockedAt")),
        dead_refresh_fp: v
            .get("deadRefreshToken")
            .and_then(Value::as_str)
            .map(str::to_string),
    }
}

/// bro `SessionLedger`: `{sessions: {"<provider>:<sessionKey>": {account, provider, lastSeenAt}}}`.
fn parse_sessions(
    path: &Path,
    accounts: &HashSet<&str>,
    notes: &mut Vec<String>,
) -> (Vec<PlannedSession>, usize) {
    let Some(v) = read_json(path, notes) else {
        return (Vec::new(), 0);
    };
    let Some(entries) = v.get("sessions").and_then(Value::as_object) else {
        return (Vec::new(), 0);
    };
    let mut out = Vec::new();
    let mut skipped = 0;
    for (key, e) in entries {
        let parsed = (|| {
            let (provider, user_id) = key.split_once(':')?;
            let provider: ProviderId = provider.parse().ok()?;
            let account = e.get("account")?.as_str()?;
            let last_seen = ms(e.get("lastSeenAt"))?;
            accounts.contains(account).then(|| PlannedSession {
                provider,
                user_id: user_id.to_string(),
                account: account.to_string(),
                last_seen,
            })
        })();
        match parsed {
            Some(s) => out.push(s),
            None => skipped += 1,
        }
    }
    (out, skipped)
}

/// bro `TUNING_BOUNDS` / `isValidTuningField`.
fn parse_tuning(v: &Value) -> TuningImport {
    let field = |k: &str, ok: fn(f64) -> bool| {
        v.get(k)
            .and_then(Value::as_f64)
            .filter(|n| n.is_finite() && ok(*n))
    };
    TuningImport {
        five_hour_exp: field("fiveHourExp", |n| (0.0..=5.0).contains(&n)),
        min_headroom: field("minHeadroom", |n| (0.0..=1.0).contains(&n)),
        headroom_taper_start: field("headroomTaperStart", |n| n > 0.0 && n <= 1.0),
    }
}

/// The bro snapshot file name for a non-default profile dir (bro `src/settings.js`
/// `stateFileFor`): stem = basename of `path.resolve(dir)` without leading dots, runs of
/// characters outside `[a-zA-Z0-9._-]` replaced by `-`, at most 40 chars; then
/// `sha256(resolved)[:8]`.
pub fn bro_snapshot_name(dir: &Path) -> String {
    let resolved = node_resolve(dir);
    let base = resolved
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let mut stem = String::new();
    let mut in_run = false;
    for c in base.trim_start_matches('.').chars() {
        if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
            stem.push(c);
            in_run = false;
        } else if !in_run {
            stem.push('-');
            in_run = true;
        }
    }
    stem.truncate(40); // ASCII only by now
    format!(
        "pool-settings.{stem}.{}.json",
        sha_hex(&resolved.to_string_lossy(), 8)
    )
}

fn plan_snapshots(bro_home: &Path, home: &Path) -> (Vec<PlannedSnapshot>, Vec<PathBuf>) {
    let Ok(rd) = fs::read_dir(bro_home) else {
        return (Vec::new(), Vec::new());
    };
    let mut files: Vec<String> = rd
        .flatten()
        .filter(|e| e.path().is_file())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| {
            n == DEFAULT_SNAPSHOT_FILE || (n.starts_with("pool-settings.") && n.ends_with(".json"))
        })
        .collect();
    files.sort();
    let default_dir = home.join(".claude");
    let named: BTreeMap<String, Profile> = discover(home)
        .into_iter()
        .filter(|p| node_resolve(&p.settings_dir) != node_resolve(&default_dir))
        .map(|p| (bro_snapshot_name(&p.settings_dir), p))
        .collect();
    let mut snapshots = Vec::new();
    let mut orphans = Vec::new();
    for name in files {
        let source = bro_home.join(&name);
        let profile = if name == DEFAULT_SNAPSHOT_FILE {
            Some(Profile::from_dir(&default_dir, home))
        } else {
            named.get(&name).cloned()
        };
        match profile {
            Some(profile) => {
                let prior = fs::read(&source)
                    .ok()
                    .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
                    .and_then(|v| match v.get("prior") {
                        Some(Value::Object(m)) => Some(m.clone()),
                        _ => None,
                    });
                snapshots.push(PlannedSnapshot {
                    source,
                    profile,
                    prior,
                });
            }
            None => orphans.push(source),
        }
    }
    (snapshots, orphans)
}

// ---- apply --------------------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdentityState {
    /// Refreshable Anthropic account whose identity has not been fetched (yet).
    Pending,
    Resolved,
    /// Codex accounts, accounts without a refresh token, and dead accounts.
    NotApplicable,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AccountOutcome {
    Imported {
        needs_login: bool,
        identity: IdentityState,
    },
    /// A pooltime account with this id already exists; it was left untouched.
    AlreadyExists,
    /// Nothing usable to import (invalid name, no credentials, ...). bro keeps the only copy.
    Skipped { reason: String },
    /// Writing the account into pooltime failed; nothing of it was kept. bro keeps the only copy.
    Failed { error: String },
}

impl AccountOutcome {
    /// bro still holds the only copy of this account (so bro's files must not be deleted).
    fn kept_in_bro(&self) -> bool {
        matches!(
            self,
            AccountOutcome::Skipped { .. } | AccountOutcome::Failed { .. }
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountReport {
    /// bro's account dir name (the pooltime id when imported).
    pub name: String,
    pub provider: Option<ProviderId>,
    /// e.g. `.credentials.json`, `Keychain "Claude Code-credentials-1a2b3c4d"`.
    pub source: Option<String>,
    pub outcome: AccountOutcome,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum ItemOutcome {
    /// bro had nothing to import.
    #[default]
    Absent,
    Imported,
    Skipped(String),
    /// Writing it into pooltime failed (the rest of the import went on).
    Failed(String),
}

#[derive(Debug, Clone, Default)]
pub struct ImportReport {
    /// Sorted by name.
    pub accounts: Vec<AccountReport>,
    pub sessions_imported: usize,
    pub sessions_skipped: usize,
    pub models: ItemOutcome,
    pub tuning: ItemOutcome,
    /// `(profile id, outcome)`.
    pub snapshots: Vec<(String, ItemOutcome)>,
    pub orphans: Vec<PathBuf>,
    /// Commands for the user to run once pooltime serves the accounts (never run by pooltime).
    pub cleanup: Vec<String>,
    pub notes: Vec<String>,
}

impl ImportReport {
    pub fn account(&self, name: &str) -> Option<&AccountReport> {
        self.accounts.iter().find(|a| a.name == name)
    }
}

/// Write `plan` into pooltime's data dir. Reads credentials (files, or `keychain` for accounts
/// without a `.credentials.json`); never writes outside `paths`. Per-item problems, including
/// failed writes, are recorded in the [`ImportReport`] and the import goes on; only failing to
/// open pooltime's data dir, registry or database (before anything is written) is an error.
/// Identities are left pending for [`fill_identities`].
pub fn apply_import(
    plan: &ImportPlan,
    paths: &Paths,
    keychain: &dyn Keychain,
) -> anyhow::Result<ImportReport> {
    paths.ensure().context("creating the pooltime data dir")?;
    let reg = Registry::load(paths)?;
    let store = Arc::new(Store::open(&paths.db)?);
    let book = StateBook::load(store.clone())?;
    let now = OffsetDateTime::now_utc();
    let mut report = ImportReport {
        notes: plan.notes.clone(),
        ..ImportReport::default()
    };

    for (name, reason) in &plan.invalid {
        report.accounts.push(AccountReport {
            name: name.clone(),
            provider: None,
            source: None,
            outcome: AccountOutcome::Skipped {
                reason: reason.clone(),
            },
        });
    }
    let mut imported = HashSet::new();
    for a in &plan.accounts {
        let outcome = import_account(a, paths, &reg, &book, &store, keychain, now);
        if matches!(outcome, AccountOutcome::Imported { .. }) {
            imported.insert(a.name.as_str());
        }
        report.accounts.push(AccountReport {
            name: a.name.clone(),
            provider: Some(a.provider),
            source: Some(a.source.describe()),
            outcome,
        });
    }
    report.accounts.sort_by(|a, b| a.name.cmp(&b.name));

    // The idle window only matters for reads; `set` stores bro's lastSeenAt as is.
    let sessions = Sessions::load(store.clone(), Duration::from_secs(30 * 60));
    report.sessions_skipped = plan.sessions_skipped;
    for s in &plan.sessions {
        match s.account.parse::<AccountId>() {
            Ok(id) if imported.contains(s.account.as_str()) => {
                sessions.set(s.provider, &s.user_id, &id, s.last_seen);
                report.sessions_imported += 1;
            }
            _ => report.sessions_skipped += 1,
        }
    }
    if let Err(e) = flush(&store) {
        report
            .notes
            .push(format!("sticky sessions may not have been saved: {e:#}"));
    }

    let failed = |e: anyhow::Error| ItemOutcome::Failed(format!("{e:#}"));
    report.models = match &plan.models_json {
        None => ItemOutcome::Absent,
        Some(_) if ModelTable::path(paths).exists() => {
            ItemOutcome::Skipped("pooltime already has a models.toml".into())
        }
        Some(text) => ModelTable::from_bro_json(text)
            .save(paths)
            .map_or_else(failed, |()| ItemOutcome::Imported),
    };

    report.tuning = match plan.tuning {
        None => ItemOutcome::Absent,
        Some(t) if t.is_empty() => ItemOutcome::Skipped("tuning.json has no valid fields".into()),
        Some(t) => apply_tuning(paths, t).unwrap_or_else(failed),
    };

    for s in &plan.snapshots {
        let outcome = apply_snapshot(paths, s).unwrap_or_else(failed);
        report.snapshots.push((s.profile.id.clone(), outcome));
    }
    report.orphans = plan.orphans.clone();
    report.cleanup = cleanup_commands(plan, &report);
    Ok(report)
}

/// Whether pooltime already has account `id` (its `account.toml` or `credentials.json`).
fn exists_in_pooltime(paths: &Paths, id: &AccountId) -> bool {
    let dir = paths.accounts.join(id.as_str());
    dir.join("account.toml").exists() || dir.join("credentials.json").exists()
}

fn import_account(
    a: &PlannedAccount,
    paths: &Paths,
    reg: &Registry,
    book: &StateBook,
    store: &Store,
    keychain: &dyn Keychain,
    now: OffsetDateTime,
) -> AccountOutcome {
    if exists_in_pooltime(paths, &a.id) {
        return AccountOutcome::AlreadyExists;
    }
    let creds = match &a.source {
        CredsSource::ClaudeFile(p) => match fs::read(p) {
            Ok(b) => claude_creds(&b),
            Err(e) => Err(format!("cannot read {CLAUDE_CREDS_FILE}: {}", e.kind())),
        },
        CredsSource::OpenAiFile(p) => match fs::read(p) {
            Ok(b) => openai_creds(&b),
            Err(e) => Err(format!("cannot read {OPENAI_CREDS_FILE}: {}", e.kind())),
        },
        CredsSource::Keychain { service } => match keychain.read(service) {
            Ok(Some(secret)) => claude_creds(secret.as_bytes()),
            Ok(None) => Err(format!(
                "no {CLAUDE_CREDS_FILE} and no Keychain item \"{service}\""
            )),
            Err(e) => Err(format!("reading Keychain item \"{service}\" failed: {e:#}")),
        },
    };
    let creds = match creds {
        Ok(c) => c,
        Err(reason) => return AccountOutcome::Skipped { reason },
    };

    // Markers first and on disk before `account.toml` appears, so an account that is visible
    // to pooltime always carries its dead/cooldown markers.
    let needs_login = seed_state(a, &creds, book, now);
    let dir = paths.accounts.join(a.id.as_str());
    let existed = fs::symlink_metadata(&dir).is_ok();
    let written = flush(store)
        .and_then(|()| reg.write_creds(&a.id, &creds))
        .and_then(|()| {
            let mut meta = AccountMeta::new(a.id.clone(), a.provider);
            meta.priority = a.priority;
            meta.weight = a.weight;
            meta.imported = true;
            reg.upsert(meta)
        });
    if let Err(e) = written {
        book.forget(&a.id);
        if !existed {
            let _ = fs::remove_dir_all(&dir);
        }
        return AccountOutcome::Failed {
            error: format!("{e:#}"),
        };
    }

    let identity =
        if a.provider == ProviderId::Anthropic && creds.refresh_token.is_some() && !needs_login {
            IdentityState::Pending
        } else {
            IdentityState::NotApplicable
        };
    AccountOutcome::Imported {
        needs_login,
        identity,
    }
}

/// Seed bro's usage snapshot, cooldowns and dead-token marker; returns whether the account
/// needs a login (bro's dead fingerprint matches the current refresh token).
fn seed_state(
    a: &PlannedAccount,
    creds: &Credentials,
    book: &StateBook,
    now: OffsetDateTime,
) -> bool {
    let Some(u) = &a.usage else {
        return false;
    };
    if let Some(snap) = u.snapshot.clone() {
        book.merge_header_snapshot(&a.id, snap);
    }
    if let Some(until) = u.rate_limited_until.filter(|t| *t > now) {
        book.mark_rate_limited(&a.id, until, None, now);
    }
    if u.access_denied_until.is_some_and(|t| t > now) {
        // pooltime re-derives the probe cooldown from its own config.
        if u.billing_blocked_at.is_some() {
            book.mark_billing(&a.id, now);
        } else {
            book.mark_access_denied(&a.id, now);
        }
    }
    match (&u.dead_refresh_fp, &creds.refresh_token) {
        (Some(fp), Some(rt)) if *fp == sha_hex(rt, 16) => {
            book.mark_refresh_dead(&a.id, rt, now);
            true
        }
        _ => false,
    }
}

/// Errors never echo file content (it holds tokens).
fn json_object(bytes: &[u8], what: &str) -> Result<Map<String, Value>, String> {
    match serde_json::from_slice::<Value>(bytes) {
        Ok(Value::Object(m)) => Ok(m),
        _ => Err(format!("{what} is not a JSON object")),
    }
}

fn nonempty(v: Option<&Value>) -> Option<String> {
    v.and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .map(str::to_string)
}

/// Claude Code's credentials blob (file or Keychain item): `{claudeAiOauth: {...}}`.
fn claude_creds(bytes: &[u8]) -> Result<Credentials, String> {
    let root = json_object(bytes, "the Claude credentials")?;
    let Some(Value::Object(o)) = root.get("claudeAiOauth") else {
        return Err("the Claude credentials have no claudeAiOauth block".into());
    };
    let access_token =
        nonempty(o.get("accessToken")).ok_or("the Claude credentials have no access token")?;
    Ok(Credentials {
        provider: ProviderId::Anthropic,
        access_token,
        refresh_token: nonempty(o.get("refreshToken")),
        expires_at: ms(o.get("expiresAt")),
        refresh_expires_at: ms(o.get("refreshTokenExpiresAt")),
        scopes: o
            .get("scopes")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default(),
        plan: nonempty(o.get("subscriptionType")),
        tier: nonempty(o.get("rateLimitTier")),
        openai_account_id: None,
    })
}

/// bro `OpenAIOauthCreds` (`openai-auth.json`); every field has a [`Credentials`] slot.
fn openai_creds(bytes: &[u8]) -> Result<Credentials, String> {
    let o = json_object(bytes, OPENAI_CREDS_FILE)?;
    let access_token =
        nonempty(o.get("accessToken")).ok_or("openai-auth.json has no access token")?;
    Ok(Credentials {
        provider: ProviderId::OpenAi,
        access_token,
        refresh_token: nonempty(o.get("refreshToken")),
        expires_at: ms(o.get("expiresAt")),
        refresh_expires_at: None,
        scopes: Vec::new(),
        plan: nonempty(o.get("planType")),
        tier: None,
        openai_account_id: nonempty(o.get("accountId")),
    })
}

/// Sets the tuning fields bro had overridden in `config.toml` (other settings untouched).
fn apply_tuning(paths: &Paths, t: TuningImport) -> anyhow::Result<ItemOutcome> {
    let Ok(mut cfg) = Config::load_file(paths) else {
        // The parse error could quote config.toml (which may hold the proxy key): no details.
        return Ok(ItemOutcome::Skipped(
            "pooltime's config.toml is unreadable or invalid".into(),
        ));
    };
    // Values the user already customised in pooltime win: skip rather than overwrite them.
    let (cur, def) = (&cfg.routing, RoutingCfg::default());
    let conflicts: Vec<&str> = [
        (
            "five_hour_exp",
            t.five_hour_exp,
            cur.five_hour_exp,
            def.five_hour_exp,
        ),
        (
            "min_headroom",
            t.min_headroom,
            cur.min_headroom,
            def.min_headroom,
        ),
        (
            "headroom_taper_start",
            t.headroom_taper_start,
            cur.headroom_taper_start,
            def.headroom_taper_start,
        ),
    ]
    .into_iter()
    .filter(|(_, new, cur, def)| new.is_some_and(|n| cur != def && *cur != n))
    .map(|(k, ..)| k)
    .collect();
    if !conflicts.is_empty() {
        return Ok(ItemOutcome::Skipped(format!(
            "pooltime's config.toml already sets routing {} (left as is)",
            conflicts.join(", ")
        )));
    }
    let r = &mut cfg.routing;
    if let Some(v) = t.five_hour_exp {
        r.five_hour_exp = v;
    }
    if let Some(v) = t.min_headroom {
        r.min_headroom = v;
    }
    if let Some(v) = t.headroom_taper_start {
        r.headroom_taper_start = v;
    }
    cfg.save(paths)?;
    Ok(ItemOutcome::Imported)
}

fn apply_snapshot(paths: &Paths, s: &PlannedSnapshot) -> anyhow::Result<ItemOutcome> {
    let Some(prior) = &s.prior else {
        return Ok(ItemOutcome::Skipped(format!(
            "{} has no readable prior values",
            s.source.display()
        )));
    };
    let dest = snapshot_path(paths, &s.profile);
    if !matches!(snapshot::read(&dest), SnapshotState::Absent) {
        return Ok(ItemOutcome::Skipped(
            "pooltime already has a snapshot for this profile".into(),
        ));
    }
    snapshot::write(&dest, &Snapshot::new(prior.clone()))
        .with_context(|| format!("writing {}", dest.display()))?;
    Ok(ItemOutcome::Imported)
}

/// Wait until every write queued on the store so far is on disk (no async runtime needed).
fn flush(store: &Store) -> anyhow::Result<()> {
    let (tx, rx) = std::sync::mpsc::channel();
    store.submit(move |_| {
        let _ = tx.send(());
        Ok(())
    })?;
    rx.recv().map_err(|_| anyhow!("the store writer stopped"))
}

/// What the user may delete once pooltime serves the accounts. bro's pool dir is only offered
/// for deletion as a whole when bro holds nothing that was not imported (accounts already in
/// pooltime count as imported); otherwise only the imported accounts' dirs are listed. Likewise
/// for snapshot files and Keychain items.
fn cleanup_commands(plan: &ImportPlan, report: &ImportReport) -> Vec<String> {
    let mut out = vec!["bro pool down".to_string()];
    let kept: Vec<&str> = report
        .accounts
        .iter()
        .filter(|a| a.outcome.kept_in_bro())
        .map(|a| a.name.as_str())
        .collect();
    let imported: Vec<&PlannedAccount> = plan
        .accounts
        .iter()
        .filter(|a| {
            report
                .account(&a.name)
                .is_some_and(|r| matches!(r.outcome, AccountOutcome::Imported { .. }))
        })
        .collect();
    if kept.is_empty() {
        out.push(format!(
            "rm -rf {}",
            shell_quote(&plan.bro_pool.to_string_lossy())
        ));
    } else {
        for a in &imported {
            let dir = plan.bro_pool.join("accounts").join(&a.name);
            out.push(format!("rm -rf {}", shell_quote(&dir.to_string_lossy())));
        }
        out.push(format!("# kept in bro (not imported): {}", kept.join(", ")));
    }

    let all_snapshots = plan.orphans.is_empty()
        && report
            .snapshots
            .iter()
            .all(|(_, o)| *o == ItemOutcome::Imported);
    if all_snapshots && !plan.snapshots.is_empty() {
        out.push(format!(
            "rm -f {}pool-settings*.json",
            shell_quote(&format!("{}/", plan.bro_home.display()))
        ));
    } else {
        for (s, (_, o)) in plan.snapshots.iter().zip(&report.snapshots) {
            if *o == ItemOutcome::Imported {
                out.push(format!(
                    "rm -f {}",
                    shell_quote(&s.source.to_string_lossy())
                ));
            }
        }
    }

    out.extend(imported.iter().filter_map(|a| match &a.source {
        CredsSource::Keychain { service } => Some(format!(
            "security delete-generic-password -s {}",
            shell_quote(service)
        )),
        _ => None,
    }));
    out
}

// ---- identity -----------------------------------------------------------------------------------

/// Fetch `/api/oauth/profile` for every imported account whose identity is
/// [`IdentityState::Pending`] and store it in `account.toml`. The base URL comes from
/// `cfg.profile_url` (minus its `/api/oauth/profile` path), else `cfg.api_base`. A failed fetch
/// leaves the account imported with its identity pending.
pub async fn fill_identities(
    paths: &Paths,
    report: &mut ImportReport,
    up: &Upstream,
    cfg: &AnthropicCfg,
) {
    let reg = match Registry::load(paths) {
        Ok(r) => r,
        Err(e) => {
            report.notes.push(format!(
                "could not open pooltime's accounts ({e:#}); identities not fetched"
            ));
            return;
        }
    };
    let base = cfg
        .profile_url
        .trim_end_matches('/')
        .strip_suffix(PROFILE_PATH)
        .map_or_else(|| cfg.api_base.clone(), str::to_string);
    let mut notes = Vec::new();
    for a in &mut report.accounts {
        let AccountOutcome::Imported { identity, .. } = &mut a.outcome else {
            continue;
        };
        if *identity != IdentityState::Pending {
            continue;
        }
        let Ok(id) = a.name.parse::<AccountId>() else {
            continue;
        };
        let creds = match reg.read_creds(&id) {
            Ok(Some(c)) => c,
            Ok(None) => {
                notes.push(format!("{id}: credentials vanished; identity not fetched"));
                continue;
            }
            // CredsError never echoes the file's content.
            Err(e) => {
                notes.push(format!("{id}: {e}; identity not fetched"));
                continue;
            }
        };
        match fetch_profile(up, &base, &creds.access_token, &cfg.user_agent).await {
            Ok(p) if p.identity.account_uuid.is_some() || p.identity.email.is_some() => {
                if let Some(mut meta) = reg.get(&id) {
                    meta.identity = p.identity;
                    match reg.upsert(meta) {
                        Ok(()) => *identity = IdentityState::Resolved,
                        Err(e) => notes.push(format!("{id}: could not save its identity: {e:#}")),
                    }
                }
            }
            Ok(_) => notes.push(format!("{id}: the profile reported no identity")),
            // Errors carry only the HTTP status / transport kind, never a body or token.
            Err(e) => notes.push(format!("{id}: identity fetch failed: {e:#}")),
        }
    }
    report.notes.extend(notes);
}

// ---- rendering ----------------------------------------------------------------------------------

const BRO_WARNING: &str = "Stop bro before pooltime serves these accounts (`bro pool down`): both \
would refresh the same tokens and one copy goes stale. While a bro pool answers on its port, \
`pooltime serve` refuses imported accounts unless started with --i-stopped-bro.";

fn tuning_text(t: &TuningImport) -> String {
    let fields: Vec<String> = [
        ("five_hour_exp", t.five_hour_exp),
        ("min_headroom", t.min_headroom),
        ("headroom_taper_start", t.headroom_taper_start),
    ]
    .into_iter()
    .filter_map(|(k, v)| v.map(|v| format!("{k}={v}")))
    .collect();
    if fields.is_empty() {
        "no valid fields".into()
    } else {
        fields.join(", ")
    }
}

impl ImportPlan {
    /// Human-readable plan (`--dry-run`). Shows names, sources and flags only, never tokens,
    /// session ids or snapshot values. `paths` is only probed for existing accounts/snapshots.
    pub fn render(&self, paths: &Paths) -> String {
        let mut s = String::new();
        let _ = writeln!(s, "Import plan from bro");
        let _ = writeln!(s, "  bro pool: {}", self.bro_pool.display());
        let _ = writeln!(s, "  bro home: {}", self.bro_home.display());
        let _ = writeln!(s, "  into:     {}", paths.home.display());
        let _ = writeln!(s, "\nAccounts:");
        if self.accounts.is_empty() && self.invalid.is_empty() {
            let _ = writeln!(s, "  (none)");
        }
        for a in &self.accounts {
            let mut flags = Vec::new();
            if let Some(u) = &a.usage {
                if let Some(snap) = &u.snapshot {
                    flags.push(format!("usage windows: {}", snap.windows.len()));
                }
                if u.dead_refresh_fp.is_some() {
                    flags.push("dead-token marker (checked against the current token)".into());
                }
                if u.rate_limited_until.is_some() || u.access_denied_until.is_some() {
                    flags.push("cooldown markers".into());
                }
            }
            if matches!(a.source, CredsSource::Keychain { .. }) {
                flags.push("Keychain read at import (may prompt)".into());
            }
            if exists_in_pooltime(paths, &a.id) {
                flags.push("SKIP: pooltime account already exists".into());
            }
            let _ = writeln!(
                s,
                "  {:<16} {:<9} creds: {}; priority {}, weight {}{}",
                a.name,
                a.provider.as_str(),
                a.source.describe(),
                a.priority,
                a.weight,
                if flags.is_empty() {
                    String::new()
                } else {
                    format!("; {}", flags.join("; "))
                }
            );
        }
        for (name, reason) in &self.invalid {
            let _ = writeln!(s, "  {name:<16} SKIP: {reason}");
        }
        let _ = writeln!(
            s,
            "\nSticky sessions: {} to import, {} skipped (other accounts)",
            self.sessions.len(),
            self.sessions_skipped
        );
        let models = match &self.models_json {
            None => "none".to_string(),
            Some(_) if ModelTable::path(paths).exists() => {
                "SKIP: pooltime already has a models.toml".into()
            }
            Some(_) => "models.json -> models.toml".into(),
        };
        let _ = writeln!(s, "Models: {models}");
        let tuning = self
            .tuning
            .as_ref()
            .map_or_else(|| "none".to_string(), tuning_text);
        let _ = writeln!(s, "Tuning: {tuning}");
        let _ = writeln!(s, "\nClaude Code settings snapshots:");
        if self.snapshots.is_empty() {
            let _ = writeln!(s, "  (none)");
        }
        for snap in &self.snapshots {
            let exists = !matches!(
                snapshot::read(&snapshot_path(paths, &snap.profile)),
                SnapshotState::Absent
            );
            let _ = writeln!(
                s,
                "  {} -> profile {} ({}){}",
                file_name(&snap.source),
                snap.profile.id,
                snap.profile.settings_dir.display(),
                if snap.prior.is_none() {
                    "; SKIP: unreadable"
                } else if exists {
                    "; SKIP: pooltime already has a snapshot"
                } else {
                    ""
                }
            );
        }
        render_orphans(&mut s, &self.orphans);
        render_notes(&mut s, &self.notes);
        let _ = writeln!(s, "\n{BRO_WARNING}");
        s
    }
}

impl ImportReport {
    /// Human-readable result, with the cleanup commands and the bro warning. Never shows
    /// tokens, session ids or snapshot values.
    pub fn render(&self) -> String {
        let mut s = String::new();
        let _ = writeln!(s, "Accounts:");
        for a in &self.accounts {
            let provider = a.provider.map_or("-", ProviderId::as_str);
            let line = match &a.outcome {
                AccountOutcome::Imported {
                    needs_login,
                    identity,
                } => {
                    let mut parts = vec![format!(
                        "imported from {}",
                        a.source.as_deref().unwrap_or("?")
                    )];
                    if *needs_login {
                        parts.push("needs login (bro marked its refresh token dead)".into());
                    }
                    match identity {
                        // Nothing fills a missing identity later (refresh does not fetch it).
                        IdentityState::Pending => parts.push(format!(
                            "identity pending (run `pooltime accounts login {}` to verify it, or it stays pending)",
                            a.name
                        )),
                        IdentityState::Resolved => parts.push("identity verified".into()),
                        IdentityState::NotApplicable => {}
                    }
                    parts.join("; ")
                }
                AccountOutcome::AlreadyExists => "skipped: a pooltime account with this id \
                     already exists (left untouched; remove it to re-import)"
                    .to_string(),
                AccountOutcome::Skipped { reason } => format!("skipped (kept in bro): {reason}"),
                AccountOutcome::Failed { error } => {
                    format!("FAILED (nothing kept; bro's copy is untouched): {error}")
                }
            };
            let _ = writeln!(s, "  {:<16} {provider:<9} {line}", a.name);
        }
        let _ = writeln!(
            s,
            "Sticky sessions: {} imported, {} skipped",
            self.sessions_imported, self.sessions_skipped
        );
        let item = |o: &ItemOutcome| match o {
            ItemOutcome::Absent => "nothing to import".to_string(),
            ItemOutcome::Imported => "imported".into(),
            ItemOutcome::Skipped(r) => format!("skipped: {r}"),
            ItemOutcome::Failed(e) => format!("FAILED: {e}"),
        };
        let _ = writeln!(s, "Models: {}", item(&self.models));
        let _ = writeln!(s, "Tuning: {}", item(&self.tuning));
        if !self.snapshots.is_empty() {
            let _ = writeln!(s, "Claude Code settings snapshots:");
            for (profile, o) in &self.snapshots {
                let _ = writeln!(s, "  {profile:<16} {}", item(o));
            }
        }
        render_orphans(&mut s, &self.orphans);
        render_notes(&mut s, &self.notes);
        let _ = writeln!(s, "\n{BRO_WARNING}");
        let _ = writeln!(
            s,
            "\nbro's files and Keychain items were not changed. Once pooltime serves these \
             accounts, clean up bro with:"
        );
        for c in &self.cleanup {
            let _ = writeln!(s, "  {c}");
        }
        s
    }
}

fn render_orphans(s: &mut String, orphans: &[PathBuf]) {
    if orphans.is_empty() {
        return;
    }
    let _ = writeln!(
        s,
        "Orphaned bro snapshots (no matching Claude Code profile; not applied):"
    );
    for o in orphans {
        let _ = writeln!(s, "  {}", file_name(o));
    }
}

fn render_notes(s: &mut String, notes: &[String]) {
    for n in notes {
        let _ = writeln!(s, "Note: {n}");
    }
}

// ---- helpers ------------------------------------------------------------------------------------

fn file_name(p: &Path) -> String {
    p.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// First `n` hex chars of sha256(`s`).
fn sha_hex(s: &str, n: usize) -> String {
    let mut h = hex::encode(Sha256::digest(s.as_bytes()));
    h.truncate(n);
    h
}

/// Node's `path.resolve` for one path: absolute (against the cwd), `.`/`..` folded lexically,
/// symlinks left alone.
fn node_resolve(p: &Path) -> PathBuf {
    let abs = if p.is_absolute() {
        p.to_path_buf()
    } else {
        std::env::current_dir().unwrap_or_default().join(p)
    };
    let mut out = PathBuf::new();
    for c in abs.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// POSIX single-quoting.
fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keychain_service_hashes_the_normalised_dir() {
        // bro's documented spike: <home>/.claude-max-pool/accounts/primary -> 28f0e4e1 for the
        // user's home; here only the rule (sha256 of the joined path string) is checked.
        let svc = keychain_service(Path::new(
            "/Users/x/.claude-max-pool/./accounts/a/../primary",
        ));
        assert_eq!(
            svc,
            format!(
                "Claude Code-credentials-{}",
                sha_hex("/Users/x/.claude-max-pool/accounts/primary", 8)
            )
        );
    }

    #[test]
    fn snapshot_name_matches_bro_state_file_for() {
        let name = bro_snapshot_name(Path::new("/h/.claude-personal"));
        assert_eq!(
            name,
            format!(
                "pool-settings.claude-personal.{}.json",
                sha_hex("/h/.claude-personal", 8)
            )
        );
        let odd = bro_snapshot_name(Path::new("/h/..my dir!!x"));
        assert!(odd.starts_with("pool-settings.my-dir-x."), "{odd}");
        let long = bro_snapshot_name(Path::new(&format!("/h/.{}", "a".repeat(60))));
        assert!(long.starts_with(&format!("pool-settings.{}.", "a".repeat(40))));
    }

    #[test]
    fn shell_quote_escapes_single_quotes() {
        assert_eq!(shell_quote("a b"), "'a b'");
        assert_eq!(shell_quote("it's"), r"'it'\''s'");
    }

    #[test]
    fn creds_errors_never_echo_content() {
        let err =
            claude_creds(br#"{"claudeAiOauth": {"accessToken": 5, "x": "SECRET"}}"#).unwrap_err();
        assert!(!err.contains("SECRET"));
        let err = openai_creds(b"{\"accessToken\": \"SECRET\"").unwrap_err();
        assert!(!err.contains("SECRET"));
    }

    #[test]
    fn plan_debug_hides_session_ids_and_snapshot_values() {
        let s = PlannedSession {
            provider: ProviderId::Anthropic,
            user_id: "user_SECRET".into(),
            account: "a".into(),
            last_seen: OffsetDateTime::UNIX_EPOCH,
        };
        assert!(!format!("{s:?}").contains("SECRET"));
        let mut prior = Map::new();
        prior.insert(
            "ANTHROPIC_AUTH_TOKEN".into(),
            Value::String("SECRET".into()),
        );
        let p = PlannedSnapshot {
            source: PathBuf::from("/x"),
            profile: Profile::from_dir(Path::new("/h/.claude"), Path::new("/h")),
            prior: Some(prior),
        };
        let d = format!("{p:?}");
        assert!(
            !d.contains("SECRET") && d.contains("ANTHROPIC_AUTH_TOKEN"),
            "{d}"
        );
    }
}
