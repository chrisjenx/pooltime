//! The pool-transfer bundle behind `pooltime accounts export|import`: every account's
//! `account.toml` and `credentials.json`, plus the portable parts of `config.toml` and
//! `models.toml`, as one JSON document sealed with a passphrase.
//!
//! Encryption is the [age](https://age-encryption.org/v1) format with its scrypt passphrase
//! recipient (the `age` crate): scrypt (work factor calibrated to about a second on the
//! exporting machine) wraps a random file key, the header is HMAC'd and the payload is
//! ChaCha20-Poly1305 in age's STREAM construction, so any change to the file fails to decrypt.
//! An export is a standard age file: `age -d` / `rage -d` can open it too.
//!
//! Left out on purpose (machine-specific or rebuilt by the daemon): `config.toml`'s
//! `[[profiles]]` (Claude Code profile paths on this machine), `run/` (pid, management key),
//! `logs/`, and `pooltime.db` (journal, usage history, rate-limit and error state).
//!
//! Nothing in here prints or logs a token: [`Bundle`] has no `Debug`, and parse errors are
//! described by category only.

use std::fs;
use std::io;

use age::secrecy::SecretString;
use anyhow::Context;
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use super::creds::Credentials;
use super::registry::{AccountMeta, Registry};
use crate::config::Paths;
use crate::models::ModelTable;
use crate::providers::ProviderId;

/// `format` of every bundle.
pub const FORMAT: &str = "pooltime-accounts-export";
/// The bundle layout this build writes and reads.
pub const VERSION: u32 = 1;
/// Env var holding the passphrase for scripted export/import (never a CLI argument).
pub const PASSPHRASE_ENV: &str = "POOLTIME_EXPORT_PASSPHRASE";

/// The decrypted export. No `Debug`: it holds tokens (and possibly the proxy key).
#[derive(Serialize, Deserialize)]
pub struct Bundle {
    pub format: String,
    pub version: u32,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    /// The exporting pooltime's version (informational).
    pub pooltime_version: String,
    pub accounts: Vec<BundledAccount>,
    /// `config.toml` without `profiles`; `None` when the exporter had none.
    #[serde(default)]
    pub config_toml: Option<String>,
    /// `models.toml` as it was on disk; `None` when the exporter had none.
    #[serde(default)]
    pub models_toml: Option<String>,
}

/// One account: its `account.toml` and, when it had one, its `credentials.json`.
#[derive(Serialize, Deserialize)]
pub struct BundledAccount {
    pub meta: AccountMeta,
    #[serde(default)]
    pub credentials: Option<Credentials>,
}

/// Whether an account's saved sign-in can still serve on arrival.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Login {
    Usable,
    /// No credentials, or the login (refresh token) has expired: needs `accounts login`.
    NeedsSignIn,
}

impl BundledAccount {
    pub fn login(&self, now: OffsetDateTime) -> Login {
        match &self.credentials {
            Some(c) if !expired(c, now) => Login::Usable,
            _ => Login::NeedsSignIn,
        }
    }
}

/// A login is over when its refresh token is past its known expiry, or when there is no refresh
/// token and the access token has expired. An unknown expiry counts as usable.
pub fn expired(c: &Credentials, now: OffsetDateTime) -> bool {
    if c.refresh_expires_at.is_some_and(|t| t <= now) {
        return true;
    }
    c.refresh_token.is_none() && c.expires_at.is_some_and(|t| t <= now)
}

impl Bundle {
    /// Accounts per provider, for summaries: `(anthropic, openai)`.
    pub fn provider_counts(&self) -> (usize, usize) {
        let a = self
            .accounts
            .iter()
            .filter(|b| b.meta.provider == ProviderId::Anthropic)
            .count();
        (a, self.accounts.len() - a)
    }
}

/// What [`collect`] could not take along (never fatal; reported by the CLI).
pub struct Collected {
    pub bundle: Bundle,
    /// Accounts whose `credentials.json` was unreadable: exported without credentials.
    pub unreadable: Vec<String>,
}

/// Gather everything under `paths` into a bundle stamped `now`.
pub fn collect(paths: &Paths, now: OffsetDateTime) -> anyhow::Result<Collected> {
    let reg = Registry::load(paths)?;
    let mut accounts = Vec::new();
    let mut unreadable = Vec::new();
    for mut meta in reg.list() {
        let credentials = match reg.read_creds(&meta.id) {
            Ok(c) => c,
            Err(_) => {
                unreadable.push(meta.id.to_string());
                None
            }
        };
        // bro's shadow check belongs to the machine the bro import ran on.
        meta.imported = false;
        accounts.push(BundledAccount { meta, credentials });
    }
    let config_toml = match read_optional(&paths.config)? {
        Some(text) => Some(portable_config(&text).with_context(|| {
            format!("{} is not valid TOML; fix it first", paths.config.display())
        })?),
        None => None,
    };
    let models_toml = read_optional(&ModelTable::path(paths))?;
    Ok(Collected {
        bundle: Bundle {
            format: FORMAT.into(),
            version: VERSION,
            created_at: now,
            pooltime_version: env!("CARGO_PKG_VERSION").into(),
            accounts,
            config_toml,
            models_toml,
        },
        unreadable,
    })
}

fn read_optional(path: &std::path::Path) -> anyhow::Result<Option<String>> {
    match fs::read_to_string(path) {
        Ok(s) => Ok(Some(s)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

/// `config.toml` minus `profiles` (Claude Code profile paths that only exist on this machine).
/// The TOML error is not chained: its message could quote the file (the proxy key).
pub fn portable_config(text: &str) -> anyhow::Result<String> {
    let mut t: toml::Table =
        toml::from_str(text).map_err(|_| anyhow::anyhow!("config.toml does not parse"))?;
    t.remove("profiles");
    Ok(toml::to_string_pretty(&t)?)
}

/// The bundle's `config.toml` for this machine: `incoming` with `existing`'s `profiles` (if any)
/// kept, so `connect`ed profiles here are not forgotten. Rejects text pooltime cannot load.
pub fn merge_config(incoming: &str, existing: Option<&str>) -> anyhow::Result<String> {
    let bad = || anyhow::anyhow!("the export's config.toml is not a valid pooltime config");
    let mut t: toml::Table = toml::from_str(incoming).map_err(|_| bad())?;
    t.remove("profiles");
    if let Some(profiles) = existing
        .and_then(|e| toml::from_str::<toml::Table>(e).ok())
        .and_then(|mut e| e.remove("profiles"))
    {
        t.insert("profiles".into(), profiles);
    }
    let out = toml::to_string_pretty(&t)?;
    toml::from_str::<crate::config::Config>(&out).map_err(|_| bad())?;
    Ok(out)
}

/// Encrypt `bundle` to `passphrase`. `work_factor` overrides age's calibrated scrypt `log_n`
/// (tests only; `None` in the CLI).
pub fn seal(
    bundle: &Bundle,
    passphrase: SecretString,
    work_factor: Option<u8>,
) -> anyhow::Result<Vec<u8>> {
    let plain = serde_json::to_vec(bundle).context("serializing the export")?;
    let mut r = age::scrypt::Recipient::new(passphrase);
    if let Some(n) = work_factor {
        r.set_work_factor(n);
    }
    age::encrypt(&r, &plain).map_err(|e| anyhow::anyhow!("encrypting the export: {e}"))
}

/// Why an export could not be opened. Messages never echo file content.
#[derive(Debug, thiserror::Error)]
pub enum OpenError {
    #[error("wrong passphrase, or the file was changed or damaged")]
    Decrypt,
    #[error("the file was changed or damaged")]
    Damaged,
    #[error("not a pooltime export (not an age passphrase-encrypted file)")]
    NotAnExport,
    #[error(
        "the export needs more scrypt work (2^{required}) than this machine allows (about 16 \
         seconds, 2^{max})"
    )]
    TooSlow { required: u8, max: u8 },
    #[error("unsupported export: {0}")]
    Unsupported(String),
}

/// Decrypt and validate an export.
pub fn open(bytes: &[u8], passphrase: SecretString) -> Result<Bundle, OpenError> {
    use age::DecryptError as D;
    let id = age::scrypt::Identity::new(passphrase);
    let plain = age::decrypt(&id, bytes).map_err(|e| match e {
        D::DecryptionFailed | D::KeyDecryptionFailed => OpenError::Decrypt,
        D::InvalidMac | D::Io(_) => OpenError::Damaged,
        D::ExcessiveWork { required, target } => OpenError::TooSlow {
            required,
            max: target.saturating_add(4),
        },
        // NoMatchingKeys (an age file for public-key recipients), InvalidHeader, UnknownFormat
        // and anything a newer age adds.
        _ => OpenError::NotAnExport,
    })?;
    parse(&plain)
}

fn parse(plain: &[u8]) -> Result<Bundle, OpenError> {
    #[derive(Deserialize)]
    struct Head {
        format: Option<String>,
        version: Option<u32>,
    }
    let head: Head = serde_json::from_slice(plain).map_err(|_| {
        OpenError::Unsupported("the decrypted content is not a pooltime export".into())
    })?;
    if head.format.as_deref() != Some(FORMAT) {
        return Err(OpenError::Unsupported(
            "the decrypted content is not a pooltime export".into(),
        ));
    }
    match head.version {
        Some(VERSION) => {}
        Some(v) if v > VERSION => {
            return Err(OpenError::Unsupported(format!(
                "format version {v} is newer than this pooltime understands ({VERSION}); \
                 upgrade pooltime"
            )))
        }
        v => {
            return Err(OpenError::Unsupported(format!(
                "unknown format version {}",
                v.map_or("(none)".into(), |v| v.to_string())
            )))
        }
    }
    // serde_json's Display can quote values (tokens): report the position only.
    let b: Bundle = serde_json::from_slice(plain).map_err(|e| {
        OpenError::Unsupported(format!(
            "malformed export (line {} column {})",
            e.line(),
            e.column()
        ))
    })?;
    for a in &b.accounts {
        a.meta
            .validate()
            .map_err(|e| OpenError::Unsupported(format!("account {}: {e}", a.meta.id)))?;
        if let Some(c) = &a.credentials {
            if c.provider != a.meta.provider {
                return Err(OpenError::Unsupported(format!(
                    "account {}: credentials are for a different provider",
                    a.meta.id
                )));
            }
        }
    }
    let mut ids: Vec<&str> = b.accounts.iter().map(|a| a.meta.id.as_str()).collect();
    ids.sort_unstable();
    if ids.windows(2).any(|w| w[0] == w[1]) {
        return Err(OpenError::Unsupported("an account id appears twice".into()));
    }
    Ok(b)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::accounts::AccountId;
    use age::secrecy::SecretString;
    use time::macros::datetime;

    const NOW: OffsetDateTime = datetime!(2026-11-19 12:00 UTC);

    fn pw(s: &str) -> SecretString {
        SecretString::from(s.to_string())
    }

    fn creds(p: ProviderId) -> Credentials {
        Credentials {
            provider: p,
            access_token: "SECRET-ACCESS".into(),
            refresh_token: Some("SECRET-REFRESH".into()),
            expires_at: Some(datetime!(2026-11-19 13:00 UTC)),
            refresh_expires_at: None,
            scopes: vec![],
            plan: None,
            tier: None,
            openai_account_id: None,
        }
    }

    fn home() -> (tempfile::TempDir, Paths) {
        let d = tempfile::tempdir().unwrap();
        let p = Paths::under(d.path());
        p.ensure().unwrap();
        (d, p)
    }

    fn seed(p: &Paths) {
        let reg = Registry::load(p).unwrap();
        let id: AccountId = "acct-a".parse().unwrap();
        let mut m = AccountMeta::new(id.clone(), ProviderId::Anthropic);
        m.weight = 2.5;
        m.priority = 7;
        m.imported = true;
        m.identity.email = Some("user@example.com".into());
        reg.upsert(m).unwrap();
        reg.write_creds(&id, &creds(ProviderId::Anthropic)).unwrap();
        let id: AccountId = "acct-b".parse().unwrap();
        reg.upsert(AccountMeta::new(id, ProviderId::OpenAi))
            .unwrap();
        fs::write(
            &p.config,
            "port = 4100\n[routing]\nmin_headroom = 0.2\n[[profiles]]\nid = \"main\"\nsettings_dir = \"/x/.claude\"\n",
        )
        .unwrap();
        fs::write(ModelTable::path(p), "mapping_enabled = true\n").unwrap();
    }

    #[test]
    fn collect_seal_open_round_trip() {
        let (_d, p) = home();
        seed(&p);
        let c = collect(&p, NOW).unwrap();
        assert!(c.unreadable.is_empty());
        let sealed = seal(&c.bundle, pw("correct horse"), Some(2)).unwrap();
        let s = String::from_utf8_lossy(&sealed);
        assert!(s.starts_with("age-encryption.org/v1"), "{s}");
        assert!(!s.contains("SECRET") && !s.contains("example.com"));

        let b = open(&sealed, pw("correct horse")).unwrap();
        assert_eq!(b.version, VERSION);
        assert_eq!(b.created_at, NOW);
        assert_eq!(b.provider_counts(), (1, 1));
        let a = &b.accounts[0];
        assert_eq!(a.meta.id.as_str(), "acct-a");
        assert_eq!((a.meta.weight, a.meta.priority), (2.5, 7));
        assert!(!a.meta.imported, "bro's import flag stays behind");
        assert_eq!(a.credentials.as_ref(), Some(&creds(ProviderId::Anthropic)));
        assert_eq!(a.login(NOW), Login::Usable);
        assert_eq!(b.accounts[1].login(NOW), Login::NeedsSignIn);
        let cfg = b.config_toml.unwrap();
        assert!(
            cfg.contains("4100") && cfg.contains("min_headroom"),
            "{cfg}"
        );
        assert!(!cfg.contains("profiles"), "{cfg}");
        assert_eq!(b.models_toml.as_deref(), Some("mapping_enabled = true\n"));
    }

    #[test]
    fn wrong_passphrase_and_tampering_fail_cleanly() {
        let (_d, p) = home();
        seed(&p);
        let b = collect(&p, NOW).unwrap().bundle;
        let sealed = seal(&b, pw("right"), Some(2)).unwrap();
        assert!(matches!(
            open(&sealed, pw("wrong")),
            Err(OpenError::Decrypt)
        ));
        // Flip a payload byte (the last one: inside the final STREAM chunk's tag).
        let mut t = sealed.clone();
        *t.last_mut().unwrap() ^= 1;
        let e = open(&t, pw("right")).err().unwrap();
        assert!(matches!(e, OpenError::Damaged | OpenError::Decrypt), "{e}");
        // Truncated.
        let e = open(&sealed[..sealed.len() - 20], pw("right"))
            .err()
            .unwrap();
        assert!(matches!(e, OpenError::Damaged | OpenError::Decrypt), "{e}");
        // Not age at all.
        assert!(matches!(
            open(b"hello", pw("right")),
            Err(OpenError::NotAnExport)
        ));
    }

    #[test]
    fn version_and_format_are_checked() {
        let seal_json = |v: serde_json::Value| {
            let r = {
                let mut r = age::scrypt::Recipient::new(pw("p"));
                r.set_work_factor(2);
                r
            };
            age::encrypt(&r, v.to_string().as_bytes()).unwrap()
        };
        let e = open(&seal_json(serde_json::json!({"hello": 1})), pw("p"))
            .err()
            .unwrap();
        assert!(e.to_string().contains("not a pooltime export"), "{e}");
        let e = open(
            &seal_json(serde_json::json!({"format": FORMAT, "version": 99})),
            pw("p"),
        )
        .err()
        .unwrap();
        assert!(e.to_string().contains("upgrade pooltime"), "{e}");
        let e = open(
            &seal_json(serde_json::json!({"format": FORMAT, "version": 1, "accounts": 3})),
            pw("p"),
        )
        .err()
        .unwrap();
        assert!(e.to_string().contains("malformed"), "{e}");
    }

    #[test]
    fn expiry_rules() {
        let mut c = creds(ProviderId::Anthropic);
        assert!(!expired(&c, NOW));
        c.expires_at = Some(datetime!(2026-11-01 0:00 UTC));
        assert!(!expired(&c, NOW), "an old access token refreshes");
        c.refresh_token = None;
        assert!(expired(&c, NOW));
        let mut c = creds(ProviderId::Anthropic);
        c.refresh_expires_at = Some(datetime!(2026-11-18 0:00 UTC));
        assert!(expired(&c, NOW));
    }

    #[test]
    fn merge_config_keeps_this_machines_profiles() {
        let incoming = "port = 4100\n[[profiles]]\nid = \"old\"\nsettings_dir = \"/old\"\n";
        let here = "port = 3460\n[[profiles]]\nid = \"new\"\nsettings_dir = \"/new\"\n";
        let out = merge_config(incoming, Some(here)).unwrap();
        assert!(
            out.contains("4100") && out.contains("/new") && !out.contains("/old"),
            "{out}"
        );
        let out = merge_config(incoming, None).unwrap();
        assert!(!out.contains("profiles"), "{out}");
        assert!(merge_config("port = \"x\"", None).is_err());
    }
}
