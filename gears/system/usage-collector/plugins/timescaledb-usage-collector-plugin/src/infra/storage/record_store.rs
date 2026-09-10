//! Postgres-backed [`RecordStore`] over the `usage_records` hypertable.
//!
//! All operations — `create` / `create_batch` / `get` / `list` / `aggregate` —
//! are real `sqlx`.

// Vendored TimescaleDB raw-SQL backend: `sqlx` is required infra (hypertable
// time-series, `time_bucket` aggregation, keyset pagination — see DESIGN.md). Tenant
// isolation is enforced by hand via parameterized `tenant_id` predicates and an
// allowlisted-identifier query builder (DESIGN.md §Injection-Safe Query Translation),
// not SecureConn/AccessScope.
#![allow(unknown_lints, de0706_no_direct_sqlx)]

use std::collections::HashMap;
use std::future::Future;
use std::sync::{Arc, LazyLock};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use bigdecimal::BigDecimal;
use rand::RngExt as _;
use rust_decimal::Decimal;
use sqlx::pool::PoolConnection;
use sqlx::postgres::PgRow;
use sqlx::{Acquire as _, AssertSqlSafe, PgPool, Postgres, Row};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use tokio_util::sync::CancellationToken;
use toolkit_odata::filter::FilterField;
use toolkit_odata::{ODataQuery, Page as ODataPage, PageInfo, SortDir, ast};
use uuid::Uuid;

use usage_collector_sdk::{
    AggregationBucket, AggregationDimension, AggregationFold, AggregationResult, MetadataFilter,
    MeterTypeId, TimeRange, UsageCollectorPluginError, UsageRecord, UsageRecordFilterField,
    canonical_period_bound, is_keyset_safe_record_field,
};

use crate::domain::ports::RecordStore;
use crate::infra::metrics::{ErrorClass, InsertMode, Metrics, OpDurationGuard, QueryKind, TimedOp};
use crate::infra::storage::entity::UsageRecordRow;
use crate::infra::storage::error::{
    DbErrorClass, acquire_error_clears_readiness, classify_db, db_code_and_constraint, map_sqlx_err,
};
use crate::infra::storage::mapper::{
    invalidation_to_row, metadata_jsonb_to_map, metadata_map_to_jsonb, record_row_to_model,
};
use crate::infra::storage::query::aggregate::{
    aggregate_limit_clause, dimension_select_expr, fold_select_expr, withdrawal_exclusion_clause,
};
use crate::infra::storage::query::keyset::{
    encode_next_cursor, ensure_forward_cursor, keyset_predicate, render_order_by,
};
use crate::infra::storage::query::translate::{
    SqlBind, SqlCtx, bind_one, bind_one_query, record_column, translate_scope,
};
use crate::infra::storage::query::{
    effective_page_size, ledger_from_clause, push_metadata_filter_clauses,
    push_meter_and_range_clauses,
};

/// Default page size when the caller omits `$top` (`query.limit`).
const DEFAULT_PAGE_SIZE: u64 = 100;

/// Column list for every `usage_records` SELECT / RETURNING, in
/// [`UsageRecordRow`] field order. A static const (never caller input), so
/// there is no risk of SQL injection.
///
/// `sqlx`'s derived `FromRow` looks each column up by the struct's own field
/// name, so the order here is a reading convenience — matching the struct and
/// the DDL — rather than a decode requirement. **Omission is the hazard**: a
/// missing column fails the decode with `no column found for name: <field>`.
///
/// The ledger's `entry_type` is deliberately absent. It is a stored generated
/// column that exists so `$filter=entry_type eq 'invalidation'` resolves to a
/// real column; nothing decodes it, because [`UsageRecordRow`] has no field
/// for it (see that struct's doc).
const RECORD_COLUMNS: &str = "id, tenant_id, gts_type_id, value, window_start, window_end, \
     resource_id, resource_type, subject_id, subject_type, idempotency_key, invalidates, \
     reason_code, origin, acceptance_sequence, metadata, ingested_at";

/// The columns every insert writes: [`RECORD_COLUMNS`] minus `ingested_at`,
/// which the table defaults to `now()`. `entry_type` is generated and appears
/// in neither.
///
/// **One spelling, used four times** — the single-row insert's column list, the
/// batch insert's column list, its `SELECT` list and its `UNNEST` alias list.
/// Written out four times instead, a name transposed in any one of them binds a
/// `text[]` to the wrong `text` column, which Postgres accepts without
/// complaint and which no row-level test can see. `metadata` is deliberately
/// **last**, because the batch `SELECT` appends `::jsonb` to this string rather
/// than restating it (see [`BATCH_INSERT_SQL`]) — the cast binds to the final
/// identifier only, which is what makes the `SELECT` list unable to be a
/// transposition rather than merely tested not to be. A test asserts the
/// position directly, because deriving it from an order assertion whose oracle
/// happens to end in `metadata` would not survive a migration that declares a
/// column after it.
const INSERT_COLUMNS: &str = "id, tenant_id, gts_type_id, value, window_start, window_end, \
     resource_id, resource_type, subject_id, subject_type, idempotency_key, invalidates, \
     reason_code, origin, acceptance_sequence, metadata";

/// Postgres array types for [`INSERT_COLUMNS`], **in the same order**, as the
/// batch insert's `UNNEST` needs them. The length is what fixes the placeholder
/// count for both inserts.
const INSERT_COLUMN_ARRAY_TYPES: [&str; 16] = [
    "uuid",
    "uuid",
    "text",
    "numeric",
    "timestamptz",
    "timestamptz",
    "text",
    "text",
    "text",
    "text",
    "text",
    "uuid",
    "text",
    "text",
    "bigint",
    "text",
];

/// The dedup 5-tuple, as an `ON CONFLICT` arbiter. Both insert paths spend
/// their one arbiter here, which is why `usage_records_one_invalidation_uniq`
/// surfaces as a raw `23505` (see [`PgRecordStore::map_insert_error`]).
const DEDUP_CONFLICT_TARGET: &str =
    "tenant_id, gts_type_id, idempotency_key, window_start, window_end";

/// `$1, $2, …, $n`.
fn placeholders(n: usize) -> String {
    (1..=n)
        .map(|i| format!("${i}"))
        .collect::<Vec<_>>()
        .join(", ")
}

/// The single-row `INSERT … ON CONFLICT (5-tuple) DO NOTHING RETURNING`.
///
/// Built rather than inlined so a test can read the column list, the
/// placeholder count and the conflict target back out of it — and built
/// **once**, because every input to it is a constant and the alternative is
/// sixteen `format!`s per write.
static SINGLE_INSERT_SQL: LazyLock<String> = LazyLock::new(|| {
    format!(
        "INSERT INTO usage_records ({INSERT_COLUMNS}) VALUES ({}) \
         ON CONFLICT ({DEDUP_CONFLICT_TARGET}) DO NOTHING \
         RETURNING {RECORD_COLUMNS}",
        placeholders(INSERT_COLUMN_ARRAY_TYPES.len()),
    )
});

/// The multi-row `INSERT … SELECT FROM UNNEST(…) ON CONFLICT (5-tuple) DO
/// NOTHING RETURNING`.
///
/// The column list, the `SELECT` list and the `UNNEST` alias list are all
/// [`INSERT_COLUMNS`], so they cannot be transposed relative to one another —
/// the `SELECT` differs only by the trailing `::jsonb`, which works because
/// `metadata` is the last column. `UNNEST`'s parameters are
/// [`INSERT_COLUMN_ARRAY_TYPES`] in the same order, so `$n` is column `n`.
///
/// Built once, for the same reason as [`SINGLE_INSERT_SQL`].
static BATCH_INSERT_SQL: LazyLock<String> = LazyLock::new(|| {
    let unnest = INSERT_COLUMN_ARRAY_TYPES
        .iter()
        .enumerate()
        .map(|(i, ty)| format!("${}::{ty}[]", i + 1))
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "INSERT INTO usage_records ({INSERT_COLUMNS}) \
         SELECT {INSERT_COLUMNS}::jsonb FROM UNNEST({unnest}) AS t({INSERT_COLUMNS}) \
         ON CONFLICT ({DEDUP_CONFLICT_TARGET}) DO NOTHING \
         RETURNING {RECORD_COLUMNS}"
    )
});

/// `sqlx`-backed implementation of [`RecordStore`] over the `usage_records`
/// hypertable.
///
/// Every operation acquires its connection through [`Self::timed_acquire`], so
/// `pool.acquire.duration` is recorded per acquire and `tls.handshake.failure.count`
/// is incremented when a fresh physical connection fails its TLS handshake (via
/// [`Self::record_backend_error`]).
#[derive(Debug, Clone)]
pub struct PgRecordStore {
    pool: PgPool,
    metrics: Arc<Metrics>,
    cancel: CancellationToken,
}

impl PgRecordStore {
    /// Build a store over an existing connection pool. `cancel` is the gear's
    /// cancellation token; the request path stops re-arming the `ready` gauge
    /// once it fires so a drain-time acquire cannot flip readiness back on after
    /// the shutdown watcher has cleared it.
    #[must_use]
    pub fn new(pool: PgPool, metrics: Arc<Metrics>, cancel: CancellationToken) -> Self {
        Self {
            pool,
            metrics,
            cancel,
        }
    }

    /// Map a `sqlx` error via [`map_sqlx_err`] and, as a side effect, increment
    /// the backend-error counter under the matching [`ErrorClass`]
    /// ([`ErrorClass::Transient`] for a [`UsageCollectorPluginError::Transient`]
    /// mapping, otherwise [`ErrorClass::Internal`]). Returns the mapped error so
    /// it slots into the existing `.map_err(...)` call sites unchanged.
    fn record_backend_error(&self, err: &sqlx::Error) -> UsageCollectorPluginError {
        // A TLS handshake failure is the plugin's one metered transport-security
        // signal (DESIGN §Observability); count it before the generic mapping.
        if matches!(err, sqlx::Error::Tls(_)) {
            self.metrics.inc_tls_handshake_failure();
        }
        let mapped = map_sqlx_err(err);
        let class = if matches!(mapped, UsageCollectorPluginError::Transient { .. }) {
            ErrorClass::Transient
        } else {
            ErrorClass::Internal
        };
        self.metrics.inc_backend_error(class);
        mapped
    }

    /// Translate an insert-path `sqlx` error into the caller-visible outcome.
    ///
    /// Two of the ledger's unique constraints answer the caller rather than
    /// reporting a fault, and they reach this seam very differently. A
    /// statement admits exactly one `ON CONFLICT` arbiter and the ingest insert
    /// spends it on the dedup 5-tuple, so the dedup constraint's violation is
    /// suppressed into a no-row result and never arrives here — that arm of
    /// [`classify_db`] stays defensive. The at-most-one-invalidation index has
    /// no arbiter left to claim, so its violation always arrives as a raw
    /// `23505`, and this is the arm that turns it into
    /// [`UsageCollectorPluginError::AlreadyInvalidated`].
    ///
    /// `slots` are the `(tenant_id, invalidation target, window_end)` index
    /// slots the failed statement tried to occupy. They are used to name the
    /// invalidation already in place — and **that lookup is a read taken after
    /// the write was already rejected, so it is diagnostic and not a check.**
    /// The SPI's atomicity obligation is discharged by the index, inside the
    /// transaction, before this function is reached; this runs afterwards on a
    /// rolled-back connection purely so the rejection can name an entry. It
    /// looks like the pre-read the SPI forbids and is not one.
    async fn map_insert_error(
        &self,
        conn: &mut sqlx::PgConnection,
        err: &sqlx::Error,
        slots: &[(Uuid, Uuid, OffsetDateTime)],
    ) -> UsageCollectorPluginError {
        let Some((code, constraint)) = db_code_and_constraint(err) else {
            return self.record_backend_error(err);
        };
        if classify_db(&code, constraint.as_deref()) != DbErrorClass::AlreadyInvalidated {
            return self.record_backend_error(err);
        }
        match find_existing_invalidation(conn, slots).await {
            Ok(Some((id, invalidated_by))) => {
                UsageCollectorPluginError::AlreadyInvalidated { id, invalidated_by }
            }
            // The index refused the write, so an accepted invalidation existed
            // at that instant. Finding none now means its chunk was dropped by
            // retention in between — the same retention-boundary race the dedup
            // path carries. Retryable, rather than a fabricated identifier.
            Ok(None) => UsageCollectorPluginError::transient(
                "the invalidation that refused this withdrawal could not be named; retry",
            ),
            Err(read_err) => self.record_backend_error(&read_err),
        }
    }

    /// Acquire a pooled connection, recording `pool.acquire.duration`. Errors map
    /// through [`Self::record_backend_error`] (which also catches a TLS-handshake
    /// failure on a fresh physical connection). Every operation acquires through
    /// this path so the acquire-latency histogram is representative.
    async fn timed_acquire(&self) -> Result<PoolConnection<Postgres>, UsageCollectorPluginError> {
        let t = Instant::now();
        match self.pool.acquire().await {
            Ok(conn) => {
                self.metrics.record_pool_acquire(t.elapsed().as_secs_f64());
                // A successful acquire re-arms readiness (DESIGN §Observability:
                // `ready` recovers once the pool serves a connection again), but
                // only while not shutting down: once `cancel` fires the shutdown
                // watcher owns the gauge, so a drain-time acquire must not flip it
                // back to 1. This gate narrows — it does not fully close — the
                // check-then-set window against the watcher; that residual race
                // is a sub-tick blip on a best-effort gauge during one-way
                // shutdown, so it is left as-is rather than serialized.
                if !self.cancel.is_cancelled() {
                    self.metrics.set_ready(true);
                }
                Ok(conn)
            }
            Err(e) => {
                // Clear readiness only on a connectivity-class failure so the
                // `uc_timescaledb_ready == 0` alert fires on a live outage but
                // not on a healthy-but-saturated pool (`PoolTimedOut` while the
                // pool still holds connections), which would otherwise flap the
                // gauge under load.
                if acquire_error_clears_readiness(&e, self.pool.size()) {
                    self.metrics.set_ready(false);
                }
                Err(self.record_backend_error(&e))
            }
        }
    }

    /// Core single-row insert path: dedup on the `usage_records`
    /// `(tenant_id, gts_type_id, idempotency_key, window_start, window_end)`
    /// UNIQUE (`usage_records_dedup_uniq`) via `INSERT … ON CONFLICT … DO
    /// NOTHING`, then lost-the-race absorb-vs-conflict resolution.
    ///
    /// **One backend transaction, and it has to be one.** Two obligations meet
    /// here. The entry's `acceptance_sequence` is claimed from
    /// `usage_acceptance_sequence` (the gear's DESIGN §3.7), and where the
    /// entry is an invalidation, `usage_records_one_invalidation_uniq` must
    /// refuse a second withdrawal of one target *atomically with the entry it
    /// admits* — the SPI says in as many words that a read followed by a write
    /// will not do, and why a gateway-side pre-read cannot substitute. So the
    /// claim and the insert commit or roll back together.
    ///
    /// **The at-most-one guarantee is conditional, and on the gateway.** The
    /// index is over `(invalidates, window_end)`, because a hypertable UNIQUE
    /// must contain the partition column. It catches two withdrawals of one
    /// target only while they share that target's `window_end` — which a
    /// faithful withdrawal does by construction, since it copies the covered
    /// period of the entry it withdraws, and which the Ingestion Gateway
    /// enforces upstream. A caller reaching this SPI directly with a mismatched
    /// period is not bound by that, and would get two accepted invalidations.
    /// Measured on a live container: same `window_end` → rejected; different
    /// `window_end` → both accepted. No hypertable-compatible index can do
    /// better, so this is recorded as a published-contract divergence rather
    /// than papered over with an in-transaction pre-read.
    ///
    /// `ON CONFLICT DO NOTHING` remains the dedup serialization authority: a
    /// concurrent same-key insert blocks on the in-progress speculative tuple
    /// until the winner commits — bounded by the connection's `lock_timeout`
    /// ([`crate::infra::storage::pool`]), so the wait cannot pin the connection
    /// indefinitely — then its `DO NOTHING` returns no row and it resolves
    /// absorb-vs-conflict against the now-visible committed row.
    ///
    /// This carries the per-row counters (dedup absorbed / idempotency conflict
    /// / compensation / backend error) so they are recorded exactly once per
    /// row whether the caller is [`RecordStore::create`] (single) or
    /// [`RecordStore::create_batch`] (per-row loop). The `insert.duration`
    /// histogram is deliberately NOT recorded here — the public methods time
    /// the whole call and tag it with the correct `mode`.
    async fn create_inner(
        &self,
        record: UsageRecord,
    ) -> Result<UsageRecord, UsageCollectorPluginError> {
        let mut conn = self.timed_acquire().await?;
        let mut tx = conn
            .begin()
            .await
            .map_err(|e| self.record_backend_error(&e))?;

        // 1. Claim this entry's acceptance_sequence inside the transaction that
        //    will insert it, so the two commit or roll back together.
        let acceptance_sequence = match claim_acceptance_sequence(
            &mut tx,
            record.tenant_id,
            record.gts_type_id.as_str(),
            1,
        )
        .await
        {
            Ok(seq) => seq,
            Err(e) => {
                rollback(tx).await;
                return Err(self.record_backend_error(&e));
            }
        };

        // 2. Insert, deduplicated on the 5-tuple UNIQUE. `RETURNING` yields the
        //    row only when we won the slot — `DO NOTHING` suppresses it on a
        //    conflict — so `Some` = fresh insert, `None` = a row with this
        //    5-tuple already exists. `ingested_at` is left to its DEFAULT and
        //    `entry_type` is generated, which is why sixteen of the seventeen
        //    [`RECORD_COLUMNS`] are bound here.
        let subject_id = record
            .subject_ref
            .as_ref()
            .map(usage_collector_sdk::SubjectRef::subject_id);
        let subject_type = record.subject_ref.as_ref().and_then(|s| s.subject_type());
        let metadata = metadata_map_to_jsonb(&record.metadata);
        // One helper for both columns, so the half-populated pair the read
        // direction refuses is unrepresentable on the way out too.
        let (invalidates, reason_code) = invalidation_to_row(record.invalidation.as_ref());
        let is_invalidation = invalidates.is_some();

        let attempted =
            sqlx::query_as::<_, UsageRecordRow>(AssertSqlSafe(SINGLE_INSERT_SQL.as_str()))
                .bind(record.id)
                .bind(record.tenant_id)
                .bind(record.gts_type_id.as_str())
                .bind(record.value)
                .bind(record.window_start)
                .bind(record.window_end)
                .bind(record.resource_ref.resource_id())
                .bind(record.resource_ref.resource_type())
                .bind(subject_id)
                .bind(subject_type)
                .bind(record.idempotency_key.as_str())
                .bind(invalidates)
                .bind(reason_code)
                .bind(record.origin.as_str())
                .bind(acceptance_sequence)
                .bind(metadata)
                .fetch_optional(&mut *tx)
                .await;

        let inserted = match attempted {
            Ok(inserted) => inserted,
            Err(e) => {
                // The failed statement aborted the transaction, so roll it back
                // before the diagnostic read below: a further query on a failed
                // transaction is refused with `25P02`, not answered.
                rollback(tx).await;
                let slots = invalidation_index_slots(&[&record]);
                return Err(self.map_insert_error(&mut conn, &e, &slots).await);
            }
        };

        if let Some(row) = inserted {
            // 3a. Won the slot — fresh insert. Commit it together with the
            //     sequence claim it was assigned.
            tx.commit()
                .await
                .map_err(|e| self.record_backend_error(&e))?;
            if is_invalidation {
                self.metrics.inc_compensation();
            }
            return record_row_to_model(row);
        }

        // 3b. Lost the slot — a row with this 5-tuple already exists. Read it
        //     and resolve absorb-vs-conflict. The read mutates nothing, and the
        //     rollback that follows releases the sequence value claimed in
        //     step 1, which is why an absorbed single-row retry leaves no gap
        //     (a batch's block claim does; see [`claim_acceptance_sequence`]).
        let select_sql = format!(
            "SELECT {RECORD_COLUMNS} FROM usage_records \
             WHERE tenant_id = $1 AND gts_type_id = $2 AND idempotency_key = $3 \
               AND window_start = $4 AND window_end = $5"
        );
        let stored = sqlx::query_as::<_, UsageRecordRow>(AssertSqlSafe(select_sql))
            .bind(record.tenant_id)
            .bind(record.gts_type_id.as_str())
            .bind(record.idempotency_key.as_str())
            .bind(record.window_start)
            .bind(record.window_end)
            .fetch_optional(&mut *tx)
            .await;
        rollback(tx).await;
        let stored = stored.map_err(|e| self.record_backend_error(&e))?;

        if let Some(row) = stored {
            self.resolve_dedup_hit(row, &record)
        } else {
            // Stale: the conflicting row's chunk was dropped by retention between
            // the conflicting insert and this read — a near-impossible race
            // against the retention boundary (the unique entry is dropped with
            // the chunk, so a retry now wins the freed slot as a fresh insert).
            // Return retryable Transient.
            self.metrics.inc_dedup_stale();
            Err(dedup_transient(
                &record,
                "conflicting record aged out during dedup resolution; retry",
            ))
        }
    }

    /// Resolve a dedup-key hit into absorb (stored row) vs `IdempotencyConflict`
    /// via [`canonical_equal`]. Called from the conflict branch of
    /// `create_inner` when an existing dedup slot's stored record is found.
    /// Increments the matching per-row counter: `dedup.absorbed` on an
    /// exact-equality absorb, `idempotency.conflict` on a canonical-field
    /// mismatch. A stored-metadata decode failure propagates as `Internal`
    /// rather than masquerading as a conflict.
    fn resolve_dedup_hit(
        &self,
        row: UsageRecordRow,
        record: &UsageRecord,
    ) -> Result<UsageRecord, UsageCollectorPluginError> {
        if canonical_equal(&row, record)? {
            // Exact-equality retry — silently absorb, returning the stored row.
            self.metrics.inc_dedup_absorbed();
            record_row_to_model(row)
        } else {
            self.metrics.inc_idempotency_conflict();
            Err(UsageCollectorPluginError::IdempotencyConflict {
                idempotency_key: record.idempotency_key.as_str().to_owned(),
                existing_id: row.id,
            })
        }
    }

    /// Insert all distinct-key representatives in one multi-row
    /// `INSERT … ON CONFLICT (5-tuple) DO NOTHING RETURNING`. The returned rows
    /// are exactly the slots we won — `DO NOTHING` suppresses any row whose
    /// `(tenant_id, gts_type_id, idempotency_key, window_start, window_end)`
    /// already exists — so the result maps each won [`DedupKey`] to its stored
    /// row. `reps` must be sorted by [`DedupKey`] so concurrent batches insert
    /// in one global order (deadlock-free), and `sequences` must be the
    /// acceptance-sequence values claimed for them, in the same order.
    ///
    /// Errors come back as the raw `sqlx::Error` rather than mapped: the caller
    /// holds the transaction that has to be rolled back before the mapping's
    /// diagnostic read can run.
    ///
    /// # Panics
    ///
    /// Never in practice: `sequences` is produced from `reps` by
    /// [`claim_batch_sequences`], one value per representative.
    async fn insert_records_on_conflict(
        tx: &mut sqlx::Transaction<'_, Postgres>,
        reps: &[&UsageRecord],
        sequences: &[i64],
    ) -> Result<HashMap<DedupKey, UsageRecordRow>, sqlx::Error> {
        if reps.is_empty() {
            return Ok(HashMap::new());
        }
        let cols = InsertColumns::build(reps, sequences);

        let rows = sqlx::query_as::<_, UsageRecordRow>(AssertSqlSafe(BATCH_INSERT_SQL.as_str()))
            .bind(&cols.ids)
            .bind(&cols.tenants)
            .bind(&cols.gts_type_ids)
            .bind(&cols.values)
            .bind(&cols.window_starts)
            .bind(&cols.window_ends)
            .bind(&cols.resource_ids)
            .bind(&cols.resource_types)
            .bind(&cols.subject_ids)
            .bind(&cols.subject_types)
            .bind(&cols.idem_keys)
            .bind(&cols.invalidates)
            .bind(&cols.reason_codes)
            .bind(&cols.origins)
            .bind(&cols.sequences)
            .bind(&cols.metadata)
            .fetch_all(&mut **tx)
            .await?;

        Ok(rows
            .into_iter()
            .map(|row| (row_dedup_key(&row), row))
            .collect())
    }

    /// For the not-won keys, read the existing `usage_records` row by its
    /// 5-tuple `(tenant_id, gts_type_id, idempotency_key, window_start,
    /// window_end)` — the batch analogue of the single path's conflict branch.
    /// Maps each key to `Stored` (row found → resolve absorb/conflict) or
    /// `Stale` (the conflicting row's chunk was dropped by retention between
    /// the conflicting insert and this read).
    async fn read_conflict_records(
        &self,
        tx: &mut sqlx::Transaction<'_, Postgres>,
        not_won: &[&UsageRecord],
    ) -> Result<HashMap<DedupKey, ConflictRead>, UsageCollectorPluginError> {
        let mut out: HashMap<DedupKey, ConflictRead> = HashMap::new();
        if not_won.is_empty() {
            return Ok(out);
        }

        let tenants: Vec<Uuid> = not_won.iter().map(|r| r.tenant_id).collect();
        let gtss: Vec<String> = not_won
            .iter()
            .map(|r| r.gts_type_id.as_str().to_owned())
            .collect();
        let keys: Vec<String> = not_won
            .iter()
            .map(|r| r.idempotency_key.as_str().to_owned())
            .collect();
        let starts: Vec<OffsetDateTime> = not_won.iter().map(|r| r.window_start).collect();
        let ends: Vec<OffsetDateTime> = not_won.iter().map(|r| r.window_end).collect();

        let select_sql = format!(
            "SELECT {RECORD_COLUMNS} FROM usage_records \
             WHERE (tenant_id, gts_type_id, idempotency_key, window_start, window_end) IN \
               (SELECT t1, t2, t3, t4, t5 \
                FROM UNNEST($1::uuid[], $2::text[], $3::text[], $4::timestamptz[], \
                            $5::timestamptz[]) \
                  AS t(t1, t2, t3, t4, t5))"
        );
        let rows = sqlx::query_as::<_, UsageRecordRow>(AssertSqlSafe(select_sql))
            .bind(&tenants)
            .bind(&gtss)
            .bind(&keys)
            .bind(&starts)
            .bind(&ends)
            .fetch_all(&mut **tx)
            .await
            .map_err(|e| self.record_backend_error(&e))?;

        let mut found: HashMap<DedupKey, UsageRecordRow> = HashMap::new();
        for row in rows {
            found.insert(row_dedup_key(&row), row);
        }

        // Every not-won key resolves to Stored (its conflicting row was read) or
        // Stale (the row's chunk was dropped by retention between the conflicting
        // insert and this read — the near-impossible retention-boundary race).
        // `reps` are distinct keys, so each `remove` is unambiguous.
        for r in not_won {
            let key = dedup_key(r);
            match found.remove(&key) {
                Some(row) => {
                    out.insert(key, ConflictRead::Stored(Box::new(row)));
                }
                None => {
                    out.insert(key, ConflictRead::Stale);
                }
            }
        }

        Ok(out)
    }

    /// Resolve every input row in original order against its authoritative
    /// record, recording the per-row counters exactly as the single path does.
    ///
    /// `inserted` is the set of slots this batch won — it *is* the answer to
    /// "did we win this key", so there is no separate `won` set to disagree
    /// with it. An earlier shape passed both, which made two states
    /// representable that the code cannot produce (a won key with no inserted
    /// row) and so two `Internal` arms that only a hand-built map could reach.
    fn resolve_batch(
        &self,
        records: &[UsageRecord],
        plan: &BatchPlan<'_>,
        inserted: &HashMap<DedupKey, UsageRecordRow>,
        conflict: &HashMap<DedupKey, ConflictRead>,
    ) -> Vec<Result<UsageRecord, UsageCollectorPluginError>> {
        let mut results = Vec::with_capacity(records.len());
        for (i, record) in records.iter().enumerate() {
            // Pre-rejected in-batch: an earlier row of this same batch already
            // withdraws this target, and both rows would have gone into one
            // multi-row INSERT where the index rejects the *statement* rather
            // than the row. See [`plan_batch`] — this check is not the
            // enforcement, only what keeps the outcome per-row.
            if let Some(&invalidated_by) = plan.duplicate_withdrawals.get(&i) {
                results.push(Err(duplicate_withdrawal_in_batch(record, invalidated_by)));
                continue;
            }
            let key = dedup_key(record);
            let outcome = match inserted.get(&key) {
                // We won this slot and this input row is its first occurrence:
                // the fresh insert.
                Some(row) if plan.first_index.get(&key) == Some(&i) => {
                    if record.invalidation.is_some() {
                        self.metrics.inc_compensation();
                    }
                    record_row_to_model(row.clone())
                }
                // We won the slot, but an earlier input row is its winner — so
                // this is an in-batch duplicate, resolved against the row we
                // just wrote exactly as the single path resolves a same-key hit.
                Some(row) => self.resolve_dedup_hit(row.clone(), record),
                None => match conflict.get(&key) {
                    Some(ConflictRead::Stored(row)) => {
                        // Clone the inner row directly; `*row.clone()` would
                        // round-trip through a throwaway `Box` allocation. The
                        // clone itself is required — a not-won key may be
                        // resolved by several input rows against the borrowed map.
                        self.resolve_dedup_hit((**row).clone(), record)
                    }
                    Some(ConflictRead::Stale) => {
                        self.metrics.inc_dedup_stale();
                        Err(dedup_transient(
                            record,
                            "conflicting record aged out during dedup resolution; retry",
                        ))
                    }
                    // Defensive: read_conflict_records populates every not-won key
                    // as Stored or Stale, so a missing entry is unreachable —
                    // surface it as retryable rather than as a silent success.
                    None => Err(dedup_transient(
                        record,
                        "conflicting record not found during dedup resolution; retry",
                    )),
                },
            };
            results.push(outcome);
        }
        results
    }

    /// Orchestrate one batch inside **one transaction**: claim an
    /// acceptance-sequence block per scope → insert (dedup on the 5-tuple
    /// UNIQUE) → read conflicts for the not-won keys → commit → resolve per row
    /// in input order. The insert's `RETURNING` rows are themselves the set of
    /// keys it claimed, so nothing else records that.
    ///
    /// The transaction is not decoration. `acceptance_sequence` is claimed here
    /// and inserted here, so the two must commit or roll back together; and the
    /// at-most-one-invalidation index has to refuse a second withdrawal
    /// atomically with the entry it admits, which the SPI is explicit about.
    ///
    /// **Two invalidations of one target that arrive together are handled
    /// before the insert, not by it** — see [`plan_batch`]. What the index
    /// still owns alone is the cross-call case, and there its violation aborts
    /// the whole statement: a batch carrying a withdrawal of an
    /// already-invalidated target fails as a whole with
    /// [`UsageCollectorPluginError::AlreadyInvalidated`] rather than yielding
    /// per-row outcomes. That residue is recorded as a divergence rather than
    /// hidden.
    async fn create_batch_inner(
        &self,
        records: &[UsageRecord],
    ) -> Result<Vec<Result<UsageRecord, UsageCollectorPluginError>>, UsageCollectorPluginError>
    {
        let plan = plan_batch(records);

        let mut conn = self.timed_acquire().await?;
        let mut tx = conn
            .begin()
            .await
            .map_err(|e| self.record_backend_error(&e))?;

        let sequences = match claim_batch_sequences(&mut tx, &plan.reps).await {
            Ok(sequences) => sequences,
            Err(e) => {
                rollback(tx).await;
                return Err(self.record_backend_error(&e));
            }
        };

        let inserted = match Self::insert_records_on_conflict(&mut tx, &plan.reps, &sequences).await
        {
            Ok(rows) => rows,
            Err(e) => {
                // The failed statement aborted the transaction; roll it back so
                // the mapping's diagnostic read has a usable connection.
                rollback(tx).await;
                let slots = invalidation_index_slots(&plan.reps);
                return Err(self.map_insert_error(&mut conn, &e, &slots).await);
            }
        };
        // `inserted` is the won set; there is no second copy of it to drift.
        let not_won: Vec<&UsageRecord> = plan
            .reps
            .iter()
            .copied()
            .filter(|r| !inserted.contains_key(&dedup_key(r)))
            .collect();
        let conflict = match self.read_conflict_records(&mut tx, &not_won).await {
            Ok(conflict) => conflict,
            Err(e) => {
                // Every other failure path here rolls back explicitly; `?` would
                // leave this one to the lazy drop-rollback alone.
                rollback(tx).await;
                return Err(e);
            }
        };
        tx.commit()
            .await
            .map_err(|e| self.record_backend_error(&e))?;

        Ok(self.resolve_batch(records, &plan, &inserted, &conflict))
    }
}

/// The sixteen per-column vectors one multi-row insert binds.
///
/// `sqlx` binds arrays, not rows, so the batch insert `UNNEST`s these back into
/// rows. Keeping them in one struct built by one function keeps the column
/// list, the `UNNEST` list and the bind order readable side by side instead of
/// spread across sixteen locals in the middle of the query.
struct InsertColumns {
    ids: Vec<Uuid>,
    tenants: Vec<Uuid>,
    gts_type_ids: Vec<String>,
    values: Vec<Decimal>,
    window_starts: Vec<OffsetDateTime>,
    window_ends: Vec<OffsetDateTime>,
    resource_ids: Vec<String>,
    resource_types: Vec<String>,
    subject_ids: Vec<Option<String>>,
    subject_types: Vec<Option<String>>,
    idem_keys: Vec<String>,
    invalidates: Vec<Option<Uuid>>,
    reason_codes: Vec<Option<String>>,
    origins: Vec<String>,
    sequences: Vec<i64>,
    metadata: Vec<String>,
}

impl InsertColumns {
    /// Pivot `reps` (plus the acceptance sequences claimed for them, in the
    /// same order) into per-column vectors.
    ///
    /// `metadata` is carried as `text[]` of JSON strings and cast `::jsonb`
    /// per-row in the query, to sidestep `jsonb[]` array encoding. The
    /// invalidation pair goes through [`invalidation_to_row`] rather than being
    /// read out of the record twice, so the two columns cannot drift apart.
    ///
    /// # Panics
    ///
    /// Never in practice: `sequences` comes from [`claim_batch_sequences`] over
    /// the same `reps`, so it is the same length. A shorter one would be a
    /// caller invariant break, and panicking beats silently writing a wrong
    /// acceptance sequence.
    fn build(reps: &[&UsageRecord], sequences: &[i64]) -> Self {
        assert_eq!(
            reps.len(),
            sequences.len(),
            "one acceptance sequence must be claimed per batch representative"
        );
        let mut cols = Self {
            ids: Vec::with_capacity(reps.len()),
            tenants: Vec::with_capacity(reps.len()),
            gts_type_ids: Vec::with_capacity(reps.len()),
            values: Vec::with_capacity(reps.len()),
            window_starts: Vec::with_capacity(reps.len()),
            window_ends: Vec::with_capacity(reps.len()),
            resource_ids: Vec::with_capacity(reps.len()),
            resource_types: Vec::with_capacity(reps.len()),
            subject_ids: Vec::with_capacity(reps.len()),
            subject_types: Vec::with_capacity(reps.len()),
            idem_keys: Vec::with_capacity(reps.len()),
            invalidates: Vec::with_capacity(reps.len()),
            reason_codes: Vec::with_capacity(reps.len()),
            origins: Vec::with_capacity(reps.len()),
            sequences: sequences.to_vec(),
            metadata: Vec::with_capacity(reps.len()),
        };
        for r in reps {
            let (invalidates, reason_code) = invalidation_to_row(r.invalidation.as_ref());
            cols.ids.push(r.id);
            cols.tenants.push(r.tenant_id);
            cols.gts_type_ids.push(r.gts_type_id.as_str().to_owned());
            cols.values.push(r.value);
            cols.window_starts.push(r.window_start);
            cols.window_ends.push(r.window_end);
            cols.resource_ids
                .push(r.resource_ref.resource_id().to_owned());
            cols.resource_types
                .push(r.resource_ref.resource_type().to_owned());
            cols.subject_ids.push(
                r.subject_ref
                    .as_ref()
                    .map(|sr| usage_collector_sdk::SubjectRef::subject_id(sr).to_owned()),
            );
            cols.subject_types.push(
                r.subject_ref
                    .as_ref()
                    .and_then(|sr| sr.subject_type())
                    .map(str::to_owned),
            );
            cols.idem_keys.push(r.idempotency_key.as_str().to_owned());
            cols.invalidates.push(invalidates);
            cols.reason_codes.push(reason_code.map(str::to_owned));
            cols.origins.push(r.origin.as_str().to_owned());
            cols.metadata
                .push(metadata_map_to_jsonb(&r.metadata).to_string());
        }
        cols
    }
}

/// Roll `tx` back now, logging a failure rather than propagating it.
///
/// Dropping a `Transaction` rolls it back too, but lazily — the `ROLLBACK` is
/// queued and sent when the connection is next used. Every caller here rolls
/// back precisely because it is about to use the connection again (for the
/// diagnostic read that names an existing invalidation) or is about to return
/// it to the pool after a failure, so "now" is the property that matters. A
/// rollback that itself fails says the connection is gone; the pool discards
/// it, and the caller's original error is the one worth returning.
async fn rollback(tx: sqlx::Transaction<'_, Postgres>) {
    if let Err(err) = tx.rollback().await {
        tracing::warn!(
            error = %err,
            "rolling back a usage-record write transaction failed"
        );
    }
}

/// Claim a contiguous block of `count` `acceptance_sequence` values for
/// `(tenant_id, gts_type_id)`, returning the block's **last** value — the block
/// is `[returned - count + 1, returned]`.
///
/// Strictly monotonic per scope, which is the gear's DESIGN §3.7 obligation. It
/// is **not** gapless and does not need to be: a batch claims one block per
/// scope up front and then commits whatever the dedup `ON CONFLICT` let it win,
/// so every value claimed for a slot it lost is spent without ever being
/// stored. (A single-row insert that loses its slot rolls back instead, so that
/// path leaves no gap.) Density is not the obligation and nothing reads the
/// sequence expecting it.
///
/// Runs inside the caller's transaction so the claim and the insert commit or
/// roll back together, and the counter row's lock is therefore held to commit.
///
/// **That lock is not the price of monotonicity — it is the price of ordered
/// visibility, and the distinction matters.** A plain Postgres `SEQUENCE` would
/// give strict monotonicity lock-free, with gaps this doc already declares
/// permitted. What it would *not* give is that claim order equals commit order:
/// the next claimer blocks on this row until the holder commits, so within a
/// scope a lower `acceptance_sequence` is always visible before a higher one.
/// The Feed Gateway serves pages "ordered by `acceptance_sequence` within each
/// `(tenant, gts_type)` scope" (`cpt-cf-usage-collector-fr-billing-usage-feed`,
/// the gear's `docs/DESIGN.md` §1.2 driver table), so a consumer that has read
/// past N must never afterwards see an N-1 commit. Swap this for a sequence and
/// the feed breaks silently. Scopes do not contend with each other.
///
/// That lock is also why `55P03 lock_not_available` belongs in the transient set
/// ([`crate::infra::storage::error`]): ingest now waits on a per-scope row on
/// every write, so timing out on a hot scope is an ordinary contention outcome
/// rather than a defect.
async fn claim_acceptance_sequence(
    tx: &mut sqlx::Transaction<'_, Postgres>,
    tenant_id: Uuid,
    gts_type_id: &str,
    count: i64,
) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar::<_, i64>(
        "INSERT INTO usage_acceptance_sequence (tenant_id, gts_type_id, next_value) \
         VALUES ($1, $2, $3) \
         ON CONFLICT (tenant_id, gts_type_id) \
         DO UPDATE SET next_value = usage_acceptance_sequence.next_value + $3 \
         RETURNING next_value",
    )
    .bind(tenant_id)
    .bind(gts_type_id)
    .bind(count)
    .fetch_one(&mut **tx)
    .await
}

/// Claim one `acceptance_sequence` per representative, aligned to `reps` order.
///
/// `reps` are sorted by [`DedupKey`], whose first two components are exactly
/// the sequence's scope, so same-scope representatives are contiguous and the
/// scopes are visited in one global order — the same discipline that keeps the
/// dedup tuple locks deadlock-free, applied to the counter rows.
///
/// One statement per scope rather than one per entry: the block claim advances
/// the counter by `count` and returns the block's last value, so a batch of `n`
/// entries in one scope costs one round trip and takes the counter's row lock
/// once.
async fn claim_batch_sequences(
    tx: &mut sqlx::Transaction<'_, Postgres>,
    reps: &[&UsageRecord],
) -> Result<Vec<i64>, sqlx::Error> {
    let mut out: Vec<i64> = Vec::with_capacity(reps.len());
    for (start, end) in scope_runs(reps) {
        // `end - start` is a slice length, so it fits an i64 on every target
        // this builds for; saturating keeps the conversion total regardless.
        let count = i64::try_from(end - start).unwrap_or(i64::MAX);
        let last = claim_acceptance_sequence(
            tx,
            reps[start].tenant_id,
            reps[start].gts_type_id.as_str(),
            count,
        )
        .await?;
        out.extend(sequence_block(last, count));
    }
    Ok(out)
}

/// The half-open `[start, end)` runs of `reps` that share one
/// `(tenant_id, gts_type_id)` acceptance-sequence scope.
///
/// Split out of [`claim_batch_sequences`] because it is the half that can be
/// wrong without a database noticing: it assumes `reps` is sorted by
/// [`DedupKey`], whose first two components *are* the scope, so same-scope
/// representatives are contiguous. A run that ended early would claim two
/// blocks for one scope — still monotonic, so no constraint would object —
/// and a run that ran on would hand one scope's values to another's entries.
fn scope_runs(reps: &[&UsageRecord]) -> Vec<(usize, usize)> {
    let mut runs = Vec::new();
    let mut start = 0usize;
    while start < reps.len() {
        let scope = (reps[start].tenant_id, reps[start].gts_type_id.as_str());
        let mut end = start + 1;
        while end < reps.len() && (reps[end].tenant_id, reps[end].gts_type_id.as_str()) == scope {
            end += 1;
        }
        runs.push((start, end));
        start = end;
    }
    runs
}

/// Expand a claimed block into the values it covers.
///
/// [`claim_acceptance_sequence`] returns the block's **last** value, because
/// that is what `RETURNING next_value` yields after adding `count`; the block
/// is `[last - count + 1, last]`. Getting the off-by-one wrong here reuses one
/// scope's sequence value or skips one, and nothing in the schema can object —
/// the counter row is the sole authority and the ledger does not re-check what
/// it hands out (`migrations/0001_init.sql`).
fn sequence_block(last: i64, count: i64) -> Vec<i64> {
    let first = last - count + 1;
    (0..count).map(|offset| first + offset).collect()
}

/// The `(tenant_id, invalidation target, window_end)` slots `records` would
/// occupy in `usage_records_one_invalidation_uniq`.
///
/// Empty for ordinary measurements, which the partial index does not cover.
/// The last two components are the index's own key, so a lookup on them finds
/// exactly the row that refused a write; `tenant_id` rides along so the lookup
/// can carry the tenant predicate every other query in this module carries.
fn invalidation_index_slots(records: &[&UsageRecord]) -> Vec<(Uuid, Uuid, OffsetDateTime)> {
    records
        .iter()
        .filter_map(|r| {
            r.invalidation
                .as_ref()
                .map(|i| (r.tenant_id, i.target, r.window_end))
        })
        .collect()
}

/// Read back the accepted invalidation occupying one of `slots`, as
/// `(withdrawn target, withdrawing entry id)`.
///
/// A batch can carry several already-invalidated targets and every match is a
/// genuine conflict, so this reports **one of possibly several**. The order is
/// pinned to the stored entry's `id` so a given failure names the same target
/// on every run rather than whichever row the planner reached first.
///
/// Diagnostic only — it runs after a write was already rejected, never before
/// one; see [`PgRecordStore::map_insert_error`].
///
/// `tenant_id` is bound even though the index key `(invalidates, window_end)`
/// already determines the row: this module enforces tenant isolation by hand
/// with parameterized `tenant_id` predicates (see the module header), and a
/// query that returns another row's `id` into a caller-visible error is the
/// last place to take an exception to that. The exception would in fact be
/// safe — `invalidates` is a `UUIDv5` over a tenant-scoped 5-tuple, so a
/// cross-tenant match is not constructible — but that is a supporting mechanism
/// a future reader would have to re-derive, and one bind is cheaper than the
/// argument.
async fn find_existing_invalidation(
    conn: &mut sqlx::PgConnection,
    slots: &[(Uuid, Uuid, OffsetDateTime)],
) -> Result<Option<(Uuid, Uuid)>, sqlx::Error> {
    if slots.is_empty() {
        return Ok(None);
    }
    let tenants: Vec<Uuid> = slots.iter().map(|(tenant, _, _)| *tenant).collect();
    let ids: Vec<Uuid> = slots.iter().map(|(_, target, _)| *target).collect();
    let ends: Vec<OffsetDateTime> = slots.iter().map(|(_, _, end)| *end).collect();
    sqlx::query_as::<_, (Uuid, Uuid)>(
        "SELECT t.target, r.id FROM usage_records AS r \
         JOIN UNNEST($1::uuid[], $2::uuid[], $3::timestamptz[]) \
           AS t(tenant_id, target, window_end) \
           ON r.tenant_id = t.tenant_id \
          AND r.invalidates = t.target \
          AND r.window_end = t.window_end \
         ORDER BY r.id \
         LIMIT 1",
    )
    .bind(&tenants)
    .bind(&ids)
    .bind(&ends)
    .fetch_optional(&mut *conn)
    .await
}

/// Extract a single order-field value from a row as its cursor-key string.
///
/// Inverse of [`cursor_key_to_bind`](crate::infra::storage::query::keyset::cursor_key_to_bind):
/// the `uuid` columns render via [`Uuid::to_string`], the `timestamptz` bounds
/// as RFC 3339, and the text columns as-is — each the spelling that helper
/// parses back for the field's declared kind, so a minted boundary re-binds to
/// the value it was read from.
///
/// **The arms are [`usage_collector_sdk::KEYSET_SAFE_RECORD_FIELDS`], and that
/// is the whole rule.** A key that is not on that list is one the SDK does not
/// carry as an attribute of every entry *in its own right* — either it can be
/// absent, or it is derived from an attribute that can be. For the first kind a
/// `NULL` compares as `NULL` inside the row-value tuple and silently drops the
/// row; for the second the SDK simply gives no guarantee to rest a keyset on.
/// `entry_type` is the case that makes the distinction necessary: it is present
/// on every entry, and still not keyset-safe. `None` here therefore means "this
/// order field is not a keyset key on the row", which is a refusal to mint
/// rather than a missing value.
///
/// This is one half of a two-sided map: [`record_column`] resolves an order
/// field to the column the `ORDER BY` and the keyset tuple are rendered from,
/// and this resolves the same field to the value the boundary carries. Nothing
/// in the type system couples them — the two-sided coupling test in
/// `record_store_tests.rs` does, by naming the value each arm must produce
/// from a row whose columns are all distinguishable.
fn record_row_key(row: &UsageRecordRow, field: &str) -> Option<String> {
    match field {
        "id" => Some(row.id.to_string()),
        "window_start" => row.window_start.format(&Rfc3339).ok(),
        "window_end" => row.window_end.format(&Rfc3339).ok(),
        "tenant_id" => Some(row.tenant_id.to_string()),
        "resource_id" => Some(row.resource_id.clone()),
        "resource_type" => Some(row.resource_type.clone()),
        "origin" => Some(row.origin.clone()),
        _ => None,
    }
}

/// The gateway's fingerprint of the query a page is read under, or a refusal.
///
/// The SPI guarantees `query.filter_hash` "on this method": the gateway
/// populates it for every `list_usage_records` dispatch, first page included,
/// "so an implementation of this method never has to handle `None`, and an
/// absent value is a gateway breach rather than a case to paper over". This is
/// where that `None` is turned into the refusal, for the two places the value
/// is load-bearing — the continuation guard, whose `Option`-to-`Option`
/// comparison would otherwise *pass* with both sides absent, and the mint,
/// where [`encode_next_cursor`] now takes a `&str` precisely so the decision
/// cannot be made by accident.
fn require_filter_hash(query: &ODataQuery) -> Result<&str, String> {
    query.filter_hash.as_deref().ok_or_else(|| {
        "list_usage_records dispatched without query.filter_hash (gateway breach)".to_owned()
    })
}

/// Build the point-lookup SQL and the binds that go with it: the asked-for
/// `id` at `$1`, conjoined with the caller's compiled PDP scope.
///
/// **The scope is the whole filter beyond the `id`.** This path carries no
/// caller-supplied `$filter`, and nothing above the SPI re-checks the row that
/// comes back, so a lookup selecting on `id` alone would answer with any row
/// whose `id` a caller can name — an existence oracle over every tenant's
/// entries.
///
/// The scope itself is translated by [`translate_scope`], which every read path
/// shares: it owns both allowlist gates, the bind numbering, and the
/// parentheses that let the fragment be conjoined safely. What is left here is
/// statement assembly, and that is all this function should ever grow.
///
/// Binds start at `$2` because the `id` occupies `$1`; [`PgRecordStore::get`]
/// binds the `id` first, before these, for that reason. The seeding matches
/// [`PgRecordStore::list`] and [`PgRecordStore::aggregate`], which seed the same
/// way behind their leading `gts_id` bind. Only the ordered bind values are
/// returned, not the [`SqlCtx`]: the statement is finished, so there is nothing
/// left for a caller to legitimately push.
///
/// # Errors
///
/// Propagates [`translate_scope`]'s refusal unchanged. A scope that fails to
/// translate and is dropped instead leaves `WHERE id = $1` — a translation
/// failure turned into an authorization bypass — so this returns `Err` rather
/// than a partial statement, and it is the only producer of this path's SQL.
fn build_get_sql(scope: &ast::Expr) -> Result<(String, Vec<SqlBind>), String> {
    // `$1` is the `id`; every scope bind therefore starts at `$2`.
    let mut ctx = SqlCtx::new(2);
    let fragment = translate_scope(scope, &mut ctx)?;
    Ok((
        format!("SELECT {RECORD_COLUMNS} FROM usage_records WHERE id = $1 AND {fragment}"),
        ctx.binds,
    ))
}

/// Build the keyset page's SQL and the binds that go with it, and be the only
/// producer of this path's statement.
///
/// The `WHERE` is assembled in bind order: the meter at `$1`, the covered
/// period's two bounds at `$2` and `$3`, then the caller's composed `$filter`,
/// the metadata side channel, and the keyset continuation. Identifiers come
/// from the [`record_column`] allowlist, the static [`RECORD_COLUMNS`], and the
/// literal column names the shared builders write — `r.gts_type_id` and
/// `r.window_end` from [`push_meter_and_range_clauses`], `r.metadata ->>` from
/// [`push_metadata_filter_clauses`]. None is caller input, and every
/// caller-derived value is bound, save the clamped page size, which is a `u64`
/// this function renders itself into the `LIMIT`.
///
/// **Selection reads the covered-period end alone** — `from <= window_end <
/// to`, per `cpt-cf-usage-collector-adr-window-end-selection` — and the
/// predicate never names `window_start`. This is a rule about which bound is
/// read, not an optimization: overlap would select an entry into two adjacent
/// ranges and containment would drop one out of both, and a period longer than
/// the range is the case that tells the three apart. `time_range` is a typed
/// parameter and is deliberately not reachable through `query.filter`, which
/// the gateway refuses to let name either bound.
///
/// **No withdrawal exclusion, and none belongs here.** `aggregate` excludes a
/// withdrawn pair because a total that counts one is a wrong total; that is a
/// derived view and this is the ledger. The SPI says it in as many words for
/// this method — "a withdrawn pair MUST likewise be returned as persisted
/// here" — and [`PgRecordStore::get`] carries no exclusion either
/// (`cpt-cf-usage-collector-adr-append-only-invalidation`).
///
/// `query.order` is rendered as handed, and the keyset tuple is built from the
/// same `query.order` in the same field order a few lines above, so the two
/// cannot name different columns or disagree about direction. That the tuple
/// and the `ORDER BY` agree is what makes a continuation resume from the
/// boundary the previous page ended on.
///
/// # Errors
///
/// Returns an error string when the composed `$filter` cannot be translated
/// (propagated from [`translate_scope`] unchanged, and never dropped — a
/// dropped scope leaves the read unscoped), when a cursor is backward, carries
/// a different fingerprint or a different order, when an order or keyset field
/// is off the allowlist or not keyset-safe, or when the order is empty or
/// mixed-direction. A caller must propagate it: this is the only producer of
/// the statement, so a partial one is never returned in its place.
fn build_list_sql(
    gts_type_id: &MeterTypeId,
    time_range: TimeRange,
    query: &ODataQuery,
    metadata_filter: &[MetadataFilter],
    limit: u64,
) -> Result<(String, Vec<SqlBind>), String> {
    let mut ctx = SqlCtx::new(1);
    let mut clauses: Vec<String> = Vec::new();

    // The meter scope and the covered-period range, from the one spelling both
    // read paths share. Read rather than transcribed, so `aggregate` cannot
    // drift from this on the ADR obligation both are held to.
    push_meter_and_range_clauses(gts_type_id, time_range, &mut ctx, &mut clauses);

    // The composed `$filter`, through the seam every read path shares. What
    // arrives is the caller's filter `And`-composed with the compiled PDP
    // scope, or the scope alone when the caller supplied none — either way one
    // expression, and in the second case the scope's own outermost node, which
    // for a multi-constraint grant is an `Or`.
    // [`translate_scope`] returns a parenthesized fragment,
    // which is what makes pushing it into a `join(" AND ")` safe; the
    // `convert_expr_to_filter_node` + `translate_record_filter` pair this call
    // replaces returned a bare one.
    if let Some(expr) = query.filter() {
        clauses.push(translate_scope(expr, &mut ctx)?);
    }

    // Metadata side-channel: AND across filters, OR within one filter's
    // values (see [`push_metadata_filter_clauses`]).
    push_metadata_filter_clauses(metadata_filter, &mut ctx, &mut clauses);

    if let Some(cursor) = query.cursor.as_ref() {
        // Forward-only: the keyset operator is derived from the sort
        // direction, not from `cursor.d`, so a backward cursor would silently
        // page forward. Reject it fail-closed.
        ensure_forward_cursor(cursor)?;
        // Resolved rather than compared as an `Option`: `cursor.f == None` and
        // `query.filter_hash == None` are equal, so the old comparison passed
        // on exactly the breach it exists to catch and left the gateway to
        // refuse the token on page two.
        let filter_hash = require_filter_hash(query)?;
        if cursor.f.as_deref() != Some(filter_hash) {
            return Err("cursor filter hash mismatch".to_owned());
        }
        // The cursor's keys (`cursor.k`) are positional, bound against the
        // live `query.order` columns below. If the order changed between pages
        // at the same arity, old keys would bind to new columns — silently
        // wrong pagination. The cursor carries the signed sort tokens
        // (`cursor.s`) precisely to detect this, mirroring the guard above.
        if !query.order.equals_signed_tokens(&cursor.s) {
            return Err("cursor sort order mismatch".to_owned());
        }
        let order_pairs: Vec<(&str, bool)> = query
            .order
            .0
            .iter()
            .map(|key| (key.field.as_str(), matches!(key.dir, SortDir::Asc)))
            .collect();
        clauses.push(keyset_predicate(
            &order_pairs,
            &cursor.k,
            record_column,
            |name| UsageRecordFilterField::from_name(name).map(|f| f.kind()),
            is_keyset_safe_record_field,
            &mut ctx,
        )?);
    }

    let order_sql = render_order_by(&query.order, record_column)?;

    Ok((
        format!(
            "SELECT {RECORD_COLUMNS} FROM {} WHERE {} ORDER BY {order_sql} LIMIT {}",
            // Called, never spelled: with a literal here the shared constant
            // would be decorative, and an alias change would red a test that
            // then gets "fixed" by editing the literal back.
            ledger_from_clause(),
            clauses.join(" AND "),
            // The look-ahead: one row past the page, so the page can tell "this
            // is the last page" from "there is another" without a second query.
            // [`build_list_page`] consumes it, and its `rows.len() > page_size`
            // is the other half of this convention — the `+ 1` here is what
            // makes that `>` correct rather than `>=`.
            limit.saturating_add(1),
        ),
        ctx.binds,
    ))
}

/// Turn the look-ahead read into the page the caller gets: drop the extra row,
/// mint the continuation from the last in-page row, and map the rest.
///
/// **`query.filter_hash` is carried into `next_cursor.f` verbatim**, which is
/// the one SPI requirement with no compiler backstop of its own —
/// `require_cursor_fingerprint` calls it "the one requirement in this gear's
/// Plugin SPI that gives an implementor no compiler error — a plugin written
/// before it recompiles clean and paginates exactly once". The gateway
/// recomputes the same string from the follow-up request and refuses a token
/// carrying a different one, or none. Nothing here interprets the value: it is
/// opaque, and its shape is the gateway's to change.
///
/// The boundary values are read in `query.order` field order, one per key, so a
/// caller ordering by `id` gets its keys in that order rather than in a
/// canonical one this function assumed.
///
/// # Precondition
///
/// `limit >= 1`, and `rows` is the look-ahead read
/// [`build_list_sql`] asked for — at most `limit + 1` rows. The two halves of
/// that convention are the `+ 1` there and the `rows.len() > page_size` here:
/// the `>` is correct precisely because the extra row was requested, and would
/// have to be `>=` if it were not. [`effective_page_size`] is what floors the
/// limit to 1; this signature does not, so a caller passing `0` truncates the
/// whole page away and reaches the `Err` below.
///
/// # Errors
///
/// Returns [`UsageCollectorPluginError::Internal`] when `query.filter_hash` is
/// absent (a gateway breach, not a case to paper over), when an order field is
/// not a keyset key on the row, when the cursor cannot be encoded, or when a
/// stored row cannot be mapped to the SDK model — and, distinctly from all
/// four, `"non-empty page lost its tail"` when the precondition above is
/// broken, which is a bug in this crate rather than anything a caller of the
/// SPI can provoke.
fn build_list_page(
    mut rows: Vec<UsageRecordRow>,
    query: &ODataQuery,
    limit: u64,
) -> Result<ODataPage<UsageRecord>, UsageCollectorPluginError> {
    let page_size = usize::try_from(limit).unwrap_or(usize::MAX);

    // Look-ahead row present -> a next page exists; drop it before mapping.
    let has_next = rows.len() > page_size;
    if has_next {
        rows.truncate(page_size);
    }

    let next_cursor = if has_next {
        let last = rows
            .last()
            .ok_or_else(|| UsageCollectorPluginError::internal("non-empty page lost its tail"))?;
        let filter_hash =
            require_filter_hash(query).map_err(UsageCollectorPluginError::internal)?;
        let keys = query
            .order
            .0
            .iter()
            .map(|key| {
                record_row_key(last, &key.field).ok_or_else(|| {
                    UsageCollectorPluginError::internal(format!(
                        "order field `{}` is not a keyset key on the row",
                        key.field
                    ))
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
        Some(
            encode_next_cursor(&query.order, &keys, filter_hash)
                .map_err(UsageCollectorPluginError::internal)?,
        )
    } else {
        None
    };

    let items = rows
        .into_iter()
        .map(record_row_to_model)
        .collect::<Result<Vec<_>, _>>()?;

    Ok(ODataPage::new(
        items,
        PageInfo {
            next_cursor,
            prev_cursor: None,
            limit,
        },
    ))
}

/// The dedup identity, mirroring the `usage_records_dedup_uniq` UNIQUE
/// `(tenant_id, gts_type_id, idempotency_key, window_start, window_end)` — the
/// same five inputs the entry `id` is a `UUIDv5` projection of
/// (`cpt-cf-usage-collector-adr-record-identity-derivation`), so the two can
/// never disagree about what one entry is.
///
/// The two covered-period bounds enter as
/// [`canonical_period_bound`](usage_collector_sdk::canonical_period_bound)
/// renders them, not as `OffsetDateTime`s. That is the SDK's own canonical
/// microsecond form, shared with the identity derivation, and it is what makes
/// an in-memory key built from a caller's value match the key built from what
/// Postgres returned: `timestamptz` stores microseconds, so sub-µs nanos and a
/// non-UTC offset do not survive the round trip, and the rendering flattens
/// both. Using the SDK's function rather than a local truncation means a
/// precision change in one crate cannot silently diverge them.
type DedupKey = (Uuid, String, String, String, String);

/// Build the [`DedupKey`] for an incoming record.
fn dedup_key(record: &UsageRecord) -> DedupKey {
    (
        record.tenant_id,
        record.gts_type_id.as_str().to_owned(),
        record.idempotency_key.as_str().to_owned(),
        canonical_period_bound(record.window_start),
        canonical_period_bound(record.window_end),
    )
}

/// Build the [`DedupKey`] for a stored row, so an `INSERT … RETURNING` result
/// and an incoming record map to the same key (both bounds canonicalized).
fn row_dedup_key(row: &UsageRecordRow) -> DedupKey {
    (
        row.tenant_id,
        row.gts_type_id.clone(),
        row.idempotency_key.clone(),
        canonical_period_bound(row.window_start),
        canonical_period_bound(row.window_end),
    )
}

/// Log a dedup-path invariant break (an `Internal`, "this should never happen"
/// condition) at `error` with the record's identifiers, then return the matching
/// [`UsageCollectorPluginError::Internal`]. Centralizing the log + build keeps
/// each silent break observable (DESIGN §Observability puts unbounded
/// identifiers in logs, not metric labels) without inflating the hot ingest
/// path's control flow.
fn dedup_invariant_break(record: &UsageRecord, msg: &'static str) -> UsageCollectorPluginError {
    tracing::error!(
        tenant_id = %record.tenant_id,
        gts_type_id = %record.gts_type_id.as_str(),
        idempotency_key = %record.idempotency_key.as_str(),
        "{msg}"
    );
    UsageCollectorPluginError::internal(msg)
}

/// Log a retryable dedup-path transient at `warn` with the record's identifiers,
/// then return the matching [`UsageCollectorPluginError::Transient`]. The
/// degraded path is self-healing on retry but must still surface at `warn` so an
/// operator can see it (DESIGN §Observability).
fn dedup_transient(record: &UsageRecord, msg: &'static str) -> UsageCollectorPluginError {
    tracing::warn!(
        tenant_id = %record.tenant_id,
        gts_type_id = %record.gts_type_id.as_str(),
        idempotency_key = %record.idempotency_key.as_str(),
        "{msg}"
    );
    UsageCollectorPluginError::transient(msg)
}

/// Deterministic plan for a batch insert.
///
/// `reps` are the first-occurrence representative records, one per distinct
/// dedup key, **sorted** by [`DedupKey`] so concurrent batches take the
/// 5-tuple-UNIQUE conflict locks — and the per-scope acceptance-sequence row
/// locks — in one global order (deadlock-free).
/// `first_index` maps each key to the input index of its first occurrence — the
/// only row that can win the slot; later same-key rows resolve against the
/// winner's record, exactly as the single-row path resolves a same-key hit.
/// `duplicate_withdrawals` names the input rows pre-rejected because an earlier
/// row of the same batch already withdraws their target.
struct BatchPlan<'a> {
    reps: Vec<&'a UsageRecord>,
    first_index: HashMap<DedupKey, usize>,
    duplicate_withdrawals: HashMap<usize, Uuid>,
}

/// Collapse a batch to its distinct dedup keys (first occurrence wins), sorted
/// for a stable lock order, and pre-reject a second in-batch withdrawal of one
/// target. Pure — no DB. `reps` borrow from `records`, which outlives the plan,
/// so no record is cloned onto the plan.
///
/// **The in-batch withdrawal check is not the at-most-one enforcement.** The
/// enforcement is `usage_records_one_invalidation_uniq`, which is the only
/// mechanism that can be atomic with the entry it admits and the only one that
/// covers a second withdrawal arriving in a *different* call. This check exists
/// because two withdrawals of one target inside a single batch would both land
/// in one multi-row `INSERT`, where the index rejects the whole **statement**
/// rather than the offending row — costing every other entry in the batch its
/// outcome, when the SPI asks for exactly one accepted and the other rejected,
/// per-record and in input order. Do not delete the index as redundant with
/// this, and do not delete this as redundant with the index.
///
/// Within a batch the rule can be applied in its true form — at most one
/// withdrawal per target, whatever period each carries — because the whole
/// batch is in hand. The index can only approximate it, over
/// `(invalidates, window_end)`; see [`PgRecordStore::create_inner`].
///
/// A repeat of the *same* withdrawal (same derived `id`, so all five dedup
/// attributes match) is an idempotent retry, not a second withdrawal, and is
/// left to the dedup path to absorb.
fn plan_batch(records: &[UsageRecord]) -> BatchPlan<'_> {
    let mut first_index: HashMap<DedupKey, usize> = HashMap::new();
    let mut reps: Vec<(DedupKey, &UsageRecord)> = Vec::new();
    let mut withdrawn: HashMap<Uuid, Uuid> = HashMap::new();
    let mut duplicate_withdrawals: HashMap<usize, Uuid> = HashMap::new();
    for (i, record) in records.iter().enumerate() {
        if let Some(invalidation) = record.invalidation.as_ref() {
            if let Some(&first_id) = withdrawn.get(&invalidation.target) {
                if first_id != record.id {
                    duplicate_withdrawals.insert(i, first_id);
                    continue;
                }
            } else {
                withdrawn.insert(invalidation.target, record.id);
            }
        }
        let key = dedup_key(record);
        if let std::collections::hash_map::Entry::Vacant(slot) = first_index.entry(key.clone()) {
            slot.insert(i);
            reps.push((key, record));
        }
    }
    reps.sort_by(|a, b| a.0.cmp(&b.0));
    BatchPlan {
        reps: reps.into_iter().map(|(_, r)| r).collect(),
        first_index,
        duplicate_withdrawals,
    }
}

/// Build the [`UsageCollectorPluginError::AlreadyInvalidated`] for a batch row
/// that an earlier row of the same batch already withdrew, logging it the way
/// the other batch-path rejections are logged.
///
/// `invalidated_by` is the earlier row's entry id. A record reaching here
/// always carries an invalidation — [`plan_batch`] only records rows that do —
/// so the `None` arm is a plugin invariant break rather than a caller shape.
fn duplicate_withdrawal_in_batch(
    record: &UsageRecord,
    invalidated_by: Uuid,
) -> UsageCollectorPluginError {
    let Some(invalidation) = record.invalidation.as_ref() else {
        return dedup_invariant_break(
            record,
            "batch row pre-rejected as a duplicate withdrawal carries no invalidation",
        );
    };
    tracing::warn!(
        tenant_id = %record.tenant_id,
        gts_type_id = %record.gts_type_id.as_str(),
        target = %invalidation.target,
        invalidated_by = %invalidated_by,
        "rejecting a second withdrawal of one entry submitted in the same batch"
    );
    UsageCollectorPluginError::AlreadyInvalidated {
        id: invalidation.target,
        invalidated_by,
    }
}

/// Total `create_batch` attempts: one initial try plus two retries. A bounded
/// in-process retry so a rare deadlock victim self-heals transparently instead
/// of bubbling an `Err(Transient)` to the host (see [`with_retry`]).
const MAX_BATCH_ATTEMPTS: u32 = 3;

/// Deterministic pre-jitter backoff base for the `attempt`-th retry (1-based).
/// A short exponential — 5 ms, 10 ms, … — because a deadlock victim can retry
/// almost immediately: the transaction that survived the deadlock has already
/// committed or aborted by the time Postgres aborts the victim, so the
/// contended dedup and acceptance-sequence locks are free. The shift is
/// saturated so the schedule can never overflow regardless of how
/// `MAX_BATCH_ATTEMPTS` grows.
fn batch_retry_backoff_base(attempt: u32) -> Duration {
    let shift = attempt.saturating_sub(1).min(6);
    Duration::from_millis(5u64 << shift)
}

/// Full jitter: a uniformly random duration in `[0, upper]`. Decorrelates the
/// retry instants of batches that deadlocked on the same dedup locks, so they
/// do not all wake and re-contend at the same moment. An `upper` of at most
/// 1 ms is returned unchanged (nothing meaningful to spread).
fn full_jitter(upper: Duration) -> Duration {
    let upper_ms = u64::try_from(upper.as_millis()).unwrap_or(u64::MAX);
    if upper_ms <= 1 {
        return upper;
    }
    let jitter_ms = rand::rng().random_range(0..=upper_ms);
    Duration::from_millis(jitter_ms)
}

/// Backoff before the `attempt`-th retry (1-based: `batch_retry_backoff(1)`
/// precedes the first retry): the exponential [`batch_retry_backoff_base`] with
/// **full jitter** applied so concurrent deadlock victims spread across the
/// window instead of retrying in lockstep (thundering herd).
fn batch_retry_backoff(attempt: u32) -> Duration {
    full_jitter(batch_retry_backoff_base(attempt))
}

/// Retry predicate for [`with_retry`] around `create_batch`: retry **only** an
/// outer [`UsageCollectorPluginError::Transient`].
///
/// The deadlock victim surfaces as an outer `Transient` — `create_batch_inner`
/// runs the whole batch in one transaction, and that transaction rolled back,
/// so the attempt left nothing behind. Serialization failures (`40001`), lock
/// timeouts (`55P03`, which ingest can now hit on the per-scope
/// acceptance-sequence row) and connection blips collapse to the same bucket
/// inside the storage helpers, and all are safe to re-run for this idempotent
/// batch. `Internal`, `IdempotencyConflict`, `AlreadyInvalidated` and the other
/// typed domain outcomes are non-retryable and returned unchanged — an
/// already-withdrawn target does not become withdrawable by waiting. Per-row
/// `Transient` outcomes carried inside an `Ok(vec)` are deliberately not seen
/// here — the batch as a whole succeeded, so the loop never inspects them.
fn is_retryable_batch_error(err: &UsageCollectorPluginError) -> bool {
    matches!(err, UsageCollectorPluginError::Transient { .. })
}

/// Run `operation`, retrying while `should_retry` accepts its error, for up to
/// `max_attempts` total invocations; sleep `backoff(attempt)` before the
/// `attempt`-th retry. Returns the first `Ok`, or — once retries are exhausted
/// or the error is non-retryable — the last `Err` unchanged.
///
/// `on_retry(attempt, &err)` is invoked exactly once before each retry — after a
/// retryable failure and before the backoff sleep — with the failed 1-based
/// `attempt` number and the error it failed with. It is the observability seam
/// (log + retry counter): the combinator stays generic and DB-free, so the
/// caller supplies the tracing/metrics side effects. It never fires on a
/// first-attempt success or on a returned (non-retried) error, so a retry can be
/// told apart from a bubbled transient failure.
///
/// Generic and DB-free so the retry mechanics are unit-tested without a
/// database at all. `operation` is an `Fn` invoked fresh each attempt (it
/// borrows the caller's input, so re-invocation is allocation-free), which is
/// exactly the right unit of retry for `create_batch_inner`: every attempt
/// acquires a fresh connection and opens a fresh transaction on it, so a failed
/// attempt leaves neither a claimed acceptance sequence nor a half-written
/// batch behind. There is zero happy-path cost — on success the loop runs the
/// operation once and neither sleeps, allocates a backoff, nor calls
/// `on_retry`.
async fn with_retry<T, E, Op, Fut>(
    max_attempts: u32,
    backoff: impl Fn(u32) -> Duration,
    should_retry: impl Fn(&E) -> bool,
    on_retry: impl Fn(u32, &E),
    operation: Op,
) -> Result<T, E>
where
    Op: Fn() -> Fut,
    Fut: Future<Output = Result<T, E>>,
{
    let mut attempt: u32 = 1;
    loop {
        match operation().await {
            Ok(value) => return Ok(value),
            Err(err) => {
                if attempt >= max_attempts || !should_retry(&err) {
                    return Err(err);
                }
                on_retry(attempt, &err);
                // `toolkit::tokio` is the crate's tokio re-export (matching
                // `toolkit::tokio::spawn` / `select!` elsewhere in this gear);
                // `tokio` itself is only a dev-dependency.
                toolkit::tokio::time::sleep(backoff(attempt)).await;
                attempt += 1;
            }
        }
    }
}

/// Outcome of reading the existing `usage_records` row for a not-won key.
enum ConflictRead {
    /// The conflicting row exists — resolve absorb vs conflict against it.
    Stored(Box<UsageRecordRow>),
    /// The conflicting row's chunk was dropped by retention between the
    /// conflicting insert and the read → retryable `Transient`.
    Stale,
}

/// Compare the caller-supplied canonical fields of a stored row against an
/// incoming record (DESIGN §3.6: absorb vs conflict).
///
/// This runs only once the dedup 5-tuple has already matched, so it answers one
/// question: does the rest of what the caller supplied match too? If it does,
/// the submission is an exact retry and is absorbed; if it does not, one
/// idempotency key is being used for two different entries and that is an
/// `IdempotencyConflict`.
///
/// The compared set is `id`, the covered period, `value`, `resource_ref`,
/// `subject_ref`, `origin`, the invalidation pair, and `metadata` — that is,
/// every caller-supplied field except `tenant_id`, `gts_type_id` and
/// `idempotency_key`, which are the remaining three dedup components and so
/// have already matched (see the second NOTE). The two server-managed columns
/// are excluded outright: `acceptance_sequence`, which this plugin assigns, and
/// `ingested_at`, the insert time. `metadata` is compared after decoding the
/// stored `jsonb` back to the typed map; the invalidation pair is compared
/// through [`invalidation_to_row`], the same helper the insert binds through,
/// so the write and the comparison cannot spell the pair differently.
///
/// NOTE — the invalidation *target* is compared here even though it is
/// deliberately excluded from the identity derivation. The SDK explains that
/// the exclusion and this inclusion are one mechanism: excluded from the
/// identity, reusing one idempotency key across an entry and its withdrawal
/// collapses them onto a single dedup slot; included here, that collapse is
/// loud (the pair compares unequal and conflicts) instead of silently absorbing
/// the withdrawal as a duplicate of the entry it meant to withdraw.
///
/// NOTE — five fields are settled by the dedup key before this function runs,
/// and they are not treated alike. `tenant_id`, `gts_type_id` and
/// `idempotency_key` are three of the key's five components and are **not**
/// compared: the row was fetched by that key, so comparing them would restate
/// the lookup. `window_start` and `window_end` are the other two, and `id` is a
/// `UUIDv5` projection of all five — those three **are** compared, as
/// deliberate defensive tautologies rather than fail-closed guards on caller
/// data, kept so a corrupted stored row surfaces as an `IdempotencyConflict`
/// rather than a silent absorb. The asymmetry is a judgement about cost, not
/// about correctness: the three cheap `String`/`Uuid` restatements buy nothing
/// the lookup did not, while a mismatching `id` or bound is the shape a
/// corrupted row would actually take. The bounds are compared through the same
/// canonical rendering the key uses, so a stored value that round-tripped
/// through `timestamptz` cannot compare unequal to the caller's on precision or
/// offset alone.
///
/// No external sanction is cited for comparing `id`, because there is none to
/// cite: the SPI here is rustdoc ([`usage_collector_sdk::UsageCollectorPluginV1`]),
/// and it says nothing either way about a plugin verifying the derived
/// identity. The argument above stands on its own — the comparison cannot
/// change the outcome for well-formed data, and it turns one shape of stored
/// corruption into a loud `IdempotencyConflict` instead of a silent absorb.
///
/// # Errors
///
/// Returns [`UsageCollectorPluginError::Internal`] when the stored `metadata`
/// `jsonb` cannot be decoded back to the typed map — a stored-data invariant
/// break, distinct from a canonical-field mismatch (which returns `Ok(false)`).
fn canonical_equal(
    row: &UsageRecordRow,
    incoming: &UsageRecord,
) -> Result<bool, UsageCollectorPluginError> {
    let stored_metadata = metadata_jsonb_to_map(row.metadata.clone())?;
    Ok(row.id == incoming.id
        && row.value == incoming.value
        && canonical_period_bound(row.window_start)
            == canonical_period_bound(incoming.window_start)
        && canonical_period_bound(row.window_end) == canonical_period_bound(incoming.window_end)
        && row.resource_id == incoming.resource_ref.resource_id()
        && row.resource_type == incoming.resource_ref.resource_type()
        && row.subject_id.as_deref()
            == incoming
                .subject_ref
                .as_ref()
                .map(usage_collector_sdk::SubjectRef::subject_id)
        && row.subject_type.as_deref()
            == incoming.subject_ref.as_ref().and_then(|s| s.subject_type())
        && row.origin == incoming.origin.as_str()
        && (row.invalidates, row.reason_code.as_deref())
            == invalidation_to_row(incoming.invalidation.as_ref())
        && stored_metadata == incoming.metadata)
}

/// The presence guard a grouped dimension needs so a row missing that
/// dimension is dropped rather than folded into a `NULL` bucket.
///
/// Built from `select_expr` — the very string the `GROUP BY` ordinal points
/// at — so it is by construction the exact negation of "the grouping
/// expression yields `NULL`" and cannot drift from the expression it guards.
/// For a metadata dimension that reads `r.metadata ->> $N IS NOT NULL`, whose
/// bound key is the one [`dimension_select_expr`] already pushed; the key is
/// therefore bound once, not twice.
///
/// `IS NOT NULL` over `->>` rather than the containment operator `?`: they
/// disagree on a key present with JSON `null`, where `?` is true and `->>` is
/// `NULL`, and it is `NULL` that decides the bucket. (Where containment really
/// is wanted the unambiguous spelling is `jsonb_exists(r.metadata, $N)`; a bare
/// `?` collides with the placeholder syntax of some drivers.)
///
/// `None` for the three columns the schema declares `NOT NULL` — a guard on
/// them would be dead SQL. The match is exhaustive on purpose: a new
/// [`AggregationDimension`] variant fails to compile here, next to the decision
/// it needs.
///
/// **The consequence is deliberate: grouped buckets need not sum to the
/// ungrouped total.** Dropping the row is what the SDK both documents and
/// does: `models.rs:1587-1592` says rows without a subject "are excluded from
/// the grouping", and `contract/reference.rs:801` reads a grouped metadata key
/// as `row.metadata.get(key).cloned()`, so an absent key yields `None` and the
/// row joins no bucket.
///
/// **`DIVERGENCES.md` §G is not the citation for this**, though it is where a
/// reader will look. §G records the question as still *open* — "DESIGN says
/// nothing about the case", "that is an argument, not a ruling ... and it is a
/// spec owner's to make", "do not write the check first". The ruling is this
/// port's Task 18, which rewrites §G; until then the two SDK sites above are
/// what this guard conforms to (`DIVERGENCES.md` §G, resolved by Task 18).
///
/// It was already true of the two subject dimensions; guarding the metadata one
/// makes it uniform rather than accidental.
fn dimension_presence_guard(dim: &AggregationDimension, select_expr: &str) -> Option<String> {
    match dim {
        AggregationDimension::SubjectId
        | AggregationDimension::SubjectType
        | AggregationDimension::Metadata(_) => Some(format!("{select_expr} IS NOT NULL")),
        AggregationDimension::TenantId
        | AggregationDimension::ResourceId
        | AggregationDimension::ResourceType => None,
    }
}

/// Build the pushed-down aggregate statement and its binds, in placeholder
/// order.
///
/// ```sql
/// SELECT <dimension exprs…>, <fold expr>
/// FROM usage_records r
/// WHERE r.gts_type_id = $1 AND r.window_end >= $2 AND r.window_end < $3
///   AND <withdrawal exclusion>
///   [AND <translated $filter>] [AND <metadata filters>] [AND <presence guards>]
/// [GROUP BY 1, 2, …] [LIMIT MAX_AGGREGATION_BUCKETS + 1]
/// ```
///
/// Every line but the last two comes from a shared builder rather than from a
/// second transcription here: [`ledger_from_clause`] is the `FROM`,
/// [`push_meter_and_range_clauses`] the meter and the covered-period range,
/// [`withdrawal_exclusion_clause`] the two obligations a withdrawn pair places
/// on every fold, and [`push_metadata_filter_clauses`] the side channel. The
/// range predicate especially: read from one place, it cannot drift onto
/// `window_start` in this path alone, which would fail `window-end-selection`
/// and `quantity-round-trip` at once (`DIVERGENCES.md` §F).
///
/// The `$filter` goes through [`translate_scope`], which parenthesizes what it
/// returns. What arrives is the caller's filter `And`-composed with the
/// compiled PDP scope, **or the scope alone when the caller supplied none** —
/// and a multi-constraint grant compiles to a disjunction, so a bare fragment
/// pushed into a `join(" AND ")` would read as `(P AND A) OR B` and answer rows
/// outside the grant. Nothing at this layer can tell how many constraints the
/// PDP returned, so the parenthesized spelling is the only safe one.
///
/// **No slot of `query` beyond `filter` and `filter_hash`'s absence is read.**
/// This path paginates nothing and mints no cursor, so `query.cursor`,
/// `query.filter_hash` and `query.limit` reach neither the statement nor the
/// binds — the SPI is explicit that an aggregate implementation must not read
/// the fingerprint slot, and the gateway assigns this call none.
///
/// Identifiers all come from closed enum matches
/// ([`fold_select_expr`], [`dimension_select_expr`], `record_column`); the only
/// caller-derived values — a grouped metadata key, `$filter` operands, side
/// channel keys and values — are bound (`$N`).
///
/// # Errors
///
/// Returns the translation error string when the composed filter names a field
/// outside the allowlist, uses an unsupported operator, or carries a value that
/// cannot be bound.
fn build_aggregate_sql(
    gts_type_id: &MeterTypeId,
    time_range: TimeRange,
    fold: AggregationFold,
    query: &ODataQuery,
    metadata_filter: &[MetadataFilter],
    group_by: &[AggregationDimension],
) -> Result<(String, Vec<SqlBind>), String> {
    let mut ctx = SqlCtx::new(1);
    let mut clauses: Vec<String> = Vec::new();

    push_meter_and_range_clauses(gts_type_id, time_range, &mut ctx, &mut clauses);

    // The one rule this path applies that the ledger paths do not: an
    // invalidation entry contributes nothing, and neither does the record an
    // accepted invalidation names. Unconditional, because both obligations hold
    // under every fold.
    clauses.push(withdrawal_exclusion_clause().to_owned());

    if let Some(expr) = query.filter() {
        clauses.push(translate_scope(expr, &mut ctx)?);
    }

    push_metadata_filter_clauses(metadata_filter, &mut ctx, &mut clauses);

    // Dimension SELECT exprs in `GROUP BY` order, each binding its metadata key
    // at most once, each contributing a presence guard when its column can be
    // `NULL`.
    let mut select_dims: Vec<String> = Vec::with_capacity(group_by.len());
    for dim in group_by {
        let expr = dimension_select_expr(dim, &mut ctx);
        if let Some(guard) = dimension_presence_guard(dim, &expr) {
            clauses.push(guard);
        }
        select_dims.push(expr);
    }

    // SELECT list = the dimension exprs, then the fold. With no dimensions the
    // SELECT is the fold alone, which is what makes the no-grouping case one
    // aggregate row rather than none.
    let dim_count = select_dims.len();
    let mut select_parts = select_dims;
    select_parts.push(fold_select_expr(fold).to_owned());
    let select_list = select_parts.join(", ");

    // `GROUP BY` by ordinal (1..=k), so a bound metadata expr is written once
    // and the second reference cannot renumber its placeholder. Omitted
    // entirely with no dimensions: `GROUP BY` with an empty ordinal list is a
    // syntax error, and grouping by nothing is what a bare aggregate already
    // does.
    let group_by_sql = if dim_count == 0 {
        String::new()
    } else {
        let ordinals = (1..=dim_count)
            .map(|n| n.to_string())
            .collect::<Vec<_>>()
            .join(", ");
        format!(" GROUP BY {ordinals}")
    };

    Ok((
        format!(
            "SELECT {select_list} FROM {} WHERE {}{group_by_sql}{}",
            // Called, never spelled: with a literal here the shared constant
            // would be decorative, and an alias change would red a test that
            // then gets "fixed" by editing the literal back.
            ledger_from_clause(),
            clauses.join(" AND "),
            aggregate_limit_clause(dim_count),
        ),
        ctx.binds,
    ))
}

/// Read one aggregate result row into a bucket: `dim_count` dimension columns
/// as the key, then the folded value.
///
/// Split out so the caller is a 1:1 `map` over the fetched rows. That is the
/// shape the no-grouping case needs: with no `GROUP BY` the statement is a bare
/// aggregate and `PostgreSQL` answers exactly one row, which becomes exactly one
/// bucket with an empty key — the shape a conforming plugin owes, where an
/// empty bucket list is not. A short circuit that answered `[]` for an empty
/// fetch would need a branch this shape has nowhere to put.
///
/// A `NULL` dimension reads as the empty string. With the presence guards
/// [`dimension_presence_guard`] emits, the nullable dimensions cannot produce
/// one; the fallback stands for the columns that never can.
///
/// # Errors
///
/// Returns [`UsageCollectorPluginError::Internal`] when a column cannot be
/// decoded at its expected type — `TEXT` for a dimension, `numeric` for the
/// fold.
fn aggregate_bucket(
    row: &PgRow,
    dim_count: usize,
) -> Result<AggregationBucket, UsageCollectorPluginError> {
    let mut key = Vec::with_capacity(dim_count);
    for i in 0..dim_count {
        let dim = row.try_get::<Option<String>, _>(i).map_err(|e| {
            UsageCollectorPluginError::internal(format!(
                "aggregate dimension column {i} read failed: {e}"
            ))
        })?;
        key.push(dim.unwrap_or_default());
    }
    let value = row
        .try_get::<Option<BigDecimal>, _>(dim_count)
        .map_err(|e| {
            UsageCollectorPluginError::internal(format!(
                "aggregate value column {dim_count} read failed: {e}"
            ))
        })?;
    Ok(AggregationBucket { key, value })
}

#[async_trait]
impl RecordStore for PgRecordStore {
    /// **This path deliberately does not retry, and the asymmetry with
    /// [`Self::create_batch`] is a decision rather than an omission.**
    ///
    /// Task 9 made the wait structural — every single-row write now takes the
    /// per-scope `usage_acceptance_sequence` row lock before it inserts — so a
    /// `55P03` on a hot scope is an ordinary outcome here, not a rarity. It is
    /// still returned unretried, because a `Transient` lifts to
    /// `ServiceUnavailable` at the dispatch boundary and reaches the caller as
    /// a 503 with a `Retry-After` slot: the client already holds the one record,
    /// and re-submitting it is cheap and exactly idempotent.
    ///
    /// A batch is the opposite trade: re-submitting it is expensive for the
    /// caller, and its value is a vector of per-row outcomes that cannot be
    /// partially returned — so absorbing a transient in-process is worth the
    /// jittered milliseconds, where here it would only duplicate a retry the
    /// caller can make just as well.
    ///
    /// Note what is *not* part of this argument: a retry costs no pooled
    /// connection. `conn` is a local of [`Self::create_inner`] and
    /// `create_batch_inner`, so it is dropped and returned to the pool when
    /// that `async fn` returns — before [`with_retry`] reaches `on_retry` or
    /// its backoff sleep. Neither path holds a connection across a wait.
    // @cpt-flow:cpt-cf-uc-plugin-seq-ingest-dedup:p2
    async fn create(&self, record: UsageRecord) -> Result<UsageRecord, UsageCollectorPluginError> {
        // Time the whole single-row call; the per-row counters live in
        // `create_inner` so they count once regardless of single-vs-batch.
        let t = Instant::now();
        let result = self.create_inner(record).await;
        self.metrics
            .record_insert(InsertMode::Single, t.elapsed().as_secs_f64());
        result
    }

    // @cpt-flow:cpt-cf-uc-plugin-seq-ingest-batch:p2
    async fn create_batch(
        &self,
        records: Vec<UsageRecord>,
    ) -> Result<Vec<Result<UsageRecord, UsageCollectorPluginError>>, UsageCollectorPluginError>
    {
        if records.is_empty() {
            tracing::warn!(
                "create_usage_records called with an empty batch (host-contract breach)"
            );
            return Err(UsageCollectorPluginError::internal(
                "create_usage_records called with an empty batch (host-contract breach)",
            ));
        }

        // Per-row dedup semantics, input order, and per-row metrics are
        // preserved by `create_batch_inner`; the multi-row write replaces the
        // former N+1 per-row loop (DESIGN cpt-cf-uc-plugin-seq-ingest-batch).
        //
        // Wrap the whole call in a bounded retry: on an outer `Transient` (the
        // classic ABBA deadlock victim aborted as `40P01`, a serialization
        // failure `40001`, a `55P03` lock timeout on a hot scope's
        // acceptance-sequence row, or a connection blip) re-run the operation up
        // to `MAX_BATCH_ATTEMPTS` times. Each attempt acquires a fresh
        // connection and opens a fresh transaction on it (`create_batch_inner`
        // does both), so a rolled-back attempt leaves no state behind — not even
        // the acceptance-sequence block it had claimed. Re-running is safe: that
        // transaction is atomic and the dedup keys make it idempotent, so a
        // re-run either re-claims the same slots or absorbs/conflicts against
        // the now-committed survivor. `Ok(vec)` is never retried — per-row
        // `Transient` outcomes inside it are the host's to handle (the batch as
        // a whole succeeded), and retrying them would be a correctness bug.
        let n = records.len();
        // Time the whole operation including any retries, recorded once
        // regardless of outcome (matches the single-call behaviour). On the
        // happy path the loop runs `create_batch_inner` exactly once.
        let t = Instant::now();
        let result = with_retry(
            MAX_BATCH_ATTEMPTS,
            batch_retry_backoff,
            is_retryable_batch_error,
            |attempt, err| {
                // Make the retry observable: a distinct warn + counter so a
                // self-healed deadlock victim can be told apart from a returned
                // transient error, which moves this counter not at all (most
                // move the backend-error counter instead; `map_insert_error`'s
                // could-not-name-the-invalidation arm moves none).
                tracing::warn!(
                    attempt,
                    max_attempts = MAX_BATCH_ATTEMPTS,
                    error = %err,
                    "retrying usage-record batch write after transient backend error"
                );
                self.metrics.inc_batch_retry();
            },
            || self.create_batch_inner(&records),
        )
        .await;
        self.metrics
            .record_insert(InsertMode::Batch, t.elapsed().as_secs_f64());
        if result.is_ok() {
            // `n` is a row count: convert via `try_from` (no `as` cast),
            // saturating an implausibly huge batch to `u32::MAX` before f64.
            self.metrics
                .record_batch_rows(f64::from(u32::try_from(n).unwrap_or(u32::MAX)));
        }
        result
    }

    /// Point lookup by `id`, intersected with the caller's compiled PDP scope.
    ///
    /// # Errors
    ///
    /// Returns [`UsageCollectorPluginError::UsageRecordNotFound`] when no row
    /// satisfies both the `id` and the scope — the two cases are one answer, on
    /// purpose — [`UsageCollectorPluginError::Internal`] when the scope cannot
    /// be translated or a stored row cannot be mapped, and the mapped backend
    /// error when the query itself fails.
    async fn get(
        &self,
        id: Uuid,
        scope: &ast::Expr,
    ) -> Result<UsageRecord, UsageCollectorPluginError> {
        // Lookup by the public `id`. This relies on a one-record-per-`id`
        // contract, which the hypertable schema cannot enforce on its own — a
        // `UNIQUE` there must include the `window_end` partition column, so only
        // the composite PK `(id, window_end)` is enforced. `fetch_optional`
        // therefore returns the first matching row.
        //
        // `id` is a `UUIDv5` over the 5-tuple dedup identity
        // `(tenant_id, gts_type_id, idempotency_key, window_start, window_end)`
        // (`cpt-cf-usage-collector-adr-record-identity-derivation`), which is
        // exactly this plugin's dedup identity — the same five inputs
        // `usage_records_dedup_uniq` is built over — so each stored row carries
        // a distinct `id` and `WHERE id = $1` matches at most one row.
        //
        // **No `invalidates` predicate belongs in this query, and none ever
        // will.** The asymmetry with the fold is deliberate, not an oversight
        // waiting to be tidied up: `aggregate` excludes a withdrawn pair
        // because a total that counts a withdrawn entry is a wrong total, and
        // that is a derived view. This is the ledger itself. The SPI states it
        // outright — a withdrawn pair MUST be returned as persisted, and a
        // plugin MUST NOT withhold a withdrawn entry from this path as a
        // kindness — because hiding either half destroys the audit trail the
        // append-only model exists to keep. Nor is this path a special case:
        // the SPI puts `list_usage_records` under the same obligation in as
        // many words — "a withdrawn pair MUST likewise be returned as
        // persisted here" — and `list` below carries no exclusion either. A
        // consumer that wants the netted view has what it needs: an
        // invalidation names its target (`UsageRecord::invalidation`), so the
        // fold happens on the reader's side. Neither `invalidates IS NULL` nor
        // an `entry_type` restriction is a kindness here; both are data loss
        // (`cpt-cf-usage-collector-adr-append-only-invalidation`).
        //
        // The scope is translated before a connection is acquired, so a scope
        // that cannot be rendered never reaches the pool and never reads a row.
        let (sql, binds) = build_get_sql(scope).map_err(UsageCollectorPluginError::internal)?;
        let mut q = sqlx::query_as::<_, UsageRecordRow>(AssertSqlSafe(sql)).bind(id);
        for b in &binds {
            q = bind_one(q, b);
        }
        let mut conn = self.timed_acquire().await?;
        let row = q
            .fetch_optional(&mut *conn)
            .await
            .map_err(|err| self.record_backend_error(&err))?;

        match row {
            Some(row) => record_row_to_model(row),
            // One arm for both "no such entry" and "exists, but outside your
            // scope": the scope is part of the `WHERE`, so a withheld row is
            // already indistinguishable from an absent one by the time we get
            // here. Nothing may be logged, counted or timed that would tell
            // them apart — that distinction *is* the existence oracle.
            None => Err(UsageCollectorPluginError::UsageRecordNotFound { id }),
        }
    }

    /// Keyset-paginated ledger read over one meter and one covered-period
    /// range, with the caller's composed `$filter`, the metadata side channel
    /// and an optional cursor.
    ///
    /// The statement is [`build_list_sql`]'s and the page is
    /// [`build_list_page`]'s; what is left here is the round trip between
    /// them. Both halves are pure, so both are tested without a database. That
    /// matters here because the fingerprint obligation below has no compiler
    /// backstop and no visible effect until page two: a unit test is what makes
    /// the mint observable at the moment it happens, rather than a page later.
    ///
    /// Selection reads the covered-period end alone, `from <= window_end <
    /// to`; entries are returned as persisted, withdrawn pairs included; and
    /// `query.filter_hash` is carried into `next_cursor.f` verbatim. Each of
    /// the three is stated where it is enforced rather than only here.
    ///
    /// # Errors
    ///
    /// Returns [`UsageCollectorPluginError::Internal`] when the statement
    /// cannot be built (an untranslatable `$filter`, an order or keyset field
    /// off the allowlist, a refused cursor) or the page cannot be assembled (an
    /// absent `query.filter_hash`, an order field that is not a keyset key, a
    /// stored row that cannot be mapped), and the mapped backend error when the
    /// query itself fails.
    // @cpt-flow:cpt-cf-uc-plugin-seq-list-keyset:p2
    async fn list(
        &self,
        gts_type_id: MeterTypeId,
        time_range: TimeRange,
        query: &ODataQuery,
        metadata_filter: &[MetadataFilter],
    ) -> Result<ODataPage<UsageRecord>, UsageCollectorPluginError> {
        // Time the full raw-list call and count the request. The drop-timer
        // records the histogram on every return — including the validation
        // error arms below — not just on success.
        let _timer =
            OpDurationGuard::start(Arc::clone(&self.metrics), TimedOp::Query(QueryKind::Raw));
        self.metrics.inc_query_request(QueryKind::Raw);

        // Defense-in-depth: clamp the caller's `$top` to `MAX_PAGE_SIZE` so a
        // value that slipped past the core gateway's `$top` cap can never drive
        // an unbounded `LIMIT n+1 ... fetch_all`.
        let limit = effective_page_size(query.limit, DEFAULT_PAGE_SIZE);

        // Built before a connection is acquired, so a query that cannot be
        // rendered never reaches the pool and never reads a row.
        let (sql, binds) = build_list_sql(&gts_type_id, time_range, query, metadata_filter, limit)
            .map_err(UsageCollectorPluginError::internal)?;

        let mut q = sqlx::query_as::<_, UsageRecordRow>(AssertSqlSafe(sql));
        for b in &binds {
            q = bind_one(q, b);
        }
        let mut conn = self.timed_acquire().await?;
        let rows = q
            .fetch_all(&mut *conn)
            .await
            .map_err(|e| self.record_backend_error(&e))?;

        // `_timer` records `query.duration` on drop (success and error alike).
        build_list_page(rows, query, limit)
    }

    /// Pushed-down fold over one meter's entries in one covered-period range,
    /// optionally grouped.
    ///
    /// The statement is [`build_aggregate_sql`]'s; the rules it encodes are
    /// documented there. Two properties are this method's rather than the
    /// builder's:
    ///
    /// - **The statement is built before a connection is acquired**, so a query
    ///   that cannot be rendered never reaches the pool and never reads a row.
    /// - **Each fetched row becomes exactly one bucket**
    ///   ([`aggregate_bucket`]), so an empty `group_by` — a bare aggregate with
    ///   no `GROUP BY`, which `PostgreSQL` answers with exactly one row — yields
    ///   the single empty-keyed bucket the SPI asks for rather than an empty
    ///   bucket list. `COUNT` over an empty selection is `Some(0)` for the same
    ///   reason: it is `SELECT COUNT(*)`'s own answer, not a special case here
    ///   ([`usage_collector_sdk::AggregationBucket::value`]).
    ///
    /// The dimension columns read positionally as `Option<String>` and the fold
    /// at index `k` as `Option<BigDecimal>` — arbitrary precision, so a wide
    /// `SUM` cannot overflow on decode.
    ///
    /// **`LATEST` materializes before it picks.** Its expression is
    /// `(ARRAY_AGG(r.value ORDER BY …))[1]`, so `PostgreSQL` builds a group's
    /// values into an array before taking the head. Peak state is **O(rows
    /// scanned), not O(largest group)**: under a `HashAggregate` plan every
    /// group's array is live at once, and only a sorted `GroupAggregate` gives
    /// the weaker bound — the planner chooses.
    /// [`aggregate_limit_clause`] offers no protection, because it bounds
    /// groups and never the rows within one; the only row bound is the
    /// `time_range`, which is a request parameter. A `LATEST` meter read over a
    /// wide window is therefore a server-side allocation sized by caller input.
    /// Recorded rather than reformulated: any alternative needs `EXPLAIN`
    /// against a real planner rather than reasoning.
    ///
    /// # Errors
    ///
    /// Returns [`UsageCollectorPluginError::Internal`] when the composed filter
    /// cannot be translated, the query fails, or a result column cannot be
    /// decoded at its expected type; a pool-acquisition failure surfaces as
    /// whatever `timed_acquire` classifies it as.
    // @cpt-flow:cpt-cf-uc-plugin-seq-query-aggregated:p2
    async fn aggregate(
        &self,
        gts_type_id: MeterTypeId,
        time_range: TimeRange,
        fold: AggregationFold,
        query: &ODataQuery,
        metadata_filter: &[MetadataFilter],
        group_by: &[AggregationDimension],
    ) -> Result<AggregationResult, UsageCollectorPluginError> {
        // Time the full aggregated-query call and count the request. The
        // drop-timer records the histogram on every return, not just success.
        let _timer = OpDurationGuard::start(
            Arc::clone(&self.metrics),
            TimedOp::Query(QueryKind::Aggregated),
        );
        self.metrics.inc_query_request(QueryKind::Aggregated);

        let (sql, binds) = build_aggregate_sql(
            &gts_type_id,
            time_range,
            fold,
            query,
            metadata_filter,
            group_by,
        )
        .map_err(UsageCollectorPluginError::internal)?;

        let mut q = sqlx::query(AssertSqlSafe(sql));
        for b in &binds {
            q = bind_one_query(q, b);
        }
        let mut conn = self.timed_acquire().await?;
        let rows = q
            .fetch_all(&mut *conn)
            .await
            .map_err(|e| self.record_backend_error(&e))?;

        // One bucket per row, in the order `PostgreSQL` emitted them. The
        // `map` is what keeps the no-grouping case honest: no branch on
        // `group_by.is_empty()` exists to answer an empty bucket list with.
        //
        // `group_by.len()` is the same count the SELECT list was built from —
        // [`build_aggregate_sql`] pushes exactly one dimension expression per
        // element — but nothing here observes that: no unit test executes a
        // statement, and `PgRow` cannot be built off a connection. Handing the
        // decoder a different count survives every test in the crate, and so
        // would a short circuit placed *after* the fetch. Task 15's integration
        // tests are where those are caught.
        //
        // A short circuit placed where one would actually be written — above
        // the acquire, to skip the query — is a different matter and is
        // covered: `the_ungrouped_fold_still_reaches_the_pool` requires the
        // ungrouped fold to reach the pool.
        let buckets = rows
            .iter()
            .map(|row| aggregate_bucket(row, group_by.len()))
            .collect::<Result<Vec<_>, _>>()?;

        // `_timer` records `query.duration` on drop (success and error alike).
        Ok(AggregationResult { buckets })
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "record_store_tests.rs"]
mod record_store_tests;
