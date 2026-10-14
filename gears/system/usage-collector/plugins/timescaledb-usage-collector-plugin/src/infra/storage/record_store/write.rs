//! The batch write's orchestration: one transaction, the guarded statement's
//! verdicts, the conflict read-back for the keys that lost, and the per-row
//! resolution the caller gets back.
//!
//! A child of [`super`] rather than a sibling, because every step here needs
//! [`PgRecordStore`]'s own fields — the metrics it counts on, the
//! [`TypeKeyCache`](crate::infra::storage::type_key::TypeKeyCache) it resolves
//! partition keys through, and the acceptance slack it binds into the
//! statement. Being a descendant module is what gives it that access without
//! widening a single field.
//!
//! The SQL it sends is not here: [`super::super::query::write`] owns
//! `BATCH_GUARDED_SQL` and `BATCH_CONFLICT_READ_SQL`, and is unit-tested
//! against their text with no database. What is here is everything that needs a
//! connection or the SDK model — the transaction, the per-column pivot that
//! binds the statement, the decode of its verdicts, and the resolution of each
//! input row against its authoritative record.

// Vendored TimescaleDB raw-SQL backend: `sqlx` is required infra (hypertable
// time-series, `time_bucket` aggregation, keyset pagination — see DESIGN.md). Tenant
// isolation is enforced by hand via parameterized `tenant_id` predicates and an
// allowlisted-identifier query builder (DESIGN.md §Injection-Safe Query Translation),
// not SecureConn/AccessScope.
#![allow(unknown_lints, de0706_no_direct_sqlx)]

use std::collections::HashMap;

use rust_decimal::Decimal;
use sqlx::pool::PoolConnection;
use sqlx::postgres::PgRow;
use sqlx::{Acquire as _, AssertSqlSafe, FromRow as _, Postgres, Row};
use time::OffsetDateTime;
use uuid::Uuid;

use usage_collector_sdk::v2::{
    MeterRef, MeteredRecord, StoredUsageRecord, UsageCollectorPluginError,
};

use crate::infra::storage::entity::UsageRecordRow;
use crate::infra::storage::mapper::{
    invalidation_to_row, metadata_map_to_jsonb, record_row_to_model,
};
use crate::infra::storage::query::write::{BATCH_CONFLICT_READ_SQL, BATCH_GUARDED_SQL};

use super::PgRecordStore;

/// The first statement of every write transaction whose commit is acknowledged
/// to a caller, per `DESIGN.md` §3.5: *"Every write transaction whose commit is
/// acknowledged to a caller runs `SET LOCAL synchronous_commit = on`, so an
/// operator-level `synchronous_commit` of `off` or `local` cannot weaken an
/// acknowledgement."* The retention sweep's transaction is not one of them, and
/// §3.6's sweep sequence omits this statement deliberately.
///
/// **Why not a connection parameter like the timeouts.** The bound is per
/// *transaction*; a session default is what an operator can already set.
/// `SET LOCAL` reverts at `COMMIT`, so it states the write path's own
/// requirement rather than reconfiguring the pool. Server-wide `fsync` and
/// `full_page_writes` genuinely cannot be forced here, hence the startup checks
/// in [`crate::infra::storage::pool`].
///
/// **It does not cost the guarded statement its place as the transaction's
/// first write**, which §3.6 requires and the feed's settled horizon rests on:
/// `SET` writes no tuple and takes no `XID` (`PostgreSQL` assigns one lazily, at
/// the first writing statement), so the transaction is still virtual until the
/// `INSERT` runs.
const FORCE_SYNCHRONOUS_COMMIT_SQL: &str = "SET LOCAL synchronous_commit = on";

/// What a caller is told when the write statement's acceptance guard refused
/// the entry.
///
/// One spelling for both write paths, and it names the remedy rather than the
/// predicate: the host lifts a `Transient` to a retryable error, and a retry is
/// stamped afresh (`DESIGN.md` §3.6), so re-submitting is what resolves it.
const STALE_ACCEPTANCE_MESSAGE: &str =
    "acceptance instant outside the configured acceptance slack; re-stamp and retry";

/// What a caller is told when a submission lost its dedup slot and the row that
/// took it could not then be read back.
///
/// **The wording says what happened, not why, and the retryability it implies
/// does not hold of everything that reaches here.** No count is given and none
/// should be added. The causes worth a reader's attention, in descending order
/// of how well a retry serves them:
///
/// * The **retention race** the arm is named for: the conflicting row's chunk
///   was dropped between the conflicting insert and this read. Near-impossible
///   against the retention boundary, and genuinely retryable — the unique entry
///   went with the chunk, so a retry wins the freed slot as a fresh insert.
/// * A **submission whose `id` does not match its own six inputs**, which, when
///   those six are already stored, collides on an arbiter that reads them off
///   the row and is then looked for under an `id` no row carries
///   ([`entry_identity`] sets out the rest). It does **not** clear on a retry,
///   because a retry re-derives the same `id`.
///
/// A deployer-set `default_transaction_isolation = repeatable read` would be a
/// third cause — the read-back could not see a winner that committed after this
/// transaction opened — but [`crate::infra::storage::pool`] forces the level per
/// connection, so no server setting reaches this arm.
///
/// Nothing here can tell them apart; `inc_dedup_stale`'s own doc says why none
/// of them gets a label or a counter of its own.
const CONFLICT_UNREADABLE_MESSAGE: &str =
    "conflicting record could not be read back during dedup resolution; retry";

/// The key both write paths decide identity on: the entry's own `id`.
///
/// The `id` arrives on the wire, derived above the SPI by
/// `usage_collector_sdk::v2::id::derive_usage_record_id` — the canonical home of the
/// dedup identity. **This plugin neither re-derives nor validates it**, so
/// everything below rests on the gateway having derived it faithfully. That is a
/// trust boundary worth naming.
///
/// What rests on it:
///
/// * **The in-batch comparison.** [`plan_batch`] collapses a batch to one
///   representative per `id` and [`PgRecordStore::resolve_batch`] resolves every
///   later row against it. Keying on the `id` rather than the 6-tuple makes "two
///   entries of one batch are the same entry" and "two entries of one batch take
///   one ledger row" the *same* statement: the guarded statement joins inserted
///   rows back to input on `id` (`LEFT JOIN ins USING (id)`), which has no
///   coherent reading for two rows that agree on `id` and differ in their six
///   inputs.
/// * **The conflict read-back.** Both paths read the row that took a lost slot
///   by `id`, per `DESIGN.md` §3.6: *"The read-back keys on `id`, never on
///   `(tenant_id, gts_type_id, idempotency_key, window_start, window_end)`"* —
///   the five alone cannot tell a withdrawn record from its withdrawal.
///
/// What does **not** rest on it is the ledger's own uniqueness: the
/// `ON CONFLICT` arbiter is
/// [`DEDUP_CONFLICT_TARGET`](crate::infra::storage::columns::DEDUP_CONFLICT_TARGET),
/// so the database decides what collides from stored values rather than from a
/// caller's assertion.
///
/// **What a mis-derived `id` costs** — the risk this trust accepts:
///
/// * **Its six inputs are already stored.** It collides on the arbiter and
///   `DO NOTHING` skips the insert, so no other unique index is reached. The
///   read-back then looks for the winner by the mis-derived `id`:
///   * **No row carries it** (the ordinary case) — a `Transient` reading
///     [`CONFLICT_UNREADABLE_MESSAGE`], counted on
///     `uc_timescaledb_dedup_stale_total`. It **does not self-heal**: every
///     retry re-derives the same `id` into the same arm.
///   * **Some other entry carries it** under the same covered-period end — the
///     caller receives an `IdempotencyConflict` carrying another identity's
///     stored entry (possibly another tenant's, the read-back binding only `id`
///     and period). Nothing detects it.
/// * **Its six inputs are novel.** The insert proceeds and the other unique
///   indexes are checked:
///   * A collision on `(id, window_end, type_key)` raises on the PRIMARY KEY,
///     which the arbiter does not cover. The batch lifts it to a `Transient` and
///     re-runs, which **does not clear it** —
///     [`MAX_BATCH_ATTEMPTS`](crate::infra::storage::retry::MAX_BATCH_ATTEMPTS)
///     is spent before the error reaches the host. Here the ledger refuses the
///     row.
///   * Otherwise it is **stored** and nothing catches it: every later faithful
///     submission of that entry collides on the arbiter and then cannot find it.
///     One mis-derivation is permanent for one identity.
///
/// Re-deriving here would put a second derivation beside the SDK's, which the
/// identity-derivation ADR places above the SPI.
///
/// **One route into the first arm is not a mis-derivation at all:** where two
/// identifiers share one registry reference (ADR-0001's accepted residual), an
/// entry under the second that agrees with a stored one on tenant, idempotency
/// key, covered period and kind collides on the arbiter — which reads the shared
/// reference off the row — while its faithfully derived `id` matches no stored
/// row. A perfectly faithful gateway reaches the never-clearing `Transient` that
/// way.
///
/// **No label or sibling counter distinguishes these**; `inc_dedup_stale`'s own
/// doc says why.
// @cpt-state:cpt-cf-uc-plugin-state-dedup-identity:p2
pub(super) const fn entry_identity(record: &StoredUsageRecord) -> Uuid {
    record.id
}

/// What the guarded statement said about one input row: the two flags it
/// carries out, decoded into the three outcomes `DESIGN.md` §3.6 names.
///
/// A sum type rather than the flags themselves: only three of their four
/// combinations are reachable, since `WHERE admitted` feeds the insert and a
/// refused row cannot have won.
enum Admission {
    /// Outside the acceptance slack. Refused as `Transient` whatever the ledger
    /// holds, because the verdict precedes the identity (§3.6).
    Stale,
    /// Admitted, and this row took the dedup slot: a fresh insert, with the
    /// `xact_id` the column default stamped on it.
    Won(Box<UsageRecordRow>),
    /// Admitted, but the identity was already stored. Resolved by reading the
    /// stored row back and comparing caller-supplied fields.
    Lost,
}

/// Decode one row of the guarded statement's outer select into an
/// [`Admission`].
///
/// **`admitted` is read first and short-circuits**, the ordering rule §3.6
/// states as *"a row not admitted is `Transient` … even when its identity
/// exists"*. The statement already enforces it; reading in this order means no
/// decode path can reintroduce the other order.
///
/// The record columns are decoded only on the won arm: on the other two `ins`
/// contributed no row to the left join, so every one of them is `NULL` and
/// [`UsageRecordRow`] could not be built from them.
fn admission_of(row: &PgRow) -> Result<Admission, sqlx::Error> {
    if !row.try_get::<bool, _>("admitted")? {
        return Ok(Admission::Stale);
    }
    if row.try_get::<bool, _>("won")? {
        return Ok(Admission::Won(Box::new(UsageRecordRow::from_row(row)?)));
    }
    Ok(Admission::Lost)
}

/// Outcome of reading the existing `usage_records` row for a lost identity.
enum ConflictRead {
    /// The conflicting row exists — resolve absorb vs conflict against it.
    Stored(Box<UsageRecordRow>),
    /// It could not be read. More than one thing does that, they answer alike,
    /// and not all of them clear on a retry — [`CONFLICT_UNREADABLE_MESSAGE`]
    /// sets them out. The name is the oldest of them.
    Stale,
}

/// Deterministic plan for a batch insert.
///
/// `reps` are the first-occurrence representative records, one per distinct
/// [`entry_identity`], **sorted** by it so concurrent batches take the
/// 6-tuple-UNIQUE speculative tuple locks in one global order (deadlock-free).
/// The sort is the whole of this plugin's deadlock-freedom argument: the
/// partition-key assignment two batches can also meet on runs in autocommit
/// before either write transaction opens and is held for the statement rather
/// than to a commit ([`crate::infra::storage::pool`]), so it cannot be an edge
/// of a cycle.
///
/// **Any total order over the key buys deadlock-freedom**, and this one is the
/// `id`'s own `Ord`. It carries no meaning: a `UUIDv5` orders by its digest
/// bytes, so a record and its withdrawal fall either way round. Nothing
/// observable depends on that — a pair written in one batch shares one
/// transaction, so feed order ties on `xact_id` and falls back to `id` (this
/// plugin's DESIGN §3.6). The gear's "an invalidation follows its target"
/// invariant is realized on `xact_id`: the gateway admits an invalidation only
/// once its target has converged, so the withdrawal commits in a later
/// transaction and takes a greater `xact_id`.
///
/// `first_index` maps each identity to the input index of its first occurrence,
/// the only row that can win the slot. Later same-identity rows resolve against
/// the winner's stored row through [`PgRecordStore::resolve_dedup_hit`].
struct BatchPlan<'a> {
    reps: Vec<&'a MeteredRecord>,
    first_index: HashMap<Uuid, usize>,
}

/// Collapse a batch to its distinct [`entry_identity`] values (first occurrence
/// wins), sorted for a stable lock order. Pure — no DB. `reps` borrow from
/// `records`.
///
/// Two withdrawals of one target in one batch repeat that target's five shared
/// components under `entry_type = invalidation`, so all six inputs agree, so
/// they derive one `id`: they share one identity and one slot, and the later
/// resolves against the earlier like any other same-identity pair. A record and
/// its withdrawal differ in the sixth input and derive two, which is what keeps
/// a batch carrying both from swallowing one.
fn plan_batch(records: &[MeteredRecord]) -> BatchPlan<'_> {
    let mut first_index: HashMap<Uuid, usize> = HashMap::new();
    let mut reps: Vec<&MeteredRecord> = Vec::new();
    for (i, pair) in records.iter().enumerate() {
        if let std::collections::hash_map::Entry::Vacant(slot) =
            first_index.entry(entry_identity(pair.record()))
        {
            slot.insert(i);
            reps.push(pair);
        }
    }
    reps.sort_by_key(|paired| entry_identity(paired.record()));
    BatchPlan { reps, first_index }
}

/// One per-column vector per column of
/// [`INSERT_COLUMNS`](crate::infra::storage::columns::INSERT_COLUMNS), which a
/// multi-row insert binds.
///
/// `sqlx` binds arrays, not rows, so the batch insert `UNNEST`s these back into
/// rows. One struct built by one function keeps the column list, the `UNNEST`
/// list and the bind order readable side by side.
struct InsertColumns {
    ids: Vec<Uuid>,
    tenants: Vec<Uuid>,
    /// The registry reference each entry's meter was dispatched under, taken
    /// from the call's own [`MeterRef`].
    gts_type_uuids: Vec<Uuid>,
    type_keys: Vec<i32>,
    quantities: Vec<Decimal>,
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
    /// Borrowed rather than owned: `EntryType::as_str` hands back a
    /// `&'static str`, so there is nothing here to clone.
    entry_types: Vec<&'static str>,
    accepted_ats: Vec<OffsetDateTime>,
    metadata: Vec<String>,
}

impl InsertColumns {
    /// Pivot `reps` (plus the partition keys resolved for them, in the same
    /// order) into per-column vectors.
    ///
    /// `metadata` is carried as `text[]` of JSON strings and cast `::jsonb`
    /// per-row in the query, to sidestep `jsonb[]` array encoding. The
    /// invalidation pair goes through [`invalidation_to_row`] rather than being
    /// read out of the record twice, so the two columns cannot drift apart.
    ///
    /// # Panics
    ///
    /// Never in practice: `type_keys` comes from a resolve loop over the same
    /// `reps`. Panicking beats silently writing a wrong partition key, which
    /// would put the row in the wrong chunk and out of reach of the unique
    /// constraints that carry it.
    fn build(reps: &[&MeteredRecord], type_keys: &[i32]) -> Self {
        assert_eq!(
            reps.len(),
            type_keys.len(),
            "one partition key must be resolved per batch representative"
        );
        let mut cols = Self {
            ids: Vec::with_capacity(reps.len()),
            tenants: Vec::with_capacity(reps.len()),
            gts_type_uuids: Vec::with_capacity(reps.len()),
            type_keys: type_keys.to_vec(),
            quantities: Vec::with_capacity(reps.len()),
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
            entry_types: Vec::with_capacity(reps.len()),
            accepted_ats: Vec::with_capacity(reps.len()),
            metadata: Vec::with_capacity(reps.len()),
        };
        for (meter, r) in reps.iter().map(|p| (p.meter(), p.record())) {
            let (invalidates, reason_code) = invalidation_to_row(r.invalidation.as_ref());
            cols.ids.push(r.id);
            cols.tenants.push(r.tenant_id);
            cols.gts_type_uuids.push(meter.uuid);
            cols.quantities.push(r.quantity.as_decimal());
            cols.window_starts.push(r.window_start);
            cols.window_ends.push(r.window_end);
            cols.resource_ids
                .push(r.resource_ref.resource_id().to_owned());
            cols.resource_types
                .push(r.resource_ref.resource_type().to_owned());
            cols.subject_ids.push(
                r.subject_ref
                    .as_ref()
                    .map(|sr| usage_collector_sdk::v2::SubjectRef::subject_id(sr).to_owned()),
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
            cols.entry_types.push(r.entry_type().as_str());
            cols.accepted_ats.push(r.accepted_at);
            cols.metadata
                .push(metadata_map_to_jsonb(&r.metadata).to_string());
        }
        cols
    }
}

/// Open a write transaction and force its durability before anything writes.
///
/// One helper, so the two write paths cannot drift in whether they issue
/// [`FORCE_SYNCHRONOUS_COMMIT_SQL`] or in what they issue it before. Both
/// resolve their type keys in autocommit first, so the transaction this opens
/// carries the guarded statement and nothing ahead of it but the `SET LOCAL`.
///
/// Errors come back as the raw `sqlx::Error`: the caller owns the mapping.
// @cpt-dod:cpt-cf-uc-plugin-dod-durable-acknowledgement:p1
async fn begin_durable_write(
    conn: &mut PoolConnection<Postgres>,
) -> Result<sqlx::Transaction<'_, Postgres>, sqlx::Error> {
    let mut tx = conn.begin().await?;
    if let Err(e) = sqlx::query(FORCE_SYNCHRONOUS_COMMIT_SQL)
        .execute(&mut *tx)
        .await
    {
        rollback(tx).await;
        return Err(e);
    }
    Ok(tx)
}

/// Roll `tx` back now, logging a failure rather than propagating it.
///
/// Dropping a `Transaction` rolls it back too, but lazily — the `ROLLBACK` is
/// queued until the connection is next used. Every caller here is about to
/// return the connection to the pool after a failure, so "now" is the property
/// that matters. A rollback that itself fails says the connection is gone; the
/// pool discards it, and the caller's original error is the one worth returning.
async fn rollback(tx: sqlx::Transaction<'_, Postgres>) {
    if let Err(err) = tx.rollback().await {
        tracing::warn!(
            error = %err,
            "rolling back a usage-record write transaction failed"
        );
    }
}

/// Log a retryable write-path transient at `warn` with the record's identifiers,
/// then return the matching [`UsageCollectorPluginError::Transient`]. `warn`
/// because not every such transient self-heals on a retry
/// ([`CONFLICT_UNREADABLE_MESSAGE`]). This helper only logs and builds the
/// error: the counter is the caller's — `inc_dedup_stale` on the
/// unreadable-conflict sites, `inc_stale_acceptance_rejection` on the guard's,
/// none on the defensive not-found arm, which is unreachable by construction.
// @cpt-algo:cpt-cf-uc-plugin-algo-label-cardinality-admission:p2
// @cpt-dod:cpt-cf-uc-plugin-dod-bounded-label-cardinality:p2
fn write_transient(
    meter: &MeterRef,
    record: &StoredUsageRecord,
    msg: &'static str,
) -> UsageCollectorPluginError {
    tracing::warn!(
        tenant_id = %record.tenant_id,
        gts_type_uuid = %meter.uuid,
        idempotency_key = %record.idempotency_key.as_str(),
        "{msg}"
    );
    UsageCollectorPluginError::transient(msg)
}

impl PgRecordStore {
    /// Resolve a hit on a stored dedup identity into absorb or conflict.
    ///
    /// The stored row is mapped into a [`StoredUsageRecord`] and compared with
    /// [`StoredUsageRecord::caller_supplied_eq`], the SDK's one definition of the
    /// comparison, so `origin` and `accepted_at` are ignored and the quantity
    /// compares digit for digit. Equal is absorbed and answers with the stored
    /// entry; different is
    /// [`UsageCollectorPluginError::IdempotencyConflict`] carrying it. A stored
    /// row that cannot be mapped (corrupt metadata, say) is `Internal`.
    ///
    /// **The `id`s are not compared, because they cannot differ.** Every caller
    /// selected this row by the very identity it is being resolved against:
    /// [`Self::read_conflict_records`] keys its map on `row.id`, read back under
    /// [`entry_identity`], and the in-batch arm passes the row the statement
    /// inserted under that same identity.
    // @cpt-algo:cpt-cf-uc-plugin-algo-duplicate-identity-resolution:p1
    // @cpt-dod:cpt-cf-uc-plugin-dod-duplicate-resolution:p1
    fn resolve_dedup_hit(
        &self,
        row: UsageRecordRow,
        record: &StoredUsageRecord,
    ) -> Result<StoredUsageRecord, UsageCollectorPluginError> {
        let stored = record_row_to_model(row)?;
        if stored.caller_supplied_eq(record) {
            self.metrics.inc_dedup_absorbed();
            Ok(stored)
        } else {
            self.metrics.inc_idempotency_conflict();
            Err(UsageCollectorPluginError::idempotency_conflict(
                record.idempotency_key.clone(),
                stored,
            ))
        }
    }

    /// Run the guarded statement ([`BATCH_GUARDED_SQL`]) over all distinct-key
    /// representatives, and return one [`Admission`] per representative.
    ///
    /// The statement reports each input row's verdict and whether it won, so
    /// this is the whole of what the batch learns from the write: a refused
    /// row, a won slot with its stored row, or a lost slot to be read back.
    /// **There is no Rust-side `inserted.contains_key` pass** — the outer
    /// select's `LEFT JOIN ins USING (id)` is what aligns inserted rows to
    /// input, and `input.id` is how a returned row names the representative it
    /// came from.
    ///
    /// `reps` must be sorted by [`entry_identity`] so concurrent batches take
    /// the speculative tuple locks in one global order (deadlock-free), and
    /// `type_keys` must be the partition keys resolved for `reps`, in the same
    /// order.
    ///
    /// Errors come back as the raw `sqlx::Error` rather than mapped: the caller
    /// holds the transaction that has to be rolled back first.
    ///
    /// **A PRIMARY KEY collision is the caller's to handle, not this
    /// function's.** The arbiter covers the dedup UNIQUE only, so a write that
    /// meets a row on the primary-key index surfaces as a `23505` rather than as
    /// an absent `ins` row. There is nothing to resolve in place — a failed
    /// statement reports no verdict at all — so [`Self::create_batch_inner`]
    /// lifts it to a `Transient` ([`PgRecordStore::record_insert_error`]) and
    /// the whole batch re-runs on a fresh transaction.
    async fn run_guarded_batch_write(
        tx: &mut sqlx::Transaction<'_, Postgres>,
        reps: &[&MeteredRecord],
        type_keys: &[i32],
        acceptance_slack_secs: i64,
    ) -> Result<HashMap<Uuid, Admission>, sqlx::Error> {
        if reps.is_empty() {
            return Ok(HashMap::new());
        }
        let cols = InsertColumns::build(reps, type_keys);

        let rows = sqlx::query(AssertSqlSafe(BATCH_GUARDED_SQL.as_str()))
            .bind(&cols.ids)
            .bind(&cols.tenants)
            .bind(&cols.gts_type_uuids)
            .bind(&cols.type_keys)
            .bind(&cols.quantities)
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
            .bind(&cols.entry_types)
            .bind(&cols.accepted_ats)
            .bind(&cols.metadata)
            .bind(acceptance_slack_secs)
            .fetch_all(&mut **tx)
            .await?;

        // The walk is over `reps` rather than the returned rows, so a key only
        // enters the map from a representative this batch actually holds; a
        // representative with no row leaves no entry, which `resolve_batch`'s
        // defensive arm answers as retryable rather than as a silent success.
        let mut by_input_id: HashMap<Uuid, &PgRow> = HashMap::with_capacity(rows.len());
        for row in &rows {
            by_input_id.insert(row.try_get("input_id")?, row);
        }
        let mut out: HashMap<Uuid, Admission> = HashMap::with_capacity(reps.len());
        for rep in reps {
            let id = entry_identity(rep.record());
            if let Some(row) = by_input_id.get(&id) {
                out.insert(id, admission_of(row)?);
            }
        }
        Ok(out)
    }

    /// For the admitted, lost identities, read the existing `usage_records` row
    /// by `id` — the key `DESIGN.md` §3.6 prescribes for the conflict branch:
    /// the five columns a withdrawn record shares with its withdrawal cannot
    /// tell the pair apart, and the `id` can, because the entry kind is one of
    /// the six inputs it is derived over ([`entry_identity`]).
    ///
    /// The covered-period end rides along for the reason
    /// [`PARTITION_PRUNE_COLUMN`](crate::infra::storage::columns::PARTITION_PRUNE_COLUMN)
    /// gives; [`BATCH_CONFLICT_READ_SQL`] is where the predicate's two conjuncts
    /// are explained and measured.
    ///
    /// Maps each identity to `Stored` (row found → resolve absorb/conflict) or
    /// `Stale` (the row this one conflicted with could not be read — see
    /// [`CONFLICT_UNREADABLE_MESSAGE`] for what does that).
    async fn read_conflict_records(
        &self,
        tx: &mut sqlx::Transaction<'_, Postgres>,
        lost: &[&MeteredRecord],
    ) -> Result<HashMap<Uuid, ConflictRead>, UsageCollectorPluginError> {
        let mut out: HashMap<Uuid, ConflictRead> = HashMap::new();
        if lost.is_empty() {
            return Ok(out);
        }

        let ids: Vec<Uuid> = lost.iter().map(|p| entry_identity(p.record())).collect();
        let window_ends: Vec<OffsetDateTime> = lost.iter().map(|p| p.record().window_end).collect();
        let rows =
            sqlx::query_as::<_, UsageRecordRow>(AssertSqlSafe(BATCH_CONFLICT_READ_SQL.as_str()))
                .bind(&ids)
                .bind(&window_ends)
                .fetch_all(&mut **tx)
                .await
                .map_err(|e| self.record_backend_error(&e))?;

        let mut found: HashMap<Uuid, UsageRecordRow> = HashMap::new();
        for row in rows {
            found.insert(row.id, row);
        }

        // `reps` are distinct identities, so each `remove` is unambiguous.
        for id in ids {
            let read = found.remove(&id).map_or(ConflictRead::Stale, |row| {
                ConflictRead::Stored(Box::new(row))
            });
            out.insert(id, read);
        }

        Ok(out)
    }

    /// Resolve every input row in original order against its authoritative
    /// record, recording the per-row counters once per row — not once per
    /// distinct identity, which is what the walk below is over `records`
    /// rather than over `plan.reps` for.
    ///
    /// `admissions` is what the guarded statement said about each distinct key
    /// — refused, won with its stored row, or lost — so there is no separate
    /// `won` set to disagree with it.
    ///
    /// **A refusal is per input row, not per identity**, which is what
    /// `cpt-cf-uc-plugin-seq-ingest-batch` asks for: *"a conflict or rejection
    /// on one record never fails the others"*. Two input rows sharing a refused
    /// identity are two refusals and two counter increments, exactly as two
    /// input rows sharing an absorbed identity are two absorbs. Asserted by
    /// `acceptance_slack_pg::a_batch_counts_every_refused_row_not_every_refused_identity`.
    // @cpt-algo:cpt-cf-uc-plugin-algo-in-batch-identity-resolution:p1
    // @cpt-dod:cpt-cf-uc-plugin-dod-batch-positional-results:p1
    fn resolve_batch(
        &self,
        records: &[MeteredRecord],
        plan: &BatchPlan<'_>,
        admissions: &HashMap<Uuid, Admission>,
        conflict: &HashMap<Uuid, ConflictRead>,
    ) -> Vec<Result<StoredUsageRecord, UsageCollectorPluginError>> {
        let mut results = Vec::with_capacity(records.len());
        for (i, (meter, record)) in records.iter().map(|p| (p.meter(), p.record())).enumerate() {
            let key = entry_identity(record);
            let outcome = match admissions.get(&key) {
                // Refused by the statement's own acceptance guard. Nothing was
                // written for this key and no ledger row was consulted for it,
                // whether or not its identity is already stored.
                Some(Admission::Stale) => {
                    self.metrics.inc_stale_acceptance_rejection();
                    Err(write_transient(meter, record, STALE_ACCEPTANCE_MESSAGE))
                }
                // We won this slot and this input row is its first occurrence:
                // the fresh insert.
                Some(Admission::Won(row)) if plan.first_index.get(&key) == Some(&i) => {
                    if record.invalidation.is_some() {
                        self.metrics.inc_invalidation();
                    }
                    record_row_to_model((**row).clone())
                }
                // An earlier input row won this slot: an in-batch duplicate,
                // resolved against the row just written by the same
                // absorb-vs-conflict comparison a stored-row hit takes.
                Some(Admission::Won(row)) => self.resolve_dedup_hit((**row).clone(), record),
                // Lost the slot, or — defensively — the statement returned no
                // verdict for this key at all, which `run_guarded_batch_write`
                // cannot produce. Both resolve against the conflict read, and
                // the second finds nothing there and answers retryable rather
                // than silently succeeding.
                Some(Admission::Lost) | None => match conflict.get(&key) {
                    Some(ConflictRead::Stored(row)) => {
                        // Clone the inner row directly; `*row.clone()` would
                        // round-trip through a throwaway `Box`. The clone is
                        // required — several input rows may resolve one not-won
                        // key against this borrowed map.
                        self.resolve_dedup_hit((**row).clone(), record)
                    }
                    Some(ConflictRead::Stale) => {
                        self.metrics.inc_dedup_stale();
                        Err(write_transient(meter, record, CONFLICT_UNREADABLE_MESSAGE))
                    }
                    None => Err(write_transient(
                        meter,
                        record,
                        "conflicting record not found during dedup resolution; retry",
                    )),
                },
            };
            results.push(outcome);
        }
        results
    }

    /// Orchestrate one batch inside **one transaction**: run the guarded
    /// statement → read conflicts for the lost keys → commit → resolve per row
    /// in input order. The statement's own per-row verdicts are the whole
    /// record of what it did, so nothing else records that.
    ///
    /// The transaction is not decoration. The write and the conflict read are
    /// one unit of work: the read has to see the ledger the write just met, and
    /// every entry of the batch has to share one `xact_id` — which the column
    /// default gives only because one `pg_current_xact_id()` covers the whole
    /// transaction (this plugin's DESIGN §3.6). A per-row transaction would
    /// scatter the batch across the feed order.
    ///
    /// **No SAVEPOINT, because there is nothing to resolve in place.** A failed
    /// statement reports no verdicts at all, so no point to roll back to would
    /// leave anything worth reading. The transaction is rolled back whole, the
    /// failure is lifted to a `Transient`, and the batch re-runs on a fresh
    /// transaction (see [`Self::run_guarded_batch_write`]).
    // @cpt-algo:cpt-cf-uc-plugin-algo-batch-transient-retry:p1
    pub(super) async fn create_batch_inner(
        &self,
        records: &[MeteredRecord],
    ) -> Result<Vec<Result<StoredUsageRecord, UsageCollectorPluginError>>, UsageCollectorPluginError>
    {
        let plan = plan_batch(records);

        let mut conn = self.timed_acquire().await?;

        // One partition key per representative, aligned to `plan.reps`, resolved
        // in autocommit before the write transaction opens. A type already seen
        // costs no round trip.
        let mut type_keys: Vec<i32> = Vec::with_capacity(plan.reps.len());
        for paired in &plan.reps {
            let key = self
                .type_keys
                .resolve(&mut conn, paired.meter().uuid)
                .await
                .map_err(|e| self.record_backend_error(&e))?;
            type_keys.push(key);
        }

        // One transaction, opened with `SET LOCAL synchronous_commit = on` as
        // its first statement, so a returned COMMIT is durable
        // ([`begin_durable_write`]).
        let mut tx = begin_durable_write(&mut conn)
            .await
            .map_err(|e| self.record_backend_error(&e))?;

        let admissions = match Self::run_guarded_batch_write(
            &mut tx,
            &plan.reps,
            &type_keys,
            self.acceptance_slack_secs,
        )
        .await
        {
            Ok(admissions) => admissions,
            Err(e) => {
                rollback(tx).await;
                return Err(self.record_insert_error(&e));
            }
        };
        // Only the **admitted, not-won** keys are read back, per
        // `cpt-cf-uc-plugin-seq-ingest-batch`: a refused key has no outcome to
        // resolve against the ledger, and reading one would be the ordering the
        // guard's precedence forbids.
        let lost: Vec<&MeteredRecord> = plan
            .reps
            .iter()
            .copied()
            .filter(|p| {
                matches!(
                    admissions.get(&entry_identity(p.record())),
                    Some(Admission::Lost)
                )
            })
            .collect();
        let conflict = match self.read_conflict_records(&mut tx, &lost).await {
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

        Ok(self.resolve_batch(records, &plan, &admissions, &conflict))
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "write_tests.rs"]
mod write_tests;
