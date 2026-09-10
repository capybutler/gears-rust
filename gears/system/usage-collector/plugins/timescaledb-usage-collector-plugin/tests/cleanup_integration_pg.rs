#![cfg(feature = "postgres")]
#![allow(clippy::expect_used, clippy::unwrap_used)]
//! `TimescaleDB`-backed tests for the `usage_records` retention policy:
//! registration (idempotent re-apply, concurrent-replica serialization) and
//! end-to-end chunk expiry. Requires Docker.
//!
//! **The horizon is measured from `window_end`** — the covered period's end,
//! which is the hypertable's partition column — and not from when the entry was
//! ingested. That is what
//! `cpt-cf-usage-collector-fr-idempotency` requires: the retention horizon has
//! to be a property of the period an entry covers, because that is what a
//! consumer reads it back over. Measuring from arrival instead would keep a
//! decade-old period alive because it was backfilled this morning, and drop a
//! current period because its late-arriving correction was not.

mod common;

use rust_decimal::Decimal;
use time::{Duration, OffsetDateTime};
use uuid::Uuid;

use timescaledb_usage_collector_plugin::domain::ports::RecordStore;
use timescaledb_usage_collector_plugin::infra::storage::pool::apply_post_migration_setup;

/// Concurrently-initializing replicas must not corrupt the post-migration
/// setup. The advisory lock in `apply_post_migration_setup` serializes them so
/// every call succeeds and exactly one retention policy remains. Without the
/// lock, concurrent `add_retention_policy` calls (which have no `if_not_exists`)
/// error with "policy already exists".
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
            // The window is irrelevant here (this test asserts serialization, and
            // inserts no rows) — but each re-apply registers a *live* policy, so
            // use the no-drop window rather than the production one to leave the
            // container in a state where a later-added row cannot vanish.
            apply_post_migration_setup(&pool, common::NO_DROP_RETENTION_SECS).await
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
}

/// The init advisory lock must not leave a modified `statement_timeout` on any
/// pooled connection. `apply_post_migration_setup` acquires the lock on a pooled
/// connection; the wait is bounded by the connection-level GUC set in
/// `build_pool`, NOT by a per-lock session-level `SET` — a session-level set
/// would leak onto the connection and silently apply to whatever request later
/// reused it. With a distinct configured timeout (17s) and a fixed 2-connection
/// pool (the lock uses one connection, the retention statements a second), every
/// pooled connection must still report the configured value.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pg_init_lock_does_not_leak_statement_timeout() {
    let h = common::bring_up_with(17, 2, 2, common::NO_DROP_RETENTION_SECS)
        .await
        .expect("timescaledb container (Docker required)");

    // Hold both pooled connections at once so each distinct one is inspected.
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

/// End-to-end retention through the REAL registered policy (not a manual
/// `drop_chunks`): an entry whose **covered period** ended before the horizon is
/// dropped when the policy job runs, while one whose period ends inside the
/// window survives.
///
/// The two fixtures differ only in their covered period. Both are written now,
/// through the real ingest path, so `ingested_at` is the same for each and
/// cannot be what decides which one goes: if the policy measured from arrival,
/// either both would survive or both would be dropped, and this test would fail
/// whichever way it went.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_registered_policy_measures_the_horizon_from_the_covered_period() {
    // The one harness that needs the production 365-day window: the subject here
    // IS retention. `bring_up_real_retention` also unschedules the job so the
    // explicit `CALL run_job` below is the only thing that can drop a chunk.
    let h = common::bring_up_real_retention()
        .await
        .expect("timescaledb container (Docker required)");
    let store = common::record_store(&h.pool);
    let meter = common::meter(common::VCPU_METER);
    let tenant = Uuid::from_u128(0xA6ED);

    // (1) A period that ended 400 days ago — past the 365-day horizon.
    let aged_end = OffsetDateTime::now_utc() - Duration::days(400);
    let aged = common::entry_over(
        &meter,
        tenant,
        "aged",
        Decimal::ONE,
        aged_end - Duration::hours(1),
        aged_end,
    );
    let aged_id = aged.id;
    store.create(aged).await.expect("create the aged entry");

    // A period ending now, in a different chunk — must survive.
    let fresh_end = OffsetDateTime::now_utc();
    let fresh = common::entry_over(
        &meter,
        tenant,
        "fresh",
        Decimal::ONE,
        fresh_end - Duration::hours(1),
        fresh_end,
    );
    let fresh_id = fresh.id;
    store.create(fresh).await.expect("create the fresh entry");

    // (2) Both are there, and both arrived at the same time — so arrival cannot
    // be what separates them below.
    let ingested: Vec<OffsetDateTime> = sqlx::query_scalar(
        "SELECT ingested_at FROM usage_records WHERE tenant_id = $1 ORDER BY window_end",
    )
    .bind(tenant)
    .fetch_all(&h.pool)
    .await
    .expect("read ingested_at");
    assert_eq!(
        ingested.len(),
        2,
        "both entries exist before retention runs"
    );
    // Five minutes, not one. This is the suite's only wall-clock assertion, and
    // its job is to rule out arrival as the discriminator against a 400-day
    // difference in covered period - so the window can be five orders of
    // magnitude looser than the effect it excludes and still exclude it, while
    // a stalled box no longer reds it for a non-reason.
    assert!(
        (ingested[1] - ingested[0]).abs() < Duration::minutes(5),
        "both entries were ingested at effectively the same moment ({ingested:?}), so a \
         policy measuring from arrival could not drop one and keep the other"
    );

    // (3) Trigger the REAL retention policy now.
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
        .expect("running the retention policy must not error");

    // (4) The aged period is gone; the fresh one survived.
    let after_aged: i64 = sqlx::query_scalar("SELECT count(*) FROM usage_records WHERE id = $1")
        .bind(aged_id)
        .fetch_one(&h.pool)
        .await
        .expect("count aged after");
    let after_fresh: i64 = sqlx::query_scalar("SELECT count(*) FROM usage_records WHERE id = $1")
        .bind(fresh_id)
        .fetch_one(&h.pool)
        .await
        .expect("count fresh after");
    assert_eq!(
        after_aged, 0,
        "an entry whose covered period ended past the horizon is dropped by the registered \
         policy"
    );
    assert_eq!(
        after_fresh, 1,
        "an entry whose covered period ends inside the window survives"
    );
}

/// The complement, and the half that pins *which* bound the horizon is measured
/// from: an entry whose period **started** before the horizon but **ended**
/// inside the window survives.
///
/// A policy measuring from `window_start` would drop this one. Nothing else in
/// this suite tells the two bounds apart, because every other fixture has both
/// bounds on the same side of the horizon.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_period_that_began_before_the_horizon_but_ends_inside_it_survives() {
    let h = common::bring_up_real_retention()
        .await
        .expect("timescaledb container (Docker required)");
    let store = common::record_store(&h.pool);
    let meter = common::meter(common::VCPU_METER);
    let tenant = Uuid::from_u128(0xA6EE);

    // A period spanning the horizon: it began 400 days ago and ends today.
    let now = OffsetDateTime::now_utc();
    let straddling = common::entry_over(
        &meter,
        tenant,
        "straddling",
        Decimal::ONE,
        now - Duration::days(400),
        now,
    );
    let straddling_id = straddling.id;
    store
        .create(straddling)
        .await
        .expect("create the straddling entry");

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
        .expect("running the retention policy must not error");

    let survived: i64 = sqlx::query_scalar("SELECT count(*) FROM usage_records WHERE id = $1")
        .bind(straddling_id)
        .fetch_one(&h.pool)
        .await
        .expect("count after");
    assert_eq!(
        survived, 1,
        "the horizon is measured from window_end: a period that began 400 days ago but \
         ends today is inside the window"
    );
}

/// Guard on the harness itself, and the exact inverse of the tests above: the
/// default [`common::bring_up`] must never register a retention policy that can
/// reach the deliberately-backdated fixtures the query and ingest suites assert
/// on.
///
/// `apply_post_migration_setup` registers a live `policy_retention` job whose
/// background schedule the image fires ~3s after registration — mid-test. With
/// the production 365-day default, `common::entry`'s covered period
/// (`2023-11-14`) is years past the cutoff and its whole 7-day chunk is
/// drop-eligible the moment it is written, so a stalled test body loses rows it
/// already inserted (observed in CI as an aggregation bucket short by one).
///
/// Firing the policy explicitly asserts the property directly instead of waiting
/// on the scheduler, so this test is fast and deterministic rather than a race
/// that usually happens not to fire.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_default_harness_retention_cannot_drop_a_backdated_fixture() {
    let h = common::bring_up()
        .await
        .expect("timescaledb container (Docker required)");
    let store = common::record_store(&h.pool);
    let meter = common::meter(common::VCPU_METER);
    let tenant = Uuid::from_u128(0xBAD_11E);

    // A stock fixture, at the stock backdated covered period, via the real
    // ingest path.
    let rec = common::entry(&meter, tenant, "backdated", Decimal::ONE);
    let rec_id = rec.id;
    let window_end = rec.window_end;
    store.create(rec).await.expect("create the backdated entry");

    // Fire the harness's own registered policy — whatever window it was given.
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
        .expect("running the retention policy must not error");

    let survived: i64 = sqlx::query_scalar("SELECT count(*) FROM usage_records WHERE id = $1")
        .bind(rec_id)
        .fetch_one(&h.pool)
        .await
        .expect("count after");
    assert_eq!(
        survived, 1,
        "the default harness registered a retention policy that can delete a stock \
         fixture row (window_end = {window_end}); every test inserting fixtures is then \
         racing the background scheduler. bring_up() must pass a window no fixture can \
         fall outside of - see common::NO_DROP_RETENTION_SECS"
    );
}
