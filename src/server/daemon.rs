//! `pooltime serve`: assembles the modules into a running daemon.
//!
//! Boot order: data dir and config (flags win over file and env) -> logging -> single-instance
//! and shadow-run guards -> store, registry, state, sessions, providers -> background tasks ->
//! serve until SIGTERM/SIGINT. The background tasks stop with the server and never take the
//! daemon down: they log failures (no tokens, no bodies) and carry on.

use std::collections::HashMap;
use std::io::IsTerminal;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use time::OffsetDateTime;
use tokio::task::JoinHandle;

use super::{health, mgmt, serve_until_signal, AppState, Outcome};
use crate::accounts::{Registry, StateBook};
use crate::cli::ServeFlags;
use crate::config::{Config, Paths};
use crate::models::ModelTable;
use crate::providers::anthropic::{
    spawn_usage_sweeper, AnthropicProvider, OverloadPolicy, SWEEP_INTERVAL,
};
use crate::providers::{Provider, ProviderId};
use crate::routing::reserve::Reservations;
use crate::routing::sessions::Sessions;
use crate::store::{prune, Store};
use crate::upstream::Upstream;

/// How often idle sticky-session pins are dropped.
const SESSION_PRUNE_EVERY: Duration = Duration::from_secs(60);
/// How often journal/usage-history retention runs.
const RETENTION_EVERY: Duration = Duration::from_secs(60 * 60);

/// Run the daemon until SIGTERM/SIGINT has drained (or a second signal forces exit).
///
/// Fails fast, before serving anything, when another pooltime is already on the port, when a
/// bro pool would share imported accounts (unless `flags.i_stopped_bro`), or when the port
/// cannot be bound.
pub async fn run_daemon(paths: Paths, flags: ServeFlags) -> anyhow::Result<()> {
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

    health::preflight(&cfg).await?;
    let registry = Arc::new(Registry::load(&paths)?);
    health::check_bro_shadow(&registry, health::BRO_PORT, flags.i_stopped_bro).await?;

    let store = Arc::new(Store::open(&paths.db)?);
    let state = Arc::new(StateBook::load(store.clone())?);
    let sessions = Arc::new(Sessions::load(store.clone(), cfg.timeouts.session_idle));
    let reservations = Reservations::new(HashMap::from([
        (ProviderId::Anthropic, cfg.routing.anthropic_max_inflight),
        (ProviderId::OpenAi, cfg.routing.codex_max_inflight),
    ]));
    let models = ModelTable::load(&paths)?;
    let mgmt_key = mgmt::ensure_mgmt_key(&paths)?;
    let upstream = Arc::new(Upstream::new());

    let t = &cfg.timeouts;
    let anthropic = Arc::new(
        AnthropicProvider::new(
            cfg.anthropic.clone(),
            registry.clone(),
            state.clone(),
            upstream.clone(),
        )
        .with_refresh_timing(t.token_refresh_skew, t.token_refresh_timeout)
        .with_rate_limit_cooldown(t.rate_limit_cooldown)
        .with_overload_policy(OverloadPolicy {
            max_retries: t.overload_retry_max,
            base: t.overload_retry_base,
            cap: t.overload_retry_cap,
            ..OverloadPolicy::default()
        }),
    );
    let providers: HashMap<ProviderId, Arc<dyn Provider>> = HashMap::from([(
        ProviderId::Anthropic,
        anthropic.clone() as Arc<dyn Provider>,
    )]);

    let retention_cfg = cfg.retention.clone();
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
        .with_upstream(upstream),
    );

    tracing::info!(
        %host, port,
        version = env!("CARGO_PKG_VERSION"),
        accounts = app.registry.list().len(),
        "pooltime starting"
    );
    let tasks = Tasks(vec![
        spawn_usage_sweeper(anthropic, SWEEP_INTERVAL),
        crate::claude_code::spawn_profile_sync(app.clone()),
        spawn_every(SESSION_PRUNE_EVERY, move || {
            sessions.prune(OffsetDateTime::now_utc());
            std::future::ready(())
        }),
        spawn_every(RETENTION_EVERY, move || {
            let (store, cfg) = (store.clone(), retention_cfg.clone());
            async move {
                match prune(&store, &cfg, OffsetDateTime::now_utc()).await {
                    Ok(r) => tracing::debug!(report = ?r, "retention prune finished"),
                    Err(e) => tracing::warn!(error = %e, "retention prune failed"),
                }
            }
        }),
    ]);

    let outcome = serve_until_signal(app).await;
    drop(tasks);
    match outcome? {
        Outcome::Drained => tracing::info!("pooltime stopped"),
        Outcome::TimedOut | Outcome::Forced => {
            tracing::warn!("pooltime stopped with requests in flight")
        }
    }
    Ok(())
}

/// Aborts the background tasks when dropped (on shutdown or a failed bind).
struct Tasks(Vec<JoinHandle<()>>);

impl Drop for Tasks {
    fn drop(&mut self) {
        for h in &self.0 {
            h.abort();
        }
    }
}

/// Run `f` at once and then every `period` until aborted. A panic in `f` ends only this task
/// (the runtime reports it); the daemon keeps serving.
fn spawn_every<F, Fut>(period: Duration, mut f: F) -> JoinHandle<()>
where
    F: FnMut() -> Fut + Send + 'static,
    Fut: std::future::Future<Output = ()> + Send + 'static,
{
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(period);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tick.tick().await;
            f().await;
        }
    })
}
