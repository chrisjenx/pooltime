//! `pooltime service install|uninstall|status|restart`: run the daemon as a background user
//! service (launchd on macOS, systemd on Linux). `uninstall` first releases every connected
//! Claude Code profile, offline from the files, so it works whether or not the daemon is up and
//! leaves no profile pointing at a daemon that is gone (spec §16).
//!
//! `install` over a running service moves it onto this binary without closing the port when the
//! definition is unchanged and the supervisor can replace itself in place
//! ([`crate::service::install`]); otherwise it is a full restart, which it warns about and (on a
//! terminal) asks before.

use std::io::Write;

use clap::{Args, Subcommand};

use crate::claude_code::{disconnect, profile_of};
use crate::config::{Config, Paths};
use crate::fsutil::WriteOutcome;
use crate::service::install::{self as plan, Plan, Seamless};
use crate::service::upgrade::{self, Live, SystemLive, Upgrade};
use crate::service::{Backend, Env, Restarted, Runner, SystemRunner};
use crate::supervisor::proto::{live_state, SupervisorState};
use crate::version::Version;

#[derive(Debug, Subcommand)]
pub enum ServiceCmd {
    /// Install and start the background service (a running one is updated in place when it can be).
    ///
    /// Over a running service whose definition is unchanged and whose supervisor can replace
    /// itself (1.3+), this moves it onto this binary without closing the port: the supervisor
    /// replaces itself in place (also when both already run this version, so a rebuilt binary is
    /// what runs), or a new worker takes over if only the worker's version differs. Otherwise (a changed
    /// definition, binary path, host or port, an older supervisor, or `--restart`) the definition
    /// is replaced and the service stopped and started: in-flight requests drain for up to 10
    /// minutes and new ones get errors meanwhile, so it asks first (`--yes` skips the question
    /// and is required without a terminal).
    Install(InstallArgs),
    /// Disconnect every Claude Code profile, then stop and remove the service.
    Uninstall,
    /// Is the service installed and running?
    Status,
    /// Restart the daemon without closing the port: a new worker (the installed binary) takes
    /// new requests while the old one finishes its in-flight ones.
    Restart,
    /// systemd's `ExecReload`: send the supervisor (`$MAINPID`) its restart signal.
    #[command(hide = true)]
    SignalReload {
        /// The supervisor's pid.
        pid: u32,
    },
}

#[derive(Debug, Clone, Default, Args)]
pub struct InstallArgs {
    /// Stop and start the running service even when it could be moved onto this binary in
    /// place (in-flight requests drain for up to 10 minutes; new ones get errors meanwhile).
    #[arg(long)]
    pub restart: bool,
    /// Do not ask before a full restart (required without a terminal when a running service
    /// would be stopped).
    #[arg(long, short = 'y')]
    pub yes: bool,
}

/// The questions `service install` may ask.
pub trait Ask {
    /// stdin is a terminal.
    fn tty(&self) -> bool;
    /// Ask a yes/no question (default no).
    fn confirm(&self, question: &str) -> anyhow::Result<bool>;
}

/// The terminal.
pub struct Terminal;

impl Ask for Terminal {
    fn tty(&self) -> bool {
        use std::io::IsTerminal;
        std::io::stdin().is_terminal()
    }

    fn confirm(&self, question: &str) -> anyhow::Result<bool> {
        print!("{question} [y/N] ");
        std::io::stdout().flush()?;
        let mut line = String::new();
        if std::io::stdin().read_line(&mut line)? == 0 {
            println!();
        }
        Ok(matches!(
            line.trim().to_ascii_lowercase().as_str(),
            "y" | "yes"
        ))
    }
}

pub fn run(paths: &Paths, cmd: ServiceCmd) -> anyhow::Result<i32> {
    if let ServiceCmd::SignalReload { pid } = cmd {
        crate::service::signal_reload(pid)?;
        return Ok(0);
    }
    let backend = Backend::native()?;
    let env = Env::from_process(paths)?;
    run_with(backend, &env, &SystemRunner, cmd)
}

pub fn run_with(
    backend: Backend,
    env: &Env,
    r: &dyn Runner,
    cmd: ServiceCmd,
) -> anyhow::Result<i32> {
    match cmd {
        ServiceCmd::SignalReload { pid } => {
            crate::service::signal_reload(pid)?;
            Ok(0)
        }
        ServiceCmd::Install(args) => {
            let live = SystemLive {
                paths: env.paths.clone(),
                timing: env.timing,
            };
            install(
                backend,
                env,
                r,
                &live,
                &Terminal,
                &args,
                &mut std::io::stdout(),
            )
        }
        ServiceCmd::Uninstall => uninstall(backend, env, r),
        ServiceCmd::Status => {
            let st = backend.status(env, r)?;
            let state = match (st.installed, st.running) {
                (_, true) => "running",
                (true, false) => "installed, not running",
                (false, false) => "not installed",
            };
            println!("pooltime service: {state}");
            if st.installed {
                println!("definition: {}", backend.definition_path(env).display());
            }
            if !st.detail.is_empty() {
                println!("{}", st.detail);
            }
            if st.running {
                if let Some(sup) = live_state(&env.paths) {
                    println!("{}", supervisor_line(&sup));
                }
            }
            if backend.supervised(env) == Some(false) {
                println!("{OLD_DEFINITION}");
            }
            Ok(if st.running { 0 } else { 1 })
        }
        ServiceCmd::Restart => {
            match crate::service::restart(backend, env, r)? {
                Restarted::HandedOver { state: sup, joined } => {
                    report_handover(&mut std::io::stdout(), &sup, joined)?;
                }
                Restarted::Started => println!("restarted the pooltime service"),
                Restarted::Reinstalled => println!(
                    "reinstalled and restarted the pooltime service; from now on \
                     `pooltime service restart` hands over without a pause"
                ),
            }
            Ok(0)
        }
    }
}

/// What a successful worker handover looks like (`service restart`, and `service install` when
/// only the worker was behind).
fn report_handover(
    out: &mut dyn Write,
    sup: &SupervisorState,
    joined: bool,
) -> std::io::Result<()> {
    if joined {
        writeln!(
            out,
            "a restart was already in progress, so this request started nothing new; that \
             restart finished:"
        )?;
    }
    writeln!(
        out,
        "restarted without closing the port: generation {} (pid {}, version {}) serves new \
         requests",
        sup.generation,
        sup.worker_pid.map_or("?".into(), |p| p.to_string()),
        sup.worker_version.as_deref().unwrap_or("?"),
    )?;
    if !sup.draining.is_empty() {
        writeln!(
            out,
            "the previous generation finishes its in-flight requests, then exits"
        )?;
    }
    if sup
        .worker_version
        .as_deref()
        .is_some_and(|v| v != sup.version)
    {
        if sup.can_upgrade {
            writeln!(
                out,
                "note: the supervisor itself stays at {}; `pooltime update` (or `pooltime \
                 service install`) moves it onto the new binary without closing the port",
                sup.version
            )?;
        } else {
            writeln!(
                out,
                "note: the supervisor itself stays at {} until `pooltime service install` (a \
                 full restart); that is fine",
                sup.version
            )?;
        }
    }
    Ok(())
}

/// `pooltime service install`: see the module docs. Returns the exit code.
pub fn install(
    backend: Backend,
    env: &Env,
    r: &dyn Runner,
    live: &dyn Live,
    ask: &dyn Ask,
    args: &InstallArgs,
    out: &mut dyn Write,
) -> anyhow::Result<i32> {
    let version = env!("CARGO_PKG_VERSION");
    let obs = plan::observe(backend, env, r, live.supervisor())?;
    let path = backend.definition_path(env);
    match plan::plan(&obs, version, args.restart) {
        Plan::Start => {
            backend.install(env, r)?;
            writeln!(
                out,
                "installed and started the pooltime service ({})",
                path.display()
            )?;
            if backend == Backend::Systemd {
                writeln!(
                    out,
                    "to keep it running after you log out: loginctl enable-linger"
                )?;
            }
            Ok(0)
        }
        Plan::Full(why) => {
            writeln!(
                out,
                "a full restart is needed: {why}.\nwarning: this stops the running service. It \
                 finishes its in-flight requests first (up to 10 minutes); new requests get \
                 errors until the new one is up."
            )?;
            if !args.yes {
                if !ask.tty() {
                    writeln!(
                        out,
                        "nothing changed; pass --yes to restart it without asking"
                    )?;
                    return Ok(1);
                }
                out.flush()?;
                if !ask.confirm("Restart the pooltime service now?")? {
                    writeln!(out, "nothing changed")?;
                    return Ok(1);
                }
            }
            out.flush()?;
            backend.install(env, r)?;
            writeln!(
                out,
                "installed and restarted the pooltime service ({})",
                path.display()
            )?;
            Ok(0)
        }
        Plan::Seamless(how) => {
            let sup = obs
                .supervisor
                .expect("a seamless plan has a running supervisor");
            seamless(backend, env, r, live, how, &sup, &obs.bin, version, out)
        }
    }
}

/// Move the running supervisor `sup` onto this binary (`version`, at `bin`) without closing the
/// port, and check that it got there. Never falls back to a stop and start.
#[allow(clippy::too_many_arguments)]
fn seamless(
    backend: Backend,
    env: &Env,
    r: &dyn Runner,
    live: &dyn Live,
    how: Seamless,
    sup: &SupervisorState,
    bin: &str,
    version: &str,
    out: &mut dyn Write,
) -> anyhow::Result<i32> {
    let target = Version::parse(version).expect("this binary's version parses");
    if backend == Backend::Systemd {
        // The unit file is unchanged; this only makes sure it starts at login (as a fresh
        // install's `enable --now` does). Best effort: the running service is unaffected.
        if let Err(e) = crate::service::run_checked(
            r,
            "systemctl",
            &["--user", "enable", crate::service::systemd::UNIT],
        ) {
            writeln!(out, "warning: could not enable the unit: {e:#}")?;
        }
    }
    writeln!(
        out,
        "the service definition is unchanged ({})",
        backend.definition_path(env).display()
    )?;
    let off = |v: Option<&str>, b: Option<&str>| {
        format!(
            "the service now runs {} from {}, not this binary's {version}: is {bin} this \
             pooltime? `pooltime service status` shows what runs",
            v.unwrap_or("an unknown version"),
            b.unwrap_or("an unknown binary"),
        )
    };
    match how {
        Seamless::Handover => {
            writeln!(
                out,
                "starting a worker from {bin} ({version}) without closing the port (worker {})...",
                sup.worker_version.as_deref().unwrap_or("?")
            )?;
            out.flush()?;
            let after = match crate::service::handover(backend, env, r, sup) {
                Ok(Restarted::HandedOver { state, joined }) => {
                    report_handover(out, &state, joined)?;
                    state
                }
                Ok(other) => {
                    writeln!(out, "unexpected restart outcome: {other:?}")?;
                    return Ok(1);
                }
                Err(e) => {
                    writeln!(out, "error: {e:#}")?;
                    writeln!(
                        out,
                        "nothing was stopped; `pooltime service install --restart` does a full \
                         restart"
                    )?;
                    return Ok(1);
                }
            };
            if after
                .worker_version
                .as_deref()
                .and_then(Version::parse)
                .as_ref()
                != Some(&target)
            {
                writeln!(
                    out,
                    "{}",
                    off(after.worker_version.as_deref(), after.binary.as_deref())
                )?;
                return Ok(1);
            }
            Ok(0)
        }
        Seamless::Upgrade | Seamless::Reload => {
            if how == Seamless::Reload {
                writeln!(
                    out,
                    "the service already runs pooltime {version}; reloading it onto the \
                     installed binary {bin} without closing the port (a rebuild keeps the \
                     version)..."
                )?;
            } else {
                writeln!(
                    out,
                    "moving the service onto {version} without closing the port (supervisor {}, \
                     worker {})...",
                    sup.version,
                    sup.worker_version.as_deref().unwrap_or("?")
                )?;
            }
            out.flush()?;
            match upgrade::upgrade_in_place(live, sup)? {
                Upgrade::Vanished => {
                    writeln!(
                        out,
                        "the pooltime supervisor (pid {}) is gone or was replaced meanwhile; \
                         nothing was signalled. Run `pooltime service install` again.",
                        sup.pid
                    )?;
                    Ok(1)
                }
                Upgrade::NoVerdict => {
                    writeln!(
                        out,
                        "the supervisor did not report the upgrade in time; check `pooltime \
                         service status` and `pooltime logs`"
                    )?;
                    Ok(1)
                }
                Upgrade::Done { result, after } => {
                    writeln!(
                        out,
                        "service: {}",
                        upgrade::upgraded_line(sup, &result, &after)
                    )?;
                    if let Some(v) = upgrade::off_target(&after, &target) {
                        writeln!(out, "{}", off(v.as_deref(), after.binary.as_deref()))?;
                        return Ok(1);
                    }
                    // The new supervisor kept the old socket: it must be the configured one.
                    if let Err(e) =
                        plan::check_listen(after.listen.as_deref(), plan::configured_listen(env))
                    {
                        writeln!(out, "{e}")?;
                        return Ok(1);
                    }
                    if how == Seamless::Reload {
                        writeln!(
                            out,
                            "reloaded onto the installed binary: supervisor pid {} and the port \
                             kept, generation {} serves",
                            after.pid, after.generation
                        )?;
                    }
                    Ok(0)
                }
                Upgrade::Failed { error, after } => {
                    writeln!(
                        out,
                        "service: upgrade failed: {error}; it keeps serving (supervisor {}, \
                         worker {}). Nothing was stopped; `pooltime service install --restart` \
                         does a full restart",
                        after.version,
                        after.worker_version.as_deref().unwrap_or("?")
                    )?;
                    Ok(1)
                }
                Upgrade::NoResult => {
                    writeln!(out, "the supervisor reported no upgrade result")?;
                    Ok(1)
                }
            }
        }
    }
}

/// Shown wherever an installed definition from before seamless restarts is noticed.
pub const OLD_DEFINITION: &str = "note: the installed service predates seamless restarts; run \
     `pooltime service install` once (or `pooltime service restart`, which does it for you)";

/// One line about the supervisor and its worker generations.
pub fn supervisor_line(s: &SupervisorState) -> String {
    let mut line = format!(
        "supervisor: pid {} (v{}), generation {}",
        s.pid, s.version, s.generation
    );
    if let Some(pid) = s.worker_pid {
        line.push_str(&format!(
            ": pid {pid} (v{})",
            s.worker_version.as_deref().unwrap_or("?")
        ));
    }
    if !s.draining.is_empty() {
        line.push_str(&format!(
            ", {} older generation(s) draining",
            s.draining.len()
        ));
    }
    line
}

/// Disconnect all configured profiles first, then unload and remove the service. A profile that
/// cannot be restored is reported and makes the exit code 1, but the service is still removed
/// (the profile stays recorded, so `pooltime disconnect` can finish the job).
fn uninstall(backend: Backend, env: &Env, r: &dyn Runner) -> anyhow::Result<i32> {
    let cfg = Config::load_file(&env.paths)?;
    let mut code = 0;
    if !cfg.profiles.is_empty() {
        let targets: Vec<_> = cfg.profiles.iter().map(profile_of).collect();
        match disconnect(&env.paths, &targets) {
            Ok(outcomes) => {
                for (id, o) in outcomes {
                    if o == WriteOutcome::GaveUp {
                        eprintln!(
                            "warning: profile {id} NOT restored: its settings kept changing; \
                             retry `pooltime disconnect {id}`"
                        );
                        code = 1;
                    } else {
                        println!("disconnected {id}");
                    }
                }
            }
            Err(e) => {
                eprintln!("warning: some profiles were not restored: {e:#}");
                eprintln!("retry with `pooltime disconnect` once fixed");
                code = 1;
            }
        }
    }
    match backend.uninstall(env, r) {
        Ok(true) => println!("removed the pooltime service"),
        Ok(false) => println!("the pooltime service was not installed"),
        Err(e) => {
            eprintln!("error: {e:#}");
            eprintln!("the service definition was kept; fix the problem and retry");
            return Ok(1);
        }
    }
    Ok(code)
}
