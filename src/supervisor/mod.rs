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
//! - `SIGTERM`/`SIGINT`: every worker is told `stop` over its control socket (a second signal:
//!   `force`), then the supervisor exits once they are gone. Workers ignore OS signals while
//!   the supervisor is there, so a signal sent to the whole process group counts once.
//!
//! - `SIGUSR2` (`pooltime update`): replace the supervisor itself with the binary at the
//!   configured path, in place: check the new binary, wait until no restart is under way,
//!   `execve` it in this process (same pid, the port and the workers kept, see [`resume`]),
//!   then the new supervisor starts a worker generation from it and hands off as on `SIGHUP`.
//!   A new binary that fails its check, or an `exec` that fails, leaves everything as it was.
//!
//! Deliberately small: no database, providers or HTTP. `service restart` does not replace a
//! running supervisor (only `SIGUSR2` or `service install` do), so its protocol with the workers
//! ([`proto`]) must stay backward compatible.

pub mod proto;
pub mod resume;

use std::io::{IsTerminal, Write};
use std::os::fd::{AsRawFd, FromRawFd, RawFd};
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
use proto::{FromWorker, RestartResult, SupervisorState, ToWorker, UpgradeResult};
use resume::{Resume, ResumedWorker};

/// How long a new worker may take to report `ready`.
pub const BOOT_TIMEOUT: Duration = Duration::from_secs(60);
/// How long the old generation may take to stop its background tasks after `handoff`.
pub const RELEASE_TIMEOUT: Duration = Duration::from_secs(10);
/// A worker that served this long before dying resets the failure count.
pub const STABLE_AFTER: Duration = Duration::from_secs(60);
/// Unexpected worker exits in a row before the supervisor gives up.
pub const MAX_FAILURES: u32 = 5;
/// How long the new binary may take to answer its probe before an upgrade.
pub const PROBE_TIMEOUT: Duration = Duration::from_secs(15);
/// How long an upgrade waits for a restart under way to finish before giving up.
pub const UPGRADE_IDLE_TIMEOUT: Duration = Duration::from_secs(120);
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
    // The stop/restart/upgrade signals reach this (main) thread only: the runtime's threads
    // block them from their start (see `cli::run`), and so does the log writer thread, spawned
    // below while they are blocked here too. Unblocked once the handlers are in
    // (`Supervisor::run`). A signal meanwhile stays pending instead of killing the process.
    resume::block_signals();
    let resume = flags.resume.as_deref().map(Resume::parse).transpose()?;
    // Re-executed, this process holds the workers' control sockets: an error from here on must
    // not end it (the workers would drain and a restarted supervisor could find the port taken).
    // It carries on with what it has; stderr goes to the service manager's log.
    if let Err(e) = paths.ensure() {
        if resume.is_none() {
            return Err(e).context("creating the pooltime data directory");
        }
        eprintln!("pooltime supervisor: preparing the data directory failed: {e:#}");
    }
    let mut cfg = match Config::load(&paths) {
        Ok(c) => c,
        // Re-executed: the workers and the port depend on this process, so a config.toml that
        // broke since the probe must not stop it (the port and host are the socket's anyway).
        Err(_) if resume.is_some() => {
            eprintln!("pooltime supervisor: config.toml could not be loaded; using defaults");
            Config::default()
        }
        Err(e) => return Err(e),
    };
    if let Some(port) = flags.port {
        cfg.port = port;
    }
    if let Some(host) = flags.host {
        cfg.host = host;
    }
    if resume.is_none() {
        cfg.validate()?;
    }
    let _log = match crate::logging::init(&paths, &cfg.retention, std::io::stderr().is_terminal()) {
        Ok(g) => Some(g),
        Err(e) if resume.is_some() => {
            eprintln!("pooltime supervisor: no log file ({e:#}); logging to stderr only");
            None
        }
        Err(e) => return Err(e),
    };
    crate::logging::install_panic_hook();

    let listener = match &resume {
        // Without the socket no new worker can start, but the adopted ones keep serving on
        // their own copies of it.
        Some(r) => match take_listener(r.listen_fd) {
            Ok(l) => Some(l),
            Err(e) => {
                tracing::error!(error = %e, "the listening socket did not survive the re-exec; the adopted workers keep serving");
                eprintln!("pooltime supervisor: {e:#}");
                None
            }
        },
        None => {
            health::preflight(&cfg).await?;
            Some(
                shutdown::bind(&cfg)
                    .await?
                    .into_std()
                    .context("preparing the listener")?,
            )
        }
    };
    let exe = worker_exe();
    match &resume {
        Some(r) => tracing::info!(
            from = %r.from_version,
            version = env!("CARGO_PKG_VERSION"),
            workers = r.workers.len(),
            worker = %exe.display(),
            "pooltime supervisor re-executed in place"
        ),
        None => tracing::info!(
            host = %cfg.host, port = cfg.port,
            version = env!("CARGO_PKG_VERSION"),
            worker = %exe.display(),
            "pooltime supervisor starting"
        ),
    }
    let binary = exe.display().to_string();
    let listen = listener
        .as_ref()
        .and_then(|l| l.local_addr().ok())
        .map(|a| a.to_string());
    let launcher = ProcessLauncher {
        exe,
        listen_fd: listener.as_ref().map_or(-1, |l| l.as_raw_fd()),
        i_stopped_bro: flags.i_stopped_bro,
    };
    let mut sup = Supervisor::new(paths.clone(), launcher);
    sup.state.binary = Some(binary);
    sup.state.listen = listen;
    if let Some(r) = resume {
        sup.adopt(r);
    }
    let code = sup.run().await;
    drop(listener);
    let _ = std::fs::remove_file(proto::state_path(&paths));
    let code = code?;
    tracing::info!(code, "pooltime supervisor stopped");
    Ok(code)
}

/// The listening socket a re-executing supervisor left at `fd`.
fn take_listener(fd: RawFd) -> anyhow::Result<std::net::TcpListener> {
    use crate::server::handoff::{fd_is_socket, is_stream, set_cloexec};
    if fd < 3 || !fd_is_socket(fd) || !is_stream(fd) {
        anyhow::bail!("--resume: descriptor {fd} is not the listening socket");
    }
    // SAFETY: an open stream socket inherited across exec (checked above); nothing else in this
    // process refers to it.
    let listener = unsafe { std::net::TcpListener::from_raw_fd(fd) };
    set_cloexec(fd).context("marking the listener close-on-exec")?;
    listener
        .local_addr()
        .context("--resume: the inherited descriptor is not a TCP listener")?;
    Ok(listener)
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

/// The supervisor's end of one worker's control socket.
trait Ctl: Send {
    fn send(&self, msg: ToWorker) -> std::io::Result<()>;
    /// The descriptor handed across a re-exec.
    fn raw_fd(&self) -> RawFd;
}

impl Ctl for UnixStream {
    fn send(&self, msg: ToWorker) -> std::io::Result<()> {
        let mut s: &UnixStream = self;
        s.write_all(msg.line().as_bytes())
    }

    fn raw_fd(&self) -> RawFd {
        self.as_raw_fd()
    }
}

/// Starts and kills workers (a fake in tests).
trait Launcher {
    /// Start generation `gen`; its messages and exit arrive on `events`.
    fn spawn(
        &mut self,
        gen: u64,
        events: mpsc::UnboundedSender<Event>,
    ) -> std::io::Result<(u32, Box<dyn Ctl>)>;
    fn kill(&mut self, pid: u32);
    /// Take over a worker a re-executed supervisor left behind.
    fn adopt(
        &mut self,
        w: &ResumedWorker,
        events: mpsc::UnboundedSender<Event>,
    ) -> std::io::Result<Box<dyn Ctl>>;
    /// Ask the binary workers run whether it can take over this supervisor; the answer arrives
    /// as [`Event::Probed`].
    fn probe(&mut self, events: mpsc::UnboundedSender<Event>);
    /// The listening socket's descriptor.
    fn listen_fd(&self) -> RawFd;
    /// Replace this process with that binary, passing `state`. Returns only on failure.
    fn reexec(&mut self, state: &Resume) -> std::io::Error;
    /// Block (or unblock) the stop/restart/upgrade signals on the supervisor's thread, the only
    /// one that takes them: blocked, they stay pending across the `exec`.
    fn guard_signals(&mut self, on: bool);
}

struct Worker {
    gen: u64,
    pid: u32,
    ctl: Box<dyn Ctl>,
    phase: Phase,
    version: Option<String>,
    since: Instant,
    /// Sent `SIGKILL` by a backstop; just waiting for its exit now.
    killed: bool,
}

enum Event {
    Msg(u64, FromWorker),
    Exited(u64, String),
    /// The new binary's probe: its version, or why it cannot take over.
    Probed(Result<String, String>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Why {
    /// The first generation.
    Initial,
    /// `SIGHUP`.
    Restart,
    /// After a worker died.
    Respawn,
    /// The first generation of a supervisor that re-executed in place.
    Upgrade,
}

impl Why {
    /// Someone waits for this generation's outcome (`service restart`, `pooltime update`).
    fn reported(self) -> bool {
        matches!(self, Why::Restart | Why::Upgrade)
    }
}

/// An upgrade (`SIGUSR2`) on its way to the `exec`.
enum Upgrade {
    /// The new binary is being asked.
    Probing { deadline: Instant },
    /// It can take over (it is this version): `exec` as soon as no restart is under way.
    Ready { version: String, deadline: Instant },
}

struct Boot {
    gen: u64,
    why: Why,
    deadline: Instant,
    timed_out: bool,
}

/// The old generation is stopping its background tasks; nobody may start them until it has
/// (or its deadline passed). Kept even if the generation waiting to take over dies: the next
/// one waits too.
struct Release {
    old: u64,
    /// The ready generation to activate afterwards (none while a replacement boots).
    new: Option<(u64, Why)>,
    deadline: Instant,
}

struct Supervisor<L: Launcher> {
    paths: Paths,
    launcher: L,
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
    upgrade: Option<Upgrade>,
    /// Re-executed from this version; cleared once the upgrade's generation has its outcome.
    resumed_from: Option<String>,
    /// Signals are blocked for the `exec` due then: until that moment the loop keeps running,
    /// so a signal taken just before the block is still acted on (and calls the `exec` off).
    exec_at: Option<Instant>,
}

/// Between blocking the signals and the `exec`: long enough for the runtime to hand over a
/// signal caught just before the block, and for the log writer to write the last lines.
const EXEC_GRACE: Duration = Duration::from_millis(150);

impl<L: Launcher> Supervisor<L> {
    fn new(paths: Paths, launcher: L) -> Supervisor<L> {
        let (tx, rx) = mpsc::unbounded_channel();
        Supervisor {
            paths,
            launcher,
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
                can_upgrade: true,
                ..SupervisorState::default()
            },
            published: None,
            upgrade: None,
            resumed_from: None,
            exec_at: None,
        }
    }

    /// Continue where the supervisor that re-executed into this one stopped: its workers, its
    /// generation numbers and counters. [`Supervisor::run`] then starts a generation from the
    /// new binary.
    fn adopt(&mut self, r: Resume) {
        self.next_gen = self.next_gen.max(r.next_gen);
        self.state.restarts = r.restarts;
        self.state.last_restart = r.last_restart;
        self.state.upgrades = r.upgrades;
        self.resumed_from = Some(r.from_version);
        for w in &r.workers {
            match self.launcher.adopt(w, self.tx.clone()) {
                Ok(ctl) => {
                    let since = Instant::now()
                        .checked_sub(Duration::from_secs(w.age_secs))
                        .unwrap_or_else(Instant::now);
                    tracing::info!(
                        generation = w.gen,
                        pid = w.pid,
                        active = w.active,
                        "worker adopted"
                    );
                    self.workers.push(Worker {
                        gen: w.gen,
                        pid: w.pid,
                        ctl,
                        phase: if w.active {
                            Phase::Active
                        } else {
                            Phase::Draining
                        },
                        version: w.version.clone(),
                        since,
                        killed: false,
                    });
                }
                // Its control socket is closed now, so it hands off and drains on its own.
                Err(e) => tracing::error!(
                    generation = w.gen, pid = w.pid, error = %e,
                    "could not adopt a worker; it drains on its own"
                ),
            }
        }
    }

    async fn run(&mut self) -> anyhow::Result<i32> {
        let mut hup = signal(SignalKind::hangup()).context("installing signal handlers")?;
        let mut term = signal(SignalKind::terminate()).context("installing signal handlers")?;
        let mut int = signal(SignalKind::interrupt()).context("installing signal handlers")?;
        let mut usr2 = signal(SignalKind::user_defined2()).context("installing signal handlers")?;
        // Blocked across a re-exec; delivered (if one came) now that the handlers are in.
        resume::unblock_signals();
        self.start(if self.resumed_from.is_some() {
            Why::Upgrade
        } else {
            Why::Initial
        });
        loop {
            self.maybe_reexec();
            self.publish();
            if self.done() {
                return Ok(self.exit_code);
            }
            let deadline = self.next_deadline();
            tokio::select! {
                _ = hup.recv() => self.on_restart(),
                _ = usr2.recv() => self.on_upgrade(),
                _ = term.recv() => self.on_stop("SIGTERM"),
                _ = int.recv() => self.on_stop("SIGINT"),
                Some(ev) = self.rx.recv() => self.on_event(ev),
                _ = async {
                    match deadline {
                        Some(d) => tokio::time::sleep_until(d).await,
                        None => std::future::pending().await,
                    }
                } => self.on_timer(),
            }
        }
    }

    fn done(&self) -> bool {
        self.stopping > 0 && self.workers.is_empty()
    }

    fn on_event(&mut self, ev: Event) {
        match ev {
            Event::Msg(gen, m) => self.on_message(gen, m),
            Event::Exited(gen, status) => self.on_exit(gen, &status),
            Event::Probed(res) => self.on_probed(res),
        }
    }

    fn worker_mut(&mut self, gen: u64) -> Option<&mut Worker> {
        self.workers.iter_mut().find(|w| w.gen == gen)
    }

    fn tell(&mut self, gen: u64, msg: ToWorker) {
        if let Some(w) = self.worker_mut(gen) {
            if let Err(e) = w.ctl.send(msg) {
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
        match self.launcher.spawn(gen, self.tx.clone()) {
            Ok((pid, ctl)) => {
                tracing::info!(generation = gen, pid, reason = ?why, "worker started");
                self.workers.push(Worker {
                    gen,
                    pid,
                    ctl,
                    phase: Phase::Booting,
                    version: None,
                    since: Instant::now(),
                    killed: false,
                });
                self.boot = Some(Boot {
                    gen,
                    why,
                    deadline: Instant::now() + BOOT_TIMEOUT,
                    timed_out: false,
                });
            }
            Err(e) => {
                let why_text = format!("could not start a worker: {e}");
                tracing::error!(generation = gen, error = %why_text, "worker not started");
                match why {
                    Why::Initial => self.fail(),
                    Why::Restart | Why::Upgrade => {
                        self.finish_restart(why, false, Some(why_text));
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
                // An older generation is still releasing its background tasks: wait for it.
                if let Some(r) = self.release.as_mut() {
                    if r.new.is_none() {
                        r.new = Some((gen, why));
                        return;
                    }
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
                            new: Some((gen, why)),
                            deadline: Instant::now() + RELEASE_TIMEOUT,
                        });
                    }
                    None => self.activate(gen, why),
                }
            }
            FromWorker::Released => {
                if self.release.as_ref().is_some_and(|r| r.old == gen) {
                    self.released();
                }
            }
        }
    }

    /// The old generation let go of the background tasks (or ran out of time): activate the
    /// generation waiting for them, if any.
    fn released(&mut self) {
        if let Some(Release {
            new: Some((gen, why)),
            ..
        }) = self.release.take()
        {
            self.activate(gen, why);
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
        if why.reported() {
            self.finish_restart(why, true, None);
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
                Why::Restart | Why::Upgrade => {
                    self.finish_restart(
                        b.why,
                        false,
                        Some(format!("{why_text}; see `pooltime logs`")),
                    );
                    self.crashed();
                }
                Why::Respawn => self.crashed(),
            }
            return;
        }
        if self.release.as_ref().is_some_and(|r| r.old == gen) {
            // Gone, so its background tasks are too.
            self.released();
        } else if let Some(r) = self
            .release
            .as_mut()
            .filter(|r| r.new.is_some_and(|(n, _)| n == gen))
        {
            let (_, why) = r.new.take().expect("checked");
            tracing::error!(generation = gen, %status, "the new worker exited during the handoff");
            if why.reported() {
                self.finish_restart(
                    why,
                    false,
                    Some(format!("the new worker exited ({status})")),
                );
            }
            self.crashed();
            return;
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
        let mut kill = Vec::new();
        if let Some(b) = self.boot.as_mut() {
            if !b.timed_out && now >= b.deadline {
                b.timed_out = true;
                tracing::error!(generation = b.gen, "worker not ready in time; killing it");
                kill.push(b.gen);
            }
        }
        if self.release.as_ref().is_some_and(|r| now >= r.deadline) {
            tracing::warn!(
                "the old generation did not release its background tasks in time; going on"
            );
            self.released();
        }
        if self.respawn_at.is_some_and(|t| now >= t) {
            self.respawn_at = None;
            self.start(Why::Respawn);
        }
        if self.upgrade_deadline().is_some_and(|d| now >= d) {
            let why = match self.upgrade {
                Some(Upgrade::Probing { .. }) => "the new binary did not answer its check in time",
                _ => "the service was busy restarting workers the whole time; try again",
            };
            self.finish_upgrade(false, Some(why.into()));
        }
        for w in &self.workers {
            if w.phase == Phase::Draining
                && !w.killed
                && now.duration_since(w.since) >= DRAIN_BACKSTOP
            {
                tracing::error!(
                    generation = w.gen,
                    "old generation still running; killing it"
                );
                kill.push(w.gen);
            }
        }
        for gen in kill {
            if let Some(w) = self.worker_mut(gen) {
                w.killed = true;
                let pid = w.pid;
                self.launcher.kill(pid);
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
            .filter(|w| w.phase == Phase::Draining && !w.killed)
            .map(|w| w.since + DRAIN_BACKSTOP);
        [
            boot,
            self.release.as_ref().map(|r| r.deadline),
            self.respawn_at,
            self.upgrade_deadline(),
            self.exec_at,
        ]
        .into_iter()
        .flatten()
        .chain(draining)
        .min()
    }

    fn restarting(&self) -> bool {
        self.boot.is_some() || self.release.is_some()
    }

    fn on_restart(&mut self) {
        if self.stopping > 0 {
            return;
        }
        if self.restarting() {
            tracing::info!("SIGHUP: a restart is already in progress");
            return;
        }
        tracing::info!("SIGHUP: starting a new worker generation");
        self.respawn_at = None;
        self.start(Why::Restart);
    }

    fn upgrade_deadline(&self) -> Option<Instant> {
        match self.upgrade {
            Some(Upgrade::Probing { deadline } | Upgrade::Ready { deadline, .. }) => Some(deadline),
            None => None,
        }
    }

    /// `SIGUSR2`: replace the supervisor with the binary at the configured path.
    fn on_upgrade(&mut self) {
        if self.stopping > 0 {
            return;
        }
        if self.upgrade.is_some() || self.resumed_from.is_some() {
            tracing::info!("SIGUSR2: an upgrade is already in progress");
            return;
        }
        tracing::info!("SIGUSR2: checking the new binary before replacing the supervisor");
        self.upgrade = Some(Upgrade::Probing {
            deadline: Instant::now() + PROBE_TIMEOUT + Duration::from_secs(5),
        });
        self.launcher.probe(self.tx.clone());
    }

    fn on_probed(&mut self, res: Result<String, String>) {
        if !matches!(self.upgrade, Some(Upgrade::Probing { .. })) {
            return;
        }
        match res {
            Ok(version) => {
                tracing::info!(%version, "the new binary can take over the supervisor");
                self.upgrade = Some(Upgrade::Ready {
                    version,
                    deadline: Instant::now() + UPGRADE_IDLE_TIMEOUT,
                });
            }
            Err(e) => {
                tracing::error!(error = %e, "not replacing the supervisor");
                self.finish_upgrade(false, Some(e));
            }
        }
    }

    /// `exec` the new binary once an upgrade is ready and nothing is half-way: no generation
    /// booting or releasing (its messages would be lost across the `exec`), none to respawn.
    fn maybe_reexec(&mut self) {
        let ready = matches!(self.upgrade, Some(Upgrade::Ready { .. }));
        let idle = ready
            && self.stopping == 0
            && !self.restarting()
            && self.respawn_at.is_none()
            && self.workers.iter().any(|w| w.phase == Phase::Active)
            && self
                .workers
                .iter()
                .all(|w| matches!(w.phase, Phase::Active | Phase::Draining));
        if !idle {
            self.disarm();
            return;
        }
        // First block the signals and let the loop run a little: a signal the runtime caught
        // before the block arrives meanwhile (a stop or restart then calls this off until it is
        // done; one that comes later stays pending and is delivered to the new process).
        let Some(at) = self.exec_at else {
            self.launcher.guard_signals(true);
            self.exec_at = Some(Instant::now() + EXEC_GRACE);
            return;
        };
        if Instant::now() < at {
            return;
        }
        let Some(Upgrade::Ready { version, .. }) = &self.upgrade else {
            return;
        };
        let state = Resume {
            proto: resume::PROTO,
            listen_fd: self.launcher.listen_fd(),
            next_gen: self.next_gen,
            from_version: self.state.version.clone(),
            restarts: self.state.restarts,
            last_restart: self.state.last_restart.clone(),
            upgrades: self.state.upgrades,
            workers: self
                .workers
                .iter()
                .map(|w| ResumedWorker {
                    gen: w.gen,
                    pid: w.pid,
                    ctl_fd: w.ctl.raw_fd(),
                    active: w.phase == Phase::Active,
                    version: w.version.clone(),
                    age_secs: w.since.elapsed().as_secs(),
                })
                .collect(),
        };
        tracing::info!(
            from = %self.state.version, to = %version,
            workers = state.workers.len(),
            "re-executing the supervisor in place"
        );
        let err = self.launcher.reexec(&state);
        self.disarm();
        tracing::error!(error = %err, "could not re-execute the supervisor; it keeps running");
        self.finish_upgrade(
            false,
            Some(format!("could not start the new supervisor: {err}")),
        );
    }

    /// Unblock the signals if an `exec` was due.
    fn disarm(&mut self) {
        if self.exec_at.take().is_some() {
            self.launcher.guard_signals(false);
        }
    }

    /// Record an upgrade's outcome (`pooltime update` waits for `upgrades` to go up).
    fn finish_upgrade(&mut self, ok: bool, error: Option<String>) {
        self.upgrade = None;
        let from = self
            .resumed_from
            .take()
            .unwrap_or_else(|| self.state.version.clone());
        self.state.upgrades += 1;
        self.state.last_upgrade = Some(UpgradeResult {
            ok,
            from_version: from,
            version: self.state.version.clone(),
            error,
        });
    }

    /// The first stop signal tells every worker to `stop` (drain), any further one to `force`.
    /// Over the control socket, not as signals: a worker may get the same `SIGTERM` directly
    /// (process-group signal, `pkill`), and must not count it twice.
    fn on_stop(&mut self, name: &str) {
        self.stopping += 1;
        self.respawn_at = None;
        if self.upgrade.is_some() || self.resumed_from.is_some() {
            self.finish_upgrade(false, Some("the service is stopping".into()));
        }
        self.disarm();
        let msg = if self.stopping == 1 {
            ToWorker::Stop
        } else {
            ToWorker::Force
        };
        tracing::info!(
            signal = name,
            workers = self.workers.len(),
            order = ?msg,
            "stopping the workers"
        );
        let gens: Vec<u64> = self.workers.iter().map(|w| w.gen).collect();
        for gen in gens {
            self.tell(gen, msg);
        }
    }

    /// Stop everything and exit 1 once the workers are gone.
    fn fail(&mut self) {
        self.exit_code = 1;
        if self.stopping == 0 {
            self.on_stop("fatal");
        }
    }

    fn finish_restart(&mut self, why: Why, ok: bool, error: Option<String>) {
        if why == Why::Upgrade {
            let upgrade_error = error.as_ref().map(|e| {
                format!(
                    "the supervisor now runs {}, but the first worker from it failed: {e}; the \
                     previous worker keeps serving",
                    self.state.version
                )
            });
            self.finish_upgrade(ok, upgrade_error);
        }
        if ok {
            self.failures = 0;
        }
        self.state.restarts += 1;
        let generation = self
            .workers
            .iter()
            .filter(|w| matches!(w.phase, Phase::Active | Phase::Serving))
            .map(|w| w.gen)
            .max()
            .unwrap_or(0);
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
        self.state.restarting = self.restarting();
        self.state.upgrading = self.upgrade.is_some() || self.resumed_from.is_some();
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

/// Real workers: `exe serve --listen-fd <listener> --control-fd <theirs>`.
struct ProcessLauncher {
    exe: PathBuf,
    listen_fd: RawFd,
    i_stopped_bro: bool,
}

impl Launcher for ProcessLauncher {
    fn spawn(
        &mut self,
        gen: u64,
        events: mpsc::UnboundedSender<Event>,
    ) -> std::io::Result<(u32, Box<dyn Ctl>)> {
        // Both ends are close-on-exec; only the child's copy of its end is made inheritable.
        let (ours, theirs) = UnixStream::pair()?;
        let control_fd = theirs.as_raw_fd();
        let listen_fd = self.listen_fd;
        if listen_fd < 0 {
            return Err(std::io::Error::other(
                "the listening socket was lost in the re-exec; `pooltime service restart` \
                 after the old workers exit, or `pooltime service install`",
            ));
        }
        let mut cmd = tokio::process::Command::new(&self.exe);
        cmd.arg("serve")
            .arg("--listen-fd")
            .arg(listen_fd.to_string())
            .arg("--control-fd")
            .arg(control_fd.to_string());
        if self.i_stopped_bro {
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
        Ok((pid, Box::new(ours)))
    }

    fn kill(&mut self, pid: u32) {
        if let Ok(pid) = libc::pid_t::try_from(pid) {
            // SAFETY: plain kill(2) on a child we have not reaped yet.
            unsafe {
                libc::kill(pid, libc::SIGKILL);
            }
        }
    }

    fn adopt(
        &mut self,
        w: &ResumedWorker,
        events: mpsc::UnboundedSender<Event>,
    ) -> std::io::Result<Box<dyn Ctl>> {
        use crate::server::handoff::{fd_is_socket, set_cloexec};
        let (gen, pid, fd) = (w.gen, w.pid, w.ctl_fd);
        let reader = if fd >= 3 && fd_is_socket(fd) {
            // SAFETY: an open socket inherited across exec (checked above), named once in the
            // resume state; this is its only owner.
            let ours = unsafe { UnixStream::from_raw_fd(fd) };
            set_cloexec(fd)?;
            let reader = ours.try_clone()?;
            reader.set_nonblocking(true)?;
            Ok((ours, tokio::net::UnixStream::from_std(reader)?))
        } else {
            Err(std::io::Error::other(format!(
                "control descriptor {fd} is not a socket"
            )))
        };
        // Not spawned by this process image, so no `Child` to wait on: reap it by pid (on
        // SIGCHLD, and once a second in case one is missed). Done even when its control socket
        // is unusable, so it never lingers as a zombie.
        let (ctl, lines) = match reader {
            Ok((ours, reader)) => (Ok(ours), Some(BufReader::new(reader).lines())),
            Err(e) => (Err(e), None),
        };
        tokio::spawn(async move {
            let mut sigchld = signal(SignalKind::child()).ok();
            let mut lines = lines;
            loop {
                if let Some(status) = resume::reap(pid) {
                    let _ = events.send(Event::Exited(gen, status));
                    return;
                }
                tokio::select! {
                    biased;
                    l = async { lines.as_mut().expect("checked").next_line().await },
                        if lines.is_some() => match l {
                        Ok(Some(l)) => {
                            if let Some(m) = FromWorker::parse(&l) {
                                let _ = events.send(Event::Msg(gen, m));
                            }
                        }
                        _ => lines = None,
                    },
                    _ = async {
                        match sigchld.as_mut() {
                            Some(s) => { s.recv().await; }
                            None => std::future::pending::<()>().await,
                        }
                    } => {}
                    _ = tokio::time::sleep(Duration::from_secs(1)) => {}
                }
            }
        });
        Ok(Box::new(ctl?))
    }

    fn probe(&mut self, events: mpsc::UnboundedSender<Event>) {
        let exe = self.exe.clone();
        tokio::spawn(async move {
            let _ = events.send(Event::Probed(probe_binary(&exe).await));
        });
    }

    fn listen_fd(&self) -> RawFd {
        self.listen_fd
    }

    fn guard_signals(&mut self, on: bool) {
        if on {
            resume::block_signals();
        } else {
            resume::unblock_signals();
        }
    }

    fn reexec(&mut self, state: &Resume) -> std::io::Error {
        let fds = state.fds();
        for fd in &fds {
            if let Err(e) = inherit(*fd) {
                restore_cloexec(&fds);
                return e;
            }
        }
        let argv = resume::argv(&self.exe, std::env::args_os().skip(1), &state.to_arg());
        let err = resume::exec(&argv);
        restore_cloexec(&fds);
        err
    }
}

fn restore_cloexec(fds: &[RawFd]) {
    for fd in fds {
        let _ = crate::server::handoff::set_cloexec(*fd);
    }
}

/// Run `exe serve --reexec-probe`: the version it answers with, or why it cannot take over.
async fn probe_binary(exe: &std::path::Path) -> Result<String, String> {
    let mut cmd = tokio::process::Command::new(exe);
    cmd.args(["serve", "--reexec-probe"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let out = match tokio::time::timeout(PROBE_TIMEOUT, cmd.output()).await {
        Err(_) => {
            return Err(format!(
                "{} did not answer its check within {}s",
                exe.display(),
                PROBE_TIMEOUT.as_secs()
            ))
        }
        Ok(Err(e)) => return Err(format!("could not run {}: {e}", exe.display())),
        Ok(Ok(out)) => out,
    };
    if !out.status.success() {
        // The probe's own messages, or an older binary's usage error: never secrets.
        let why: String = String::from_utf8_lossy(&out.stderr)
            .lines()
            .map(str::trim)
            .find(|l| !l.is_empty())
            .unwrap_or("no message")
            .chars()
            .take(200)
            .collect();
        return Err(format!(
            "{} cannot take over the supervisor ({}): {why}",
            exe.display(),
            out.status
        ));
    }
    resume::check_probe(&String::from_utf8_lossy(&out.stdout))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

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

    /// Everything the supervisor told or did to its workers, in order.
    type Log = Arc<Mutex<Vec<String>>>;

    struct FakeCtl(u64, Log);

    impl Ctl for FakeCtl {
        fn send(&self, msg: ToWorker) -> std::io::Result<()> {
            self.1
                .lock()
                .unwrap()
                .push(format!("{} {}", self.0, msg.line().trim()));
            Ok(())
        }

        fn raw_fd(&self) -> RawFd {
            100 + self.0 as RawFd
        }
    }

    #[derive(Default)]
    struct Fake {
        log: Log,
        /// What the next `reexec` returns (it only ever returns on failure).
        exec_error: Option<String>,
        /// The state the last `reexec` was given.
        execed: Option<Resume>,
    }

    impl Launcher for Fake {
        fn spawn(
            &mut self,
            gen: u64,
            _: mpsc::UnboundedSender<Event>,
        ) -> std::io::Result<(u32, Box<dyn Ctl>)> {
            self.log.lock().unwrap().push(format!("{gen} spawn"));
            Ok((1000 + gen as u32, Box::new(FakeCtl(gen, self.log.clone()))))
        }

        fn kill(&mut self, pid: u32) {
            self.log.lock().unwrap().push(format!("kill {pid}"));
        }

        fn adopt(
            &mut self,
            w: &ResumedWorker,
            _: mpsc::UnboundedSender<Event>,
        ) -> std::io::Result<Box<dyn Ctl>> {
            self.log.lock().unwrap().push(format!("{} adopt", w.gen));
            if w.ctl_fd < 0 {
                return Err(std::io::Error::other("closed"));
            }
            Ok(Box::new(FakeCtl(w.gen, self.log.clone())))
        }

        fn probe(&mut self, _: mpsc::UnboundedSender<Event>) {
            self.log.lock().unwrap().push("probe".into());
        }

        fn listen_fd(&self) -> RawFd {
            3
        }

        fn guard_signals(&mut self, on: bool) {
            self.log
                .lock()
                .unwrap()
                .push(if on { "block" } else { "unblock" }.into());
        }

        fn reexec(&mut self, state: &Resume) -> std::io::Error {
            self.log.lock().unwrap().push("exec".into());
            self.execed = Some(state.clone());
            std::io::Error::other(self.exec_error.clone().unwrap_or_else(|| "fake".into()))
        }
    }

    fn sup() -> (tempfile::TempDir, Supervisor<Fake>, Log) {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::under(dir.path());
        paths.ensure().unwrap();
        let fake = Fake::default();
        let log = fake.log.clone();
        (dir, Supervisor::new(paths, fake), log)
    }

    fn ready(gen: u64) -> FromWorker {
        FromWorker::Ready {
            version: format!("v{gen}"),
        }
    }

    fn take(log: &Log) -> Vec<String> {
        std::mem::take(&mut *log.lock().unwrap())
    }

    #[tokio::test]
    async fn restart_activates_the_new_generation_only_after_the_old_one_released() {
        let (_d, mut s, log) = sup();
        s.start(Why::Initial);
        s.on_message(1, ready(1));
        assert_eq!(take(&log), ["1 spawn", "1 active"]);

        s.on_restart();
        s.on_message(2, ready(2));
        assert_eq!(
            take(&log),
            ["2 spawn", "1 handoff"],
            "generation 2 serves, but its background tasks wait"
        );
        s.publish();
        assert!(s.state.restarting);
        // A second SIGHUP meanwhile starts nothing.
        s.on_restart();
        assert!(take(&log).is_empty());

        s.on_message(1, FromWorker::Released);
        assert_eq!(take(&log), ["2 active"]);
        s.publish();
        assert_eq!(s.state.restarts, 1);
        assert_eq!(s.state.generation, 2);
        assert_eq!(s.state.draining, [1001]);
        assert!(!s.state.restarting);
        assert!(s.state.last_restart.as_ref().unwrap().ok);
        s.on_exit(1, "exit status: 0");
        s.publish();
        assert!(s.state.draining.is_empty());
    }

    #[tokio::test]
    async fn a_replacement_waits_for_the_release_of_the_generation_before_it() {
        let (_d, mut s, log) = sup();
        s.start(Why::Initial);
        s.on_message(1, ready(1));
        s.on_restart();
        s.on_message(2, ready(2));
        take(&log);

        // Generation 2 dies after `ready`, before generation 1 has released.
        s.on_exit(2, "signal: 9 (SIGKILL)");
        assert!(!s.state.last_restart.as_ref().unwrap().ok);
        assert!(s.respawn_at.is_some(), "nobody serves: respawn");
        s.on_timer();
        s.on_message(3, ready(3));
        assert_eq!(
            take(&log),
            ["3 spawn"],
            "generation 1 is still stopping its background tasks: 3 must wait"
        );
        s.on_message(1, FromWorker::Released);
        assert_eq!(take(&log), ["3 active"]);
    }

    #[tokio::test]
    async fn the_release_deadline_and_the_old_generations_exit_also_unblock() {
        let (_d, mut s, log) = sup();
        s.start(Why::Initial);
        s.on_message(1, ready(1));
        s.on_restart();
        s.on_message(2, ready(2));
        take(&log);
        s.on_exit(1, "exit status: 0");
        assert_eq!(take(&log), ["2 active"]);

        s.on_restart();
        s.on_message(3, ready(3));
        take(&log);
        s.release.as_mut().unwrap().deadline = Instant::now();
        s.on_timer();
        assert_eq!(take(&log), ["3 active"]);
    }

    #[tokio::test]
    async fn a_failed_restart_keeps_the_old_generation_active() {
        let (_d, mut s, log) = sup();
        s.start(Why::Initial);
        s.on_message(1, ready(1));
        s.on_restart();
        s.on_exit(2, "exit status: 1");
        assert_eq!(take(&log), ["1 spawn", "1 active", "2 spawn"]);
        let r = s.state.last_restart.clone().unwrap();
        assert!(!r.ok);
        assert!(r.error.unwrap().contains("exited before it was ready"));
        assert!(s.respawn_at.is_none(), "generation 1 still serves");
        s.publish();
        assert_eq!(s.state.generation, 1);
    }

    #[tokio::test]
    async fn stop_and_force_go_over_the_control_socket_once_each() {
        let (_d, mut s, log) = sup();
        s.start(Why::Initial);
        s.on_message(1, ready(1));
        s.on_restart();
        s.on_message(2, ready(2));
        take(&log);
        s.on_stop("SIGTERM");
        assert_eq!(take(&log), ["1 stop", "2 stop"]);
        s.on_stop("SIGTERM");
        assert_eq!(take(&log), ["1 force", "2 force"]);
        s.on_exit(1, "exit status: 0");
        assert!(!s.done());
        s.on_exit(2, "exit status: 0");
        assert!(s.done());
        assert_eq!(s.exit_code, 0);
    }

    #[tokio::test]
    async fn the_drain_backstop_kills_once() {
        let (_d, mut s, log) = sup();
        s.start(Why::Initial);
        s.on_message(1, ready(1));
        s.on_restart();
        s.on_message(2, ready(2));
        s.on_message(1, FromWorker::Released);
        take(&log);
        s.worker_mut(1).unwrap().since = Instant::now() - DRAIN_BACKSTOP;
        s.on_timer();
        s.on_timer();
        assert_eq!(take(&log), ["kill 1001"]);
        assert!(s.next_deadline().is_none(), "no deadline left in the past");
    }

    #[tokio::test]
    async fn the_first_worker_failing_stops_the_supervisor_with_1() {
        let (_d, mut s, _log) = sup();
        s.start(Why::Initial);
        s.on_exit(1, "exit status: 1");
        assert!(s.done());
        assert_eq!(s.exit_code, 1);
    }

    #[tokio::test]
    async fn upgrade_probes_waits_for_a_restart_to_finish_then_execs_with_every_worker() {
        let (_d, mut s, log) = sup();
        s.start(Why::Initial);
        s.on_message(1, ready(1));
        s.on_restart();
        s.on_message(2, ready(2));
        take(&log);

        // SIGUSR2 while generation 1 is still releasing: probe now, exec later.
        s.on_upgrade();
        s.on_upgrade();
        assert_eq!(take(&log), ["probe"], "a second SIGUSR2 starts nothing");
        s.publish();
        assert!(s.state.upgrading && s.state.can_upgrade);
        s.on_probed(Ok("9.9.9".into()));
        s.maybe_reexec();
        assert!(take(&log).is_empty(), "a restart is under way: no exec yet");

        s.on_message(1, FromWorker::Released);
        assert_eq!(take(&log), ["2 active"]);
        s.launcher.exec_error = Some("Exec format error".into());
        s.maybe_reexec();
        assert_eq!(
            take(&log),
            ["block"],
            "signals blocked first; the exec waits a moment"
        );
        s.maybe_reexec();
        assert!(take(&log).is_empty(), "not yet");
        assert!(s.next_deadline().is_some());
        s.exec_at = Some(Instant::now());
        s.maybe_reexec();
        assert_eq!(take(&log), ["exec", "unblock"]);
        let r = s.launcher.execed.clone().unwrap();
        assert_eq!(r.listen_fd, 3);
        assert_eq!(r.next_gen, 3);
        assert_eq!(r.restarts, 1);
        assert_eq!(r.from_version, env!("CARGO_PKG_VERSION"));
        let w: Vec<_> = r
            .workers
            .iter()
            .map(|w| (w.gen, w.pid, w.ctl_fd, w.active))
            .collect();
        assert_eq!(w, [(1, 1001, 101, false), (2, 1002, 102, true)]);

        // The exec failed: everything keeps running, the failure is recorded once.
        let u = s.state.last_upgrade.clone().unwrap();
        assert!(!u.ok);
        assert!(u.error.unwrap().contains("Exec format error"));
        assert_eq!(s.state.upgrades, 1);
        s.maybe_reexec();
        assert!(take(&log).is_empty());
        s.publish();
        assert!(!s.state.upgrading);
        assert_eq!(s.state.generation, 2);
    }

    /// Drive an upgrade up to the moment the signals are blocked for the exec.
    fn armed() -> (tempfile::TempDir, Supervisor<Fake>, Log) {
        let (d, mut s, log) = sup();
        s.start(Why::Initial);
        s.on_message(1, ready(1));
        s.on_upgrade();
        s.on_probed(Ok("9.9.9".into()));
        s.maybe_reexec();
        assert_eq!(take(&log), ["1 spawn", "1 active", "probe", "block"]);
        (d, s, log)
    }

    #[tokio::test]
    async fn a_restart_signal_caught_before_the_block_calls_the_exec_off_until_it_is_done() {
        let (_d, mut s, log) = armed();
        // SIGHUP handed over by the runtime during the grace: the restart runs first.
        s.on_restart();
        s.exec_at = Some(Instant::now());
        s.maybe_reexec();
        assert_eq!(take(&log), ["2 spawn", "unblock"]);
        s.on_message(2, ready(2));
        s.on_message(1, FromWorker::Released);
        take(&log);
        // Idle again: blocked again, then the exec.
        s.maybe_reexec();
        s.exec_at = Some(Instant::now());
        s.maybe_reexec();
        assert_eq!(take(&log), ["block", "exec", "unblock"]);
    }

    #[tokio::test]
    async fn a_stop_signal_caught_before_the_block_cancels_the_upgrade() {
        let (_d, mut s, log) = armed();
        s.on_stop("SIGTERM");
        assert_eq!(take(&log), ["unblock", "1 stop"]);
        s.maybe_reexec();
        assert!(take(&log).is_empty(), "no exec");
        let u = s.state.last_upgrade.clone().unwrap();
        assert!(!u.ok && u.error.unwrap().contains("stopping"));
    }

    #[tokio::test]
    async fn a_stop_while_the_resumed_supervisors_first_worker_boots_records_the_upgrade() {
        let (_d, mut s, _log) = sup();
        s.adopt(resumed(vec![rw(4, 8, true)]));
        s.start(Why::Upgrade);
        s.on_stop("SIGTERM");
        assert_eq!(s.state.upgrades, 2);
        let u = s.state.last_upgrade.clone().unwrap();
        assert!(!u.ok, "{u:?}");
        assert_eq!(u.from_version, "1.2.9");
        assert!(u.error.unwrap().contains("stopping"));
        s.publish();
        assert!(!s.state.upgrading);
        // Its boot ending afterwards records nothing more.
        s.on_exit(5, "exit status: 0");
        assert_eq!(s.state.upgrades, 2);
    }

    #[tokio::test]
    async fn a_failed_probe_is_reported_and_changes_nothing() {
        let (_d, mut s, log) = sup();
        s.start(Why::Initial);
        s.on_message(1, ready(1));
        take(&log);
        s.on_upgrade();
        s.on_probed(Err("the new binary does not support it".into()));
        s.maybe_reexec();
        assert_eq!(take(&log), ["probe"]);
        let u = s.state.last_upgrade.clone().unwrap();
        assert!(!u.ok && u.error.unwrap().contains("does not support"));
        // A late answer for an upgrade that is over is ignored.
        s.on_probed(Ok("9.9.9".into()));
        s.maybe_reexec();
        assert!(take(&log).is_empty());
        assert_eq!(s.state.upgrades, 1);
    }

    #[tokio::test]
    async fn an_upgrade_gives_up_when_the_service_never_settles_or_stops() {
        let (_d, mut s, log) = sup();
        s.start(Why::Initial);
        s.on_message(1, ready(1));
        s.on_restart(); // generation 2 boots and never gets ready
        s.on_upgrade();
        s.on_probed(Ok("9.9.9".into()));
        if let Some(Upgrade::Ready { deadline, .. }) = s.upgrade.as_mut() {
            *deadline = Instant::now();
        }
        s.on_timer();
        let u = s.state.last_upgrade.clone().unwrap();
        assert!(u.error.unwrap().contains("busy"));

        s.on_upgrade();
        s.on_stop("SIGTERM");
        assert_eq!(s.state.upgrades, 2);
        assert!(s
            .state
            .last_upgrade
            .clone()
            .unwrap()
            .error
            .unwrap()
            .contains("stopping"));
        take(&log);
        s.on_upgrade();
        assert!(take(&log).is_empty(), "no upgrade while stopping");
    }

    fn resumed(workers: Vec<ResumedWorker>) -> Resume {
        Resume {
            proto: resume::PROTO,
            listen_fd: 3,
            next_gen: 5,
            from_version: "1.2.9".into(),
            restarts: 3,
            last_restart: None,
            upgrades: 1,
            workers,
        }
    }

    fn rw(gen: u64, ctl_fd: RawFd, active: bool) -> ResumedWorker {
        ResumedWorker {
            gen,
            pid: 1000 + gen as u32,
            ctl_fd,
            active,
            version: Some("1.2.9".into()),
            age_secs: 1,
        }
    }

    #[tokio::test]
    async fn a_resumed_supervisor_adopts_its_workers_and_hands_off_to_a_new_generation() {
        let (_d, mut s, log) = sup();
        s.adopt(resumed(vec![rw(3, 7, false), rw(4, 8, true)]));
        s.start(Why::Upgrade);
        assert_eq!(take(&log), ["3 adopt", "4 adopt", "5 spawn"]);
        s.publish();
        assert_eq!(
            s.state.generation, 4,
            "the adopted generation serves meanwhile"
        );
        assert_eq!(s.state.draining, [1003]);
        assert!(s.state.upgrading);
        // SIGHUP and SIGUSR2 meanwhile start nothing.
        s.on_restart();
        s.on_upgrade();
        assert!(take(&log).is_empty());

        s.on_message(5, ready(5));
        assert_eq!(take(&log), ["4 handoff"]);
        s.on_message(4, FromWorker::Released);
        assert_eq!(take(&log), ["5 active"]);
        s.publish();
        assert_eq!(s.state.generation, 5);
        assert_eq!(s.state.restarts, 4);
        assert_eq!(s.state.upgrades, 2);
        assert!(!s.state.upgrading);
        let u = s.state.last_upgrade.clone().unwrap();
        assert!(u.ok, "{u:?}");
        assert_eq!(u.from_version, "1.2.9");
        assert_eq!(u.version, env!("CARGO_PKG_VERSION"));

        // The adopted ones exit like any other.
        s.on_exit(3, "exit status: 0");
        s.on_exit(4, "exit status: 0");
        assert!(s.respawn_at.is_none());
        s.publish();
        assert!(s.state.draining.is_empty());
    }

    #[tokio::test]
    async fn a_resumed_supervisor_whose_new_worker_fails_keeps_the_adopted_one() {
        let (_d, mut s, log) = sup();
        s.adopt(resumed(vec![rw(4, 8, true)]));
        s.start(Why::Upgrade);
        s.on_exit(5, "exit status: 1");
        assert_eq!(take(&log), ["4 adopt", "5 spawn"]);
        let u = s.state.last_upgrade.clone().unwrap();
        assert!(!u.ok);
        let e = u.error.unwrap();
        assert!(
            e.contains("now runs") && e.contains("previous worker keeps serving"),
            "{e}"
        );
        assert!(!s.state.last_restart.clone().unwrap().ok);
        assert!(s.respawn_at.is_none(), "generation 4 still serves");
        s.publish();
        assert_eq!(s.state.generation, 4);
        assert!(!s.state.upgrading);
    }

    #[tokio::test]
    async fn a_worker_that_cannot_be_adopted_is_left_to_drain() {
        let (_d, mut s, log) = sup();
        s.adopt(resumed(vec![rw(4, -1, true)]));
        s.start(Why::Upgrade);
        s.on_message(5, ready(5));
        assert_eq!(take(&log), ["4 adopt", "5 spawn", "5 active"]);
        assert!(s.state.last_upgrade.clone().unwrap().ok);
    }
}
