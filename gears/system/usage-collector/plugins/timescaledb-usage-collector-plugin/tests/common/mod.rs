#![cfg(feature = "postgres")]
// Shared across test binaries: `mod common;` compiles a private copy into each
// one, so a helper another binary uses is `dead_code` in this one. These
// helpers also panic on invalid test input by design.
//
// The allowance is earned by that sharing and by nothing else — `dead_code`
// hides a helper with no callers at all just as well, which is how
// `insert_raw_usage_record` kept an `INSERT` naming columns the table does not
// have, and a doc citing a foreign key the schema does not have, until Task 14
// deleted it. Before adding a helper here, check it has a caller somewhere;
// the lint will not.
#![allow(dead_code, clippy::expect_used, clippy::unwrap_used)]
//! Shared `TimescaleDB` testcontainer harness. Requires Docker.

use std::sync::Arc;
use std::time::Duration;

use sqlx::PgPool;
use testcontainers::core::WaitFor;
use testcontainers::runners::AsyncRunner;
use testcontainers::{ContainerAsync, GenericImage, ImageExt};
use tokio_util::sync::CancellationToken;

use timescaledb_usage_collector_plugin::config::TimescaleDbPluginConfig;
use timescaledb_usage_collector_plugin::domain::adapter::StorageAdapter;
use timescaledb_usage_collector_plugin::domain::ports::RecordStore;
use timescaledb_usage_collector_plugin::infra::metrics::Metrics;
use timescaledb_usage_collector_plugin::infra::storage::pool::{
    MIGRATOR, apply_post_migration_setup, build_pool,
};
use timescaledb_usage_collector_plugin::infra::storage::record_store::PgRecordStore;

pub struct TsHarness {
    pub pool: PgPool,
    _container: ContainerAsync<GenericImage>,
}

/// Retention window for harnesses whose fixtures cover a deliberately backdated
/// period.
///
/// `apply_post_migration_setup` registers a REAL `policy_retention` job, and the
/// image's background scheduler fires it on its own roughly 3 seconds after
/// registration — i.e. while the test body is still running. The policy measures
/// from `window_end` (the hypertable's partition column), so at the production
/// default window (365 days) any fixture whose period ended more than a year ago
/// is past the cutoff, and they are spread over several 7-day chunks: that
/// first scheduled run drops those chunks out from under the test. A stalled insert loop
/// (parallel containers on a loaded CI box) then sees rows vanish mid-test: one
/// aggregation bucket short, a `count` off by one, a cursor walk ending early.
///
/// At the config's documented ceiling (`MAX_RETENTION_SECS`, `src/config.rs` —
/// private, so the value is repeated here) the cutoff lands in 1926, no chunk is
/// ever drop-eligible, and the registered policy is a structural no-op: still
/// registered, still scheduled, still run — it just finds nothing to delete.
pub const NO_DROP_RETENTION_SECS: u64 = 100 * 365 * 86_400;

/// The production default (365 days). Only for tests whose subject IS retention.
pub const REAL_RETENTION_SECS: u64 = 365 * 86_400;

pub async fn bring_up() -> anyhow::Result<TsHarness> {
    // Default pool bounds and statement timeout (mirrors the config defaults),
    // plus a retention window that cannot reach the backdated fixtures.
    bring_up_with(30, 2, 16, NO_DROP_RETENTION_SECS).await
}

/// Like [`bring_up`] but with the production 365-day retention window, and with
/// the registered job's background schedule disabled.
///
/// For tests whose subject is retention itself: they insert genuinely aged rows
/// and fire the policy by hand (`CALL run_job`). Leaving the job scheduled would
/// let the background scheduler drop those rows first, both flaking the
/// "exists before retention runs" precondition and letting the assertion pass
/// for the wrong reason (background run did the work, the manual one was a
/// no-op). Unscheduling makes the explicit `run_job` the only deleter, so the
/// test observes exactly the policy it registered.
pub async fn bring_up_real_retention() -> anyhow::Result<TsHarness> {
    let h = bring_up_with(30, 2, 16, REAL_RETENTION_SECS).await?;
    // `alter_job(scheduled => false)` keeps the job row (so the schema test's
    // "a policy is registered" assertion still holds) and leaves `run_job`
    // working; it only stops the scheduler from firing it unprompted.
    let unscheduled = sqlx::query(
        "SELECT alter_job(job_id, scheduled => false) \
         FROM timescaledb_information.jobs \
         WHERE proc_name = 'policy_retention' AND hypertable_name = 'usage_records'",
    )
    .execute(&h.pool)
    .await?
    .rows_affected();
    // A `WHERE` that matches nothing is not an error, so an unschedule that
    // quietly hit zero rows would leave the job scheduled and re-arm the very
    // race this harness exists to remove — and the retention test would then
    // pass on the background run's work. Fail loudly instead, so a TimescaleDB
    // bump that reshapes `timescaledb_information.jobs` surfaces here.
    anyhow::ensure!(
        unscheduled == 1,
        "expected to unschedule exactly 1 policy_retention job on usage_records, \
         matched {unscheduled}"
    );
    Ok(h)
}

/// Like [`bring_up`] but with an explicit request-path `statement_timeout` (secs),
/// pool bounds, and retention window. Used to assert the init path does not leak
/// a modified `statement_timeout` onto pooled connections: pass a value distinct
/// from any the init path might set, and a small fixed pool so every connection
/// can be inspected.
///
/// `retention_secs` is threaded into the config JSON rather than left to
/// `#[serde(default)]`: the default is the production 365-day window, which arms
/// a live chunk-dropper against the backdated fixtures (see
/// [`NO_DROP_RETENTION_SECS`]).
pub async fn bring_up_with(
    statement_timeout_secs: u64,
    pool_size_min: u32,
    pool_size_max: u32,
    retention_secs: u64,
) -> anyhow::Result<TsHarness> {
    // The tag lives in `test_containers::TIMESCALEDB_TAG`; keep that constant
    // in sync with `TimescaleDbSidecar.IMAGE` in `testing/e2e/lib/sidecars.py`.
    // A skew means these migrations are validated against a different
    // PostgreSQL major than E2E runs.
    let image = test_containers::timescaledb()
        .with_wait_for(WaitFor::message_on_stderr(
            "database system is ready to accept connections",
        ))
        .with_env_var("POSTGRES_USER", "user")
        .with_env_var("POSTGRES_PASSWORD", "pass")
        .with_env_var("POSTGRES_DB", "app");
    let container = image.start().await?;
    let port = container.get_host_port_ipv4(5432).await?;

    // The test container serves no TLS; `sslmode=disable` is the deliberate
    // opt-out that `build_pool` honors (production DSNs without an explicit
    // sslmode are upgraded to `require` — see `connect_options`). Built by
    // deserialization because the secret-wrapped `database_url` has no public
    // literal constructor (the production path is always serde + expand-vars).
    let cfg: TimescaleDbPluginConfig = serde_json::from_str(&format!(
        r#"{{ "database_url": "postgres://user:pass@127.0.0.1:{port}/app?sslmode=disable",
              "statement_timeout_secs": {statement_timeout_secs},
              "pool_size_min": {pool_size_min}, "pool_size_max": {pool_size_max},
              "retention_period_secs": {retention_secs} }}"#
    ))
    .expect("valid test config json");

    let mut pool = None;
    let mut last = None;
    for _ in 0..20 {
        match build_pool(&cfg).await {
            Ok(p) => {
                pool = Some(p);
                break;
            }
            Err(e) => {
                last = Some(e);
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
        }
    }
    let pool = pool.ok_or_else(|| anyhow::anyhow!("pool connect failed: {last:?}"))?;

    MIGRATOR.run(&pool).await?;
    apply_post_migration_setup(&pool, cfg.retention_period_secs).await?;
    Ok(TsHarness {
        pool,
        _container: container,
    })
}

/// Build a fresh metric inventory over `pool`.
///
/// The stores now take an `Arc<Metrics>`; tests only need a live handle, not to
/// assert on it, so each call mints its own inventory against the global meter
/// provider (recording is a no-op without an exporter installed).
#[must_use]
pub fn metrics(pool: &PgPool) -> Arc<Metrics> {
    Arc::new(Metrics::new(pool.clone()))
}

/// Convenience builder for a [`PgRecordStore`] with its own metric handle.
#[must_use]
pub fn record_store(pool: &PgPool) -> PgRecordStore {
    PgRecordStore::new(pool.clone(), metrics(pool), CancellationToken::new())
}

/// Bring up a migrated `TimescaleDB` and return the SPI implementation over it.
///
/// The DESIGN section 3.3 contract suite takes a `&dyn
/// UsageCollectorPluginV1`, and [`StorageAdapter`] is the crate's only
/// implementation of it, so this is the whole of the wiring: the same
/// container [`bring_up`] starts, the same [`PgRecordStore`] every other
/// caller of [`record_store`] drives, behind the adapter the gear registers
/// in `ClientHub`.
///
/// The retention window is [`bring_up`]'s [`NO_DROP_RETENTION_SECS`] rather
/// than the production default, and that is load-bearing here: the suite
/// offsets every fixture period from its own `FIXTURE_EPOCH`
/// (`2020-01-01T00:00:00Z`), which a 365-day window puts years past the
/// cutoff — the six checks sit at `FIXTURE_EPOCH` + 0/30/60/90/120/150 days,
/// so that is six distinct 7-day chunks, and the scheduled `policy_retention`
/// job would drop them while the run is still writing to them. The checks
/// would then report a conforming backend as losing entries.
///
/// The returned [`TsHarness`] owns the container: hold it for the length of
/// the test, or the database goes away with it. The suite writes entries and
/// never removes them, but each call starts a container of its own, so a run
/// never meets a previous run's rows and the fixtures' keying assumptions
/// never come into it.
///
/// # Panics
///
/// If the container or the migration fails; there is no test to run without
/// a backend.
pub async fn start_backend() -> (TsHarness, StorageAdapter) {
    let harness = bring_up()
        .await
        .expect("contract suite needs a migrated TimescaleDB container");
    let store: Arc<dyn RecordStore> = Arc::new(record_store(&harness.pool));
    (harness, StorageAdapter::new(store))
}
