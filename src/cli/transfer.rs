//! `pooltime accounts export|import`: move a pool to another machine without signing every
//! account in again (the bundle and its encryption: [`crate::accounts::transfer`]).
//!
//! Export reads the files directly (the daemon may be running; every file is replaced
//! atomically, so a read never sees half a write). Import writes accounts the way
//! `accounts add --from-codex-auth` does: through the management API's account creation when a
//! daemon answers (so it notices the new account at once), then `account.toml` and
//! `credentials.json` on disk, which the daemon reads per request. Without a daemon it writes
//! the files in-process.
//!
//! The passphrase comes from `--passphrase-file`, else `POOLTIME_EXPORT_PASSPHRASE`, else a
//! no-echo prompt on the terminal (twice for export). Never from a command-line argument.

use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use age::secrecy::SecretString;
use anyhow::{bail, Context};
use serde_json::json;
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;

use super::client::{self, seg, Client};
use crate::accounts::transfer::{self, Bundle, BundledAccount, Login, PASSPHRASE_ENV};
use crate::accounts::{Registry, StateBook};
use crate::config::Paths;
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
    /// skipping them.
    #[arg(long)]
    pub replace: bool,
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
    let given = match file {
        Some(p) => Some(read_passphrase_file(p)?),
        None => std::env::var(PASSPHRASE_ENV).ok().filter(|v| !v.is_empty()),
    };
    let pw = match given {
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

/// The first line, without its line ending.
fn read_passphrase_file(p: &Path) -> anyhow::Result<String> {
    let text = std::fs::read_to_string(p)
        .with_context(|| format!("reading the passphrase file {}", p.display()))?;
    Ok(text.lines().next().unwrap_or_default().to_string())
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

/// How one account's import went.
enum Done {
    Imported(Login),
    Skipped,
}

/// Where accounts are written: through a running daemon, or straight to the files.
enum Target<'a> {
    Daemon(&'a Client),
    Offline { state: Option<Arc<StateBook>> },
}

pub async fn import(paths: &Paths, args: ImportArgs) -> anyhow::Result<i32> {
    let bytes =
        std::fs::read(&args.file).with_context(|| format!("reading {}", args.file.display()))?;
    let pw = passphrase(args.passphrase_file.as_deref(), Purpose::Import)?;
    let bundle = tokio::task::spawn_blocking(move || transfer::open(&bytes, pw))
        .await
        .context("decrypting the export")?
        .map_err(|e| anyhow::anyhow!("cannot open {}: {e}", args.file.display()))?;
    let created = bundle
        .created_at
        .format(&Rfc3339)
        .unwrap_or_else(|_| "?".into());
    println!(
        "export from {created} (pooltime {}): {}",
        bundle.pooltime_version,
        describe(&bundle)
    );

    paths
        .ensure()
        .context("creating the pooltime data directory")?;
    let found = client::find(paths).await?;
    let mut target = match &found {
        Some(c) => Target::Daemon(c),
        None => Target::Offline { state: None },
    };
    let reg = Registry::load(paths)?;
    let now = OffsetDateTime::now_utc();
    let mut imported = 0;
    let mut sign_in = Vec::new();
    for a in &bundle.accounts {
        let id = a.meta.id.to_string();
        let provider = a.meta.provider;
        let r = import_one(paths, &reg, &mut target, a, args.replace, now)
            .await
            .with_context(|| format!("importing '{id}'"))?;
        match r {
            Done::Imported(Login::Usable) => {
                imported += 1;
                println!("  {id} ({provider}): imported");
            }
            Done::Imported(Login::NeedsSignIn) => {
                imported += 1;
                sign_in.push(id.clone());
                println!(
                    "  {id} ({provider}): imported, needs sign-in (no saved login, or it had \
                     expired; not restored)"
                );
            }
            Done::Skipped => println!(
                "  {id} ({provider}): skipped, an account with this name exists here (--replace \
                 overwrites it)"
            ),
        }
    }
    if let Target::Offline { state: Some(s) } = &target {
        s.flush().await?;
    }

    let mut restart = false;
    if let Some(text) = &bundle.config_toml {
        restart |= restore_config(paths, text, args.replace, found.as_ref()).await?;
    }
    if let Some(text) = &bundle.models_toml {
        restart |= restore_models(paths, text, args.replace)?;
    }

    println!(
        "imported {imported} of {}",
        plural(bundle.accounts.len(), "account", "accounts")
    );
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
    Ok(0)
}

async fn import_one(
    paths: &Paths,
    reg: &Registry,
    target: &mut Target<'_>,
    a: &BundledAccount,
    replace: bool,
    now: OffsetDateTime,
) -> anyhow::Result<Done> {
    let id = &a.meta.id;
    if reg.get(id).is_some() {
        if !replace {
            return Ok(Done::Skipped);
        }
        match target {
            // Also ends a pending sign-in and forgets its runtime state.
            Target::Daemon(c) => {
                c.delete(&format!("/api/v1/accounts/{}", seg(id.as_str())))
                    .await?;
            }
            Target::Offline { state } => {
                reg.remove(id)?;
                if state.is_none() {
                    *state = Some(Arc::new(StateBook::load(Arc::new(Store::open(
                        &paths.db,
                    )?))?));
                }
                if let Some(s) = state {
                    s.forget(id);
                }
            }
        }
    }
    if let Target::Daemon(c) = target {
        c.post(
            "/api/v1/accounts",
            json!({
                "id": id.as_str(),
                "provider": a.meta.provider.as_str(),
                "email": a.meta.identity.email,
            }),
        )
        .await?;
    }
    // The full account.toml (priority, weight, identity, note, override).
    reg.upsert(a.meta.clone())?;
    let login = a.login(now);
    if let (Login::Usable, Some(creds)) = (login, &a.credentials) {
        reg.write_creds(id, creds)?;
    }
    Ok(Done::Imported(login))
}

/// Write `config.toml` unless one exists (and `--replace` was not given). This machine's
/// `profiles` are always kept. Returns whether it was written.
async fn restore_config(
    paths: &Paths,
    text: &str,
    replace: bool,
    daemon: Option<&Client>,
) -> anyhow::Result<bool> {
    let existing = match std::fs::read_to_string(&paths.config) {
        Ok(s) => Some(s),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(e).with_context(|| format!("reading {}", paths.config.display())),
    };
    if existing.is_some() && !replace {
        println!("config.toml: kept this machine's (--replace takes the export's)");
        return Ok(false);
    }
    let merged = transfer::merge_config(text, existing.as_deref())?;
    crate::fsutil::atomic_write(&paths.config, merged.as_bytes(), 0o600)
        .with_context(|| format!("writing {}", paths.config.display()))?;
    println!("config.toml: restored");
    // A running daemon saves its in-memory tuning back into config.toml on its next tuning
    // change: give it the restored values now so that save cannot undo them.
    if let Some(c) = daemon {
        if let Ok(cfg) = crate::config::Config::load_file(paths) {
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
    }
    Ok(true)
}

/// Write `models.toml` unless one exists (and `--replace` was not given).
fn restore_models(paths: &Paths, text: &str, replace: bool) -> anyhow::Result<bool> {
    let path = ModelTable::path(paths);
    if path.exists() && !replace {
        println!("models.toml: kept this machine's (--replace takes the export's)");
        return Ok(false);
    }
    if toml::from_str::<toml::Table>(text).is_err() {
        eprintln!("warning: the export's models.toml does not parse; not restored");
        return Ok(false);
    }
    crate::fsutil::atomic_write(&path, text.as_bytes(), 0o600)
        .with_context(|| format!("writing {}", path.display()))?;
    println!("models.toml: restored");
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use age::secrecy::ExposeSecret;

    #[test]
    fn passphrase_file_reads_the_first_line_and_refuses_empty() {
        let d = tempfile::tempdir().unwrap();
        let f = d.path().join("pw");
        std::fs::write(&f, "correct horse battery\nignored\n").unwrap();
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
}
