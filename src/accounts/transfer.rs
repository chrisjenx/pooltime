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

/// The settings that tie Claude Code profiles to this machine's pool: `connect`ed profiles point
/// at this address and send this key. Kept from this machine unless explicitly taken.
pub const NETWORK_KEYS: [&str; 3] = ["host", "port", "proxy_key"];

/// [`merge_config`]'s result.
pub struct MergedConfig {
    /// The new `config.toml`.
    pub text: String,
    /// Dotted keys whose effective value changes (names only: values may be secret).
    pub changed: Vec<String>,
    /// [`NETWORK_KEYS`] the export sets differently, kept from this machine instead.
    pub kept_network: Vec<&'static str>,
}

/// The export's `config.toml` for this machine. Always keeps `existing`'s `profiles`; keeps its
/// [`NETWORK_KEYS`] (or the defaults) too unless `take_network`. The result must load and pass
/// [`Config::validate`](crate::config::Config::validate) with the `POOLTIME_*` overlay `env`
/// (the non-loopback host check), else nothing is returned. Errors never quote either file.
pub fn merge_config(
    incoming: &str,
    existing: Option<&str>,
    take_network: bool,
    env: impl Fn(&str) -> Option<String>,
) -> anyhow::Result<MergedConfig> {
    use crate::config::Config;
    let bad = || anyhow::anyhow!("the export's config.toml is not a valid pooltime config");
    let mut t: toml::Table = toml::from_str(incoming).map_err(|_| bad())?;
    let here: toml::Table = match existing {
        Some(e) => toml::from_str(e).map_err(|_| {
            anyhow::anyhow!("this machine's config.toml does not parse; fix it first")
        })?,
        None => toml::Table::new(),
    };
    let before: Config = toml::Value::Table(here.clone()).try_into().map_err(|_| {
        anyhow::anyhow!("this machine's config.toml is not a valid pooltime config")
    })?;
    let exported: Config = toml::Value::Table(t.clone())
        .try_into()
        .map_err(|_| bad())?;
    t.remove("profiles");
    if let Some(p) = here.get("profiles") {
        t.insert("profiles".into(), p.clone());
    }
    let mut kept_network = Vec::new();
    if !take_network {
        for k in NETWORK_KEYS {
            t.remove(k);
            if let Some(v) = here.get(k) {
                t.insert(k.into(), v.clone());
            }
        }
        if exported.host != before.host {
            kept_network.push("host");
        }
        if exported.port != before.port {
            kept_network.push("port");
        }
        if exported.proxy_key != before.proxy_key {
            kept_network.push("proxy_key");
        }
    }
    let text = toml::to_string_pretty(&t)?;
    let after: Config = toml::from_str(&text).map_err(|_| bad())?;
    let mut runtime = after.clone();
    runtime.apply_env(env)?;
    runtime
        .validate()
        .context("the export's config.toml would not run here")?;
    let changed = changed_keys(&before, &after);
    Ok(MergedConfig {
        text,
        changed,
        kept_network,
    })
}

/// Dotted keys (`routing.min_headroom`) whose values differ between two configs, `profiles`
/// left out.
fn changed_keys(a: &crate::config::Config, b: &crate::config::Config) -> Vec<String> {
    fn flat(prefix: &str, v: &toml::Value, out: &mut std::collections::BTreeMap<String, String>) {
        match v {
            toml::Value::Table(t) => {
                for (k, v) in t {
                    let key = if prefix.is_empty() {
                        k.clone()
                    } else {
                        format!("{prefix}.{k}")
                    };
                    flat(&key, v, out);
                }
            }
            other => {
                out.insert(prefix.to_string(), other.to_string());
            }
        }
    }
    let map = |c: &crate::config::Config| {
        let mut out = std::collections::BTreeMap::new();
        if let Ok(v) = toml::Value::try_from(c) {
            flat("", &v, &mut out);
        }
        out.retain(|k, _| k != "profiles" && !k.starts_with("profiles."));
        out
    };
    let (a, b) = (map(a), map(b));
    let keys: std::collections::BTreeSet<&String> = a.keys().chain(b.keys()).collect();
    keys.into_iter()
        .filter(|k| a.get(*k) != b.get(*k))
        .cloned()
        .collect()
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
    fn merge_config_keeps_this_machines_profiles_and_network_settings() {
        let no_env = |_: &str| None;
        let incoming = "port = 4100\nproxy_key = \"OLD-KEY\"\n[routing]\nmin_headroom = 0.3\n\
                        [[profiles]]\nid = \"old\"\nsettings_dir = \"/old\"\n";
        let here = "port = 3460\nproxy_key = \"NEW-KEY\"\n[[profiles]]\nid = \"new\"\n\
                    settings_dir = \"/new\"\n";
        let m = merge_config(incoming, Some(here), false, no_env).unwrap();
        let out = &m.text;
        assert!(out.contains("3460") && out.contains("NEW-KEY"), "{out}");
        assert!(!out.contains("4100") && !out.contains("OLD-KEY"), "{out}");
        assert!(out.contains("/new") && !out.contains("/old"), "{out}");
        assert!(out.contains("min_headroom = 0.3"), "{out}");
        assert_eq!(m.kept_network, ["port", "proxy_key"]);
        assert_eq!(m.changed, ["routing.min_headroom"]);

        let m = merge_config(incoming, Some(here), true, no_env).unwrap();
        assert!(
            m.text.contains("4100") && m.text.contains("OLD-KEY"),
            "{}",
            m.text
        );
        assert!(m.kept_network.is_empty());
        assert_eq!(m.changed, ["port", "proxy_key", "routing.min_headroom"]);

        // A fresh machine: defaults stand in for this machine's values.
        let m = merge_config(incoming, None, false, no_env).unwrap();
        assert!(
            !m.text.contains("profiles") && !m.text.contains("port"),
            "{}",
            m.text
        );
        assert_eq!(m.kept_network, ["port", "proxy_key"]);

        assert!(merge_config("port = \"x\"", None, false, no_env).is_err());
        // Full validation: a network bind without a proxy key is refused.
        let open = "host = \"0.0.0.0\"\n";
        let e = merge_config(open, None, true, no_env).err().unwrap();
        assert!(format!("{e:#}").contains("loopback"), "{e:#}");
        assert!(merge_config(open, None, true, |k: &str| {
            (k == "POOLTIME_PROXY_KEY").then(|| "k".to_string())
        })
        .is_ok());
    }
}
