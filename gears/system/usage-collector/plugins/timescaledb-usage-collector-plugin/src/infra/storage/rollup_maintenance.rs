//! Everything that maintains the `usage_rollup_1h` continuous aggregate apart
//! from reading it: its refresh policies here, and (in later tasks) the lookup
//! of its materialisation table and the sampling of its refresh jobs.

// Vendored TimescaleDB raw-SQL backend: `sqlx` is required infra (see
// `record_store.rs`).
#![allow(unknown_lints, de0706_no_direct_sqlx)]

use sqlx::PgPool;

use crate::config::TimescaleDbPluginConfig;

/// The rollup view `migrations/0002_usage_rollup.sql` creates.
pub const ROLLUP_VIEW: &str = "usage_rollup_1h";

/// Deletes every refresh policy of the rollup. For a refresh policy
/// `timescaledb_information.jobs.hypertable_name` is the view itself (verified
/// on 2.29.2), and a policy with the same offsets as an existing one is refused,
/// so setup deletes before it adds.
pub const DELETE_ROLLUP_POLICIES_SQL: &str = "SELECT delete_job(job_id) \
     FROM timescaledb_information.jobs \
     WHERE proc_name = 'policy_refresh_continuous_aggregate' \
     AND hypertable_schema = current_schema() AND hypertable_name = 'usage_rollup_1h'";

/// The live policy: `$1` live window, `$2` materialisation lag, `$3` schedule,
/// all in seconds.
pub const ADD_LIVE_POLICY_SQL: &str = "SELECT add_continuous_aggregate_policy('usage_rollup_1h', \
     start_offset => make_interval(secs => $1::double precision), \
     end_offset => make_interval(secs => $2::double precision), \
     schedule_interval => make_interval(secs => $3::double precision))";

/// The history policy: everything older than the live window. `$1` live
/// window, `$2` schedule, in seconds. A refresh over a range with nothing
/// invalidated is a no-op, so an unbounded start costs nothing steady-state.
pub const ADD_HISTORY_POLICY_SQL: &str = "SELECT add_continuous_aggregate_policy('usage_rollup_1h', \
     start_offset => NULL, \
     end_offset => make_interval(secs => $1::double precision), \
     schedule_interval => make_interval(secs => $2::double precision))";

/// A config value in seconds as the `double precision` `make_interval` takes.
/// Every value is validated `<= MAX_INTERVAL_SECS` (100 years), well inside
/// `i64`, so the saturation is unreachable in practice.
fn secs(value: u64) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

/// Replace the rollup's refresh policies with the two `cfg` describes.
/// Idempotent: a restart with changed values applies them.
///
/// # Errors
/// Returns `sqlx::Error` if any statement fails.
pub async fn apply_rollup_policies(
    pool: &PgPool,
    cfg: &TimescaleDbPluginConfig,
) -> Result<(), sqlx::Error> {
    sqlx::query(DELETE_ROLLUP_POLICIES_SQL)
        .execute(pool)
        .await?;
    sqlx::query(ADD_LIVE_POLICY_SQL)
        .bind(secs(cfg.rollup_live_window_secs))
        .bind(secs(cfg.rollup_materialization_lag_secs))
        .bind(secs(cfg.rollup_refresh_interval_secs))
        .execute(pool)
        .await?;
    sqlx::query(ADD_HISTORY_POLICY_SQL)
        .bind(secs(cfg.rollup_live_window_secs))
        .bind(secs(cfg.rollup_history_refresh_interval_secs))
        .execute(pool)
        .await?;
    Ok(())
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "rollup_maintenance_tests.rs"]
mod rollup_maintenance_tests;
