//! Samples the feed's settled-horizon lag: the age of the oldest write
//! transaction the plugin role can see in `pg_stat_activity`, which is what
//! bounds acceptance-to-feed-visibility (`docs/DESIGN.md` §4.1 item 2,
//! §4.3).

// Vendored TimescaleDB raw-SQL backend: `sqlx` is required infra (see
// `record_store.rs`).
#![allow(unknown_lints, de0706_no_direct_sqlx)]

use std::sync::Arc;

use sqlx::PgPool;

use crate::infra::metrics::Metrics;

/// The age of the oldest write transaction the plugin role can see.
///
/// `backend_xid IS NOT NULL` is what makes it a *write* transaction: a
/// read-only transaction is assigned no id and so cannot hold the settled
/// horizon back. `NULL` when the role sees no such session, which the caller
/// turns into a gauge left unset rather than a zero.
const HORIZON_LAG_SQL: &str = "SELECT extract(epoch FROM (now() - min(xact_start)))::float8 \
     FROM pg_stat_activity WHERE backend_xid IS NOT NULL";

/// Publishes the feed's settled-horizon lag on the plugin's metric inventory.
///
/// Modelled on
/// [`RollupMonitor`](crate::infra::storage::rollup_maintenance::RollupMonitor):
/// same two fields, same `sample_once` shape, same error contract — a
/// `Result` that fails only when the query itself fails, never on "nothing to
/// report".
pub struct FeedHorizonMonitor {
    pool: PgPool,
    metrics: Arc<Metrics>,
}

impl FeedHorizonMonitor {
    #[must_use]
    pub fn new(pool: PgPool, metrics: Arc<Metrics>) -> Self {
        Self { pool, metrics }
    }

    /// Sample the settled-horizon lag and set the gauge when one is observed.
    ///
    /// Returns the observed lag, or `None` when the plugin role sees no
    /// backend holding an open write transaction — best-effort (`docs/DESIGN.md`
    /// §4.3): the gauge is left unset on that path rather than zeroed, because
    /// a zero would read as a healthy instance rather than as an unanswerable
    /// question.
    ///
    /// **A prior value is not reset.** Both an `Err` and a `None` observation
    /// leave the gauge exactly as it was, so "unset" is only ever
    /// distinguishable from a stale reading by an instrument that has *never*
    /// been recorded on at all (see
    /// `metrics_tests::the_horizon_lag_gauge_is_absent_until_something_sets_it`).
    /// Once a real lag has been recorded once, a later idle tick cannot make
    /// the series disappear again — an operator reading it after the write
    /// transaction that produced the last value has long since closed sees
    /// that transaction's age, not "no observation". This is inherent to an
    /// `OTel` gauge's last-value semantics, not a defect this code could fix by
    /// itself.
    ///
    /// # Errors
    /// Returns `sqlx::Error` if the query fails; the gauge then keeps its
    /// last value.
    pub async fn sample_once(&self) -> Result<Option<f64>, sqlx::Error> {
        let lag: Option<f64> = sqlx::query_scalar(HORIZON_LAG_SQL)
            .fetch_one(&self.pool)
            .await?;
        if let Some(seconds) = lag {
            self.metrics.set_feed_horizon_lag(seconds);
        }
        Ok(lag)
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "feed_horizon_tests.rs"]
mod feed_horizon_tests;
