//! `pooltime accounts list|add|login|relogin|remove|set`.
//!
//! Everything goes through the management API of the running daemon, except sign-in, which runs
//! in-process when no daemon answers (the daemon would otherwise own the account files), and the
//! hidden `login --spike-login-hint`, which always runs in-process against a scratch home.
//!
//! Sign-in: `--mode callback` opens the browser (`open` / `xdg-open`) and waits for the localhost
//! redirect; `--mode paste` prints the URL and reads the `code#state` line the sign-in page shows.
//! Default: paste when headless ([`client::headless`]), else callback. A first sign-in on an
//! account with no identity on file shows the email and asks before binding it (`--yes` skips
//! the question). No token is ever printed.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, bail, Context};
use clap::{ArgGroup, Args, Subcommand, ValueEnum};
use serde_json::{json, Value};
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;

use super::client::{self, seg, Client};
use super::status::accounts_table;
use crate::accounts::{AccountId, AccountMeta, Identity, Registry, StateBook};
use crate::config::{Config, Paths};
use crate::providers::ProviderId;
use crate::signin::claude::ClaudeSignIn;
use crate::signin::{PendingSignIn, SignInFlow, SignInHandle, SignInMode, SignInResult};
use crate::store::Store;
use crate::upstream::Upstream;

#[derive(Debug, Subcommand)]
pub enum AccountsCmd {
    /// List accounts with their status.
    List {
        /// Print the accounts as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Add an account (sign in afterwards with `accounts login`, or pass --login).
    Add {
        name: String,
        /// anthropic (Claude) or openai (ChatGPT/Codex).
        #[arg(long, default_value = "anthropic")]
        provider: String,
        /// Sign in right away.
        #[arg(long)]
        login: bool,
        /// The account's email: recorded as its expected identity (the first sign-in must be
        /// this account) and pre-filled on the sign-in page.
        #[arg(long)]
        email: Option<String>,
        #[command(flatten)]
        signin: SignInFlags,
    },
    /// Sign in (or re-login) an account; creates it when it does not exist.
    Login(LoginArgs),
    /// Sign in several accounts one after another.
    #[command(group(ArgGroup::new("which").required(true).args(["expiring", "dead", "all"])))]
    Relogin {
        /// Accounts whose login expires within DAYS days (default 3).
        #[arg(long, value_name = "DAYS", num_args = 0..=1, default_missing_value = "3")]
        expiring: Option<u32>,
        /// Accounts that need login.
        #[arg(long)]
        dead: bool,
        /// Every Claude account.
        #[arg(long)]
        all: bool,
        #[command(flatten)]
        signin: SignInFlags,
    },
    /// Remove an account and its saved credentials.
    Remove {
        name: String,
        /// Do not ask for confirmation.
        #[arg(long, short)]
        yes: bool,
    },
    /// Change an account's routing priority / weight.
    Set {
        name: String,
        /// Priority tier (lower is preferred).
        #[arg(long)]
        priority: Option<u32>,
        /// Routing weight (0.1..=10). With --until, a temporary override.
        #[arg(long, allow_negative_numbers = true)]
        weight: Option<f64>,
        /// End of a temporary weight: RFC 3339 time or a duration from now ("2h", "3d").
        #[arg(long, value_name = "TIME")]
        until: Option<String>,
        /// Cancel an active temporary weight.
        #[arg(long, conflicts_with = "until")]
        clear_override: bool,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Mode {
    /// Open the browser here and catch the localhost redirect.
    Callback,
    /// Print the URL and paste the code shown after signing in.
    Paste,
}

#[derive(Debug, Clone, Default, Args)]
pub struct SignInFlags {
    /// How to finish the sign-in (default: paste when headless, else callback).
    #[arg(long, value_enum)]
    pub mode: Option<Mode>,
    /// The `code#state` line to submit instead of prompting for it.
    #[arg(long, value_name = "CODE#STATE")]
    pub code: Option<String>,
    /// Bind a first sign-in to the account without asking.
    #[arg(long, short)]
    pub yes: bool,
}

#[derive(Debug, Args)]
pub struct LoginArgs {
    /// Account name.
    #[arg(required_unless_present = "spike_login_hint")]
    pub name: Option<String>,
    /// Email to pre-fill on the sign-in page (default: the account's email on file).
    #[arg(long)]
    pub email: Option<String>,
    #[command(flatten)]
    pub signin: SignInFlags,
    /// Diagnostic: sign in with login_hint=EMAIL against a throwaway account, print which
    /// identity came back, and save nothing.
    #[arg(long, hide = true, value_name = "EMAIL", conflicts_with = "name")]
    pub spike_login_hint: Option<String>,
}

pub async fn run(paths: &Paths, cmd: AccountsCmd) -> anyhow::Result<i32> {
    match cmd {
        AccountsCmd::List { json } => {
            let status = client::require(paths).await?.get("/api/v1/status").await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&status["accounts"])?);
            } else {
                print!("{}", accounts_table(&status));
            }
            Ok(0)
        }
        AccountsCmd::Add {
            name,
            provider,
            login: then_login,
            email,
            signin,
        } => {
            let c = client::require(paths).await?;
            let v = c
                .post(
                    "/api/v1/accounts",
                    json!({ "id": name, "provider": provider, "email": email }),
                )
                .await?;
            let mail = v["email"]
                .as_str()
                .map(|e| format!(", {e}"))
                .unwrap_or_default();
            println!(
                "added account '{}' ({}{mail})",
                v["id"].as_str().unwrap_or(&name),
                v["provider"].as_str().unwrap_or(&provider)
            );
            if !then_login {
                println!("sign in with: pooltime accounts login {name}");
                return Ok(0);
            }
            let saved = login(paths, Some(&c), &name, email.as_deref(), &signin).await?;
            Ok(if saved { 0 } else { 1 })
        }
        AccountsCmd::Login(args) => {
            if let Some(hint) = &args.spike_login_hint {
                return spike_login_hint(paths, hint, &args.signin).await;
            }
            let name = args
                .name
                .as_deref()
                .context("an account name is required")?;
            let c = client::find(paths).await?;
            let saved = login(paths, c.as_ref(), name, args.email.as_deref(), &args.signin).await?;
            Ok(if saved { 0 } else { 1 })
        }
        AccountsCmd::Relogin {
            expiring,
            dead,
            all,
            signin,
        } => relogin(paths, expiring, dead, all, &signin).await,
        AccountsCmd::Remove { name, yes } => {
            let c = client::require(paths).await?;
            if !yes
                && !client::confirm(&format!(
                    "Remove account '{name}' and its saved credentials?"
                ))
                .await?
            {
                eprintln!("not removed");
                return Ok(1);
            }
            c.delete(&format!("/api/v1/accounts/{}", seg(&name)))
                .await?;
            println!("removed account '{name}'");
            Ok(0)
        }
        AccountsCmd::Set {
            name,
            priority,
            weight,
            until,
            clear_override,
        } => {
            let c = client::require(paths).await?;
            let mut body = serde_json::Map::new();
            if let Some(p) = priority {
                body.insert("priority".into(), json!(p));
            }
            if let Some(w) = weight {
                body.insert("weight".into(), json!(w));
            }
            if let Some(u) = until {
                body.insert(
                    "until".into(),
                    json!(parse_until(&u, OffsetDateTime::now_utc())?),
                );
            }
            if clear_override {
                body.insert("until".into(), Value::Null);
            }
            let v = c
                .post(
                    &format!("/api/v1/accounts/{}/routing", seg(&name)),
                    Value::Object(body),
                )
                .await?;
            let over = match &v["weight_override"] {
                Value::Object(o) => format!(
                    ", weight {} until {}",
                    o["weight"],
                    o["until"].as_str().unwrap_or("?")
                ),
                _ => String::new(),
            };
            println!(
                "'{name}': priority {}, weight {}{over}",
                v["priority"], v["weight"]
            );
            Ok(0)
        }
    }
}

/// `--until`: an RFC 3339 time, or a duration from `now` (`90m`, `2h`, `3d`).
fn parse_until(raw: &str, now: OffsetDateTime) -> anyhow::Result<String> {
    if OffsetDateTime::parse(raw, &Rfc3339).is_ok() {
        return Ok(raw.to_string());
    }
    let d = humantime_serde::re::humantime::parse_duration(raw).map_err(|_| {
        anyhow!("--until must be an RFC 3339 time (2026-11-14T09:00:00Z) or a duration (2h, 3d)")
    })?;
    let t = now + time::Duration::try_from(d).context("--until is too far away")?;
    Ok(t.format(&Rfc3339)?)
}

fn mode_of(flags: &SignInFlags) -> Mode {
    flags.mode.unwrap_or(if client::headless() {
        Mode::Paste
    } else {
        Mode::Callback
    })
}

fn signin_mode(m: Mode) -> SignInMode {
    match m {
        Mode::Callback => SignInMode::Callback,
        Mode::Paste => SignInMode::PasteCode,
    }
}

/// Show the authorize URL (and try the browser).
fn present(url: &str, mode: Mode) {
    let opened = client::open_browser(url);
    match (mode, opened) {
        (Mode::Callback, true) => {
            println!("Opened your browser to sign in. If it did not open, visit:")
        }
        (Mode::Callback, false) => {
            println!("Open this URL in a browser on this machine to sign in:")
        }
        (Mode::Paste, _) => {
            println!("Open this URL in any browser, sign in, then paste the code it shows:")
        }
    }
    println!("  {url}");
    if mode == Mode::Callback {
        println!("Waiting for the sign-in to finish (up to 5 minutes)...");
    }
}

/// How a sign-in ended, as the CLI reports it.
enum Outcome {
    Saved(Identity),
    NeedsBinding(Identity),
    Mismatch(String),
    Failed(String),
}

fn identity_from(v: &Value) -> Identity {
    Identity {
        account_uuid: None,
        email: v["email"].as_str().map(str::to_owned),
        org_uuid: None,
    }
}

fn email_of(i: &Identity) -> &str {
    i.email.as_deref().unwrap_or("an account without an email")
}

/// The two places a sign-in can run.
enum Backend<'a> {
    Daemon { client: &'a Client, signin: String },
    Local { handle: SignInHandle },
}

impl Backend<'_> {
    async fn submit(&self, code: &str) -> Result<(), String> {
        match self {
            Backend::Daemon { client, signin } => client
                .post(
                    &format!("/api/v1/signin/{}/code", seg(signin)),
                    json!({ "code": code }),
                )
                .await
                .map(|_| ())
                .map_err(|e| format!("{e:#}")),
            Backend::Local { handle } => handle.submit_code(code).map_err(|e| e.to_string()),
        }
    }

    async fn bind(&self) -> anyhow::Result<Identity> {
        match self {
            Backend::Daemon { client, signin } => {
                let v = client
                    .post(&format!("/api/v1/signin/{}/bind", seg(signin)), json!({}))
                    .await?;
                Ok(identity_from(&v["identity"]))
            }
            Backend::Local { handle } => handle.bind().await,
        }
    }
}

/// Read pasted codes until one is accepted (`--code` is tried once, with no prompt).
async fn paste(backend: &Backend<'_>, flags: &SignInFlags) -> anyhow::Result<()> {
    if let Some(code) = &flags.code {
        return backend.submit(code).await.map_err(|e| anyhow!(e));
    }
    for _ in 0..3 {
        print!("Code: ");
        std::io::Write::flush(&mut std::io::stdout())?;
        let Some(line) = client::read_line().await? else {
            println!();
            bail!("no code entered; sign-in cancelled");
        };
        if line.is_empty() {
            continue;
        }
        match backend.submit(&line).await {
            Ok(()) => return Ok(()),
            Err(e) => eprintln!("{e}"),
        }
    }
    bail!("no valid code entered; sign-in cancelled")
}

/// Poll the daemon's view of a sign-in until it ends.
async fn wait_daemon(c: &Client, signin: &str) -> anyhow::Result<Outcome> {
    let path = format!("/api/v1/signin/{}", seg(signin));
    loop {
        let v = c.get(&path).await?;
        let msg = |v: &Value| {
            v["message"]
                .as_str()
                .unwrap_or("sign-in failed")
                .to_string()
        };
        match v["status"].as_str() {
            Some("pending") => tokio::time::sleep(Duration::from_millis(300)).await,
            Some("saved") => return Ok(Outcome::Saved(identity_from(&v["identity"]))),
            Some("needs_binding") => return Ok(Outcome::NeedsBinding(identity_from(&v["got"]))),
            Some("mismatch") => return Ok(Outcome::Mismatch(msg(&v))),
            _ => return Ok(Outcome::Failed(msg(&v))),
        }
    }
}

fn outcome_of(account: &AccountId, r: SignInResult) -> Outcome {
    match r {
        SignInResult::Saved { identity } => Outcome::Saved(identity),
        SignInResult::NeedsBinding { got } => Outcome::NeedsBinding(got),
        SignInResult::Mismatch { expected, got } => Outcome::Mismatch(
            crate::server::mgmt::mismatch_message(account, &expected, &got),
        ),
        SignInResult::Failed(m) => Outcome::Failed(m),
    }
}

/// Sign `name` in, through the daemon when `client` is given, else in-process. Returns whether
/// credentials were saved.
pub async fn login(
    paths: &Paths,
    client: Option<&Client>,
    name: &str,
    hint: Option<&str>,
    flags: &SignInFlags,
) -> anyhow::Result<bool> {
    let account: AccountId = name.parse()?;
    let mode = mode_of(flags);
    match client {
        Some(c) => {
            let mut body = json!({
                "mode": match mode { Mode::Callback => "callback", Mode::Paste => "paste" },
            });
            if let Some(h) = hint {
                body["hint"] = json!(h);
            }
            let v = c
                .post(&format!("/api/v1/accounts/{}/login", seg(name)), body)
                .await?;
            let signin = v["signin_id"]
                .as_str()
                .context("pooltime returned no sign-in id")?
                .to_string();
            present(v["authorize_url"].as_str().unwrap_or_default(), mode);
            let backend = Backend::Daemon {
                client: c,
                signin: signin.clone(),
            };
            if mode == Mode::Paste {
                paste(&backend, flags).await?;
            }
            let outcome = wait_daemon(c, &signin).await?;
            conclude(name, &backend, outcome, flags).await
        }
        None => {
            let local = Local::open(paths)?;
            local.ensure_account(&account)?;
            let pending = local.flow().start(&account, hint, signin_mode(mode))?;
            present(&pending.authorize_url, mode);
            let backend = Backend::Local {
                handle: pending.handle(),
            };
            let outcome = finish_local(&account, pending, &backend, mode, flags).await?;
            let saved = conclude(name, &backend, outcome, flags).await?;
            // The login cleared a dead-refresh marker: make sure it is on disk before exit.
            local.state.flush().await?;
            Ok(saved)
        }
    }
}

async fn finish_local(
    account: &AccountId,
    pending: PendingSignIn,
    backend: &Backend<'_>,
    mode: Mode,
    flags: &SignInFlags,
) -> anyhow::Result<Outcome> {
    if mode == Mode::Paste {
        paste(backend, flags).await?;
    }
    Ok(outcome_of(account, pending.wait().await))
}

/// Report the outcome; confirm (or `--yes`) and bind a first sign-in.
async fn conclude(
    name: &str,
    backend: &Backend<'_>,
    outcome: Outcome,
    flags: &SignInFlags,
) -> anyhow::Result<bool> {
    match outcome {
        Outcome::Saved(i) => {
            println!("Signed in: '{name}' is {}", email_of(&i));
            Ok(true)
        }
        Outcome::NeedsBinding(got) => {
            println!("Signed in as {}.", email_of(&got));
            let yes = flags.yes
                || client::confirm(&format!("Use this login for account '{name}'?")).await?;
            if !yes {
                println!("Not saved.");
                return Ok(false);
            }
            let i = backend.bind().await?;
            println!("Signed in: '{name}' is {}", email_of(&i));
            Ok(true)
        }
        Outcome::Mismatch(m) => {
            eprintln!("{m}");
            Ok(false)
        }
        Outcome::Failed(m) => {
            eprintln!("sign-in failed: {m}");
            Ok(false)
        }
    }
}

/// What an in-process sign-in needs (no daemon is running, so nothing else owns these files).
struct Local {
    cfg: Config,
    registry: Arc<Registry>,
    state: Arc<StateBook>,
}

impl Local {
    fn open(paths: &Paths) -> anyhow::Result<Local> {
        paths
            .ensure()
            .context("creating the pooltime data directory")?;
        let cfg = Config::load(paths)?;
        let registry = Arc::new(Registry::load(paths)?);
        let store = Arc::new(Store::open(&paths.db)?);
        let state = Arc::new(StateBook::load(store)?);
        Ok(Local {
            cfg,
            registry,
            state,
        })
    }

    /// Create `id` as a Claude account when missing; refuse other providers.
    fn ensure_account(&self, id: &AccountId) -> anyhow::Result<()> {
        match self.registry.get(id) {
            Some(m) if m.provider != ProviderId::Anthropic => {
                bail!("sign-in for {} accounts is not yet supported", m.provider)
            }
            Some(_) => Ok(()),
            None => self
                .registry
                .upsert(AccountMeta::new(id.clone(), ProviderId::Anthropic)),
        }
    }

    fn flow(&self) -> ClaudeSignIn {
        ClaudeSignIn::new(
            self.cfg.anthropic.clone(),
            self.registry.clone(),
            self.state.clone(),
            Arc::new(Upstream::new()),
        )
    }
}

/// `accounts login --spike-login-hint <email>`: sign in with `login_hint=<email>` against a
/// throwaway account in a temp home and print the identity that came back. The credentials are
/// held in memory only (never bound) and the temp home is deleted.
async fn spike_login_hint(paths: &Paths, hint: &str, flags: &SignInFlags) -> anyhow::Result<i32> {
    let cfg = Config::load(paths)?;
    let scratch = tempfile::tempdir().context("creating a scratch home")?;
    let sp = Paths::under(scratch.path());
    sp.ensure()?;
    let registry = Arc::new(Registry::load(&sp)?);
    let id: AccountId = "spike".parse()?;
    registry.upsert(AccountMeta::new(id.clone(), ProviderId::Anthropic))?;
    let state = Arc::new(StateBook::load(Arc::new(Store::open(&sp.db)?))?);
    let flow = ClaudeSignIn::new(cfg.anthropic, registry, state, Arc::new(Upstream::new()));
    let mode = mode_of(flags);
    let pending = flow.start(&id, Some(hint), signin_mode(mode))?;
    println!("Spike: login_hint={hint} (nothing will be saved)");
    present(&pending.authorize_url, mode);
    let backend = Backend::Local {
        handle: pending.handle(),
    };
    let outcome = finish_local(&id, pending, &backend, mode, flags).await?;
    let got = match outcome {
        Outcome::NeedsBinding(got) | Outcome::Saved(got) => got,
        Outcome::Mismatch(m) | Outcome::Failed(m) => {
            eprintln!("sign-in failed: {m}");
            return Ok(1);
        }
    };
    let show = |v: &Option<String>| v.clone().unwrap_or_else(|| "-".into());
    println!(
        "login_hint {hint} came back as: email {}, account {}, org {}",
        show(&got.email),
        show(&got.account_uuid),
        show(&got.org_uuid)
    );
    let same = got
        .email
        .as_deref()
        .is_some_and(|e| e.eq_ignore_ascii_case(hint));
    println!(
        "{}",
        if same {
            "the hinted account signed in"
        } else {
            "a DIFFERENT account signed in than the hint"
        }
    );
    drop(scratch);
    Ok(0)
}

/// Which accounts `relogin` walks, from the status document at its `now`.
fn select_relogin(status: &Value, expiring: Option<u32>, dead: bool, all: bool) -> Vec<String> {
    let now = status["now"]
        .as_str()
        .and_then(|s| OffsetDateTime::parse(s, &Rfc3339).ok())
        .unwrap_or_else(OffsetDateTime::now_utc);
    status["accounts"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|a| a["provider"] == "anthropic")
        .filter(|a| {
            let needs_login = a["status"] == "needs_login";
            let expires_within = |days: u32| {
                a["refresh_expires_at"]
                    .as_str()
                    .and_then(|s| OffsetDateTime::parse(s, &Rfc3339).ok())
                    .is_some_and(|t| t <= now + time::Duration::days(i64::from(days)))
            };
            all || (dead && needs_login)
                || expiring.is_some_and(|d| !needs_login && expires_within(d))
        })
        .filter_map(|a| a["id"].as_str().map(str::to_owned))
        .collect()
}

/// A pasted code belongs to exactly one sign-in (its `state`), so it cannot drive a batch.
fn check_relogin_flags(flags: &SignInFlags) -> anyhow::Result<()> {
    if flags.code.is_some() {
        bail!(
            "--code belongs to a single sign-in; use it with `accounts login <name>`, not relogin"
        );
    }
    Ok(())
}

async fn relogin(
    paths: &Paths,
    expiring: Option<u32>,
    dead: bool,
    all: bool,
    flags: &SignInFlags,
) -> anyhow::Result<i32> {
    check_relogin_flags(flags)?;
    let c = client::require(paths).await?;
    let status = c.get("/api/v1/status").await?;
    let ids = select_relogin(&status, expiring, dead, all);
    if ids.is_empty() {
        println!("no accounts to re-login");
        return Ok(0);
    }
    let mut saved = 0;
    for (i, id) in ids.iter().enumerate() {
        println!("\n[{}/{}] {id}", i + 1, ids.len());
        match login(paths, Some(&c), id, None, flags).await {
            Ok(true) => saved += 1,
            Ok(false) => {}
            Err(e) => eprintln!("{id}: {e:#}"),
        }
    }
    println!("\nre-logged in {saved} of {} accounts", ids.len());
    Ok(if saved == ids.len() { 0 } else { 1 })
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::datetime;

    #[test]
    fn until_accepts_rfc3339_and_durations() {
        let now = datetime!(2026-11-07 12:00 UTC);
        assert_eq!(
            parse_until("2026-11-14T09:00:00Z", now).unwrap(),
            "2026-11-14T09:00:00Z"
        );
        assert_eq!(parse_until("2h", now).unwrap(), "2026-11-07T14:00:00Z");
        assert_eq!(parse_until("3d", now).unwrap(), "2026-11-10T12:00:00Z");
        assert!(parse_until("soon", now).is_err());
    }

    #[test]
    fn relogin_refuses_a_code() {
        assert!(check_relogin_flags(&SignInFlags::default()).is_ok());
        let flags = SignInFlags {
            code: Some("c#s".into()),
            ..SignInFlags::default()
        };
        let e = check_relogin_flags(&flags).unwrap_err().to_string();
        assert!(e.contains("--code"), "{e}");
    }

    #[test]
    fn relogin_selection() {
        let s = json!({
            "now": "2026-11-07T12:00:00Z",
            "accounts": [
                {"id": "dead", "provider": "anthropic", "status": "needs_login",
                 "refresh_expires_at": null},
                {"id": "soon", "provider": "anthropic", "status": "ready",
                 "refresh_expires_at": "2026-11-09T12:00:00Z"},
                {"id": "later", "provider": "anthropic", "status": "ready",
                 "refresh_expires_at": "2026-12-09T12:00:00Z"},
                {"id": "codex", "provider": "openai", "status": "needs_login"}
            ]
        });
        assert_eq!(select_relogin(&s, None, true, false), ["dead"]);
        assert_eq!(select_relogin(&s, Some(3), false, false), ["soon"]);
        assert_eq!(
            select_relogin(&s, Some(1), false, false),
            Vec::<String>::new()
        );
        assert_eq!(select_relogin(&s, Some(3), true, false), ["dead", "soon"]);
        assert_eq!(
            select_relogin(&s, None, false, true),
            ["dead", "soon", "later"]
        );
    }
}
