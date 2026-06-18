//! Postgres-backed [`RecordStore`] over the `usage_records` hypertable.
//!
//! All operations — `create` / `create_batch` / `get` / `list` / `aggregate` /
//! `deactivate` — are real `sqlx`.

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use rust_decimal::Decimal;
use sqlx::pool::PoolConnection;
use sqlx::{Connection, PgPool, Postgres, Row};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use toolkit_odata::filter::{FilterField, convert_expr_to_filter_node};
use toolkit_odata::{ODataQuery, Page as ODataPage, PageInfo, SortDir};
use uuid::Uuid;

use usage_collector_sdk::{
    AggregationBucket, AggregationDimension, AggregationResult, AggregationSpec, MetadataFilter,
    UsageCollectorPluginError, UsageRecord, UsageRecordFilterField, UsageTypeGtsId,
};

use crate::domain::ports::RecordStore;
use crate::infra::metrics::{ErrorClass, InsertMode, Metrics, OpDurationGuard, QueryKind, TimedOp};
use crate::infra::storage::entity::UsageRecordRow;
use crate::infra::storage::error::{acquire_error_clears_readiness, map_sqlx_err};
use crate::infra::storage::mapper::{
    gts_id_str, metadata_jsonb_to_map, metadata_map_to_jsonb, record_row_to_model,
};
use crate::infra::storage::query::aggregate::{agg_select_expr, dimension_select_expr};
use crate::infra::storage::query::keyset::{
    encode_next_cursor, ensure_forward_cursor, keyset_predicate, render_order_by,
};
use crate::infra::storage::query::translate::{
    SqlBind, SqlCtx, bind_one, bind_one_query, record_column, translate_record_filter,
};

/// Default page size when the caller omits `$top` (`query.limit`).
const DEFAULT_PAGE_SIZE: u64 = 100;

/// Column list for every `usage_records` SELECT / RETURNING, in
/// [`UsageRecordRow`] field order. A static const (never caller input), so
/// `sqlx::query_as::<_, UsageRecordRow>` decodes positionally without risk of
/// SQL injection.
const RECORD_COLUMNS: &str = "uuid, tenant_id, gts_id, value, created_at, resource_id, \
     resource_type, subject_id, subject_type, idempotency_key, corrects_id, status, metadata, \
     ingested_at";

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
}

impl PgRecordStore {
    /// Build a store over an existing connection pool.
    #[must_use]
    pub fn new(pool: PgPool, metrics: Arc<Metrics>) -> Self {
        Self { pool, metrics }
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
                // `ready` recovers once the pool serves a connection again).
                self.metrics.set_ready(true);
                Ok(conn)
            }
            Err(e) => {
                // Clear readiness only on a connectivity-class failure so the
                // `uc.timescaledb.ready == 0` alert fires on a live outage but
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

    /// Core single-row insert path: atomic dedup serialization via `usage_dedup`,
    /// INSERT to `usage_records`, and lost-the-race absorb-vs-conflict resolution.
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

        // 1. Claim the dedup slot. The usage_dedup 3-tuple PK is the atomic
        //    serialization authority: a concurrent same-key insert blocks on the
        //    row lock and resolves on commit. DO NOTHING + RETURNING 1
        //    distinguishes "won the slot" (Some) from "already held" (None).
        let won = sqlx::query_scalar::<_, i32>(
            "INSERT INTO usage_dedup \
             (tenant_id, gts_id, idempotency_key, record_uuid, record_created_at) \
             VALUES ($1, $2, $3, $4, $5) \
             ON CONFLICT (tenant_id, gts_id, idempotency_key) DO NOTHING \
             RETURNING 1",
        )
        .bind(record.tenant_id)
        .bind(gts_id_str(&record.gts_id))
        .bind(record.idempotency_key.as_str())
        .bind(record.uuid)
        .bind(record.created_at)
        .fetch_optional(&mut *tx)
        .await
        .map_err(|e| self.record_backend_error(&e))?;

        if won.is_some() {
            // 2a. Won the slot — insert the record. We own the dedup key, so the
            //     4-tuple unique cannot conflict; a missing RETURNING row is an
            //     invariant break.
            let insert_sql = format!(
                "INSERT INTO usage_records \
                 (uuid, tenant_id, gts_id, value, created_at, resource_id, resource_type, \
                  subject_id, subject_type, idempotency_key, corrects_id, metadata) \
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12) \
                 RETURNING {RECORD_COLUMNS}"
            );
            let subject_id = record
                .subject_ref
                .as_ref()
                .map(usage_collector_sdk::SubjectRef::subject_id);
            let subject_type = record.subject_ref.as_ref().and_then(|s| s.subject_type());
            let metadata = metadata_map_to_jsonb(&record.metadata);
            let is_compensation = record.corrects_id.is_some();

            let inserted = sqlx::query_as::<_, UsageRecordRow>(&insert_sql)
                .bind(record.uuid)
                .bind(record.tenant_id)
                .bind(gts_id_str(&record.gts_id))
                .bind(record.value)
                .bind(record.created_at)
                .bind(record.resource_ref.resource_id())
                .bind(record.resource_ref.resource_type())
                .bind(subject_id)
                .bind(subject_type)
                .bind(record.idempotency_key.as_str())
                .bind(record.corrects_id)
                .bind(metadata)
                .fetch_optional(&mut *tx)
                .await
                .map_err(|e| self.record_backend_error(&e))?;

            let row = inserted.ok_or_else(|| {
                dedup_invariant_break(
                    &record,
                    "won the dedup slot but the record insert returned no row \
                     (concurrent-insert invariant break)",
                )
            })?;

            tx.commit()
                .await
                .map_err(|e| self.record_backend_error(&e))?;

            if is_compensation {
                self.metrics.inc_compensation();
            }
            return record_row_to_model(row);
        }

        // 2b. Slot already held — read the stored pointer, then the record, and
        //     resolve absorb-vs-conflict. The read path mutates nothing.
        let pointer = sqlx::query_as::<_, (Uuid, OffsetDateTime)>(
            "SELECT record_uuid, record_created_at FROM usage_dedup \
             WHERE tenant_id = $1 AND gts_id = $2 AND idempotency_key = $3",
        )
        .bind(record.tenant_id)
        .bind(gts_id_str(&record.gts_id))
        .bind(record.idempotency_key.as_str())
        .fetch_optional(&mut *tx)
        .await
        .map_err(|e| self.record_backend_error(&e))?;

        let Some((existing_uuid, existing_created_at)) = pointer else {
            // The conflicting dedup row was deleted (cleanup) between our failed
            // insert and this read. Retryable: a retry re-claims the slot.
            tx.rollback().await.ok();
            return Err(dedup_transient(
                &record,
                "dedup slot disappeared during conflict resolution; retry",
            ));
        };

        let select_sql = format!(
            "SELECT {RECORD_COLUMNS} FROM usage_records WHERE uuid = $1 AND created_at = $2"
        );
        let stored = sqlx::query_as::<_, UsageRecordRow>(&select_sql)
            .bind(existing_uuid)
            .bind(existing_created_at)
            .fetch_optional(&mut *tx)
            .await
            .map_err(|e| self.record_backend_error(&e))?;

        // Read-only path: only SELECTs were issued, so rollback and commit are
        // equivalent; rollback is chosen to make it explicit that nothing is persisted.
        tx.rollback().await.ok();

        if let Some(row) = stored {
            self.resolve_dedup_hit(row, &record)
        } else {
            // Stale: the dedup row outlived its record (record chunk dropped
            // before the prune job reclaimed the dedup row). Reachable only by
            // replaying a key older than retention. Return retryable Transient
            // so a retry lands after cleanup (spec §4.1).
            self.metrics.inc_dedup_stale();
            Err(dedup_transient(
                &record,
                "dedup entry references an aged-out record; retry",
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
                existing_uuid: row.uuid,
            })
        }
    }

    /// Claim every distinct dedup slot in one multi-row INSERT. `RETURNING`
    /// yields exactly the keys we won (DO NOTHING rows are not returned); a
    /// concurrent same-key claim blocks on the row lock and resolves on commit
    /// — the single-row serialization authority, batched. `reps` must be sorted
    /// by [`DedupKey`] so concurrent batches lock in one global order.
    async fn claim_dedup_slots(
        &self,
        conn: &mut sqlx::PgConnection,
        reps: &[UsageRecord],
    ) -> Result<HashSet<DedupKey>, UsageCollectorPluginError> {
        let tenants: Vec<Uuid> = reps.iter().map(|r| r.tenant_id).collect();
        let gtss: Vec<String> = reps
            .iter()
            .map(|r| gts_id_str(&r.gts_id).to_owned())
            .collect();
        let keys: Vec<String> = reps
            .iter()
            .map(|r| r.idempotency_key.as_str().to_owned())
            .collect();
        let uuids: Vec<Uuid> = reps.iter().map(|r| r.uuid).collect();
        let cats: Vec<OffsetDateTime> = reps.iter().map(|r| r.created_at).collect();

        let rows = sqlx::query(
            "INSERT INTO usage_dedup \
             (tenant_id, gts_id, idempotency_key, record_uuid, record_created_at) \
             SELECT * FROM UNNEST($1::uuid[], $2::text[], $3::text[], $4::uuid[], $5::timestamptz[]) \
             ON CONFLICT (tenant_id, gts_id, idempotency_key) DO NOTHING \
             RETURNING tenant_id, gts_id, idempotency_key",
        )
        .bind(&tenants)
        .bind(&gtss)
        .bind(&keys)
        .bind(&uuids)
        .bind(&cats)
        .fetch_all(&mut *conn)
        .await
        .map_err(|e| self.record_backend_error(&e))?;

        Ok(rows
            .into_iter()
            .map(|row| {
                (
                    row.get::<Uuid, _>("tenant_id"),
                    row.get::<String, _>("gts_id"),
                    row.get::<String, _>("idempotency_key"),
                )
            })
            .collect())
    }

    /// Insert the records for the won keys in one multi-row INSERT, returning a
    /// `DedupKey` → stored row map. We own each won dedup slot, so the 4-tuple
    /// unique cannot conflict; a returned-count mismatch is an invariant break.
    /// `metadata` is bound as `text[]` of JSON strings and cast `::jsonb`
    /// per-row to sidestep `jsonb[]` array encoding.
    /// Unlike the single-row path (which fails just that row), this count
    /// mismatch is an `Internal` error that rolls back the whole batch — the
    /// signed-off atomic-batch behavior.
    async fn insert_won_records(
        &self,
        conn: &mut sqlx::PgConnection,
        reps: &[UsageRecord],
        won: &HashSet<DedupKey>,
    ) -> Result<HashMap<DedupKey, UsageRecordRow>, UsageCollectorPluginError> {
        let winners: Vec<&UsageRecord> = reps
            .iter()
            .filter(|r| won.contains(&dedup_key(r)))
            .collect();
        if winners.is_empty() {
            return Ok(HashMap::new());
        }

        let uuids: Vec<Uuid> = winners.iter().map(|r| r.uuid).collect();
        let tenants: Vec<Uuid> = winners.iter().map(|r| r.tenant_id).collect();
        let gtss: Vec<String> = winners
            .iter()
            .map(|r| gts_id_str(&r.gts_id).to_owned())
            .collect();
        let values: Vec<Decimal> = winners.iter().map(|r| r.value).collect();
        let cats: Vec<OffsetDateTime> = winners.iter().map(|r| r.created_at).collect();
        let resource_ids: Vec<String> = winners
            .iter()
            .map(|r| r.resource_ref.resource_id().to_owned())
            .collect();
        let resource_types: Vec<String> = winners
            .iter()
            .map(|r| r.resource_ref.resource_type().to_owned())
            .collect();
        let subject_ids: Vec<Option<String>> = winners
            .iter()
            .map(|r| {
                r.subject_ref
                    .as_ref()
                    .map(|s| usage_collector_sdk::SubjectRef::subject_id(s).to_owned())
            })
            .collect();
        let subject_types: Vec<Option<String>> = winners
            .iter()
            .map(|r| {
                r.subject_ref
                    .as_ref()
                    .and_then(|s| s.subject_type())
                    .map(str::to_owned)
            })
            .collect();
        let idem_keys: Vec<String> = winners
            .iter()
            .map(|r| r.idempotency_key.as_str().to_owned())
            .collect();
        let corrects: Vec<Option<Uuid>> = winners.iter().map(|r| r.corrects_id).collect();
        let metadata: Vec<String> = winners
            .iter()
            .map(|r| metadata_map_to_jsonb(&r.metadata).to_string())
            .collect();

        let sql = format!(
            "INSERT INTO usage_records \
             (uuid, tenant_id, gts_id, value, created_at, resource_id, resource_type, \
              subject_id, subject_type, idempotency_key, corrects_id, metadata) \
             SELECT uuid, tenant_id, gts_id, value, created_at, resource_id, resource_type, \
              subject_id, subject_type, idempotency_key, corrects_id, metadata::jsonb \
             FROM UNNEST($1::uuid[], $2::uuid[], $3::text[], $4::numeric[], $5::timestamptz[], \
              $6::text[], $7::text[], $8::text[], $9::text[], $10::text[], $11::uuid[], $12::text[]) \
              AS t(uuid, tenant_id, gts_id, value, created_at, resource_id, resource_type, \
                   subject_id, subject_type, idempotency_key, corrects_id, metadata) \
             RETURNING {RECORD_COLUMNS}"
        );

        let rows = sqlx::query_as::<_, UsageRecordRow>(&sql)
            .bind(&uuids)
            .bind(&tenants)
            .bind(&gtss)
            .bind(&values)
            .bind(&cats)
            .bind(&resource_ids)
            .bind(&resource_types)
            .bind(&subject_ids)
            .bind(&subject_types)
            .bind(&idem_keys)
            .bind(&corrects)
            .bind(&metadata)
            .fetch_all(&mut *conn)
            .await
            .map_err(|e| self.record_backend_error(&e))?;

        if rows.len() != winners.len() {
            // `winners` is non-empty here (the empty case returned above), so its
            // first element is a safe sample identifier for the failed batch.
            let sample = winners[0];
            tracing::error!(
                won_slots = winners.len(),
                inserted_records = rows.len(),
                sample_tenant_id = %sample.tenant_id,
                sample_gts_id = %gts_id_str(&sample.gts_id),
                "won dedup slots but inserted fewer records (concurrent-insert invariant break)"
            );
            return Err(UsageCollectorPluginError::internal(format!(
                "won {} dedup slots but inserted {} records (concurrent-insert invariant break)",
                winners.len(),
                rows.len(),
            )));
        }

        Ok(rows
            .into_iter()
            .map(|row| {
                (
                    (
                        row.tenant_id,
                        row.gts_id.clone(),
                        row.idempotency_key.clone(),
                    ),
                    row,
                )
            })
            .collect())
    }

    /// For the not-won keys, read the dedup pointer (D1) then the referenced
    /// record (D2) — the batch analogue of the single path's conflict branch.
    /// Maps each key to `Stored` / `Stale` / `Disappeared`.
    async fn read_conflict_records(
        &self,
        conn: &mut sqlx::PgConnection,
        not_won: &[&UsageRecord],
    ) -> Result<HashMap<DedupKey, ConflictRead>, UsageCollectorPluginError> {
        let mut out: HashMap<DedupKey, ConflictRead> = HashMap::new();
        if not_won.is_empty() {
            return Ok(out);
        }

        let tenants: Vec<Uuid> = not_won.iter().map(|r| r.tenant_id).collect();
        let gtss: Vec<String> = not_won
            .iter()
            .map(|r| gts_id_str(&r.gts_id).to_owned())
            .collect();
        let keys: Vec<String> = not_won
            .iter()
            .map(|r| r.idempotency_key.as_str().to_owned())
            .collect();

        let pointer_rows = sqlx::query(
            "SELECT d.tenant_id, d.gts_id, d.idempotency_key, d.record_uuid, d.record_created_at \
             FROM UNNEST($1::uuid[], $2::text[], $3::text[]) AS k(tenant_id, gts_id, idempotency_key) \
             JOIN usage_dedup d \
               ON d.tenant_id = k.tenant_id AND d.gts_id = k.gts_id \
              AND d.idempotency_key = k.idempotency_key",
        )
        .bind(&tenants)
        .bind(&gtss)
        .bind(&keys)
        .fetch_all(&mut *conn)
        .await
        .map_err(|e| self.record_backend_error(&e))?;

        let mut pointers: HashMap<DedupKey, (Uuid, OffsetDateTime)> = HashMap::new();
        for row in pointer_rows {
            let key = (
                row.get::<Uuid, _>("tenant_id"),
                row.get::<String, _>("gts_id"),
                row.get::<String, _>("idempotency_key"),
            );
            pointers.insert(
                key,
                (
                    row.get::<Uuid, _>("record_uuid"),
                    row.get::<OffsetDateTime, _>("record_created_at"),
                ),
            );
        }

        // Populate `Disappeared` entries BEFORE the empty-pointers early return,
        // so an all-disappeared batch still returns a fully-populated map.
        for r in not_won {
            let key = dedup_key(r);
            if !pointers.contains_key(&key) {
                out.insert(key, ConflictRead::Disappeared);
            }
        }
        if pointers.is_empty() {
            return Ok(out);
        }

        let pairs: Vec<(Uuid, OffsetDateTime)> = pointers.values().copied().collect();
        let p_uuids: Vec<Uuid> = pairs.iter().map(|(u, _)| *u).collect();
        let p_cats: Vec<OffsetDateTime> = pairs.iter().map(|(_, c)| *c).collect();
        let select_sql = format!(
            "SELECT {RECORD_COLUMNS} FROM usage_records \
             WHERE (uuid, created_at) IN \
               (SELECT u, c FROM UNNEST($1::uuid[], $2::timestamptz[]) AS t(u, c))"
        );
        let record_rows = sqlx::query_as::<_, UsageRecordRow>(&select_sql)
            .bind(&p_uuids)
            .bind(&p_cats)
            .fetch_all(&mut *conn)
            .await
            .map_err(|e| self.record_backend_error(&e))?;

        let mut by_pk: HashMap<(Uuid, OffsetDateTime), UsageRecordRow> = HashMap::new();
        for row in record_rows {
            by_pk.insert((row.uuid, row.created_at), row);
        }

        for (key, pk) in pointers {
            // Move the row out of `by_pk` (each pk is referenced by exactly one
            // pointer) rather than cloning it into the map; a missing pk means
            // the record aged out between the pointer and record reads → Stale.
            match by_pk.remove(&pk) {
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
    fn resolve_batch(
        &self,
        records: Vec<UsageRecord>,
        plan: &BatchPlan,
        won: &HashSet<DedupKey>,
        inserted: &HashMap<DedupKey, UsageRecordRow>,
        conflict: &HashMap<DedupKey, ConflictRead>,
    ) -> Vec<Result<UsageRecord, UsageCollectorPluginError>> {
        let mut results = Vec::with_capacity(records.len());
        for (i, record) in records.into_iter().enumerate() {
            let key = dedup_key(&record);
            let is_winner = won.contains(&key) && plan.first_index.get(&key) == Some(&i);
            let outcome = if is_winner {
                match inserted.get(&key) {
                    Some(row) => {
                        if record.corrects_id.is_some() {
                            self.metrics.inc_compensation();
                        }
                        record_row_to_model(row.clone())
                    }
                    None => Err(dedup_invariant_break(
                        &record,
                        "won dedup slot but no inserted record was returned \
                         (concurrent-insert invariant break)",
                    )),
                }
            } else if won.contains(&key) {
                match inserted.get(&key) {
                    Some(row) => self.resolve_dedup_hit(row.clone(), &record),
                    None => Err(dedup_invariant_break(
                        &record,
                        "intra-batch duplicate of a won key with no inserted record",
                    )),
                }
            } else {
                match conflict.get(&key) {
                    Some(ConflictRead::Stored(row)) => {
                        // Clone the inner row directly; `*row.clone()` would
                        // round-trip through a throwaway `Box` allocation. The
                        // clone itself is required — a not-won key may be
                        // resolved by several input rows against the borrowed map.
                        self.resolve_dedup_hit((**row).clone(), &record)
                    }
                    Some(ConflictRead::Stale) => {
                        self.metrics.inc_dedup_stale();
                        Err(dedup_transient(
                            &record,
                            "dedup entry references an aged-out record; retry",
                        ))
                    }
                    Some(ConflictRead::Disappeared) | None => Err(dedup_transient(
                        &record,
                        "dedup slot disappeared during conflict resolution; retry",
                    )),
                }
            };
            results.push(outcome);
        }
        results
    }

    /// Orchestrate one batch in a single transaction: claim → insert winners →
    /// read conflicts → commit → resolve per row in input order.
    async fn create_batch_inner(
        &self,
        records: Vec<UsageRecord>,
    ) -> Result<Vec<Result<UsageRecord, UsageCollectorPluginError>>, UsageCollectorPluginError>
    {
        let plan = plan_batch(&records);

        let mut conn = self.timed_acquire().await?;
        let mut tx = conn
            .begin()
            .await
            .map_err(|e| self.record_backend_error(&e))?;

        let won = self.claim_dedup_slots(&mut tx, &plan.reps).await?;
        let inserted = self.insert_won_records(&mut tx, &plan.reps, &won).await?;
        let not_won: Vec<&UsageRecord> = plan
            .reps
            .iter()
            .filter(|r| !won.contains(&dedup_key(r)))
            .collect();
        let conflict = self.read_conflict_records(&mut tx, &not_won).await?;

        tx.commit()
            .await
            .map_err(|e| self.record_backend_error(&e))?;

        Ok(self.resolve_batch(records, &plan, &won, &inserted, &conflict))
    }
}

/// Append the metadata side-channel filters as parameterized `WHERE` clauses.
///
/// Shared by [`PgRecordStore::list`] and [`PgRecordStore::aggregate`] so both
/// expand the side channel identically: AND across filters, OR within one
/// filter's values (`metadata ->> $key IN ($v1, $v2, …)`). The key and every
/// value are bound via `ctx` (`$N`); only the `metadata ->> $N` shape is
/// interpolated, so this is injection-safe. An empty value set matches nothing
/// (the gateway rejects it, but be defensive): a `FALSE` clause is emitted so
/// the result is empty rather than unfiltered.
fn push_metadata_filter_clauses(
    metadata_filter: &[MetadataFilter],
    ctx: &mut SqlCtx,
    clauses: &mut Vec<String>,
) {
    for mf in metadata_filter {
        if mf.values().is_empty() {
            clauses.push("FALSE".to_owned());
            continue;
        }
        let key_n = ctx.push(SqlBind::Str(mf.key().as_str().to_owned()));
        let placeholders = mf
            .values()
            .iter()
            .map(|v| format!("${}", ctx.push(SqlBind::Str(v.clone()))))
            .collect::<Vec<_>>();
        clauses.push(format!(
            "metadata ->> ${key_n} IN ({})",
            placeholders.join(", ")
        ));
    }
}

/// Extract a single order-field value from a row as its cursor-key string.
///
/// Inverse of [`cursor_key_to_bind`](crate::infra::storage::query::keyset::cursor_key_to_bind):
/// `uuid` / `corrects_id` render via [`Uuid::to_string`], `created_at` as
/// RFC 3339, `tenant_id` via its `Uuid` string, and the text columns
/// (`resource_id` / `resource_type` / `subject_id` / `subject_type` / `status`)
/// as-is. Returns `None` for an unknown field or a `NULL` optional column (a
/// `NULL` value can't seed a stable keyset boundary).
fn record_row_key(row: &UsageRecordRow, field: &str) -> Option<String> {
    match field {
        "uuid" => Some(row.uuid.to_string()),
        "corrects_id" => row.corrects_id.map(|id| id.to_string()),
        "created_at" => row.created_at.format(&Rfc3339).ok(),
        "tenant_id" => Some(row.tenant_id.to_string()),
        "resource_id" => Some(row.resource_id.clone()),
        "resource_type" => Some(row.resource_type.clone()),
        "subject_id" => row.subject_id.clone(),
        "subject_type" => row.subject_type.clone(),
        "status" => Some(row.status.clone()),
        _ => None,
    }
}

/// The `created_at`-independent dedup identity, mirroring the `usage_dedup`
/// 3-tuple primary key `(tenant_id, gts_id, idempotency_key)`.
type DedupKey = (Uuid, String, String);

/// Build the [`DedupKey`] for a record.
fn dedup_key(record: &UsageRecord) -> DedupKey {
    (
        record.tenant_id,
        gts_id_str(&record.gts_id).to_owned(),
        record.idempotency_key.as_str().to_owned(),
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
        gts_id = %gts_id_str(&record.gts_id),
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
        gts_id = %gts_id_str(&record.gts_id),
        idempotency_key = %record.idempotency_key.as_str(),
        "{msg}"
    );
    UsageCollectorPluginError::transient(msg)
}

/// Deterministic plan for a batch claim.
///
/// `reps` are the first-occurrence representative records, one per distinct
/// dedup key, **sorted** by [`DedupKey`] so concurrent batches acquire
/// `usage_dedup` row locks in one global order (deadlock-free). `first_index`
/// maps each key to the input index of its first occurrence — the only row
/// that can win the slot; later same-key rows resolve against the winner's
/// record, exactly as the single-row path resolves a same-key hit.
struct BatchPlan {
    reps: Vec<UsageRecord>,
    first_index: HashMap<DedupKey, usize>,
}

/// Collapse a batch to its distinct dedup keys (first occurrence wins),
/// sorted for a stable lock order. Pure — no DB.
fn plan_batch(records: &[UsageRecord]) -> BatchPlan {
    let mut first_index: HashMap<DedupKey, usize> = HashMap::new();
    let mut reps: Vec<(DedupKey, UsageRecord)> = Vec::new();
    for (i, record) in records.iter().enumerate() {
        let key = dedup_key(record);
        if let std::collections::hash_map::Entry::Vacant(slot) = first_index.entry(key.clone()) {
            slot.insert(i);
            reps.push((key, record.clone()));
        }
    }
    reps.sort_by(|a, b| a.0.cmp(&b.0));
    BatchPlan {
        reps: reps.into_iter().map(|(_, r)| r).collect(),
        first_index,
    }
}

/// Total `create_batch` attempts: one initial try plus two retries. A bounded
/// in-process retry so a rare deadlock victim self-heals transparently instead
/// of bubbling an `Err(Transient)` to the host (see [`with_retry`]).
const MAX_BATCH_ATTEMPTS: u32 = 3;

/// Backoff before the `attempt`-th retry (1-based: `batch_retry_backoff(1)`
/// precedes the first retry). A short exponential — 5 ms, 10 ms, … — because a
/// deadlock victim can retry almost immediately: the surviving transaction has
/// already committed or aborted by the time Postgres aborts the victim, so the
/// contended dedup locks are free. The shift is saturated so the schedule can
/// never overflow regardless of how `MAX_BATCH_ATTEMPTS` grows.
fn batch_retry_backoff(attempt: u32) -> Duration {
    let shift = attempt.saturating_sub(1).min(6);
    Duration::from_millis(5u64 << shift)
}

/// Retry predicate for [`with_retry`] around `create_batch`: retry **only** an
/// outer [`UsageCollectorPluginError::Transient`].
///
/// The deadlock victim surfaces as an outer `Transient` (the whole transaction
/// rolled back); serialization failures (`40001`) and connection blips collapse
/// to the same bucket inside the storage helpers, and all are safe to re-run
/// for this idempotent batch. `Internal`, `IdempotencyConflict`, and the typed
/// domain errors are non-retryable and returned unchanged. Per-row `Transient`
/// outcomes carried inside an `Ok(vec)` are deliberately not seen here — the
/// batch as a whole succeeded, so the loop never inspects them.
fn is_retryable_batch_error(err: &UsageCollectorPluginError) -> bool {
    matches!(err, UsageCollectorPluginError::Transient { .. })
}

/// Run `operation`, retrying while `should_retry` accepts its error, for up to
/// `max_attempts` total invocations; sleep `backoff(attempt)` before the
/// `attempt`-th retry. Returns the first `Ok`, or — once retries are exhausted
/// or the error is non-retryable — the last `Err` unchanged.
///
/// Generic and DB-free so the retry mechanics are unit-tested without a
/// transaction. `operation` is an `Fn` invoked fresh each attempt (the caller
/// rebuilds any owned input per call), which is exactly the right unit of retry
/// for `create_batch_inner`: every attempt acquires a fresh connection and opens
/// a fresh transaction. There is zero happy-path cost — on success the loop runs
/// the operation once and neither sleeps nor allocates a backoff.
async fn with_retry<T, E, Op, Fut>(
    max_attempts: u32,
    backoff: impl Fn(u32) -> Duration,
    should_retry: impl Fn(&E) -> bool,
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
                // `toolkit::tokio` is the crate's tokio re-export (matching
                // `toolkit::tokio::spawn` / `select!` elsewhere in this gear);
                // `tokio` itself is only a dev-dependency.
                toolkit::tokio::time::sleep(backoff(attempt)).await;
                attempt += 1;
            }
        }
    }
}

/// Outcome of reading the authoritative stored record for a not-won key.
enum ConflictRead {
    /// The dedup row's record exists — resolve absorb vs conflict against it.
    Stored(Box<UsageRecordRow>),
    /// The dedup row exists but its record has aged out → retryable `Transient`.
    Stale,
    /// The dedup row vanished between claim and read (cleanup raced) →
    /// retryable `Transient`.
    Disappeared,
}

/// `OffsetDateTime` at microsecond precision, as the unix-epoch microsecond
/// count.
///
/// Postgres `timestamptz` stores microseconds; an incoming `OffsetDateTime`
/// may carry sub-microsecond nanos that never survive the round-trip. Comparing
/// the microsecond counts makes the canonical-equality check agree with what
/// the DB actually persisted. Built from the whole-second timestamp plus the
/// sub-second microsecond component (no integer division).
fn to_micros(dt: OffsetDateTime) -> i128 {
    i128::from(dt.unix_timestamp()) * 1_000_000 + i128::from(dt.microsecond())
}

/// Compare the caller-supplied canonical fields of a stored row against an
/// incoming record (§3.6: absorb vs conflict).
///
/// The dedup-key fields (`tenant_id` / `gts_id` / `idempotency_key`) are not
/// compared — they are the lookup key and already match. Server-managed
/// `status` / `ingested_at` are likewise excluded. `created_at` is compared at
/// microsecond precision (see [`to_micros`]); `metadata` is compared after
/// decoding the stored `jsonb` back to the typed map.
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
    Ok(row.uuid == incoming.uuid
        && row.value == incoming.value
        && to_micros(row.created_at) == to_micros(incoming.created_at)
        && row.resource_id == incoming.resource_ref.resource_id()
        && row.resource_type == incoming.resource_ref.resource_type()
        && row.subject_id.as_deref()
            == incoming
                .subject_ref
                .as_ref()
                .map(usage_collector_sdk::SubjectRef::subject_id)
        && row.subject_type.as_deref()
            == incoming.subject_ref.as_ref().and_then(|s| s.subject_type())
        && row.corrects_id == incoming.corrects_id
        && stored_metadata == incoming.metadata)
}

#[async_trait]
impl RecordStore for PgRecordStore {
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
        // failure `40001`, or a connection blip) re-run the operation up to
        // `MAX_BATCH_ATTEMPTS` times. Each attempt acquires a fresh connection
        // and opens a fresh transaction (`create_batch_inner` does both), so a
        // rolled-back attempt leaves no state behind. Re-running is safe: the
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
            || self.create_batch_inner(records.clone()),
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

    async fn get(&self, uuid: Uuid) -> Result<UsageRecord, UsageCollectorPluginError> {
        let sql = format!("SELECT {RECORD_COLUMNS} FROM usage_records WHERE uuid = $1");
        let mut conn = self.timed_acquire().await?;
        let row = sqlx::query_as::<_, UsageRecordRow>(&sql)
            .bind(uuid)
            .fetch_optional(&mut *conn)
            .await
            .map_err(|err| self.record_backend_error(&err))?;

        match row {
            Some(row) => record_row_to_model(row),
            None => Err(UsageCollectorPluginError::UsageRecordNotFound { id: uuid }),
        }
    }

    /// Keyset-paginated `usage_records` list, scoped to `gts_id` (bound at
    /// `$1`), with optional `$filter`, metadata side-channel filters, and a
    /// cursor.
    ///
    /// Builds `SELECT {RECORD_COLUMNS} FROM usage_records WHERE gts_id = $1
    /// [AND <filter>] [AND <metadata>] [AND <keyset>] ORDER BY <order> LIMIT
    /// <n+1>`. The extra `+1` row is the look-ahead that detects a following
    /// page; it is truncated before mapping. All identifiers come from the
    /// [`record_column`] allowlist and the static [`RECORD_COLUMNS`]; every
    /// value is bound (`$N`).
    ///
    /// # Errors
    ///
    /// Returns [`UsageCollectorPluginError::Internal`] when the filter AST
    /// references an unknown field, the cursor's filter hash disagrees with
    /// `query.filter_hash`, an order/keyset field is off the allowlist, a stored
    /// row cannot be mapped, or the DB query fails.
    // @cpt-flow:cpt-cf-uc-plugin-seq-list-keyset:p2
    async fn list(
        &self,
        gts_id: UsageTypeGtsId,
        query: &ODataQuery,
        metadata_filter: &[MetadataFilter],
    ) -> Result<ODataPage<UsageRecord>, UsageCollectorPluginError> {
        // Time the full raw-list call and count the request. The drop-timer
        // records the histogram on every return — including the validation
        // error arms below — not just on success.
        let _timer =
            OpDurationGuard::start(Arc::clone(&self.metrics), TimedOp::Query(QueryKind::Raw));
        self.metrics.inc_query_request(QueryKind::Raw);
        let limit = query.limit.unwrap_or(DEFAULT_PAGE_SIZE);

        // `$1` is reserved for the `gts_id` scope bind; every translated bind
        // therefore starts at `$2`.
        let mut ctx = SqlCtx::new(2);
        let mut clauses: Vec<String> = vec!["gts_id = $1".to_owned()];

        // `$filter` (validated AST -> typed node -> parameterized fragment).
        if let Some(expr) = query.filter() {
            let node = convert_expr_to_filter_node::<UsageRecordFilterField>(expr)
                .map_err(|e| UsageCollectorPluginError::internal(format!("invalid filter: {e}")))?;
            let fragment = translate_record_filter(&node, &mut ctx)
                .map_err(UsageCollectorPluginError::internal)?;
            clauses.push(fragment);
        }

        // Metadata side-channel: AND across filters, OR within one filter's
        // values (see [`push_metadata_filter_clauses`]).
        push_metadata_filter_clauses(metadata_filter, &mut ctx, &mut clauses);

        // Keyset continuation (forward only). The cursor's filter hash must
        // match the live query's so a cursor is never replayed against a
        // different filter.
        if let Some(cursor) = query.cursor.as_ref() {
            // Forward-only: the keyset operator is derived from the sort
            // direction, not from `cursor.d`, so a backward cursor would
            // silently page forward. Reject it fail-closed.
            ensure_forward_cursor(cursor).map_err(UsageCollectorPluginError::internal)?;
            if cursor.f.as_deref() != query.filter_hash.as_deref() {
                return Err(UsageCollectorPluginError::internal(
                    "cursor filter hash mismatch",
                ));
            }
            // The cursor's keys (`cursor.k`) are positional, bound against the
            // live `query.order` columns below. If the order changed between
            // pages at the same arity, old keys would bind to new columns —
            // silently wrong pagination. The cursor carries the signed sort
            // tokens (`cursor.s`) precisely to detect this, mirroring the
            // filter-hash guard above.
            if !query.order.equals_signed_tokens(&cursor.s) {
                return Err(UsageCollectorPluginError::internal(
                    "cursor sort order mismatch",
                ));
            }
            let order_pairs: Vec<(&str, bool)> = query
                .order
                .0
                .iter()
                .map(|key| (key.field.as_str(), matches!(key.dir, SortDir::Asc)))
                .collect();
            let predicate = keyset_predicate(
                &order_pairs,
                &cursor.k,
                record_column,
                |name| UsageRecordFilterField::from_name(name).map(|f| f.kind()),
                &mut ctx,
            )
            .map_err(UsageCollectorPluginError::internal)?;
            clauses.push(predicate);
        }

        let order_sql = render_order_by(&query.order, record_column)
            .map_err(UsageCollectorPluginError::internal)?;

        let sql = format!(
            "SELECT {RECORD_COLUMNS} FROM usage_records WHERE {} ORDER BY {order_sql} LIMIT {}",
            clauses.join(" AND "),
            limit.saturating_add(1),
        );

        let mut q = sqlx::query_as::<_, UsageRecordRow>(&sql).bind(gts_id_str(&gts_id));
        for b in &ctx.binds {
            q = bind_one(q, b);
        }
        let mut conn = self.timed_acquire().await?;
        let mut rows = q
            .fetch_all(&mut *conn)
            .await
            .map_err(|e| self.record_backend_error(&e))?;

        // Look-ahead row present -> a next page exists; drop it before mapping.
        let has_next = rows.len() > usize::try_from(limit).unwrap_or(usize::MAX);
        if has_next {
            rows.truncate(usize::try_from(limit).unwrap_or(usize::MAX));
        }

        let next_cursor = if has_next {
            let last = rows.last().ok_or_else(|| {
                UsageCollectorPluginError::internal("non-empty page lost its tail")
            })?;
            let keys = query
                .order
                .0
                .iter()
                .map(|key| {
                    record_row_key(last, &key.field).ok_or_else(|| {
                        UsageCollectorPluginError::internal(format!(
                            "order field `{}` has no cursor key on the row",
                            key.field
                        ))
                    })
                })
                .collect::<Result<Vec<_>, _>>()?;
            let token = encode_next_cursor(&query.order, &keys, query.filter_hash.as_deref())
                .map_err(UsageCollectorPluginError::internal)?;
            Some(token)
        } else {
            None
        };

        let items = rows
            .into_iter()
            .map(record_row_to_model)
            .collect::<Result<Vec<_>, _>>()?;

        // `_timer` records `query.duration` on drop (success and error alike).
        Ok(ODataPage::new(
            items,
            PageInfo {
                next_cursor,
                prev_cursor: None,
                limit,
            },
        ))
    }

    /// Pushed-down aggregation over `usage_records`, scoped to `gts_id` (bound
    /// at `$1`) and to `status = 'active'`, with optional `$filter`, metadata
    /// side-channel filters, and a `GROUP BY` over the spec's dimensions
    /// (§3.6 aggregated query).
    ///
    /// Builds `SELECT <dim exprs…>, <AGG> FROM usage_records WHERE gts_id = $1
    /// AND status = 'active' [AND <filter>] [AND <metadata>] [AND
    /// <subject-not-null guards>] [GROUP BY 1, 2, …]`. The aggregate
    /// ([`agg_select_expr`]) and each dimension ([`dimension_select_expr`])
    /// come from closed enum allowlists; the only caller-derived values (a
    /// grouped metadata key, `$filter` operands, metadata side-channel values)
    /// are bound (`$N`). `status = 'active'` is always applied: compensation
    /// rows net out because they carry a signed `value` and stay `active`
    /// until deactivated. With an empty `group_by` there is no `GROUP BY`
    /// clause, so the query yields exactly one bucket with `key = []`.
    ///
    /// Each returned row maps to one [`AggregationBucket`]: the `k` dimension
    /// columns read positionally as `Option<String>` (a `NULL` dimension
    /// becomes the empty string — relevant only when a grouped metadata key is
    /// absent on some active rows), and the aggregate at index `k` reads as
    /// `Option<Decimal>` (carried through as-is; `NULL` -> `None`).
    ///
    /// # Errors
    ///
    /// Returns [`UsageCollectorPluginError::Internal`] when the filter AST
    /// references an unknown field or is otherwise invalid, the DB query fails,
    /// or a result column cannot be read at its expected type.
    // @cpt-flow:cpt-cf-uc-plugin-seq-query-aggregated:p2
    async fn aggregate(
        &self,
        gts_id: UsageTypeGtsId,
        query: &ODataQuery,
        metadata_filter: &[MetadataFilter],
        spec: AggregationSpec,
    ) -> Result<AggregationResult, UsageCollectorPluginError> {
        // Time the full aggregated-query call and count the request. The
        // drop-timer records the histogram on every return, not just success.
        let _timer = OpDurationGuard::start(
            Arc::clone(&self.metrics),
            TimedOp::Query(QueryKind::Aggregated),
        );
        self.metrics.inc_query_request(QueryKind::Aggregated);
        // `$1` is reserved for the `gts_id` scope bind; every translated bind
        // therefore starts at `$2`.
        let mut ctx = SqlCtx::new(2);
        let mut clauses: Vec<String> =
            vec!["gts_id = $1".to_owned(), "status = 'active'".to_owned()];

        // `$filter` (validated AST -> typed node -> parameterized fragment).
        if let Some(expr) = query.filter() {
            let node = convert_expr_to_filter_node::<UsageRecordFilterField>(expr)
                .map_err(|e| UsageCollectorPluginError::internal(format!("invalid filter: {e}")))?;
            let fragment = translate_record_filter(&node, &mut ctx)
                .map_err(UsageCollectorPluginError::internal)?;
            clauses.push(fragment);
        }

        // Metadata side-channel (same expansion as `list`).
        push_metadata_filter_clauses(metadata_filter, &mut ctx, &mut clauses);

        // Build dimension SELECT exprs in GROUP-BY order, binding any metadata
        // keys, and emit subject-not-null guards so subject-less rows are
        // excluded from subject grouping (per the SDK dimension docs).
        let mut select_dims: Vec<String> = Vec::with_capacity(spec.group_by.len());
        for dim in &spec.group_by {
            match dim {
                AggregationDimension::SubjectId => {
                    clauses.push("subject_id IS NOT NULL".to_owned());
                }
                AggregationDimension::SubjectType => {
                    clauses.push("subject_type IS NOT NULL".to_owned());
                }
                _ => {}
            }
            select_dims.push(dimension_select_expr(dim, &mut ctx));
        }

        // SELECT list = dimension exprs ++ the aggregate. With no dimensions
        // the SELECT is just the aggregate (single-bucket / no-grouping case).
        let dim_count = select_dims.len();
        let mut select_parts = select_dims;
        select_parts.push(agg_select_expr(spec.op).to_owned());
        let select_list = select_parts.join(", ");

        // GROUP BY by ordinal (1..=k) so the bound metadata expr is not
        // repeated; omitted entirely when there are no dimensions.
        let group_by = if dim_count == 0 {
            String::new()
        } else {
            let ordinals = (1..=dim_count)
                .map(|n| n.to_string())
                .collect::<Vec<_>>()
                .join(", ");
            format!(" GROUP BY {ordinals}")
        };

        let sql = format!(
            "SELECT {select_list} FROM usage_records WHERE {}{group_by}",
            clauses.join(" AND "),
        );

        let mut q = sqlx::query(&sql).bind(gts_id_str(&gts_id));
        for b in &ctx.binds {
            q = bind_one_query(q, b);
        }
        let mut conn = self.timed_acquire().await?;
        let rows = q
            .fetch_all(&mut *conn)
            .await
            .map_err(|e| self.record_backend_error(&e))?;

        let mut buckets = Vec::with_capacity(rows.len());
        for row in rows {
            let mut key = Vec::with_capacity(dim_count);
            for i in 0..dim_count {
                let dim = row.try_get::<Option<String>, _>(i).map_err(|e| {
                    UsageCollectorPluginError::internal(format!(
                        "aggregate dimension column {i} read failed: {e}"
                    ))
                })?;
                // A NULL dimension (e.g. a grouped metadata key absent on some
                // active rows) becomes the empty string in the bucket key.
                key.push(dim.unwrap_or_default());
            }
            let value = row.try_get::<Option<Decimal>, _>(dim_count).map_err(|e| {
                UsageCollectorPluginError::internal(format!(
                    "aggregate value column {dim_count} read failed: {e}"
                ))
            })?;
            buckets.push(AggregationBucket { key, value });
        }

        // `_timer` records `query.duration` on drop (success and error alike).
        Ok(AggregationResult { buckets })
    }

    /// Deactivate a record and its depth-1 active compensations in one
    /// transaction (§3.6 deactivate-cascade).
    ///
    /// Locks the target row `FOR UPDATE` and reads its `status`: a missing row
    /// is `UsageRecordNotFound`, an already-`inactive` row is
    /// `UsageRecordAlreadyInactive`. An `active` target and every `active` row
    /// whose `corrects_id` points at it (depth-1 only) flip to `inactive` in a
    /// single `UPDATE`. The transition is one-way and mutates no other column;
    /// rows already `inactive` and unrelated rows are untouched. `uuid` is
    /// effectively unique across the table (caller-supplied, one row per record,
    /// per the dedup design), so `WHERE uuid = $1` addresses exactly one row.
    // @cpt-flow:cpt-cf-uc-plugin-seq-deactivate-cascade:p2
    async fn deactivate(&self, id: Uuid) -> Result<(), UsageCollectorPluginError> {
        // Time the full deactivation cascade; the drop-timer records the
        // duration on every return — including the not-found / already-inactive
        // and error arms — not just on a successful commit.
        let _timer = OpDurationGuard::start(Arc::clone(&self.metrics), TimedOp::Deactivate);
        let mut conn = self.timed_acquire().await?;
        let mut tx = conn
            .begin()
            .await
            .map_err(|e| self.record_backend_error(&e))?;

        // Lock + read the target's status. `uuid` is unique, so at most one row.
        let status = sqlx::query_scalar::<_, String>(
            "SELECT status FROM usage_records WHERE uuid = $1 FOR UPDATE",
        )
        .bind(id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(|e| self.record_backend_error(&e))?;

        match status {
            None => {
                tx.rollback().await.ok();
                return Err(UsageCollectorPluginError::UsageRecordNotFound { id });
            }
            Some(s) if s == "inactive" => {
                tx.rollback().await.ok();
                return Err(UsageCollectorPluginError::UsageRecordAlreadyInactive { id });
            }
            Some(_) => {}
        }

        // Flip the target and its depth-1 active compensations. One-way; the
        // `status = 'active'` guard on the compensations keeps already-inactive
        // children untouched and bounds the cascade to a single level.
        sqlx::query(
            "UPDATE usage_records SET status = 'inactive' \
             WHERE uuid = $1 OR (corrects_id = $1 AND status = 'active')",
        )
        .bind(id)
        .execute(&mut *tx)
        .await
        .map_err(|e| self.record_backend_error(&e))?;

        tx.commit()
            .await
            .map_err(|e| self.record_backend_error(&e))?;
        // `_timer` records `deactivate.duration` on drop.
        Ok(())
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "record_store_tests.rs"]
mod record_store_tests;
