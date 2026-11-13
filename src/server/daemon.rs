//! `pooltime serve`: assembles the modules into a running daemon.
//!
//! Boot order: data dir and config (flags win over file and env) -> logging -> single-instance
//! and shadow-run guards -> store, registry, state, sessions, providers -> background tasks ->
//! serve until SIGTERM/SIGINT. The background tasks stop with the server and never take the
//! daemon down: they log failures (no tokens, no bodies) and carry on.
//!
//! Run by the supervisor (`--listen-fd`/`--control-fd`, see [`crate::supervisor`]) the daemon
//! is one worker generation: it serves the inherited socket, skips the "already running"
//! preflight (the previous generation still answers there), starts its background tasks only
//! when made active, and hands them back on `handoff` before draining.

use std::collections::HashMap;
use std::io::IsTerminal;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use time::OffsetDateTime;
use tokio::task::JoinHandle;

use super::handoff::{until, Control, Inherited};
use super::shutdown::install_signals_supervised;
use super::{
    health, mgmt, serve_until, serve_until_signal, AppState, Outcome, ServeOpts, StopKind,
};
use crate::accounts::{Registry, StateBook};
use crate::claude_code::statusline::StatusCache;
use crate::cli::ServeFlags;
use crate::config::{Config, Paths};
use crate::models::ModelTable;
use crate::providers::anthropic::{
    spawn_usage_sweeper, AnthropicProvider, OverloadPolicy, SWEEP_INTERVAL,
};
use crate::providers::openai::{self, OpenAiProvider, CATALOG_REFRESH_EVERY};
use crate::providers::{Provider, ProviderId};
use crate::routing::reserve::Reservations;
use crate::routing::sessions::Sessions;
use crate::store::{prune, Store};
use crate::supervisor::proto::FromWorker;
use crate::upstream::Upstream;

/// How often idle sticky-session pins are dropped.
const SESSION_PRUNE_EVERY: Duration = Duration::from_secs(60);
/// How long shutdown waits for in-flight token refreshes.
const REFRESH_DRAIN: Duration = Duration::from_secs(30);
/// The refresh grace on a forced exit.
const FORCED_REFRESH_DRAIN: Duration = Duration::from_secs(2);
/// How long shutdown waits for queued database writes.
const STORE_FLUSH: Duration = Duration::from_secs(5);
/// How often journal/usage-history retention runs.
const RETENTION_EVERY: Duration = Duration::from_secs(60 * 60);

/// Run the daemon until SIGTERM/SIGINT has drained (or a second signal forces exit).
///
/// Fails fast, before serving anything, when another pooltime is already on the port, when a
/// bro pool would share imported accounts (unless `flags.i_stopped_bro`), or when the port
/// cannot be bound.
pub async fn run_daemon(paths: Paths, flags: ServeFlags) -> anyhow::Result<()> {
    // A supervised worker: own the inherited sockets before anything else opens a file.
    let inherited = match (flags.listen_fd, flags.control_fd) {
        (Some(l), Some(c)) => Some(Inherited::take(l, c)?),
        (None, None) => None,
        _ => anyhow::bail!("--listen-fd and --control-fd go together"),
    };
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
    // A worker serves the supervisor's socket: its address is the one in effect.
    let configured = SocketAddr::new(cfg.host, cfg.port);
    let bound = match &inherited {
        Some(i) => Some(i.listener.local_addr()?),
        None => None,
    };
    if let Some(a) = bound {
        (cfg.host, cfg.port) = (a.ip(), a.port());
    }
    cfg.validate()?;
    let _log = crate::logging::init(&paths, &cfg.retention, std::io::stderr().is_terminal())?;
    crate::logging::install_panic_hook();
    if bound.is_some_and(|a| a != configured) {
        tracing::warn!(
            %configured, serving = %SocketAddr::new(cfg.host, cfg.port),
            "the configured address changed: `pooltime service install` moves the service to it"
        );
    }

    // Another generation answering on the inherited socket is expected, not a conflict.
    if inherited.is_none() {
        health::preflight(&cfg).await?;
    }
    let registry = Arc::new(Registry::load(&paths)?);
    health::check_bro_shadow(&registry, health::BRO_PORT, flags.i_stopped_bro).await?;

    let store = Arc::new(Store::open(&paths.db)?);
    let state = Arc::new(state_book(store.clone(), &cfg.timeouts)?);
    let sessions = Arc::new(Sessions::load(store.clone(), cfg.timeouts.session_idle));
    let reservations = Reservations::new(HashMap::from([
        (ProviderId::Anthropic, cfg.routing.anthropic_max_inflight),
        (ProviderId::OpenAi, cfg.routing.codex_max_inflight),
    ]));
    let models = ModelTable::load(&paths)?;
    let mgmt_key = mgmt::ensure_mgmt_key(&paths)?;
    let mcp_key = crate::mcp::ensure_mcp_key(&paths)?;
    let fixture_state = load_fixture_state()?;
    let upstream = Arc::new(Upstream::new());

    let t = &cfg.timeouts;
    let overload = OverloadPolicy {
        max_retries: t.overload_retry_max,
        base: t.overload_retry_base,
        cap: t.overload_retry_cap,
        ..OverloadPolicy::default()
    };
    let anthropic = Arc::new(
        AnthropicProvider::new(
            cfg.anthropic.clone(),
            registry.clone(),
            state.clone(),
            upstream.clone(),
        )
        .with_refresh_timing(t.token_refresh_skew, t.token_refresh_timeout)
        .with_rate_limit_cooldown(t.rate_limit_cooldown)
        .with_overload_policy(overload.clone()),
    );
    // Codex: same refresh timing, cooldown and backoff as Anthropic (5xx terminal after the
    // same-account backoff, docs/parity.md); keep-alive from `timeouts.stream_keepalive`.
    let openai = Arc::new(
        OpenAiProvider::new(
            cfg.openai.clone(),
            registry.clone(),
            state.clone(),
            upstream.clone(),
        )
        .with_refresh_timing(t.token_refresh_skew, t.token_refresh_timeout)
        .with_rate_limit_cooldown(t.rate_limit_cooldown)
        .with_overload_policy(overload)
        .with_keepalive(t.stream_keepalive)
        // Codex usage polling shares the `[anthropic]` usage knobs (like bro's single setting).
        .with_usage_timing(
            cfg.anthropic.usage_refresh,
            cfg.anthropic.usage_ttl,
            cfg.anthropic.usage_fetch_timeout,
        ),
    );
    let providers: HashMap<ProviderId, Arc<dyn Provider>> = HashMap::from([
        (
            ProviderId::Anthropic,
            anthropic.clone() as Arc<dyn Provider>,
        ),
        (ProviderId::OpenAi, openai.clone() as Arc<dyn Provider>),
    ]);

    let (host, port) = (cfg.host, cfg.port);
    let app = Arc::new(
        AppState::new(
            cfg,
            paths,
            registry,
            state,
            sessions.clone(),
            reservations,
            models,
            providers,
            store.clone(),
            mgmt_key,
        )
        .with_upstream(upstream)
        .with_mcp_key(mcp_key)
        .with_fixture_state(fixture_state),
    );

    tracing::info!(
        %host, port,
        version = env!("CARGO_PKG_VERSION"),
        accounts = app.registry.list().len(),
        supervised = inherited.is_some(),
        "pooltime starting"
    );
    let spawn = {
        let app = app.clone();
        move || spawn_background(app, anthropic, openai, sessions)
    };
    let outcome = match inherited {
        None => {
            let bg = spawn();
            let outcome = serve_until_signal(app.clone()).await;
            // Status lines say `pool down` right away instead of once the cache goes stale.
            bg.stop().await;
            outcome
        }
        Some(inherited) => serve_supervised(app.clone(), inherited, spawn).await,
    };
    // A refresh that already spent a rotating refresh token must get the new one to disk.
    let refreshes = app.providers.values().map(|p| {
        let p = p.clone();
        Box::pin(async move { p.drain_refreshes().await }) as _
    });
    let (refresh_bound, flush_bound) = shutdown_bounds(outcome.as_ref().ok());
    if !drain_within(refreshes.collect(), refresh_bound).await {
        tracing::warn!("token refreshes still running at exit");
    }
    // Journal, state and session writes queued so far.
    if !flush_store(&app.store, flush_bound).await {
        tracing::warn!("database writes still queued at exit");
    }
    match outcome? {
        Outcome::Drained => tracing::info!("pooltime stopped"),
        Outcome::TimedOut | Outcome::Forced => {
            tracing::warn!("pooltime stopped with requests in flight")
        }
    }
    Ok(())
}

/// The daemon's background work: pollers, profile sync, health and alerts, sampler, pruning and
/// the status-line cache. Exactly one process runs it (the single writer of those files and
/// rows): a supervised worker only once the supervisor makes it the active generation.
struct Background {
    tasks: Tasks,
    cache: StatusCache,
}

/// How long a handing-off worker waits for its background work to stop.
const RELEASE_BOUND: Duration = Duration::from_secs(5);

impl Background {
    /// Shutting down: stop, and delete the status cache so status lines say `pool down` at once.
    async fn stop(self) {
        drop(self.tasks);
        self.cache.shutdown().await;
    }
}

fn spawn_background(
    app: Arc<AppState>,
    anthropic: Arc<AnthropicProvider>,
    openai: Arc<OpenAiProvider>,
    sessions: Arc<Sessions>,
) -> Background {
    let store = app.store.clone();
    let retention_cfg = app.cfg.retention.clone();
    let tasks = Tasks(vec![
        spawn_usage_sweeper(anthropic, SWEEP_INTERVAL),
        openai::spawn_usage_sweeper(openai, SWEEP_INTERVAL),
        // Codex catalog discovery: at boot (providers are registered by now), then hourly. New
        // routes go live in the model table and trigger a profile re-sync.
        spawn_every("codex catalog refresh", CATALOG_REFRESH_EVERY, {
            let app = app.clone();
            move || {
                let app = app.clone();
                async move { openai::refresh_codex_catalog(&app).await }
            }
        }),
        crate::claude_code::spawn_profile_sync(app.clone()),
        // Health tick: one probe refresh per passed login expiry (spec §16).
        spawn_every("health tick", crate::health::HEALTH_TICK, {
            let app = app.clone();
            let probe = Arc::new(crate::health::ExpiryProbe::default());
            move || {
                let (app, probe) = (app.clone(), probe.clone());
                async move {
                    probe.tick(&app, OffsetDateTime::now_utc()).await;
                }
            }
        }),
        // Expiry / dead-account notifications (spec §4 "Alerts").
        crate::health::alerts::spawn_alerts(
            app.clone(),
            Arc::new(crate::health::notify::SystemNotifier::from_env()),
        ),
        spawn_every("session prune", SESSION_PRUNE_EVERY, move || {
            sessions.prune(OffsetDateTime::now_utc());
            std::future::ready(())
        }),
        // Weekly-burn history (spec §5): 7-day utilization samples on every state change.
        crate::analytics::sampler::spawn_sampler(
            app.registry.clone(),
            app.state.clone(),
            store.clone(),
        ),
        spawn_every("retention prune", RETENTION_EVERY, move || {
            let (store, cfg) = (store.clone(), retention_cfg.clone());
            async move {
                // Local time: usage-history buckets follow the local ISO weeks of the burn report.
                let now = OffsetDateTime::now_utc();
                let now = now.to_offset(crate::analytics::local_offset(now));
                match prune(&store, &cfg, now).await {
                    Ok(r) => tracing::debug!(report = ?r, "retention prune finished"),
                    Err(e) => tracing::warn!(error = %e, "retention prune failed"),
                }
            }
        }),
    ]);
    // run/status-cache.json for `pooltime statusline` (spec §16).
    let cache = crate::claude_code::statusline::spawn_status_cache(app);
    Background { tasks, cache }
}

/// Handing off to a newer generation: stop the background tasks and wait for the blocking work
/// they started, stop persisting account state, and get every queued write to disk, so that
/// after `released` the next generation is the only writer (it reloads account state then).
/// The status cache is left for it to update. Bounded by [`RELEASE_BOUND`].
async fn release(app: &AppState, bg: Option<Background>) {
    let work = async {
        if let Some(bg) = bg {
            bg.tasks.stop().await;
            bg.cache.release().await;
        }
        crate::background::idle().await;
        app.state.stop_persisting();
        let _ = app.store.exec(|_| Ok(())).await;
    };
    if tokio::time::timeout(RELEASE_BOUND, work).await.is_err() {
        tracing::warn!("background work still stopping at handoff");
    }
    app.state.stop_persisting();
}

/// A worker run by the supervisor: serve the inherited socket at once and follow the
/// supervisor's orders over the control socket. `active`: reload account state from the
/// database and start the background tasks. `handoff`: [`release`] them, answer `released`,
/// stop accepting and drain. `stop`: drain as an unsupervised daemon does; `force`: stop now.
///
/// OS signals are ignored while the supervisor is there (it receives the service manager's
/// signals and passes them on as orders), so a `SIGTERM` sent to the whole process group is
/// never counted twice. If the control socket closes (the supervisor died), the worker hands
/// off on its own, and from then on its second `SIGTERM` forces exit.
async fn serve_supervised(
    app: Arc<AppState>,
    inherited: Inherited,
    spawn: impl FnOnce() -> Background + Send + 'static,
) -> anyhow::Result<Outcome> {
    let signals = install_signals_supervised().context("installing signal handlers")?;
    let ctl = Arc::new(Control::start(inherited.control)?);
    let listener = tokio::net::TcpListener::from_std(inherited.listener)?;
    let slot: Arc<std::sync::Mutex<Option<Background>>> = Arc::default();
    let life = tokio::spawn({
        let (ctl, slot, app) = (ctl.clone(), slot.clone(), app.clone());
        async move {
            let mut rx = ctl.orders();
            let o = until(&mut rx, |o| o.active || o.ending().is_some()).await;
            if o.ending().is_none() {
                // What the previous generation learned before it released.
                let book = app.state.clone();
                let now = OffsetDateTime::now_utc();
                match tokio::task::spawn_blocking(move || book.reload(now)).await {
                    Ok(Ok(())) => {}
                    Ok(Err(e)) => {
                        tracing::warn!(error = %format!("{e:#}"), "could not reload account state")
                    }
                    Err(e) => tracing::warn!(error = %e, "account state reload failed"),
                }
                *slot.lock().unwrap_or_else(|e| e.into_inner()) = Some(spawn());
                tracing::info!("active generation: background tasks started");
            }
            let o = until(&mut rx, |o| o.ending().is_some()).await;
            if o.ending() == Some(StopKind::Handoff) {
                let bg = slot.lock().unwrap_or_else(|e| e.into_inner()).take();
                release(&app, bg).await;
                tracing::info!("background work released for the handoff");
                ctl.send(&FromWorker::Released);
            }
        }
    });

    let mut rx = ctl.orders();
    let shutdown = async move {
        until(&mut rx, |o| o.ending().is_some())
            .await
            .ending()
            .unwrap_or(StopKind::Stop)
    };
    // Spawned now, not when the drain first polls it (after the handoff grace): SIGTERMs are
    // counted from the moment the supervisor is found gone.
    let mut rx = ctl.orders();
    let mut sigs = signals.counter();
    let (forced_tx, mut forced) = tokio::sync::watch::channel(false);
    let watch_force = tokio::spawn(async move {
        let o = until(&mut rx, |o| o.force || o.orphaned).await;
        if !o.force {
            // Orphaned and draining on its own: the second SIGTERM from here on forces exit.
            let from = *sigs.borrow_and_update();
            let _ = sigs.wait_for(|n| *n >= from + 2).await;
        }
        let _ = forced_tx.send(true);
    });
    let force = async move {
        let _ = forced.wait_for(|f| *f).await;
    };
    let opts = ServeOpts::default().with_force(force);
    let router = super::router(app.clone());
    let serving = serve_until(listener, app.clone(), router, shutdown, opts);
    ctl.send(&FromWorker::Ready {
        version: env!("CARGO_PKG_VERSION").into(),
    });
    let served = serving.await;
    watch_force.abort();
    if matches!(served, Ok((_, StopKind::Handoff))) {
        // Released long ago (it races only the grace period); let it finish its message.
        let _ = tokio::time::timeout(RELEASE_BOUND, life).await;
    } else {
        life.abort();
        let bg = slot.lock().unwrap_or_else(|e| e.into_inner()).take();
        if let Some(bg) = bg {
            bg.stop().await;
        }
    }
    served.map(|(outcome, _)| outcome)
}

/// The account state book, with the configured counter window and access-denied cooldown.
fn state_book(store: Arc<Store>, t: &crate::config::Timeouts) -> anyhow::Result<StateBook> {
    Ok(StateBook::load(store)?
        .with_usage_window(t.usage_window)
        .with_access_denied_cooldown(t.access_denied_cooldown))
}

/// Wait for every future in `futs`, at most `within`. False when the bound cut it short.
async fn drain_within(
    futs: Vec<std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>>,
    within: Duration,
) -> bool {
    tokio::time::timeout(within, futures_util::future::join_all(futs))
        .await
        .is_ok()
}

/// How long shutdown may wait for in-flight refreshes and for the store queue. A forced exit
/// (second signal) gets a short refresh grace so it stays prompt.
fn shutdown_bounds(outcome: Option<&Outcome>) -> (Duration, Duration) {
    match outcome {
        Some(Outcome::Forced) => (FORCED_REFRESH_DRAIN, STORE_FLUSH),
        _ => (REFRESH_DRAIN, STORE_FLUSH),
    }
}

/// Wait (at most `within`) until every write queued on the store so far is on disk (the writer
/// is FIFO). False when the bound cut it short.
async fn flush_store(store: &Store, within: Duration) -> bool {
    match tokio::time::timeout(within, store.exec(|_| Ok(()))).await {
        Ok(Ok(())) => true,
        Ok(Err(e)) => {
            tracing::warn!(error = %format!("{e:#}"), "could not flush the database writes");
            true
        }
        Err(_) => false,
    }
}

/// Aborts the background tasks when dropped (on shutdown or a failed bind).
struct Tasks(Vec<JoinHandle<()>>);

impl Tasks {
    /// Abort every task and wait until each has stopped at its next await point.
    async fn stop(mut self) {
        let handles = std::mem::take(&mut self.0);
        for h in &handles {
            h.abort();
        }
        futures_util::future::join_all(handles).await;
    }
}

impl Drop for Tasks {
    fn drop(&mut self) {
        for h in &self.0 {
            h.abort();
        }
    }
}

/// Run `f` at once and then every `period` until aborted. A panicking iteration is logged
/// (with `name`) and the next tick runs as usual.
fn spawn_every<F, Fut>(name: &'static str, period: Duration, mut f: F) -> JoinHandle<()>
where
    F: FnMut() -> Fut + Send + 'static,
    Fut: std::future::Future<Output = ()> + Send + 'static,
{
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(period);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tick.tick().await;
            crate::logging::catch_panic(name, f()).await;
        }
    })
}

/// Dev/test hook for dashboard screenshots: `POOLTIME_FIXTURE_STATE=<path to a JSON file>` makes
/// `GET /api/v1/status` serve that document verbatim (e.g. `tests/support/fixtures/dashboard.json`)
/// so the dashboard can be exercised without real accounts. Inert unless the variable is set;
/// the service units never set it. Inference and every mutation still act on the real state.
/// A set but unreadable or non-JSON file stops startup rather than serving live data unawares.
pub const FIXTURE_STATE_ENV: &str = "POOLTIME_FIXTURE_STATE";

fn load_fixture_state() -> anyhow::Result<Option<serde_json::Value>> {
    let Some(path) = std::env::var_os(FIXTURE_STATE_ENV).filter(|v| !v.is_empty()) else {
        return Ok(None);
    };
    let raw = std::fs::read_to_string(&path).with_context(|| {
        format!(
            "reading {FIXTURE_STATE_ENV} file {}",
            path.to_string_lossy()
        )
    })?;
    let v: serde_json::Value = serde_json::from_str(&raw)
        .with_context(|| format!("{FIXTURE_STATE_ENV} file is not JSON"))?;
    tracing::warn!(
        "{FIXTURE_STATE_ENV} is set: /api/v1/status serves fixture data, not the live pool"
    );
    Ok(Some(v))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[tokio::test]
    async fn spawn_every_survives_a_panicking_iteration() {
        let runs = Arc::new(AtomicUsize::new(0));
        let seen = runs.clone();
        let task = spawn_every("test task", Duration::from_millis(10), move || {
            let n = seen.fetch_add(1, Ordering::SeqCst);
            async move {
                if n == 0 {
                    panic!("first iteration fails");
                }
            }
        });
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while runs.load(Ordering::SeqCst) < 3 {
            assert!(
                !task.is_finished(),
                "the task died with its first iteration"
            );
            assert!(tokio::time::Instant::now() < deadline, "stopped ticking");
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        task.abort();
    }

    #[test]
    fn state_book_applies_configured_window_and_cooldown() {
        use crate::accounts::{AccountId, AccountStatus};
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::open(&dir.path().join("pooltime.db")).unwrap());
        let t = crate::config::Timeouts {
            usage_window: Duration::from_secs(3600),
            access_denied_cooldown: Duration::from_secs(600),
            ..Default::default()
        };
        let book = state_book(store, &t).unwrap();
        let a: AccountId = "a".parse().unwrap();
        let at = |s: i64| OffsetDateTime::UNIX_EPOCH + time::Duration::seconds(1_800_000_000 + s);

        book.mark_access_denied(&a, at(0));
        assert_eq!(
            book.state(&a, at(0)).access_denied_until,
            Some(at(600)),
            "cooldown from timeouts.access_denied_cooldown"
        );
        assert_eq!(book.status(&a, true, at(601)), AccountStatus::Ready);

        let b: AccountId = "b".parse().unwrap();
        book.record_success(&b, Default::default(), at(0));
        assert_eq!(book.state(&b, at(1800)).window_counters.requests, 1);
        // Past the configured 1 h (not the built-in 5 h): the window is empty.
        assert_eq!(book.state(&b, at(3601)).window_counters.requests, 0);
    }

    #[tokio::test]
    async fn refresh_drain_is_bounded() {
        let started = tokio::time::Instant::now();
        let stuck = std::future::pending::<()>();
        let done = drain_within(vec![Box::pin(stuck) as _], Duration::from_millis(50)).await;
        assert!(!done);
        assert!(started.elapsed() < Duration::from_secs(5));
        let quick = async {};
        assert!(drain_within(vec![Box::pin(quick) as _], Duration::from_secs(5)).await);
    }

    #[tokio::test]
    async fn store_flush_waits_for_queued_writes() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("pooltime.db")).unwrap();
        let written = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let w = written.clone();
        store
            .submit(move |_| {
                std::thread::sleep(Duration::from_millis(200));
                w.store(true, Ordering::SeqCst);
                Ok(())
            })
            .unwrap();
        assert!(flush_store(&store, Duration::from_secs(5)).await);
        assert!(written.load(Ordering::SeqCst), "queued write lost at exit");
    }

    #[tokio::test]
    async fn forced_exit_is_prompt() {
        let (refresh, flush) = shutdown_bounds(Some(&Outcome::Forced));
        assert!(refresh <= Duration::from_secs(2), "{refresh:?}");
        assert!(flush <= Duration::from_secs(5), "{flush:?}");
        assert_eq!(shutdown_bounds(Some(&Outcome::Drained)).0, REFRESH_DRAIN);

        // A wedged writer does not hold the exit beyond the flush bound.
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("pooltime.db")).unwrap();
        store
            .submit(|_| {
                std::thread::sleep(Duration::from_secs(2));
                Ok(())
            })
            .unwrap();
        let started = tokio::time::Instant::now();
        assert!(!flush_store(&store, Duration::from_millis(100)).await);
        assert!(started.elapsed() < Duration::from_secs(1));
    }
}
