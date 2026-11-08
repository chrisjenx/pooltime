//! `pooltime service install|uninstall|status|restart`: run the daemon as a background user
//! service (launchd on macOS, systemd on Linux). `uninstall` first releases every connected
//! Claude Code profile, offline from the files, so it works whether or not the daemon is up and
//! leaves no profile pointing at a daemon that is gone (spec §16).

use clap::Subcommand;

use crate::claude_code::{disconnect, profile_of};
use crate::config::{Config, Paths};
use crate::fsutil::WriteOutcome;
use crate::service::{Backend, Env, Runner, SystemRunner};

#[derive(Debug, Subcommand)]
pub enum ServiceCmd {
    /// Install and start the background service (re-installing replaces and reloads it).
    Install,
    /// Disconnect every Claude Code profile, then stop and remove the service.
    Uninstall,
    /// Is the service installed and running?
    Status,
    /// Restart the running service.
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
            Ok(if st.running { 0 } else { 1 })
        }
        ServiceCmd::Restart => {
            backend.restart(env, r)?;
            println!("restarted the pooltime service");
            Ok(0)
        }
    }
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
    if backend.uninstall(env, r)? {
        println!("removed the pooltime service");
    } else {
        println!("the pooltime service was not installed");
    }
    Ok(code)
}
