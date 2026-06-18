use std::str::FromStr;
use std::time::Duration;

use sqlx::PgPool;
use sqlx::postgres::{PgConnectOptions, PgPoolOptions, PgSslMode};

use crate::config::TimescaleDbPluginConfig;

/// Embedded schema migrations (`migrations/` at crate root).
pub static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("./migrations");

/// Parse the DSN into connection options with TLS enforced by default.
///
/// TLS is the plugin's one stated security obligation (DESIGN §3.5 / §Security),
/// so it is enforced here rather than left to operator DSN convention: sqlx's
/// default is `prefer`, which silently falls back to plaintext. The silent
/// modes — an unspecified `sslmode`, `prefer`, or `allow` — are upgraded to
/// `require` so credentials and usage data are never sent in cleartext by
/// omission. A stronger operator choice (`verify-ca` / `verify-full`) is
/// preserved.
///
/// An explicit `sslmode=disable` is honored as a deliberate, auditable opt-out
/// (trusted networks / local dev / the integration test container, which serves
/// no TLS). This still closes the defect — the *silent* plaintext fallback —
/// while leaving a documented escape hatch.
///
/// # Errors
/// Returns `sqlx::Error` if the DSN cannot be parsed.
fn connect_options(database_url: &str) -> Result<PgConnectOptions, sqlx::Error> {
    let opts = PgConnectOptions::from_str(database_url)?;
    Ok(match opts.get_ssl_mode() {
        // Silent fallback modes (incl. the unspecified default `prefer`): upgrade.
        PgSslMode::Allow | PgSslMode::Prefer => opts.ssl_mode(PgSslMode::Require),
        // Explicit `disable` is a deliberate opt-out; `require` and the verifying
        // modes already meet the obligation. All kept as-is.
        PgSslMode::Disable | PgSslMode::Require | PgSslMode::VerifyCa | PgSslMode::VerifyFull => {
            opts
        }
    })
}

/// Build the connection pool with TLS enforced (`sslmode >= require`, see
/// [`connect_options`]).
///
/// # Errors
/// Returns `sqlx::Error` if the DSN is malformed or the pool cannot connect
/// within the timeout.
pub async fn build_pool(cfg: &TimescaleDbPluginConfig) -> Result<PgPool, sqlx::Error> {
    PgPoolOptions::new()
        .min_connections(cfg.pool_size_min)
        .max_connections(cfg.pool_size_max)
        .acquire_timeout(Duration::from_secs(cfg.connection_timeout_secs))
        .connect_with(connect_options(&cfg.database_url)?)
        .await
}

/// Fixed advisory-lock key namespacing the plugin's post-migration setup.
/// Arbitrary but stable; the plugin owns its database, so a collision with an
/// unrelated advisory lock is not a concern. (`0x7563_7462` == ASCII `"uctb"`.)
const INIT_ADVISORY_LOCK_KEY: i64 = 0x7563_7462;

/// Run the post-migration policy/job registration under a database advisory
/// lock so concurrently-initializing replicas serialize here.
///
/// [`apply_retention_policy`]'s remove-then-add and [`apply_dedup_cleanup_job`]'s
/// alter-then-add sequences are each non-atomic, and `add_retention_policy`
/// (no `if_not_exists`) *errors* if a policy already exists — so two pods racing
/// this section could leave a half-applied state or fail outright. A
/// session-level `pg_advisory_lock` held on a dedicated connection for the whole
/// section lets only one replica apply at a time; the rest block until it
/// releases. (Schema migrations themselves are already serialized by sqlx's own
/// migration lock; this covers the registration that sqlx does not.)
///
/// The policy/job functions keep running in autocommit on the pool, exactly as
/// before — deliberately *not* wrapped in an explicit transaction, since
/// `TimescaleDB` policy functions are happiest in autocommit. The lock is
/// released on every return path; if the holding process dies, Postgres releases
/// it when the session ends.
///
/// # Errors
/// Returns `sqlx::Error` if the lock cannot be taken or either registration
/// step fails.
pub async fn apply_post_migration_setup(
    pool: &PgPool,
    retention_secs: u64,
) -> Result<(), sqlx::Error> {
    // Hold a session-level advisory lock on a dedicated connection for the whole
    // critical section. Concurrent replicas block on this `pg_advisory_lock`
    // until the holder releases it below, so only one applies at a time.
    let mut lock_conn = pool.acquire().await?;
    sqlx::query("SELECT pg_advisory_lock($1)")
        .bind(INIT_ADVISORY_LOCK_KEY)
        .execute(&mut *lock_conn)
        .await?;

    let result = async {
        apply_retention_policy(pool, retention_secs).await?;
        apply_dedup_cleanup_job(pool, retention_secs).await
    }
    .await;

    // Release on every path (including the error path) so a failing replica
    // never wedges the others. If the unlock itself fails the session is likely
    // already broken, in which case Postgres frees the lock when it ends.
    if let Err(e) = sqlx::query("SELECT pg_advisory_unlock($1)")
        .bind(INIT_ADVISORY_LOCK_KEY)
        .execute(&mut *lock_conn)
        .await
    {
        tracing::warn!(
            error = %e,
            "failed to release init advisory lock; it frees when the session ends"
        );
    }

    result
}

/// Idempotently register the config-driven retention policy, **updating** it if
/// it already exists so a changed `retention_secs` takes effect on restart.
/// Runs after migrations.
///
/// `add_retention_policy(if_not_exists => TRUE)` would *skip* an existing
/// policy and silently keep the old window; remove-then-add applies the new
/// one. The sub-second gap with no policy is harmless — retention is a slow
/// background job.
///
/// # Errors
/// Returns `sqlx::Error` if either statement fails.
pub async fn apply_retention_policy(pool: &PgPool, retention_secs: u64) -> Result<(), sqlx::Error> {
    let secs = i64::try_from(retention_secs).unwrap_or(i64::MAX);
    sqlx::query("SELECT remove_retention_policy('usage_records', if_exists => TRUE)")
        .execute(pool)
        .await?;
    sqlx::query(
        "SELECT add_retention_policy('usage_records', \
         drop_after => make_interval(secs => $1::double precision))",
    )
    .bind(secs)
    .execute(pool)
    .await?;
    Ok(())
}

/// Idempotently register (or update) the `usage_dedup` cleanup job. Runs after
/// migrations, mirroring [`apply_retention_policy`]. The job calls the
/// `prune_usage_dedup` procedure (migration 0002), which deletes dedup rows
/// whose record has aged out. Update-then-create so a changed retention is
/// applied on restart and exactly one job exists.
///
/// `TimescaleDB` 2.x `add_job` does not have an `if_not_exists` parameter, so
/// the two-query pattern is: (1) `alter_job` if the job exists (0 rows if not),
/// then (2) `add_job ... WHERE NOT EXISTS` to create it only when absent.
///
/// # Errors
/// Returns `sqlx::Error` if either statement fails.
pub async fn apply_dedup_cleanup_job(
    pool: &PgPool,
    retention_secs: u64,
) -> Result<(), sqlx::Error> {
    // Concurrent replicas are serialized by the session advisory lock that
    // `apply_post_migration_setup` holds across this call (see there), so this
    // runs single-flight in practice. The alter-then-add two-query pattern below
    // is the idempotency mechanism for re-apply on restart — update an existing
    // job's config in place, create one only when absent — and doubles as a
    // belt-and-suspenders guard against a duplicate were the lock ever bypassed;
    // never data corruption. The same reasoning applies to `apply_retention_policy`
    // above.
    let secs = i64::try_from(retention_secs).unwrap_or(i64::MAX);
    // Update the existing job's config if present (no-op when job does not exist).
    sqlx::query(
        "SELECT alter_job(j.job_id, schedule_interval => INTERVAL '1 day', \
                config => jsonb_build_object('retention_secs', $1::bigint)) \
         FROM timescaledb_information.jobs j WHERE j.proc_name = 'prune_usage_dedup'",
    )
    .bind(secs)
    .execute(pool)
    .await?;
    // Create the job only when it was not there; guards against duplicate creation
    // on re-apply (add_job in TimescaleDB 2.x has no if_not_exists parameter).
    sqlx::query(
        "SELECT add_job('prune_usage_dedup', INTERVAL '1 day', \
                config => jsonb_build_object('retention_secs', $1::bigint)) \
         WHERE NOT EXISTS ( \
             SELECT 1 FROM timescaledb_information.jobs WHERE proc_name = 'prune_usage_dedup' \
         )",
    )
    .bind(secs)
    .execute(pool)
    .await?;
    Ok(())
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "pool_tests.rs"]
mod pool_tests;
