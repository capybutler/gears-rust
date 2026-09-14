//! The retention sweep: drops every ledger chunk whose types have all passed
//! their declared retention.
//!
//! Retention lives in `types-registry`, which the database cannot reach, so the
//! sweep runs in the plugin rather than as a `TimescaleDB` job. It reads each
//! chunk's time and type-key range from the `TimescaleDB` catalog, resolves the
//! current retention of every type in that key range, and decides the chunk
//! through [`drop_decision`] — which never drops without a definite retention
//! for every type the chunk may hold.

// Vendored TimescaleDB raw-SQL backend: `sqlx` is required infra (see
// `record_store.rs`).
#![allow(unknown_lints, de0706_no_direct_sqlx)]

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::{Duration, Instant};

use sqlx::{Connection as _, PgPool};
use time::OffsetDateTime;

use crate::domain::ports::{RetentionError, RetentionSource};
use crate::domain::retention::{Decision, drop_decision};
use crate::infra::metrics::{Metrics, SweepOutcome};

/// Advisory-lock key admitting one sweeper at a time across replicas.
/// Arbitrary but stable, and distinct from the init lock's key.
/// (`0x7563_7473` == ASCII `"ucts"`.)
pub const SWEEP_ADVISORY_LOCK_KEY: i64 = 0x7563_7473;

/// Every chunk of `usage_records` with its time range end and type-key range.
///
/// Reads internal catalog tables, whose shape changed between `TimescaleDB`
/// 2.17 and 2.29; `tests/schema_integration_pg.rs` pins it against the image
/// the plugin is tested on. Each chunk collapses to one row **before** the
/// time conversion, so no planner order can apply the conversion to the key
/// dimension's range.
pub const LIST_CHUNKS_SQL: &str = "SELECT ch.relid::text AS chunk, \
     _timescaledb_functions.to_timestamp(\
     max(ds.range_end) FILTER (WHERE d.column_name = 'window_end')) AS time_end, \
     max(ds.range_start) FILTER (WHERE d.column_name = 'type_key') AS key_start, \
     max(ds.range_end) FILTER (WHERE d.column_name = 'type_key') AS key_end \
     FROM _timescaledb_catalog.chunk ch \
     JOIN _timescaledb_catalog.hypertable h ON h.id = ch.hypertable_id \
     JOIN _timescaledb_catalog.dimension_slice ds ON ds.chunk_id = ch.id \
     JOIN _timescaledb_catalog.dimension d ON d.id = ds.dimension_id \
     WHERE h.table_name = 'usage_records' \
     GROUP BY ch.relid";

/// Drops one chunk by its schema-qualified name. `DROP TABLE <chunk>` is an
/// equivalent fallback should this internal function change.
pub const DROP_CHUNK_SQL: &str = "SELECT _timescaledb_functions.drop_chunk($1::regclass)";

/// One ledger chunk, as the `TimescaleDB` catalog describes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChunkSlice {
    /// Schema-qualified chunk table, e.g. `_timescaledb_internal._hyper_1_1_chunk`.
    pub chunk: String,
    /// Exclusive end of the chunk's `window_end` range.
    pub time_end: OffsetDateTime,
    /// Inclusive start of the chunk's `type_key` range.
    pub key_start: i64,
    /// Exclusive end of the chunk's `type_key` range.
    pub key_end: i64,
}

/// What one sweep did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SweepReport {
    /// Another replica held the sweep lock, so nothing was read.
    pub skipped_locked: bool,
    pub chunks_seen: usize,
    pub dropped: usize,
    pub kept_unresolved: usize,
    pub drop_failures: usize,
}

/// One raw row of [`LIST_CHUNKS_SQL`], before the range-presence check that
/// turns it into a [`ChunkSlice`]. Named so its type stays under the
/// workspace's `type-complexity-threshold` at the call site.
type ChunkRow = (String, Option<OffsetDateTime>, Option<i64>, Option<i64>);

/// Every chunk of the ledger. A chunk missing either range — which a
/// two-dimension hypertable does not produce — is logged and left out, and so
/// is never dropped.
///
/// # Errors
///
/// Returns the `sqlx` error of the catalog query.
pub async fn list_chunks(pool: &PgPool) -> Result<Vec<ChunkSlice>, sqlx::Error> {
    let rows: Vec<ChunkRow> = sqlx::query_as(LIST_CHUNKS_SQL).fetch_all(pool).await?;
    Ok(rows
        .into_iter()
        .filter_map(|(chunk, time_end, key_start, key_end)| {
            if let (Some(time_end), Some(key_start), Some(key_end)) = (time_end, key_start, key_end)
            {
                Some(ChunkSlice {
                    chunk,
                    time_end,
                    key_start,
                    key_end,
                })
            } else {
                tracing::warn!(
                    chunk = %chunk,
                    "a ledger chunk lacks a window_end or type_key range; it is kept"
                );
                None
            }
        })
        .collect())
}

/// Runs retention sweeps over one database.
pub struct PgRetentionSweeper {
    pool: PgPool,
    source: Arc<dyn RetentionSource>,
    metrics: Arc<Metrics>,
}

impl PgRetentionSweeper {
    #[must_use]
    pub fn new(pool: PgPool, source: Arc<dyn RetentionSource>, metrics: Arc<Metrics>) -> Self {
        Self {
            pool,
            source,
            metrics,
        }
    }

    /// One sweep, recorded under its outcome whatever that is.
    ///
    /// # Errors
    ///
    /// Returns the `sqlx` error that ended the sweep: acquiring the lock
    /// connection, taking the lock, listing chunks or loading type keys. A
    /// failed drop does not end the sweep; it is counted in the report.
    pub async fn sweep_once(&self) -> Result<SweepReport, sqlx::Error> {
        let started = Instant::now();
        let result = self.sweep_under_lock().await;
        let outcome = match &result {
            Ok(report) if report.skipped_locked => SweepOutcome::SkippedLocked,
            Ok(_) => SweepOutcome::Completed,
            Err(_) => SweepOutcome::Failed,
        };
        self.metrics
            .record_retention_sweep(outcome, started.elapsed().as_secs_f64());
        result
    }

    /// Take the sweep lock and sweep, or report a skip.
    ///
    /// The lock is session-level and taken on a **detached** connection, so it
    /// lives exactly as long as that connection: closing it releases the lock
    /// on every path, including a sweep abandoned mid-way, and a locked
    /// connection is never handed back to the pool.
    async fn sweep_under_lock(&self) -> Result<SweepReport, sqlx::Error> {
        let mut lock_conn = self.pool.acquire().await?.detach();
        let locked: bool = sqlx::query_scalar("SELECT pg_try_advisory_lock($1)")
            .bind(SWEEP_ADVISORY_LOCK_KEY)
            .fetch_one(&mut lock_conn)
            .await?;
        let result = if locked {
            self.sweep(OffsetDateTime::now_utc()).await
        } else {
            Ok(SweepReport {
                skipped_locked: true,
                ..SweepReport::default()
            })
        };
        if let Err(e) = lock_conn.close().await {
            tracing::warn!(
                error = %e,
                "closing the retention sweep's lock connection failed; the lock frees with the session"
            );
        }
        result
    }

    /// One pass over every chunk, deciding each against the retentions of the
    /// types in its key range.
    async fn sweep(&self, now: OffsetDateTime) -> Result<SweepReport, sqlx::Error> {
        let chunks = list_chunks(&self.pool).await?;
        let type_keys: BTreeMap<i64, String> =
            sqlx::query_as::<_, (i32, String)>("SELECT type_key, gts_type_id FROM usage_type_key")
                .fetch_all(&self.pool)
                .await?
                .into_iter()
                .map(|(key, gts_type_id)| (i64::from(key), gts_type_id))
                .collect();

        let mut report = SweepReport {
            chunks_seen: chunks.len(),
            ..SweepReport::default()
        };
        // Resolved at most once per sweep and never across sweeps: retention is
        // mutable, and an amendment must reach the next sweep.
        let mut resolved: HashMap<String, Result<Duration, RetentionError>> = HashMap::new();

        for chunk in &chunks {
            let retentions = self
                .resolve_chunk_retentions(chunk, &type_keys, &mut resolved)
                .await;
            let decision = drop_decision(chunk.time_end, &retentions, now);
            self.apply_decision(chunk, decision, &mut report).await;
        }

        let remaining = chunks.len().saturating_sub(report.dropped);
        self.metrics
            .set_chunks(u64::try_from(remaining).unwrap_or(u64::MAX));
        Ok(report)
    }

    /// The retention of every type in `chunk`'s key range, resolved from
    /// `resolved` where already known this sweep and from the source
    /// otherwise.
    async fn resolve_chunk_retentions(
        &self,
        chunk: &ChunkSlice,
        type_keys: &BTreeMap<i64, String>,
        resolved: &mut HashMap<String, Result<Duration, RetentionError>>,
    ) -> Vec<Result<Duration, RetentionError>> {
        let mut retentions = Vec::new();
        for gts_type_id in type_keys
            .range(chunk.key_start..chunk.key_end)
            .map(|(_, id)| id)
        {
            let retention = if let Some(known) = resolved.get(gts_type_id) {
                known.clone()
            } else {
                let fresh = self.source.retention(gts_type_id).await;
                resolved.insert(gts_type_id.clone(), fresh.clone());
                fresh
            };
            retentions.push(retention);
        }
        retentions
    }

    /// Act on one chunk's [`Decision`]: drop it, or count and log a kept
    /// chunk whose retention could not be resolved.
    async fn apply_decision(
        &self,
        chunk: &ChunkSlice,
        decision: Decision,
        report: &mut SweepReport,
    ) {
        match decision {
            Decision::Drop => self.drop_chunk(chunk, report).await,
            Decision::Keep(reason) if reason.is_unresolved() => {
                report.kept_unresolved += 1;
                self.metrics.inc_retention_chunk_kept_unresolved(reason);
                tracing::warn!(
                    chunk = %chunk.chunk,
                    reason = reason.as_label(),
                    "kept a ledger chunk whose retention could not be resolved"
                );
            }
            Decision::Keep(_) => {}
        }
    }

    /// Drop one expired chunk, counting the outcome either way.
    async fn drop_chunk(&self, chunk: &ChunkSlice, report: &mut SweepReport) {
        match sqlx::query(DROP_CHUNK_SQL)
            .bind(&chunk.chunk)
            .execute(&self.pool)
            .await
        {
            Ok(_) => {
                report.dropped += 1;
                self.metrics.inc_retention_chunk_dropped();
            }
            Err(e) => {
                report.drop_failures += 1;
                self.metrics.inc_retention_drop_failure();
                tracing::warn!(
                    chunk = %chunk.chunk,
                    error = %e,
                    "dropping an expired ledger chunk failed; the next sweep retries it"
                );
            }
        }
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "retention_sweep_tests.rs"]
mod retention_sweep_tests;
