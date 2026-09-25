//! Everything that maintains the `usage_rollup_1h` continuous aggregate apart
//! from reading it: its refresh policies here, and (in later tasks) the lookup
//! of its materialisation table and the sampling of its refresh jobs.

// Vendored TimescaleDB raw-SQL backend: `sqlx` is required infra (see
// `record_store.rs`).
#![allow(unknown_lints, de0706_no_direct_sqlx)]

use std::sync::Arc;

use sqlx::PgPool;

use crate::config::TimescaleDbPluginConfig;
use crate::infra::metrics::Metrics;

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

/// The rollup's materialisation hypertable, schema-qualified and quoted with
/// `%I` by the database, from the public information view rather than the
/// internal catalog.
pub const MATERIALIZATION_TABLE_SQL: &str = "SELECT format('%I.%I', materialization_hypertable_schema, materialization_hypertable_name) \
     FROM timescaledb_information.continuous_aggregates \
     WHERE view_schema = current_schema() AND view_name = 'usage_rollup_1h'";

/// The materialisation hypertable, or `None` when the rollup does not exist.
///
/// # Errors
/// Returns `sqlx::Error` if the query fails.
pub async fn materialization_table(pool: &PgPool) -> Result<Option<String>, sqlx::Error> {
    sqlx::query_scalar(MATERIALIZATION_TABLE_SQL)
        .fetch_optional(pool)
        .await
}

/// Deletes the rollup rows a dropped ledger chunk fed: `$1`/`$2` the chunk's
/// `type_key` range, `$3`/`$4` its `window_end` range.
///
/// Only buckets **wholly** inside the chunk are cut. With the hour-multiple
/// chunk interval every bucket is; a straddling bucket from a chunk created
/// under an earlier interval is kept while its neighbour still holds rows, so
/// the rollup can over-retain one bucket and never under-count.
///
/// `table` must come from [`materialization_table`], which quotes it; it is
/// never caller input.
#[must_use]
pub fn delete_rollup_rows_sql(table: &str) -> String {
    format!(
        "DELETE FROM {table} WHERE type_key >= $1::bigint AND type_key < $2::bigint \
         AND bucket >= $3 AND bucket + INTERVAL '1 hour' <= $4"
    )
}

/// Each rollup refresh policy with its last run status and the seconds since
/// its last success. `last_successful_finish` is `-infinity` before the first
/// success, which makes the age `+infinity`.
pub const REFRESH_JOB_STATUS_SQL: &str = "SELECT (j.config->>'start_offset') IS NULL AS is_history, \
     js.last_run_status::text, \
     EXTRACT(EPOCH FROM (now() - js.last_successful_finish))::double precision AS age_secs \
     FROM timescaledb_information.jobs j \
     LEFT JOIN timescaledb_information.job_stats js ON js.job_id = j.job_id \
     WHERE j.proc_name = 'policy_refresh_continuous_aggregate' \
     AND j.hypertable_schema = current_schema() AND j.hypertable_name = 'usage_rollup_1h'";

/// Which of the two refresh policies a job is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefreshPolicy {
    /// The frequent policy over the live window.
    Live,
    /// The policy over everything older than the live window.
    History,
}

impl RefreshPolicy {
    /// The bounded `policy` label value.
    #[must_use]
    pub const fn as_label(self) -> &'static str {
        match self {
            Self::Live => "live",
            Self::History => "history",
        }
    }
}

/// One refresh policy's health.
#[derive(Debug, Clone, PartialEq)]
pub struct RefreshJobStatus {
    pub policy: RefreshPolicy,
    /// The last run failed.
    pub failing: bool,
    /// Seconds since the last success; `None` if it has never succeeded.
    pub secs_since_success: Option<f64>,
}

/// Interpret one row of [`REFRESH_JOB_STATUS_SQL`].
#[must_use]
pub fn job_status_from_row(
    is_history: bool,
    last_run_status: Option<&str>,
    age_secs: Option<f64>,
) -> RefreshJobStatus {
    RefreshJobStatus {
        policy: if is_history {
            RefreshPolicy::History
        } else {
            RefreshPolicy::Live
        },
        failing: last_run_status == Some("Failure"),
        secs_since_success: age_secs.filter(|a| a.is_finite()).map(|a| a.max(0.0)),
    }
}

/// Every refresh policy of the rollup, as currently recorded.
///
/// # Errors
/// Returns `sqlx::Error` if the query fails.
#[allow(clippy::type_complexity)]
pub async fn refresh_job_statuses(pool: &PgPool) -> Result<Vec<RefreshJobStatus>, sqlx::Error> {
    let rows: Vec<(bool, Option<String>, Option<f64>)> = sqlx::query_as(REFRESH_JOB_STATUS_SQL)
        .fetch_all(pool)
        .await?;
    Ok(rows
        .into_iter()
        .map(|(is_history, status, age)| job_status_from_row(is_history, status.as_deref(), age))
        .collect())
}

/// Publishes refresh-policy health on the plugin's metric inventory.
pub struct RollupMonitor {
    pool: PgPool,
    metrics: Arc<Metrics>,
}

impl RollupMonitor {
    #[must_use]
    pub fn new(pool: PgPool, metrics: Arc<Metrics>) -> Self {
        Self { pool, metrics }
    }

    /// Read every policy's status and set its gauges. Returns how many
    /// policies were sampled.
    ///
    /// # Errors
    /// Returns `sqlx::Error` if the status query fails; the gauges then keep
    /// their last values.
    pub async fn sample_once(&self) -> Result<usize, sqlx::Error> {
        let statuses = refresh_job_statuses(&self.pool).await?;
        for status in &statuses {
            self.metrics.set_rollup_refresh_status(status);
        }
        self.metrics
            .set_rollup_refresh_policies(u64::try_from(statuses.len()).unwrap_or(u64::MAX));
        Ok(statuses.len())
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "rollup_maintenance_tests.rs"]
mod rollup_maintenance_tests;
