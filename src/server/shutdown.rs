//! Serving and graceful drain.
//!
//! First signal: stop taking inference work but let in-flight requests (streams included) finish,
//! for up to [`DRAIN_TIMEOUT`]. While draining the listener stays bound and answers every new
//! connection with a 503 (`/health` says `draining`), so a second instance cannot slip in on the
//! port and a client's retry gets a clean "overloaded" instead of a refused connection. A second
//! signal, or the timeout, ends the drain immediately.

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
    force: BoxFuture<'static, ()>,
}

impl Default for ServeOpts {
    fn default() -> Self {
        ServeOpts {
            drain_timeout: DRAIN_TIMEOUT,
            force: Box::pin(std::future::pending()),
        }
    }
}

impl ServeOpts {
    pub fn with_drain_timeout(mut self, d: Duration) -> Self {
        self.drain_timeout = d;
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
    use tokio::signal::unix::{signal, SignalKind};
    let mut term = signal(SignalKind::terminate())?;
    let mut int = signal(SignalKind::interrupt())?;
    let (tx, rx) = watch::channel(0u32);
    tokio::spawn(async move {
        loop {
            let name = tokio::select! {
                _ = term.recv() => "SIGTERM",
                _ = int.recv() => "SIGINT",
            };
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
    let listener = Arc::new(listener);
    let (started_tx, started_rx) = oneshot::channel::<()>();
    let flag = state.clone();
    let signal = async move {
        shutdown.await;
        flag.runtime.begin_drain();
        let _ = started_tx.send(());
    };
    let main = axum::serve(Shared(listener.clone()), app)
        .with_graceful_shutdown(signal)
        .into_future();
    let mut main = pin!(main);

    tokio::select! {
        r = &mut main => {
            r.context("server stopped")?;
            return Ok(Outcome::Drained);
        }
        _ = started_rx => {}
    }

    tracing::info!(
        pending = state.runtime.pending(),
        timeout_secs = opts.drain_timeout.as_secs(),
        "draining: waiting for in-flight requests (signal again to force)"
    );
    // axum's graceful shutdown dropped its accept loop; keep the port answering meanwhile.
    let holder =
        tokio::spawn(axum::serve(Shared(listener.clone()), draining_router()).into_future());
    let outcome = tokio::select! {
        r = &mut main => {
            r.context("server failed while draining")?;
            Outcome::Drained
        }
        _ = tokio::time::sleep(opts.drain_timeout) => Outcome::TimedOut,
        _ = opts.force => Outcome::Forced,
    };
    holder.abort();
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
    Ok(outcome)
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

/// A listener shared by the serving and the draining server. `TCP_NODELAY` keeps SSE chunks from
/// waiting on Nagle.
struct Shared(Arc<TcpListener>);

impl Listener for Shared {
    type Io = TcpStream;
    type Addr = SocketAddr;

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        loop {
            match self.0.accept().await {
                Ok((stream, addr)) => {
                    let _ = stream.set_nodelay(true);
                    return (stream, addr);
                }
                // Aborted handshakes retry at once; resource exhaustion (EMFILE) must not spin.
                Err(_) => tokio::time::sleep(Duration::from_millis(50)).await,
            }
        }
    }

    fn local_addr(&self) -> io::Result<Self::Addr> {
        self.0.local_addr()
    }
}
