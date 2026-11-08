//! Run the daemon as a background user service: a launchd agent on macOS ([`launchd`]), a systemd
//! user unit on Linux ([`systemd`]). Both wrap `pooltime serve`.
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

use anyhow::{anyhow, Context};

use crate::config::Paths;

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
        })
    }

    /// The binary path the service definition will name: [`stable_bin`] of [`Env::bin`], which
    /// must exist (a Homebrew keg that is not linked has no stable path to point at).
    pub(crate) fn service_bin(&self) -> anyhow::Result<PathBuf> {
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

    pub fn restart(self, env: &Env, r: &dyn Runner) -> anyhow::Result<()> {
        match self {
            Backend::Launchd => launchd::restart(env, r),
            Backend::Systemd => systemd::restart(env, r),
        }
    }
}
