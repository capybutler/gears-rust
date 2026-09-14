use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;
use toolkit::Gear;
use toolkit::client_hub::ClientScope;
use toolkit::context::GearCtx;
use toolkit::contracts::RunnableCapability;
use toolkit::gts::PluginV1;
use toolkit::tokio::task::JoinHandle;
use toolkit::tokio::time::MissedTickBehavior;
use tracing::info;
use types_registry_sdk::{RegisterResult, TypesRegistryClient};
use usage_collector_sdk::{UsageCollectorPluginSpecV1, UsageCollectorPluginV1};

use crate::config::TimescaleDbPluginConfig;
use crate::domain::adapter::StorageAdapter;
use crate::domain::ports::RecordStore;
use crate::infra::metrics::Metrics;
use crate::infra::registry_retention::TypesRegistryRetentionSource;
use crate::infra::storage::pool::{MIGRATOR, apply_post_migration_setup, build_pool};
use crate::infra::storage::record_store::PgRecordStore;
use crate::infra::storage::retention_sweep::PgRetentionSweeper;
use crate::infra::storage::rollup_maintenance::RollupMonitor;

/// `TimescaleDB` Usage Collector storage backend plugin module.
///
/// Conforms to the storage Plugin SPI: connects + migrates a `TimescaleDB`
/// database, performs the full GTS registration handshake, then registers
/// the scoped `StorageAdapter` client so the plugin host resolves it on
/// first dispatch. It also runs the per-type retention sweep as its
/// background task (`RunnableCapability`).
#[toolkit::gear(
    name = "timescaledb-usage-collector-plugin",
    deps = [types_registry],
    capabilities = [stateful]
)]
#[derive(Default)]
pub struct TimescaleDbUsageCollectorPlugin {
    /// Built by `init`, run by `start`.
    sweep: OnceLock<SweepWiring>,
    sweep_cancel: Mutex<Option<CancellationToken>>,
    sweep_handle: Mutex<Option<JoinHandle<()>>>,
}

/// What `start` needs from `init` to run the retention sweep and the rollup
/// refresh-policy monitor.
struct SweepWiring {
    sweeper: Arc<PgRetentionSweeper>,
    interval: Duration,
    monitor: Arc<RollupMonitor>,
}

#[async_trait]
impl Gear for TimescaleDbUsageCollectorPlugin {
    // @cpt-flow:cpt-cf-usage-collector-flow-foundation-plugin-host-binding:p1
    async fn init(&self, ctx: &GearCtx) -> anyhow::Result<()> {
        let cfg: TimescaleDbPluginConfig = ctx.config_expanded_or_default()?;
        cfg.validate()
            .map_err(|e| anyhow::anyhow!("invalid timescaledb plugin config: {e}"))?;

        // Connect, migrate, and apply the configured partitioning.
        // Race the startup-I/O sequence against the gear's cancellation token so
        // a shutdown mid-startup aborts promptly instead of blocking on each
        // call's own timeout. `Metrics::new` and the migration-failure counter
        // stay inside the raced block (the metric needs the pool; the counter
        // must still fire on a migration error), so the block yields the
        // `(pool, metrics)` it built. The `ready` gauge is deliberately left
        // unset (0) here: it is flipped to 1 only once the full init sequence —
        // including GTS registration — has succeeded (see below), so the gauge
        // means "fully initialized", not merely "migrated".
        let cancel = ctx.cancellation_token().clone();
        let (pool, metrics) = toolkit::tokio::select! {
            biased;
            () = cancel.cancelled() => {
                return Err(anyhow::anyhow!("init cancelled during shutdown"));
            }
            res = async {
                let pool = build_pool(&cfg).await?;
                let metrics = Arc::new(Metrics::new(pool.clone()));
                if let Err(e) = MIGRATOR.run(&pool).await {
                    metrics.inc_migration_failure();
                    return Err::<_, anyhow::Error>(e.into());
                }
                apply_post_migration_setup(&pool, &cfg).await?;
                Ok((pool, metrics))
            } => res?,
        };

        // Build registration payload and instance id for this plugin.
        let (instance_id, instance_json) =
            PluginV1::<UsageCollectorPluginSpecV1>::build_registration(
                "cf.core._.timescaledb_usage_collector.v1",
                cfg.vendor.clone(),
                cfg.priority,
            )?;

        // Publish to types-registry.
        let registry = ctx.client_hub().get::<dyn TypesRegistryClient>()?;
        let results = registry.register(vec![instance_json]).await?;
        RegisterResult::ensure_all_ok(&results)?;

        // The full init sequence — pool, migration, retention, and GTS
        // registration — has now succeeded, so mark the plugin ready. The Gear
        // trait exposes no shutdown hook (only `init`), so the cancellation
        // token is the only shutdown signal: a detached watcher flips `ready`
        // back to 0 when the gear is cancelled, so the gauge tracks live
        // readiness rather than "ready at last init". The watcher is spawned
        // AFTER `set_ready(true)` so a cancellation that already fired (a
        // shutdown racing the registration above) is still observed —
        // `cancelled()` resolves immediately on an already-cancelled token — and
        // clears the gauge rather than leaving it stuck at 1. Best-effort: if
        // the meter provider is already gone the record is a harmless no-op.
        metrics.set_ready(true);
        let ready_metrics = metrics.clone();
        toolkit::tokio::spawn(async move {
            cancel.cancelled().await;
            ready_metrics.set_ready(false);
        });

        // The retention sweep reads each type's declared retention from
        // types-registry itself — the one declaration attribute this plugin
        // reads, because it is the component that applies it.
        let retention = Arc::new(TypesRegistryRetentionSource::new(ctx.client_hub()));
        let sweeper = Arc::new(PgRetentionSweeper::new(
            pool.clone(),
            retention,
            Arc::clone(&metrics),
        ));
        let monitor = Arc::new(RollupMonitor::new(pool.clone(), Arc::clone(&metrics)));
        self.sweep
            .set(SweepWiring {
                sweeper,
                interval: Duration::from_secs(cfg.retention_sweep_interval_secs),
                monitor,
            })
            .map_err(|_| anyhow::anyhow!("timescaledb plugin init ran twice"))?;

        // Wire the storage stack: the record store behind the adapter. It takes
        // the one metric inventory built above via `Arc<Metrics>`.
        let record: Arc<dyn RecordStore> = Arc::new(PgRecordStore::new(
            pool,
            metrics,
            ctx.cancellation_token().clone(),
        ));
        let adapter = StorageAdapter::new(record);

        // Register the scoped backend client in ClientHub under the GTS
        // instance scope so the plugin host resolves it on first dispatch.
        ctx.client_hub()
            .register_scoped::<dyn UsageCollectorPluginV1>(
                ClientScope::gts_id(&instance_id),
                Arc::new(adapter) as Arc<dyn UsageCollectorPluginV1>,
            );

        info!(
            instance_id = %instance_id,
            vendor = %cfg.vendor,
            priority = cfg.priority,
            "Registered TimescaleDB usage-collector plugin instance"
        );
        Ok(())
    }
}

#[async_trait]
impl RunnableCapability for TimescaleDbUsageCollectorPlugin {
    async fn start(&self, cancel: CancellationToken) -> anyhow::Result<()> {
        let wiring = self.sweep.get().ok_or_else(|| {
            anyhow::anyhow!("retention sweep not initialized - init() must run before start()")
        })?;
        let sweeper = Arc::clone(&wiring.sweeper);
        let interval = wiring.interval;
        let monitor = Arc::clone(&wiring.monitor);
        let token = cancel.child_token();
        {
            let mut guard = self
                .sweep_cancel
                .lock()
                .map_err(|e| anyhow::anyhow!("sweep_cancel lock: {e}"))?;
            if guard.is_some() {
                anyhow::bail!("retention sweep already started");
            }
            *guard = Some(token.clone());
        }
        let handle = toolkit::tokio::spawn(run_background(sweeper, interval, monitor, token));
        *self
            .sweep_handle
            .lock()
            .map_err(|e| anyhow::anyhow!("sweep_handle lock: {e}"))? = Some(handle);
        info!(
            interval_secs = interval.as_secs(),
            "retention sweep and rollup monitor started"
        );
        Ok(())
    }

    async fn stop(&self, deadline: CancellationToken) -> anyhow::Result<()> {
        if let Some(token) = self
            .sweep_cancel
            .lock()
            .map_err(|e| anyhow::anyhow!("sweep_cancel lock: {e}"))?
            .take()
        {
            token.cancel();
        }
        let handle = self
            .sweep_handle
            .lock()
            .map_err(|e| anyhow::anyhow!("sweep_handle lock: {e}"))?
            .take();
        if let Some(handle) = handle {
            let mut handle = handle;
            toolkit::tokio::select! {
                result = &mut handle => {
                    if let Err(e) = result
                        && !e.is_cancelled()
                    {
                        tracing::warn!(error = ?e, "retention sweep task failed");
                    }
                }
                () = deadline.cancelled() => {
                    // Safe to abort mid-sweep: the sweep lock lives on a
                    // detached connection (`sweep_under_lock`) that closes,
                    // and so releases the lock, on drop regardless of how the
                    // task ends.
                    handle.abort();
                    tracing::info!("retention sweep stop cut short by the framework deadline");
                }
            }
        }
        Ok(())
    }
}

/// How often refresh-policy health is sampled.
const ROLLUP_MONITOR_INTERVAL: Duration = Duration::from_mins(1);

/// Sweep and sample now, then each on its own interval, until `cancel` fires.
///
/// Cancellation is observed between operations only. A sweep holds the sweep
/// lock and may be part-way through a chunk drop; letting it finish is cheaper
/// than reasoning about where it stopped, and its lock connection closes either
/// way.
async fn run_background(
    sweeper: Arc<PgRetentionSweeper>,
    sweep_interval: Duration,
    monitor: Arc<RollupMonitor>,
    cancel: CancellationToken,
) {
    let mut sweep_tick = toolkit::tokio::time::interval(sweep_interval);
    sweep_tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut monitor_tick = toolkit::tokio::time::interval(ROLLUP_MONITOR_INTERVAL);
    monitor_tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
    loop {
        toolkit::tokio::select! {
            biased;
            () = cancel.cancelled() => break,
            _ = sweep_tick.tick() => sweep_and_log(&sweeper).await,
            _ = monitor_tick.tick() => sample_and_log(&monitor).await,
        }
    }
}

/// Run one sweep and log its outcome, whatever that is.
async fn sweep_and_log(sweeper: &PgRetentionSweeper) {
    match sweeper.sweep_once().await {
        Ok(report) => tracing::debug!(?report, "retention sweep finished"),
        Err(e) => tracing::warn!(error = %e, "retention sweep failed; retrying next interval"),
    }
}

/// Sample refresh-policy health once and log a failure.
async fn sample_and_log(monitor: &RollupMonitor) {
    match monitor.sample_once().await {
        Ok(n) => tracing::debug!(policies = n, "rollup refresh health sampled"),
        Err(e) => {
            tracing::warn!(error = %e, "sampling rollup refresh health failed; retrying next interval");
        }
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "gear_tests.rs"]
mod gear_tests;
