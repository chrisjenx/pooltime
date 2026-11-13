//! Run the daemon as a background user service: a launchd agent on macOS ([`launchd`]), a systemd
//! user unit on Linux ([`systemd`]). Both run `pooltime serve --supervise`: a small supervisor
//! that owns the port and runs the daemon as worker generations, so `service restart` hands over
//! to a new worker without closing the port ([`restart`], [`crate::supervisor`]).
//!
//! The render functions are pure so their output is testable on every platform. Everything that
//! shells out goes through a [`Runner`], so tests substitute a recorder (or a `PATH` shim) for the
//! real `launchctl` / `systemctl`. `service uninstall` first releases every connected Claude Code
//! profile (see `cli::service`), so no profile is left pointing at a daemon that is gone.

pub mod launchd;
pub mod systemd;

use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context};

use crate::config::Paths;
use crate::supervisor::proto::{self, SupervisorState};

pub use launchd::render_launchd_plist;
pub use systemd::render_systemd_unit;

/// What a finished command reported.
#[derive(Debug, Clone)]
pub struct CmdOutput {
    pub success: bool,
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

/// Runs the service manager's CLI. `program` is looked up on `PATH`.
pub trait Runner {
    fn run(&self, program: &str, args: &[&str]) -> io::Result<CmdOutput>;
}

/// The real thing: spawns the process, capturing its output.
pub struct SystemRunner;

impl Runner for SystemRunner {
    fn run(&self, program: &str, args: &[&str]) -> io::Result<CmdOutput> {
        let out = Command::new(program)
            .args(args)
            .stdin(Stdio::null())
            .output()?;
        Ok(CmdOutput {
            success: out.status.success(),
            code: out.status.code(),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        })
    }
}

/// Run `program args`, failing (with the tool's stderr, which never carries credentials) when it
/// cannot be started or exits non-zero.
pub(crate) fn run_checked(
    r: &dyn Runner,
    program: &str,
    args: &[&str],
) -> anyhow::Result<CmdOutput> {
    let out = r
        .run(program, args)
        .with_context(|| format!("running `{program}`: is it installed and on PATH?"))?;
    if !out.success {
        let why = out.stderr.trim();
        return Err(anyhow!(
            "`{program} {}` failed ({}){}{}",
            args.join(" "),
            out.code.map_or("signal".into(), |c| format!("exit {c}")),
            if why.is_empty() { "" } else { ": " },
            why
        ));
    }
    Ok(out)
}

/// Tries at `launchctl bootstrap` (it can race the teardown of the job it replaces).
pub(crate) const BOOTSTRAP_ATTEMPTS: u32 = 3;

/// How long to wait for a job to unload, and how often to look; the pause between retries of a
/// failed load; how long to wait for the supervisor to confirm a restart. Injectable so tests
/// need not sleep.
#[derive(Debug, Clone, Copy)]
pub struct Timing {
    pub poll_step: Duration,
    /// Covers the daemon's drain (in-flight requests get up to 10 minutes).
    pub unload_timeout: Duration,
    pub retry_delay: Duration,
    pub handoff_timeout: Duration,
}

impl Default for Timing {
    fn default() -> Timing {
        Timing {
            poll_step: Duration::from_millis(200),
            unload_timeout: crate::server::shutdown::DRAIN_TIMEOUT + Duration::from_secs(60),
            retry_delay: Duration::from_secs(1),
            // A worker's boot budget, plus the old generation's release.
            handoff_timeout: crate::supervisor::BOOT_TIMEOUT
                + crate::supervisor::RELEASE_TIMEOUT
                + Duration::from_secs(10),
        }
    }
}

impl Timing {
    /// Check once, never sleep.
    pub fn instant() -> Timing {
        Timing {
            poll_step: Duration::ZERO,
            unload_timeout: Duration::ZERO,
            retry_delay: Duration::ZERO,
            handoff_timeout: Duration::ZERO,
        }
    }

    /// Poll `present` until it is false; `false` if it is still true after `unload_timeout`.
    /// `go_on(waited)` runs between polls (`waited` is zero except once per whole second): it
    /// may print progress, and stops the wait by returning `false`.
    pub(crate) fn wait_until_gone_with(
        &self,
        mut present: impl FnMut() -> bool,
        mut go_on: impl FnMut(Duration) -> bool,
    ) -> bool {
        let start = Instant::now();
        let mut last = 0;
        loop {
            if !present() {
                return true;
            }
            let waited = start.elapsed();
            if waited >= self.unload_timeout {
                return false;
            }
            // Once per whole second, so progress lines are not repeated.
            let secs = waited.as_secs();
            let tick = if secs != last {
                last = secs;
                waited
            } else {
                Duration::ZERO
            };
            if !go_on(tick) {
                return false;
            }
            std::thread::sleep(self.poll_step);
        }
    }

    /// Poll `done` until it is true; `false` if it is still false after `timeout`.
    pub(crate) fn wait_until(&self, timeout: Duration, mut done: impl FnMut() -> bool) -> bool {
        let start = Instant::now();
        loop {
            if done() {
                return true;
            }
            if start.elapsed() >= timeout {
                return false;
            }
            std::thread::sleep(self.poll_step);
        }
    }
}

/// Everything the service commands need to know about the user's machine, resolved once so the
/// flows can be tested against temp directories.
#[derive(Debug, Clone)]
pub struct Env {
    pub paths: Paths,
    /// The running binary (canonical path).
    pub bin: PathBuf,
    /// The user's `$HOME` (launch agents live under it).
    pub home: PathBuf,
    /// `$XDG_CONFIG_HOME` when set (and absolute).
    pub xdg_config_home: Option<PathBuf>,
    pub uid: u32,
    pub timing: Timing,
}

impl Env {
    pub fn from_process(paths: &Paths) -> anyhow::Result<Env> {
        let var = |k: &str| {
            std::env::var_os(k)
                .filter(|v| !v.is_empty())
                .map(PathBuf::from)
        };
        let home = var("HOME").ok_or_else(|| anyhow!("HOME is not set"))?;
        let bin = std::env::current_exe()
            .and_then(|p| p.canonicalize())
            .context("locating the pooltime binary")?;
        Ok(Env {
            paths: paths.clone(),
            bin,
            home,
            xdg_config_home: var("XDG_CONFIG_HOME").filter(|p| p.is_absolute()),
            // SAFETY: getuid has no preconditions and cannot fail.
            uid: unsafe { libc::getuid() },
            timing: Timing::default(),
        })
    }

    /// The binary path the service definition will name: [`stable_bin`] of [`Env::bin`], which
    /// must exist (a Homebrew keg that is not linked has no stable path to point at).
    pub(crate) fn service_bin(&self) -> anyhow::Result<PathBuf> {
        // A newline in a path would split a unit-file line or smuggle in one more.
        for p in [&self.bin, &self.paths.home] {
            if p.to_string_lossy().contains(['\n', '\r']) {
                return Err(anyhow!(
                    "a newline in the path {:?} cannot be written to a service definition",
                    p
                ));
            }
        }
        let bin = stable_bin(&self.bin);
        if bin != self.bin && !bin.exists() {
            return Err(anyhow!(
                "{} does not exist; run `brew link pooltime` and retry",
                bin.display()
            ));
        }
        Ok(bin)
    }
}

/// A Homebrew install lives at `<prefix>/Cellar/pooltime/<version>/bin/pooltime`, a path that
/// changes on every upgrade; `<prefix>/bin/pooltime` is the stable symlink to it. Any other path
/// is returned unchanged.
pub fn stable_bin(bin: &Path) -> PathBuf {
    let keg = (|| {
        if bin.file_name()? != "pooltime" {
            return None;
        }
        let bin_dir = bin.parent()?;
        let version = bin_dir.parent()?;
        let formula = version.parent()?;
        let cellar = formula.parent()?;
        let named = |p: &Path, n: &str| p.file_name().is_some_and(|f| f == n);
        (named(bin_dir, "bin") && named(formula, "pooltime") && named(cellar, "Cellar"))
            .then(|| {
                cellar
                    .parent()
                    .map(|prefix| prefix.join("bin").join("pooltime"))
            })
            .flatten()
    })();
    keg.unwrap_or_else(|| bin.to_path_buf())
}

static INTERRUPTS: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

extern "C" fn count_interrupt(_: libc::c_int) {
    INTERRUPTS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
}

/// Ctrl-C counted instead of obeyed while a service definition is being swapped (only when
/// stdin is a terminal: nobody presses Ctrl-C in a script or a test). The previous `SIGINT`
/// disposition is restored on drop.
pub(crate) struct Interrupts {
    previous: Option<libc::sigaction>,
}

impl Interrupts {
    /// Count nothing.
    pub(crate) fn none() -> Interrupts {
        Interrupts { previous: None }
    }

    pub(crate) fn catch() -> Interrupts {
        use std::io::IsTerminal;
        if !std::io::stdin().is_terminal() {
            return Interrupts::none();
        }
        INTERRUPTS.store(0, std::sync::atomic::Ordering::SeqCst);
        // SAFETY: installing a handler that only touches an atomic; `old` receives the
        // previous action, restored on drop.
        unsafe {
            let mut new: libc::sigaction = std::mem::zeroed();
            new.sa_sigaction = count_interrupt as *const () as libc::sighandler_t;
            libc::sigemptyset(&mut new.sa_mask);
            let mut old: libc::sigaction = std::mem::zeroed();
            if libc::sigaction(libc::SIGINT, &new, &mut old) != 0 {
                return Interrupts::none();
            }
            Interrupts {
                previous: Some(old),
            }
        }
    }

    /// Ctrl-Cs so far.
    pub(crate) fn count(&self) -> u32 {
        if self.previous.is_none() {
            return 0;
        }
        INTERRUPTS.load(std::sync::atomic::Ordering::SeqCst)
    }
}

impl Drop for Interrupts {
    fn drop(&mut self) {
        if let Some(old) = self.previous.take() {
            // SAFETY: restoring the action saved in `catch`.
            unsafe {
                libc::sigaction(libc::SIGINT, &old, std::ptr::null_mut());
            }
        }
    }
}

/// What `service status` reports.
#[derive(Debug, Clone)]
pub struct Status {
    /// The service definition file exists.
    pub installed: bool,
    /// The manager reports the service loaded and running.
    pub running: bool,
    /// Human-readable lines from the manager.
    pub detail: String,
}

/// The service manager of this platform.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backend {
    Launchd,
    Systemd,
}

impl Backend {
    /// launchd on macOS, systemd on Linux; an error elsewhere.
    pub fn native() -> anyhow::Result<Backend> {
        if cfg!(target_os = "macos") {
            Ok(Backend::Launchd)
        } else if cfg!(target_os = "linux") {
            Ok(Backend::Systemd)
        } else {
            Err(anyhow!(
                "`pooltime service` supports macOS (launchd) and Linux (systemd) only"
            ))
        }
    }

    /// Where the definition file lives.
    pub fn definition_path(self, env: &Env) -> PathBuf {
        match self {
            Backend::Launchd => launchd::plist_path(env),
            Backend::Systemd => systemd::unit_path(env),
        }
    }

    /// Write the definition (replacing any old one) and load it.
    pub fn install(self, env: &Env, r: &dyn Runner) -> anyhow::Result<()> {
        match self {
            Backend::Launchd => launchd::install(env, r),
            Backend::Systemd => systemd::install(env, r),
        }
    }

    /// Unload and remove the definition; `Ok(false)` if there was none.
    pub fn uninstall(self, env: &Env, r: &dyn Runner) -> anyhow::Result<bool> {
        match self {
            Backend::Launchd => launchd::uninstall(env, r),
            Backend::Systemd => systemd::uninstall(env, r),
        }
    }

    pub fn status(self, env: &Env, r: &dyn Runner) -> anyhow::Result<Status> {
        match self {
            Backend::Launchd => launchd::status(env, r),
            Backend::Systemd => systemd::status(env, r),
        }
    }

    /// Stop and start the whole service (supervisor included).
    pub fn restart(self, env: &Env, r: &dyn Runner) -> anyhow::Result<()> {
        match self {
            Backend::Launchd => launchd::restart(env, r),
            Backend::Systemd => systemd::restart(env, r),
        }
    }

    /// Send the supervisor its restart signal (`SIGHUP`).
    pub fn reload(self, env: &Env, r: &dyn Runner) -> anyhow::Result<()> {
        match self {
            Backend::Launchd => launchd::reload(env, r),
            Backend::Systemd => systemd::reload(env, r),
        }
    }

    /// Whether the installed definition runs the supervisor; `None` when nothing is installed.
    pub fn supervised(self, env: &Env) -> Option<bool> {
        std::fs::read_to_string(self.definition_path(env))
            .ok()
            .map(|d| definition_is_supervised(&d))
    }
}

/// A definition written before seamless restarts runs plain `serve`.
pub fn definition_is_supervised(definition: &str) -> bool {
    definition.contains("--supervise")
}

/// What `service restart` did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Restarted {
    /// A new worker generation serves; the previous one finishes its requests.
    /// `joined`: another restart was already under way, so the supervisor started nothing new
    /// and this is that restart's outcome.
    HandedOver {
        state: SupervisorState,
        joined: bool,
    },
    /// No supervisor was running: the service was started the ordinary way.
    Started,
    /// The definition predated seamless restarts: it was rewritten and reloaded (that restart
    /// drained the old way).
    Reinstalled,
}

/// `service restart`: with a supervised definition and a live supervisor, ask it for a new
/// worker generation and wait for its verdict; with no supervisor running, restart the service;
/// with a definition from before seamless restarts, reinstall it once.
pub fn restart(backend: Backend, env: &Env, r: &dyn Runner) -> anyhow::Result<Restarted> {
    match backend.supervised(env) {
        None => {
            return Err(anyhow!(
                "the pooltime service is not installed: run `pooltime service install`"
            ))
        }
        Some(false) => {
            eprintln!(
                "the installed service predates seamless restarts: reinstalling it once (this \
                 restart still waits for in-flight requests, up to 10 minutes); later restarts \
                 hand over without a pause"
            );
            backend.install(env, r)?;
            return Ok(Restarted::Reinstalled);
        }
        Some(true) => {}
    }
    let Some(before) = proto::live_state(&env.paths) else {
        backend.restart(env, r)?;
        return Ok(Restarted::Started);
    };
    backend
        .reload(env, r)
        .context("signalling the pooltime supervisor")?;
    let mut after = None;
    env.timing.wait_until(env.timing.handoff_timeout, || {
        after = proto::read_state(&env.paths).filter(|s| s.restarts > before.restarts);
        after.is_some()
    });
    let Some(after) = after else {
        return Err(anyhow!(
            "the pooltime supervisor did not confirm the restart within {}s; see `pooltime logs`",
            env.timing.handoff_timeout.as_secs()
        ));
    };
    match &after.last_restart {
        Some(res) if res.ok => Ok(Restarted::HandedOver {
            state: after,
            joined: before.restarting,
        }),
        Some(res) => Err(anyhow!(
            "{}restart failed: {}; the running daemon keeps serving",
            if before.restarting {
                "a restart was already in progress (this request started nothing new) and that "
            } else {
                ""
            },
            res.error.as_deref().unwrap_or("unknown error")
        )),
        None => Err(anyhow!(
            "the pooltime supervisor reported no restart result"
        )),
    }
}
