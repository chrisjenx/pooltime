//! What `pooltime service install` does to a running service: move it onto this binary without
//! closing the port when it can, or stop and start it (the old way) when it must.
//!
//! The seamless path needs all of: the service runs; the definition this binary would write is
//! byte-identical to the installed one (and to what the manager has loaded: systemd's
//! `NeedDaemonReload`, launchd's arguments, environment and log paths); the
//! service's main process is a supervisor that can replace itself in place (1.3+), named in
//! `run/supervisor.json`, running the binary path the definition names, and listening on the
//! address `config.toml` asks for. Anything else (or `--restart`) is a full restart. The decision
//! ([`plan`]) is pure; [`observe`] gathers its inputs.

use std::net::{SocketAddr, TcpStream};
use std::time::Duration;

use super::{Backend, Env, Runner};
use crate::config::Config;
use crate::supervisor::proto::SupervisorState;

/// Everything [`plan`] looks at.
#[derive(Debug, Clone, Default)]
pub struct Observed {
    /// The installed definition, if any.
    pub installed: Option<String>,
    /// The definition this binary writes.
    pub rendered: String,
    /// The manager runs the service.
    pub running: bool,
    /// The service's main process, as the manager reports it.
    pub main_pid: Option<u32>,
    /// systemd: the unit file on disk is newer than the one loaded (`NeedDaemonReload`).
    pub stale: bool,
    /// The running supervisor (`run/supervisor.json`, live pid).
    pub supervisor: Option<SupervisorState>,
    /// The binary path the definition names.
    pub bin: String,
    /// The configured address is the one the service listens on; `None`: cannot tell.
    pub listen_matches: Option<bool>,
}

/// What `service install` will do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Plan {
    /// Nothing runs: write the definition, load and start it.
    Start,
    /// Keep the service running and move it onto this binary.
    Seamless(Seamless),
    /// Stop the running service and start it from the new definition, and why.
    Full(String),
}

/// How a running service is moved onto this binary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Seamless {
    /// The supervisor runs another version: it replaces itself in place (`SIGUSR2`), which
    /// also hands over to a worker from this binary.
    Upgrade,
    /// The supervisor runs this version but its worker does not: a new worker generation
    /// (`SIGHUP`, as `service restart`).
    Handover,
    /// Both already run this version, but possibly an older build of it (a rebuilt or
    /// reinstalled binary keeps its version): the supervisor replaces itself in place anyway
    /// (`SIGUSR2`), so what runs is the installed binary.
    Reload,
}

/// Decide what `service install` does, for this binary's `version`; `force` is `--restart`.
pub fn plan(o: &Observed, version: &str, force: bool) -> Plan {
    if !o.running {
        return Plan::Start;
    }
    if force {
        return Plan::Full("--restart was given".into());
    }
    if o.installed.as_deref() != Some(o.rendered.as_str()) {
        return Plan::Full(if o.installed.is_none() {
            "no service definition is installed".into()
        } else {
            "the service definition changed".into()
        });
    }
    if o.stale {
        return Plan::Full(
            "the loaded service differs from the definition on disk (changed and not reloaded)"
                .into(),
        );
    }
    let Some(sup) = &o.supervisor else {
        return Plan::Full(
            "the running service has no supervisor state (run/supervisor.json)".into(),
        );
    };
    if !sup.can_upgrade {
        return Plan::Full(format!(
            "the running supervisor ({}) predates in-place upgrades",
            sup.version
        ));
    }
    if o.main_pid != Some(sup.pid) {
        return Plan::Full(format!(
            "the service's main process ({}) is not the supervisor in run/supervisor.json (pid {})",
            o.main_pid.map_or("unknown".into(), |p| p.to_string()),
            sup.pid
        ));
    }
    if sup.binary.as_deref() != Some(o.bin.as_str()) {
        return Plan::Full(format!(
            "the running supervisor runs {}, not {}",
            sup.binary.as_deref().unwrap_or("an unknown binary"),
            o.bin
        ));
    }
    match o.listen_matches {
        Some(true) => {}
        Some(false) => {
            return Plan::Full(
                "the host or port in config.toml differs from the socket the service listens on"
                    .into(),
            )
        }
        None => {
            return Plan::Full("cannot tell whether the host or port in config.toml changed".into())
        }
    }
    if sup.version != version {
        Plan::Seamless(Seamless::Upgrade)
    } else if sup.worker_version.as_deref() != Some(version) {
        Plan::Seamless(Seamless::Handover)
    } else {
        Plan::Seamless(Seamless::Reload)
    }
}

/// Gather [`Observed`] from the files, the service manager and the supervisor's state.
pub fn observe(
    backend: Backend,
    env: &Env,
    r: &dyn Runner,
    supervisor: Option<SupervisorState>,
) -> anyhow::Result<Observed> {
    let bin = env.service_bin()?;
    let rendered = backend.render(&bin, &env.paths);
    let installed = std::fs::read_to_string(backend.definition_path(env)).ok();
    let running = backend.status(env, r)?.running;
    let (main_pid, stale) = if running {
        backend.loaded(env, r, &bin)
    } else {
        (None, false)
    };
    let listen_matches = supervisor.as_ref().and_then(|s| listen_matches(env, s));
    Ok(Observed {
        installed,
        rendered,
        running,
        main_pid,
        stale,
        supervisor,
        bin: bin.to_string_lossy().into_owned(),
        listen_matches,
    })
}

/// The address `config.toml` asks for, as the service reads it (no shell `POOLTIME_*`
/// overrides); `None` when it does not load.
pub fn configured_listen(env: &Env) -> Option<SocketAddr> {
    let cfg = Config::load_with_env(&env.paths, |_| None).ok()?;
    Some(SocketAddr::new(cfg.host, cfg.port))
}

/// After an in-place upgrade the new supervisor records the address of the socket it inherited
/// (`listen`): it must be the configured one. A 1.3.0 supervisor was only checked by a
/// connection to the configured address beforehand, which cannot tell `0.0.0.0` from
/// `127.0.0.1` or the service from another listener; this settles it.
pub fn check_listen(listen: Option<&str>, want: Option<SocketAddr>) -> Result<(), String> {
    const FIX: &str = "run `pooltime service install --restart`";
    let Some(want) = want else {
        return Err(format!(
            "config.toml could not be read to check the address the service listens on; fix it \
             (`pooltime doctor`), then {FIX} if its host or port changed"
        ));
    };
    let Some(listen) = listen else {
        return Err(format!(
            "the supervisor did not report the address it listens on (see `pooltime logs`); if \
             config.toml's host or port ({want}) changed, {FIX}"
        ));
    };
    if listen.parse::<SocketAddr>().ok() == Some(want) {
        return Ok(());
    }
    Err(format!(
        "the service still listens on {listen} but config.toml says {want}; {FIX}"
    ))
}

/// Whether the supervisor listens where `config.toml` (as the service reads it: no shell
/// `POOLTIME_*` overrides) asks. A supervisor from 1.3.0 does not record its address: then the
/// configured one must accept a connection.
fn listen_matches(env: &Env, sup: &SupervisorState) -> Option<bool> {
    let want = configured_listen(env)?;
    match sup.listen.as_deref() {
        Some(addr) => Some(addr.parse::<SocketAddr>().ok()? == want),
        None => {
            let probe = crate::server::health::probe_addr(want.ip(), want.port());
            Some(TcpStream::connect_timeout(&probe, Duration::from_secs(1)).is_ok())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const V: &str = "1.4.0";

    fn sup(version: &str, worker: &str) -> SupervisorState {
        SupervisorState {
            pid: 4242,
            version: version.into(),
            generation: 3,
            worker_pid: Some(4243),
            worker_version: Some(worker.into()),
            can_upgrade: true,
            binary: Some("/opt/homebrew/bin/pooltime".into()),
            listen: Some("127.0.0.1:3460".into()),
            ..SupervisorState::default()
        }
    }

    fn running(s: SupervisorState) -> Observed {
        Observed {
            installed: Some("<plist/>".into()),
            rendered: "<plist/>".into(),
            running: true,
            main_pid: Some(s.pid),
            stale: false,
            supervisor: Some(s),
            bin: "/opt/homebrew/bin/pooltime".into(),
            listen_matches: Some(true),
        }
    }

    #[test]
    fn same_definition_and_an_upgradable_supervisor_is_seamless() {
        let o = running(sup("1.3.0", "1.3.0"));
        assert_eq!(plan(&o, V, false), Plan::Seamless(Seamless::Upgrade));
        // Supervisor current, worker not: a handover is enough.
        let o = running(sup(V, "1.3.0"));
        assert_eq!(plan(&o, V, false), Plan::Seamless(Seamless::Handover));
        let o = running(sup(V, V));
        assert_eq!(plan(&o, V, false), Plan::Seamless(Seamless::Reload));
        // A worker of unknown version gets a handover too.
        let mut s = sup(V, V);
        s.worker_version = None;
        assert_eq!(
            plan(&running(s), V, false),
            Plan::Seamless(Seamless::Handover)
        );
    }

    #[test]
    fn the_upgraded_supervisor_must_listen_where_the_config_says() {
        let want: SocketAddr = "127.0.0.1:3460".parse().unwrap();
        assert_eq!(check_listen(Some("127.0.0.1:3460"), Some(want)), Ok(()));
        let err = check_listen(Some("0.0.0.0:3460"), Some(want)).unwrap_err();
        assert!(
            err.contains("still listens on 0.0.0.0:3460 but config.toml says 127.0.0.1:3460"),
            "{err}"
        );
        assert!(err.contains("service install --restart"), "{err}");
        let err = check_listen(Some("127.0.0.1:3999"), Some(want)).unwrap_err();
        assert!(err.contains("still listens on 127.0.0.1:3999"), "{err}");
        let err = check_listen(None, Some(want)).unwrap_err();
        assert!(err.contains("did not report the address"), "{err}");
        let err = check_listen(Some("127.0.0.1:3460"), None).unwrap_err();
        assert!(err.contains("could not be read"), "{err}");
    }

    #[test]
    fn nothing_running_is_a_plain_install() {
        let o = Observed {
            running: false,
            ..running(sup("1.3.0", "1.3.0"))
        };
        assert_eq!(plan(&o, V, false), Plan::Start);
        assert_eq!(plan(&o, V, true), Plan::Start);
        assert_eq!(plan(&Observed::default(), V, false), Plan::Start);
    }

    #[test]
    fn anything_unsafe_is_a_full_restart() {
        let full = |o: Observed, why: &str| match plan(&o, V, false) {
            Plan::Full(r) => assert!(r.contains(why), "{r}"),
            other => panic!("{other:?} for {why}"),
        };
        let base = || running(sup("1.3.0", "1.3.0"));

        full(
            Observed {
                installed: Some("<plist old/>".into()),
                ..base()
            },
            "definition changed",
        );
        full(
            Observed {
                installed: None,
                ..base()
            },
            "no service definition",
        );
        full(
            Observed {
                stale: true,
                ..base()
            },
            "loaded service differs",
        );
        full(
            Observed {
                supervisor: None,
                ..base()
            },
            "no supervisor state",
        );
        let mut old = sup("1.2.0", "1.3.0");
        old.can_upgrade = false;
        full(running(old), "(1.2.0) predates in-place upgrades");
        full(
            Observed {
                main_pid: Some(99),
                ..base()
            },
            "main process (99)",
        );
        full(
            Observed {
                main_pid: None,
                ..base()
            },
            "main process (unknown)",
        );
        let mut elsewhere = sup("1.3.0", "1.3.0");
        elsewhere.binary = Some("/home/u/.local/bin/pooltime".into());
        full(
            running(elsewhere),
            "runs /home/u/.local/bin/pooltime, not /opt/homebrew/bin/pooltime",
        );
        full(
            Observed {
                listen_matches: Some(false),
                ..base()
            },
            "host or port in config.toml differs",
        );
        full(
            Observed {
                listen_matches: None,
                ..base()
            },
            "cannot tell",
        );
        // --restart forces it even when it could be seamless.
        assert_eq!(
            plan(&running(sup(V, V)), V, true),
            Plan::Full("--restart was given".into())
        );
    }
}
