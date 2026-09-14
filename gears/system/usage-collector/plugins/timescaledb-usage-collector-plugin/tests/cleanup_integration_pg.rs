#![cfg(feature = "postgres")]
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::type_complexity)]
//! `TimescaleDB`-backed tests for the post-migration partitioning setup:
//! concurrent-replica serialization and the pooled-connection hygiene of its
//! advisory lock. Requires Docker. The per-type retention sweep has its own
//! suite (`retention_sweep_integration_pg`).

mod common;

use timescaledb_usage_collector_plugin::infra::storage::pool::apply_post_migration_setup;

/// Concurrently-initializing replicas must not corrupt the post-migration
/// setup. The advisory lock serializes them, so every call succeeds, no
/// table-wide retention policy is left behind, and both configured intervals
/// are what the dimensions report.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pg_concurrent_post_migration_setup_is_serialized() {
    let h = common::bring_up()
        .await
        .expect("timescaledb container (Docker required)");

    let mut tasks = Vec::new();
    for _ in 0..8u32 {
        let pool = h.pool.clone();
        tasks.push(tokio::spawn(async move {
            apply_post_migration_setup(&pool, 604_800, 1).await
        }));
    }
    for t in tasks {
        t.await
            .expect("setup task did not panic")
            .expect("concurrent post-migration setup must succeed under the advisory lock");
    }

    let policies: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM timescaledb_information.jobs \
         WHERE proc_name = 'policy_retention' AND hypertable_name = 'usage_records'",
    )
    .fetch_one(&h.pool)
    .await
    .expect("retention policy count");
    assert_eq!(
        policies, 0,
        "no table-wide retention policy may remain: retention is per type"
    );

    let dims: Vec<(String, Option<String>, Option<i64>)> = sqlx::query_as(
        "SELECT column_name::text, time_interval::text, integer_interval \
         FROM timescaledb_information.dimensions \
         WHERE hypertable_name = 'usage_records' ORDER BY dimension_number",
    )
    .fetch_all(&h.pool)
    .await
    .expect("dimension intervals");
    assert_eq!(
        dims,
        vec![
            ("window_end".to_owned(), Some("7 days".to_owned()), None),
            ("type_key".to_owned(), None, Some(1)),
        ],
        "the configured chunk interval and slice width must be applied"
    );
}

/// The init advisory lock must not leave a modified `statement_timeout` on any
/// pooled connection. `apply_post_migration_setup` acquires the lock on a pooled
/// connection; the wait is bounded by the connection-level GUC set in
/// `build_pool`, NOT by a per-lock session-level `SET` — a session-level set
/// would leak onto the connection and silently apply to whatever request later
/// reused it. With a distinct configured timeout (17s) and a fixed 2-connection
/// pool (the lock uses one connection, the setup statements a second), every
/// pooled connection must still report the configured value.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pg_init_lock_does_not_leak_statement_timeout() {
    let h = common::bring_up_with(17, 2, 2)
        .await
        .expect("timescaledb container (Docker required)");

    let mut c1 = h.pool.acquire().await.expect("acquire conn 1");
    let mut c2 = h.pool.acquire().await.expect("acquire conn 2");
    let t1: String = sqlx::query_scalar("SHOW statement_timeout")
        .fetch_one(&mut *c1)
        .await
        .expect("SHOW statement_timeout on conn 1");
    let t2: String = sqlx::query_scalar("SHOW statement_timeout")
        .fetch_one(&mut *c2)
        .await
        .expect("SHOW statement_timeout on conn 2");

    assert_eq!(
        t1, "17s",
        "conn 1 carries a leaked statement_timeout; the init path must not set a \
         session-level statement_timeout on a pooled connection"
    );
    assert_eq!(
        t2, "17s",
        "conn 2 carries a leaked statement_timeout; the init path must not set a \
         session-level statement_timeout on a pooled connection"
    );
}
