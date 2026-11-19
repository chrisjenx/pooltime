//! `pooltime update`: install the latest release the way this binary was installed, then move
//! the running service onto it without dropping a connection.
//!
//! 1. **How it was installed**, from this binary's path ([`detect`]): a Homebrew keg
//!    (`<prefix>/Cellar/pooltime/<version>/bin/pooltime`) upgrades with `brew upgrade
//!    chrisjenx/tap/pooltime`; a standalone binary in a directory this user can write (where
//!    `install.sh` puts it) gets the release's `install.sh` re-run for that directory and
//!    version (download, sha256 check, atomic rename). A `cargo install` binary or a build
//!    under `target/` is not replaced: the command says what to run and exits 1.
//! 2. **The latest release** comes from GitHub's releases API (`POOLTIME_RELEASES_API`, as for
//!    `doctor`). `--check` reports it with the installed and service versions and changes
//!    nothing.
//! 3. **The service.** When a supervisor runs and its version or its worker's differs from the
//!    installed binary, it gets `SIGUSR2`: it re-executes itself in place (same pid, the port
//!    never closes) and hands off to a worker from the new binary ([`crate::supervisor`]); the
//!    command waits for its verdict. A supervisor from before in-place upgrades gets a normal
//!    `service restart` instead (new workers; the supervisor stays until `service install`).
//!    No service running: only the binary is replaced.
//!
//! Interactive (stdin a terminal, no `--yes`): asks before changing anything. Otherwise
//! `--yes` is required. Every outside effect goes through [`Sys`], so tests need no network,
//! `brew` or service.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{anyhow, Context};
use async_trait::async_trait;
use clap::Args;

use crate::config::Paths;
use crate::service::{Backend, Env, Restarted, SystemRunner};
use crate::supervisor::proto::{self, SupervisorState};
use crate::version::Version;

/// The Homebrew formula.
pub const FORMULA: &str = "chrisjenx/tap/pooltime";
/// `cargo install` from source.
pub const CARGO_INSTALL: &str =
    "cargo install --locked --git https://github.com/chrisjenx/pooltime pooltime";
/// The release's own install script (`{tag}` replaced).
const INSTALL_SCRIPT: &str =
    "https://raw.githubusercontent.com/chrisjenx/pooltime/{tag}/install.sh";
const HTTP_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Debug, Clone, Default, Args)]
pub struct UpdateArgs {
    /// Only report the installed, latest and service versions; change nothing.
    #[arg(long)]
    pub check: bool,
    /// Do not ask (required when stdin is not a terminal).
    #[arg(long, short = 'y')]
    pub yes: bool,
}

/// How this binary was installed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Method {
    /// A Homebrew keg; `brew` is the prefix's own `brew` (or the one on `PATH`).
    Homebrew { brew: PathBuf },
    /// A standalone binary in `dir` (install.sh's layout).
    Script { dir: PathBuf },
    /// `cargo install`.
    Cargo,
    /// A build in a cargo `target/` directory.
    Dev,
    /// Anything else, and why it cannot be replaced.
    Unknown { why: String },
}

impl Method {
    fn describe(&self) -> String {
        match self {
            Method::Homebrew { .. } => "installed with Homebrew".into(),
            Method::Script { dir } => format!("installed in {}", dir.display()),
            Method::Cargo => "installed with cargo".into(),
            Method::Dev => "a development build".into(),
            Method::Unknown { why } => format!("install method unknown: {why}"),
        }
    }
}

/// Tell the install method from the binary's canonical path `exe`. `cargo_bin` is
/// `$CARGO_HOME/bin` (or `~/.cargo/bin`); `writable` says whether a directory can be written.
pub fn detect(exe: &Path, cargo_bin: Option<&Path>, writable: &dyn Fn(&Path) -> bool) -> Method {
    let stable = crate::service::stable_bin(exe);
    if stable != exe {
        // <prefix>/bin/pooltime -> <prefix>/bin/brew
        let brew = stable.with_file_name("brew");
        return Method::Homebrew { brew };
    }
    let Some(dir) = exe.parent() else {
        return Method::Unknown {
            why: "the binary has no directory".into(),
        };
    };
    // target/debug/pooltime, target/<triple>/release/pooltime
    if exe
        .ancestors()
        .skip(1)
        .take(3)
        .any(|a| a.file_name().is_some_and(|n| n == "target"))
    {
        return Method::Dev;
    }
    if cargo_bin.is_some_and(|c| c == dir) {
        return Method::Cargo;
    }
    if exe.file_name().is_none_or(|n| n != "pooltime") {
        return Method::Unknown {
            why: format!("the binary is not named pooltime ({})", exe.display()),
        };
    }
    if !writable(dir) {
        return Method::Unknown {
            why: format!("{} is not writable by this user", dir.display()),
        };
    }
    Method::Script {
        dir: dir.to_path_buf(),
    }
}

/// The binary an upgrade with `method` replaces (the path the service should name).
fn installed_bin(method: &Method, exe: &Path) -> PathBuf {
    match method {
        Method::Script { dir } => dir.join("pooltime"),
        _ => crate::service::stable_bin(exe),
    }
}

/// Everything `update` does outside its own logic.
#[async_trait]
pub trait Sys: Send + Sync {
    /// The latest release's tag (`v1.2.3`).
    async fn latest_tag(&self) -> anyhow::Result<String>;
    /// `brew upgrade chrisjenx/tap/pooltime` (output shown).
    async fn brew_upgrade(&self, brew: &Path) -> anyhow::Result<()>;
    /// Re-run the release's install script for `dir` and `version`.
    async fn script_install(&self, dir: &Path, version: &str) -> anyhow::Result<()>;
    /// What `bin --version` says.
    async fn version_of(&self, bin: &Path) -> Option<String>;
    /// The running supervisor, if any.
    fn supervisor(&self) -> Option<SupervisorState>;
    /// Whether the installed service definition runs the supervisor (`None`: not installed).
    fn service_supervised(&self) -> Option<bool>;
    /// Whether `pid` is a running `pooltime serve --supervise` process.
    fn is_supervisor(&self, pid: u32) -> bool;
    /// `SIGUSR2` to the supervisor.
    fn signal_upgrade(&self, pid: u32) -> anyhow::Result<()>;
    /// Wait until the supervisor has finished an upgrade after the first `after`.
    async fn wait_upgrade(&self, after: u64) -> Option<SupervisorState>;
    /// `pooltime service restart` (new workers, same supervisor).
    async fn restart_workers(&self) -> anyhow::Result<Restarted>;
    /// Ask a yes/no question on the terminal.
    async fn confirm(&self, question: &str) -> anyhow::Result<bool>;
}

/// What the flow knows about this process.
#[derive(Debug, Clone)]
pub struct Ctx {
    /// This binary's version.
    pub current: String,
    /// This binary (canonical path).
    pub exe: PathBuf,
    pub method: Method,
    /// stdin is a terminal.
    pub tty: bool,
}

/// `pooltime update` with every effect behind `sys`; returns the exit code.
pub async fn update(
    sys: &dyn Sys,
    ctx: &Ctx,
    args: &UpdateArgs,
    out: &mut dyn Write,
) -> anyhow::Result<i32> {
    let current = Version::parse(&ctx.current)
        .ok_or_else(|| anyhow!("unparsable version {}", ctx.current))?;
    writeln!(out, "pooltime {current} ({})", ctx.method.describe())?;
    let tag = sys
        .latest_tag()
        .await
        .context("asking GitHub for the latest release")?;
    let latest = Version::parse(&tag)
        .ok_or_else(|| anyhow!("the latest release has no version tag ({tag})"))?;
    let newer = latest > current;
    let sup = sys.supervisor();

    if args.check {
        if newer {
            writeln!(out, "latest release: {latest} (run `pooltime update`)")?;
        } else {
            writeln!(out, "latest release: {latest} (up to date)")?;
        }
        writeln!(
            out,
            "{}",
            service_line(sup.as_ref(), sys.service_supervised())
        )?;
        return Ok(0);
    }
    let go_ahead = |q: String| async move {
        if args.yes {
            return Ok::<bool, anyhow::Error>(true);
        }
        if !ctx.tty {
            return Ok(false);
        }
        sys.confirm(&q).await
    };

    let target = if newer {
        let action = match &ctx.method {
            Method::Homebrew { .. } => format!("`brew upgrade {FORMULA}`"),
            Method::Script { dir } => format!("download {latest} into {}", dir.display()),
            Method::Cargo => {
                writeln!(
                    out,
                    "{latest} is available. Installed with cargo: run `{CARGO_INSTALL}`, then \
                     `pooltime update` again to move the running service onto it."
                )?;
                return Ok(1);
            }
            Method::Dev => {
                writeln!(
                    out,
                    "{latest} is available, but {} is a development build: pull and rebuild, or \
                     install a release (docs/install.md).",
                    ctx.exe.display()
                )?;
                return Ok(1);
            }
            Method::Unknown { why } => {
                writeln!(
                    out,
                    "{latest} is available, but pooltime cannot replace this binary ({why}). \
                     Reinstall with Homebrew (`brew install {FORMULA}`) or the install script \
                     (docs/install.md)."
                )?;
                return Ok(1);
            }
        };
        if !go_ahead(format!("Update pooltime {current} -> {latest} ({action})?")).await? {
            writeln!(
                out,
                "{latest} is available; nothing changed{}",
                if args.yes || ctx.tty {
                    ""
                } else {
                    " (pass --yes to update without asking)"
                }
            )?;
            return Ok(1);
        }
        match &ctx.method {
            Method::Homebrew { brew } => sys.brew_upgrade(brew).await?,
            Method::Script { dir } => sys.script_install(dir, &latest.to_string()).await?,
            _ => unreachable!("only Homebrew and script installs get here"),
        }
        let bin = installed_bin(&ctx.method, &ctx.exe);
        let installed = sys
            .version_of(&bin)
            .await
            .and_then(|v| Version::parse(&v))
            .ok_or_else(|| anyhow!("{} does not run after the update", bin.display()))?;
        if installed <= current {
            writeln!(
                out,
                "the update left pooltime at {installed}{}",
                if matches!(ctx.method, Method::Homebrew { .. }) {
                    "; try `brew update` and run `pooltime update` again"
                } else {
                    ""
                }
            )?;
            return Ok(1);
        }
        writeln!(out, "installed pooltime {installed}")?;
        installed
    } else {
        writeln!(out, "pooltime {current} is the latest release")?;
        current
    };

    // The service, as it is now (the update above may have taken minutes).
    let sup = if newer { sys.supervisor() } else { sup };
    let Some(sup) = sup else {
        if sys.service_supervised() == Some(false) {
            writeln!(out, "{}", super::service::OLD_DEFINITION)?;
        } else {
            writeln!(out, "the service is not running; nothing else to update")?;
        }
        return Ok(0);
    };
    let target_s = target.to_string();
    let runs = |v: Option<&str>| v.and_then(Version::parse).as_ref() == Some(&target);
    if runs(Some(&sup.version)) && runs(sup.worker_version.as_deref()) {
        writeln!(out, "the service already runs {target}")?;
        return Ok(0);
    }
    if !newer
        && !go_ahead(format!(
            "Move the running service (supervisor {}, worker {}) onto {target}?",
            sup.version,
            sup.worker_version.as_deref().unwrap_or("?")
        ))
        .await?
    {
        writeln!(
            out,
            "the service still runs {}; nothing changed{}",
            sup.version,
            if args.yes || ctx.tty {
                ""
            } else {
                " (pass --yes to update without asking)"
            }
        )?;
        return Ok(1);
    }
    // The binary the service should now run.
    let bin = installed_bin(&ctx.method, &ctx.exe);
    let elsewhere = |runs_version: &str, binary: Option<&str>| {
        format!(
            "the service runs {}, which is {runs_version}, not the {target} at {}: point the \
             service at it with `{} service install` (a full restart that waits for in-flight \
             requests)",
            binary.unwrap_or("another pooltime binary"),
            bin.display(),
            bin.display()
        )
    };
    if !sup.can_upgrade {
        let r = sys.restart_workers().await?;
        let worker = match &r {
            Restarted::HandedOver { state, .. } => state.worker_version.clone(),
            _ => None,
        };
        writeln!(
            out,
            "service: restarted without closing the port; the worker runs {}",
            worker.as_deref().unwrap_or(&target_s)
        )?;
        if let Some(w) = worker.as_deref().filter(|w| !runs(Some(w))) {
            writeln!(out, "{}", elsewhere(w, sup.binary.as_deref()))?;
            return Ok(1);
        }
        writeln!(
            out,
            "note: the supervisor stays at {} (it predates in-place upgrades): run `pooltime \
             service install` once (a full restart that waits for in-flight requests); later \
             updates replace it without a pause",
            sup.version
        )?;
        return Ok(0);
    }
    writeln!(
        out,
        "moving the service onto {target} without closing the port (supervisor {}, worker {})...",
        sup.version,
        sup.worker_version.as_deref().unwrap_or("?")
    )?;
    out.flush()?;
    // Signal only the supervisor that is running right now: the state file is re-read and its
    // pid must still be a `pooltime serve --supervise` process (not a reused pid).
    match sys.supervisor() {
        Some(now) if now.pid == sup.pid && sys.is_supervisor(sup.pid) => {}
        _ => {
            writeln!(
                out,
                "the pooltime supervisor (pid {}) is gone or was replaced meanwhile; nothing was \
                 signalled. Run `pooltime update` again.",
                sup.pid
            )?;
            return Ok(1);
        }
    }
    sys.signal_upgrade(sup.pid)
        .context("signalling the pooltime supervisor")?;
    let Some(after) = sys.wait_upgrade(sup.upgrades).await else {
        writeln!(
            out,
            "the supervisor did not report the upgrade in time; check `pooltime service status` \
             and `pooltime logs`"
        )?;
        return Ok(1);
    };
    let worker = after.worker_version.as_deref().unwrap_or("?");
    match &after.last_upgrade {
        Some(u) if u.ok => {
            writeln!(
                out,
                "service: supervisor {} -> {}, worker {} -> {worker} (pid {} kept, no connection \
                 dropped)",
                u.from_version,
                u.version,
                sup.worker_version.as_deref().unwrap_or("?"),
                after.pid
            )?;
            // It re-executed the binary at its own path, which may not be the one updated.
            let off = [
                Some(after.version.as_str()),
                after.worker_version.as_deref(),
            ]
            .into_iter()
            .find(|v| !runs(*v));
            if let Some(v) = off {
                writeln!(
                    out,
                    "{}",
                    elsewhere(v.unwrap_or("unknown"), after.binary.as_deref())
                )?;
                return Ok(1);
            }
            Ok(0)
        }
        Some(u) => {
            writeln!(
                out,
                "service: upgrade failed: {}; it keeps serving (supervisor {}, worker {worker})",
                u.error.as_deref().unwrap_or("unknown error"),
                after.version
            )?;
            Ok(1)
        }
        None => {
            writeln!(out, "the supervisor reported no upgrade result")?;
            Ok(1)
        }
    }
}

/// `service: ...` for `--check`.
fn service_line(sup: Option<&SupervisorState>, supervised: Option<bool>) -> String {
    match (sup, supervised) {
        (Some(s), _) => format!(
            "service: supervisor {}, worker {}{}",
            s.version,
            s.worker_version.as_deref().unwrap_or("?"),
            if s.can_upgrade {
                ""
            } else {
                " (the supervisor predates in-place upgrades)"
            }
        ),
        (None, Some(false)) => "service: from before seamless restarts".into(),
        (None, Some(true)) => "service: installed, not running".into(),
        (None, None) => "service: not installed".into(),
    }
}

/// `pooltime update`.
pub async fn run(paths: &Paths, args: UpdateArgs) -> anyhow::Result<i32> {
    use std::io::IsTerminal;
    let exe = std::env::current_exe()
        .and_then(|p| p.canonicalize())
        .context("locating the pooltime binary")?;
    let cargo_bin = std::env::var_os("CARGO_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME")
                .filter(|v| !v.is_empty())
                .map(|h| PathBuf::from(h).join(".cargo"))
        })
        .map(|c| c.join("bin"));
    let method = detect(&exe, cargo_bin.as_deref(), &dir_writable);
    let ctx = Ctx {
        current: env!("CARGO_PKG_VERSION").into(),
        exe,
        method,
        tty: std::io::stdin().is_terminal(),
    };
    let sys = RealSys {
        paths: paths.clone(),
        releases_api: super::doctor::Options::from_env(false).releases_api,
    };
    let mut out = std::io::stdout();
    update(&sys, &ctx, &args, &mut out).await
}

fn dir_writable(dir: &Path) -> bool {
    use std::os::unix::ffi::OsStrExt;
    let Ok(c) = std::ffi::CString::new(dir.as_os_str().as_bytes()) else {
        return false;
    };
    // SAFETY: access(2) on a NUL-terminated path we own.
    unsafe { libc::access(c.as_ptr(), libc::W_OK) == 0 }
}

/// The command line of process `pid`, its arguments separated by spaces.
fn command_line(pid: u32) -> Option<String> {
    if cfg!(target_os = "linux") {
        let raw = std::fs::read(format!("/proc/{pid}/cmdline")).ok()?;
        let args: Vec<String> = raw
            .split(|b| *b == 0)
            .filter(|a| !a.is_empty())
            .map(|a| String::from_utf8_lossy(a).into_owned())
            .collect();
        return (!args.is_empty()).then(|| args.join(" "));
    }
    let out = std::process::Command::new("ps")
        .args(["-p", &pid.to_string(), "-o", "command="])
        .stdin(std::process::Stdio::null())
        .output()
        .ok()?;
    let line = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (out.status.success() && !line.is_empty()).then_some(line)
}

/// A `pooltime serve --supervise` command line (any path to the binary, any further flags).
fn looks_like_supervisor(cmdline: &str) -> bool {
    let words: Vec<&str> = cmdline.split_whitespace().collect();
    let Some(serve) = words.iter().position(|w| *w == "serve") else {
        return false;
    };
    serve > 0
        && words[serve - 1]
            .rsplit('/')
            .next()
            .is_some_and(|n| n.starts_with("pooltime"))
        && words[serve + 1..].contains(&"--supervise")
}

struct RealSys {
    paths: Paths,
    releases_api: String,
}

fn http() -> anyhow::Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .connect_timeout(HTTP_TIMEOUT)
        .timeout(HTTP_TIMEOUT)
        .build()?)
}

const USER_AGENT: &str = concat!("pooltime/", env!("CARGO_PKG_VERSION"));

#[async_trait]
impl Sys for RealSys {
    async fn latest_tag(&self) -> anyhow::Result<String> {
        let resp = http()?
            .get(&self.releases_api)
            .header(http::header::ACCEPT, "application/vnd.github+json")
            .header(http::header::USER_AGENT, USER_AGENT)
            .send()
            .await
            .map_err(|e| anyhow!("the release server is unreachable ({})", e.without_url()))?;
        let status = resp.status();
        if !status.is_success() {
            return Err(anyhow!(
                "the release server answered HTTP {}",
                status.as_u16()
            ));
        }
        let body = resp.bytes().await.context("reading the release")?;
        let v: serde_json::Value = serde_json::from_slice(&body).context("reading the release")?;
        v["tag_name"]
            .as_str()
            .map(str::to_string)
            .ok_or_else(|| anyhow!("the latest release has no tag"))
    }

    async fn brew_upgrade(&self, brew: &Path) -> anyhow::Result<()> {
        let brew = if brew.exists() {
            brew.to_path_buf()
        } else {
            PathBuf::from("brew")
        };
        let st = tokio::process::Command::new(&brew)
            .args(["upgrade", FORMULA])
            .stdin(std::process::Stdio::null())
            .status()
            .await
            .with_context(|| format!("running {}", brew.display()))?;
        if !st.success() {
            return Err(anyhow!("`brew upgrade {FORMULA}` failed ({st})"));
        }
        Ok(())
    }

    async fn script_install(&self, dir: &Path, version: &str) -> anyhow::Result<()> {
        let url = INSTALL_SCRIPT.replace("{tag}", &format!("v{version}"));
        let resp = http()?
            .get(&url)
            .header(http::header::USER_AGENT, USER_AGENT)
            .send()
            .await
            .map_err(|e| {
                anyhow!(
                    "downloading the install script failed ({})",
                    e.without_url()
                )
            })?;
        if !resp.status().is_success() {
            return Err(anyhow!(
                "downloading {url} failed: HTTP {}",
                resp.status().as_u16()
            ));
        }
        let script = resp
            .bytes()
            .await
            .context("downloading the install script")?;
        // Its own fresh directory; removed afterwards.
        let tmp = tempfile::tempdir().context("creating a temporary directory")?;
        let path = tmp.path().join("install.sh");
        std::fs::write(&path, &script).context("saving the install script")?;
        let st = tokio::process::Command::new("sh")
            .arg(&path)
            .env("POOLTIME_INSTALL_DIR", dir)
            .env("POOLTIME_VERSION", version)
            .stdin(std::process::Stdio::null())
            .status()
            .await
            .context("running the install script")?;
        if !st.success() {
            return Err(anyhow!("the install script failed ({st})"));
        }
        Ok(())
    }

    async fn version_of(&self, bin: &Path) -> Option<String> {
        let out = tokio::process::Command::new(bin)
            .arg("--version")
            .stdin(std::process::Stdio::null())
            .output()
            .await
            .ok()?;
        if !out.status.success() {
            return None;
        }
        // "pooltime 1.2.3"
        String::from_utf8_lossy(&out.stdout)
            .split_whitespace()
            .nth(1)
            .map(str::to_string)
    }

    fn supervisor(&self) -> Option<SupervisorState> {
        proto::live_state(&self.paths)
    }

    fn service_supervised(&self) -> Option<bool> {
        let backend = Backend::native().ok()?;
        let env = Env::from_process(&self.paths).ok()?;
        backend.supervised(&env)
    }

    fn is_supervisor(&self, pid: u32) -> bool {
        command_line(pid).is_some_and(|c| looks_like_supervisor(&c))
    }

    fn signal_upgrade(&self, pid: u32) -> anyhow::Result<()> {
        crate::service::signal_upgrade(pid)
    }

    async fn wait_upgrade(&self, after: u64) -> Option<SupervisorState> {
        let timing = crate::service::Timing::default();
        let budget = timing.handoff_timeout
            + crate::supervisor::PROBE_TIMEOUT
            + crate::supervisor::UPGRADE_IDLE_TIMEOUT
            + Duration::from_secs(10);
        let start = std::time::Instant::now();
        loop {
            if let Some(s) = proto::read_state(&self.paths).filter(|s| s.upgrades > after) {
                return Some(s);
            }
            if start.elapsed() >= budget {
                return None;
            }
            tokio::time::sleep(timing.poll_step).await;
        }
    }

    async fn restart_workers(&self) -> anyhow::Result<Restarted> {
        let paths = self.paths.clone();
        tokio::task::spawn_blocking(move || {
            let backend = Backend::native()?;
            let env = Env::from_process(&paths)?;
            crate::service::restart(backend, &env, &SystemRunner)
        })
        .await?
    }

    async fn confirm(&self, question: &str) -> anyhow::Result<bool> {
        super::client::confirm(question).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::supervisor::proto::UpgradeResult;
    use std::sync::Mutex;

    #[test]
    fn install_method_from_the_binary_path() {
        let yes = |_: &Path| true;
        let no = |_: &Path| false;
        let cargo = Path::new("/home/u/.cargo/bin");
        let d = |p: &str, w: &dyn Fn(&Path) -> bool| detect(Path::new(p), Some(cargo), w);
        assert_eq!(
            d("/opt/homebrew/Cellar/pooltime/1.2.1/bin/pooltime", &no),
            Method::Homebrew {
                brew: "/opt/homebrew/bin/brew".into()
            }
        );
        assert_eq!(
            d(
                "/home/linuxbrew/.linuxbrew/Cellar/pooltime/1.2.1/bin/pooltime",
                &no
            ),
            Method::Homebrew {
                brew: "/home/linuxbrew/.linuxbrew/bin/brew".into()
            }
        );
        assert_eq!(
            d("/home/u/.local/bin/pooltime", &yes),
            Method::Script {
                dir: "/home/u/.local/bin".into()
            }
        );
        assert_eq!(d("/home/u/.cargo/bin/pooltime", &yes), Method::Cargo);
        assert_eq!(d("/src/pooltime/target/debug/pooltime", &yes), Method::Dev);
        assert_eq!(
            d(
                "/src/pooltime/target/aarch64-apple-darwin/release/pooltime",
                &yes
            ),
            Method::Dev
        );
        assert!(matches!(
            d("/usr/local/bin/pooltime", &no),
            Method::Unknown { why } if why.contains("not writable")
        ));
        assert!(matches!(
            d("/home/u/bin/pooltime-1.2", &yes),
            Method::Unknown { why } if why.contains("not named pooltime")
        ));
        // Not a keg layout: a directory merely named Cellar is a plain install.
        assert_eq!(
            d("/home/u/Cellar/bin/pooltime", &yes),
            Method::Script {
                dir: "/home/u/Cellar/bin".into()
            }
        );
    }

    fn state(version: &str, worker: &str, can_upgrade: bool) -> SupervisorState {
        SupervisorState {
            pid: 4242,
            version: version.into(),
            generation: 1,
            worker_pid: Some(4243),
            worker_version: Some(worker.into()),
            can_upgrade,
            ..SupervisorState::default()
        }
    }

    /// Records every call; answers from its fields.
    #[derive(Default)]
    struct Fake {
        latest: String,
        /// What `version_of` answers after an upgrade.
        installed: Option<String>,
        sup: Option<SupervisorState>,
        supervised: Option<bool>,
        after: Option<SupervisorState>,
        answer: bool,
        /// The pid in the state file belongs to some other process now.
        not_supervisor: bool,
        calls: Mutex<Vec<String>>,
    }

    impl Fake {
        fn call(&self, c: impl Into<String>) {
            self.calls.lock().unwrap().push(c.into());
        }
        fn calls(&self) -> Vec<String> {
            self.calls.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl Sys for Fake {
        async fn latest_tag(&self) -> anyhow::Result<String> {
            if self.latest.is_empty() {
                return Err(anyhow!("offline"));
            }
            Ok(self.latest.clone())
        }
        async fn brew_upgrade(&self, brew: &Path) -> anyhow::Result<()> {
            self.call(format!("brew {}", brew.display()));
            Ok(())
        }
        async fn script_install(&self, dir: &Path, version: &str) -> anyhow::Result<()> {
            self.call(format!("script {} {version}", dir.display()));
            Ok(())
        }
        async fn version_of(&self, bin: &Path) -> Option<String> {
            self.call(format!("version {}", bin.display()));
            self.installed.clone()
        }
        fn supervisor(&self) -> Option<SupervisorState> {
            self.sup.clone()
        }
        fn service_supervised(&self) -> Option<bool> {
            self.supervised
        }
        fn is_supervisor(&self, pid: u32) -> bool {
            !self.not_supervisor && self.sup.as_ref().is_some_and(|s| s.pid == pid)
        }
        fn signal_upgrade(&self, pid: u32) -> anyhow::Result<()> {
            self.call(format!("usr2 {pid}"));
            Ok(())
        }
        async fn wait_upgrade(&self, after: u64) -> Option<SupervisorState> {
            self.call(format!("wait {after}"));
            self.after.clone()
        }
        async fn restart_workers(&self) -> anyhow::Result<Restarted> {
            self.call("restart");
            Ok(Restarted::HandedOver {
                state: state("1.2.0", "1.3.0", false),
                joined: false,
            })
        }
        async fn confirm(&self, q: &str) -> anyhow::Result<bool> {
            self.call(format!("ask {q}"));
            Ok(self.answer)
        }
    }

    fn brew_ctx() -> Ctx {
        Ctx {
            current: "1.2.0".into(),
            exe: "/opt/homebrew/Cellar/pooltime/1.2.0/bin/pooltime".into(),
            method: Method::Homebrew {
                brew: "/opt/homebrew/bin/brew".into(),
            },
            tty: false,
        }
    }

    async fn go(sys: &Fake, ctx: &Ctx, args: UpdateArgs) -> (i32, String) {
        let mut out = Vec::new();
        let code = update(sys, ctx, &args, &mut out).await.unwrap();
        (code, String::from_utf8(out).unwrap())
    }

    fn yes() -> UpdateArgs {
        UpdateArgs {
            check: false,
            yes: true,
        }
    }

    fn upgraded(from: &str, to: &str) -> SupervisorState {
        SupervisorState {
            upgrades: 1,
            last_upgrade: Some(UpgradeResult {
                ok: true,
                from_version: from.into(),
                version: to.into(),
                error: None,
            }),
            ..state(to, to, true)
        }
    }

    #[tokio::test]
    async fn homebrew_update_then_the_supervisor_replaces_itself() {
        let sys = Fake {
            latest: "v1.3.0".into(),
            installed: Some("1.3.0".into()),
            sup: Some(state("1.2.0", "1.2.0", true)),
            after: Some(upgraded("1.2.0", "1.3.0")),
            ..Fake::default()
        };
        let (code, out) = go(&sys, &brew_ctx(), yes()).await;
        assert_eq!(code, 0, "{out}");
        assert_eq!(
            sys.calls(),
            [
                "brew /opt/homebrew/bin/brew",
                "version /opt/homebrew/bin/pooltime",
                "usr2 4242",
                "wait 0"
            ]
        );
        assert!(out.contains("installed pooltime 1.3.0"), "{out}");
        assert!(
            out.contains("service: supervisor 1.2.0 -> 1.3.0, worker 1.2.0 -> 1.3.0"),
            "{out}"
        );
    }

    #[tokio::test]
    async fn script_install_reruns_the_script_and_no_service_means_binary_only() {
        let sys = Fake {
            latest: "v1.3.0".into(),
            installed: Some("1.3.0".into()),
            supervised: None,
            ..Fake::default()
        };
        let ctx = Ctx {
            exe: "/home/u/.local/bin/pooltime".into(),
            method: Method::Script {
                dir: "/home/u/.local/bin".into(),
            },
            ..brew_ctx()
        };
        let (code, out) = go(&sys, &ctx, yes()).await;
        assert_eq!(code, 0, "{out}");
        assert_eq!(
            sys.calls(),
            [
                "script /home/u/.local/bin 1.3.0",
                "version /home/u/.local/bin/pooltime"
            ]
        );
        assert!(out.contains("service is not running"), "{out}");
    }

    #[tokio::test]
    async fn check_changes_nothing() {
        let sys = Fake {
            latest: "v1.3.0".into(),
            sup: Some(state("1.1.0", "1.2.0", false)),
            ..Fake::default()
        };
        let (code, out) = go(
            &sys,
            &brew_ctx(),
            UpdateArgs {
                check: true,
                yes: false,
            },
        )
        .await;
        assert_eq!(code, 0);
        assert!(sys.calls().is_empty(), "{:?}", sys.calls());
        assert!(
            out.contains("latest release: 1.3.0 (run `pooltime update`)"),
            "{out}"
        );
        assert!(
            out.contains("service: supervisor 1.1.0, worker 1.2.0 (the supervisor predates"),
            "{out}"
        );
    }

    #[tokio::test]
    async fn dev_cargo_and_unknown_installs_say_what_to_do_and_exit_1() {
        for (method, says) in [
            (Method::Dev, "development build"),
            (Method::Cargo, "cargo install --locked"),
            (
                Method::Unknown {
                    why: "x is not writable".into(),
                },
                "x is not writable",
            ),
        ] {
            let sys = Fake {
                latest: "v1.3.0".into(),
                sup: Some(state("1.2.0", "1.2.0", true)),
                ..Fake::default()
            };
            let ctx = Ctx {
                method,
                ..brew_ctx()
            };
            let (code, out) = go(&sys, &ctx, yes()).await;
            assert_eq!(code, 1, "{out}");
            assert!(out.contains(says), "{out}");
            assert!(sys.calls().is_empty(), "{:?}", sys.calls());
        }
    }

    #[tokio::test]
    async fn without_a_terminal_or_yes_nothing_changes_and_a_no_is_respected() {
        let sys = Fake {
            latest: "v1.3.0".into(),
            ..Fake::default()
        };
        let (code, out) = go(&sys, &brew_ctx(), UpdateArgs::default()).await;
        assert_eq!(code, 1);
        assert!(out.contains("pass --yes"), "{out}");
        assert!(sys.calls().is_empty());

        let ctx = Ctx {
            tty: true,
            ..brew_ctx()
        };
        let (code, out) = go(&sys, &ctx, UpdateArgs::default()).await;
        assert_eq!(code, 1);
        assert!(!out.contains("pass --yes"), "{out}");
        assert_eq!(
            sys.calls(),
            ["ask Update pooltime 1.2.0 -> 1.3.0 (`brew upgrade chrisjenx/tap/pooltime`)?"]
        );
    }

    #[tokio::test]
    async fn up_to_date_still_moves_a_stale_service_and_leaves_a_current_one() {
        // After a manual `brew upgrade`: the binary is current, the service is not.
        let sys = Fake {
            latest: "v1.2.0".into(),
            sup: Some(state("1.1.0", "1.2.0", true)),
            after: Some(upgraded("1.1.0", "1.2.0")),
            ..Fake::default()
        };
        let (code, out) = go(&sys, &brew_ctx(), yes()).await;
        assert_eq!(code, 0, "{out}");
        assert_eq!(sys.calls(), ["usr2 4242", "wait 0"]);
        assert!(out.contains("1.2.0 is the latest release"), "{out}");

        let sys = Fake {
            latest: "v1.2.0".into(),
            sup: Some(state("1.2.0", "1.2.0", true)),
            ..Fake::default()
        };
        let (code, out) = go(&sys, &brew_ctx(), UpdateArgs::default()).await;
        assert_eq!(code, 0, "{out}");
        assert!(sys.calls().is_empty());
        assert!(out.contains("already runs 1.2.0"), "{out}");
    }

    #[tokio::test]
    async fn an_old_supervisor_gets_new_workers_and_a_note() {
        let sys = Fake {
            latest: "v1.3.0".into(),
            installed: Some("1.3.0".into()),
            sup: Some(state("1.2.0", "1.2.0", false)),
            ..Fake::default()
        };
        let (code, out) = go(&sys, &brew_ctx(), yes()).await;
        assert_eq!(code, 0, "{out}");
        assert_eq!(sys.calls().last().unwrap(), "restart");
        assert!(out.contains("the worker runs 1.3.0"), "{out}");
        assert!(out.contains("pooltime service install"), "{out}");
    }

    #[tokio::test]
    async fn a_failed_supervisor_upgrade_is_reported_with_exit_1() {
        let mut after = upgraded("1.2.0", "1.2.0");
        after.last_upgrade = Some(UpgradeResult {
            ok: false,
            from_version: "1.2.0".into(),
            version: "1.2.0".into(),
            error: Some("the new binary cannot take over".into()),
        });
        let sys = Fake {
            latest: "v1.3.0".into(),
            installed: Some("1.3.0".into()),
            sup: Some(state("1.2.0", "1.2.0", true)),
            after: Some(after),
            ..Fake::default()
        };
        let (code, out) = go(&sys, &brew_ctx(), yes()).await;
        assert_eq!(code, 1, "{out}");
        assert!(
            out.contains("upgrade failed: the new binary cannot take over"),
            "{out}"
        );
        assert!(out.contains("it keeps serving"), "{out}");
    }

    #[test]
    fn supervisor_command_lines() {
        for yes in [
            "/opt/homebrew/bin/pooltime serve --supervise",
            "pooltime serve --supervise --i-stopped-bro",
            "/home/u/.local/bin/pooltime serve --supervise --resume {\"proto\":1}",
        ] {
            assert!(looks_like_supervisor(yes), "{yes}");
        }
        for no in [
            "/usr/bin/vim notes",
            "pooltime serve",
            "pooltime serve --listen-fd 3 --control-fd 4",
            "/bin/sh -c echo serve --supervise",
            "",
        ] {
            assert!(!looks_like_supervisor(no), "{no}");
        }
        assert!(command_line(std::process::id()).is_some());
    }

    #[tokio::test]
    async fn a_reused_pid_is_never_signalled() {
        let sys = Fake {
            latest: "v1.3.0".into(),
            installed: Some("1.3.0".into()),
            sup: Some(state("1.2.0", "1.2.0", true)),
            not_supervisor: true,
            ..Fake::default()
        };
        let (code, out) = go(&sys, &brew_ctx(), yes()).await;
        assert_eq!(code, 1, "{out}");
        assert!(out.contains("nothing was signalled"), "{out}");
        assert!(!sys.calls().iter().any(|c| c.starts_with("usr2")));
    }

    #[tokio::test]
    async fn a_service_on_another_binary_is_not_reported_as_updated() {
        // The supervisor re-executed the binary it was installed with, which is not the one
        // this update replaced.
        let mut after = upgraded("1.2.0", "1.2.0");
        after.binary = Some("/home/u/.local/bin/pooltime".into());
        let sys = Fake {
            latest: "v1.3.0".into(),
            installed: Some("1.3.0".into()),
            sup: Some(state("1.2.0", "1.2.0", true)),
            after: Some(after),
            ..Fake::default()
        };
        let (code, out) = go(&sys, &brew_ctx(), yes()).await;
        assert_eq!(code, 1, "{out}");
        assert!(
            out.contains("the service runs /home/u/.local/bin/pooltime, which is 1.2.0, not the 1.3.0 at /opt/homebrew/bin/pooltime"),
            "{out}"
        );
        assert!(
            out.contains("/opt/homebrew/bin/pooltime service install"),
            "{out}"
        );
    }

    #[tokio::test]
    async fn an_upgrade_that_did_not_land_is_an_error() {
        let sys = Fake {
            latest: "v1.3.0".into(),
            installed: Some("1.2.0".into()),
            ..Fake::default()
        };
        let (code, out) = go(&sys, &brew_ctx(), yes()).await;
        assert_eq!(code, 1, "{out}");
        assert!(out.contains("brew update"), "{out}");

        let offline = Fake::default();
        let mut sink = Vec::new();
        let err = update(&offline, &brew_ctx(), &yes(), &mut sink)
            .await
            .unwrap_err();
        assert!(format!("{err:#}").contains("latest release"), "{err:#}");
    }
}
