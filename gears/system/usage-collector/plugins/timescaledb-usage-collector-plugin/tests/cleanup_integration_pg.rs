#![cfg(feature = "postgres")]
#![allow(clippy::expect_used, clippy::unwrap_used)]
//! TimescaleDB-backed tests for the dedup cleanup job and update-on-restart
//! retention registration. Requires Docker.

mod common;

use time::{Duration, OffsetDateTime};
use uuid::Uuid;

use timescaledb_usage_collector_plugin::domain::ports::{CatalogStore, RecordStore};
use timescaledb_usage_collector_plugin::infra::storage::pool::{
    apply_dedup_cleanup_job, apply_post_migration_setup,
};

const VCPU_GTS: &str = "gts.cf.core.uc.usage_record.v1~cf.compute._.vcpu_hours.v1";

/// The harness registers exactly one `prune_usage_dedup` job.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pg_dedup_cleanup_job_registered() {
    let h = common::bring_up()
        .await
        .expect("timescaledb container (Docker required)");

    let n: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM timescaledb_information.jobs WHERE proc_name = 'prune_usage_dedup'",
    )
    .fetch_one(&h.pool)
    .await
    .expect("jobs query");
    assert_eq!(n, 1, "exactly one dedup cleanup job must be registered");
}

/// Re-registering with a changed retention updates the existing job's config in
/// place (no duplicate job) — i.e. retention is live on restart.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pg_dedup_cleanup_job_updates_retention_on_reapply() {
    let h = common::bring_up()
        .await
        .expect("timescaledb container (Docker required)");

    apply_dedup_cleanup_job(&h.pool, 100)
        .await
        .expect("first re-apply");
    apply_dedup_cleanup_job(&h.pool, 200)
        .await
        .expect("second re-apply with changed retention");

    let cfg: serde_json::Value = sqlx::query_scalar(
        "SELECT config FROM timescaledb_information.jobs WHERE proc_name = 'prune_usage_dedup'",
    )
    .fetch_one(&h.pool)
    .await
    .expect("config query");
    assert_eq!(
        cfg["retention_secs"],
        serde_json::json!(200),
        "changed retention must be applied on re-apply"
    );

    let n: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM timescaledb_information.jobs WHERE proc_name = 'prune_usage_dedup'",
    )
    .fetch_one(&h.pool)
    .await
    .expect("jobs count");
    assert_eq!(n, 1, "re-apply must not create a duplicate job");
}

/// Concurrently-initializing replicas must not corrupt the post-migration
/// setup. The advisory lock in `apply_post_migration_setup` serializes them so
/// every call succeeds and exactly one retention policy + one dedup cleanup job
/// remain. Without the lock, concurrent `add_retention_policy` calls (which have
/// no `if_not_exists`) error with "policy already exists".
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pg_concurrent_post_migration_setup_is_serialized() {
    let h = common::bring_up()
        .await
        .expect("timescaledb container (Docker required)");

    // bring_up already applied the setup once; now hammer it concurrently.
    let mut tasks = Vec::new();
    for _ in 0..8u32 {
        let pool = h.pool.clone();
        tasks.push(tokio::spawn(async move {
            apply_post_migration_setup(&pool, 31_536_000).await
        }));
    }
    for t in tasks {
        t.await
            .expect("setup task did not panic")
            .expect("concurrent post-migration setup must succeed under the advisory lock");
    }

    let retention_jobs: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM timescaledb_information.jobs \
         WHERE proc_name = 'policy_retention' AND hypertable_name = 'usage_records'",
    )
    .fetch_one(&h.pool)
    .await
    .expect("retention jobs count");
    assert_eq!(
        retention_jobs, 1,
        "exactly one retention policy must remain after concurrent setup"
    );

    let prune_jobs: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM timescaledb_information.jobs WHERE proc_name = 'prune_usage_dedup'",
    )
    .fetch_one(&h.pool)
    .await
    .expect("prune jobs count");
    assert_eq!(
        prune_jobs, 1,
        "exactly one dedup cleanup job must remain after concurrent setup"
    );
}

/// `prune_usage_dedup` deletes an orphaned dedup row (record gone) older than
/// retention, keeps a live one (record present), and keeps a recent orphan.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pg_prune_removes_only_aged_orphans() {
    async fn key_exists(pool: &sqlx::PgPool, tenant: Uuid, idem: &str) -> bool {
        sqlx::query_scalar::<_, bool>(
            "SELECT EXISTS (SELECT 1 FROM usage_dedup \
             WHERE tenant_id = $1 AND gts_id = $2 AND idempotency_key = $3)",
        )
        .bind(tenant)
        .bind(VCPU_GTS)
        .bind(idem)
        .fetch_one(pool)
        .await
        .expect("existence query")
    }

    let h = common::bring_up()
        .await
        .expect("timescaledb container (Docker required)");
    // Satisfy the usage_records.gts_id FK.
    let catalog = common::catalog_store(&h.pool);
    catalog
        .create(common::fixture_usage_type(VCPU_GTS, "counter", &[]))
        .await
        .expect("register usage type");
    let store = common::record_store(&h.pool);
    let tenant = Uuid::from_u128(0x9C1E);

    // Live: a real record (created_at = fixture's 2023 instant) + its dedup row.
    let live = common::fixture_usage_record(
        VCPU_GTS,
        tenant,
        "idem-live",
        rust_decimal::Decimal::ONE,
        0xC01,
    );
    store.create(live).await.expect("create live record");

    // Aged orphan: dedup row pointing at a non-existent record, old timestamp.
    sqlx::query(
        "INSERT INTO usage_dedup \
         (tenant_id, gts_id, idempotency_key, record_uuid, record_created_at) \
         VALUES ($1, $2, 'idem-orphan', $3, TIMESTAMPTZ '2023-01-01 00:00:00+00')",
    )
    .bind(tenant)
    .bind(VCPU_GTS)
    .bind(Uuid::from_u128(0x00DE_AD01))
    .execute(&h.pool)
    .await
    .expect("insert aged orphan");

    // Recent orphan: same, but record_created_at = now() (inside retention).
    sqlx::query(
        "INSERT INTO usage_dedup \
         (tenant_id, gts_id, idempotency_key, record_uuid, record_created_at) \
         VALUES ($1, $2, 'idem-recent', $3, now())",
    )
    .bind(tenant)
    .bind(VCPU_GTS)
    .bind(Uuid::from_u128(0x00DE_AD02))
    .execute(&h.pool)
    .await
    .expect("insert recent orphan");

    // Run the prune with a 1-day retention: 2023 rows are candidates, now() is not.
    sqlx::query("CALL prune_usage_dedup(0, $1::jsonb)")
        .bind(serde_json::json!({ "retention_secs": 86400 }))
        .execute(&h.pool)
        .await
        .expect("call prune procedure");

    assert!(
        key_exists(&h.pool, tenant, "idem-live").await,
        "live dedup row (record present) kept"
    );
    assert!(
        !key_exists(&h.pool, tenant, "idem-orphan").await,
        "aged orphan (record gone) deleted"
    );
    assert!(
        key_exists(&h.pool, tenant, "idem-recent").await,
        "recent orphan (inside retention) kept"
    );
}

/// End-to-end retention through the REAL registered policy (not a manual
/// `drop_chunks`): backdated data is dropped when the policy job is run, while
/// fresh data survives — proving `apply_retention_policy` wired the right
/// hypertable + window and that the outbound `gts_id` FK does not block it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pg_registered_retention_policy_drops_aged_data() {
    let h = common::bring_up()
        .await
        .expect("timescaledb container (Docker required)");
    let catalog = common::catalog_store(&h.pool);
    catalog
        .create(common::fixture_usage_type(VCPU_GTS, "counter", &[]))
        .await
        .expect("register usage type (satisfies the gts_id FK)");
    let store = common::record_store(&h.pool);
    let tenant = Uuid::from_u128(0xA6ED);

    // (1) outdated data, 400 days old (> 365d window), via the real ingest path.
    let mut aged =
        common::fixture_usage_record(VCPU_GTS, tenant, "aged", rust_decimal::Decimal::ONE, 0xA01);
    aged.created_at = OffsetDateTime::now_utc() - Duration::days(400);
    let aged_uuid = aged.uuid;
    store.create(aged).await.expect("create aged record");

    // fresh row (now) in a different chunk — must survive.
    let mut fresh =
        common::fixture_usage_record(VCPU_GTS, tenant, "fresh", rust_decimal::Decimal::ONE, 0xA02);
    fresh.created_at = OffsetDateTime::now_utc();
    let fresh_uuid = fresh.uuid;
    store.create(fresh).await.expect("create fresh record");

    // (2) verify it exists.
    let before: i64 = sqlx::query_scalar("SELECT count(*) FROM usage_records WHERE uuid = $1")
        .bind(aged_uuid)
        .fetch_one(&h.pool)
        .await
        .expect("count before");
    assert_eq!(before, 1, "aged record must exist before retention runs");

    // (3) trigger the REAL retention policy now.
    let job_id: i32 = sqlx::query_scalar(
        "SELECT job_id FROM timescaledb_information.jobs \
         WHERE proc_name = 'policy_retention' AND hypertable_name = 'usage_records'",
    )
    .fetch_one(&h.pool)
    .await
    .expect("the retention policy must be registered against usage_records");
    sqlx::query("CALL run_job($1)")
        .bind(job_id)
        .execute(&h.pool)
        .await
        .expect("running the retention policy must not error (FK must not block it)");

    // (4) verify it is gone, and the fresh row survived.
    let after_aged: i64 = sqlx::query_scalar("SELECT count(*) FROM usage_records WHERE uuid = $1")
        .bind(aged_uuid)
        .fetch_one(&h.pool)
        .await
        .expect("count aged after");
    let after_fresh: i64 = sqlx::query_scalar("SELECT count(*) FROM usage_records WHERE uuid = $1")
        .bind(fresh_uuid)
        .fetch_one(&h.pool)
        .await
        .expect("count fresh after");
    assert_eq!(
        after_aged, 0,
        "aged record dropped by the registered retention policy"
    );
    assert_eq!(
        after_fresh, 1,
        "fresh record (inside window) survives retention"
    );
}
