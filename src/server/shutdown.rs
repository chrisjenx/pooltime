//! Serving and graceful drain.
//!
//! First signal: stop taking inference work but let in-flight requests (streams included) finish,
//! for up to [`DRAIN_TIMEOUT`]. While draining the listener stays bound and answers every new
//! connection with a 503 (`/health` says `draining`), so a second instance cannot slip in on the
//! port and a client's retry gets a clean "overloaded" instead of a refused connection. A second
//! signal, or the timeout, ends the drain immediately.
//!
//! A worker run by the supervisor (`serve --supervise`) can instead be told to hand off: a newer
//! generation accepts on the same listening socket, so this one stops accepting and drains
//! without holding the port (see [`serve_until`] and [`crate::supervisor`]).

use std::future::{Future, IntoFuture};
use std::io;
use std::net::SocketAddr;
use std::pin::pin;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use axum::routing::get;
use axum::serve::Listener;
use axum::Router;
use futures_util::future::BoxFuture;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{oneshot, watch};

use super::{health, router, AppState};
use crate::config::Config;
use crate::providers::anthropic::anthropic_error;

/// Long by design: a slow restart beats killing a generation mid-stream.
pub const DRAIN_TIMEOUT: Duration = Duration::from_secs(10 * 60);

/// How long a handing-off generation keeps serving its open connections before draining them:
/// a client that connected just before the handoff still gets its request answered.
pub const HANDOFF_GRACE: Duration = Duration::from_secs(2);

/// Why serving stops.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopKind {
    /// Shutting down: drain while the port answers 503.
    Stop,
    /// A newer generation took over the listening socket: stop accepting and drain.
    Handoff,
}

/// How a server run ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// Every in-flight request finished.
    Drained,
    /// The drain budget ran out with requests still in flight.
    TimedOut,
    /// A second signal cut the drain short.
    Forced,
}

/// Drain behaviour. `force` resolves on the second signal; the default never does.
pub struct ServeOpts {
    pub drain_timeout: Duration,
    pub handoff_grace: Duration,
    force: BoxFuture<'static, ()>,
}

impl Default for ServeOpts {
    fn default() -> Self {
        ServeOpts {
            drain_timeout: DRAIN_TIMEOUT,
            handoff_grace: HANDOFF_GRACE,
            force: Box::pin(std::future::pending()),
        }
    }
}

impl ServeOpts {
    pub fn with_drain_timeout(mut self, d: Duration) -> Self {
        self.drain_timeout = d;
        self
    }

    pub fn with_handoff_grace(mut self, d: Duration) -> Self {
        self.handoff_grace = d;
        self
    }

    pub fn with_force(mut self, f: impl Future<Output = ()> + Send + 'static) -> Self {
        self.force = Box::pin(f);
        self
    }
}

/// SIGTERM/SIGINT counter: [`Signals::first`] starts the drain, [`Signals::second`] forces exit.
/// Handlers are installed eagerly so no signal is lost between the two.
pub struct Signals {
    rx: watch::Receiver<u32>,
}

pub fn install_signals() -> io::Result<Signals> {
    install(false)
}

/// For a worker run by the supervisor: `SIGINT` (a terminal's Ctrl-C reaches the whole process
/// group) and `SIGHUP` are ignored, and `SIGTERM` is only counted: the worker acts on it only
/// once its supervisor is gone (see `daemon::serve_supervised`).
pub fn install_signals_supervised() -> io::Result<Signals> {
    install(true)
}

fn install(supervised: bool) -> io::Result<Signals> {
    use tokio::signal::unix::{signal, SignalKind};
    let mut term = signal(SignalKind::terminate())?;
    let mut int = signal(SignalKind::interrupt())?;
    let mut hup = if supervised {
        Some(signal(SignalKind::hangup())?)
    } else {
        None
    };
    let (tx, rx) = watch::channel(0u32);
    tokio::spawn(async move {
        loop {
            let name = tokio::select! {
                _ = term.recv() => "SIGTERM",
                _ = int.recv() => "SIGINT",
                _ = async { hup.as_mut().expect("guarded").recv().await }, if hup.is_some() => "SIGHUP",
            };
            if supervised && name != "SIGTERM" {
                tracing::debug!(signal = name, "signal ignored: the supervisor handles it");
                continue;
            }
            tracing::info!(signal = name, "signal received");
            tx.send_modify(|n| *n += 1);
        }
    });
    Ok(Signals { rx })
}

impl Signals {
    fn nth(&self, n: u32) -> impl Future<Output = ()> + Send + 'static {
        let mut rx = self.rx.clone();
        async move {
            let _ = rx.wait_for(|c| *c >= n).await;
        }
    }

    pub fn first(&self) -> impl Future<Output = ()> + Send + 'static {
        self.nth(1)
    }

    pub fn second(&self) -> impl Future<Output = ()> + Send + 'static {
        self.nth(2)
    }

    /// The running count of counted signals.
    pub fn counter(&self) -> watch::Receiver<u32> {
        self.rx.clone()
    }
}

/// Bind `cfg.host:cfg.port`.
pub async fn bind(cfg: &Config) -> anyhow::Result<TcpListener> {
    let addr = SocketAddr::new(cfg.host, cfg.port);
    TcpListener::bind(addr)
        .await
        .with_context(|| format!("binding {addr} (is another process using the port?)"))
}

/// Bind, serve the standard router and drain when `shutdown` resolves. A second signal is not
/// wired here; use [`serve_until_signal`] (daemon) or [`serve_listener`] with [`ServeOpts`].
pub async fn serve(
    state: Arc<AppState>,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> anyhow::Result<()> {
    let listener = bind(&state.cfg).await?;
    let app = router::router(state.clone());
    serve_listener(listener, state, app, shutdown, ServeOpts::default()).await?;
    Ok(())
}

/// The daemon entry point: serve until SIGTERM/SIGINT, drain, and let a second signal force exit.
pub async fn serve_until_signal(state: Arc<AppState>) -> anyhow::Result<Outcome> {
    let signals = install_signals().context("installing signal handlers")?;
    let listener = bind(&state.cfg).await?;
    let app = router::router(state.clone());
    let opts = ServeOpts::default().with_force(signals.second());
    serve_listener(listener, state, app, signals.first(), opts).await
}

/// Serve `app` on `listener` until `shutdown` resolves, then drain (see the module docs).
pub async fn serve_listener(
    listener: TcpListener,
    state: Arc<AppState>,
    app: Router,
    shutdown: impl Future<Output = ()> + Send + 'static,
    opts: ServeOpts,
) -> anyhow::Result<Outcome> {
    let stop = async move {
        shutdown.await;
        StopKind::Stop
    };
    Ok(serve_until(listener, state, app, stop, opts).await?.0)
}

/// Serve `app` on `listener` until `shutdown` says how to stop, then drain:
///
/// - [`StopKind::Stop`]: as in the module docs (the port keeps answering 503 meanwhile).
/// - [`StopKind::Handoff`]: another process serves the same listening socket now. Stop
///   accepting and close our copy of the listener at once (queued connections stay in the
///   shared backlog for the other process), keep serving the open connections for
///   [`ServeOpts::handoff_grace`], then drain them without answering for the port.
pub async fn serve_until(
    listener: TcpListener,
    state: Arc<AppState>,
    app: Router,
    shutdown: impl Future<Output = StopKind> + Send + 'static,
    opts: ServeOpts,
) -> anyhow::Result<(Outcome, StopKind)> {
    let gate = Gate::new(listener);
    let (started_tx, started_rx) = oneshot::channel::<StopKind>();
    let flag = state.clone();
    let closer = gate.clone();
    let grace = opts.handoff_grace;
    let signal = async move {
        let kind = shutdown.await;
        if kind == StopKind::Handoff {
            closer.close();
            tracing::info!(
                pending = flag.runtime.pending(),
                "handing off: no longer accepting, the new generation takes new connections"
            );
            tokio::time::sleep(grace).await;
        }
        flag.runtime.begin_drain();
        let _ = started_tx.send(kind);
    };
    let main = axum::serve(gate.acceptor(), app)
        .with_graceful_shutdown(signal)
        .into_future();
    let mut main = pin!(main);

    let kind = tokio::select! {
        r = &mut main => {
            r.context("server stopped")?;
            return Ok((Outcome::Drained, StopKind::Stop));
        }
        k = started_rx => k.unwrap_or(StopKind::Stop),
    };

    tracing::info!(
        pending = state.runtime.pending(),
        timeout_secs = opts.drain_timeout.as_secs(),
        "draining: waiting for in-flight requests (signal again to force)"
    );
    // axum's graceful shutdown dropped its accept loop; keep the port answering meanwhile. A
    // handoff holds nothing: the new generation answers.
    let holder = (kind == StopKind::Stop)
        .then(|| tokio::spawn(axum::serve(gate.acceptor(), draining_router()).into_future()));
    drop(gate);
    let outcome = tokio::select! {
        r = &mut main => {
            r.context("server failed while draining")?;
            Outcome::Drained
        }
        _ = tokio::time::sleep(opts.drain_timeout) => Outcome::TimedOut,
        _ = opts.force => Outcome::Forced,
    };
    if let Some(h) = holder {
        h.abort();
    }
    match outcome {
        Outcome::Drained => tracing::info!("drained, exiting"),
        Outcome::TimedOut => tracing::warn!(
            pending = state.runtime.pending(),
            "drain timeout reached with requests in flight, exiting"
        ),
        Outcome::Forced => tracing::warn!(
            pending = state.runtime.pending(),
            "second signal, exiting without waiting for in-flight requests"
        ),
    }
    Ok((outcome, kind))
}

/// Answers during the drain: health says `draining`, everything else is a retryable 503.
fn draining_router() -> Router {
    Router::new()
        .route("/health", get(|| async { health::draining_response() }))
        .fallback(|| async {
            anthropic_error(
                503,
                "overloaded_error",
                "pooltime is shutting down; retry shortly",
            )
        })
}

/// The listening socket, shared by the serving and the draining server. [`Gate::close`] (a
/// handoff) stops every accept loop for good and drops the socket once they let go of it.
#[derive(Clone)]
struct Gate {
    listener: Arc<std::sync::Mutex<Option<Arc<TcpListener>>>>,
    closed: Arc<watch::Sender<bool>>,
}

impl Gate {
    fn new(listener: TcpListener) -> Gate {
        Gate {
            listener: Arc::new(std::sync::Mutex::new(Some(Arc::new(listener)))),
            closed: Arc::new(watch::channel(false).0),
        }
    }

    fn current(&self) -> Option<Arc<TcpListener>> {
        self.listener
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    fn close(&self) {
        self.listener
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        self.closed.send_replace(true);
    }

    fn acceptor(&self) -> Acceptor {
        Acceptor {
            gate: self.clone(),
            closed: self.closed.subscribe(),
            addr: self.current().and_then(|l| l.local_addr().ok()),
        }
    }
}

/// One accept loop's handle on the [`Gate`]. `TCP_NODELAY` keeps SSE chunks from waiting on
/// Nagle.
struct Acceptor {
    gate: Gate,
    closed: watch::Receiver<bool>,
    addr: Option<SocketAddr>,
}

impl Listener for Acceptor {
    type Io = TcpStream;
    type Addr = SocketAddr;

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        loop {
            let open = !*self.closed.borrow();
            let Some(l) = self.gate.current().filter(|_| open) else {
                // Closed for good (handoff): never accept again.
                return std::future::pending().await;
            };
            let closed = &mut self.closed;
            tokio::select! {
                biased;
                _ = async { closed.wait_for(|c| *c).await.is_ok() } => continue,
                r = l.accept() => match r {
                    Ok((stream, addr)) => {
                        let _ = stream.set_nodelay(true);
                        return (stream, addr);
                    }
                    // Aborted handshakes retry at once; resource exhaustion (EMFILE) must not
                    // spin.
                    Err(_) => tokio::time::sleep(Duration::from_millis(50)).await,
                },
            }
        }
    }

    fn local_addr(&self) -> io::Result<Self::Addr> {
        self.addr
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotConnected, "listener closed"))
    }
}
