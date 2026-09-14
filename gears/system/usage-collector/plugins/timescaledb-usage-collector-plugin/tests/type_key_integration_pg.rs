#![cfg(feature = "postgres")]
#![allow(clippy::expect_used, clippy::unwrap_used)]
//! The per-type partitioning key, assigned on first write. Requires Docker.

mod common;

use rust_decimal::Decimal;
use uuid::Uuid;

use timescaledb_usage_collector_plugin::domain::ports::RecordStore;

/// Eight stores, each with a cold key cache, write the first entry of one type
/// at once. Every one takes the database path, so a race in the assignment
/// would show up as two keys for one type.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_first_writes_of_one_type_share_one_key() {
    let h = common::bring_up()
        .await
        .expect("timescaledb container (Docker required)");
    let meter = common::meter(common::VCPU_METER);

    let mut tasks = Vec::new();
    for i in 0..8u128 {
        let store = common::record_store(&h.pool);
        let meter = meter.clone();
        tasks.push(tokio::spawn(async move {
            store
                .create(common::entry(
                    &meter,
                    Uuid::from_u128(0x7E00 + i),
                    "first",
                    Decimal::ONE,
                ))
                .await
        }));
    }
    for task in tasks {
        task.await
            .expect("task did not panic")
            .expect("a concurrent first write must succeed");
    }

    let keys: Vec<i32> = sqlx::query_scalar("SELECT DISTINCT type_key FROM usage_records")
        .fetch_all(&h.pool)
        .await
        .expect("read ledger keys");
    assert_eq!(keys.len(), 1, "one type must carry one key: {keys:?}");

    let mapped: i64 = sqlx::query_scalar("SELECT count(*) FROM usage_type_key")
        .fetch_one(&h.pool)
        .await
        .expect("count mapped types");
    assert_eq!(mapped, 1, "one type must be mapped exactly once");
}

/// Two types written through each insert path get two keys, and every ledger
/// row carries the key its own type maps to.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn each_row_carries_its_own_types_key_on_both_insert_paths() {
    let h = common::bring_up()
        .await
        .expect("timescaledb container (Docker required)");
    let store = common::record_store(&h.pool);
    let tenant = Uuid::from_u128(0x7E10);
    let vcpu = common::meter(common::VCPU_METER);
    let gb = common::meter(common::GB_METER);

    store
        .create(common::entry(&vcpu, tenant, "single-vcpu", Decimal::ONE))
        .await
        .expect("single insert");
    let outcomes = store
        .create_batch(vec![
            common::entry(&vcpu, tenant, "batch-vcpu", Decimal::ONE),
            common::entry(&gb, tenant, "batch-gb", Decimal::ONE),
        ])
        .await
        .expect("batch insert");
    assert!(outcomes.iter().all(Result::is_ok), "{outcomes:?}");

    let rows: Vec<(String, i32, i32)> = sqlx::query_as(
        "SELECT r.gts_type_id, r.type_key, k.type_key \
         FROM usage_records r JOIN usage_type_key k USING (gts_type_id) \
         ORDER BY r.idempotency_key",
    )
    .fetch_all(&h.pool)
    .await
    .expect("read rows with their mapped keys");

    assert_eq!(rows.len(), 3);
    for (gts_type_id, row_key, mapped_key) in &rows {
        assert_eq!(
            row_key, mapped_key,
            "{gts_type_id} must carry its mapped key"
        );
    }
    let vcpu_key = rows.iter().find(|r| r.0 == common::VCPU_METER).unwrap().1;
    let gb_key = rows.iter().find(|r| r.0 == common::GB_METER).unwrap().1;
    assert_ne!(vcpu_key, gb_key, "distinct types must get distinct keys");
}
