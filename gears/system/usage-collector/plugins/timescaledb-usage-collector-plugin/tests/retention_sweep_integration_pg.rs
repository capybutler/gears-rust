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
    PgRetentionSweeper, SWEEP_ADVISORY_LOCK_KEY,
};

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
