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
    CHUNK_HIGHEST_POSITIONS_SQL, PgRetentionSweeper, SWEEP_ADVISORY_LOCK_KEY, list_chunks,
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
    let store = common::record_store(&h);
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
    let store = common::record_store(&h);
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
    let store = common::record_store(&h);
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
    let store = common::record_store(&h);
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
    let store = common::record_store(&h);
    let stub = Arc::new(StubRetention::default());
    stub.set(common::VCPU_METER, days(30));

    let target = aged(common::VCPU_METER, Uuid::from_u128(0x5E05), "target", 100);
    let withdrawal = common::withdrawal_of(&target);
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
    let store = common::record_store(&h);
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
    let store = common::record_store(&h);
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
    let store = common::record_store(&h);
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
    let store = common::record_store(&h);
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
    let store = common::record_store(&h);
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
    let store = common::record_store(&h);
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
    let store = common::record_store(&h);
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
    let store = common::record_store(&h);
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

/// The mark a sweep leaves for the type it dropped, or `None`.
async fn mark_of(pool: &sqlx::PgPool, gts_type_id: &str) -> Option<(u64, Uuid)> {
    let row: Option<(String, Uuid)> = sqlx::query_as(
        "SELECT xact_id::text, id FROM usage_feed_retention_marks WHERE gts_type_id = $1",
    )
    .bind(gts_type_id)
    .fetch_optional(pool)
    .await
    .expect("read the mark");
    row.map(|(xact_id, id)| (xact_id.parse().expect("xid8 digits"), id))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_drop_raises_the_marks_of_every_type_in_the_chunk() {
    let h = common::bring_up()
        .await
        .expect("timescaledb container (Docker required)");
    let store = common::record_store(&h);
    let tenant = Uuid::from_u128(0x5E11);
    let stub = Arc::new(StubRetention::default());
    stub.set(common::VCPU_METER, days(30));

    let entry = aged(common::VCPU_METER, tenant, "interlock-1", 400);
    let stored_entry = store.create(entry).await.expect("the entry stores");
    let position = common::xact_id_of(&h.pool, stored_entry.id).await;

    let report = sweeper(&h.pool, &stub)
        .sweep_once()
        .await
        .expect("the sweep runs");
    assert_eq!(report.dropped, 1, "one expired chunk: {report:?}");
    assert!(!stored(&h.pool, stored_entry.id).await, "the entry is gone");

    assert_eq!(
        mark_of(&h.pool, common::VCPU_METER).await,
        Some((position, stored_entry.id)),
        "the mark names the highest position the chunk held, which is this \
         entry's: the sweep reads it under the chunk's ACCESS EXCLUSIVE lock \
         and commits the mark in the transaction that drops the rows \
         (DESIGN section 3.6 cpt-cf-uc-plugin-seq-retention-sweep)"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_later_sweep_of_an_older_chunk_does_not_lower_the_mark() {
    // Chunks are swept in catalog order, not in time order, so this is the
    // shape a real deployment reaches rather than a contrived one: the mark
    // must hold at the highest position retention has ever deleted for the
    // type. A lowered mark makes a feed page serve a range retention has
    // truncated instead of refusing it, and nothing else in the suite can
    // see that.
    //
    // A chunk's relid is assigned at creation, and creation happens on its
    // first write, so with only one write per chunk whichever chunk is
    // written first gets both the lower relid (visited first by a
    // catalog-ordered sweep) and the lower position — the two orderings are
    // structurally coupled, and a two-chunk, one-write-each fixture can
    // never present a lower position to a later-visited chunk. Breaking that
    // coupling needs a *third* write, back into the first chunk, after the
    // second chunk already exists: the first chunk stays visited first, but
    // now holds the highest position of the three, so the second chunk's own
    // read — visited second — presents a position lower than what the first
    // chunk already raised.
    let h = common::bring_up()
        .await
        .expect("timescaledb container (Docker required)");
    let store = common::record_store(&h);
    let tenant = Uuid::from_u128(0x5E12);
    let stub = Arc::new(StubRetention::default());

    let first = store
        .create(aged(common::VCPU_METER, tenant, "interlock-first", 400))
        .await
        .expect("the first entry stores");
    let second = store
        .create(aged(common::VCPU_METER, tenant, "interlock-second", 800))
        .await
        .expect("the second entry stores");
    // Back into the first chunk (same covered-period age as `first`), so the
    // chunk visited first by the sweep ends up holding the highest position
    // of the three.
    let third = store
        .create(aged(common::VCPU_METER, tenant, "interlock-third", 400))
        .await
        .expect("the third entry stores");

    let first_pos = common::xact_id_of(&h.pool, first.id).await;
    let second_pos = common::xact_id_of(&h.pool, second.id).await;
    let third_pos = common::xact_id_of(&h.pool, third.id).await;
    assert!(
        first_pos < second_pos && second_pos < third_pos,
        "the fixture needs strictly increasing positions in write order: \
         first={first_pos}, second={second_pos}, third={third_pos}"
    );

    stub.set(common::VCPU_METER, days(30));
    let report = sweeper(&h.pool, &stub)
        .sweep_once()
        .await
        .expect("the sweep runs");
    assert_eq!(report.dropped, 2, "both chunks expired: {report:?}");

    assert_eq!(
        mark_of(&h.pool, common::VCPU_METER).await,
        Some((third_pos, third.id)),
        "the mark holds at the greatest position any chunk held, the third \
         write's, back in the first-visited chunk, not the lower position \
         the second-visited chunk presents afterward"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_held_chunk_lock_makes_the_drop_time_out_and_keep_the_chunk() {
    // The `lock_timeout` is what bounds the wait; without it this sweep
    // blocks for as long as the competing transaction runs, holding every
    // feed page queued behind its lock request. The competing lock is taken
    // in a separate transaction that is never committed inside the timeout.
    let h = common::bring_up()
        .await
        .expect("timescaledb container (Docker required)");
    let store = common::record_store(&h);
    let tenant = Uuid::from_u128(0x5E13);
    let stub = Arc::new(StubRetention::default());
    stub.set(common::VCPU_METER, days(30));

    let entry = store
        .create(aged(common::VCPU_METER, tenant, "interlock-locked", 400))
        .await
        .expect("the entry stores");

    let chunk =
        timescaledb_usage_collector_plugin::infra::storage::retention_sweep::list_chunks(&h.pool)
            .await
            .expect("list chunks")
            .into_iter()
            .next()
            .expect("one chunk");

    let mut blocker = h.pool.acquire().await.expect("a blocking connection");
    sqlx::query("BEGIN")
        .execute(&mut *blocker)
        .await
        .expect("begin");
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "LOCK TABLE {} IN ACCESS EXCLUSIVE MODE",
        chunk.chunk
    )))
    .execute(&mut *blocker)
    .await
    .expect("the blocker takes the chunk lock");

    let report = sweeper(&h.pool, &stub)
        .sweep_once()
        .await
        .expect("the sweep completes even though the drop could not");
    assert_eq!(report.dropped, 0, "nothing dropped: {report:?}");
    assert_eq!(report.drop_failures, 1, "one counted failure: {report:?}");

    // The blocker's ACCESS EXCLUSIVE lock is released here, before the checks
    // below, rather than after them as the brief's draft had it. `id` is not
    // the hypertable's partitioning key, so `stored`'s query can't prune the
    // still-locked chunk at plan time; it would need an `AccessShareLock` on
    // it and block behind the very lock this test holds, timing out on the
    // pool's own session-level `lock_timeout` (`pool.rs`) instead of
    // exercising the assertion. `mark_of` queries `usage_feed_retention_marks`,
    // an ordinary non-hypertable table unrelated to the dropped chunk, so it
    // was never at risk. The report above is already captured, so releasing
    // the lock first changes nothing the test proves.
    sqlx::query("ROLLBACK")
        .execute(&mut *blocker)
        .await
        .expect("rollback");

    assert!(
        stored(&h.pool, entry.id).await,
        "the chunk is kept for the next sweep"
    );
    assert_eq!(
        mark_of(&h.pool, common::VCPU_METER).await,
        None,
        "a rolled-back drop leaves no mark: the mark and the drop commit \
         together or not at all, which is what lets a page treat a mark as \
         naming entries that are already gone"
    );
}

/// A chunk whose two maxima cross: the entry with the greatest `xact_id` does
/// not carry the greatest `id`, and vice versa. `id` is derived client-side
/// before either write reaches the database, so which of two candidate
/// entries carries the greater `id` is known before either is stored; writing
/// the greater-`id` candidate first forces the second write — which always
/// takes the greater `xact_id` — to carry the *lesser* `id`, guaranteeing a
/// cross without any retry or search.
///
/// This is what distinguishes `CHUNK_HIGHEST_POSITIONS_SQL`'s `DISTINCT ON`
/// from a grouped `max(xact_id)` beside `max(id)`: the two independent maxima
/// would name `(second_pos, first_id)`, a pair no entry in the chunk actually
/// carries, where `DISTINCT ON`'s `ORDER BY` picks the one row that is
/// greatest as a *pair* — the second write's own `(xact_id, id)`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_crossing_of_the_chunks_two_maxima_still_names_a_real_entry() {
    let h = common::bring_up()
        .await
        .expect("timescaledb container (Docker required)");
    let store = common::record_store(&h);
    let tenant = Uuid::from_u128(0x5E14);
    let stub = Arc::new(StubRetention::default());
    stub.set(common::VCPU_METER, days(30));

    // Both candidates share one window, so both land in the same chunk.
    let end = OffsetDateTime::now_utc() - Duration::days(400);
    let candidate_a = common::entry_over(
        &common::meter(common::VCPU_METER),
        tenant,
        "cross-a",
        Decimal::ONE,
        end - Duration::hours(1),
        end,
    );
    let candidate_b = common::entry_over(
        &common::meter(common::VCPU_METER),
        tenant,
        "cross-b",
        Decimal::ONE,
        end - Duration::hours(1),
        end,
    );
    // Whichever candidate carries the greater `id` is written first, so the
    // second write — which always takes the greater `xact_id` — is forced to
    // carry the lesser `id`.
    let (first, second) = if candidate_a.id > candidate_b.id {
        (candidate_a, candidate_b)
    } else {
        (candidate_b, candidate_a)
    };
    let (first_id, second_id) = (first.id, second.id);
    store
        .create(first)
        .await
        .expect("the first candidate stores");
    store
        .create(second)
        .await
        .expect("the second candidate stores");
    let first_pos = common::xact_id_of(&h.pool, first_id).await;
    let second_pos = common::xact_id_of(&h.pool, second_id).await;
    assert!(
        second_pos > first_pos,
        "the second write must carry the greater xact_id: first={first_pos}, \
         second={second_pos}"
    );
    assert!(
        second_id < first_id,
        "the construction must give the second write the lesser id, so the \
         two maxima cross: first={first_id}, second={second_id}"
    );

    let report = sweeper(&h.pool, &stub)
        .sweep_once()
        .await
        .expect("the sweep runs");
    assert_eq!(report.dropped, 1, "one expired chunk: {report:?}");

    assert_eq!(
        mark_of(&h.pool, common::VCPU_METER).await,
        Some((second_pos, second_id)),
        "the mark must name the greatest (xact_id, id) PAIR an entry actually \
         carried, the second write's, not the pointwise maxima of xact_id \
         and id taken separately, which would name (second_pos, first_id), a \
         position no entry in the chunk carries"
    );
}

/// `CHUNK_HIGHEST_POSITIONS_SQL` must order `xact_id` numerically, not as
/// rendered digit text. `PostgreSQL` resolves a bare `ORDER BY` name that
/// matches both an output column and an input column to the *output*
/// column, so aliasing the `::text` cast back to `xact_id` — its own source
/// column's name — would make the `ORDER BY` bind to the text column and
/// sort lexicographically, ranking `"9"` above `"10"`.
///
/// Two back-to-back inserts through the ingest path cannot reach this:
/// consecutive `xact_id`s share a decimal digit count, so no ordinary
/// fixture straddles the boundary. This test pins explicit `xid8` values
/// directly, in the test's own SQL, to force the straddle. Slice 2's rule
/// that the Record Store never binds `xact_id` binds production code, not a
/// test pinning ordering semantics.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_position_read_orders_xact_id_numerically_not_lexicographically() {
    let h = common::bring_up()
        .await
        .expect("timescaledb container (Docker required)");
    let store = common::record_store(&h);
    let tenant = Uuid::from_u128(0x5EA1);

    let low = store
        .create(aged(common::VCPU_METER, tenant, "digit-low", 400))
        .await
        .expect("the low entry stores");
    let high = store
        .create(aged(common::VCPU_METER, tenant, "digit-high", 400))
        .await
        .expect("the high entry stores");

    // Pin xid8 values that straddle a digit-length boundary: under a
    // lexicographic (text) comparison "9" sorts above "10".
    sqlx::query("UPDATE usage_records SET xact_id = '9'::xid8 WHERE id = $1")
        .bind(low.id)
        .execute(&h.pool)
        .await
        .expect("pin the low entry's xact_id");
    sqlx::query("UPDATE usage_records SET xact_id = '10'::xid8 WHERE id = $1")
        .bind(high.id)
        .execute(&h.pool)
        .await
        .expect("pin the high entry's xact_id");

    let chunk = list_chunks(&h.pool)
        .await
        .expect("list chunks")
        .into_iter()
        .next()
        .expect("one chunk");
    let sql = CHUNK_HIGHEST_POSITIONS_SQL.replace("{chunk}", &chunk.chunk);
    let positions: Vec<(String, String, Uuid)> = sqlx::query_as(sqlx::AssertSqlSafe(sql))
        .fetch_all(&h.pool)
        .await
        .expect("read positions");

    assert_eq!(
        positions,
        vec![(common::VCPU_METER.to_owned(), "10".to_owned(), high.id)],
        "the numerically greater xact_id (10) must win over the \
         lexicographically greater digit string (\"9\"); a bug here makes \
         the sweep raise a mark below the chunk's true highest deleted \
         position: {positions:?}"
    );
}
