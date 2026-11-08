//! The local HTTP server: shared state, routing, auth, health and graceful drain.
//!
//! Layout: [`router`] builds the route table and middleware, [`auth`] guards inference and
//! management traffic, [`models_route`] serves `/v1/models`, [`health`] serves `/health` and the startup preflight, [`shutdown`] owns
//! signal handling and the drain sequence.

pub mod auth;
pub mod classifier;
pub mod fallback;
pub mod health;
pub mod inference;
pub mod models_route;
pub mod router;
pub mod shutdown;

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, RwLock};

use crate::accounts::{Registry, StateBook};
use crate::config::{Config, Paths};
use crate::models::ModelTable;
use crate::providers::{Provider, ProviderId};
use crate::routing::reserve::Reservations;
use crate::routing::sessions::Sessions;
use crate::store::Store;
use crate::upstream::Upstream;

pub use router::router;
pub use shutdown::{serve, serve_listener, serve_until_signal, Outcome, ServeOpts};

/// Everything a request handler can reach. Built once at startup (tests build it from fakes).
pub struct AppState {
    pub cfg: Config,
    pub paths: Paths,
    pub registry: Arc<Registry>,
    pub state: Arc<StateBook>,
    pub sessions: Arc<Sessions>,
    pub reservations: Reservations,
    pub models: RwLock<ModelTable>,
    pub providers: HashMap<ProviderId, Arc<dyn Provider>>,
    pub store: Arc<Store>,
    /// Secret for dashboard/management mutations (never the proxy key).
    pub mgmt_key: String,
    pub runtime: Runtime,
    /// Shared upstream HTTP client handed to every attempt.
    pub upstream: Arc<Upstream>,
    /// Classifier marker-drift warning throttle.
    pub drift: classifier::DriftWarner,
    /// Round-robin cursor for exact score ties (shared by every pick, like bro's).
    pub cursor: AtomicUsize,
}

impl AppState {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        cfg: Config,
        paths: Paths,
        registry: Arc<Registry>,
        state: Arc<StateBook>,
        sessions: Arc<Sessions>,
        reservations: Reservations,
        models: ModelTable,
        providers: HashMap<ProviderId, Arc<dyn Provider>>,
        store: Arc<Store>,
        mgmt_key: String,
    ) -> AppState {
        AppState {
            cfg,
            paths,
            registry,
            state,
            sessions,
            reservations,
            models: RwLock::new(models),
            providers,
            store,
            mgmt_key,
            runtime: Runtime::default(),
            upstream: Arc::new(Upstream::new()),
            drift: classifier::DriftWarner::default(),
            cursor: AtomicUsize::new(0),
        }
    }

    /// Use this upstream client (the daemon shares one with the providers).
    pub fn with_upstream(mut self, upstream: Arc<Upstream>) -> AppState {
        self.upstream = upstream;
        self
    }
}

/// Process-lifecycle flags shared with the middleware: the drain switch and the in-flight count.
#[derive(Debug, Default)]
pub struct Runtime {
    draining: AtomicBool,
    pending: AtomicUsize,
}

impl Runtime {
    pub fn draining(&self) -> bool {
        self.draining.load(Ordering::SeqCst)
    }

    pub fn begin_drain(&self) {
        self.draining.store(true, Ordering::SeqCst);
    }

    /// Requests currently being served (response bodies included), `/health` excluded.
    pub fn pending(&self) -> usize {
        self.pending.load(Ordering::SeqCst)
    }
}

/// One in-flight request. Released by `Drop`, so a cancelled handler and a finished (or
/// abandoned) response body both give the slot back.
pub(crate) struct PendingGuard(Arc<AppState>);

impl PendingGuard {
    pub(crate) fn new(app: Arc<AppState>) -> PendingGuard {
        app.runtime.pending.fetch_add(1, Ordering::SeqCst);
        PendingGuard(app)
    }
}

impl Drop for PendingGuard {
    fn drop(&mut self) {
        self.0.runtime.pending.fetch_sub(1, Ordering::SeqCst);
    }
}
