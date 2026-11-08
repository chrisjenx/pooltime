//! On-disk account registry: `accounts/<id>/{account.toml,credentials.json,lock}`.
//!
//! Every call reads from disk (no cache); files are replaced atomically so
//! concurrent readers never see partial content. Account dirs are 0700, files
//! 0600.

use std::fs::{self, DirBuilder};
use std::io;
use std::os::unix::fs::DirBuilderExt;
use std::path::PathBuf;

use anyhow::{bail, Context};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use super::creds::{Credentials, CredsError};
use super::id::AccountId;
use crate::config::Paths;
use crate::fsutil::{self, FileLock};
use crate::providers::ProviderId;

pub const DEFAULT_PRIORITY: u32 = 100;
pub const DEFAULT_WEIGHT: f64 = 1.0;
pub const MIN_WEIGHT: f64 = 0.1;
pub const MAX_WEIGHT: f64 = 10.0;

/// Who the upstream says this account is (filled in by login/profile fetch).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Identity {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub account_uuid: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub org_uuid: Option<String>,
}

/// A weight that applies until `until`, then reverts to the account's own weight.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TimedOverride {
    pub weight: f64,
    #[serde(with = "time::serde::rfc3339")]
    pub until: OffsetDateTime,
}

/// `account.toml`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AccountMeta {
    pub id: AccountId,
    pub provider: ProviderId,
    #[serde(default = "default_priority")]
    pub priority: u32,
    #[serde(default = "default_weight")]
    pub weight: f64,
    #[serde(default)]
    pub identity: Identity,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub weight_override: Option<TimedOverride>,
}

fn default_priority() -> u32 {
    DEFAULT_PRIORITY
}

fn default_weight() -> f64 {
    DEFAULT_WEIGHT
}

impl AccountMeta {
    /// Defaults: priority 100, weight 1, empty identity, no note/override.
    pub fn new(id: AccountId, provider: ProviderId) -> AccountMeta {
        AccountMeta {
            id,
            provider,
            priority: DEFAULT_PRIORITY,
            weight: DEFAULT_WEIGHT,
            identity: Identity::default(),
            note: None,
            weight_override: None,
        }
    }

    /// Weight (and override weight) must be finite and within 0.1..=10.
    pub fn validate(&self) -> anyhow::Result<()> {
        check_weight("weight", self.weight)?;
        if let Some(o) = &self.weight_override {
            check_weight("weight_override.weight", o.weight)?;
        }
        Ok(())
    }
}

fn check_weight(what: &str, w: f64) -> anyhow::Result<()> {
    if !(MIN_WEIGHT..=MAX_WEIGHT).contains(&w) {
        bail!("{what} must be within {MIN_WEIGHT}..={MAX_WEIGHT}, got {w}");
    }
    Ok(())
}

#[derive(Debug, Clone)]
pub struct Registry {
    accounts: PathBuf,
}

impl Registry {
    /// Opens the registry, creating the accounts dir (0700) if missing.
    pub fn load(p: &Paths) -> anyhow::Result<Registry> {
        DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&p.accounts)
            .with_context(|| format!("creating {}", p.accounts.display()))?;
        Ok(Registry {
            accounts: p.accounts.clone(),
        })
    }

    fn dir(&self, id: &AccountId) -> PathBuf {
        self.accounts.join(id.as_str())
    }

    fn ensure_dir(&self, id: &AccountId) -> io::Result<PathBuf> {
        let d = self.dir(id);
        DirBuilder::new().recursive(true).mode(0o700).create(&d)?;
        fsutil::ensure_mode(&d, 0o700)?;
        Ok(d)
    }

    fn read_meta(&self, id: &AccountId) -> anyhow::Result<AccountMeta> {
        let path = self.dir(id).join("account.toml");
        let text =
            fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
        let m: AccountMeta =
            toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
        if &m.id != id {
            bail!(
                "{} declares id {:?}, expected {id}",
                path.display(),
                m.id.as_str()
            );
        }
        m.validate()
            .with_context(|| format!("invalid {}", path.display()))?;
        Ok(m)
    }

    /// All valid accounts sorted by id. Directories without a usable
    /// `account.toml` are skipped with a warning; the list never fails.
    pub fn list(&self) -> Vec<AccountMeta> {
        let Ok(rd) = fs::read_dir(&self.accounts) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for entry in rd.flatten() {
            if !entry.path().is_dir() {
                continue;
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            let id = match name.parse::<AccountId>() {
                Ok(id) => id,
                Err(e) => {
                    tracing::warn!(dir = ?name, error = %e, "skipping account dir with invalid id");
                    continue;
                }
            };
            match self.read_meta(&id) {
                Ok(m) => out.push(m),
                Err(e) => {
                    tracing::warn!(account = %id, error = %format!("{e:#}"), "skipping account dir without valid account.toml");
                }
            }
        }
        out.sort_by(|a, b| a.id.cmp(&b.id));
        out
    }

    pub fn get(&self, id: &AccountId) -> Option<AccountMeta> {
        match self.read_meta(id) {
            Ok(m) => Some(m),
            Err(e) => {
                if self.dir(id).exists() {
                    tracing::warn!(account = %id, error = %format!("{e:#}"), "account.toml unusable");
                }
                None
            }
        }
    }

    /// Validates and atomically writes `account.toml` (0600), creating the
    /// account dir (0700) if needed.
    pub fn upsert(&self, m: AccountMeta) -> anyhow::Result<()> {
        m.validate()?;
        let dir = self.ensure_dir(&m.id)?;
        let _guard = FileLock::acquire(&dir.join("lock"))?;
        let body = toml::to_string(&m).context("serializing account.toml")?;
        fsutil::atomic_write(&dir.join("account.toml"), body.as_bytes(), 0o600)?;
        Ok(())
    }

    /// Deletes the account directory (meta, credentials, lock). Missing is Ok.
    pub fn remove(&self, id: &AccountId) -> anyhow::Result<()> {
        let dir = self.dir(id);
        if !dir.exists() {
            return Ok(());
        }
        let _guard = FileLock::acquire(&dir.join("lock"))?;
        fs::remove_dir_all(&dir).with_context(|| format!("removing {}", dir.display()))?;
        Ok(())
    }

    /// `Ok(None)` only when `credentials.json` does not exist. Anything else
    /// unusable is [`CredsError::Corrupt`] (never reported as "logged out").
    pub fn read_creds(&self, id: &AccountId) -> Result<Option<Credentials>, CredsError> {
        let path = self.dir(id).join("credentials.json");
        let corrupt = |error: String| {
            tracing::warn!(account = %id, path = %path.display(), %error, "credentials file is corrupt");
            CredsError::Corrupt {
                path: path.clone(),
                error,
            }
        };
        let bytes = match fs::read(&path) {
            Ok(b) => b,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(corrupt(format!("unreadable: {}", e.kind()))),
        };
        match serde_json::from_slice(&bytes) {
            Ok(c) => Ok(Some(c)),
            // serde_json's Display can echo field values (tokens); describe only
            // the category and position.
            Err(e) => {
                let what = match e.classify() {
                    serde_json::error::Category::Io => "unreadable",
                    serde_json::error::Category::Syntax => "invalid JSON",
                    serde_json::error::Category::Data => "unexpected structure or types",
                    serde_json::error::Category::Eof => "truncated JSON",
                };
                Err(corrupt(format!(
                    "{what} at line {} column {}",
                    e.line(),
                    e.column()
                )))
            }
        }
    }

    /// Atomically writes `credentials.json` (0600) while holding the account lock.
    /// Callers that already hold [`Registry::lock`] must use
    /// [`Registry::write_creds_unlocked`] instead (flock is not re-entrant).
    pub fn write_creds(&self, id: &AccountId, c: &Credentials) -> anyhow::Result<()> {
        let _guard = self.lock(id)?;
        self.write_creds_unlocked(id, c)
    }

    /// Like [`Registry::write_creds`] but does not take the lock; the caller
    /// must hold `lock(id)`.
    pub fn write_creds_unlocked(&self, id: &AccountId, c: &Credentials) -> anyhow::Result<()> {
        let dir = self.ensure_dir(id)?;
        let body = serde_json::to_vec_pretty(c).context("serializing credentials")?;
        fsutil::atomic_write(&dir.join("credentials.json"), &body, 0o600)?;
        Ok(())
    }

    /// Blocking exclusive lock on `accounts/<id>/lock` (creates the dir, 0700).
    pub fn lock(&self, id: &AccountId) -> io::Result<FileLock> {
        let dir = self.ensure_dir(id)?;
        FileLock::acquire(&dir.join("lock"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Paths;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use time::macros::datetime;

    fn setup() -> (tempfile::TempDir, Paths, Registry) {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::under(dir.path());
        paths.ensure().unwrap();
        let reg = Registry::load(&paths).unwrap();
        (dir, paths, reg)
    }

    fn id(s: &str) -> AccountId {
        s.parse().unwrap()
    }

    fn meta(s: &str) -> AccountMeta {
        AccountMeta::new(id(s), ProviderId::Anthropic)
    }

    fn creds() -> Credentials {
        Credentials {
            provider: ProviderId::Anthropic,
            access_token: "sk-ant-oat-SECRET-ACCESS".into(),
            refresh_token: Some("sk-ant-ort-SECRET-REFRESH".into()),
            expires_at: Some(datetime!(2026-11-07 12:00:00 UTC)),
            refresh_expires_at: None,
            scopes: vec!["user:inference".into()],
            plan: Some("max".into()),
            tier: Some("default_claude_max_20x".into()),
            openai_account_id: None,
        }
    }

    fn mode_of(p: &std::path::Path) -> u32 {
        fs::metadata(p).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn write_then_read_creds_roundtrip_0600() {
        let (_d, paths, reg) = setup();
        reg.upsert(meta("work")).unwrap();
        assert_eq!(reg.read_creds(&id("work")).unwrap(), None);
        reg.write_creds(&id("work"), &creds()).unwrap();
        assert_eq!(reg.read_creds(&id("work")).unwrap(), Some(creds()));
        let f = paths.accounts.join("work/credentials.json");
        assert_eq!(mode_of(&f), 0o600);
        assert_eq!(mode_of(&paths.accounts.join("work")), 0o700);
        assert_eq!(mode_of(&paths.accounts.join("work/account.toml")), 0o600);
        let json = fs::read_to_string(f).unwrap();
        assert!(json.contains("2026-11-07T12:00:00Z"), "{json}");
    }

    #[test]
    fn debug_never_prints_tokens() {
        let s = format!("{:?}", creds());
        assert!(!s.contains("SECRET"), "{s}");
        assert!(s.contains("redacted"));
    }

    #[test]
    fn corrupt_creds_is_an_error_not_none() {
        let (_d, paths, reg) = setup();
        reg.upsert(meta("work")).unwrap();
        let f = paths.accounts.join("work/credentials.json");
        fs::write(
            &f,
            r#"{"provider": "anthropic", "access_token": "SECRET-LEAK" "#,
        )
        .unwrap();
        let err = reg.read_creds(&id("work")).unwrap_err();
        let CredsError::Corrupt { path, error } = &err;
        assert_eq!(path, &f);
        assert!(!error.contains("SECRET"), "{error}");
        assert!(!err.to_string().contains("SECRET"));
        // wrong shape (valid JSON) is also corrupt, and does not echo values
        fs::write(
            &f,
            r#"{"provider": "anthropic", "access_token": 5, "x": "SECRET-LEAK"}"#,
        )
        .unwrap();
        let err = reg.read_creds(&id("work")).unwrap_err();
        assert!(!err.to_string().contains("SECRET"));
        // unreadable (a directory) is corrupt too
        fs::remove_file(&f).unwrap();
        fs::create_dir(&f).unwrap();
        assert!(reg.read_creds(&id("work")).is_err());
    }

    #[test]
    fn remove_deletes_dir() {
        let (_d, paths, reg) = setup();
        reg.upsert(meta("work")).unwrap();
        reg.write_creds(&id("work"), &creds()).unwrap();
        reg.remove(&id("work")).unwrap();
        assert!(!paths.accounts.join("work").exists());
        assert!(reg.get(&id("work")).is_none());
        // removing a missing account is fine
        reg.remove(&id("work")).unwrap();
    }

    #[test]
    fn weight_and_priority_bounds_enforced() {
        let (_d, _p, reg) = setup();
        for w in [0.0, 0.09, 10.01, f64::NAN, f64::INFINITY, -1.0] {
            let mut m = meta("a");
            m.weight = w;
            assert!(reg.upsert(m).is_err(), "weight {w} should be rejected");
        }
        for w in [0.1, 1.0, 10.0] {
            let mut m = meta("a");
            m.weight = w;
            reg.upsert(m).unwrap();
        }
        let mut m = meta("a");
        m.weight_override = Some(TimedOverride {
            weight: 11.0,
            until: datetime!(2030-01-01 0:00 UTC),
        });
        assert!(reg.upsert(m).is_err());
        let mut m = meta("a");
        m.weight_override = Some(TimedOverride {
            weight: 5.0,
            until: datetime!(2030-01-01 0:00 UTC),
        });
        reg.upsert(m.clone()).unwrap();
        assert_eq!(reg.get(&id("a")).unwrap(), m);
    }

    #[test]
    fn upsert_get_list_roundtrip_sorted() {
        let (_d, _p, reg) = setup();
        let mut b = meta("b");
        b.priority = 7;
        b.weight = 2.5;
        b.note = Some("second".into());
        b.identity = Identity {
            account_uuid: Some("u-1".into()),
            email: Some("b@example.com".into()),
            org_uuid: Some("o-1".into()),
        };
        reg.upsert(b.clone()).unwrap();
        reg.upsert(meta("a")).unwrap();
        let list = reg.list();
        assert_eq!(
            list.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
            ["a", "b"]
        );
        assert_eq!(list[0].priority, 100);
        assert_eq!(list[0].weight, 1.0);
        assert_eq!(reg.get(&id("b")).unwrap(), b);
        // upsert replaces
        b.priority = 1;
        reg.upsert(b.clone()).unwrap();
        assert_eq!(reg.get(&id("b")).unwrap().priority, 1);
        assert_eq!(reg.list().len(), 2);
    }

    #[test]
    fn list_skips_dirs_without_valid_account_toml() {
        let (_d, paths, reg) = setup();
        reg.upsert(meta("good")).unwrap();
        fs::create_dir(paths.accounts.join("no-toml")).unwrap();
        fs::create_dir(paths.accounts.join("bad-toml")).unwrap();
        fs::write(paths.accounts.join("bad-toml/account.toml"), "not = [valid").unwrap();
        fs::create_dir(paths.accounts.join("mismatch")).unwrap();
        let mut other = meta("other");
        other.priority = 3;
        let body = toml::to_string(&other).unwrap();
        fs::write(paths.accounts.join("mismatch/account.toml"), body).unwrap();
        fs::create_dir(paths.accounts.join("bad id")).unwrap();
        fs::write(paths.accounts.join("stray-file"), "x").unwrap();
        let list = reg.list();
        assert_eq!(
            list.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
            ["good"]
        );
    }

    #[test]
    fn lock_is_exclusive_and_write_creds_waits_for_it() {
        let (_d, paths, reg) = setup();
        reg.upsert(meta("work")).unwrap();
        let guard = reg.lock(&id("work")).unwrap();
        let lock_path = paths.accounts.join("work/lock");
        assert!(crate::fsutil::FileLock::try_acquire(&lock_path)
            .unwrap()
            .is_none());
        // holder of lock() writes with the unlocked variant (no self-deadlock)
        reg.write_creds_unlocked(&id("work"), &creds()).unwrap();
        drop(guard);
        assert!(crate::fsutil::FileLock::try_acquire(&lock_path)
            .unwrap()
            .is_some());
        reg.write_creds(&id("work"), &creds()).unwrap();
    }
}
