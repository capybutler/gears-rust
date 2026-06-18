#![cfg(feature = "postgres")]
mod common;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pg_migrations_create_hypertable_and_retention() {
    let h = common::bring_up()
        .await
        .expect("timescaledb container (Docker required)");

    let ht: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM timescaledb_information.hypertables WHERE hypertable_name = 'usage_records'",
    )
    .fetch_one(&h.pool)
    .await
    .expect("hypertable query");
    assert_eq!(ht, 1, "usage_records must be a hypertable");

    let jobs: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM timescaledb_information.jobs \
         WHERE proc_name = 'policy_retention' AND hypertable_name = 'usage_records'",
    )
    .fetch_one(&h.pool)
    .await
    .expect("jobs query");
    assert!(jobs >= 1, "retention policy must be registered");

    sqlx::query("SELECT gts_id, kind, metadata_fields FROM usage_type_catalog LIMIT 0")
        .fetch_all(&h.pool)
        .await
        .expect("usage_type_catalog must exist");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pg_migration_creates_usage_dedup_and_prune_procedure() {
    let h = common::bring_up()
        .await
        .expect("timescaledb container (Docker required)");

    let table: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM information_schema.tables \
         WHERE table_name = 'usage_dedup')",
    )
    .fetch_one(&h.pool)
    .await
    .expect("usage_dedup existence query");
    assert!(table, "usage_dedup table must be created by migration 0002");

    let proc: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM pg_proc WHERE proname = 'prune_usage_dedup')",
    )
    .fetch_one(&h.pool)
    .await
    .expect("prune_usage_dedup existence query");
    assert!(
        proc,
        "prune_usage_dedup procedure must be created by migration 0002"
    );
}
