//! `pooltime service install|uninstall|status|restart`: run the daemon as a background user
//! service (launchd on macOS, systemd on Linux). `uninstall` first releases every connected
//! Claude Code profile, offline from the files, so it works whether or not the daemon is up and
//! leaves no profile pointing at a daemon that is gone (spec §16).

use clap::Subcommand;

use crate::claude_code::{disconnect, profile_of};
use crate::config::{Config, Paths};
use crate::fsutil::WriteOutcome;
use crate::service::{Backend, Env, Restarted, Runner, SystemRunner};
use crate::supervisor::proto::{live_state, SupervisorState};

#[derive(Debug, Subcommand)]
pub enum ServiceCmd {
    /// Install and start the background service (re-installing replaces and reloads it).
    Install,
    /// Disconnect every Claude Code profile, then stop and remove the service.
    Uninstall,
    /// Is the service installed and running?
    Status,
    /// Restart the daemon without closing the port: a new worker (the installed binary) takes
    /// new requests while the old one finishes its in-flight ones.
    Restart,
}

pub fn run(paths: &Paths, cmd: ServiceCmd) -> anyhow::Result<i32> {
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
        ServiceCmd::Install => {
            backend.install(env, r)?;
            println!(
                "installed and started the pooltime service ({})",
                backend.definition_path(env).display()
            );
            if backend == Backend::Systemd {
                println!("to keep it running after you log out: loginctl enable-linger");
            }
            Ok(0)
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
                    if joined {
                        println!(
                            "a restart was already in progress, so this request started nothing \
                             new; that restart finished:"
                        );
                    }
                    println!(
                        "restarted without closing the port: generation {} (pid {}, version {}) \
                         serves new requests",
                        sup.generation,
                        sup.worker_pid.map_or("?".into(), |p| p.to_string()),
                        sup.worker_version.as_deref().unwrap_or("?"),
                    );
                    if !sup.draining.is_empty() {
                        println!(
                            "the previous generation finishes its in-flight requests, then exits"
                        );
                    }
                    if sup
                        .worker_version
                        .as_deref()
                        .is_some_and(|v| v != sup.version)
                    {
                        println!(
                            "note: the supervisor itself stays at {} until `pooltime service \
                             install` (a full restart); that is fine",
                            sup.version
                        );
                    }
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
