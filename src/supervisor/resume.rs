//! In-place upgrade of the supervisor (`SIGUSR2`, sent by `pooltime update`): the supervisor
//! `execve`s the new binary in its own process, so the pid that launchd / systemd track stays the
//! same, the listening socket never closes and the workers stay its children.
//!
//! What crosses the `exec`: the listening socket and each worker's control socket (their
//! `FD_CLOEXEC` cleared just before), and a [`Resume`] record as the hidden `--resume <json>`
//! argument (descriptor numbers, pids, generations, counters; nothing secret). The new process
//! adopts the workers, then starts a worker generation from the new binary and hands off to it
//! the usual way.
//!
//! Before the `exec`, the new binary is asked `serve --reexec-probe`: it must answer with a
//! protocol this supervisor speaks (an older binary does not know the flag and fails), and it
//! checks that `config.toml` loads and the log directory is writable. A failed probe or `exec`
//! leaves the old supervisor running as it was.
//!
//! Signals: in supervisor mode only the main thread (the supervisor's loop) takes the stop,
//! restart and upgrade signals; every other thread blocks them from its start. Before the
//! `exec` the main thread blocks them too, then the loop runs on for a moment so a signal the
//! runtime caught just before is still acted on (it calls the `exec` off). From then on a
//! signal stays pending in the kernel, survives the `exec` (pending signals and the mask do) and
//! is delivered once the new process has its handlers and unblocks.

use std::ffi::{CString, OsString};
use std::os::fd::RawFd;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

use serde::{Deserialize, Serialize};

use super::proto::RestartResult;

/// The [`Resume`] format this build writes.
pub const PROTO: u32 = 1;
/// Every [`Resume`] format this build reads.
pub const ACCEPTS: &[u32] = &[1];
/// The first word of the probe's answer.
const PROBE_MARK: &str = "pooltime-resume";

/// What a supervisor passes to the binary it re-executes into.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Resume {
    pub proto: u32,
    pub listen_fd: RawFd,
    /// The generation number the next worker gets.
    pub next_gen: u64,
    /// The version that re-executed.
    pub from_version: String,
    pub restarts: u64,
    pub last_restart: Option<RestartResult>,
    /// Upgrades finished before this one.
    pub upgrades: u64,
    pub workers: Vec<ResumedWorker>,
}

/// One live worker at the moment of the `exec`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResumedWorker {
    pub gen: u64,
    pub pid: u32,
    /// The supervisor's end of its control socket.
    pub ctl_fd: RawFd,
    /// The active generation (else: draining after a handoff).
    pub active: bool,
    pub version: Option<String>,
    /// Seconds since it entered its phase (the drain backstop keeps counting).
    pub age_secs: u64,
}

impl Resume {
    pub fn to_arg(&self) -> String {
        serde_json::to_string(self).expect("resume state serializes")
    }

    /// Parse `--resume`; an unknown format is an error (the probe should have caught it).
    pub fn parse(arg: &str) -> anyhow::Result<Resume> {
        let r: Resume = serde_json::from_str(arg)
            .map_err(|e| anyhow::anyhow!("unreadable --resume state: {e}"))?;
        if !ACCEPTS.contains(&r.proto) {
            anyhow::bail!("--resume state format {} is not supported", r.proto);
        }
        Ok(r)
    }

    /// The descriptors that must survive the `exec`.
    pub fn fds(&self) -> Vec<RawFd> {
        std::iter::once(self.listen_fd)
            .chain(self.workers.iter().map(|w| w.ctl_fd))
            .collect()
    }
}

/// What `serve --reexec-probe` prints.
pub fn probe_line() -> String {
    let accepts: Vec<String> = ACCEPTS.iter().map(u32::to_string).collect();
    format!(
        "{PROBE_MARK} {} {}",
        accepts.join(","),
        env!("CARGO_PKG_VERSION")
    )
}

/// The new binary's version, if its probe answer accepts [`PROTO`].
pub fn check_probe(stdout: &str) -> Result<String, String> {
    let line = stdout.lines().next().unwrap_or_default();
    let mut words = line.split_whitespace();
    if words.next() != Some(PROBE_MARK) {
        return Err("the new binary does not support replacing the supervisor in place".into());
    }
    let accepts = words.next().unwrap_or_default();
    if !accepts.split(',').any(|p| p.parse() == Ok(PROTO)) {
        return Err(format!(
            "the new binary cannot take over this supervisor (it reads formats {accepts}, this \
             one writes {PROTO})"
        ));
    }
    match words.next() {
        Some(v) => Ok(v.to_string()),
        None => Err("the new binary did not say its version".into()),
    }
}

/// The new process's argv: `exe`, the original arguments without any earlier `--resume`, then
/// `--resume <json>`.
pub fn argv(exe: &Path, args: impl IntoIterator<Item = OsString>, resume: &str) -> Vec<OsString> {
    let mut out = vec![exe.as_os_str().to_owned()];
    let mut skip = false;
    for a in args {
        if std::mem::take(&mut skip) {
            continue;
        }
        if a == "--resume" {
            skip = true;
            continue;
        }
        if a.as_bytes().starts_with(b"--resume=") {
            continue;
        }
        out.push(a);
    }
    out.push("--resume".into());
    out.push(resume.into());
    out
}

/// The signals that must not hit the process between the `exec` and its new handlers.
fn guarded() -> libc::sigset_t {
    // SAFETY: sigemptyset/sigaddset initialise and fill a set we own.
    unsafe {
        let mut set: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut set);
        for s in [libc::SIGHUP, libc::SIGTERM, libc::SIGINT, libc::SIGUSR2] {
            libc::sigaddset(&mut set, s);
        }
        set
    }
}

/// Replace this process with `argv` (`argv[0]` is the program); the signal mask is kept (block
/// first with [`block_signals`]). Returns only on failure.
pub fn exec(argv: &[OsString]) -> std::io::Error {
    let args: Result<Vec<CString>, _> = argv
        .iter()
        .map(|a| CString::new(a.as_bytes().to_vec()))
        .collect();
    let args = match args {
        Ok(a) if !a.is_empty() => a,
        _ => {
            return std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "an argument contains a NUL byte",
            )
        }
    };
    let mut ptrs: Vec<*const libc::c_char> = args.iter().map(|a| a.as_ptr()).collect();
    ptrs.push(std::ptr::null());
    // SAFETY: execv with NUL-terminated strings and a null-terminated array that outlive the
    // call (it does not return on success).
    unsafe {
        libc::execv(ptrs[0], ptrs.as_ptr());
    }
    std::io::Error::last_os_error()
}

/// Block the stop, restart and upgrade signals on the calling thread (threads it starts
/// inherit the mask).
pub fn block_signals() {
    let set = guarded();
    // SAFETY: pthread_sigmask on this thread with a set we own.
    unsafe {
        libc::pthread_sigmask(libc::SIG_BLOCK, &set, std::ptr::null_mut());
    }
}

/// Unblock what [`block_signals`] blocked (a no-op when nothing was).
pub fn unblock_signals() {
    let set = guarded();
    // SAFETY: pthread_sigmask on this thread with a set we own.
    unsafe {
        libc::pthread_sigmask(libc::SIG_UNBLOCK, &set, std::ptr::null_mut());
    }
}

/// Reap `pid` if it has exited: its status in the words `std` uses (`exit status: 0`).
pub fn reap(pid: u32) -> Option<String> {
    use std::os::unix::process::ExitStatusExt;
    let pid = libc::pid_t::try_from(pid).ok()?;
    let mut status = 0;
    // SAFETY: waitpid on one pid with a status int we own; WNOHANG never blocks.
    let r = unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) };
    if r == pid {
        return Some(std::process::ExitStatus::from_raw(status).to_string());
    }
    if r < 0 {
        let e = std::io::Error::last_os_error();
        if e.raw_os_error() == Some(libc::ECHILD) {
            return Some("gone (not a child of this supervisor)".into());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Resume {
        Resume {
            proto: PROTO,
            listen_fd: 7,
            next_gen: 4,
            from_version: "1.3.0".into(),
            restarts: 2,
            last_restart: Some(RestartResult {
                ok: true,
                generation: 3,
                error: None,
            }),
            upgrades: 1,
            workers: vec![
                ResumedWorker {
                    gen: 3,
                    pid: 4243,
                    ctl_fd: 9,
                    active: true,
                    version: Some("1.3.0".into()),
                    age_secs: 120,
                },
                ResumedWorker {
                    gen: 2,
                    pid: 4242,
                    ctl_fd: 8,
                    active: false,
                    version: None,
                    age_secs: 5,
                },
            ],
        }
    }

    #[test]
    fn resume_state_round_trips_and_names_its_descriptors() {
        let r = sample();
        assert_eq!(Resume::parse(&r.to_arg()).unwrap(), r);
        assert_eq!(r.fds(), [7, 9, 8]);
        let future = Resume { proto: 99, ..r };
        assert!(Resume::parse(&future.to_arg()).is_err());
        assert!(Resume::parse("{not json").is_err());
    }

    #[test]
    fn probe_answers_are_checked() {
        let ours = probe_line();
        assert_eq!(
            check_probe(&format!("{ours}\n")),
            Ok(env!("CARGO_PKG_VERSION").to_string())
        );
        assert_eq!(
            check_probe("pooltime-resume 1,2 9.0.0"),
            Ok("9.0.0".to_string())
        );
        // A newer binary that dropped this format, an old one that printed something else.
        assert!(check_probe("pooltime-resume 2 9.0.0")
            .unwrap_err()
            .contains("cannot take over"));
        assert!(check_probe("").is_err());
        assert!(check_probe("pooltime 1.2.1").is_err());
        assert!(check_probe("pooltime-resume 1").is_err());
    }

    #[test]
    fn argv_keeps_the_original_flags_and_replaces_an_earlier_resume() {
        let exe = Path::new("/opt/homebrew/bin/pooltime");
        let os = |v: &[&str]| v.iter().map(OsString::from).collect::<Vec<_>>();
        assert_eq!(
            argv(exe, os(&["serve", "--supervise", "--i-stopped-bro"]), "{}"),
            os(&[
                "/opt/homebrew/bin/pooltime",
                "serve",
                "--supervise",
                "--i-stopped-bro",
                "--resume",
                "{}"
            ])
        );
        assert_eq!(
            argv(
                exe,
                os(&["serve", "--supervise", "--resume", "{old}", "--resume={x}"]),
                "{new}"
            ),
            os(&[
                "/opt/homebrew/bin/pooltime",
                "serve",
                "--supervise",
                "--resume",
                "{new}"
            ])
        );
    }

    #[test]
    fn a_failed_exec_returns_and_the_signal_guard_toggles() {
        let mask = || {
            // SAFETY: reading this thread's mask into a set we own.
            unsafe {
                let mut cur: libc::sigset_t = std::mem::zeroed();
                libc::pthread_sigmask(libc::SIG_BLOCK, std::ptr::null(), &mut cur);
                libc::sigismember(&cur, libc::SIGUSR2)
            }
        };
        assert_eq!(mask(), 0);
        block_signals();
        assert_eq!(mask(), 1);
        let err = exec(&[OsString::from("/nonexistent/pooltime-test-binary")]);
        assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
        assert_eq!(mask(), 1, "exec leaves the mask alone");
        unblock_signals();
        assert_eq!(mask(), 0);
        assert_eq!(
            exec(&[OsString::from("a\0b")]).kind(),
            std::io::ErrorKind::InvalidInput
        );
    }

    #[test]
    fn reap_reports_an_exited_child_once() {
        let child = std::process::Command::new("true").spawn().unwrap();
        let pid = child.id();
        let start = std::time::Instant::now();
        let status = loop {
            if let Some(s) = reap(pid) {
                break s;
            }
            assert!(start.elapsed() < std::time::Duration::from_secs(10));
            std::thread::sleep(std::time::Duration::from_millis(10));
        };
        assert_eq!(status, "exit status: 0");
        assert!(reap(pid).unwrap().contains("not a child"));
        std::mem::forget(child);
    }
}
