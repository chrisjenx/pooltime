//! Shared integration-test helpers: the scriptable fake upstream, a temp-dir pool fixture with
//! one Anthropic account, and process-wide JSON log capture.
#![allow(dead_code)]

pub mod fake_anthropic;
pub mod fake_openai;
pub mod fake_provider;
pub mod procs;
pub mod sockets;

use std::collections::HashMap;
use std::io::Write;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use bytes::Bytes;
use http::HeaderMap;
use pooltime::accounts::{AccountMeta, Credentials, Registry, StateBook};
use pooltime::config::{AnthropicCfg, Paths};
use pooltime::models::{Route, RouteDef};
use pooltime::providers::anthropic::{AnthropicProvider, OverloadPolicy, Sleeper};
use pooltime::providers::{InboundRequest, Provider, ProviderId};
use pooltime::store::Store;
use pooltime::upstream::{Limits, Upstream};
use time::OffsetDateTime;

#[allow(unused_imports)]
pub use fake_anthropic::{FakeAnthropic, Hold, Recorded, Reply, MESSAGES, TOKEN};

pub const ACCESS: &str = "acct-access-token-SECRET";
pub const REFRESH: &str = "acct-refresh-token-SECRET";

/// Temp pool home with one Anthropic account (`work`) whose creds point at nothing real.
pub struct Pool {
    pub dir: tempfile::TempDir,
    pub reg: Arc<Registry>,
    pub store: Arc<Store>,
    pub state: Arc<StateBook>,
    pub meta: AccountMeta,
    pub up: Arc<Upstream>,
}

impl Pool {
    pub fn new() -> Pool {
        // Tests using the fixture log through the capture subscriber from their first event.
        capture_logs();
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::under(dir.path());
        paths.ensure().unwrap();
        let reg = Arc::new(Registry::load(&paths).unwrap());
        let store = Arc::new(Store::open(&dir.path().join("pooltime.db")).unwrap());
        let state = Arc::new(StateBook::load(store.clone()).unwrap());
        let meta = AccountMeta::new("work".parse().unwrap(), ProviderId::Anthropic);
        reg.upsert(meta.clone()).unwrap();
        reg.write_creds(&meta.id, &creds(ACCESS, 3600)).unwrap();
        Pool {
            dir,
            reg,
            store,
            state,
            meta,
            up: Arc::new(Upstream::new()),
        }
    }

    /// Provider whose API base and token URL are `fake`, with the given overload policy.
    pub fn provider(&self, fake: &FakeAnthropic, policy: OverloadPolicy) -> AnthropicProvider {
        let cfg = AnthropicCfg {
            api_base: fake.url(),
            token_url: format!("{}{}", fake.url(), TOKEN),
            ..AnthropicCfg::default()
        };
        AnthropicProvider::new(cfg, self.reg.clone(), self.state.clone(), self.up.clone())
            .with_overload_policy(policy)
    }

    pub fn creds(&self) -> Credentials {
        self.reg.read_creds(&self.meta.id).unwrap().unwrap()
    }

    /// Add (or replace) an account `name` of `provider` with the given priority and access token.
    pub fn add_account(
        &self,
        name: &str,
        provider: ProviderId,
        priority: u32,
        access: &str,
    ) -> AccountMeta {
        let mut meta = AccountMeta::new(name.parse().unwrap(), provider);
        meta.priority = priority;
        self.reg.upsert(meta.clone()).unwrap();
        let mut c = creds(access, 3600);
        c.provider = provider;
        self.reg.write_creds(&meta.id, &c).unwrap();
        meta
    }

    /// Remove the default `work` account.
    pub fn without_default_account(self) -> Pool {
        self.reg.remove(&self.meta.id).unwrap();
        self
    }

    /// Start the real HTTP server on 127.0.0.1:0 over this pool (providers from `opts`).
    pub async fn boot(&self, opts: Boot) -> Booted {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let mut cfg = pooltime::config::Config {
            port: addr.port(),
            proxy_key: opts.proxy_key,
            ..pooltime::config::Config::default()
        };
        if let Some(edit) = opts.configure {
            edit(&mut cfg);
        }
        let reservations = pooltime::routing::reserve::Reservations::new(HashMap::from([
            (ProviderId::Anthropic, cfg.routing.anthropic_max_inflight),
            (ProviderId::OpenAi, cfg.routing.codex_max_inflight),
        ]));
        let paths = Paths::under(self.dir.path());
        let sessions = Arc::new(pooltime::routing::sessions::Sessions::load(
            self.store.clone(),
            Duration::from_secs(1800),
        ));
        let app = Arc::new(
            pooltime::server::AppState::new(
                cfg,
                paths,
                self.reg.clone(),
                self.state.clone(),
                sessions,
                reservations,
                opts.models
                    .unwrap_or_else(pooltime::models::ModelTable::defaults),
                opts.providers,
                self.store.clone(),
                "mgmt-test-key".into(),
            )
            .with_upstream(self.up.clone())
            .with_fixture_state(opts.fixture_state),
        );
        let mut router = pooltime::server::router::routes(app.clone());
        if let Some(extra) = opts.extra {
            router = extra(router);
        }
        let router = pooltime::server::router::finish(router, app.clone());
        let (shutdown, rx) = tokio::sync::oneshot::channel::<()>();
        let (force, force_rx) = tokio::sync::oneshot::channel::<()>();
        let serve_opts = pooltime::server::ServeOpts::default()
            .with_drain_timeout(opts.drain_timeout)
            .with_force(async move {
                let _ = force_rx.await;
            });
        let join = tokio::spawn(pooltime::server::serve_listener(
            listener,
            app.clone(),
            router,
            async move {
                let _ = rx.await;
            },
            serve_opts,
        ));
        Booted {
            addr,
            app,
            shutdown: Some(shutdown),
            force: Some(force),
            join,
        }
    }
}

/// A config edit applied by [`Pool::boot`].
pub type ConfigEdit = Box<dyn FnOnce(&mut pooltime::config::Config) + Send>;

/// Knobs for [`Pool::boot`].
pub struct Boot {
    pub proxy_key: Option<String>,
    pub drain_timeout: Duration,
    /// Adds test-only routes before the standard layers are applied.
    pub extra: Option<Box<dyn FnOnce(axum::Router) -> axum::Router + Send>>,
    /// Providers the inference handler may use (none by default).
    pub providers: HashMap<ProviderId, Arc<dyn Provider>>,
    /// Model table (bundled defaults when `None`).
    pub models: Option<pooltime::models::ModelTable>,
    /// Edits the daemon config after the port/proxy key are set.
    pub configure: Option<ConfigEdit>,
    /// Serve this document from `/api/v1/status` (the `POOLTIME_FIXTURE_STATE` hook).
    pub fixture_state: Option<serde_json::Value>,
}

impl Default for Boot {
    fn default() -> Self {
        Boot {
            proxy_key: None,
            drain_timeout: Duration::from_secs(30),
            extra: None,
            providers: HashMap::new(),
            models: None,
            configure: None,
            fixture_state: None,
        }
    }
}

impl Boot {
    /// Register `p` under its own id.
    pub fn provider(mut self, p: Arc<dyn Provider>) -> Boot {
        self.providers.insert(p.id(), p);
        self
    }
}

/// A running test server.
pub struct Booted {
    pub addr: std::net::SocketAddr,
    pub app: Arc<pooltime::server::AppState>,
    shutdown: Option<tokio::sync::oneshot::Sender<()>>,
    force: Option<tokio::sync::oneshot::Sender<()>>,
    pub join: tokio::task::JoinHandle<anyhow::Result<pooltime::server::Outcome>>,
}

impl Booted {
    /// First signal: start draining.
    pub fn signal(&mut self) {
        let _ = self.shutdown.take().expect("already signalled").send(());
    }

    /// Second signal: force exit.
    pub fn force(&mut self) {
        let _ = self.force.take().expect("already forced").send(());
    }

    /// Wait for `serve` to return.
    pub async fn finished(self, within: Duration) -> pooltime::server::Outcome {
        tokio::time::timeout(within, self.join)
            .await
            .expect("serve did not return in time")
            .expect("serve task panicked")
            .expect("serve failed")
    }
}

/// A route handler streaming whatever is sent on the returned channel (chunked, no end until the
/// sender is dropped). Mount at `path` through [`Boot::extra`].
pub fn stream_route(
    path: &'static str,
) -> (
    tokio::sync::mpsc::Sender<Bytes>,
    Box<dyn FnOnce(axum::Router) -> axum::Router + Send>,
) {
    let (tx, rx) = tokio::sync::mpsc::channel::<Bytes>(16);
    let rx = Arc::new(Mutex::new(Some(rx)));
    let add = Box::new(move |router: axum::Router| {
        router.route(
            path,
            axum::routing::get(move || {
                let rx = rx.lock().unwrap().take().expect("stream route used once");
                async move {
                    let body = futures_util::stream::unfold(rx, |mut rx| async move {
                        rx.recv()
                            .await
                            .map(|b| (Ok::<_, std::convert::Infallible>(b), rx))
                    });
                    axum::response::Response::new(axum::body::Body::from_stream(body))
                }
            }),
        )
    });
    (tx, add)
}

pub fn creds(access: &str, expires_in: i64) -> Credentials {
    Credentials {
        provider: ProviderId::Anthropic,
        access_token: access.into(),
        refresh_token: Some(REFRESH.into()),
        expires_at: Some(OffsetDateTime::now_utc() + time::Duration::seconds(expires_in)),
        refresh_expires_at: None,
        scopes: vec!["user:inference".into()],
        plan: Some("max".into()),
        tier: None,
        openai_account_id: None,
    }
}

/// Records requested sleeps and returns immediately.
#[derive(Default)]
pub struct RecordingSleeper(pub Mutex<Vec<Duration>>);

impl Sleeper for RecordingSleeper {
    fn sleep(&self, d: Duration) -> futures_util::future::BoxFuture<'static, ()> {
        self.0.lock().unwrap().push(d);
        Box::pin(async {})
    }
}

impl RecordingSleeper {
    pub fn slept(&self) -> Vec<Duration> {
        self.0.lock().unwrap().clone()
    }
}

/// Instant sleeps (recorded) and max jitter, so delays equal their ceilings.
pub fn fast_policy() -> (OverloadPolicy, Arc<RecordingSleeper>) {
    let sleeper = Arc::new(RecordingSleeper::default());
    let policy = OverloadPolicy {
        sleeper: sleeper.clone(),
        jitter: Arc::new(|| 1.0),
        ..OverloadPolicy::default()
    };
    (policy, sleeper)
}

pub fn route(model: &str) -> Route {
    Route {
        requested: model.into(),
        def: RouteDef {
            id: model.into(),
            provider: ProviderId::Anthropic,
            upstream_model: model.into(),
            context_window: None,
            max_context_window: None,
            supported_efforts: vec![],
            effort_map: None,
        },
        effort: None,
    }
}

pub fn limits() -> Limits {
    Limits {
        header_wait: Duration::from_secs(5),
        idle: Duration::from_secs(5),
        deadline: tokio::time::Instant::now() + Duration::from_secs(30),
    }
}

/// Non-streaming inbound request with `body` and `headers`.
pub fn inbound(body: &serde_json::Value, headers: HeaderMap) -> InboundRequest {
    let raw = Bytes::from(serde_json::to_vec(body).unwrap());
    InboundRequest {
        json: body.clone(),
        model: body["model"].as_str().unwrap_or_default().into(),
        stream: body["stream"].as_bool().unwrap_or(false),
        raw_body: raw,
        headers,
        session_user_id: None,
        affinity: true,
        request_id: "req_testinbound".into(),
    }
}

pub fn simple_body() -> serde_json::Value {
    serde_json::json!({
        "model": "claude-sonnet-5",
        "max_tokens": 8,
        "messages": [{"role": "user", "content": "hi"}]
    })
}

pub async fn body_bytes(resp: axum::response::Response) -> Bytes {
    axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap()
}

// ---- log capture ----

#[derive(Clone, Default)]
struct Buf(Arc<Mutex<Vec<u8>>>);

impl Write for Buf {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(b);
        Ok(b.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

static LOGS: OnceLock<Buf> = OnceLock::new();

/// Install (once per test binary) a global JSON subscriber at TRACE that appends every event
/// to an in-memory buffer. Call before the code under test runs.
pub fn capture_logs() {
    LOGS.get_or_init(|| {
        let buf = Buf::default();
        let writer = buf.clone();
        let sub = tracing_subscriber::fmt()
            .json()
            .with_max_level(tracing::Level::TRACE)
            .with_writer(move || writer.clone())
            .finish();
        tracing::subscriber::set_global_default(sub).expect("no other global subscriber");
        buf
    });
}

/// Everything logged so far by any test in this binary.
pub fn captured_logs() -> String {
    let buf = LOGS.get().expect("capture_logs() first");
    String::from_utf8_lossy(&buf.0.lock().unwrap()).into_owned()
}
