//! `pooltime serve --supervise`: the service's main process. It binds the port once and runs
//! `pooltime serve` workers that inherit the listening socket, so a restart overlaps two worker
//! generations instead of closing the port (see `docs/specs/2026-11-13-seamless-restart.md`).
//!
//! - `SIGHUP`: start a new generation from the configured binary path (a Homebrew upgrade takes
//!   effect), wait for its `ready`, tell the old one to hand off (stop accepting, stop its
//!   background tasks, drain, exit), and after its `released` tell the new one it is `active`.
//!   A new generation that fails leaves the old one serving.
//! - A worker that dies is respawned at once (the port stays bound, so clients wait in the
//!   backlog instead of being refused); repeated failures back off, and after [`MAX_FAILURES`]
//!   in a row, or when the very first worker never gets ready, the supervisor exits non-zero and
//!   the service manager's restart policy takes over.
//! - `SIGTERM`/`SIGINT`: forwarded to every worker as `SIGTERM` (a second one forces them), then
//!   exit once they are gone.
//!
//! Deliberately small: no database, providers or HTTP. A running supervisor is not replaced by
//! `service restart`, only by `service install` (a full restart), so its protocol with the
//! workers ([`proto`]) must stay backward compatible.

pub mod proto;

use std::io::{IsTerminal, Write};
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use anyhow::Context;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::signal::unix::{signal, SignalKind};
use tokio::sync::mpsc;
use tokio::time::Instant;

use crate::cli::ServeFlags;
use crate::config::{Config, Paths};
use crate::server::{health, shutdown};
use proto::{FromWorker, RestartResult, SupervisorState, ToWorker};

/// How long a new worker may take to report `ready`.
pub const BOOT_TIMEOUT: Duration = Duration::from_secs(60);
/// How long the old generation may take to stop its background tasks after `handoff`.
pub const RELEASE_TIMEOUT: Duration = Duration::from_secs(10);
/// A worker that served this long before dying resets the failure count.
pub const STABLE_AFTER: Duration = Duration::from_secs(60);
/// Unexpected worker exits in a row before the supervisor gives up.
pub const MAX_FAILURES: u32 = 5;
/// A draining worker still alive this long after its handoff is killed (its own drain budget,
/// refresh drain and store flush come first).
const DRAIN_BACKSTOP: Duration = Duration::from_secs(shutdown::DRAIN_TIMEOUT.as_secs() + 120);

/// The pause before respawning after `failures` unexpected exits in a row: none for the first,
/// then 1, 2, 4, 8 s.
pub fn backoff(failures: u32) -> Duration {
    match failures {
        0 | 1 => Duration::ZERO,
        n => Duration::from_secs(1u64 << (n - 2).min(3)),
    }
}

/// The binary workers run: the path the service manager started us with (absolute `argv[0]`),
/// else this executable, mapped to the stable Homebrew symlink so an upgrade takes effect.
pub fn worker_exe() -> PathBuf {
    let exe = std::env::args_os()
        .next()
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| std::env::current_exe().ok())
        .unwrap_or_else(|| PathBuf::from("pooltime"));
    crate::service::stable_bin(&exe)
}

/// Run the supervisor until it is stopped; returns the exit code.
pub async fn run(paths: Paths, flags: ServeFlags) -> anyhow::Result<i32> {
    paths
        .ensure()
        .context("creating the pooltime data directory")?;
    let mut cfg = Config::load(&paths)?;
    if let Some(port) = flags.port {
        cfg.port = port;
    }
    if let Some(host) = flags.host {
        cfg.host = host;
    }
    cfg.validate()?;
    let _log = crate::logging::init(&paths, &cfg.retention, std::io::stderr().is_terminal())?;
    crate::logging::install_panic_hook();

    health::preflight(&cfg).await?;
    let listener = shutdown::bind(&cfg)
        .await?
        .into_std()
        .context("preparing the listener")?;
    let exe = worker_exe();
    tracing::info!(
        host = %cfg.host, port = cfg.port,
        version = env!("CARGO_PKG_VERSION"),
        worker = %exe.display(),
        "pooltime supervisor starting"
    );
    let mut sup = Supervisor::new(paths.clone(), exe, listener, flags.i_stopped_bro);
    let code = sup.run().await;
    let _ = std::fs::remove_file(proto::state_path(&paths));
    let code = code?;
    tracing::info!(code, "pooltime supervisor stopped");
    Ok(code)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    /// Started, not ready yet.
    Booting,
    /// Ready and accepting, waiting for `active` (the old generation is releasing).
    Serving,
    /// The generation that runs the background tasks.
    Active,
    /// Told to hand off; finishing its requests.
    Draining,
}

struct Worker {
    gen: u64,
    pid: u32,
    ctl: UnixStream,
    phase: Phase,
    version: Option<String>,
    since: Instant,
}

enum Event {
    Msg(u64, FromWorker),
    Exited(u64, String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Why {
    /// The first generation.
    Initial,
    /// `SIGHUP`.
    Restart,
    /// After a worker died.
    Respawn,
}

struct Boot {
    gen: u64,
    why: Why,
    deadline: Instant,
    timed_out: bool,
}

struct Release {
    old: u64,
    new: u64,
    why: Why,
    deadline: Instant,
}

struct Supervisor {
    paths: Paths,
    exe: PathBuf,
    listener: std::net::TcpListener,
    i_stopped_bro: bool,
    workers: Vec<Worker>,
    next_gen: u64,
    tx: mpsc::UnboundedSender<Event>,
    rx: mpsc::UnboundedReceiver<Event>,
    boot: Option<Boot>,
    release: Option<Release>,
    respawn_at: Option<Instant>,
    failures: u32,
    stopping: u32,
    exit_code: i32,
    state: SupervisorState,
    published: Option<SupervisorState>,
}

fn send_signal(pid: u32, sig: libc::c_int) {
    if let Ok(pid) = libc::pid_t::try_from(pid) {
        // SAFETY: plain kill(2) on a child we have not reaped yet.
        unsafe {
            libc::kill(pid, sig);
        }
    }
}

impl Supervisor {
    fn new(
        paths: Paths,
        exe: PathBuf,
        listener: std::net::TcpListener,
        i_stopped_bro: bool,
    ) -> Supervisor {
        let (tx, rx) = mpsc::unbounded_channel();
        Supervisor {
            paths,
            exe,
            listener,
            i_stopped_bro,
            workers: Vec::new(),
            next_gen: 1,
            tx,
            rx,
            boot: None,
            release: None,
            respawn_at: None,
            failures: 0,
            stopping: 0,
            exit_code: 0,
            state: SupervisorState {
                pid: std::process::id(),
                version: env!("CARGO_PKG_VERSION").into(),
                ..SupervisorState::default()
            },
            published: None,
        }
    }

    async fn run(&mut self) -> anyhow::Result<i32> {
        let mut hup = signal(SignalKind::hangup()).context("installing signal handlers")?;
        let mut term = signal(SignalKind::terminate()).context("installing signal handlers")?;
        let mut int = signal(SignalKind::interrupt()).context("installing signal handlers")?;
        self.start(Why::Initial);
        loop {
            self.publish();
            if self.stopping > 0 && self.workers.is_empty() {
                return Ok(self.exit_code);
            }
            let deadline = self.next_deadline();
            tokio::select! {
                _ = hup.recv() => self.on_restart(),
                _ = term.recv() => self.on_stop("SIGTERM"),
                _ = int.recv() => self.on_stop("SIGINT"),
                Some(ev) = self.rx.recv() => match ev {
                    Event::Msg(gen, m) => self.on_message(gen, m),
                    Event::Exited(gen, status) => self.on_exit(gen, &status),
                },
                _ = async {
                    match deadline {
                        Some(d) => tokio::time::sleep_until(d).await,
                        None => std::future::pending().await,
                    }
                } => self.on_timer(),
            }
        }
    }

    fn worker_mut(&mut self, gen: u64) -> Option<&mut Worker> {
        self.workers.iter_mut().find(|w| w.gen == gen)
    }

    fn tell(&mut self, gen: u64, msg: ToWorker) {
        if let Some(w) = self.worker_mut(gen) {
            if let Err(e) = (&w.ctl).write_all(msg.line().as_bytes()) {
                tracing::warn!(generation = gen, error = %e, "could not reach a worker");
            }
        }
    }

    /// Someone serves now, or will once its boot finishes.
    fn serving_or_booting(&self) -> bool {
        self.workers
            .iter()
            .any(|w| matches!(w.phase, Phase::Booting | Phase::Serving | Phase::Active))
    }

    fn start(&mut self, why: Why) {
        let gen = self.next_gen;
        self.next_gen += 1;
        match spawn_worker(
            &self.exe,
            gen,
            self.listener.as_raw_fd(),
            self.i_stopped_bro,
            self.tx.clone(),
        ) {
            Ok((pid, ctl)) => {
                tracing::info!(generation = gen, pid, reason = ?why, "worker started");
                self.workers.push(Worker {
                    gen,
                    pid,
                    ctl,
                    phase: Phase::Booting,
                    version: None,
                    since: Instant::now(),
                });
                self.boot = Some(Boot {
                    gen,
                    why,
                    deadline: Instant::now() + BOOT_TIMEOUT,
                    timed_out: false,
                });
            }
            Err(e) => {
                let why_text = format!("could not start {}: {e}", self.exe.display());
                tracing::error!(generation = gen, error = %why_text, "worker not started");
                match why {
                    Why::Initial => self.fail(),
                    Why::Restart => {
                        self.finish_restart(false, Some(why_text));
                        self.crashed();
                    }
                    Why::Respawn => self.crashed(),
                }
            }
        }
    }

    fn on_message(&mut self, gen: u64, msg: FromWorker) {
        match msg {
            FromWorker::Ready { version } => {
                let Some(w) = self.worker_mut(gen) else {
                    return;
                };
                if w.phase != Phase::Booting {
                    return;
                }
                w.phase = Phase::Serving;
                w.version = Some(version.clone());
                w.since = Instant::now();
                let why = match self.boot.take() {
                    Some(b) if b.gen == gen => b.why,
                    other => {
                        self.boot = other;
                        Why::Respawn
                    }
                };
                tracing::info!(generation = gen, %version, "worker ready");
                if self.stopping > 0 {
                    return;
                }
                let old = self
                    .workers
                    .iter()
                    .find(|w| w.phase == Phase::Active)
                    .map(|w| w.gen);
                match old {
                    Some(old) => {
                        tracing::info!(
                            from = old,
                            to = gen,
                            "handing off: the old generation stops accepting and drains"
                        );
                        self.tell(old, ToWorker::Handoff);
                        if let Some(w) = self.worker_mut(old) {
                            w.phase = Phase::Draining;
                            w.since = Instant::now();
                        }
                        self.release = Some(Release {
                            old,
                            new: gen,
                            why,
                            deadline: Instant::now() + RELEASE_TIMEOUT,
                        });
                    }
                    None => self.activate(gen, why),
                }
            }
            FromWorker::Released => {
                if self.release.as_ref().is_some_and(|r| r.old == gen) {
                    let r = self.release.take().expect("checked");
                    self.activate(r.new, r.why);
                }
            }
        }
    }

    fn activate(&mut self, gen: u64, why: Why) {
        let Some(w) = self.worker_mut(gen) else {
            return;
        };
        if w.phase != Phase::Serving {
            return;
        }
        w.phase = Phase::Active;
        w.since = Instant::now();
        self.tell(gen, ToWorker::Active);
        tracing::info!(generation = gen, "worker active");
        if why == Why::Restart {
            self.finish_restart(true, None);
        }
    }

    fn on_exit(&mut self, gen: u64, status: &str) {
        let Some(i) = self.workers.iter().position(|w| w.gen == gen) else {
            return;
        };
        let w = self.workers.remove(i);
        let boot = match self.boot.take() {
            Some(b) if b.gen == gen => Some(b),
            other => {
                self.boot = other;
                None
            }
        };
        if self.stopping > 0 {
            tracing::info!(generation = gen, %status, "worker stopped");
            return;
        }
        if let Some(b) = boot {
            let why_text = if b.timed_out {
                format!(
                    "the new worker was not ready within {}s",
                    BOOT_TIMEOUT.as_secs()
                )
            } else {
                format!("the new worker exited before it was ready ({status})")
            };
            tracing::error!(generation = gen, reason = ?b.why, error = %why_text, "worker failed to start");
            match b.why {
                Why::Initial => self.fail(),
                Why::Restart => {
                    self.finish_restart(false, Some(format!("{why_text}; see `pooltime logs`")));
                    self.crashed();
                }
                Why::Respawn => self.crashed(),
            }
            return;
        }
        if let Some(r) = self.release.take() {
            if r.old == gen {
                self.activate(r.new, r.why);
            } else if r.new == gen {
                tracing::error!(generation = gen, %status, "the new worker exited during the handoff");
                if r.why == Why::Restart {
                    self.finish_restart(false, Some(format!("the new worker exited ({status})")));
                }
                self.crashed();
                return;
            } else {
                self.release = Some(r);
            }
        }
        match w.phase {
            Phase::Draining => tracing::info!(generation = gen, %status, "old generation drained"),
            Phase::Booting | Phase::Serving | Phase::Active => {
                tracing::error!(generation = gen, %status, "worker exited unexpectedly");
                if w.phase == Phase::Active && w.since.elapsed() >= STABLE_AFTER {
                    self.failures = 0;
                }
                self.crashed();
            }
        }
    }

    /// Nobody may be serving: schedule a respawn (with backoff), or give up.
    fn crashed(&mut self) {
        if self.stopping > 0 || self.serving_or_booting() || self.respawn_at.is_some() {
            return;
        }
        self.failures += 1;
        if self.failures > MAX_FAILURES {
            tracing::error!(
                failures = self.failures - 1,
                "workers keep failing; exiting so the service manager can restart pooltime"
            );
            self.fail();
            return;
        }
        let d = backoff(self.failures);
        tracing::warn!(
            delay_ms = d.as_millis() as u64,
            failures = self.failures,
            "respawning a worker"
        );
        self.respawn_at = Some(Instant::now() + d);
    }

    fn on_timer(&mut self) {
        let now = Instant::now();
        let mut kill_boot = None;
        if let Some(b) = self.boot.as_mut() {
            if !b.timed_out && now >= b.deadline {
                b.timed_out = true;
                kill_boot = Some(b.gen);
            }
        }
        if let Some(gen) = kill_boot {
            if let Some(w) = self.worker_mut(gen) {
                tracing::error!(generation = gen, "worker not ready in time; killing it");
                send_signal(w.pid, libc::SIGKILL);
            }
        }
        if self.release.as_ref().is_some_and(|r| now >= r.deadline) {
            let r = self.release.take().expect("checked");
            tracing::warn!(
                old = r.old,
                new = r.new,
                "the old generation did not release its background tasks in time; activating the new one anyway"
            );
            self.activate(r.new, r.why);
        }
        if self.respawn_at.is_some_and(|t| now >= t) {
            self.respawn_at = None;
            self.start(Why::Respawn);
        }
        for w in &self.workers {
            if w.phase == Phase::Draining && now.duration_since(w.since) >= DRAIN_BACKSTOP {
                tracing::error!(
                    generation = w.gen,
                    "old generation still running; killing it"
                );
                send_signal(w.pid, libc::SIGKILL);
            }
        }
    }

    fn next_deadline(&self) -> Option<Instant> {
        let boot = self
            .boot
            .as_ref()
            .filter(|b| !b.timed_out)
            .map(|b| b.deadline);
        let draining = self
            .workers
            .iter()
            .filter(|w| w.phase == Phase::Draining)
            .map(|w| w.since + DRAIN_BACKSTOP);
        [
            boot,
            self.release.as_ref().map(|r| r.deadline),
            self.respawn_at,
        ]
        .into_iter()
        .flatten()
        .chain(draining)
        .min()
    }

    fn on_restart(&mut self) {
        if self.stopping > 0 {
            return;
        }
        if self.boot.is_some() || self.release.is_some() {
            tracing::info!("SIGHUP: a restart is already in progress");
            return;
        }
        tracing::info!("SIGHUP: starting a new worker generation");
        self.respawn_at = None;
        self.start(Why::Restart);
    }

    fn on_stop(&mut self, name: &str) {
        self.stopping += 1;
        self.respawn_at = None;
        tracing::info!(
            signal = name,
            workers = self.workers.len(),
            "stopping: forwarding SIGTERM to the workers"
        );
        for w in &self.workers {
            send_signal(w.pid, libc::SIGTERM);
        }
    }

    /// Stop everything and exit 1 once the workers are gone.
    fn fail(&mut self) {
        self.exit_code = 1;
        self.on_stop("fatal");
    }

    fn finish_restart(&mut self, ok: bool, error: Option<String>) {
        if ok {
            self.failures = 0;
        }
        self.state.restarts += 1;
        let generation = self
            .workers
            .iter()
            .find(|w| matches!(w.phase, Phase::Active | Phase::Serving))
            .map_or(0, |w| w.gen);
        self.state.last_restart = Some(RestartResult {
            ok,
            generation,
            error,
        });
    }

    /// Rewrite `run/supervisor.json` when something changed.
    fn publish(&mut self) {
        let serving = self
            .workers
            .iter()
            .filter(|w| matches!(w.phase, Phase::Active | Phase::Serving))
            .max_by_key(|w| w.gen);
        self.state.generation = serving.map_or(0, |w| w.gen);
        self.state.worker_pid = serving.map(|w| w.pid);
        self.state.worker_version = serving.and_then(|w| w.version.clone());
        self.state.draining = self
            .workers
            .iter()
            .filter(|w| w.phase == Phase::Draining)
            .map(|w| w.pid)
            .collect();
        if self.published.as_ref() == Some(&self.state) {
            return;
        }
        match proto::write_state(&self.paths, &self.state) {
            Ok(()) => self.published = Some(self.state.clone()),
            Err(e) => tracing::warn!(error = %e, "could not write run/supervisor.json"),
        }
    }
}

/// Clear `FD_CLOEXEC` so `fd` survives the `exec` of a worker.
fn inherit(fd: RawFd) -> std::io::Result<()> {
    // SAFETY: fcntl on an fd we own; async-signal-safe, so fine between fork and exec.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    if flags < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: as above.
    if unsafe { libc::fcntl(fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) } < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

/// Start `exe serve --listen-fd <listener> --control-fd <ours>` and watch it: control messages
/// and its exit arrive on `events`. Returns its pid and our end of the control socket.
fn spawn_worker(
    exe: &std::path::Path,
    gen: u64,
    listen_fd: RawFd,
    i_stopped_bro: bool,
    events: mpsc::UnboundedSender<Event>,
) -> std::io::Result<(u32, UnixStream)> {
    // Both ends are close-on-exec; only the child's copy of its end is made inheritable.
    let (ours, theirs) = UnixStream::pair()?;
    let control_fd = theirs.as_raw_fd();
    let mut cmd = tokio::process::Command::new(exe);
    cmd.arg("serve")
        .arg("--listen-fd")
        .arg(listen_fd.to_string())
        .arg("--control-fd")
        .arg(control_fd.to_string());
    if i_stopped_bro {
        cmd.arg("--i-stopped-bro");
    }
    cmd.stdin(Stdio::null()).kill_on_drop(false);
    // SAFETY: the closure only calls fcntl, which is async-signal-safe.
    unsafe {
        cmd.pre_exec(move || {
            inherit(listen_fd)?;
            inherit(control_fd)
        });
    }
    let mut child = cmd.spawn()?;
    drop(theirs);
    let pid = child.id().unwrap_or(0);
    let reader = ours.try_clone()?;
    reader.set_nonblocking(true)?;
    let reader = tokio::net::UnixStream::from_std(reader)?;
    tokio::spawn(async move {
        let mut lines = BufReader::new(reader).lines();
        let mut open = true;
        loop {
            tokio::select! {
                biased;
                l = lines.next_line(), if open => match l {
                    Ok(Some(l)) => {
                        if let Some(m) = FromWorker::parse(&l) {
                            let _ = events.send(Event::Msg(gen, m));
                        }
                    }
                    _ => open = false,
                },
                st = child.wait() => {
                    let status = match st {
                        Ok(s) => s.to_string(),
                        Err(e) => format!("wait failed: {e}"),
                    };
                    let _ = events.send(Event::Exited(gen, status));
                    return;
                }
            }
        }
    });
    Ok((pid, ours))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_starts_immediately_then_doubles_to_eight_seconds() {
        let secs: Vec<u64> = (1..=7).map(|n| backoff(n).as_secs()).collect();
        assert_eq!(secs, [0, 1, 2, 4, 8, 8, 8]);
        assert_eq!(backoff(0), Duration::ZERO);
    }

    #[test]
    fn inherit_clears_close_on_exec() {
        let (a, _b) = UnixStream::pair().unwrap();
        let fd = a.as_raw_fd();
        // SAFETY: reading the flags of an fd we own.
        let before = unsafe { libc::fcntl(fd, libc::F_GETFD) };
        assert_ne!(before & libc::FD_CLOEXEC, 0, "std opens close-on-exec");
        inherit(fd).unwrap();
        // SAFETY: as above.
        let after = unsafe { libc::fcntl(fd, libc::F_GETFD) };
        assert_eq!(after & libc::FD_CLOEXEC, 0);
    }
}
