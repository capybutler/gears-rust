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

use std::collections::BTreeMap;
use std::sync::Arc;

use rust_decimal::Decimal;
use sqlx::PgPool;
use testcontainers::core::WaitFor;
use testcontainers::core::logs::LogSource;
use testcontainers::core::wait::LogWaitStrategy;
use testcontainers::runners::AsyncRunner;
use testcontainers::{ContainerAsync, GenericImage, ImageExt};
use time::OffsetDateTime;
use tokio_util::sync::CancellationToken;
use toolkit_odata::ast;
use uuid::Uuid;

use usage_collector_sdk::{
    IdempotencyKey, Invalidation, MeterTypeId, ReasonCode, RecordOrigin, ResourceRef, UsageRecord,
    derive_usage_record_id,
};

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

/// How many containers [`bring_up_with`] will burn through before giving up,
/// and how long it waits before starting the next one.
///
/// Three, because the failures it absorbs are contention against the Docker
/// daemon and a third attempt has never been needed; the backoff is there
/// because an immediate restart re-enters the contention that just lost.
const CONTAINER_ATTEMPTS: u32 = 3;
const CONTAINER_RETRY_BACKOFF: std::time::Duration = std::time::Duration::from_millis(300);

/// How many times [`bring_up_with`] tries to open the pool against **one**
/// container, and how long it waits between tries - 10 seconds per container,
/// 30 seconds across all three.
///
/// Stated rather than inlined as a bare `0..20` because it is the only thing
/// that absorbs a server which has announced itself but is not yet accepting
/// connections, so its size decides whether the suite is flaky under load.
const CONNECT_ATTEMPTS: u32 = 20;
const CONNECT_INTERVAL: std::time::Duration = std::time::Duration::from_millis(500);

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
    // A closure, not a value: `ContainerRequest` is consumed by `start()` and is
    // not `Clone`, so the retry below needs to build a fresh one per attempt.
    let image = || {
        test_containers::timescaledb()
            // Wait for the **second** "ready", across **both** streams. Both
            // halves of that are load-bearing.
            //
            // *Why the second.* This image announces readiness twice, and only
            // the second server is one a test can reach. `docker-entrypoint.sh`
            // runs a temporary bootstrap server for initdb with
            // `-c listen_addresses=''` (entrypoint line 297), i.e. **unix
            // socket only**; that one announces itself first. The image then
            // runs `/docker-entrypoint-initdb.d/001_timescaledb_tune.sh` - which
            // is also what makes `work_mem` 7837kB rather than the compiled
            // 4 MB - stops the bootstrap server, and starts the real one, which
            // announces itself again. Measured on
            // `timescale/timescaledb:2.29.2-pg18`: the bootstrap server at
            // `21:42:21.855`, the tune script, `PostgreSQL init process
            // complete`, then the real server at `21:42:22.498`. Waiting for
            // the first hands back a container whose published TCP port
            // refuses connections, leaving `build_pool`'s retry budget below as
            // the only thing between this suite and `pool connect failed`.
            //
            // *Why `BothStd`.* The two lines are not on the same stream as the
            // Docker API frames them: measured against this exact API,
            // `LogSource::StdErr` with `times(2)` never fires (45 s timeout),
            // while `BothStd` with `times(2)` returns in ~1.2 s and `BothStd`
            // with `times(3)` never fires - so there are exactly two in total
            // and they are split across the streams. Do not "tighten" this to
            // `stderr`; it was tried, and it hangs.
            //
            // Every container here is freshly created, so the count is always
            // exactly 2. A reused volume would skip initdb and log it once,
            // which is one more reason this harness never reuses one.
            .with_wait_for(WaitFor::log(
                LogWaitStrategy::new(
                    LogSource::BothStd,
                    "database system is ready to accept connections",
                )
                .with_times(2),
            ))
            .with_env_var("POSTGRES_USER", "user")
            .with_env_var("POSTGRES_PASSWORD", "pass")
            .with_env_var("POSTGRES_DB", "app")
    };
    // Start a container and connect to it, and treat **the whole of that** as
    // one attempt that may be retried with a fresh container.
    //
    // Three failure modes were measured on a fully loaded run of this suite,
    // and they are all the same underlying thing - the Docker daemon's port
    // publication racing a container that is already running - so they are
    // handled together rather than one at a time:
    //
    // 1. `get_host_port_ipv4` answers `container '<id>' does not expose port
    //    5432/tcp`. Measured at ~11 occurrences across 6 full runs of ~264
    //    containers each, i.e. under 1%.
    // 2. `start()` exceeds its startup timeout waiting for a readiness message
    //    a loaded box is slow to produce.
    // 3. **The published port answers, and it is not this container's
    //    PostgreSQL.** Observed once as `pool connect failed … UnexpectedEof
    //    "expected to read 1414811696 bytes, got 47 bytes at EOF"` - and
    //    `1414811696` is `0x54524150`, the ASCII bytes `TRAP`, read as a
    //    length prefix. A Postgres server does not send that; something else
    //    was on the port. It happened in the one run of six that also retried
    //    a container, which is what ties it to the same race.
    //
    // (3) is why the pool connect is **inside** this loop rather than after it:
    // no amount of retrying `build_pool` against a wrong port can help, because
    // the port stays wrong. Discarding the container and starting another is
    // the only thing that can, and it is what the earlier shape - retry the
    // port lookup, then retry the pool separately for 30 s - could not do.
    //
    // Bounded at three containers, and the last attempt's error is returned
    // **unchanged**, so an image that genuinely does not expose 5432, or a
    // config that genuinely cannot connect, still fails with the message that
    // says so rather than with a message about retries. That is also why this
    // is here rather than `nextest --retries`: a blanket retry cannot tell a
    // harness failure from an assertion failure, and would mask the second.
    //
    // The backoff is not decoration - the stated cause is contention, so an
    // immediate restart re-enters exactly what just lost.
    //
    // **The messages below land on the stderr of a test that then passes**,
    // which nextest captures and discards. A rate rising from 1-in-100 to
    // 1-in-3 is therefore invisible until an attempt-3 failure. To see it:
    // `cargo nextest run … --success-output immediate | grep -c "starting
    // another"`.
    let mut brought_up: Option<(
        ContainerAsync<GenericImage>,
        PgPool,
        TimescaleDbPluginConfig,
    )> = None;
    for attempt in 1..=CONTAINER_ATTEMPTS {
        let last = attempt == CONTAINER_ATTEMPTS;
        let container = match image().start().await {
            Ok(container) => container,
            Err(err) if last => return Err(err.into()),
            Err(err) => {
                eprintln!(
                    "timescaledb container failed to start (attempt {attempt}/\
                     {CONTAINER_ATTEMPTS}): {err}; starting another"
                );
                tokio::time::sleep(CONTAINER_RETRY_BACKOFF).await;
                continue;
            }
        };
        let port = match container.get_host_port_ipv4(5432).await {
            Ok(port) => port,
            Err(err) if last => return Err(err.into()),
            Err(err) => {
                eprintln!(
                    "timescaledb container came up without a published port (attempt \
                     {attempt}/{CONTAINER_ATTEMPTS}): {err}; starting another"
                );
                drop(container);
                tokio::time::sleep(CONTAINER_RETRY_BACKOFF).await;
                continue;
            }
        };

        // The test container serves no TLS; `sslmode=disable` is the deliberate
        // opt-out that `build_pool` honors (production DSNs without an explicit
        // sslmode are upgraded to `require` - see `connect_options`). Built by
        // deserialization because the secret-wrapped `database_url` has no
        // public literal constructor (the production path is always serde +
        // expand-vars).
        let cfg: TimescaleDbPluginConfig = serde_json::from_str(&format!(
            r#"{{ "database_url": "postgres://user:pass@127.0.0.1:{port}/app?sslmode=disable",
                  "statement_timeout_secs": {statement_timeout_secs},
                  "pool_size_min": {pool_size_min}, "pool_size_max": {pool_size_max},
                  "retention_period_secs": {retention_secs} }}"#
        ))
        .expect("valid test config json");

        // A server that has announced itself is not the same as one accepting a
        // TCP connection this instant under load, so a connect gets its own
        // short budget before the container is written off. Ten seconds per
        // container, three containers: the same 30 s ceiling the flat loop had,
        // spent where it can actually help.
        let mut pool = None;
        let mut connect_err = None;
        for _ in 0..CONNECT_ATTEMPTS {
            match build_pool(&cfg).await {
                Ok(p) => {
                    pool = Some(p);
                    break;
                }
                Err(e) => {
                    connect_err = Some(e);
                    tokio::time::sleep(CONNECT_INTERVAL).await;
                }
            }
        }
        match pool {
            Some(pool) => {
                brought_up = Some((container, pool, cfg));
                break;
            }
            None if last => {
                return Err(anyhow::anyhow!(
                    "pool connect failed on every one of {CONTAINER_ATTEMPTS} containers, \
                     each given {CONNECT_ATTEMPTS} attempts over {:?}: {connect_err:?}",
                    CONNECT_INTERVAL * CONNECT_ATTEMPTS,
                ));
            }
            None => {
                let detail = connect_err
                    .as_ref()
                    .map_or_else(|| "no error recorded".to_owned(), ToString::to_string);
                eprintln!(
                    "timescaledb container published a port that would not serve a pool \
                     (attempt {attempt}/{CONTAINER_ATTEMPTS}): {detail}; starting another"
                );
                drop(container);
                tokio::time::sleep(CONTAINER_RETRY_BACKOFF).await;
            }
        }
    }
    let (container, pool, cfg) = brought_up
        .ok_or_else(|| anyhow::anyhow!("container bring-up loop ended without a container"))?;

    MIGRATOR.run(&pool).await?;
    apply_post_migration_setup(&pool, cfg.retention_period_secs).await?;
    Ok(TsHarness {
        pool,
        _container: container,
    })
}

/// Build a fresh metric inventory over `pool`.
///
/// Private: [`record_store`] is its only caller, and a `pub` helper here is a
/// helper the `dead_code` allowance would hide if that caller went away.
///
/// The stores now take an `Arc<Metrics>`; tests only need a live handle, not to
/// assert on it, so each call mints its own inventory against the global meter
/// provider (recording is a no-op without an exporter installed).
#[must_use]
fn metrics(pool: &PgPool) -> Arc<Metrics> {
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

// ---------------------------------------------------------------------------
// Ledger-entry fixtures
//
// Authored here rather than in each suite because four of the five share them,
// which is the same sharing the file-header `dead_code` allowance is earned by.
// What Task 14 declined to do was *port* the retired builders; these are built
// against the current `UsageRecord` and every one of them is run.
//
// The one decision a fixture cannot avoid is the covered period, because both
// bounds are inputs to the derived identity
// (`cpt-cf-usage-collector-adr-record-identity-derivation`). It is taken once,
// here, as [`FIXTURE_WINDOW_START`] / [`FIXTURE_WINDOW_END`], so a suite that
// needs a *different* period says so at the call site instead of every suite
// picking one.
// ---------------------------------------------------------------------------

/// A valid meter type id: the reserved base plus one derivation segment,
/// `~`-terminated, which is what `MeterTypeId::new` validates.
pub const VCPU_METER: &str = "gts.cf.core.uc.usage_record.v1~cf.compute._.vcpu_hours.v1~";

/// A second meter, for the assertions whose subject is that a scope is per
/// `(tenant_id, gts_type_id)` rather than per tenant.
pub const GB_METER: &str = "gts.cf.core.uc.usage_record.v1~cf.storage._.gb_hours.v1~";

/// Inclusive start of the covered period every fixture carries by default:
/// `2023-11-14T22:13:20Z`.
pub const FIXTURE_WINDOW_START_UNIX: i64 = 1_700_000_000;

/// Exclusive end of that period, one hour later. Distinct from the start, so a
/// fixture is a period rather than a point event — a point event is a case the
/// suites ask for explicitly (`window_start == window_end`) rather than the
/// shape everything else accidentally inherits.
pub const FIXTURE_WINDOW_END_UNIX: i64 = 1_700_003_600;

/// The parsed [`FIXTURE_WINDOW_START_UNIX`].
///
/// # Panics
///
/// Never: the constant is a valid Unix instant.
#[must_use]
pub fn fixture_window_start() -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(FIXTURE_WINDOW_START_UNIX).expect("valid instant")
}

/// The parsed [`FIXTURE_WINDOW_END_UNIX`].
///
/// # Panics
///
/// Never: the constant is a valid Unix instant.
#[must_use]
pub fn fixture_window_end() -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(FIXTURE_WINDOW_END_UNIX).expect("valid instant")
}

/// A meter id from its wire string.
///
/// # Panics
///
/// If `id` is not a valid meter type id; every caller here passes a constant.
#[must_use]
pub fn meter(id: &str) -> MeterTypeId {
    MeterTypeId::new(id).expect("valid meter type id")
}

/// An ordinary measurement over an explicit covered period, with the derived
/// identity the Ingestion Gateway would stamp on it.
///
/// The `id` is [`derive_usage_record_id`] over the same five inputs the ledger's
/// `usage_records_dedup_uniq` is built over — this is the gateway's job, not the
/// plugin's, so deriving it here is standing in for the gateway rather than
/// asking the code under test to check itself.
///
/// Every field that is **not** one of those five (value, attribution, metadata,
/// origin, the invalidation pair) can be overwritten with a struct update
/// afterwards without invalidating the identity. The five that are appear in
/// this signature for exactly that reason.
///
/// # Panics
///
/// If `idem` is not a valid idempotency key, or the fixed resource reference
/// fails to validate; both are constants here.
#[must_use]
pub fn entry_over(
    meter_id: &MeterTypeId,
    tenant: Uuid,
    idem: &str,
    value: Decimal,
    window_start: OffsetDateTime,
    window_end: OffsetDateTime,
) -> UsageRecord {
    let idempotency_key = IdempotencyKey::new(idem).expect("valid idempotency key");
    UsageRecord {
        id: derive_usage_record_id(tenant, meter_id, &idempotency_key, window_start, window_end),
        gts_type_id: meter_id.clone(),
        tenant_id: tenant,
        resource_ref: ResourceRef::new("res-1", "compute.vm").expect("valid resource ref"),
        subject_ref: None,
        metadata: BTreeMap::new(),
        value,
        idempotency_key,
        origin: RecordOrigin::Live,
        invalidation: None,
        window_start,
        window_end,
    }
}

/// [`entry_over`] at the default covered period.
#[must_use]
pub fn entry(meter_id: &MeterTypeId, tenant: Uuid, idem: &str, value: Decimal) -> UsageRecord {
    entry_over(
        meter_id,
        tenant,
        idem,
        value,
        fixture_window_start(),
        fixture_window_end(),
    )
}

/// A faithful withdrawal of `target`: a copy of the entry it withdraws, plus the
/// invalidation pair, under its own idempotency key.
///
/// "A faithful copy of the entry it withdraws" is the schema's own phrase, and
/// every field copied below is copied for a reason rather than for tidiness:
///
/// * **The quantity** — an invalidation echoes what it withdraws rather than
///   negating it (`cpt-cf-usage-collector-adr-append-only-invalidation`), which
///   is why netting the two would now double-count.
/// * **The covered period** — `usage_records_one_invalidation_uniq` is over
///   `(invalidates, window_end)`, so a withdrawal carrying a different period is
///   outside the index's reach and the at-most-one guarantee does not hold for
///   it.
/// * **The attribution, metadata and origin** — so the only fields separating
///   the pair are `invalidates`, `reason_code` and the idempotency key. A
///   withdrawal that quietly differed in, say, `resource_type` would let a
///   `$filter` test look like it discriminated when it had only found an
///   asymmetry the fixture put there.
///
/// The idempotency key is the one input to the derivation the two do not share,
/// and is therefore the whole reason their identifiers differ.
///
/// # Panics
///
/// If `idem` or the fixed reason code fails to validate.
#[must_use]
pub fn withdrawal_of(target: &UsageRecord, idem: &str) -> UsageRecord {
    UsageRecord {
        invalidation: Some(Invalidation {
            target: target.id,
            reason: ReasonCode::new("duplicate_submission").expect("valid reason code"),
        }),
        resource_ref: target.resource_ref.clone(),
        subject_ref: target.subject_ref.clone(),
        metadata: target.metadata.clone(),
        origin: target.origin,
        ..entry_over(
            &target.gts_type_id,
            target.tenant_id,
            idem,
            target.value,
            target.window_start,
            target.window_end,
        )
    }
}

/// Restamp `record`'s derived identity after one of the five dedup-identity
/// inputs was changed by a struct update.
///
/// [`entry_over`] takes all five as parameters precisely so this is rarely
/// needed — every field a caller usually overwrites afterwards (quantity,
/// attribution, metadata, origin, the invalidation pair) is outside the
/// derivation. The exception is a test that starts from [`withdrawal_of`] and
/// then moves the entry into a different scope: `tenant_id` and `gts_type_id`
/// *are* inputs, so leaving the stamped id alone would store a row whose id no
/// emitter could reproduce, and the ledger's `id` and its dedup UNIQUE would
/// disagree about what the entry is.
///
/// # Panics
///
/// Never: the record already carries a validated key and meter.
#[must_use]
pub fn rederive(record: UsageRecord) -> UsageRecord {
    UsageRecord {
        id: derive_usage_record_id(
            record.tenant_id,
            &record.gts_type_id,
            &record.idempotency_key,
            record.window_start,
            record.window_end,
        ),
        ..record
    }
}

/// The compiled PDP scope a read is intersected with: every entry of `tenant`.
///
/// `get` takes the scope as its whole filter, so a test that wants a point
/// lookup to succeed has to hand it one the row satisfies. This is the narrowest
/// honest one — a real grant is a disjunction over the tenants a principal
/// holds, and a single-tenant grant is one arm of it.
#[must_use]
pub fn tenant_scope(tenant: Uuid) -> ast::Expr {
    ast::Expr::Compare(
        Box::new(ast::Expr::Identifier("tenant_id".to_owned())),
        ast::CompareOperator::Eq,
        Box::new(ast::Expr::Value(ast::Value::Uuid(tenant))),
    )
}
