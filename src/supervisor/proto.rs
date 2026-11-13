//! What the supervisor and its workers say to each other, and the state file the CLI reads.
//!
//! The control channel is one line per message over an anonymous `socketpair` (no filesystem
//! path, so no other process can connect). It must stay backward compatible: a supervisor keeps
//! running across `brew upgrade` and talks to workers of newer versions. Unknown lines are
//! ignored by both sides.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::config::Paths;

/// Supervisor -> worker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToWorker {
    /// You are the serving generation: start the background tasks.
    Active,
    /// A newer generation serves now: stop accepting, stop the background tasks, answer
    /// [`FromWorker::Released`], drain and exit.
    Handoff,
    /// The service is stopping: drain (the port answers 503 meanwhile) and exit. A supervised
    /// worker ignores OS signals while the supervisor is there, so one `SIGTERM` that reaches
    /// both (a process-group signal, `pkill`) is never counted twice.
    Stop,
    /// Stop now, without waiting for in-flight requests (a second stop signal).
    Force,
}

/// Worker -> supervisor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FromWorker {
    /// Booted and accepting on the inherited listener.
    Ready { version: String },
    /// Background tasks stopped after a [`ToWorker::Handoff`].
    Released,
}

impl ToWorker {
    pub fn line(self) -> &'static str {
        match self {
            ToWorker::Active => "active\n",
            ToWorker::Handoff => "handoff\n",
            ToWorker::Stop => "stop\n",
            ToWorker::Force => "force\n",
        }
    }

    pub fn parse(line: &str) -> Option<ToWorker> {
        match line.trim() {
            "active" => Some(ToWorker::Active),
            "handoff" => Some(ToWorker::Handoff),
            "stop" => Some(ToWorker::Stop),
            "force" => Some(ToWorker::Force),
            _ => None,
        }
    }
}

impl FromWorker {
    pub fn line(&self) -> String {
        match self {
            FromWorker::Ready { version } => format!("ready {version}\n"),
            FromWorker::Released => "released\n".into(),
        }
    }

    pub fn parse(line: &str) -> Option<FromWorker> {
        let line = line.trim();
        match line.split_once(' ') {
            Some(("ready", v)) => Some(FromWorker::Ready {
                version: v.trim().to_string(),
            }),
            None if line == "ready" => Some(FromWorker::Ready {
                version: String::new(),
            }),
            None if line == "released" => Some(FromWorker::Released),
            _ => None,
        }
    }
}

/// `run/supervisor.json`: written by the supervisor on every change, removed when it exits.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SupervisorState {
    pub pid: u32,
    pub version: String,
    /// The serving generation (1 at start, +1 per worker started).
    pub generation: u64,
    pub worker_pid: Option<u32>,
    pub worker_version: Option<String>,
    /// Older generations still finishing their requests.
    #[serde(default)]
    pub draining: Vec<u32>,
    /// Restarts (`SIGHUP`) finished so far, successful or not: `service restart` waits for this
    /// to go up.
    #[serde(default)]
    pub restarts: u64,
    /// A restart is under way (a new generation booting or the old one releasing): another
    /// `SIGHUP` now is ignored.
    #[serde(default)]
    pub restarting: bool,
    pub last_restart: Option<RestartResult>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RestartResult {
    pub ok: bool,
    /// The generation serving afterwards.
    pub generation: u64,
    /// Why it failed (never upstream text or credentials).
    pub error: Option<String>,
}

pub fn state_path(paths: &Paths) -> PathBuf {
    paths.run.join("supervisor.json")
}

/// The state file, if present and readable.
pub fn read_state(paths: &Paths) -> Option<SupervisorState> {
    let raw = std::fs::read(state_path(paths)).ok()?;
    serde_json::from_slice(&raw).ok()
}

/// [`read_state`], but only if its supervisor process is still alive (a supervisor that was
/// killed leaves its file behind).
pub fn live_state(paths: &Paths) -> Option<SupervisorState> {
    read_state(paths).filter(|s| pid_alive(s.pid))
}

pub(crate) fn pid_alive(pid: u32) -> bool {
    let Ok(pid) = libc::pid_t::try_from(pid) else {
        return false;
    };
    if pid <= 0 {
        return false;
    }
    // SAFETY: signal 0 only checks that the process exists and may be signalled.
    unsafe { libc::kill(pid, 0) == 0 }
}

pub(crate) fn write_state(paths: &Paths, st: &SupervisorState) -> std::io::Result<()> {
    let bytes = serde_json::to_vec_pretty(st).expect("state serializes");
    crate::fsutil::atomic_write(&state_path(paths), &bytes, 0o600)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn messages_round_trip_and_unknown_lines_are_ignored() {
        for m in [
            ToWorker::Active,
            ToWorker::Handoff,
            ToWorker::Stop,
            ToWorker::Force,
        ] {
            assert_eq!(ToWorker::parse(m.line()), Some(m));
        }
        for m in [
            FromWorker::Ready {
                version: "1.2.3".into(),
            },
            FromWorker::Released,
        ] {
            assert_eq!(FromWorker::parse(&m.line()), Some(m));
        }
        assert_eq!(ToWorker::parse("reload\n"), None);
        assert_eq!(FromWorker::parse("hello there"), None);
        assert_eq!(
            FromWorker::parse("ready"),
            Some(FromWorker::Ready {
                version: String::new()
            })
        );
    }

    #[test]
    fn state_file_is_private_and_round_trips() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::under(dir.path());
        paths.ensure().unwrap();
        assert_eq!(read_state(&paths), None);
        let st = SupervisorState {
            pid: std::process::id(),
            version: "1.0.0".into(),
            generation: 2,
            worker_pid: Some(7),
            worker_version: Some("1.0.1".into()),
            draining: vec![6],
            restarts: 1,
            restarting: false,
            last_restart: Some(RestartResult {
                ok: true,
                generation: 2,
                error: None,
            }),
        };
        write_state(&paths, &st).unwrap();
        assert_eq!(read_state(&paths).as_ref(), Some(&st));
        assert_eq!(
            live_state(&paths).as_ref(),
            Some(&st),
            "this process is alive"
        );
        let mode = std::fs::metadata(state_path(&paths))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);

        // A dead supervisor's leftover file is not live.
        let dead = SupervisorState {
            pid: i32::MAX as u32,
            ..st
        };
        write_state(&paths, &dead).unwrap();
        assert!(read_state(&paths).is_some());
        assert_eq!(live_state(&paths), None);
    }
}
