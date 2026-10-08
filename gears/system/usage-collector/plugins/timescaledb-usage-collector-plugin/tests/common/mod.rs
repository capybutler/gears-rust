#![cfg(feature = "postgres")]
// Shared across test binaries: `mod common;` compiles a private copy into each
// one, so a helper another binary uses is `dead_code` in this one. These
// helpers also panic on invalid test input by design.
//
// The allowance is earned by that sharing and by nothing else — `dead_code`
// hides a helper with no callers at all just as well. Before adding a helper
// here, check it has a caller somewhere; the lint will not.
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
use time::{Duration, OffsetDateTime};
use tokio_util::sync::CancellationToken;
use toolkit_odata::ast;
use uuid::Uuid;

use usage_collector_sdk::contract::retention::ContractRetention;
use usage_collector_sdk::{
    EntryType, IdempotencyKey, Invalidation, MeterRef, MeterTypeId, ReasonCode, RecordOrigin,
    ResourceRef, StoredUsageRecord, UsageCollectorPluginError, UsageCollectorPluginV1,
    UsageQuantity, UsageRecord, derive_usage_record_id,
};

use timescaledb_usage_collector_plugin::config::TimescaleDbPluginConfig;
use timescaledb_usage_collector_plugin::domain::adapter::StorageAdapter;
use timescaledb_usage_collector_plugin::domain::ports::{
    RecordStore, RetentionError, RetentionSource,
};
use timescaledb_usage_collector_plugin::infra::metrics::Metrics;
use timescaledb_usage_collector_plugin::infra::storage::pool::{
    MIGRATOR, apply_post_migration_setup, build_pool,
};
use timescaledb_usage_collector_plugin::infra::storage::record_store::PgRecordStore;
use timescaledb_usage_collector_plugin::infra::storage::retention_sweep::PgRetentionSweeper;

pub struct TsHarness {
    pub pool: PgPool,
    pub cfg: TimescaleDbPluginConfig,
    container: ContainerAsync<GenericImage>,
}

impl TsHarness {
    /// The container this harness started, for a test that needs to reach it
    /// directly rather than through the pool. A method rather than a `pub`
    /// field: a caller is meant to *read* the container, not replace or drop
    /// it out from under the harness.
    #[must_use]
    pub fn container(&self) -> &ContainerAsync<GenericImage> {
        &self.container
    }
}

/// How many containers [`bring_up_with`] will burn through before giving up,
/// and how long it waits before starting the next one. The failures absorbed
/// are contention against the Docker daemon; the backoff is there because an
/// immediate restart re-enters the contention that just lost.
const CONTAINER_ATTEMPTS: u32 = 3;
const CONTAINER_RETRY_BACKOFF: std::time::Duration = std::time::Duration::from_millis(300);

/// How many times [`bring_up_with`] tries to open the pool against **one**
/// container, and how long it waits between tries.
///
/// Named rather than inlined because it is the only thing that absorbs a
/// server which has announced itself but is not yet accepting connections, so
/// its size decides whether the suite is flaky under load.
const CONNECT_ATTEMPTS: u32 = 20;
const CONNECT_INTERVAL: std::time::Duration = std::time::Duration::from_millis(500);

/// The acceptance slack every shared-harness backend runs at.
///
/// **Why it is not the published default.** The write path refuses a row whose
/// `accepted_at` is further than this from the INSERT's own
/// `statement_timestamp()` (`docs/DESIGN.md` §3.6), and every fixture in this
/// lane is dated — [`fixture_window_end`] and the SDK contract suite's
/// `CONTRACT_ACCEPTED_AT` are both fixed, because `server-field-round-trip`
/// compares a read against a literal and `latest-tie-break` varies
/// `accepted_at` to rank entries, and neither works against a clock.
///
/// So the shared harness widens the operator setting rather than re-dating the
/// fixtures. **The consequence is that the shared run exercises the guard's
/// admit path only**; its refusal is covered by `acceptance_slack_pg.rs`, which
/// builds its own backend at a narrow slack. The value is far wider than any
/// fixture's distance from the clock, and deliberately **not** described as the
/// largest a deployment could configure: the ceiling
/// `TimescaleDbPluginConfig::validate` applies is private to `config` and
/// nothing checks this constant against it.
pub const HARNESS_ACCEPTANCE_SLACK_SECS: u64 = 100 * 365 * 86_400;

/// The chunk interval the **contract** lane runs at, and only that lane.
///
/// One hour, where every other pg suite takes the 7-day default. The contract
/// suite's `ContractRetention` drive purges by driving this plugin's own
/// retention sweep, and the sweep decides a whole chunk against
/// `chunk.time_end`; the suite's fixtures sit whole hours apart. At the default
/// interval one chunk holds a whole check's entries, so a drive would take
/// more than it was asked for and `feed-retention-refusal`'s guards would fail.
///
/// At an hour the boundary is exact rather than approximate, and the arithmetic
/// is the whole reason for the number. `drop_decision` drops a chunk iff
/// `time_end + retention < now`, strictly, so a drive asking for a floor sets a
/// retention of `now - floor - δ` and the sweep then drops iff
/// `time_end < floor + δ`. Chunk boundaries and fixture covered-period ends are
/// both whole hours from the Unix epoch, so for any `0 < δ ≤ 1 hour` that is
/// exactly `time_end ≤ floor` — `ContractRetention::drop_before`'s own
/// exclusive bound.
///
/// The precedent for widening a shared harness setting is
/// [`HARNESS_ACCEPTANCE_SLACK_SECS`], and the cost is the same shape: **a suite
/// that reads chunk counts or chunk geometry reads a different number in the
/// contract lane than elsewhere.** Nothing does today.
pub const CONTRACT_CHUNK_INTERVAL_SECS: u64 = 3_600;

/// [`bring_up_with`] at the shared harness's own pool bounds, statement
/// timeout and acceptance slack, varying only the chunk interval.
///
/// [`bring_up`] and [`start_backend_with_retention_drive`] both call this
/// rather than each spelling those literals out, so the two cannot silently
/// diverge the moment one of those defaults changes.
async fn bring_up_at_chunk_interval(chunk_time_interval_secs: u64) -> anyhow::Result<TsHarness> {
    bring_up_with(
        30,
        2,
        16,
        HARNESS_ACCEPTANCE_SLACK_SECS,
        chunk_time_interval_secs,
    )
    .await
}

pub async fn bring_up() -> anyhow::Result<TsHarness> {
    // The config default chunk interval (7 days), which every suite but the
    // contract one runs at.
    bring_up_at_chunk_interval(7 * 86_400).await
}

/// Like [`bring_up`] but with an explicit request-path `statement_timeout`
/// (secs), pool bounds, acceptance slack, and chunk interval. The timeout and
/// pool bounds are used to assert the init path does not leak a modified
/// `statement_timeout` onto pooled connections: pass a value distinct from any
/// the init path might set, and a small fixed pool so every connection can be
/// inspected. `feed_acceptance_slack_secs` is `acceptance_slack_pg`'s, a
/// parameter rather than a second builder so one function still owns the
/// harness config; `chunk_time_interval_secs` is
/// [`CONTRACT_CHUNK_INTERVAL_SECS`]'s, every other caller passing the default
/// `bring_up` does.
pub async fn bring_up_with(
    statement_timeout_secs: u64,
    pool_size_min: u32,
    pool_size_max: u32,
    feed_acceptance_slack_secs: u64,
    chunk_time_interval_secs: u64,
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
            // *Why the second.* This image announces readiness twice and only
            // the second server is one a test can reach: `docker-entrypoint.sh`
            // runs a temporary unix-socket-only bootstrap server for initdb,
            // which announces itself first, then runs
            // `/docker-entrypoint-initdb.d/001_timescaledb_tune.sh`, stops the
            // bootstrap server, and starts the real one. Waiting for the first
            // hands back a container whose published TCP port refuses
            // connections.
            //
            // *Why `BothStd`.* The two lines are not on the same stream as the
            // Docker API frames them: `LogSource::StdErr` with `times(2)` never
            // fires, while `BothStd` with `times(2)` returns in ~1.2 s. Do not
            // "tighten" this to `stderr`; it was tried, and it hangs.
            //
            // Every container here is freshly created, so both lines always
            // appear. A reused volume would skip initdb and log it once, which
            // is one more reason this harness never reuses one.
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
            // Cap the shared-memory segment. This lane starts one container
            // per integration test at `test-threads = num_cpus`, and each runs
            // `/docker-entrypoint-initdb.d/001_timescaledb_tune.sh`, which
            // sizes `shared_buffers` from **host** RAM with no idea it has
            // siblings — fine at one container, N times that on a wide runner.
            //
            // The override leaves the rest of the tune script's work in place
            // and the readiness message still appears twice, so the wait
            // strategy above is unaffected. 256 MB is twice PostgreSQL's own
            // compiled default, so no test here has less headroom than a stock
            // server would give it.
            //
            // Set at THIS call site rather than in
            // `test_containers::timescaledb()`: the concurrency that makes the
            // default bite is this lane's, and the shared helper hands back a
            // bare `GenericImage` precisely so callers supply their own runtime
            // arguments.
            .with_cmd(["postgres", "-c", "shared_buffers=256MB"])
    };
    // Start a container and connect to it, and treat **the whole of that** as
    // one attempt that may be retried with a fresh container.
    //
    // The failure modes measured on a fully loaded run of this suite are all
    // the same underlying thing — the Docker daemon's port publication racing
    // a container that is already running — so they are handled together:
    //
    // 1. `get_host_port_ipv4` answers `container '<id>' does not expose port
    //    5432/tcp` (observed at under 1% of containers).
    // 2. `start()` exceeds its startup timeout waiting for a readiness message
    //    a loaded box is slow to produce.
    // 3. **The published port answers, and an HTTP server is behind it.**
    //    Observed once as an `UnexpectedEof` whose wire bytes decoded (against
    //    `sqlx-postgres`'s `connection/stream.rs`) to `HTTP/1.1 400 Bad
    //    Request`. So the peer was a Docker Desktop port-forwarder or another
    //    local service holding that ephemeral port, not a slow Postgres. On a
    //    recurrence: find what is listening and whether it speaks HTTP.
    //
    // (3) is why the pool connect is **inside** this loop rather than after it:
    // no amount of retrying `build_pool` against a wrong port can help, because
    // the port stays wrong. Discarding the container and starting another is
    // the only thing that can.
    //
    // The `start()` / port arms return the last attempt's error **unchanged**,
    // so an image that genuinely does not expose 5432 still fails with the
    // message that says so; the pool arm wraps, because "every container
    // refused a connection" is itself the diagnosis — but it carries the
    // underlying `sqlx` error verbatim. That is also why this is here rather
    // than `nextest --retries`: a blanket retry cannot tell a harness failure
    // from an assertion failure, and would mask the second.
    //
    // **The messages below land on the stderr of a test that then passes**,
    // which nextest captures and discards, so a rising retry rate is invisible
    // until a final-attempt failure. To see it: `cargo nextest run …
    // --success-output immediate | grep -c "starting another"`.
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
        // sslmode are upgraded to `require` — see `connect_options`). Built by
        // deserialization because the secret-wrapped `database_url` has no
        // public literal constructor.
        let cfg: TimescaleDbPluginConfig = serde_json::from_str(&format!(
            r#"{{ "database_url": "postgres://user:pass@127.0.0.1:{port}/app?sslmode=disable",
                  "statement_timeout_secs": {statement_timeout_secs},
                  "feed_acceptance_slack_secs": {feed_acceptance_slack_secs},
                  "chunk_time_interval_secs": {chunk_time_interval_secs},
                  "pool_size_min": {pool_size_min}, "pool_size_max": {pool_size_max} }}"#
        ))
        .expect("valid test config json");

        // A server that has announced itself is not the same as one accepting a
        // TCP connection this instant under load, so a connect gets its own
        // short budget before the container is written off.
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
    apply_post_migration_setup(&pool, &cfg).await?;
    settle_and_remove_rollup_policies(&pool).await?;
    Ok(TsHarness {
        pool,
        cfg,
        container,
    })
}

/// How many times [`await_setting`] reads a server setting back, and how long
/// it waits between reads.
///
/// `pg_reload_conf()` signals the postmaster and returns before every backend
/// has re-read the file, so a caller that asserted straight after it would be
/// asserting against the value the connection still held. In practice a
/// `sighup` setting flipped by `ALTER SYSTEM` reads back changed on the very
/// next statement; the budget is what turns a slower box into a failure rather
/// than a hang.
const SETTING_ATTEMPTS: u32 = 50;
const SETTING_INTERVAL: std::time::Duration = std::time::Duration::from_millis(100);

/// Block until `current_setting(setting)` reads `expected` on `pool`.
///
/// # Panics
///
/// If it still does not after [`SETTING_ATTEMPTS`] reads, naming the setting,
/// what was wanted and what was last read. Carrying on regardless would assert
/// against a server it has not in fact reconfigured.
pub async fn await_setting(pool: &PgPool, setting: &str, expected: &str) {
    let mut last = String::new();
    for _ in 0..SETTING_ATTEMPTS {
        last = sqlx::query_scalar("SELECT current_setting($1)")
            .bind(setting)
            .fetch_one(pool)
            .await
            .expect("read the setting back");
        if last == expected {
            return;
        }
        tokio::time::sleep(SETTING_INTERVAL).await;
    }
    panic!(
        "{setting} still reads {last} after {:?}, wanted {expected}",
        SETTING_INTERVAL * SETTING_ATTEMPTS
    );
}

/// Wait for the refresh policies' first run, which `TimescaleDB` starts within
/// seconds of creating them, then remove them. A background refresh racing a
/// test would make "stale until refreshed" assertions flaky; tests refresh
/// explicitly through [`refresh_rollup`] instead. A test that re-applies setup
/// (which re-creates the policies) must call this again afterwards, once it is
/// done asserting on them.
pub async fn settle_and_remove_rollup_policies(pool: &PgPool) -> anyhow::Result<()> {
    for _ in 0..60 {
        let ran: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM timescaledb_information.jobs j \
             JOIN timescaledb_information.job_stats js ON js.job_id = j.job_id \
             WHERE j.proc_name = 'policy_refresh_continuous_aggregate' AND js.total_runs >= 1",
        )
        .fetch_one(pool)
        .await?;
        if ran >= 2 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }
    sqlx::query(
        "SELECT delete_job(job_id) FROM timescaledb_information.jobs \
         WHERE proc_name = 'policy_refresh_continuous_aggregate'",
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// Materialise every invalidated range of the rollup now. Autocommit only:
/// `refresh_continuous_aggregate` refuses a transaction block.
pub async fn refresh_rollup(pool: &PgPool) {
    sqlx::query("CALL refresh_continuous_aggregate('usage_rollup_1h', NULL, NULL)")
        .execute(pool)
        .await
        .expect("refresh the rollup");
}

/// The `xact_id` the ledger stamped on the stored entry `id`.
///
/// Read as text and parsed, because `xid8` has no `sqlx` `Decode`
/// implementation — the same reason `FeedRecordRow::xact_id` is a `String`.
/// The parse makes `<` an order over transaction ids rather than over their
/// digit strings, which disagree as soon as two ids differ in length.
///
/// # Panics
///
/// If no row carries `id`, or the stored value does not parse as a `u64`. The
/// first means the write under test did not happen; the second means the
/// column is no longer an `xid8`.
pub async fn xact_id_of(pool: &PgPool, id: Uuid) -> u64 {
    let rendered: String =
        sqlx::query_scalar("SELECT xact_id::text FROM usage_records WHERE id = $1")
            .bind(id)
            .fetch_one(pool)
            .await
            .expect("the entry must be stored");
    rendered
        .parse()
        .unwrap_or_else(|e| panic!("xact_id {rendered} must render as a u64: {e}"))
}

/// Build a fresh metric inventory over `pool`.
///
/// Private: [`record_store`] is its only caller, and a `pub` helper here is a
/// helper the `dead_code` allowance would hide if that caller went away. Tests
/// only need a live handle, not to assert on it, so each call mints its own
/// inventory against the global meter provider (recording is a no-op without
/// an exporter installed).
#[must_use]
fn metrics(pool: &PgPool) -> Arc<Metrics> {
    Arc::new(Metrics::new(pool.clone()))
}

/// Convenience builder for a [`PgRecordStore`] with its own metric handle,
/// over the harness's own pool and acceptance slack.
///
/// It takes the whole [`TsHarness`] rather than its pool because the store also
/// reads a setting off the config: a builder taking the pool alone would have
/// to pick an acceptance slack of its own, and a suite that set one on the
/// harness would silently not get it.
#[must_use]
pub fn record_store(h: &TsHarness) -> PgRecordStore {
    PgRecordStore::new(
        h.pool.clone(),
        metrics(&h.pool),
        CancellationToken::new(),
        h.cfg.feed_acceptance_slack_secs,
    )
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
/// Nothing in the harness drops data: chunks are dropped only by the retention
/// sweep, which a test runs explicitly.
///
/// The returned [`TsHarness`] owns the container: hold it for the length of
/// the test, or the database goes away with it. Each call starts a container of
/// its own, so a run never meets a previous run's rows.
///
/// # Panics
///
/// If the container or the migration fails; there is no test to run without
/// a backend.
pub async fn start_backend() -> (TsHarness, StorageAdapter) {
    let harness = bring_up()
        .await
        .expect("contract suite needs a migrated TimescaleDB container");
    let store: Arc<dyn RecordStore> = Arc::new(record_store(&harness));
    (harness, StorageAdapter::new(store))
}

/// Like [`start_backend`], but the container runs at
/// [`CONTRACT_CHUNK_INTERVAL_SECS`] and the harness comes back beside a
/// [`SweepDrive`] over its own pool, for `contract::run_all_with_retention`.
///
/// A second function rather than another element on [`start_backend`]'s tuple:
/// that one is destructured at many call sites that do not want a retention
/// drive, and widening its return type would edit every one of them.
///
/// # Panics
///
/// If the container or the migration fails; there is no test to run without
/// a backend.
pub async fn start_backend_with_retention_drive() -> (TsHarness, StorageAdapter, SweepDrive) {
    let harness = bring_up_at_chunk_interval(CONTRACT_CHUNK_INTERVAL_SECS)
        .await
        .expect("contract suite needs a migrated TimescaleDB container");
    let store: Arc<dyn RecordStore> = Arc::new(record_store(&harness));
    let drive = SweepDrive {
        pool: harness.pool.clone(),
        metrics: metrics(&harness.pool),
    };
    (harness, StorageAdapter::new(store), drive)
}

/// Drives this backend's own retention sweep, as `ContractRetention` asks.
///
/// Not a test hook: the trait's own docs say a porter implements it against
/// "whatever its storage engine already does on a timer", and what this does
/// is ask for that sweep at a moment a check chooses instead of waiting for a
/// deployment's timer. The production `PgRetentionSweeper` runs, with a stub
/// retention source in place of the registry, so the mark a feed page later
/// refuses against is written by exactly the production interlock.
pub struct SweepDrive {
    pool: PgPool,
    metrics: Arc<Metrics>,
}

#[async_trait::async_trait]
impl ContractRetention for SweepDrive {
    /// Removes `meter`'s entries whose covered period ends before `floor`, by
    /// running the production sweep with a retention of `now - floor - delta`
    /// for that type alone.
    ///
    /// Keyed on [`MeterRef::uuid`], because the production sweep is: this
    /// backend's `usage_feed_retention_marks` and `usage_type_key` both hold
    /// the registry reference, so the sweep has no identifier to resolve by.
    ///
    /// **Two `Duration` types.** `RetentionSource::retention` answers
    /// `std::time::Duration`, while `now - floor` yields `time::Duration`. The
    /// comparison against zero and the arithmetic with `delta` both stay in
    /// `time::Duration`, and only the checked-positive result crosses over via
    /// `TryFrom`.
    async fn drop_before(&self, meter: &MeterRef, floor: OffsetDateTime) -> Result<(), String> {
        let now = OffsetDateTime::now_utc();
        // Half the chunk interval: δ must exceed the drift between this
        // reading of the clock and the one `sweep_under_lock` takes
        // (milliseconds), and must stay inside one chunk so the boundary does
        // not slide to the next.
        //
        // A right shift rather than `/ 2`: this workspace denies
        // `clippy::integer_division`, and a shift halves an even interval
        // exactly, which `CONTRACT_CHUNK_INTERVAL_SECS` is.
        let delta = Duration::seconds(
            i64::try_from(CONTRACT_CHUNK_INTERVAL_SECS >> 1).expect("a small interval"),
        );
        let retention = now - floor - delta;
        if retention <= Duration::ZERO {
            // A floor at or after now has no representable retention, and
            // saturating to zero would purge the whole meter silently. Every
            // fixture period is historical, so this is unreachable from this
            // suite — and `drop_before` returns a String precisely for a
            // scenario the harness could not set up.
            return Err(format!(
                "a retention drive needs a floor in the past: {floor} is not \
                 more than {delta} before {now}"
            ));
        }
        let retention = std::time::Duration::try_from(retention).map_err(|e| {
            format!(
                "a retention drive computed a positive `time::Duration` that would not convert \
                 to `std::time::Duration`: {e}"
            )
        })?;
        let stub = Arc::new(DriveRetention::for_type(meter.uuid, retention));
        PgRetentionSweeper::new(
            self.pool.clone(),
            stub as Arc<dyn RetentionSource>,
            Arc::clone(&self.metrics),
        )
        .sweep_once()
        .await
        .map_err(|e| format!("the retention sweep the drive runs failed: {e}"))?;
        Ok(())
    }
}

/// A [`RetentionSource`] that answers one type's retention and
/// `Err(RetentionError::NotFound)` for every other — the pattern
/// `retention_sweep_integration_pg.rs`'s `StubRetention` already uses.
///
/// **This is what keeps every other check's chunks in place.** `drop_decision`
/// returns `Keep` on the first `Err` among a chunk's types, so a chunk holding
/// a type this drive was never asked about is kept rather than swept, without
/// this drive having to name a retention for it that would then have to stay
/// plausible for every other check sharing the backend.
struct DriveRetention {
    gts_type_uuid: Uuid,
    retention: std::time::Duration,
}

impl DriveRetention {
    fn for_type(gts_type_uuid: Uuid, retention: std::time::Duration) -> Self {
        Self {
            gts_type_uuid,
            retention,
        }
    }
}

#[async_trait::async_trait]
impl RetentionSource for DriveRetention {
    async fn retention(&self, gts_type_uuid: Uuid) -> Result<std::time::Duration, RetentionError> {
        if gts_type_uuid == self.gts_type_uuid {
            Ok(self.retention)
        } else {
            Err(RetentionError::NotFound)
        }
    }
}

// Ledger-entry fixtures
//
// Authored here rather than in each suite because most of the pg suites share
// them, which is the same sharing the file-header `dead_code` allowance is
// earned by.
//
// The one decision a fixture cannot avoid is the covered period, because both
// bounds are inputs to the derived identity
// (`cpt-cf-usage-collector-adr-record-identity-derivation`). It is taken once,
// here, so a suite that needs a *different* period says so at the call site
// instead of every suite picking one.

/// A valid meter type id: the reserved base plus one derivation segment,
/// `~`-terminated, which is what `MeterTypeId::new` validates.
pub const VCPU_METER: &str = "gts.cf.core.uc.usage_record.v1~cf.compute._.vcpu_hours.v1~";

/// A second meter, for the assertions whose subject is that a scope is per
/// `(tenant_id, gts_type_uuid)` rather than per tenant.
pub const GB_METER: &str = "gts.cf.core.uc.usage_record.v1~cf.storage._.gb_hours.v1~";

/// Inclusive start of the covered period every fixture carries by default:
/// `2023-11-14T22:13:20Z`.
pub const FIXTURE_WINDOW_START_UNIX: i64 = 1_700_000_000;

/// Exclusive end of that period, one hour later. Distinct from the start, so a
/// fixture is a period rather than a point event — the latter is a case the
/// suites ask for explicitly (`window_start == window_end`) rather than a shape
/// everything else accidentally inherits.
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

/// The namespace [`meter_ref`] mints its references under.
///
/// A namespace of this harness's own, deliberately **not** the GTS one: a
/// fixture's reference stands in for whatever `types-registry` would have
/// issued, and minting it under the real namespace would make these fixtures
/// look like they derived it — which is the derivation
/// `cpt-cf-types-registry-adr-storage-identity-query-model` bars production
/// code from performing.
const FIXTURE_METER_NAMESPACE: Uuid = Uuid::from_u128(0xfeed_ca75_0000_4000_8000_0000_0000_0001);

/// Every reference this harness has minted, by the identifier it was minted
/// for — the inverse [`meter_of`] reads.
///
/// A registry rather than a second derivation: a `UUIDv5` cannot be
/// inverted, and what a fixture needs back is the identifier, so the only
/// honest way to recover one is to have kept it.
static MINTED_METERS: std::sync::OnceLock<
    std::sync::Mutex<std::collections::HashMap<Uuid, MeterTypeId>>,
> = std::sync::OnceLock::new();

/// Wraps a meter identifier as the reference every SPI method now takes.
///
/// **One reference per identifier.** A `StoredUsageRecord` names its meter by
/// reference alone, so a shared constant would have every entry in this suite
/// come back under one reference whatever meter wrote it — and the multi-meter
/// feed tests, whose whole subject is which meter a page's entries belong to,
/// would pass for the wrong reason.
///
/// A `UUIDv5` over the identifier is a fixture's way of getting a distinct,
/// stable value per meter without inventing a table of literals. It is not a
/// claim about what `types-registry` would issue, and nothing production
/// side may derive a reference this way.
///
/// # Panics
///
/// If the mint registry's lock is poisoned — an earlier fixture panicked
/// while holding it, which is a harness fault either way.
#[must_use]
pub fn meter_ref(id: MeterTypeId) -> MeterRef {
    let uuid = Uuid::new_v5(&FIXTURE_METER_NAMESPACE, id.as_str().as_bytes());
    MINTED_METERS
        .get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
        .lock()
        .expect("the fixture meter registry lock")
        .insert(uuid, id.clone());
    MeterRef::new(uuid, id)
}

/// The registry reference a meter identifier was minted under — the read-side
/// counterpart to [`meter_ref`], for a caller that needs the reference alone.
///
/// Every `RecordStore` read method keys on the reference: the ledger stores no
/// meter identifier. The suites still spell their meters as identifiers,
/// because that is what every fixture builder takes — this is the one place
/// that turns one into the other.
#[must_use]
pub fn meter_reference(id: &MeterTypeId) -> Uuid {
    meter_ref(id.clone()).uuid
}

/// The meter a fixture entry was built under, recovered from the reference
/// it carries.
///
/// Every fixture builder here goes through [`meter_ref`], which records the
/// pair, so this answers for anything this harness built. It is what lets a
/// test write an entry without naming its meter a second time at the call —
/// see [`StoreFixtures`].
///
/// # Panics
///
/// If `record` carries a reference this harness never minted, which means it
/// was not built by a fixture builder here.
#[must_use]
pub fn meter_of(record: &StoredUsageRecord) -> MeterRef {
    let id = MINTED_METERS
        .get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
        .lock()
        .expect("the fixture meter registry lock")
        .get(&record.gts_type_uuid)
        .cloned()
        .unwrap_or_else(|| {
            panic!(
                "entry {} carries registry reference {}, which this harness never minted: build \
                 fixtures through `entry_over` / `entry` / `withdrawal_of` so the meter is \
                 recoverable",
                record.id, record.gts_type_uuid
            )
        });
    MeterRef::new(record.gts_type_uuid, id)
}

/// `RecordStore` and SPI writes for an entry this harness built.
///
/// Every write path takes the meter beside the entry, and every fixture here
/// already carries the reference of the meter it was built under. These recover
/// it ([`meter_of`]) rather than making every call site name the same meter
/// twice — which would be two places for one fact, and the place a test could
/// write an entry under a meter it was not built for unnoticed.
#[async_trait::async_trait]
pub trait StoreFixtures {
    /// `RecordStore::create_batch` for a single fixture entry: one entry in,
    /// one slot out. The store has no single-row path any more, so this is a
    /// one-element batch rather than a second method.
    async fn create_fixture(
        &self,
        record: StoredUsageRecord,
    ) -> Result<StoredUsageRecord, UsageCollectorPluginError>;
    /// `RecordStore::create_batch` for fixture entries.
    async fn create_fixture_batch(
        &self,
        records: Vec<StoredUsageRecord>,
    ) -> Result<Vec<Result<StoredUsageRecord, UsageCollectorPluginError>>, UsageCollectorPluginError>;
}

#[async_trait::async_trait]
impl<T: RecordStore + ?Sized> StoreFixtures for T {
    async fn create_fixture(
        &self,
        record: StoredUsageRecord,
    ) -> Result<StoredUsageRecord, UsageCollectorPluginError> {
        let mut outcomes = self
            .create_batch(vec![(meter_of(&record), record)])
            .await?
            .into_iter();
        outcomes
            .next()
            .expect("one entry in, one slot out: create_batch's per-record-outcomes guarantee")
    }

    async fn create_fixture_batch(
        &self,
        records: Vec<StoredUsageRecord>,
    ) -> Result<Vec<Result<StoredUsageRecord, UsageCollectorPluginError>>, UsageCollectorPluginError>
    {
        let paired = records
            .into_iter()
            .map(|record| (meter_of(&record), record))
            .collect();
        self.create_batch(paired).await
    }
}

/// The same, one level up: the SPI surface rather than the store port.
#[async_trait::async_trait]
pub trait PluginFixtures {
    /// `UsageCollectorPluginV1::create_usage_records` for a single fixture
    /// entry: one entry in, one slot out, over the batch SPI.
    async fn create_fixture_usage_record(
        &self,
        record: StoredUsageRecord,
    ) -> Result<StoredUsageRecord, UsageCollectorPluginError>;
}

#[async_trait::async_trait]
impl<T: UsageCollectorPluginV1 + ?Sized> PluginFixtures for T {
    async fn create_fixture_usage_record(
        &self,
        record: StoredUsageRecord,
    ) -> Result<StoredUsageRecord, UsageCollectorPluginError> {
        let meter = meter_of(&record);
        let mut outcomes = self
            .create_usage_records(vec![(meter, record)])
            .await?
            .into_iter();
        outcomes.next().expect(
            "one entry in, one slot out: create_usage_records's per-record-outcomes guarantee",
        )
    }
}

/// An ordinary measurement over an explicit covered period, with the derived
/// identity the Ingestion Gateway would stamp on it.
///
/// The `id` is [`derive_usage_record_id`] over the same inputs the ledger's
/// `usage_records_dedup_uniq` is built over — the gateway's job, not the
/// plugin's, so deriving it here stands in for the gateway rather than asking
/// the code under test to check itself. The entry type is
/// [`EntryType::Record`] here because this builds a measurement;
/// [`withdrawal_of`] builds the other kind.
///
/// A field outside the identity inputs (quantity, attribution, metadata,
/// origin) can be overwritten with a struct update afterwards. The
/// caller-chosen identity inputs appear in this signature for exactly that
/// reason; setting the invalidation pair afterwards does move the entry's
/// identity, which is what [`rederive`] is for.
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
) -> StoredUsageRecord {
    let idempotency_key = IdempotencyKey::new(idem).expect("valid idempotency key");
    UsageRecord {
        id: derive_usage_record_id(
            tenant,
            meter_id,
            &idempotency_key,
            window_start,
            window_end,
            EntryType::Record,
        ),
        gts_type_id: meter_id.clone(),
        tenant_id: tenant,
        resource_ref: ResourceRef::new("res-1", "compute.vm").expect("valid resource ref"),
        subject_ref: None,
        metadata: BTreeMap::new(),
        quantity: UsageQuantity::try_from(value).expect("fixture quantity"),
        idempotency_key,
        accepted_at: fixture_window_end(),
        origin: RecordOrigin::Live,
        invalidation: None,
        window_start,
        window_end,
    }
    // Built as a `UsageRecord` and converted, so the derived identity still
    // reads the identifier; the reference is attached afterwards, from the
    // one place this harness mints references.
    .into_stored(meter_ref(meter_id.clone()).uuid)
}

/// [`entry_over`] at the default covered period.
#[must_use]
pub fn entry(
    meter_id: &MeterTypeId,
    tenant: Uuid,
    idem: &str,
    value: Decimal,
) -> StoredUsageRecord {
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
/// invalidation pair, under the target's own idempotency key.
///
/// The published schema's own phrase for this shape is *"An invalidation is a
/// faithful copy of its target"* (`usage-collector-v1.yaml`), and every field
/// copied below is copied for a reason rather than for tidiness:
///
/// * **The quantity** — an invalidation echoes what it withdraws rather than
///   negating it (`cpt-cf-usage-collector-adr-append-only-invalidation`), so
///   netting the two would double-count.
/// * **The covered period and the idempotency key** — dedup-identity inputs,
///   so a withdrawal carrying different ones is a different identity and no
///   longer collides with another withdrawal of the same target.
/// * **The attribution, metadata and origin** — so the only fields separating
///   the pair are `invalidates` and `reason_code`. A withdrawal that quietly
///   differed in, say, `resource_type` would let a `$filter` test look like it
///   discriminated when it had only found an asymmetry the fixture put there.
///
/// The entry type is the one derivation input the two do not share, and is
/// therefore the whole reason their identifiers differ
/// (`cpt-cf-usage-collector-adr-record-identity-derivation`); two withdrawals
/// of one target agree on every input and derive one identifier. There is no
/// parameter for the key: it is the target's, so `entry_over`'s `idem` argument
/// only ever seeds a value this function immediately overwrites.
///
/// # Panics
///
/// If the fixed reason code fails to validate.
#[must_use]
pub fn withdrawal_of(target: &StoredUsageRecord) -> StoredUsageRecord {
    rederive(StoredUsageRecord {
        idempotency_key: target.idempotency_key.clone(),
        invalidation: Some(Invalidation {
            target: target.id,
            reason: ReasonCode::new("duplicate_submission").expect("valid reason code"),
        }),
        resource_ref: target.resource_ref.clone(),
        subject_ref: target.subject_ref.clone(),
        metadata: target.metadata.clone(),
        origin: target.origin,
        ..entry_over(
            &meter_of(target).id,
            target.tenant_id,
            "withdrawal-of-placeholder-key",
            target.quantity.as_decimal(),
            target.window_start,
            target.window_end,
        )
    })
}

/// A withdrawal of `target` carrying `reason` instead of [`withdrawal_of`]'s
/// fixed `"duplicate_submission"`.
///
/// Every withdrawal of one target carries the target's own idempotency key and
/// `entry_type = invalidation`, so every identity input agrees
/// (`cpt-cf-usage-collector-adr-record-identity-derivation`) and two
/// withdrawals built through [`withdrawal_of`] alone would be byte-for-byte
/// identical and absorbed as an idempotent replay rather than decided against
/// each other. `reason_code` is the one field outside the derivation, so a test
/// building a *second* withdrawal of one target uses this to keep them apart.
///
/// # Panics
///
/// If `reason` fails to validate.
#[must_use]
pub fn withdrawal_of_with_reason(target: &StoredUsageRecord, reason: &str) -> StoredUsageRecord {
    StoredUsageRecord {
        invalidation: Some(Invalidation {
            target: target.id,
            reason: ReasonCode::new(reason).expect("valid reason code"),
        }),
        ..withdrawal_of(target)
    }
}

/// Restamp `record`'s derived identity after a dedup-identity input was changed
/// by a struct update.
///
/// [`entry_over`] takes the caller-chosen inputs as parameters precisely so
/// this is rarely needed. Two cases are left: setting the invalidation pair
/// changes the entry type, which is why [`withdrawal_of`] ends here; and a test
/// that starts from [`withdrawal_of`] and moves the entry into a different
/// scope changes `tenant_id` or the meter. Leaving the stamped id alone in
/// either case would store a row whose id no emitter could reproduce, and the
/// ledger's `id` and its dedup UNIQUE would disagree about what the entry is.
///
/// The entry type is read off the record rather than passed in, so it cannot be
/// stamped as anything other than what the record's own invalidation pair says.
///
/// # Panics
///
/// Never: the record already carries a validated key and meter.
#[must_use]
pub fn rederive(record: StoredUsageRecord) -> StoredUsageRecord {
    StoredUsageRecord {
        id: derive_usage_record_id(
            record.tenant_id,
            &meter_of(&record).id,
            &record.idempotency_key,
            record.window_start,
            record.window_end,
            record.entry_type(),
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

/// Insert one `usage_records` row binding every column directly, so a test can
/// write a shape the SPI cannot express.
///
/// Every column is defaulted to a well-formed ordinary measurement, and the
/// columns `usage_records_invalidation_pairing` spans are the settable ones:
/// the SPI builds an entry's kind and its withdrawal pair from one
/// `Option<Invalidation>` (`UsageRecord::entry_type`,
/// `mapper::invalidation_to_row`), so a row whose declared kind disagrees with
/// its pair is reachable only here. Binding `entry_type` explicitly is what
/// makes this an escape hatch rather than a second `create`.
#[must_use]
pub fn insert_raw_entry(pool: &PgPool) -> RawEntry<'_> {
    RawEntry {
        pool,
        id: Uuid::new_v4(),
        entry_type: "record",
        invalidates: None,
        reason_code: None,
    }
}

/// The tenant every [`insert_raw_entry`] row is written under.
///
/// The max UUID, because every other tenant in these suites is a small
/// hand-picked integer: a raw row can then never be mistaken for one an
/// SPI-driven test wrote, whatever values that test picks later.
const RAW_ENTRY_TENANT: Uuid = Uuid::max();

/// The builder [`insert_raw_entry`] returns. See its doc.
pub struct RawEntry<'a> {
    pool: &'a PgPool,
    id: Uuid,
    entry_type: &'a str,
    invalidates: Option<Uuid>,
    reason_code: Option<&'a str>,
}

impl<'a> RawEntry<'a> {
    /// Declare the entry's kind, as the `usage_entry_type` label to store.
    /// Taken as a string rather than as [`EntryType`] so a test can write a
    /// label the enum does not carry.
    #[must_use]
    pub const fn entry_type(mut self, kind: &'a str) -> Self {
        self.entry_type = kind;
        self
    }

    /// Set (or clear) the entry this one withdraws.
    #[must_use]
    pub const fn invalidates(mut self, target: Option<Uuid>) -> Self {
        self.invalidates = target;
        self
    }

    /// Set (or clear) the withdrawal reason.
    #[must_use]
    pub const fn reason_code(mut self, reason: Option<&'a str>) -> Self {
        self.reason_code = reason;
        self
    }

    /// Run the insert.
    ///
    /// Returns the raw `sqlx` error on refusal, so a caller can assert on the
    /// constraint name `PostgreSQL` reports.
    ///
    /// The type key is bound as a literal rather than resolved through
    /// `usage_type_key`: nothing references that table, so any integer
    /// partitions the row correctly for a test that never reads it back by
    /// type.
    ///
    /// # Errors
    ///
    /// Whatever `PostgreSQL` refuses the row with.
    pub async fn execute(self) -> Result<(), sqlx::Error> {
        sqlx::query(
            "INSERT INTO usage_records (id, tenant_id, gts_type_uuid, type_key, quantity, \
                 window_start, window_end, resource_id, resource_type, subject_id, \
                 subject_type, idempotency_key, invalidates, reason_code, origin, \
                 entry_type, accepted_at, metadata) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, NULL, NULL, $10, $11, $12, 'live', \
                 $13::usage_entry_type, $14, '{}'::jsonb)",
        )
        .bind(self.id)
        .bind(RAW_ENTRY_TENANT)
        // The meter's registry reference, the ledger's only meter identity.
        .bind(meter_ref(meter(VCPU_METER)).uuid)
        .bind(1_i32)
        .bind(Decimal::ONE)
        .bind(fixture_window_start())
        .bind(fixture_window_end())
        .bind("raw-resource-id")
        .bind("raw-resource-type")
        .bind(self.id.to_string())
        .bind(self.invalidates)
        .bind(self.reason_code)
        .bind(self.entry_type)
        .bind(fixture_window_end())
        .execute(self.pool)
        .await
        .map(|_| ())
    }
}
