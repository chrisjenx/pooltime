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
use crate::providers::openai::{self, OpenAiProvider, CATALOG_REFRESH_EVERY};
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
        .with_upstream(upstream)
        .with_mcp_key(mcp_key)
        .with_fixture_state(fixture_state),
    );

    tracing::info!(
        %host, port,
        version = env!("CARGO_PKG_VERSION"),
        accounts = app.registry.list().len(),
        "pooltime starting"
    );
    let tasks = Tasks(vec![
        spawn_usage_sweeper(anthropic, SWEEP_INTERVAL),
        openai::spawn_usage_sweeper(openai, SWEEP_INTERVAL),
        // Codex catalog discovery: at boot (providers are registered by now), then hourly. New
        // routes go live in the model table and trigger a profile re-sync.
        spawn_every(CATALOG_REFRESH_EVERY, {
            let app = app.clone();
            move || {
                let app = app.clone();
                async move { openai::refresh_codex_catalog(&app).await }
            }
        }),
        crate::claude_code::spawn_profile_sync(app.clone()),
        // Health tick: one probe refresh per passed login expiry (spec §16).
        spawn_every(crate::health::HEALTH_TICK, {
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
        spawn_every(SESSION_PRUNE_EVERY, move || {
            sessions.prune(OffsetDateTime::now_utc());
            std::future::ready(())
        }),
        // Weekly-burn history (spec §5): 7-day utilization samples on every state change.
        crate::analytics::sampler::spawn_sampler(
            app.registry.clone(),
            app.state.clone(),
            store.clone(),
        ),
        spawn_every(RETENTION_EVERY, move || {
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
    let cache = crate::claude_code::statusline::spawn_status_cache(app.clone());
    let outcome = serve_until_signal(app).await;
    drop(tasks);
    // Status lines say `pool down` right away instead of once the cache goes stale.
    cache.shutdown().await;
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
