#![cfg(feature = "postgres")]
#![allow(clippy::expect_used, clippy::unwrap_used)]
//! The retention sweep against a live `TimescaleDB`, over a stub retention
//! source whose values a test can amend between sweeps. Requires Docker.
//!
//! Fixtures are written through the real ingest path with a covered period
//! that ended a given number of days ago; the default 7-day chunk interval puts
//! entries of one type and one age into one chunk.

mod common;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration as StdDuration;

use async_trait::async_trait;
use rust_decimal::Decimal;
use time::{Duration, OffsetDateTime};
use uuid::Uuid;

use usage_collector_sdk::UsageRecord;

use timescaledb_usage_collector_plugin::domain::ports::{
    RecordStore, RetentionError, RetentionSource,
};
use timescaledb_usage_collector_plugin::infra::metrics::Metrics;
use timescaledb_usage_collector_plugin::infra::storage::pool::apply_post_migration_setup;
use timescaledb_usage_collector_plugin::infra::storage::retention_sweep::{
    PgRetentionSweeper, SWEEP_ADVISORY_LOCK_KEY, list_chunks,
};
use timescaledb_usage_collector_plugin::infra::storage::rollup_maintenance::materialization_table;

const DAY: u64 = 86_400;

/// Retention per type, amendable mid-test. A type with no entry is not found.
#[derive(Default)]
struct StubRetention {
    by_type: Mutex<HashMap<String, Result<StdDuration, RetentionError>>>,
}

impl StubRetention {
    fn set(&self, gts_type_id: &str, retention: Result<StdDuration, RetentionError>) {
        self.by_type
            .lock()
            .unwrap()
            .insert(gts_type_id.to_owned(), retention);
    }
}

#[async_trait]
impl RetentionSource for StubRetention {
    async fn retention(&self, gts_type_id: &str) -> Result<StdDuration, RetentionError> {
        self.by_type
            .lock()
            .unwrap()
            .get(gts_type_id)
            .cloned()
            .unwrap_or(Err(RetentionError::NotFound))
    }
}

// The `Result` wrapper is never `Err` in this suite, but it must match
// `StubRetention::set`'s `Result<StdDuration, RetentionError>` parameter, so
// every call site can read `days(n)` beside the `Err(...)` fixtures without a
// second wrapping.
#[allow(clippy::unnecessary_wraps)]
fn days(n: u64) -> Result<StdDuration, RetentionError> {
    Ok(StdDuration::from_secs(n * DAY))
}

/// An entry of `meter` whose covered period ended `days_ago` days ago.
fn aged(meter: &str, tenant: Uuid, idem: &str, days_ago: i64) -> UsageRecord {
    let end = OffsetDateTime::now_utc() - Duration::days(days_ago);
    common::entry_over(
        &common::meter(meter),
        tenant,
        idem,
        Decimal::ONE,
        end - Duration::hours(1),
        end,
    )
}

async fn stored(pool: &sqlx::PgPool, id: Uuid) -> bool {
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM usage_records WHERE id = $1")
        .bind(id)
        .fetch_one(pool)
        .await
        .expect("count by id");
    n == 1
}

fn sweeper(pool: &sqlx::PgPool, stub: &Arc<StubRetention>) -> PgRetentionSweeper {
    PgRetentionSweeper::new(
        pool.clone(),
        Arc::clone(stub) as Arc<dyn RetentionSource>,
        Arc::new(Metrics::new(pool.clone())),
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn each_type_expires_at_its_own_retention() {
    let h = common::bring_up()
        .await
        .expect("timescaledb container (Docker required)");
    let store = common::record_store(&h.pool);
    let tenant = Uuid::from_u128(0x5E01);
    let stub = Arc::new(StubRetention::default());
    stub.set(common::VCPU_METER, days(30));
    stub.set(common::GB_METER, days(400));

    let old_vcpu = aged(common::VCPU_METER, tenant, "old-vcpu", 100);
    let old_gb = aged(common::GB_METER, tenant, "old-gb", 100);
    let new_vcpu = aged(common::VCPU_METER, tenant, "new-vcpu", 0);
    let (old_vcpu_id, old_gb_id, new_vcpu_id) = (old_vcpu.id, old_gb.id, new_vcpu.id);
    for entry in [old_vcpu, old_gb, new_vcpu] {
        store.create(entry).await.expect("create fixture");
    }

    let report = sweeper(&h.pool, &stub).sweep_once().await.expect("sweep");

    assert_eq!(report.dropped, 1, "{report:?}");
    assert!(
        !stored(&h.pool, old_vcpu_id).await,
        "vcpu past 30 days is dropped"
    );
    assert!(
        stored(&h.pool, old_gb_id).await,
        "gb inside 400 days survives beside it"
    );
    assert!(
        stored(&h.pool, new_vcpu_id).await,
        "a current vcpu period survives"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_amended_retention_applies_to_entries_already_stored() {
    let h = common::bring_up()
        .await
        .expect("timescaledb container (Docker required)");
    let store = common::record_store(&h.pool);
    let stub = Arc::new(StubRetention::default());
    let entry = aged(common::VCPU_METER, Uuid::from_u128(0x5E02), "amended", 100);
    let id = entry.id;
    store.create(entry).await.expect("create fixture");
    let sweeper = sweeper(&h.pool, &stub);

    stub.set(common::VCPU_METER, days(400));
    let raised = sweeper.sweep_once().await.expect("sweep after raise");
    assert_eq!(raised.dropped, 0, "{raised:?}");
    assert!(
        stored(&h.pool, id).await,
        "a raised retention keeps an entry already stored"
    );

    stub.set(common::VCPU_METER, days(30));
    let lowered = sweeper.sweep_once().await.expect("sweep after lower");
    assert_eq!(lowered.dropped, 1, "{lowered:?}");
    assert!(
        !stored(&h.pool, id).await,
        "a lowered retention drops an entry already stored"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unresolvable_type_keeps_its_chunks_while_others_still_drop() {
    let h = common::bring_up()
        .await
        .expect("timescaledb container (Docker required)");
    let store = common::record_store(&h.pool);
    let tenant = Uuid::from_u128(0x5E03);
    let stub = Arc::new(StubRetention::default());
    stub.set(
        common::VCPU_METER,
        Err(RetentionError::Unavailable("registry down".to_owned())),
    );
    stub.set(common::GB_METER, days(30));

    let vcpu = aged(common::VCPU_METER, tenant, "vcpu", 100);
    let gb = aged(common::GB_METER, tenant, "gb", 100);
    let (vcpu_id, gb_id) = (vcpu.id, gb.id);
    store.create(vcpu).await.expect("create vcpu");
    store.create(gb).await.expect("create gb");

    let report = sweeper(&h.pool, &stub).sweep_once().await.expect("sweep");

    assert_eq!(
        (report.dropped, report.kept_unresolved),
        (1, 1),
        "{report:?}"
    );
    assert!(
        stored(&h.pool, vcpu_id).await,
        "no definite retention, no drop"
    );
    assert!(
        !stored(&h.pool, gb_id).await,
        "a resolvable expired type still drops"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_shared_slice_is_held_to_its_longest_retention() {
    let h = common::bring_up()
        .await
        .expect("timescaledb container (Docker required)");
    // Before any write, so the first chunks are created at width 4: keys 1 and
    // 2 share the slice [0, 4).
    let widened = timescaledb_usage_collector_plugin::config::TimescaleDbPluginConfig {
        type_key_slice_width: 4,
        ..h.cfg.clone()
    };
    apply_post_migration_setup(&h.pool, &widened)
        .await
        .expect("widen the type-key slice");
    // apply_post_migration_setup re-creates the refresh policies; remove them
    // again so a background refresh cannot race this test's own sweeps.
    common::settle_and_remove_rollup_policies(&h.pool)
        .await
        .expect("settle and remove the re-created policies");
    let store = common::record_store(&h.pool);
    let tenant = Uuid::from_u128(0x5E04);
    let stub = Arc::new(StubRetention::default());
    stub.set(common::VCPU_METER, days(30));
    stub.set(common::GB_METER, days(400));

    let vcpu = aged(common::VCPU_METER, tenant, "vcpu", 100);
    let gb = aged(common::GB_METER, tenant, "gb", 100);
    let (vcpu_id, gb_id) = (vcpu.id, gb.id);
    store.create(vcpu).await.expect("create vcpu");
    store.create(gb).await.expect("create gb");
    let chunks: i64 = sqlx::query_scalar("SELECT count(*) FROM show_chunks('usage_records')")
        .fetch_one(&h.pool)
        .await
        .expect("show_chunks");
    assert_eq!(chunks, 1, "both types share one chunk at width 4");
    let sweeper = sweeper(&h.pool, &stub);

    let held = sweeper.sweep_once().await.expect("first sweep");
    assert_eq!(held.dropped, 0, "{held:?}");
    assert!(
        stored(&h.pool, vcpu_id).await,
        "held to gb's 400 days, not vcpu's 30"
    );

    stub.set(common::GB_METER, days(30));
    let released = sweeper.sweep_once().await.expect("second sweep");
    assert_eq!(released.dropped, 1, "{released:?}");
    assert!(!stored(&h.pool, vcpu_id).await && !stored(&h.pool, gb_id).await);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_invalidation_and_its_target_drop_together() {
    let h = common::bring_up()
        .await
        .expect("timescaledb container (Docker required)");
    let store = common::record_store(&h.pool);
    let stub = Arc::new(StubRetention::default());
    stub.set(common::VCPU_METER, days(30));

    let target = aged(common::VCPU_METER, Uuid::from_u128(0x5E05), "target", 100);
    let withdrawal = common::withdrawal_of(&target, "withdrawal");
    let (target_id, withdrawal_id) = (target.id, withdrawal.id);
    store.create(target).await.expect("create target");
    store.create(withdrawal).await.expect("create withdrawal");

    sweeper(&h.pool, &stub).sweep_once().await.expect("sweep");

    assert!(!stored(&h.pool, target_id).await, "the target is dropped");
    assert!(
        !stored(&h.pool, withdrawal_id).await,
        "its invalidation goes with it"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_write_into_a_dropped_range_is_collected_by_the_next_sweep() {
    let h = common::bring_up()
        .await
        .expect("timescaledb container (Docker required)");
    let store = common::record_store(&h.pool);
    let tenant = Uuid::from_u128(0x5E06);
    let stub = Arc::new(StubRetention::default());
    stub.set(common::VCPU_METER, days(30));
    let sweeper = sweeper(&h.pool, &stub);

    store
        .create(aged(common::VCPU_METER, tenant, "first", 100))
        .await
        .expect("create first");
    assert_eq!(sweeper.sweep_once().await.expect("sweep 1").dropped, 1);

    let late = aged(common::VCPU_METER, tenant, "late", 100);
    let late_id = late.id;
    store
        .create(late)
        .await
        .expect("a write into a dropped range recreates its chunk");
    assert!(stored(&h.pool, late_id).await);

    assert_eq!(sweeper.sweep_once().await.expect("sweep 2").dropped, 1);
    assert!(
        !stored(&h.pool, late_id).await,
        "the recreated chunk is collected"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_sweep_skips_while_another_session_holds_the_lock() {
    let h = common::bring_up()
        .await
        .expect("timescaledb container (Docker required)");
    let store = common::record_store(&h.pool);
    let stub = Arc::new(StubRetention::default());
    stub.set(common::VCPU_METER, days(30));
    let entry = aged(common::VCPU_METER, Uuid::from_u128(0x5E07), "locked", 100);
    let id = entry.id;
    store.create(entry).await.expect("create fixture");
    let sweeper = sweeper(&h.pool, &stub);

    // A different session: the advisory lock is re-entrant within one.
    let mut holder = h.pool.acquire().await.expect("acquire lock holder");
    sqlx::query("SELECT pg_advisory_lock($1)")
        .bind(SWEEP_ADVISORY_LOCK_KEY)
        .execute(&mut *holder)
        .await
        .expect("take the sweep lock");

    let skipped = sweeper.sweep_once().await.expect("sweep while locked");
    assert!(skipped.skipped_locked, "{skipped:?}");
    assert!(stored(&h.pool, id).await, "a skipped sweep drops nothing");

    sqlx::query("SELECT pg_advisory_unlock($1)")
        .bind(SWEEP_ADVISORY_LOCK_KEY)
        .execute(&mut *holder)
        .await
        .expect("release the sweep lock");
    let ran = sweeper.sweep_once().await.expect("sweep after release");
    assert!(!ran.skipped_locked && ran.dropped == 1, "{ran:?}");
}

/// Whether `usage_rollup_1h` holds a row for `meter` at exactly `bucket`.
async fn bucket_exists(pool: &sqlx::PgPool, meter: &str, bucket: OffsetDateTime) -> bool {
    let n: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM usage_rollup_1h WHERE gts_type_id = $1 AND bucket = $2",
    )
    .bind(meter)
    .bind(bucket)
    .fetch_one(pool)
    .await
    .expect("count rollup bucket");
    n > 0
}

async fn rollup_rows(pool: &sqlx::PgPool, meter: &str) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM usage_rollup_1h WHERE gts_type_id = $1")
        .bind(meter)
        .fetch_one(pool)
        .await
        .expect("count rollup rows")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_drop_takes_its_types_rollup_rows_and_leaves_the_others() {
    let h = common::bring_up()
        .await
        .expect("timescaledb container (Docker required)");
    let store = common::record_store(&h.pool);
    let tenant = Uuid::from_u128(0x5E10);
    let stub = Arc::new(StubRetention::default());
    stub.set(common::VCPU_METER, days(30));
    stub.set(common::GB_METER, days(400));
    store
        .create(aged(common::VCPU_METER, tenant, "vcpu", 100))
        .await
        .expect("vcpu");
    store
        .create(aged(common::GB_METER, tenant, "gb", 100))
        .await
        .expect("gb");
    common::refresh_rollup(&h.pool).await;
    assert_eq!(
        (
            rollup_rows(&h.pool, common::VCPU_METER).await,
            rollup_rows(&h.pool, common::GB_METER).await
        ),
        (1, 1)
    );

    let report = sweeper(&h.pool, &stub).sweep_once().await.expect("sweep");

    assert_eq!(
        (report.dropped, report.rollup_rows_deleted),
        (1, 1),
        "{report:?}"
    );
    assert_eq!(
        rollup_rows(&h.pool, common::VCPU_METER).await,
        0,
        "the expired type's rollup rows go with its chunk"
    );
    assert_eq!(
        rollup_rows(&h.pool, common::GB_METER).await,
        1,
        "the other type's rollup rows stay"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_shared_slice_drop_cuts_every_type_in_its_key_range() {
    let h = common::bring_up()
        .await
        .expect("timescaledb container (Docker required)");
    let widened = timescaledb_usage_collector_plugin::config::TimescaleDbPluginConfig {
        type_key_slice_width: 4,
        ..h.cfg.clone()
    };
    apply_post_migration_setup(&h.pool, &widened)
        .await
        .expect("widen");
    // apply_post_migration_setup re-creates the refresh policies; remove them
    // again so a background refresh cannot race this test's own sweep.
    common::settle_and_remove_rollup_policies(&h.pool)
        .await
        .expect("settle and remove the re-created policies");
    let store = common::record_store(&h.pool);
    let tenant = Uuid::from_u128(0x5E11);
    let stub = Arc::new(StubRetention::default());
    stub.set(common::VCPU_METER, days(30));
    stub.set(common::GB_METER, days(30));
    store
        .create(aged(common::VCPU_METER, tenant, "vcpu", 100))
        .await
        .expect("vcpu");
    store
        .create(aged(common::GB_METER, tenant, "gb", 100))
        .await
        .expect("gb");
    common::refresh_rollup(&h.pool).await;

    let report = sweeper(&h.pool, &stub).sweep_once().await.expect("sweep");

    assert_eq!(
        (report.dropped, report.rollup_rows_deleted),
        (1, 2),
        "{report:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn without_the_rollup_nothing_is_dropped() {
    let h = common::bring_up()
        .await
        .expect("timescaledb container (Docker required)");
    let store = common::record_store(&h.pool);
    let stub = Arc::new(StubRetention::default());
    stub.set(common::VCPU_METER, days(30));
    let entry = aged(common::VCPU_METER, Uuid::from_u128(0x5E12), "kept", 100);
    let id = entry.id;
    store.create(entry).await.expect("create");
    sqlx::query("DROP MATERIALIZED VIEW usage_rollup_1h")
        .execute(&h.pool)
        .await
        .expect("drop the rollup");

    assert!(sweeper(&h.pool, &stub).sweep_once().await.is_err());
    assert!(
        stored(&h.pool, id).await,
        "no rollup to cut, so no chunk is dropped"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_late_write_into_a_dropped_range_is_the_only_thing_the_next_refresh_rolls_up() {
    let h = common::bring_up()
        .await
        .expect("timescaledb container (Docker required)");
    let store = common::record_store(&h.pool);
    let tenant = Uuid::from_u128(0x5E13);
    let stub = Arc::new(StubRetention::default());
    stub.set(common::VCPU_METER, days(30));
    let first = aged(common::VCPU_METER, tenant, "first", 100);
    let (start, end) = (first.window_start, first.window_end);
    store.create(first).await.expect("first");
    common::refresh_rollup(&h.pool).await;
    assert_eq!(
        sweeper(&h.pool, &stub)
            .sweep_once()
            .await
            .expect("sweep")
            .dropped,
        1
    );

    let late = common::entry_over(
        &common::meter(common::VCPU_METER),
        tenant,
        "late",
        Decimal::from(3),
        start,
        end,
    );
    store
        .create(late)
        .await
        .expect("late write recreates the chunk");
    common::refresh_rollup(&h.pool).await;

    let total: Option<rust_decimal::Decimal> =
        sqlx::query_scalar("SELECT sum(sum_value) FROM usage_rollup_1h WHERE gts_type_id = $1")
            .bind(common::VCPU_METER)
            .fetch_one(&h.pool)
            .await
            .expect("rollup total");
    assert_eq!(
        total,
        Some(Decimal::from(3)),
        "only the surviving late row is rolled up"
    );
}

/// The rollup cut is bounded to the dropped chunk's own bucket range: an
/// expired chunk's rollup row is deleted, and the very next hour's row, which
/// belongs to a chunk that has not expired, survives untouched.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_rollup_drop_cuts_only_the_dropped_chunks_bucket_range() {
    let h = common::bring_up()
        .await
        .expect("timescaledb container (Docker required)");
    let store = common::record_store(&h.pool);
    let tenant = Uuid::from_u128(0x5E20);
    let stub = Arc::new(StubRetention::default());

    // Discover the boundary between two adjacent chunks under the default
    // 7-day chunk_time_interval_secs, without assuming how TimescaleDB
    // anchors it: write a throw-away entry far in the past, read its chunk's
    // [time_start, time_end) back from the catalog via list_chunks, and take
    // time_end as the boundary. The row is then removed out of band so it
    // does not add a second rollup bucket to the expired chunk below.
    let probe = aged(common::VCPU_METER, tenant, "probe", 100);
    let probe_id = probe.id;
    let probe_end = probe.window_end;
    store.create(probe).await.expect("create probe");
    let probe_chunk = list_chunks(&h.pool)
        .await
        .expect("list chunks")
        .into_iter()
        .find(|c| c.time_start <= probe_end && probe_end < c.time_end)
        .expect("the probe landed in a chunk");
    let boundary = probe_chunk.time_end;
    sqlx::query("DELETE FROM usage_records WHERE id = $1")
        .bind(probe_id)
        .execute(&h.pool)
        .await
        .expect("remove the probe");

    // Two adjacent chunks at the discovered boundary: the expired chunk gets
    // an entry in its LAST hour (window_end in [boundary - 1h, boundary)),
    // the fresh chunk directly after it gets an entry in its FIRST hour
    // (window_end in [boundary, boundary + 1h)). chunk_time_interval_secs is
    // validated as a multiple of 3600 precisely so every hourly bucket lies
    // inside exactly one chunk (spec §5.4); that makes `boundary` itself
    // hour-aligned, so each entry's time_bucket('1 hour', window_end) bucket
    // lands wholly inside its own chunk.
    let expired_end = boundary - Duration::minutes(30);
    let fresh_end = boundary + Duration::minutes(30);
    let expired = common::entry_over(
        &common::meter(common::VCPU_METER),
        tenant,
        "expired",
        Decimal::ONE,
        expired_end - Duration::hours(1),
        expired_end,
    );
    let fresh = common::entry_over(
        &common::meter(common::VCPU_METER),
        tenant,
        "fresh",
        Decimal::ONE,
        fresh_end - Duration::hours(1),
        fresh_end,
    );
    store.create(expired).await.expect("create expired entry");
    store.create(fresh).await.expect("create fresh entry");

    // Retention arithmetic. `drop_decision` drops a chunk when
    // `chunk.time_end + retention < now`. The expired chunk's time_end is
    // `boundary`; the fresh chunk's is `boundary + 7 days` (the default
    // chunk width). Anchoring the retention off `now - boundary`, rather
    // than off a fixed day count, makes the arithmetic hold regardless of
    // exactly where inside the 7-day window the 100-day-old probe landed:
    //   expired: boundary + retention < now        =>  retention < now - boundary
    //   fresh:   boundary + 7d + retention >= now   =>  retention >= now - boundary - 7d
    // retention = (now - boundary) - 1 day satisfies both: a full day of
    // margin on the expired side, and (7 days - 1 day) = 6 days of margin on
    // the fresh side.
    let now = OffsetDateTime::now_utc();
    let secs_since_boundary = (now - boundary).whole_seconds();
    assert!(
        secs_since_boundary > i64::try_from(DAY).expect("DAY fits i64"),
        "the discovered boundary must be more than a day in the past: {secs_since_boundary}s"
    );
    let retention_secs = u64::try_from(secs_since_boundary).expect("positive, checked above") - DAY;
    stub.set(
        common::VCPU_METER,
        Ok(StdDuration::from_secs(retention_secs)),
    );

    common::refresh_rollup(&h.pool).await;
    let report = sweeper(&h.pool, &stub).sweep_once().await.expect("sweep");

    assert_eq!(
        (report.dropped, report.rollup_rows_deleted),
        (1, 1),
        "{report:?}"
    );

    let expired_bucket = boundary - Duration::hours(1);
    let fresh_bucket = boundary;
    assert!(
        !bucket_exists(&h.pool, common::VCPU_METER, expired_bucket).await,
        "the expired chunk's rollup bucket is cut with its chunk"
    );
    assert!(
        bucket_exists(&h.pool, common::VCPU_METER, fresh_bucket).await,
        "the fresh chunk's rollup bucket survives untouched"
    );
}

/// A failed rollup-row delete rolls back the whole drop: the chunk stays, the
/// ledger entry stays, and the failure is counted rather than the sweep
/// silently dropping a chunk whose rollup rows it could not also delete.
///
/// Mechanism: a `BEFORE DELETE` trigger on the materialisation hypertable
/// (`rollup_maintenance::materialization_table`) that always raises. Tried
/// against a live `timescale/timescaledb:2.29.2-pg18` container before this
/// test was written: `TimescaleDB` accepts an ordinary trigger on that table
/// without complaint (it is a plain heap table under
/// `_timescaledb_internal`, owned by the same role the test pool connects
/// as), so the `REVOKE DELETE` fallback the brief allows for was not needed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_failed_rollup_row_delete_rolls_back_the_chunk_drop() {
    let h = common::bring_up()
        .await
        .expect("timescaledb container (Docker required)");
    let store = common::record_store(&h.pool);
    let stub = Arc::new(StubRetention::default());
    stub.set(common::VCPU_METER, days(30));

    let entry = aged(common::VCPU_METER, Uuid::from_u128(0x5E21), "rollback", 100);
    let id = entry.id;
    store.create(entry).await.expect("create fixture");
    common::refresh_rollup(&h.pool).await;

    let table = materialization_table(&h.pool)
        .await
        .expect("materialisation table query")
        .expect("the rollup has a materialisation table");
    sqlx::query(
        "CREATE FUNCTION uc_test_raise_on_rollup_delete() RETURNS trigger AS \
         $$ BEGIN RAISE EXCEPTION 'delete refused for rollback test'; END; $$ \
         LANGUAGE plpgsql",
    )
    .execute(&h.pool)
    .await
    .expect("create the raising trigger function");
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "CREATE TRIGGER uc_test_refuse_delete BEFORE DELETE ON {table} \
         FOR EACH ROW EXECUTE FUNCTION uc_test_raise_on_rollup_delete()"
    )))
    .execute(&h.pool)
    .await
    .expect("install the raising trigger");

    let report = sweeper(&h.pool, &stub).sweep_once().await.expect("sweep");

    assert_eq!((report.drop_failures, report.dropped), (1, 0), "{report:?}");
    assert!(
        stored(&h.pool, id).await,
        "the chunk drop rolled back along with the failed rollup-row delete"
    );
}
