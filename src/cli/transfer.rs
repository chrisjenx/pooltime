//! `pooltime accounts export|import`: move a pool to another machine without signing every
//! account in again (the bundle and its encryption: [`crate::accounts::transfer`]).
//!
//! Export reads the files directly (the daemon may be running; every file is replaced
//! atomically, so a read never sees half a write). Import writes accounts the way
//! `accounts add --from-codex-auth` does: through the management API's account creation when a
//! daemon answers (so it notices the new account at once), then `account.toml` and
//! `credentials.json` on disk, which the daemon reads per request. Without a daemon it writes
//! the files in-process. Both paths make the same decisions: everything (accounts, config,
//! models) is planned and validated before the first write, and an account whose import fails
//! is reported (a replaced one is put back) while the others carry on.
//!
//! The passphrase comes from `--passphrase-file`, else `POOLTIME_EXPORT_PASSPHRASE`, else a
//! no-echo prompt on the terminal (twice for export). Never from a command-line argument.

use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use age::secrecy::SecretString;
use anyhow::{anyhow, bail, Context};
use serde_json::json;
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;

use super::client::{self, seg, Client};
use crate::accounts::transfer::{
    self, Bundle, BundledAccount, Login, MergedConfig, PASSPHRASE_ENV,
};
use crate::accounts::{AccountId, AccountMeta, Credentials, Registry, StateBook};
use crate::config::{Config, Paths};
use crate::models::ModelTable;
use crate::store::Store;

/// Said after every export (and in the docs): both machines refreshing the same sign-in can
/// log each other out, because refresh tokens rotate.
pub const STOP_OLD_POOL_HINT: &str = "\
Next: import it on the new machine, then stop the pool here for good (`pooltime service \
uninstall`, or remove the accounts with `pooltime accounts remove <name>`). Refresh tokens \
rotate: whichever machine refreshes an account first logs the other one out of it.";

/// Said after every export and import.
pub const DELETE_FILE_HINT: &str = "holds live sign-ins (refresh tokens)";

#[derive(Debug, clap::Args)]
pub struct ExportArgs {
    /// File to write (mode 0600). Conventionally `*.age`.
    pub file: PathBuf,
    /// Replace FILE if it already exists.
    #[arg(long)]
    pub force: bool,
    /// Read the passphrase from the first line of this file instead of prompting
    /// (or set POOLTIME_EXPORT_PASSPHRASE).
    #[arg(long, value_name = "PATH")]
    pub passphrase_file: Option<PathBuf>,
}

#[derive(Debug, clap::Args)]
pub struct ImportArgs {
    /// An export written by `pooltime accounts export`.
    pub file: PathBuf,
    /// Overwrite accounts that already exist here (and config.toml / models.toml), instead of
    /// skipping them. An account whose exported sign-in had expired keeps this machine's.
    #[arg(long)]
    pub replace: bool,
    /// Also take the export's host, port and proxy_key when config.toml is restored (by
    /// default this machine's are kept, so Claude Code profiles connected here keep working).
    #[arg(long)]
    pub replace_network_settings: bool,
    /// Read the passphrase from the first line of this file instead of prompting
    /// (or set POOLTIME_EXPORT_PASSPHRASE).
    #[arg(long, value_name = "PATH")]
    pub passphrase_file: Option<PathBuf>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Purpose {
    Export,
    Import,
}

/// `--passphrase-file`, then the env var, then the terminal. Empty is refused everywhere.
fn passphrase(file: Option<&Path>, purpose: Purpose) -> anyhow::Result<SecretString> {
    let file_text = match file {
        Some(p) => Some(
            std::fs::read_to_string(p)
                .with_context(|| format!("reading the passphrase file {}", p.display()))?,
        ),
        None => None,
    };
    let pw = match given_passphrase(file_text, std::env::var(PASSPHRASE_ENV).ok()) {
        Some(pw) => pw,
        None => prompt(purpose)?,
    };
    if pw.trim().is_empty() {
        bail!("the passphrase must not be empty");
    }
    if purpose == Purpose::Export && pw.chars().count() < 8 {
        eprintln!("warning: a short passphrase is easy to guess; this file holds live sign-ins");
    }
    Ok(SecretString::from(pw))
}

/// The passphrase file's first line, else the env var; both without a trailing line ending
/// (`echo pw > file` and `VAR=$(cat file)` mean the same passphrase). `None`: ask.
fn given_passphrase(file_text: Option<String>, env: Option<String>) -> Option<String> {
    let first_line = |s: &str| {
        s.split('\n')
            .next()
            .unwrap_or_default()
            .trim_end_matches('\r')
            .to_string()
    };
    match file_text {
        Some(t) => Some(first_line(&t)),
        None => env
            .map(|v| v.trim_end_matches(['\r', '\n']).to_string())
            .filter(|v| !v.is_empty()),
    }
}

fn prompt(purpose: Purpose) -> anyhow::Result<String> {
    if !std::io::stdin().is_terminal() {
        bail!(
            "no terminal to ask for the passphrase: pass --passphrase-file PATH or set \
             {PASSPHRASE_ENV}"
        );
    }
    let ask = |q: &str| rpassword::prompt_password(q).context("reading the passphrase");
    match purpose {
        Purpose::Import => ask("Passphrase: "),
        Purpose::Export => {
            let first = ask("Passphrase for the export: ")?;
            if first.trim().is_empty() {
                bail!("the passphrase must not be empty");
            }
            let again = ask("Repeat the passphrase: ")?;
            if first != again {
                bail!("the passphrases do not match; nothing was written");
            }
            Ok(first)
        }
    }
}

fn plural(n: usize, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

fn describe(b: &Bundle) -> String {
    let (a, o) = b.provider_counts();
    let mut s = format!(
        "{} ({a} Claude, {o} ChatGPT/Codex)",
        plural(b.accounts.len(), "account", "accounts")
    );
    let settings: Vec<&str> = [
        b.config_toml.as_ref().map(|_| "config.toml"),
        b.models_toml.as_ref().map(|_| "models.toml"),
    ]
    .into_iter()
    .flatten()
    .collect();
    if !settings.is_empty() {
        s.push_str(&format!(" and {}", settings.join(", ")));
    }
    s
}

pub async fn export(paths: &Paths, args: ExportArgs) -> anyhow::Result<i32> {
    if !args.force && std::fs::symlink_metadata(&args.file).is_ok() {
        bail!(
            "{} already exists; pass --force to replace it",
            args.file.display()
        );
    }
    let pw = passphrase(args.passphrase_file.as_deref(), Purpose::Export)?;
    let now = OffsetDateTime::now_utc();
    let collected = transfer::collect(paths, now.replace_nanosecond(0).unwrap_or(now))?;
    if collected.bundle.accounts.is_empty() {
        bail!("there are no accounts to export");
    }
    for id in &collected.unreadable {
        eprintln!(
            "warning: '{id}' has an unreadable credentials.json; exported without its sign-in \
             (sign it in on the new machine)"
        );
    }
    let sealed = tokio::task::spawn_blocking(move || {
        let sealed = transfer::seal(&collected.bundle, pw, None)?;
        Ok::<_, anyhow::Error>((sealed, describe(&collected.bundle)))
    })
    .await
    .context("encrypting the export")??;
    let (bytes, what) = sealed;
    crate::fsutil::atomic_write(&args.file, &bytes, 0o600)
        .with_context(|| format!("writing {}", args.file.display()))?;
    println!("exported {what} to {} (mode 0600)", args.file.display());
    println!(
        "warning: {} {DELETE_FILE_HINT}: move it over a channel you trust and delete it once \
         it is imported.",
        args.file.display()
    );
    if matches!(client::find(paths).await, Ok(Some(_))) {
        println!(
            "note: pooltime is still running here; its next token refresh can make the exported \
             sign-ins stale. Stop it now."
        );
    }
    println!("{STOP_OLD_POOL_HINT}");
    Ok(0)
}

// ---- import ----------------------------------------------------------------------------------

/// This machine's copy of an account, to put back if replacing it fails.
struct Snapshot {
    meta: AccountMeta,
    creds: Option<Credentials>,
}

/// What import does with one account, decided before anything is written.
enum Action {
    Add,
    Skip,
    /// `--replace`, but the export has no usable sign-in and this machine has one for the same
    /// provider: replace `account.toml` only and keep `credentials.json`.
    MetadataOnly,
    /// `--replace`: remove this machine's account, then add the export's.
    Replace(Box<Snapshot>),
}

struct Planned<'a> {
    a: &'a BundledAccount,
    login: Login,
    /// The email as the management API accepts it (`None` when it would be refused), so the
    /// daemon's create cannot fail on it. The full identity is written to `account.toml` after.
    api_email: Option<String>,
    action: Action,
}

/// How one account's import went.
enum Outcome {
    Added(Login),
    Replaced(Login),
    KeptSignIn,
    Skipped,
    Failed(String),
}

/// Where accounts are written: through a running daemon, or straight to the files.
enum Target<'a> {
    Daemon(&'a Client),
    Offline { state: Option<Arc<StateBook>> },
}

/// The management API's rule for `email` on account creation (`server::mgmt`).
fn api_email(e: Option<&str>) -> Option<String> {
    e.map(str::trim)
        .filter(|e| e.len() > 2 && e.contains('@'))
        .map(str::to_string)
}

fn plan<'a>(
    reg: &Registry,
    bundle: &'a Bundle,
    replace: bool,
    now: OffsetDateTime,
) -> Vec<Planned<'a>> {
    bundle
        .accounts
        .iter()
        .map(|a| {
            let login = a.login(now);
            let action = match reg.get(&a.meta.id) {
                None => Action::Add,
                Some(_) if !replace => Action::Skip,
                Some(meta) => {
                    // An unreadable credentials.json counts as none: nothing to keep.
                    let creds = reg.read_creds(&a.meta.id).ok().flatten();
                    let keep = login == Login::NeedsSignIn
                        && creds
                            .as_ref()
                            .is_some_and(|c| c.provider == a.meta.provider);
                    if keep {
                        Action::MetadataOnly
                    } else {
                        Action::Replace(Box::new(Snapshot { meta, creds }))
                    }
                }
            };
            Planned {
                a,
                login,
                api_email: api_email(a.meta.identity.email.as_deref()),
                action,
            }
        })
        .collect()
}

/// A daemon answers on this home's port: make sure it is this home's (its management key is
/// ours) before writing anything, instead of failing halfway or writing behind its back.
async fn check_daemon_home(c: &Client) -> anyhow::Result<()> {
    let wrong = || {
        anyhow!(
            "a pooltime from a different home (POOLTIME_HOME) is listening on port {}: set \
             POOLTIME_PORT to a free port for this home, or stop that pooltime, then retry. \
             Nothing was imported.",
            c.addr.port()
        )
    };
    if c.mgmt_key().is_err() {
        return Err(wrong());
    }
    match c.get_authed_opt("/api/v1/debug").await {
        Ok(_) => Ok(()),
        Err(e) if format!("{e:#}").contains("management key required") => Err(wrong()),
        Err(e) => Err(e),
    }
}

pub async fn import(paths: &Paths, args: ImportArgs) -> anyhow::Result<i32> {
    let bytes =
        std::fs::read(&args.file).with_context(|| format!("reading {}", args.file.display()))?;
    let pw = passphrase(args.passphrase_file.as_deref(), Purpose::Import)?;
    let bundle = tokio::task::spawn_blocking(move || transfer::open(&bytes, pw))
        .await
        .context("decrypting the export")?
        .map_err(|e| anyhow!("cannot open {}: {e}", args.file.display()))?;
    let created = bundle
        .created_at
        .format(&Rfc3339)
        .unwrap_or_else(|_| "?".into());
    println!(
        "export from {created} (pooltime {}): {}",
        bundle.pooltime_version,
        describe(&bundle)
    );

    let found = client::find(paths).await?;
    if let Some(c) = &found {
        check_daemon_home(c).await?;
    }

    // Plan and validate everything before the first write.
    let config = match &bundle.config_toml {
        Some(text) => plan_config(paths, text, args.replace, args.replace_network_settings)?,
        None => None,
    };
    let models = match &bundle.models_toml {
        Some(text) => plan_models(paths, text, args.replace)?,
        None => None,
    };
    paths
        .ensure()
        .context("creating the pooltime data directory")?;
    let reg = Registry::load(paths)?;
    let now = OffsetDateTime::now_utc();
    let planned = plan(&reg, &bundle, args.replace, now);

    let mut target = match &found {
        Some(c) => Target::Daemon(c),
        None => Target::Offline { state: None },
    };
    let (mut imported, mut failed) = (0, 0);
    let mut sign_in = Vec::new();
    for p in &planned {
        let id = p.a.meta.id.as_str();
        let provider = p.a.meta.provider;
        let outcome = apply(paths, &reg, &mut target, p).await;
        let line = match &outcome {
            Outcome::Added(_) => "imported".to_string(),
            Outcome::Replaced(_) => "replaced with the export's".to_string(),
            Outcome::KeptSignIn => "settings replaced; kept this machine's sign-in (the \
                                    export's had expired)"
                .to_string(),
            Outcome::Skipped => "skipped, an account with this name exists here (--replace \
                                 overwrites it)"
                .to_string(),
            Outcome::Failed(e) => format!("FAILED: {e}"),
        };
        match &outcome {
            Outcome::Added(l) | Outcome::Replaced(l) => {
                imported += 1;
                if *l == Login::NeedsSignIn {
                    sign_in.push(id.to_string());
                }
            }
            Outcome::KeptSignIn => imported += 1,
            Outcome::Skipped => {}
            Outcome::Failed(_) => failed += 1,
        }
        let needs = matches!(
            outcome,
            Outcome::Added(Login::NeedsSignIn) | Outcome::Replaced(Login::NeedsSignIn)
        );
        println!(
            "  {id} ({provider}): {line}{}",
            if needs {
                ", needs sign-in (no saved login, or it had expired; not restored)"
            } else {
                ""
            }
        );
    }
    if let Target::Offline { state: Some(s) } = &target {
        s.flush().await?;
    }

    let mut restart = false;
    if let Some(m) = &config {
        restart |= write_config(paths, m, found.as_ref()).await?;
    }
    if let Some(t) = &models {
        t.save(paths).context("writing models.toml")?;
        println!("models.toml: restored");
        restart = true;
    }

    println!(
        "imported {imported} of {}",
        plural(bundle.accounts.len(), "account", "accounts")
    );
    if failed > 0 {
        println!(
            "{} could not be imported (see above)",
            plural(failed, "account", "accounts")
        );
    }
    for id in &sign_in {
        println!("sign in: pooltime accounts login {id}");
    }
    if restart && found.is_some() {
        println!(
            "restart pooltime to apply the restored settings: `pooltime service restart` \
             (a changed port or host needs `pooltime service install`)"
        );
    }
    if found.is_none() && imported > 0 {
        println!("start pooltime to serve them: `pooltime service install` or `pooltime serve`");
    }
    println!(
        "Delete {} now: it {DELETE_FILE_HINT}. Stop the pool on the old machine if you have \
         not yet (both refreshing the same sign-ins log each other out).",
        args.file.display()
    );
    Ok(if failed > 0 { 1 } else { 0 })
}

/// Carry out one planned account. Never aborts the import: a failure is reported, and a
/// replaced account is put back as it was.
async fn apply(paths: &Paths, reg: &Registry, target: &mut Target<'_>, p: &Planned<'_>) -> Outcome {
    let id = &p.a.meta.id;
    match &p.action {
        Action::Skip => Outcome::Skipped,
        Action::MetadataOnly => match reg.upsert(p.a.meta.clone()) {
            Ok(()) => Outcome::KeptSignIn,
            Err(e) => Outcome::Failed(format!("{e:#}")),
        },
        Action::Add => match add(reg, target, p).await {
            Ok(()) => Outcome::Added(p.login),
            Err(e) => Outcome::Failed(format!("{e:#}")),
        },
        Action::Replace(old) => {
            if let Err(e) = remove(paths, reg, target, id).await {
                return Outcome::Failed(format!(
                    "{e:#} (this machine's account was left as it was)"
                ));
            }
            match add(reg, target, p).await {
                Ok(()) => Outcome::Replaced(p.login),
                Err(e) => match restore(reg, old) {
                    Ok(()) => {
                        Outcome::Failed(format!("{e:#} (this machine's account was put back)"))
                    }
                    Err(r) => Outcome::Failed(format!(
                        "{e:#}; putting this machine's account back failed too: {r:#}"
                    )),
                },
            }
        }
    }
}

/// Remove this machine's account: through the daemon (which also ends a pending sign-in and
/// forgets its runtime state), or the files and the state row.
async fn remove(
    paths: &Paths,
    reg: &Registry,
    target: &mut Target<'_>,
    id: &AccountId,
) -> anyhow::Result<()> {
    match target {
        Target::Daemon(c) => {
            c.delete(&format!("/api/v1/accounts/{}", seg(id.as_str())))
                .await?;
        }
        Target::Offline { state } => {
            if state.is_none() {
                *state = Some(Arc::new(StateBook::load(Arc::new(Store::open(
                    &paths.db,
                )?))?));
            }
            reg.remove(id)?;
            if let Some(s) = state {
                s.forget(id);
            }
        }
    }
    Ok(())
}

/// Create the account (through the daemon when one runs), then write the full `account.toml`
/// (priority, weight, identity, note, override) and the sign-in if it is still usable.
async fn add(reg: &Registry, target: &Target<'_>, p: &Planned<'_>) -> anyhow::Result<()> {
    let id = &p.a.meta.id;
    if let Target::Daemon(c) = target {
        c.post(
            "/api/v1/accounts",
            json!({
                "id": id.as_str(),
                "provider": p.a.meta.provider.as_str(),
                "email": p.api_email,
            }),
        )
        .await?;
    }
    reg.upsert(p.a.meta.clone())?;
    if let (Login::Usable, Some(creds)) = (p.login, &p.a.credentials) {
        reg.write_creds(id, creds)?;
    }
    Ok(())
}

fn restore(reg: &Registry, old: &Snapshot) -> anyhow::Result<()> {
    reg.upsert(old.meta.clone())?;
    if let Some(c) = &old.creds {
        reg.write_creds(&old.meta.id, c)?;
    }
    Ok(())
}

/// The merged `config.toml` to write, or `None` to keep this machine's. Validated in full here
/// (before any write); prints what will be kept.
fn plan_config(
    paths: &Paths,
    text: &str,
    replace: bool,
    take_network: bool,
) -> anyhow::Result<Option<MergedConfig>> {
    let existing = match std::fs::read_to_string(&paths.config) {
        Ok(s) => Some(s),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(e).with_context(|| format!("reading {}", paths.config.display())),
    };
    if existing.is_some() && !replace {
        println!("config.toml: kept this machine's (--replace takes the export's)");
        return Ok(None);
    }
    let merged = transfer::merge_config(text, existing.as_deref(), take_network, |k| {
        std::env::var(k).ok()
    })
    .context("config.toml not restored, nothing was imported")?;
    Ok(Some(merged))
}

/// Write the merged `config.toml`.
///
/// The daemon's own config.toml writers hold its in-process `config_write` lock, which this
/// process cannot take. They each re-read the file under that lock and replace only the fields
/// they own: the routing tuning (from the daemon's memory) and `connect`'s profiles and proxy
/// key. So the restored tuning is sent to the daemon first (its next tuning save then writes
/// the same values), and profiles are always, the proxy key by default, this machine's. Only a
/// tuning change or `connect` running at the same moment as the import could interleave.
async fn write_config(
    paths: &Paths,
    m: &MergedConfig,
    daemon: Option<&Client>,
) -> anyhow::Result<bool> {
    if let Some(c) = daemon {
        let cfg: Config = toml::from_str(&m.text).context("parsing the merged config")?;
        let r = &cfg.routing;
        let body = json!({
            "five_hour_exp": r.five_hour_exp,
            "min_headroom": r.min_headroom,
            "headroom_taper_start": r.headroom_taper_start,
        });
        if let Err(e) = c.post("/api/v1/tuning", body).await {
            eprintln!("warning: could not apply the restored tuning live: {e:#}");
        }
    }
    crate::fsutil::atomic_write(&paths.config, m.text.as_bytes(), 0o600)
        .with_context(|| format!("writing {}", paths.config.display()))?;
    if m.changed.is_empty() {
        println!("config.toml: restored (no setting changed)");
    } else {
        println!("config.toml: restored; changed: {}", m.changed.join(", "));
    }
    if !m.kept_network.is_empty() {
        println!(
            "config.toml: kept this machine's {} (--replace-network-settings takes the export's)",
            m.kept_network.join(", ")
        );
    }
    Ok(!m.changed.is_empty())
}

/// The export's model table, or `None` to keep this machine's `models.toml`. It must parse, and
/// is loaded as a [`ModelTable`] (rows and mappings sanitised as at startup) and saved from that.
fn plan_models(paths: &Paths, text: &str, replace: bool) -> anyhow::Result<Option<ModelTable>> {
    if ModelTable::path(paths).exists() && !replace {
        println!("models.toml: kept this machine's (--replace takes the export's)");
        return Ok(None);
    }
    if toml::from_str::<toml::Table>(text).is_err() {
        bail!("the export's models.toml does not parse; nothing was imported");
    }
    Ok(Some(ModelTable::from_toml_str(text)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::ProviderId;
    use age::secrecy::ExposeSecret;

    #[test]
    fn passphrase_file_reads_the_first_line_and_refuses_empty() {
        let d = tempfile::tempdir().unwrap();
        let f = d.path().join("pw");
        std::fs::write(&f, "correct horse battery\r\nignored\n").unwrap();
        let pw = passphrase(Some(&f), Purpose::Import).unwrap();
        assert_eq!(pw.expose_secret(), "correct horse battery");
        std::fs::write(&f, "\n").unwrap();
        let e = passphrase(Some(&f), Purpose::Export)
            .unwrap_err()
            .to_string();
        assert!(e.contains("empty"), "{e}");
        std::fs::write(&f, "").unwrap();
        assert!(passphrase(Some(&f), Purpose::Import).is_err());
    }

    #[test]
    fn env_passphrase_loses_its_line_ending_like_the_file() {
        let env = |v: &str| given_passphrase(None, Some(v.to_string()));
        assert_eq!(env("pw here\n").as_deref(), Some("pw here"));
        assert_eq!(env("pw here\r\n").as_deref(), Some("pw here"));
        assert_eq!(
            env("  pw  ").as_deref(),
            Some("  pw  "),
            "inner spaces are kept"
        );
        assert_eq!(env("\n"), None);
        assert_eq!(
            given_passphrase(Some("pw here\n".into()), Some("other".into())).as_deref(),
            Some("pw here"),
            "the file wins"
        );
    }

    #[test]
    fn api_email_matches_the_daemons_rule() {
        assert_eq!(
            api_email(Some(" user@example.com ")).as_deref(),
            Some("user@example.com")
        );
        assert_eq!(api_email(Some("@")), None);
        assert_eq!(api_email(Some("a@")), None);
        assert_eq!(api_email(Some("nobody")), None);
        assert_eq!(api_email(None), None);
    }

    fn creds(p: ProviderId) -> Credentials {
        serde_json::from_value(json!({
            "provider": p.as_str(), "access_token": "SECRET-A", "refresh_token": "SECRET-R"
        }))
        .unwrap()
    }

    #[test]
    fn replace_keeps_a_working_sign_in_when_the_export_has_none() {
        let d = tempfile::tempdir().unwrap();
        let paths = Paths::under(d.path());
        paths.ensure().unwrap();
        let reg = Registry::load(&paths).unwrap();
        for id in ["acct-a", "acct-b", "acct-c"] {
            let id: AccountId = id.parse().unwrap();
            reg.upsert(AccountMeta::new(id.clone(), ProviderId::Anthropic))
                .unwrap();
            reg.write_creds(&id, &creds(ProviderId::Anthropic)).unwrap();
        }
        let entry = |id: &str, c: Option<Credentials>, p: ProviderId| BundledAccount {
            meta: AccountMeta::new(id.parse().unwrap(), p),
            credentials: c,
        };
        let bundle = Bundle {
            format: transfer::FORMAT.into(),
            version: transfer::VERSION,
            created_at: OffsetDateTime::now_utc(),
            pooltime_version: "test".into(),
            accounts: vec![
                // No sign-in in the export: keep this machine's.
                entry("acct-a", None, ProviderId::Anthropic),
                // A live sign-in in the export: replace.
                entry(
                    "acct-b",
                    Some(creds(ProviderId::Anthropic)),
                    ProviderId::Anthropic,
                ),
                // Another provider: this machine's sign-in would not fit; replace.
                entry("acct-c", None, ProviderId::OpenAi),
                entry("acct-d", None, ProviderId::Anthropic),
            ],
            config_toml: None,
            models_toml: None,
        };
        let now = OffsetDateTime::now_utc();
        let p = plan(&reg, &bundle, true, now);
        assert!(matches!(p[0].action, Action::MetadataOnly));
        assert!(matches!(p[1].action, Action::Replace(_)));
        assert!(matches!(p[2].action, Action::Replace(_)));
        assert!(matches!(p[3].action, Action::Add));
        let p = plan(&reg, &bundle, false, now);
        assert!(matches!(p[0].action, Action::Skip));
    }
}
