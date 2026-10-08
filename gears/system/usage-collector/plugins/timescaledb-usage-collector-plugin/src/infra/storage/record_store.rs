//! Postgres-backed [`RecordStore`] over the `usage_records` hypertable.
//!
//! All operations — `create` / `create_batch` / `get` / `list` / `aggregate` —
//! are real `sqlx`.
//!
//! The free SQL-builder functions here were considered for a sibling module and
//! left in place: no name describes the boundary except "these don't touch
//! `sqlx`". Re-open it if this file becomes hard to *edit*, not on line count.

// Vendored TimescaleDB raw-SQL backend: `sqlx` is required infra (hypertable
// time-series, `time_bucket` aggregation, keyset pagination — see DESIGN.md). Tenant
// isolation is enforced by hand via parameterized `tenant_id` predicates and an
// allowlisted-identifier query builder (DESIGN.md §Injection-Safe Query Translation),
// not SecureConn/AccessScope.
#![allow(unknown_lints, de0706_no_direct_sqlx)]

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::num::NonZeroU64;
use std::sync::{Arc, LazyLock};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use bigdecimal::BigDecimal;
use rand::RngExt as _;
use rust_decimal::Decimal;
use sqlx::pool::PoolConnection;
use sqlx::postgres::{PgConnection, PgRow};
use sqlx::{Acquire as _, AssertSqlSafe, Connection as _, FromRow as _, PgPool, Postgres, Row};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use tokio_util::sync::CancellationToken;
use toolkit_odata::{ODataQuery, SortDir, ast};
use uuid::Uuid;

use usage_collector_sdk::{
    AggregationBucket, AggregationDimension, AggregationFold, AggregationResult, Keyset,
    MetadataFilter, MeterRef, ObservedQuantity, QuantitySummary, ReconciliationMetadata,
    RecordPage, StoredUsageRecord, TimeRange, UsageCollectorPluginError, UsageQuantity,
    is_keyset_safe_record_field,
};

use crate::domain::feed_position::MAX_UUID;
use crate::domain::ports::{FeedPageRows, RecordStore};
use crate::infra::metrics::{ErrorClass, Metrics, OpDurationGuard, QueryKind, TimedOp};
use crate::infra::storage::entity::{FeedRecordRow, UsageRecordRow};
use crate::infra::storage::error::{
    acquire_error_clears_readiness, is_ledger_pk_violation, map_sqlx_err,
};
use crate::infra::storage::mapper::{
    invalidation_to_row, metadata_map_to_jsonb, record_row_to_model,
};
use crate::infra::storage::query::aggregate::{
    aggregate_limit_clause, dimension_presence_guard, dimension_select_expr, fold_select_expr,
    withdrawal_exclusion_clause,
};
use crate::infra::storage::query::feed::{MARK_ABOVE_SQL, build_feed_page_sql};
use crate::infra::storage::query::keyset::{keyset_predicate, render_order_by, uniform_dir};
use crate::infra::storage::query::reconciliation::{
    accrued_summary_select_expr, observation_count_select_expr, observation_latest_select_expr,
};
use crate::infra::storage::query::rollup::{build_rollup_aggregate_sql, rollup_eligible};
use crate::infra::storage::query::translate::{
    SqlBind, SqlCtx, bind_one, bind_one_query, record_column, record_field_kind, translate_scope,
};
use crate::infra::storage::query::{
    effective_page_size, ledger_from_clause, push_metadata_filter_clauses,
    push_meter_and_range_clauses, push_meter_clause,
};
use crate::infra::storage::type_key::TypeKeyCache;

/// Default page size when the caller omits `$top` (`query.limit`).
const DEFAULT_PAGE_SIZE: u64 = 100;

fn dedup_subscription_types(subscription: &[Uuid]) -> Vec<Uuid> {
    let mut seen = HashSet::with_capacity(subscription.len());
    let mut types = Vec::with_capacity(subscription.len());
    for m in subscription {
        if seen.insert(*m) {
            types.push(*m);
        }
    }
    types
}

/// Column list for `get`, `list`, and every write path's conflict read-back
/// and `RETURNING`, in [`UsageRecordRow`] field order. A static const (never
/// caller input), so there is no risk of SQL injection.
///
/// `sqlx`'s derived `FromRow` looks each column up by field name, so the order
/// is a reading convenience. **Omission is the hazard**: a missing column fails
/// the decode with `no column found for name: <field>`.
///
/// Same column set as [`INSERT_COLUMNS`], cast where decoding needs it, not a
/// superset: `xact_id` — stamped by its column default, bound by no insert — is
/// carried by [`FEED_COLUMNS`] instead.
///
/// **One entry is cast rather than named bare**, and the cast is what makes the
/// column decodable at all: `usage_entry_type` is a `PostgreSQL` enum and
/// [`UsageRecordRow`] carries a [`String`], which `sqlx` holds incompatible with
/// an enum. Hence `entry_type::text AS entry_type`. The alias is written out,
/// though `PostgreSQL` would supply it, so the decoded name is stated here
/// rather than inherited — [`ins_record_columns`] parses exactly that decoded
/// name back out of this constant to qualify it with `ins`.
pub(crate) const RECORD_COLUMNS: &str = "id, tenant_id, gts_type_uuid, type_key, quantity, \
     window_start, window_end, resource_id, resource_type, subject_id, subject_type, \
     idempotency_key, invalidates, reason_code, origin, entry_type::text AS entry_type, \
     accepted_at, metadata";

/// [`RECORD_COLUMNS`] plus `xact_id`, for the one reader that needs the feed
/// order's own key: `query/feed.rs`'s page statement. `xact_id` is the whole of
/// the difference between the two lists.
///
/// **The cast is aliased `xact_id_text`, not `xact_id`, and that is
/// load-bearing.** `query/feed.rs` orders by the bare name `xact_id`
/// (`ORDER BY xact_id, id`, over `usage_records_feed_idx`), and `PostgreSQL`
/// resolves a bare `ORDER BY` name matching both an output and an input column
/// to the *output* one. Aliasing this cast back to `xact_id` would make that
/// `ORDER BY` sort lexicographically over digit strings rather than numerically
/// over the `xid8` column — the same hazard
/// [`crate::infra::storage::retention_sweep::CHUNK_HIGHEST_POSITIONS_SQL`]
/// avoids with its own `xact_id_text` alias. [`FeedRecordRow::xact_id`]'s
/// `#[sqlx(rename)]` lets the field keep its expected name while decoding from
/// the differently named column.
///
/// `xid8` has no `sqlx` `Decode` implementation, which is why the column is
/// cast to `text` at all — the same reason [`FeedRecordRow::xact_id`] is a
/// [`String`].
///
/// `pub(crate)` so `query::feed`'s page-statement builder reads the same
/// spelling rather than a second one.
pub(crate) const FEED_COLUMNS: &str = "id, tenant_id, gts_type_uuid, type_key, quantity, \
     window_start, window_end, resource_id, resource_type, subject_id, subject_type, \
     idempotency_key, invalidates, reason_code, origin, entry_type::text AS entry_type, \
     accepted_at, metadata, xact_id::text AS xact_id_text";

/// The columns every insert writes: every ledger column but `xact_id`, which
/// the database stamps.
///
/// **One spelling for every place a write statement names its columns** — each
/// path's `input` CTE select list, the `INSERT`'s own column list, the `SELECT`
/// that feeds it from `input`, and the alias list of the `VALUES` row or
/// `UNNEST` the input is built from ([`guarded_statement`]). Spelled out
/// separately, a name transposed in any one of them binds a value to the wrong
/// same-typed column, which Postgres accepts without complaint and no row-level
/// test can see.
///
/// The order is `0001_init.sql`'s declaration order. `metadata` being last is
/// **not** load-bearing and must not become so: [`input_select_list`] names the
/// cast column explicitly rather than appending `::jsonb AS metadata` to the
/// whole string.
const INSERT_COLUMNS: &str = "id, tenant_id, gts_type_uuid, type_key, quantity, window_start, \
     window_end, resource_id, resource_type, subject_id, subject_type, idempotency_key, \
     invalidates, reason_code, origin, entry_type, accepted_at, metadata";

/// The `PostgreSQL` enum `entry_type` is declared as
/// (`migrations/0001_init.sql`), and the cast every bind of that column
/// carries.
///
/// `sqlx` types a bound `&str` as `text`, and `PostgreSQL` refuses `text` in
/// assignment to an enum column (`42804`), so a bare `$n` does not land; an
/// explicit cast does. Hence [`INSERT_COLUMN_TYPES`] names the enum where the
/// DDL does rather than diverging to `text` the way `metadata` does — the write
/// reads it as `UNNEST($n::usage_entry_type[])`.
///
/// `pub(crate)` so the `$filter` translator's `bind_cast`
/// ([`super::query::translate`]) spells it from here too. Test oracles spell it
/// out instead, deliberately: one derived from this const could not see a
/// rename, because both sides would move together.
pub(crate) const ENTRY_TYPE_ENUM: &str = "usage_entry_type";

/// Postgres types for [`INSERT_COLUMNS`], **in the same order**, as the write
/// path's `input` source needs them: [`batch_input_source`] `UNNEST`s the array
/// of each (`text` becomes `text[]`). A test pins its length equal to the
/// number of names in [`INSERT_COLUMNS`], which is what fixes the parameter
/// count.
///
/// Named for the columns rather than the arrays: [`super::query::translate`]
/// reads the same names to cast a scalar bind.
const INSERT_COLUMN_TYPES: [&str; 18] = [
    "uuid",
    "uuid",
    "uuid",
    "int",
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
    ENTRY_TYPE_ENUM,
    "timestamptz",
    "text",
];

/// The dedup 6-tuple plus the partition key the hypertable requires in every
/// UNIQUE, as an `ON CONFLICT` arbiter. Both write paths spend their one
/// arbiter here.
///
/// `entry_type` is one of the six, named here as the bare column: an arbiter
/// names index columns, never the values compared against them, so the enum
/// needs no cast here. Without it a withdrawal would arbitrate against the very
/// entry it withdraws, which repeats its tenant, type, idempotency key and
/// covered period.
const DEDUP_CONFLICT_TARGET: &str =
    "tenant_id, gts_type_uuid, idempotency_key, window_start, window_end, entry_type, type_key";

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

/// The ledger's **time** partition column, named beside the `id` in both
/// conflict read-backs **for pruning and for nothing else**.
///
/// `usage_records` is a hypertable with two range dimensions,
/// `by_range('window_end')` and `by_range('type_key', …)`
/// (`migrations/0001_init.sql`). A predicate constraining neither cannot
/// exclude a chunk at all: `WHERE id = $1` alone probes the
/// `(id, window_end, type_key)` index of *every* chunk the ledger holds. Both
/// read-backs run **inside the write transaction**, holding its speculative
/// tuple locks, on the ordinary idempotent-retry path — the hold time
/// [`crate::infra::storage::pool`]'s `lock_timeout` reasoning is about.
///
/// Constraining this column prunes the **time** dimension to the chunks of the
/// entry's own period — one per `type_key` slice, since the second dimension is
/// left unconstrained. `type_key` is in scope at both read-backs and could be
/// bound too; that is a separate change with its own measurement to take.
///
/// **It is not a retreat to the 6-tuple.** The read-back keys on `id`
/// (`DESIGN.md` §3.6), and this column discriminates nothing `id` does not: the
/// covered-period end is one of the six inputs `id` is derived over, so two rows
/// agreeing on `id` agree on it. Naming it changes which chunks are scanned,
/// never which row is selected.
const PARTITION_PRUNE_COLUMN: &str = "window_end";

/// The `$n` the acceptance slack binds at: one past the last inserted column,
/// on both write paths.
///
/// Counted off [`INSERT_COLUMNS`] rather than written out, so the ledger
/// gaining or losing a written column moves it without anything being edited
/// twice.
fn slack_placeholder() -> usize {
    INSERT_COLUMNS.split(',').count() + 1
}

/// The admission guard `DESIGN.md` §3.6 requires **inside** the write
/// statement: the entry's `accepted_at` within `feed_acceptance_slack_secs` of
/// *that statement's own* `statement_timestamp()`, **in either direction**.
///
/// `abs(…)` is what makes it either direction. Dropped, an entry dated next year
/// is admitted; the sign flipped, the past-dated half goes unguarded.
/// `acceptance_slack_pg` carries one assertion per direction.
///
/// **One spelling, and the statement's own.** §3.6: *"Nothing before or after
/// the statement computes the guard, since a later statement would run under a
/// later `statement_timestamp()`."* It reads `t.accepted_at` off the `input`
/// relation rather than a placeholder, so the verdict is computed over the same
/// rows the INSERT selects from.
fn admitted_expr() -> String {
    format!(
        "(abs(extract(epoch FROM (t.accepted_at - statement_timestamp()))) <= ${}::bigint) \
         AS admitted",
        slack_placeholder()
    )
}

/// [`RECORD_COLUMNS`] as the outer select reads it back off `ins`.
///
/// The read list's one cast entry was already cast inside `ins`' `RETURNING`,
/// so `ins` exposes the **decoded** name — `entry_type` is a `text` column
/// there. Qualifying every name with `ins.` is what makes the not-won row's
/// columns `NULL` rather than silently picking up `input`'s like-named ones;
/// under `USING (id)` the merged `id` is the left side's and never null, so
/// `won` computed off an unqualified `id` would be true for every row.
fn ins_record_columns() -> String {
    RECORD_COLUMNS
        .split(',')
        .map(str::trim)
        .map(|entry| entry.rsplit_once(" AS ").map_or(entry, |(_, alias)| alias))
        .map(|name| format!("ins.{name}"))
        .collect::<Vec<_>>()
        .join(", ")
}

/// `DESIGN.md` §3.6's guarded write statement over `input_source`, which is the
/// **only** thing the two write paths differ in.
///
/// ```sql
/// WITH input AS (SELECT <INSERT_COLUMNS, metadata cast ::jsonb>, <admitted>
///                FROM <input_source>),
///      ins AS (INSERT INTO usage_records (<INSERT_COLUMNS>)
///              SELECT <INSERT_COLUMNS> FROM input WHERE admitted
///              ON CONFLICT (<DEDUP_CONFLICT_TARGET>) DO NOTHING
///              RETURNING <RECORD_COLUMNS>)
/// SELECT input.id AS input_id, input.admitted, (ins.id IS NOT NULL) AS won,
///        <RECORD_COLUMNS off ins>
/// FROM input LEFT JOIN ins USING (id)
/// ```
///
/// **`WHERE admitted` is what makes the verdict precede the identity.** A row
/// outside the slack never reaches `ON CONFLICT`, so it comes back
/// `admitted = false, won = false` whether or not its six inputs are already
/// stored, and the caller answers `Transient` without reading the ledger. §3.6:
/// *"a row not admitted is `Transient` … even when its identity exists"*.
///
/// **`input.id` is carried out** so the caller can align each returned row with
/// its input; `ins.id` is null on every row that did not win.
///
/// `xact_id` is bound nowhere and never read back here: the column default
/// stamps it from the inserting transaction, and only [`FEED_COLUMNS`]' one
/// reader needs the feed order's own key.
///
/// `input`'s `SELECT` list is [`input_select_list`]'s, so its columns are
/// [`INSERT_COLUMNS`]' names exactly and the inner `SELECT <INSERT_COLUMNS> FROM
/// input` cannot be a transposition of them.
// @cpt-algo:cpt-cf-uc-plugin-algo-guarded-insert-statement:p1
// @cpt-dod:cpt-cf-uc-plugin-dod-single-entry-persistence:p1
// @cpt-dod:cpt-cf-uc-plugin-dod-acceptance-slack-refusal:p1
fn guarded_statement(input_source: &str) -> String {
    format!(
        "WITH input AS (\
             SELECT {}, {} FROM {input_source}\
         ), \
         ins AS (\
             INSERT INTO usage_records ({INSERT_COLUMNS}) \
             SELECT {INSERT_COLUMNS} FROM input WHERE admitted \
             ON CONFLICT ({DEDUP_CONFLICT_TARGET}) DO NOTHING \
             RETURNING {RECORD_COLUMNS}\
         ) \
         SELECT input.id AS input_id, input.admitted, (ins.id IS NOT NULL) AS won, {} \
         FROM input LEFT JOIN ins USING (id)",
        input_select_list(),
        admitted_expr(),
        ins_record_columns(),
    )
}

/// [`INSERT_COLUMNS`] as the `input` CTE selects it: every column by its own
/// name, with `metadata` cast `::jsonb` and aliased back to itself.
///
/// **The cast is named rather than positional.** A trailing
/// `{INSERT_COLUMNS}::jsonb AS metadata` would bind the cast to whichever
/// identifier happens to be last, and breaks the moment the ledger declares a
/// column after `metadata`. Building the list from [`INSERT_COLUMNS`] itself,
/// one entry per name in its own order, keeps `input`'s columns exactly that
/// constant's names.
fn input_select_list() -> String {
    INSERT_COLUMNS
        .split(',')
        .map(str::trim)
        .map(|name| {
            if name == "metadata" {
                "metadata::jsonb AS metadata".to_owned()
            } else {
                name.to_owned()
            }
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// The batch path's `input` source: one array per column, `UNNEST`ed back into
/// rows and aliased to [`INSERT_COLUMNS`].
///
/// `sqlx` binds arrays, not rows. `UNNEST`'s parameters are
/// [`INSERT_COLUMN_TYPES`] in the same order, so `$n` is column `n`, and
/// the alias list is [`INSERT_COLUMNS`] itself, so the two cannot be transposed
/// relative to one another.
fn batch_input_source() -> String {
    let unnest = INSERT_COLUMN_TYPES
        .iter()
        .enumerate()
        .map(|(i, ty)| format!("${}::{ty}[]", i + 1))
        .collect::<Vec<_>>()
        .join(", ");
    format!("UNNEST({unnest}) AS t({INSERT_COLUMNS})")
}

/// The batch conflict read-back: the rows that took these identities' slots.
///
/// **Two conjuncts, and neither is redundant in practice.** The row-value `IN`
/// pairs each `id` with its own period, which a second `= ANY` could not — that
/// would match one entry's `id` under another's period. The leading
/// `= ANY` is what the planner can exclude chunks with; it is logically implied
/// by the row-value and is therefore easy to mistake for dead weight.
///
/// Measured on `timescale/timescaledb:2.29.2-pg18`: without the `= ANY`
/// conjunct the `Append` lists every chunk of the ledger; with it the `Append`
/// lists only the time chunks the batch's periods fall in (times the number of
/// `type_key` slices, for the reason [`PARTITION_PRUNE_COLUMN`] gives). Dropping
/// the conjunct makes the read-back touch every retained chunk, inside the write
/// transaction and while it holds the batch's speculative tuple locks.
static BATCH_CONFLICT_READ_SQL: LazyLock<String> = LazyLock::new(|| {
    format!(
        "SELECT {RECORD_COLUMNS} FROM usage_records \
         WHERE {PARTITION_PRUNE_COLUMN} = ANY($2::timestamptz[]) \
           AND (id, {PARTITION_PRUNE_COLUMN}) IN \
             (SELECT t1, t2 FROM UNNEST($1::uuid[], $2::timestamptz[]) AS t(t1, t2))"
    )
});

/// The batch guarded statement ([`guarded_statement`] over
/// [`batch_input_source`]).
///
/// Built rather than inlined so a test can read the column list, the
/// placeholder count and the conflict target back out of it — and built
/// **once**, because every input to it is a constant and the alternative is one
/// `format!` per column of [`INSERT_COLUMNS`] on every write.
static BATCH_GUARDED_SQL: LazyLock<String> =
    LazyLock::new(|| guarded_statement(&batch_input_source()));

/// `sqlx`-backed implementation of [`RecordStore`] over the `usage_records`
/// hypertable.
///
/// Every operation acquires its connection through `Self::timed_acquire`, so
/// `uc_timescaledb_pool_acquire_duration_seconds` is recorded per acquire and
/// `uc_timescaledb_tls_handshake_failures_total` is incremented when a fresh
/// physical connection fails its TLS handshake (via
/// `Self::record_backend_error`). Both are named in plain backticks rather than
/// linked: they are private, so an intra-doc link from this public item would
/// resolve only under `--document-private-items`.
#[derive(Debug, Clone)]
pub struct PgRecordStore {
    pool: PgPool,
    metrics: Arc<Metrics>,
    cancel: CancellationToken,
    type_keys: Arc<TypeKeyCache>,
    /// Whether `aggregate` may serve an eligible query from `usage_rollup_1h`.
    /// Always `true` outside tests; `Self::without_rollup` turns it off so
    /// the integration suite can compare both reads over one database. Not
    /// linked, because that constructor is `#[cfg(any(test, feature =
    /// "postgres"))]` and so does not exist in a default-feature doc build.
    rollup_enabled: bool,
    /// `feed_acceptance_slack_secs`, bound into every write statement's
    /// admission guard (see [`admitted_expr`]). Held as the `i64` it binds as.
    /// Config validation caps it far below `i64::MAX`, so [`Self::new`]'s
    /// saturation is unreachable.
    acceptance_slack_secs: i64,
}

impl PgRecordStore {
    /// Build a store over an existing connection pool. `cancel` is the gear's
    /// cancellation token; the request path stops re-arming the `ready` gauge
    /// once it fires so a drain-time acquire cannot flip readiness back on after
    /// the shutdown watcher has cleared it.
    ///
    /// `acceptance_slack_secs` is the deployment's `feed_acceptance_slack_secs`
    /// (`crate::config`), and it reaches the database as a bind on every write
    /// rather than as anything this type decides.
    #[must_use]
    pub fn new(
        pool: PgPool,
        metrics: Arc<Metrics>,
        cancel: CancellationToken,
        acceptance_slack_secs: u64,
    ) -> Self {
        Self {
            pool,
            metrics,
            cancel,
            type_keys: Arc::new(TypeKeyCache::default()),
            rollup_enabled: true,
            acceptance_slack_secs: i64::try_from(acceptance_slack_secs).unwrap_or(i64::MAX),
        }
    }

    /// The same store with the rollup read switched off, so every aggregate
    /// takes the exact scan. Test-only: the equivalence suite compares the two.
    #[cfg(any(test, feature = "postgres"))]
    #[must_use]
    pub fn without_rollup(mut self) -> Self {
        self.rollup_enabled = false;
        self
    }

    /// Map a `sqlx` error via [`map_sqlx_err`] and, as a side effect, increment
    /// the backend-error counter under the matching [`ErrorClass`]
    /// ([`ErrorClass::Transient`] for a [`UsageCollectorPluginError::Transient`]
    /// mapping, otherwise [`ErrorClass::Internal`]). Returns the mapped error so
    /// it slots into the existing `.map_err(...)` call sites unchanged.
    fn record_backend_error(&self, err: &sqlx::Error) -> UsageCollectorPluginError {
        // A TLS handshake failure is the plugin's one metered transport-security
        // signal (`uc_timescaledb_tls_handshake_failures_total`); count it
        // before the generic mapping.
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

    /// [`Self::record_backend_error`], plus the one insert-path error that is
    /// retryable without being a connectivity or serialization fault: a PRIMARY
    /// KEY collision on one of this write's own dedup identities
    /// ([`is_ledger_pk_violation`]). The case it is **sized for** is a
    /// concurrent writer of that identity; [`entry_identity`] sets out a second
    /// route to the same raise, which a mis-derived `id` reaches with no
    /// concurrency at all.
    ///
    /// **It is lifted here rather than in
    /// [`crate::infra::storage::error::classify_db`]**, which still maps a
    /// primary-key `23505` to a non-retryable `Internal` — right for a collision
    /// nothing intercepted. Only a write path knows it is in the race, so only a
    /// write path may reclassify it.
    ///
    /// **Re-running resolves the race, and it must be a fresh transaction**: the
    /// winner is committed by the time this error is raised, so the re-run's
    /// `ON CONFLICT` pre-check sees its row and the batch resolves it as an
    /// ordinary dedup hit. `create_batch`'s bounded jittered retry
    /// ([`with_retry`]) is exactly that.
    ///
    /// **It does not resolve the other route**: on a mis-derived `id` the re-run
    /// finds nothing, meets the same primary-key entry again, and
    /// [`MAX_BATCH_ATTEMPTS`] is spent before the host is told. The detail string
    /// names the race deliberately — re-wording it would buy a caller nothing it
    /// can act on differently.
    fn record_insert_error(&self, err: &sqlx::Error) -> UsageCollectorPluginError {
        if is_ledger_pk_violation(err) {
            self.metrics.inc_backend_error(ErrorClass::Transient);
            return UsageCollectorPluginError::transient(
                "a concurrent write of the same usage-entry identity committed first",
            );
        }
        self.record_backend_error(err)
    }

    /// Acquire a pooled connection, recording
    /// `uc_timescaledb_pool_acquire_duration_seconds`. Errors map
    /// through [`Self::record_backend_error`] (which also catches a TLS-handshake
    /// failure on a fresh physical connection). Every operation acquires through
    /// this path so the acquire-latency histogram is representative.
    // @cpt-algo:cpt-cf-uc-plugin-algo-backend-readiness-gauge:p2
    // @cpt-dod:cpt-cf-uc-plugin-dod-readiness-gauge-semantics:p2
    // @cpt-state:cpt-cf-uc-plugin-state-backend-readiness:p2
    async fn timed_acquire(&self) -> Result<PoolConnection<Postgres>, UsageCollectorPluginError> {
        let t = Instant::now();
        match self.pool.acquire().await {
            Ok(conn) => {
                self.metrics.record_pool_acquire(t.elapsed().as_secs_f64());
                // A successful acquire re-arms `uc_timescaledb_ready`, but only
                // while not shutting down: once `cancel` fires the shutdown
                // watcher owns the gauge. This gate narrows but does not close
                // the check-then-set race against the watcher; the residual is a
                // sub-tick blip on a best-effort gauge during one-way shutdown.
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
    /// [`PgRecordStore::read_conflict_records`] keys its map on `row.id`, read
    /// back under [`entry_identity`], and the in-batch arm passes the row the
    /// statement inserted under that same identity.
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
                record.idempotency_key.as_str(),
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
    /// statement reports no verdict at all — so `create_batch_inner` lifts it to
    /// a `Transient` ([`PgRecordStore::record_insert_error`]) and the whole batch
    /// re-runs on a fresh transaction.
    async fn run_guarded_batch_write(
        tx: &mut sqlx::Transaction<'_, Postgres>,
        reps: &[&(MeterRef, StoredUsageRecord)],
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
        for (_, rep) in reps {
            let id = entry_identity(rep);
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
    /// [`PARTITION_PRUNE_COLUMN`] gives; [`BATCH_CONFLICT_READ_SQL`] is where
    /// the predicate's two conjuncts are explained and measured.
    ///
    /// Maps each identity to `Stored` (row found → resolve absorb/conflict) or
    /// `Stale` (the row this one conflicted with could not be read — see
    /// [`CONFLICT_UNREADABLE_MESSAGE`] for what does that).
    async fn read_conflict_records(
        &self,
        tx: &mut sqlx::Transaction<'_, Postgres>,
        lost: &[&(MeterRef, StoredUsageRecord)],
    ) -> Result<HashMap<Uuid, ConflictRead>, UsageCollectorPluginError> {
        let mut out: HashMap<Uuid, ConflictRead> = HashMap::new();
        if lost.is_empty() {
            return Ok(out);
        }

        let ids: Vec<Uuid> = lost.iter().map(|(_, r)| entry_identity(r)).collect();
        let window_ends: Vec<OffsetDateTime> = lost.iter().map(|(_, r)| r.window_end).collect();
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
        records: &[(MeterRef, StoredUsageRecord)],
        plan: &BatchPlan<'_>,
        admissions: &HashMap<Uuid, Admission>,
        conflict: &HashMap<Uuid, ConflictRead>,
    ) -> Vec<Result<StoredUsageRecord, UsageCollectorPluginError>> {
        let mut results = Vec::with_capacity(records.len());
        for (i, (meter, record)) in records.iter().enumerate() {
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
    async fn create_batch_inner(
        &self,
        records: &[(MeterRef, StoredUsageRecord)],
    ) -> Result<Vec<Result<StoredUsageRecord, UsageCollectorPluginError>>, UsageCollectorPluginError>
    {
        let plan = plan_batch(records);

        let mut conn = self.timed_acquire().await?;

        // One partition key per representative, aligned to `plan.reps`, resolved
        // in autocommit before the write transaction opens. A type already seen
        // costs no round trip.
        let mut type_keys: Vec<i32> = Vec::with_capacity(plan.reps.len());
        for (meter, _) in &plan.reps {
            let key = self
                .type_keys
                .resolve(&mut conn, meter.uuid)
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
        let lost: Vec<&(MeterRef, StoredUsageRecord)> = plan
            .reps
            .iter()
            .copied()
            .filter(|(_, r)| matches!(admissions.get(&entry_identity(r)), Some(Admission::Lost)))
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

    /// Steps 2 to 4 of the feed page protocol, inside the open snapshot
    /// transaction `conn` already holds (`RecordStore::feed_page`'s step 1).
    ///
    /// Returns early with [`InTransactionPage::marked`] when step 3 finds a
    /// mark, in which case the page statement is never sent: `docs/DESIGN.md`
    /// §3.6's diagram wraps the page statement in `opt not marked`, so an
    /// early mark refuses without building a page.
    ///
    /// Over clippy's argument limit: each parameter is a distinct input the
    /// six-step protocol names, and this is a private helper with one call site,
    /// so bundling them into a struct would be indirection for its own sake.
    #[allow(clippy::too_many_arguments)]
    // @cpt-algo:cpt-cf-uc-plugin-algo-next-position-selection:p1
    // @cpt-dod:cpt-cf-uc-plugin-dod-snapshot-and-bounded-replay:p1
    // @cpt-dod:cpt-cf-uc-plugin-dod-live-head-position:p1
    async fn feed_page_in_transaction(
        &self,
        conn: &mut PgConnection,
        sql: &str,
        binds: &[SqlBind],
        types: &[Uuid],
        after: Option<(u64, Uuid)>,
        until: Option<(u64, Uuid)>,
        limit: u64,
    ) -> Result<InTransactionPage, UsageCollectorPluginError> {
        // Step 2. Fixes the snapshot and yields the settled horizon. Read as
        // text because `xid8` has no `sqlx` Decode, which is the same reason
        // `FeedRecordRow` holds `xact_id` as a String.
        let horizon: String =
            sqlx::query_scalar("SELECT pg_snapshot_xmin(pg_current_snapshot())::text")
                .fetch_one(&mut *conn)
                .await
                .map_err(|e| self.record_backend_error(&e))?;
        let horizon_num: u64 = horizon.parse().map_err(|_| {
            UsageCollectorPluginError::internal(format!(
                "the settled horizon `{horizon}` read off pg_snapshot_xmin did not parse as a u64"
            ))
        })?;

        // Step 3. A fast path only: this transaction's own snapshot can miss
        // a drop that commits after it, which is why step 6 re-checks in
        // autocommit after COMMIT.
        if let Some(position) = after
            && self.mark_stands_above(&mut *conn, types, position).await?
        {
            return Ok(InTransactionPage::marked());
        }

        // Step 4. `persistent(false)` so this call never *inserts* a plan into
        // the connection's statement cache, and nothing else in this crate
        // prepares this SQL text persistently, so there is no stale entry to
        // find: the statement is planned fresh after step 2. TimescaleDB
        // excludes chunks at plan time against the catalog it then sees, so a
        // plan cached before a chunk existed could silently skip that chunk's
        // rows — the failure §3.6's "Why" paragraph rules out.
        let mut q = sqlx::query_as::<_, FeedRecordRow>(AssertSqlSafe(sql)).persistent(false);
        q = q.bind(types).bind(&horizon);
        if let Some((xact_id, id)) = after {
            q = q.bind(xact_id.to_string()).bind(id);
        }
        if let Some((xact_id, id)) = until {
            q = q.bind(xact_id.to_string()).bind(id);
        }
        for b in binds {
            q = bind_one(q, b);
        }
        let rows = q
            .fetch_all(&mut *conn)
            .await
            .map_err(|e| self.record_backend_error(&e))?;

        let limit_as_usize = usize::try_from(limit).unwrap_or(usize::MAX);
        let row_count = rows.len();
        // Captured before `row.record` is moved into `record_row_to_model`
        // below: `xact_id` lives on `FeedRecordRow` alone, not on the
        // `UsageRecordRow` that function consumes nor on the SDK model.
        let last_position = rows
            .last()
            .map(|row| -> Result<(u64, Uuid), UsageCollectorPluginError> {
                let xact_id: u64 = row.xact_id.parse().map_err(|_| {
                    UsageCollectorPluginError::internal(format!(
                        "a fed row's xact_id `{}` did not parse as a u64",
                        row.xact_id
                    ))
                })?;
                Ok((xact_id, row.record.id))
            })
            .transpose()?;

        let mut entries = Vec::with_capacity(row_count);
        for row in rows {
            entries.push(record_row_to_model(row.record)?);
        }

        // The disposition, in DESIGN §3.6's own order: a bounded replay that
        // reached its `until` closes; a page filled to its limit continues
        // from its last entry's own position; a short page has reached the
        // horizon and continues from the head position.
        //
        // "Reached `until`" is not the same test as "short": a page can fill to
        // exactly `limit` rows with its last row's position equal to `until`.
        // Testing `row_count < limit_as_usize` alone would fall through to the
        // filled-to-limit arm and mint a continuation for a closed replay.
        let next = if until.is_some() && (row_count < limit_as_usize || last_position == until) {
            None
        } else if row_count >= limit_as_usize {
            Some(last_position.ok_or_else(|| {
                UsageCollectorPluginError::internal(
                    "a feed page filled to its limit carried no rows, which the caller's \
                     limit >= 1 obligation should make unreachable",
                )
            })?)
        } else {
            Some((horizon_num.saturating_sub(1), MAX_UUID))
        };

        Ok(InTransactionPage {
            entries,
            next,
            marked: false,
        })
    }

    /// Whether a mark of any subscribed type stands above `position`.
    ///
    /// One small statement ([`MARK_ABOVE_SQL`]), run twice per page at most:
    /// once under the page's own snapshot as a fast path
    /// ([`Self::feed_page_in_transaction`], step 3) and once in autocommit as
    /// the authority (`RecordStore::feed_page`, step 6), both against the same
    /// pooled connection before and after its `COMMIT`.
    // @cpt-flow:cpt-cf-uc-plugin-flow-resume-after-retention:p2
    // @cpt-algo:cpt-cf-uc-plugin-algo-retention-mark-check:p1
    // @cpt-dod:cpt-cf-uc-plugin-dod-retention-refusal:p1
    async fn mark_stands_above(
        &self,
        conn: &mut PgConnection,
        types: &[Uuid],
        position: (u64, Uuid),
    ) -> Result<bool, UsageCollectorPluginError> {
        let found: Option<i32> = sqlx::query_scalar(MARK_ABOVE_SQL)
            .bind(types)
            .bind(position.0.to_string())
            .bind(position.1)
            .fetch_optional(&mut *conn)
            .await
            .map_err(|e| self.record_backend_error(&e))?;
        Ok(found.is_some())
    }
}

/// One page read's outcome inside the open snapshot transaction: either a
/// mark already stood above the position (the fast path, step 3), or the page
/// statement ran and this carries its rows and disposition.
struct InTransactionPage {
    /// The page's entries, already mapped to the SDK model. Empty when
    /// `marked` is `true`: the page statement is never sent on that path
    /// (`docs/DESIGN.md` §3.6's diagram wraps it in `opt not marked`).
    entries: Vec<StoredUsageRecord>,
    /// The continuation, as a position pair. `None` only when `marked` is
    /// `true` (mirroring `entries`) or when a bounded replay reached its
    /// `until`.
    next: Option<(u64, Uuid)>,
    /// Whether step 3's fast-path check already found a mark above the
    /// position. `RecordStore::feed_page` short-circuits step 6 on this: a mark
    /// the fast path found is certain, so no re-check confirms it.
    marked: bool,
}

impl InTransactionPage {
    /// Step 3 found a mark above the position. No page statement is sent.
    fn marked() -> Self {
        Self {
            entries: Vec::new(),
            next: None,
            marked: true,
        }
    }
}

/// One per-column vector per column of [`INSERT_COLUMNS`], which a multi-row
/// insert binds.
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
    fn build(reps: &[&(MeterRef, StoredUsageRecord)], type_keys: &[i32]) -> Self {
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
        for (meter, r) in reps {
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

/// Extract a single order-field value from a row as its cursor-key string.
///
/// Inverse of [`cursor_key_to_bind`](crate::infra::storage::query::keyset::cursor_key_to_bind):
/// the `uuid` columns render via [`Uuid`]'s [`Display`](std::fmt::Display) — the
/// canonical hyphenated lower-case form — the `timestamptz` bounds
/// as RFC 3339, and the text columns as-is — each the spelling that helper
/// parses back for the field's declared kind, so a minted boundary re-binds to
/// the value it was read from.
///
/// **The arms are [`usage_collector_sdk::KEYSET_SAFE_RECORD_FIELDS`], and that
/// is the whole rule.** A field off that list is one the SDK does not carry as
/// an attribute of every entry in its own right: either it can be absent (a
/// `NULL` compares as `NULL` inside the row-value tuple and silently drops the
/// row), or it is derived from one that can. `None` here means "not a keyset key
/// on the row" — a refusal to mint, not a missing value.
///
/// One half of a two-sided map: [`record_column`] resolves an order field to the
/// column the `ORDER BY` and the keyset tuple render from, and this resolves the
/// same field to the value the boundary carries. The two-sided coupling test in
/// `record_store_tests.rs` is what couples them.
fn record_row_key(row: &UsageRecordRow, field: &str) -> Option<String> {
    match field {
        "id" => Some(row.id.to_string()),
        "window_start" => row.window_start.format(&Rfc3339).ok(),
        "window_end" => row.window_end.format(&Rfc3339).ok(),
        "tenant_id" => Some(row.tenant_id.to_string()),
        "resource_id" => Some(row.resource_id.clone()),
        "resource_type" => Some(row.resource_type.clone()),
        "origin" => Some(row.origin.clone()),
        "accepted_at" => row.accepted_at.format(&Rfc3339).ok(),
        _ => None,
    }
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
/// statement assembly.
///
/// Binds start at `$2` because the `id` occupies `$1`, which is why
/// [`PgRecordStore::get`] binds the `id` first. This is the only seeded caller
/// on a production path; both collection paths seed at 1, their leading meter
/// value going through the same [`SqlCtx`] as everything else. Only the ordered
/// bind values are returned, not the [`SqlCtx`]: the statement is finished.
///
/// # Errors
///
/// Propagates [`translate_scope`]'s refusal unchanged. A scope that fails to
/// translate and is dropped instead leaves `WHERE id = $1` — a translation
/// failure turned into an authorization bypass — so this returns `Err` rather
/// than a partial statement, and it is the only producer of this path's SQL.
// @cpt-algo:cpt-cf-uc-plugin-algo-scoped-point-lookup:p1
// @cpt-dod:cpt-cf-uc-plugin-dod-scoped-point-lookup:p1
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
/// literal column names the shared builders write — `r.gts_type_uuid` and
/// `r.window_end` from [`push_meter_and_range_clauses`], `r.metadata ->>` from
/// [`push_metadata_filter_clauses`]. None is caller input, and every
/// caller-derived value is bound, save the clamped page size, which is a `u64`
/// this function renders itself into the `LIMIT`.
///
/// **Selection reads the covered-period end alone** — `from <= window_end < to`,
/// per `usage_collector_sdk::TimeRange::contains_window_end`, which is where
/// that rule lives. The predicate never names `window_start`. `time_range` is a
/// typed parameter and deliberately not reachable through `query.filter`.
///
/// **No withdrawal exclusion, and none belongs here.** `aggregate` excludes a
/// withdrawn pair because a total that counts one is a wrong total; that is a
/// derived view and this is the ledger. The SPI says it in as many words — "a
/// withdrawn pair MUST likewise be returned as persisted here" — and
/// [`PgRecordStore::get`] carries no exclusion either.
///
/// `query.order` is rendered as handed, and the keyset tuple is built from the
/// same `query.order` in the same field order a few lines above, so the two
/// cannot name different columns or disagree about direction — which is what
/// makes a continuation resume from the boundary the previous page ended on.
///
/// # Errors
///
/// Returns an error string when the composed `$filter` cannot be translated
/// (propagated from [`translate_scope`] unchanged, and never dropped — a
/// dropped scope leaves the read unscoped), or when an order or keyset field
/// is off the allowlist or not keyset-safe, or when the order is empty or
/// mixed-direction. A caller must propagate it: this is the only producer of
/// the statement, so a partial one is never returned in its place.
// @cpt-algo:cpt-cf-uc-plugin-algo-keyset-seek-page:p1
// @cpt-dod:cpt-cf-uc-plugin-dod-keyset-seek-pagination:p1
// @cpt-dod:cpt-cf-uc-plugin-dod-no-filter-widening-no-offset:p1
// @cpt-dod:cpt-cf-uc-plugin-dod-entries-as-persisted:p1
fn build_list_sql(
    gts_type_uuid: Uuid,
    time_range: TimeRange,
    query: &ODataQuery,
    metadata_filter: &[MetadataFilter],
    keyset: Option<&Keyset>,
    limit: u64,
) -> Result<(String, Vec<SqlBind>), String> {
    let mut ctx = SqlCtx::new(1);
    let mut clauses: Vec<String> = Vec::new();

    // The meter scope and the covered-period range, from the one spelling both
    // read paths share. Read rather than transcribed, so `aggregate` cannot
    // drift from this on the ADR obligation both are held to.
    push_meter_and_range_clauses(gts_type_uuid, time_range, &mut ctx, &mut clauses);

    // The composed `$filter`: the caller's filter `And`-composed with the
    // compiled PDP scope, or the scope alone when the caller supplied none —
    // either way one expression, which for a multi-constraint grant is an `Or`.
    // `translate_scope` returns a parenthesized fragment, which is what makes
    // pushing it into a `join(" AND ")` safe.
    if let Some(expr) = query.filter() {
        clauses.push(translate_scope(expr, &mut ctx)?);
    }

    // Metadata side-channel: AND across filters, OR within one filter's
    // values (see [`push_metadata_filter_clauses`]).
    push_metadata_filter_clauses(metadata_filter, &mut ctx, &mut clauses);

    if let Some(keyset) = keyset {
        let order_pairs: Vec<(&str, bool)> = query
            .order
            .0
            .iter()
            .map(|key| (key.field.as_str(), matches!(key.dir, SortDir::Asc)))
            .collect();
        clauses.push(keyset_predicate(
            &order_pairs,
            keyset.values(),
            record_column,
            record_field_kind,
            is_keyset_safe_record_field,
            &mut ctx,
        )?);
    }

    let order_sql = render_order_by(&query.order, record_column)?;

    Ok((
        format!(
            "SELECT {RECORD_COLUMNS} FROM {} WHERE {} ORDER BY {order_sql} LIMIT {}",
            // Called, never spelled: with a literal here the shared constant
            // would be decorative.
            ledger_from_clause(),
            clauses.join(" AND "),
            // The look-ahead: one row past the page, so the page can tell "last
            // page" from "there is another" without a second query. This `+ 1`
            // is what makes `build_list_page`'s `rows.len() > page_size` correct
            // rather than `>=`.
            limit.saturating_add(1),
        ),
        ctx.binds,
    ))
}

/// Turn the look-ahead read into the page the caller gets: drop the
/// look-ahead row and return the last in-page row's keyset for the gateway
/// to mint a continuation from.
///
/// No wire cursor and no `query.filter_hash` is produced here: minting,
/// encoding and fingerprinting belong to the gateway, and this hands back a
/// [`Keyset`] of the last in-page row's sort values for it to mint from.
///
/// The boundary values are read in `query.order` field order, one per key, not
/// in a canonical order this function assumed.
///
/// # Precondition
///
/// `limit >= 1`, and `rows` is the look-ahead read [`build_list_sql`] asked for
/// — at most `limit + 1` rows. The `+ 1` there is what makes the
/// `rows.len() > page_size` here correct rather than `>=`.
/// [`effective_page_size`] floors the limit to 1; this signature does not, so a
/// caller passing `0` truncates the whole page away and reaches the `Err` below.
///
/// The direction is resolved through [`uniform_dir`] rather than read off the
/// leading key — see `keyset.rs` for why every entry point that acts on a
/// direction resolves it that way. This is a pure, directly unit-tested function
/// holding its own contract, so it does not rely on `render_order_by` happening
/// to run first in [`build_list_sql`].
///
/// # Errors
///
/// Returns [`UsageCollectorPluginError::Internal`] when an order field is not a
/// keyset key on the row, when `query.order` mixes sort directions or is empty
/// (both from [`uniform_dir`]), when the keyset is too large
/// ([`usage_collector_sdk::KeysetInvalid::TooLarge`];
/// [`usage_collector_sdk::KeysetInvalid::Empty`] cannot surface, `uniform_dir`'s
/// own empty check running first over the same order), or when a stored row
/// cannot be mapped to the SDK model. Distinctly from all four, `"non-empty page
/// lost its tail"` means the precondition above was broken — a bug in this crate
/// rather than anything a caller of the SPI can provoke.
// @cpt-algo:cpt-cf-uc-plugin-algo-keyset-seek-page:p1
// @cpt-dod:cpt-cf-uc-plugin-dod-next-page-detection:p1
fn build_list_page(
    mut rows: Vec<UsageRecordRow>,
    query: &ODataQuery,
    limit: u64,
) -> Result<RecordPage, UsageCollectorPluginError> {
    let page_size = usize::try_from(limit).unwrap_or(usize::MAX);

    // Look-ahead row present -> a next page exists; drop it before mapping.
    let has_next = rows.len() > page_size;
    if has_next {
        rows.truncate(page_size);
    }

    let next = if has_next {
        let last = rows
            .last()
            .ok_or_else(|| UsageCollectorPluginError::internal("non-empty page lost its tail"))?;
        let values = query
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
        // Through `uniform_dir`, not the leading key, so the keyset's direction
        // cannot describe an order the page was not read in.
        let direction = uniform_dir(query.order.0.iter().map(|key| key.dir))
            .map_err(UsageCollectorPluginError::internal)?;
        Some(
            Keyset::new(values, direction)
                .map_err(|err| UsageCollectorPluginError::internal(err.to_string()))?,
        )
    } else {
        None
    };

    let items = rows
        .into_iter()
        .map(record_row_to_model)
        .collect::<Result<Vec<_>, _>>()?;

    Ok(RecordPage { items, next })
}

/// The key both write paths decide identity on: the entry's own `id`.
///
/// The `id` arrives on the wire, derived above the SPI by
/// `usage_collector_sdk::id::derive_usage_record_id` — the canonical home of the
/// dedup identity. **This plugin neither re-derives nor validates it**, so
/// everything below rests on the gateway having derived it faithfully. That is a
/// trust boundary worth naming.
///
/// What rests on it:
///
/// * **The in-batch comparison.** [`plan_batch`] collapses a batch to one
///   representative per `id` and [`resolve_batch`](PgRecordStore::resolve_batch)
///   resolves every later row against it. Keying on the `id` rather than the
///   6-tuple makes "two entries of one batch are the same entry" and "two
///   entries of one batch take one ledger row" the *same* statement: the guarded
///   statement joins inserted rows back to input on `id`
///   (`LEFT JOIN ins USING (id)`), which has no coherent reading for two rows
///   that agree on `id` and differ in their six inputs.
/// * **The conflict read-back.** Both paths read the row that took a lost slot
///   by `id`, per `DESIGN.md` §3.6: *"The read-back keys on `id`, never on
///   `(tenant_id, gts_type_id, idempotency_key, window_start, window_end)`"* —
///   the five alone cannot tell a withdrawn record from its withdrawal.
///
/// What does **not** rest on it is the ledger's own uniqueness: the
/// `ON CONFLICT` arbiter is [`DEDUP_CONFLICT_TARGET`], so the database decides
/// what collides from stored values rather than from a caller's assertion.
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
///     re-runs, which **does not clear it** — [`MAX_BATCH_ATTEMPTS`] is spent
///     before the error reaches the host. Here the ledger refuses the row.
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
const fn entry_identity(record: &StoredUsageRecord) -> Uuid {
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
/// transaction opened — but [`super::pool`] forces the level per connection, so
/// no server setting reaches this arm.
///
/// Nothing here can tell them apart; `inc_dedup_stale`'s own doc says why none
/// of them gets a label or a counter of its own.
const CONFLICT_UNREADABLE_MESSAGE: &str =
    "conflicting record could not be read back during dedup resolution; retry";

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
    reps: Vec<&'a (MeterRef, StoredUsageRecord)>,
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
fn plan_batch(records: &[(MeterRef, StoredUsageRecord)]) -> BatchPlan<'_> {
    let mut first_index: HashMap<Uuid, usize> = HashMap::new();
    let mut reps: Vec<&(MeterRef, StoredUsageRecord)> = Vec::new();
    for (i, pair) in records.iter().enumerate() {
        if let std::collections::hash_map::Entry::Vacant(slot) =
            first_index.entry(entry_identity(&pair.1))
        {
            slot.insert(i);
            reps.push(pair);
        }
    }
    reps.sort_by_key(|(_, r)| entry_identity(r));
    BatchPlan { reps, first_index }
}

/// Total `create_batch` attempts: one initial try plus two retries. A bounded
/// in-process retry so a transient backend error self-heals transparently
/// instead of bubbling an `Err(Transient)` to the host. `is_retryable_batch_error`
/// is where the causes that reach this loop are set against the code.
const MAX_BATCH_ATTEMPTS: u32 = 3;

/// Deterministic pre-jitter backoff base for the `attempt`-th retry (1-based).
/// A short exponential — 5 ms, 10 ms, … — because a deadlock victim can retry
/// almost immediately: the surviving transaction has already committed or
/// aborted by the time Postgres aborts the victim, so the contended dedup locks
/// are free. The shift is saturated so the schedule cannot overflow however
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
/// The deadlock victim surfaces as an outer `Transient`: `create_batch_inner`
/// runs the whole batch in one transaction, and that transaction rolled back, so
/// the attempt left nothing behind. Serialization failures (`40001`), lock
/// timeouts (`55P03`, including on the partition-key assignment
/// `create_batch_inner` runs per representative in autocommit) and connectivity
/// faults collapse to the same bucket inside the storage helpers. A concurrent
/// writer colliding on the ledger's PRIMARY KEY reaches it by a different route:
/// the arbiter does not cover that index, so `classify_db` leaves the `23505` in
/// `Other` and `record_insert_error` lifts it. Every one of these is **safe** to
/// re-run, this batch being idempotent — a weaker claim than that a re-run
/// succeeds, and the only one this predicate needs. `Internal`,
/// `IdempotencyConflict` and the other typed domain outcomes are non-retryable
/// and returned unchanged. Per-row `Transient` outcomes carried inside an
/// `Ok(vec)` are deliberately not seen here — the batch as a whole succeeded.
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
/// `attempt` number. It is the observability seam (log + retry counter), kept
/// out of this combinator so it stays generic and DB-free. It never fires on a
/// first-attempt success or on a returned (non-retried) error, so a retry can be
/// told apart from a bubbled transient failure.
///
/// `operation` is an `Fn` invoked fresh each attempt, which is the right unit of
/// retry for `create_batch_inner`: every attempt acquires a fresh connection and
/// opens a fresh transaction, so a failed attempt leaves no half-written batch
/// behind — and, the row's `xact_id` coming from the transaction that wrote it,
/// no attempt leaves a feed position a later one contradicts. On success the
/// loop neither sleeps, allocates a backoff, nor calls `on_retry`.
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

/// Outcome of reading the existing `usage_records` row for a lost identity.
enum ConflictRead {
    /// The conflicting row exists — resolve absorb vs conflict against it.
    Stored(Box<UsageRecordRow>),
    /// It could not be read. More than one thing does that, they answer alike,
    /// and not all of them clear on a retry — [`CONFLICT_UNREADABLE_MESSAGE`]
    /// sets them out. The name is the oldest of them.
    Stale,
}

/// One assembled aggregate statement: the SQL, the binds in placeholder order,
/// and **the dimension count the SELECT list was actually built from**.
///
/// `dim_count` is carried rather than recomputed by the caller: it is the count
/// [`build_aggregate_sql`] numbered the `GROUP BY` ordinals with and placed the
/// fold after, and it is what the decoder must read the same number of key
/// columns with. Re-derived at the call site the two could drift with nothing to
/// notice — no unit test executes a statement, so a decoder reading the wrong
/// number of columns is invisible until a live query.
///
/// `dim_count` is the one output of [`build_aggregate_sql`] that leaves no trace
/// in the SQL string, so no text oracle reaches it;
/// `the_builder_reports_the_dimension_count_its_select_list_was_built_from` is
/// what holds it. Contrast `dimension_presence_guard`, which is correct by
/// construction: the guard string literally contains the select expression.
struct AggregateStatement {
    sql: String,
    binds: Vec<SqlBind>,
    dim_count: usize,
}

/// Build the pushed-down aggregate statement and its binds, in placeholder
/// order.
///
/// ```sql
/// SELECT <dimension exprs…>, <fold expr>
/// FROM usage_records r
/// WHERE r.gts_type_uuid = $1 AND r.type_key = (…) AND r.window_end >= $2 AND r.window_end < $3
///   AND <withdrawal exclusion>
///   [AND <translated $filter>] [AND <metadata filters>] [AND <presence guards>]
/// [GROUP BY 1, 2, …] [LIMIT MAX_AGGREGATION_BUCKETS + 1]
/// ```
///
/// Every line but the last two comes from a shared builder rather than from a
/// second transcription here: [`ledger_from_clause`] is the `FROM`,
/// [`push_meter_and_range_clauses`] the meter and the covered-period range,
/// [`withdrawal_exclusion_clause`] the two obligations a withdrawn pair places
/// on every fold, and [`push_metadata_filter_clauses`] the side channel. Read
/// from one place, the range predicate cannot drift onto `window_start` in this
/// path alone.
///
/// The `$filter` goes through [`translate_scope`], which parenthesizes what it
/// returns. What arrives is the caller's filter `And`-composed with the compiled
/// PDP scope, **or the scope alone when the caller supplied none** — and a
/// multi-constraint grant compiles to a disjunction, so a bare fragment pushed
/// into a `join(" AND ")` would read as `(P AND A) OR B` and answer rows outside
/// the grant.
///
/// **No slot of `query` beyond `filter` is read.** This path paginates nothing
/// and mints no cursor, so `query.cursor`, `query.filter_hash` and `query.limit`
/// reach neither the statement nor the binds.
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
// @cpt-algo:cpt-cf-uc-plugin-algo-exact-scan-fold:p1
// @cpt-dod:cpt-cf-uc-plugin-dod-pushed-down-fold:p1
fn build_aggregate_sql(
    gts_type_uuid: Uuid,
    time_range: TimeRange,
    fold: AggregationFold,
    query: &ODataQuery,
    metadata_filter: &[MetadataFilter],
    group_by: &[AggregationDimension],
) -> Result<AggregateStatement, String> {
    let mut ctx = SqlCtx::new(1);
    let mut clauses: Vec<String> = Vec::new();

    push_meter_and_range_clauses(gts_type_uuid, time_range, &mut ctx, &mut clauses);

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

    // With no dimensions the SELECT is the fold alone, which is what makes the
    // no-grouping case one aggregate row rather than none.
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

    Ok(AggregateStatement {
        sql: format!(
            "SELECT {select_list} FROM {} WHERE {}{group_by_sql}{}",
            // Called, never spelled: with a literal here the shared constant
            // would be decorative.
            ledger_from_clause(),
            clauses.join(" AND "),
            aggregate_limit_clause(dim_count),
        ),
        binds: ctx.binds,
        // The same count the SELECT list above was built from, handed to the
        // decoder rather than derived again there.
        dim_count,
    })
}

/// Read one aggregate result row into a bucket: `dim_count` dimension columns
/// as the key, then the folded value.
///
/// Split out so the caller is a 1:1 `map` over the fetched rows — the shape the
/// no-grouping case needs: with no `GROUP BY` the statement is a bare aggregate
/// and `PostgreSQL` answers exactly one row, which becomes one bucket with an
/// empty key. That, not an empty bucket list, is what a conforming plugin owes.
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
// @cpt-algo:cpt-cf-uc-plugin-algo-bucket-key-rendering:p1
// @cpt-dod:cpt-cf-uc-plugin-dod-bucket-key-rendering:p1
// @cpt-dod:cpt-cf-uc-plugin-dod-empty-selection-values:p1
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

/// One assembled S1∧S2 statement (spec §9.3): `accepted_count` and the
/// fold-appropriate summary, both read off one ranged scan of the scope.
struct ReconciliationSummaryStatement {
    sql: String,
    binds: Vec<SqlBind>,
}

/// Build spec §9.3's S1 (`accepted_count`) and S2 (the summary), composed
/// as one statement over one ranged scan.
///
/// ```sql
/// SELECT COUNT(*), <accrued_summary_select_expr()>
/// FROM usage_records r
/// WHERE r.gts_type_uuid = $1 AND r.type_key = (…) AND r.window_end >= $2
///   AND r.window_end < $3 AND r.tenant_id = $4 AND <translated scope>
/// ```
/// or, for every fold but `Sum`:
/// ```sql
/// SELECT COUNT(*), <observation_count_select_expr()>, <observation_latest_select_expr()>
/// FROM usage_records r
/// WHERE … (the same five predicates)
/// ```
///
/// **`accepted_count` carries no predicate of its own** — it is the bare
/// `COUNT(*)` the DESIGN gives it, over every row the `WHERE` selects,
/// invalidations and withdrawn pairs included. The summary excludes a
/// withdrawn pair through the `FILTER` clause each of
/// [`accrued_summary_select_expr`]/[`observation_count_select_expr`]/
/// [`observation_latest_select_expr`] already attaches, rather than a second
/// `WHERE`-scoped scan: the two figures differ by exactly that one predicate,
/// so `FILTER` lets `PostgreSQL` answer both from the rows it reads once.
///
/// `FROM` is [`ledger_from_clause`], shared with every other read path; the
/// meter and the range are [`push_meter_and_range_clauses`], shared with
/// `list` and `aggregate`; the tenant and the compiled scope are this
/// statement's own, bound after the range in that order.
///
/// **This statement carries no `GROUP BY` and `PostgreSQL` still answers exactly
/// one row for it**, the same "no rows in, one row out" behaviour
/// [`build_aggregate_sql`] relies on: a selection matching nothing yields
/// `accepted_count = 0` and a zero (or `NULL` `latest`) summary, with no branch
/// here for "the range selected nothing" versus "everything the range selected
/// was withdrawn".
///
/// # Errors
///
/// Returns the translation error string when `scope` names a field outside
/// the allowlist, uses an unsupported operator, or carries a value that
/// cannot be bound.
fn build_reconciliation_summary_sql(
    gts_type_uuid: Uuid,
    time_range: TimeRange,
    tenant_id: Uuid,
    fold: AggregationFold,
    scope: &ast::Expr,
) -> Result<ReconciliationSummaryStatement, String> {
    let mut ctx = SqlCtx::new(1);
    let mut clauses: Vec<String> = Vec::new();

    push_meter_and_range_clauses(gts_type_uuid, time_range, &mut ctx, &mut clauses);
    let tenant_n = ctx.push(SqlBind::Uuid(tenant_id));
    clauses.push(format!("r.tenant_id = ${tenant_n}"));
    clauses.push(translate_scope(scope, &mut ctx)?);

    let select_list = if QuantitySummary::accrues(fold) {
        format!("COUNT(*), {}", accrued_summary_select_expr())
    } else {
        format!(
            "COUNT(*), {}, {}",
            observation_count_select_expr(),
            observation_latest_select_expr()
        )
    };

    Ok(ReconciliationSummaryStatement {
        sql: format!(
            "SELECT {select_list} FROM {} WHERE {}",
            ledger_from_clause(),
            clauses.join(" AND "),
        ),
        binds: ctx.binds,
    })
}

/// One assembled S3 statement (spec §9.3): the two watermarks, unbounded by
/// any range.
struct ReconciliationWatermarksStatement {
    sql: String,
    binds: Vec<SqlBind>,
}

/// Build spec §9.3's S3 — `max(accepted_at)` and `max(window_end)` over the
/// scope alone, with **no range predicate** — as two scalar subqueries
/// sharing one `WHERE`, rather than folding either into
/// [`build_reconciliation_summary_sql`]'s ranged scan.
///
/// ```sql
/// SELECT
///   (SELECT r.accepted_at FROM usage_records r WHERE <scope> ORDER BY r.accepted_at DESC LIMIT 1),
///   (SELECT r.window_end  FROM usage_records r WHERE <scope> ORDER BY r.window_end  DESC LIMIT 1)
/// ```
///
/// **Two scalar subqueries, not one aggregate with two `MAX`s, and not folded
/// into S1∧S2's `WHERE`.** Both watermarks are unbounded by the request's range
/// (DESIGN §3.6: "… unbounded, regardless of range"), so adding them to
/// [`build_reconciliation_summary_sql`]'s ranged `WHERE` would force an unranged
/// `FROM` there too — a full scan of the scope's history. Each subquery is an
/// ordinary `ORDER BY … LIMIT 1`, which makes the index reachable however the
/// planner would rewrite a bare `MAX(…)`: `usage_records_watermark_idx
/// (gts_type_uuid, tenant_id, accepted_at DESC)` serves `max_accepted_at` and
/// `usage_records_tenant_type_window_idx (tenant_id, gts_type_uuid, window_end
/// DESC)` serves `max_window_end`, both leading on exactly the columns
/// `push_meter_clause` and the tenant predicate below fix.
///
/// The `WHERE` — the meter identity alone (no range: [`push_meter_clause`], not
/// [`push_meter_and_range_clauses`]), the tenant, and the compiled scope — is
/// built once and spliced into both subqueries verbatim, so every placeholder it
/// binds is written twice in the rendered SQL but bound once.
///
/// A scope, tenant or type this backend holds no entry under leaves both
/// subqueries at zero rows, so both columns decode as `NULL` —
/// `max_accepted_at` and `max_window_end` both `None`, which is
/// [`ReconciliationMetadata::empty_for`]'s watermark half.
///
/// # Errors
///
/// Returns the translation error string when `scope` names a field outside
/// the allowlist, uses an unsupported operator, or carries a value that
/// cannot be bound.
fn build_reconciliation_watermarks_sql(
    gts_type_uuid: Uuid,
    tenant_id: Uuid,
    scope: &ast::Expr,
) -> Result<ReconciliationWatermarksStatement, String> {
    let mut ctx = SqlCtx::new(1);
    let mut clauses: Vec<String> = Vec::new();

    push_meter_clause(gts_type_uuid, &mut ctx, &mut clauses);
    let tenant_n = ctx.push(SqlBind::Uuid(tenant_id));
    clauses.push(format!("r.tenant_id = ${tenant_n}"));
    clauses.push(translate_scope(scope, &mut ctx)?);
    let where_sql = clauses.join(" AND ");
    let from = ledger_from_clause();

    Ok(ReconciliationWatermarksStatement {
        sql: format!(
            "SELECT \
             (SELECT r.accepted_at FROM {from} WHERE {where_sql} \
              ORDER BY r.accepted_at DESC LIMIT 1), \
             (SELECT r.window_end FROM {from} WHERE {where_sql} \
              ORDER BY r.window_end DESC LIMIT 1)"
        ),
        binds: ctx.binds,
    })
}

#[async_trait]
impl RecordStore for PgRecordStore {
    // @cpt-flow:cpt-cf-uc-plugin-seq-ingest-batch:p2
    // @cpt-flow:cpt-cf-uc-plugin-flow-persist-entry-batch:p1
    // @cpt-flow:cpt-cf-uc-plugin-flow-persist-withdrawal:p1
    // @cpt-state:cpt-cf-uc-plugin-state-entry-withdrawal:p2
    // @cpt-dod:cpt-cf-uc-plugin-dod-record-and-withdrawal-coexist:p1
    // @cpt-dod:cpt-cf-uc-plugin-dod-withdrawal-as-appended-entry:p1
    // @cpt-dod:cpt-cf-uc-plugin-dod-no-mutation-of-target:p1
    // @cpt-dod:cpt-cf-uc-plugin-dod-linearizable-dedup-level:p1
    async fn create_batch(
        &self,
        records: Vec<(MeterRef, StoredUsageRecord)>,
    ) -> Result<Vec<Result<StoredUsageRecord, UsageCollectorPluginError>>, UsageCollectorPluginError>
    {
        if records.is_empty() {
            tracing::warn!(
                "create_usage_records called with an empty batch (host-contract breach)"
            );
            return Err(UsageCollectorPluginError::internal(
                "create_usage_records called with an empty batch (host-contract breach)",
            ));
        }

        // Bounded retry around the whole call: on an outer `Transient`, re-run
        // up to `MAX_BATCH_ATTEMPTS` times. `is_retryable_batch_error` lists the
        // causes that reach here. Each attempt acquires a fresh connection and
        // opens a fresh transaction, so a rolled-back attempt leaves no state
        // behind; the transaction is atomic and the identities make it
        // idempotent, so a re-run cannot double-write. `Ok(vec)` is never
        // retried — per-row `Transient` outcomes inside it are the host's to
        // handle, and retrying them would be a correctness bug.
        let n = records.len();
        // Timed once including any retries, regardless of outcome.
        let t = Instant::now();
        let result = with_retry(
            MAX_BATCH_ATTEMPTS,
            batch_retry_backoff,
            is_retryable_batch_error,
            |attempt, err| {
                // A distinct warn + counter, so a self-healed transient can be
                // told apart from a returned one (which never moves this
                // counter).
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
        self.metrics.record_insert(t.elapsed().as_secs_f64());
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
    // @cpt-algo:cpt-cf-uc-plugin-algo-scoped-point-lookup:p1
    // @cpt-dod:cpt-cf-uc-plugin-dod-scoped-point-lookup:p1
    // @cpt-algo:cpt-cf-uc-plugin-algo-consistency-profile-publication:p1
    // @cpt-dod:cpt-cf-uc-plugin-dod-consistency-profile-publication:p1
    async fn get(
        &self,
        id: Uuid,
        scope: &ast::Expr,
    ) -> Result<StoredUsageRecord, UsageCollectorPluginError> {
        // Lookup by the public `id`. This relies on a one-record-per-`id`
        // contract, which the hypertable schema cannot enforce on its own — a
        // `UNIQUE` there must include every partition column, so only the
        // composite PK `(id, window_end, type_key)` is enforced.
        // `fetch_optional` therefore returns the first matching row.
        //
        // `id` is the derived dedup identity (`entry_identity`), so each stored
        // row carries a distinct `id` and `WHERE id = $1` matches at most one
        // row. A withdrawn record and the withdrawal naming it are two `id`s,
        // differing in the entry-kind input. The derivation keys on the
        // *identifier* while `usage_records_dedup_uniq` spans `gts_type_uuid`;
        // the two partition rows into identical identity classes under the
        // managed identifier profile types-registry ADR-0001 admits, and into
        // coarser ones where an external source serves two identifiers embedding
        // one explicit UUID tail — never finer.
        //
        // **No `invalidates` predicate belongs in this query, and none ever
        // will.** The asymmetry with the fold is deliberate: `aggregate`
        // excludes a withdrawn pair because a total that counts one is wrong,
        // and that is a derived view; this is the ledger itself. The SPI states
        // it outright — a withdrawn pair MUST be returned as persisted here and
        // from `list_usage_records` — because hiding either half destroys the
        // audit trail the append-only model exists to keep. A consumer wanting
        // the netted view has what it needs: an invalidation names its target
        // (`UsageRecord::invalidation`), so the fold happens on the reader's
        // side.
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

    /// One feed page, run as the six-step protocol this plugin's
    /// `docs/DESIGN.md` §3.6 `cpt-cf-uc-plugin-seq-feed-page` sets out.
    ///
    /// Steps 1-5 run on one connection, inside a real [`sqlx::Transaction`]
    /// opened with [`sqlx::Connection::begin_with`] rather than a raw `BEGIN`.
    /// That is not cosmetic: `sqlx` tracks an open `Transaction`, so if this
    /// future is dropped before [`sqlx::Transaction::commit`] runs (a host-side
    /// timeout or a client disconnect) the drop queues a `ROLLBACK`. Without
    /// that tracking the connection would return to the pool still holding an
    /// open `REPEATABLE READ READ ONLY` transaction, and the next caller to
    /// acquire it would run inside an abandoned snapshot.
    ///
    /// The transaction borrows `conn` only until [`sqlx::Transaction::commit`]
    /// consumes it, so step 6 — the authoritative autocommit mark re-check —
    /// runs on the same pooled connection afterward, and both it and step 3's
    /// fast-path check reach one [`Self::mark_stands_above`].
    ///
    /// # Errors
    ///
    /// See [`RecordStore::feed_page`].
    // @cpt-flow:cpt-cf-uc-plugin-seq-feed-page:p2
    // @cpt-flow:cpt-cf-uc-plugin-flow-read-feed-page:p1
    // @cpt-algo:cpt-cf-uc-plugin-algo-feed-page-protocol:p1
    // @cpt-state:cpt-cf-uc-plugin-state-feed-position:p2
    // @cpt-dod:cpt-cf-uc-plugin-dod-settled-page-protocol:p1
    // @cpt-dod:cpt-cf-uc-plugin-dod-feed-completeness:p1
    // @cpt-dod:cpt-cf-uc-plugin-dod-named-start:p1
    // @cpt-dod:cpt-cf-uc-plugin-dod-gateway-owned-position:p1
    // @cpt-dod:cpt-cf-uc-plugin-dod-feed-freshness-bound:p1
    async fn feed_page(
        &self,
        subscription: &[Uuid],
        scope: &ast::Expr,
        after: Option<(u64, Uuid)>,
        until: Option<(u64, Uuid)>,
        limit: u64,
    ) -> Result<FeedPageRows, UsageCollectorPluginError> {
        // The drop-timer records `uc_timescaledb_feed_page_duration_seconds` on
        // every return, including the validation and refusal error arms below.
        let _timer = OpDurationGuard::start(Arc::clone(&self.metrics), TimedOp::FeedPage);

        // Translated before a connection is acquired, so a scope that cannot
        // be rendered never opens a transaction — the discipline `get`
        // already keeps.
        let (sql, binds) = build_feed_page_sql(after, until, scope, limit)
            .map_err(UsageCollectorPluginError::internal)?;
        // Deduplicated, order-preserving: `unnest($1::uuid[])` iterates per
        // array *element*, so a repeated type would deliver its rows twice.
        // `FeedSubscription::new` already sorts and dedups, but that invariant
        // lives two crates away, so this statement keeps its own correctness
        // local. `sort_unstable` + `dedup` would also dedup but erase the
        // caller's order, which `unnest` walks the array in.
        let types = dedup_subscription_types(subscription);

        let mut conn = self.timed_acquire().await?;

        // Step 1. READ ONLY because nothing here writes, and REPEATABLE READ
        // because the horizon read must fix the snapshot the page statement
        // then runs under. See this method's own doc for why `begin_with`
        // rather than a raw `BEGIN` statement.
        let mut tx = conn
            .begin_with("BEGIN ISOLATION LEVEL REPEATABLE READ READ ONLY")
            .await
            .map_err(|e| self.record_backend_error(&e))?;

        let page = self
            .feed_page_in_transaction(&mut tx, &sql, &binds, &types, after, until, limit)
            .await;

        // Step 5, on the success path only. `?` propagates a steps-2-4 error
        // before `tx` is committed; dropping an uncommitted `Transaction` queues
        // its `ROLLBACK`, so an error is rolled back rather than left for an
        // unconditional `COMMIT` to paper over.
        let page = page?;
        tx.commit()
            .await
            .map_err(|e| self.record_backend_error(&e))?;

        // Step 6. Authoritative, in autocommit, after the COMMIT: step 3 runs
        // under the page's snapshot and can miss a drop that commits after
        // it. Short-circuiting on `page.marked` is what keeps this the
        // "opt page_after present, not marked" branch of DESIGN §3.6's
        // diagram: a mark the fast path already found is not re-read here.
        //
        // One shared return for both refusal reasons (step 3's fast-path find
        // or step 6's live re-check), so one counter call here covers both:
        // the counter counts refusals, not re-checks, and this method never
        // returns `CursorBeyondRetention` from anywhere else.
        if let Some(position) = after
            && (page.marked || self.mark_stands_above(&mut conn, &types, position).await?)
        {
            self.metrics.inc_feed_cursor_refusal();
            return Err(UsageCollectorPluginError::CursorBeyondRetention);
        }

        Ok(FeedPageRows {
            entries: page.entries,
            next: page.next,
        })
    }

    /// Keyset-paginated ledger read over one meter and one covered-period
    /// range, with the caller's composed `$filter`, the metadata side channel
    /// and an optional keyset.
    ///
    /// The statement is [`build_list_sql`]'s and the page is
    /// [`build_list_page`]'s; what is left here is the round trip between
    /// them. Both halves are pure, so both are tested without a database.
    ///
    /// Selection reads the covered-period end alone, `from <= window_end <
    /// to`, and entries are returned as persisted, withdrawn pairs included.
    /// Each of the two is stated where it is enforced rather than only here.
    ///
    /// # Errors
    ///
    /// Returns [`UsageCollectorPluginError::Internal`] when the statement
    /// cannot be built (an untranslatable `$filter`, an order or keyset field
    /// off the allowlist) or the page cannot be assembled (an order field
    /// that is not a keyset key, a keyset that is empty or too large, a
    /// stored row that cannot be mapped), and the mapped backend error when
    /// the query itself fails.
    // @cpt-flow:cpt-cf-uc-plugin-seq-list-keyset:p2
    // @cpt-flow:cpt-cf-uc-plugin-flow-read-raw-page:p1
    // @cpt-algo:cpt-cf-uc-plugin-algo-consistency-profile-publication:p1
    // @cpt-dod:cpt-cf-uc-plugin-dod-consistency-profile-publication:p1
    async fn list(
        &self,
        gts_type_uuid: Uuid,
        time_range: TimeRange,
        query: &ODataQuery,
        metadata_filter: &[MetadataFilter],
        keyset: Option<&Keyset>,
    ) -> Result<RecordPage, UsageCollectorPluginError> {
        // The drop-timer records the histogram on every return, including the
        // validation error arms below, not just on success.
        let _timer =
            OpDurationGuard::start(Arc::clone(&self.metrics), TimedOp::Query(QueryKind::Raw));
        self.metrics.inc_query_request(QueryKind::Raw);

        // Defense-in-depth: clamp the caller's `$top` to `MAX_PAGE_SIZE` so a
        // value that slipped past the core gateway's `$top` cap can never drive
        // an unbounded `LIMIT n+1 ... fetch_all`.
        let limit = effective_page_size(query.limit, DEFAULT_PAGE_SIZE);

        // Built before a connection is acquired, so a query that cannot be
        // rendered never reaches the pool and never reads a row.
        let (sql, binds) = build_list_sql(
            gts_type_uuid,
            time_range,
            query,
            metadata_filter,
            keyset,
            limit,
        )
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
    ///   bucket list. That bucket carries `Some(0)` under `COUNT` **and** under
    ///   `SUM`, both of which DESIGN §3.3 defines over an empty selection;
    ///   `MAX`, `MIN` and `LATEST` are not defined over one and keep the `None`
    ///   ([`usage_collector_sdk::AggregationBucket::value`]). The two zeroes
    ///   arrive differently: `COUNT` is `SELECT COUNT(*)`'s own answer, while
    ///   `SUM` over zero rows is `NULL` in `PostgreSQL` and is made `0` by a
    ///   `COALESCE` in the statement — [`fold_select_expr`] on the scan path,
    ///   [`build_rollup_aggregate_sql`] on the rollup one.
    ///
    /// Both are held by a pair of tests against a lazy pool at a dead DSN, where
    /// reaching the pool answers `Transient` and stopping before it answers
    /// `Internal`: `a_fold_that_cannot_be_built_never_reaches_the_pool` requires
    /// the `Internal`, `the_ungrouped_fold_still_reaches_the_pool` the
    /// `Transient`. Either alone would leave the ordering unpinned.
    ///
    /// The dimension columns read positionally as `Option<String>` and the fold
    /// at index `k` as `Option<BigDecimal>` — arbitrary precision, so a wide
    /// `SUM` cannot overflow on decode.
    ///
    /// **`LATEST` materializes before it picks, so its peak memory is O(largest
    /// group).** The expression is `(ARRAY_AGG(r.quantity ORDER BY …))[1]`, so
    /// `PostgreSQL` builds a group's values into an array before taking the
    /// head. An aggregate carrying its own `ORDER BY` takes the grouped node off
    /// the hash path, so the plan is `Sort → GroupAggregate` and exactly one
    /// array is live at a time. Peak memory therefore grows with the row count
    /// of the largest group, not with the number of groups, and the array does
    /// not spill. (The `Sort` beneath is `work_mem`-bounded and needed by every
    /// candidate formulation, so it is not a cost of this one.)
    ///
    /// **Nothing bounds the rows within one group except the caller's covered
    /// period, which is a request parameter.** [`aggregate_limit_clause`] bounds
    /// the number of groups and never the rows inside one, so a `LATEST` read
    /// with a wide largest group is a server-side allocation sized by caller
    /// input. `SUM`, `COUNT`, `MAX` and `MIN` accumulate fixed state per group.
    ///
    /// **Two alternatives are O(1) per group and both are kept out.**
    /// `DISTINCT ON` and `ROW_NUMBER() OVER (PARTITION BY …) = 1` avoid the
    /// array, but neither is a `SELECT`-list expression a caller can drop in
    /// beside `SUM`, and neither expresses the ungrouped fold — `DISTINCT ON ()`
    /// is a syntax error, and `PARTITION BY` nothing answers **zero** rows over
    /// an empty selection where this method owes exactly one empty-keyed bucket.
    ///
    /// An eligible `SUM` or `COUNT` is served from `usage_rollup_1h` (see
    /// `query::rollup`); the result is numerically identical to the scan's, up
    /// to the rollup's refresh watermark (spec §8.1) and given spec §5.2's
    /// invariants.
    ///
    /// # Errors
    ///
    /// Returns [`UsageCollectorPluginError::Internal`] when the composed filter
    /// cannot be translated, the query fails, or a result column cannot be
    /// decoded at its expected type; a pool-acquisition failure surfaces as
    /// whatever `timed_acquire` classifies it as.
    // @cpt-flow:cpt-cf-uc-plugin-seq-query-aggregated:p2
    // @cpt-flow:cpt-cf-uc-plugin-flow-run-aggregated-query:p1
    // @cpt-dod:cpt-cf-uc-plugin-dod-rollup-eligibility-and-path-reporting:p1
    // @cpt-dod:cpt-cf-uc-plugin-dod-latest-memory-bound-publication:p1
    async fn aggregate(
        &self,
        gts_type_uuid: Uuid,
        time_range: TimeRange,
        fold: AggregationFold,
        query: &ODataQuery,
        metadata_filter: &[MetadataFilter],
        group_by: &[AggregationDimension],
    ) -> Result<AggregationResult, UsageCollectorPluginError> {
        // The drop-timer records the histogram on every return, not just
        // success.
        let _timer = OpDurationGuard::start(
            Arc::clone(&self.metrics),
            TimedOp::Query(QueryKind::Aggregated),
        );
        self.metrics.inc_query_request(QueryKind::Aggregated);

        // The rollup answers an eligible SUM/COUNT exactly (spec §6); every
        // other query takes the scan it always has. The eligibility test reads
        // the same composed filter the scan would translate.
        let routed = if self.rollup_enabled {
            Some(rollup_eligible(
                fold,
                query.filter(),
                metadata_filter,
                group_by,
                time_range,
            ))
        } else {
            None
        };
        let statement = match routed {
            Some(Ok(split)) => {
                let st = build_rollup_aggregate_sql(
                    gts_type_uuid,
                    split,
                    fold,
                    query.filter(),
                    group_by,
                )
                .map_err(UsageCollectorPluginError::internal)?;
                self.metrics.record_aggregate_path(None);
                AggregateStatement {
                    sql: st.sql,
                    binds: st.binds,
                    dim_count: st.dim_count,
                }
            }
            other => {
                if let Some(Err(reason)) = other {
                    self.metrics.record_aggregate_path(Some(reason));
                }
                build_aggregate_sql(
                    gts_type_uuid,
                    time_range,
                    fold,
                    query,
                    metadata_filter,
                    group_by,
                )
                .map_err(UsageCollectorPluginError::internal)?
            }
        };

        let mut q = sqlx::query(AssertSqlSafe(statement.sql));
        for b in &statement.binds {
            q = bind_one_query(q, b);
        }
        let mut conn = self.timed_acquire().await?;
        let rows = q
            .fetch_all(&mut *conn)
            .await
            .map_err(|e| self.record_backend_error(&e))?;

        // One bucket per row, in the order `PostgreSQL` emitted them. The `map`
        // keeps the no-grouping case honest: no branch on `group_by.is_empty()`
        // exists to answer an empty bucket list with. The count comes from the
        // statement that was built, not a second reading of `group_by`, so the
        // decoder cannot read a different number of key columns than the SELECT
        // list emits.
        let buckets = rows
            .iter()
            .map(|row| aggregate_bucket(row, statement.dim_count))
            .collect::<Result<Vec<_>, _>>()?;

        Ok(AggregationResult { buckets })
    }

    /// Spec §9.3's three selections, run as [`build_reconciliation_summary_sql`]'s
    /// S1∧S2 statement followed by [`build_reconciliation_watermarks_sql`]'s S3,
    /// on the same pooled connection.
    ///
    /// **[`OpDurationGuard`] with [`TimedOp::Reconciliation`], not a third
    /// [`QueryKind`] variant.** This plugin's `QueryKind` is fixed at
    /// `{Aggregated, Raw}` by `docs/DESIGN.md`, so a third value would emit a
    /// label that document does not declare.
    /// `uc_timescaledb_reconciliation_duration_seconds` (§4.3) is unlabelled, as
    /// `uc_timescaledb_feed_page_duration_seconds` is, so a dedicated label-free
    /// `TimedOp` variant records it.
    ///
    /// Two statements rather than one: see
    /// [`build_reconciliation_watermarks_sql`] for why S3 cannot join S1∧S2's
    /// ranged scan without forfeiting the leading-edge index read. Both run
    /// against the one connection [`Self::timed_acquire`] returns, but are not
    /// wrapped in one transaction: `READ COMMITTED` still lets a commit land
    /// between them, which the DESIGN sequence does not rule out either.
    ///
    /// # Errors
    ///
    /// Returns [`UsageCollectorPluginError::Internal`] when `scope` cannot be
    /// translated, when either statement fails, or when a result column
    /// cannot be decoded at its expected type; a pool-acquisition failure
    /// surfaces as whatever [`Self::timed_acquire`] classifies it as.
    // @cpt-flow:cpt-cf-uc-plugin-seq-reconciliation:p2
    // @cpt-flow:cpt-cf-uc-plugin-flow-read-reconciliation-figures:p2
    // @cpt-algo:cpt-cf-uc-plugin-algo-accepted-count:p2
    // @cpt-algo:cpt-cf-uc-plugin-algo-fold-appropriate-summary:p2
    // @cpt-algo:cpt-cf-uc-plugin-algo-watermark-reads:p2
    // @cpt-dod:cpt-cf-uc-plugin-dod-accepted-count-includes-withdrawals:p2
    // @cpt-dod:cpt-cf-uc-plugin-dod-unbounded-watermarks:p2
    // @cpt-dod:cpt-cf-uc-plugin-dod-scope-applied-first:p2
    // @cpt-dod:cpt-cf-uc-plugin-dod-empty-selection-figures:p2
    // @cpt-dod:cpt-cf-uc-plugin-dod-no-paging-no-rollup:p2
    async fn reconciliation(
        &self,
        tenant_id: Uuid,
        meter: &MeterRef,
        time_range: TimeRange,
        fold: AggregationFold,
        scope: &ast::Expr,
    ) -> Result<ReconciliationMetadata, UsageCollectorPluginError> {
        let gts_type_uuid = meter.uuid;

        // The drop-timer records
        // `uc_timescaledb_reconciliation_duration_seconds` on every return,
        // including the validation and decode error arms below.
        let _timer = OpDurationGuard::start(Arc::clone(&self.metrics), TimedOp::Reconciliation);

        // Both statements are built before a connection is acquired, so a
        // scope that cannot be rendered never reaches the pool and never
        // reads a row.
        let summary_stmt =
            build_reconciliation_summary_sql(gts_type_uuid, time_range, tenant_id, fold, scope)
                .map_err(UsageCollectorPluginError::internal)?;
        let watermarks_stmt = build_reconciliation_watermarks_sql(gts_type_uuid, tenant_id, scope)
            .map_err(UsageCollectorPluginError::internal)?;

        let mut conn = self.timed_acquire().await?;

        let mut q = sqlx::query(AssertSqlSafe(summary_stmt.sql));
        for b in &summary_stmt.binds {
            q = bind_one_query(q, b);
        }
        let summary_row = q
            .fetch_one(&mut *conn)
            .await
            .map_err(|e| self.record_backend_error(&e))?;

        // Column 0 is always `accepted_count`, the bare unfiltered `COUNT(*)`
        // — never absent, never negative.
        let accepted_count: i64 = summary_row.try_get(0).map_err(|e| {
            UsageCollectorPluginError::internal(format!(
                "reconciliation accepted_count column read failed: {e}"
            ))
        })?;

        // Columns 1.. are `build_reconciliation_summary_sql`'s two shapes, read
        // by the same `QuantitySummary::accrues` branch that chose which one the
        // statement built, so the decoder cannot read a shape the SELECT list
        // disagrees with.
        let quantity_summary = if QuantitySummary::accrues(fold) {
            let total: BigDecimal = summary_row.try_get(1).map_err(|e| {
                UsageCollectorPluginError::internal(format!(
                    "reconciliation accrued summary column read failed: {e}"
                ))
            })?;
            QuantitySummary::Accrued(total)
        } else {
            let count: i64 = summary_row.try_get(1).map_err(|e| {
                UsageCollectorPluginError::internal(format!(
                    "reconciliation observation count column read failed: {e}"
                ))
            })?;
            let latest: Option<Decimal> = summary_row.try_get(2).map_err(|e| {
                UsageCollectorPluginError::internal(format!(
                    "reconciliation latest observation column read failed: {e}"
                ))
            })?;
            let latest = latest
                .map(UsageQuantity::try_from)
                .transpose()
                .map_err(|e| {
                    UsageCollectorPluginError::internal(format!(
                        "reconciliation latest observation quantity invalid: {e}"
                    ))
                })?;
            // `count` (column 1, `COUNT(*)`) and `latest` (column 2, a separate
            // "pick the latest row" subquery) are two independently computed SQL
            // projections; nothing but the query text keeps them in agreement. A
            // disagreement here means those two projections drifted apart, which
            // `ObservedQuantity` cannot represent, so it is reported rather than
            // silently reshaped into one branch or the other.
            let count = NonZeroU64::new(u64::try_from(count).unwrap_or(u64::MAX));
            let observed = match (count, latest) {
                (None, None) => None,
                (Some(count), Some(latest)) => Some(ObservedQuantity { count, latest }),
                (count, latest) => {
                    return Err(UsageCollectorPluginError::internal(format!(
                        "reconciliation observation count ({count:?}) and latest observation \
                         ({latest:?}) disagree on whether the range selected anything"
                    )));
                }
            };
            QuantitySummary::Observations(observed)
        };

        let mut q = sqlx::query(AssertSqlSafe(watermarks_stmt.sql));
        for b in &watermarks_stmt.binds {
            q = bind_one_query(q, b);
        }
        let watermarks_row = q
            .fetch_one(&mut *conn)
            .await
            .map_err(|e| self.record_backend_error(&e))?;
        let max_accepted_at: Option<OffsetDateTime> = watermarks_row.try_get(0).map_err(|e| {
            UsageCollectorPluginError::internal(format!(
                "reconciliation max_accepted_at column read failed: {e}"
            ))
        })?;
        let max_window_end: Option<OffsetDateTime> = watermarks_row.try_get(1).map_err(|e| {
            UsageCollectorPluginError::internal(format!(
                "reconciliation max_window_end column read failed: {e}"
            ))
        })?;

        Ok(ReconciliationMetadata {
            accepted_count: u64::try_from(accepted_count).unwrap_or(u64::MAX),
            quantity_summary,
            max_accepted_at,
            max_window_end,
        })
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "record_store_tests.rs"]
mod record_store_tests;
