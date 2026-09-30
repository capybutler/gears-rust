//! Postgres-backed [`RecordStore`] over the `usage_records` hypertable.
//!
//! All operations — `create` / `create_batch` / `get` / `list` / `aggregate` —
//! are real `sqlx`.
//!
//! **On this file's size.** Task 13 of the `TimescaleDB` port considered
//! splitting the 23 free functions here (845 lines, none of which touches
//! `PgRecordStore` or `sqlx`) into a sibling module and **decided against it**.
//! The argument that motivated the split — letting a future task
//! `#[path]`-include a real file instead of a regenerated extract — expired
//! when the crate started compiling, and more than half of what would move is
//! read-path, so no name describes the boundary except "these don't touch
//! `sqlx`". Re-open it if this file becomes hard to *edit* for a concrete
//! reason, not on line count. Full reasoning:
//! `docs/superpowers/plans/2026-09-09-usage-collector-timescaledb-port.md`,
//! Task 13 Step 2c.

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
use sqlx::postgres::{PgConnection, PgRow};
use sqlx::{Acquire as _, AssertSqlSafe, Connection as _, FromRow as _, PgPool, Postgres, Row};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use tokio_util::sync::CancellationToken;
use toolkit_odata::filter::FilterField;
use toolkit_odata::{ODataQuery, Page as ODataPage, PageInfo, SortDir, ast};
use uuid::Uuid;

use usage_collector_sdk::{
    AggregationBucket, AggregationDimension, AggregationFold, AggregationResult, MetadataFilter,
    MeterTypeId, TimeRange, UsageCollectorPluginError, UsageRecord, UsageRecordFilterField,
    is_keyset_safe_record_field,
};

use crate::domain::ports::{FeedPageRows, RecordStore};
use crate::infra::metrics::{ErrorClass, InsertMode, Metrics, OpDurationGuard, QueryKind, TimedOp};
use crate::infra::storage::entity::{FeedRecordRow, UsageRecordRow};
use crate::infra::storage::error::{
    acquire_error_clears_readiness, is_ledger_pk_violation, map_sqlx_err,
};
use crate::infra::storage::feed_position::MAX_UUID;
use crate::infra::storage::mapper::{
    invalidation_to_row, metadata_map_to_jsonb, record_row_to_model,
};
use crate::infra::storage::query::aggregate::{
    aggregate_limit_clause, dimension_presence_guard, dimension_select_expr, fold_select_expr,
    withdrawal_exclusion_clause,
};
use crate::infra::storage::query::feed::{MARK_ABOVE_SQL, build_feed_page_sql};
use crate::infra::storage::query::keyset::{
    encode_next_cursor, ensure_forward_cursor, keyset_predicate, render_order_by,
};
use crate::infra::storage::query::rollup::{build_rollup_aggregate_sql, rollup_eligible};
use crate::infra::storage::query::translate::{
    SqlBind, SqlCtx, bind_one, bind_one_query, record_column, translate_scope,
};
use crate::infra::storage::query::{
    effective_page_size, ledger_from_clause, push_metadata_filter_clauses,
    push_meter_and_range_clauses,
};
use crate::infra::storage::type_key::TypeKeyCache;

/// Default page size when the caller omits `$top` (`query.limit`).
const DEFAULT_PAGE_SIZE: u64 = 100;

/// Column list for `get`, `list`, and every write path's conflict read-back
/// and `RETURNING`, in [`UsageRecordRow`] field order. A static const (never
/// caller input), so there is no risk of SQL injection.
///
/// `sqlx`'s derived `FromRow` looks each column up by the struct's own field
/// name, so the order here is a reading convenience — matching the struct and
/// the DDL — rather than a decode requirement. **Omission is the hazard**: a
/// missing column fails the decode with `no column found for name: <field>`.
///
/// **This is the same column set as [`INSERT_COLUMNS`], cast where decoding
/// needs it, not a superset of it.** `xact_id` — stamped by its column
/// default and bound by no insert — used to be read back here too, which was
/// the one difference from [`INSERT_COLUMNS`]; [`FEED_COLUMNS`] carries it
/// now, because the feed page is the one reader that needs it and every
/// other reader of this constant does not.
///
/// **One entry is cast rather than named bare**, and the cast is what makes
/// the column decodable at all: `usage_entry_type` is a `PostgreSQL` enum,
/// and the [`String`] [`UsageRecordRow`] carries declares itself `TEXT`,
/// which `sqlx` holds incompatible with an enum. It therefore reads as
/// `entry_type::text AS entry_type`. The cast is required; the alias is not,
/// since `PostgreSQL` names the output of a bare column cast after the
/// column anyway. It is written out so the decoded name
/// [`UsageRecordRow::entry_type`] is looked up by is stated here rather than
/// inherited from a server naming rule — and [`ins_record_columns`] reads
/// exactly that decoded name back out of this constant, to qualify it with
/// the guarded statement's `ins` relation.
pub(crate) const RECORD_COLUMNS: &str = "id, tenant_id, gts_type_id, type_key, quantity, \
     window_start, window_end, resource_id, resource_type, subject_id, subject_type, \
     idempotency_key, invalidates, reason_code, origin, entry_type::text AS entry_type, \
     accepted_at, metadata";

/// [`RECORD_COLUMNS`] plus `xact_id`, for the one reader that needs the feed
/// order's own key: `query/feed.rs`'s page statement.
///
/// **`xact_id` is the whole of the difference between this and
/// [`RECORD_COLUMNS`]**, the way it used to be the whole of the difference
/// between [`RECORD_COLUMNS`] and [`INSERT_COLUMNS`] before this split. There
/// are now two read column lists rather than one, and this sentence is the
/// account of both: [`RECORD_COLUMNS`] names exactly what every insert
/// writes, and this constant adds the one column the database stamps that
/// [`RECORD_COLUMNS`]' own readers never needed.
///
/// **The cast is aliased `xact_id_text`, not `xact_id`, and that is
/// load-bearing rather than decorative.** `query/feed.rs`'s statement orders
/// by the bare name `xact_id` (`ORDER BY xact_id, id`, over
/// `usage_records_feed_idx`), and `PostgreSQL` resolves a bare `ORDER BY`
/// name that matches both an output column and an input column to the
/// *output* column. Aliasing this cast back to `xact_id` — its own source
/// column's name — would make that `ORDER BY` bind to this `text` column and
/// sort lexicographically over digit strings rather than numerically over
/// the `xid8` one, exactly the hazard
/// [`crate::infra::storage::retention_sweep::CHUNK_HIGHEST_POSITIONS_SQL`]'s
/// own `xact_id_text` alias avoids in the retention sweep's own read.
/// [`FeedRecordRow::xact_id`]'s `#[sqlx(rename)]` is what lets the field keep
/// the name every reader of it expects while decoding from the differently
/// named column.
///
/// `xid8` has no `sqlx` `Decode` implementation, which is why the column is
/// cast to `text` at all — the same reason [`FeedRecordRow::xact_id`] is a
/// [`String`].
///
/// `pub(crate)` so `query::feed`'s page-statement builder can read the same
/// spelling rather than a second one — the same reason [`ENTRY_TYPE_ENUM`] is
/// `pub(crate)` for `query::translate`.
pub(crate) const FEED_COLUMNS: &str = "id, tenant_id, gts_type_id, type_key, quantity, \
     window_start, window_end, resource_id, resource_type, subject_id, subject_type, \
     idempotency_key, invalidates, reason_code, origin, entry_type::text AS entry_type, \
     accepted_at, metadata, xact_id::text AS xact_id_text";

/// The columns every insert writes: every ledger column but `xact_id`, which
/// the database stamps.
///
/// **One spelling for every place a write statement names its columns** — each
/// path's `input` CTE select list, the `INSERT`'s own column list, the `SELECT`
/// that feeds it from `input`, and the alias list of the `VALUES` row or
/// `UNNEST` the input is built from ([`guarded_statement`]). Written out
/// separately instead, a name transposed in any one of them binds a value to
/// the wrong same-typed column, which Postgres accepts without complaint and
/// which no row-level test can see. The list is not counted here, because it
/// grows.
///
/// `metadata` is deliberately **last**, because the `input` select list appends
/// `::jsonb AS metadata` to this string rather than restating it (see
/// [`guarded_statement`]) — the cast binds to the final identifier only, which
/// is what makes that list unable to be a
/// transposition rather than merely tested not to be. A test asserts the
/// position directly, because deriving it from an order assertion whose oracle
/// happens to end in `metadata` would not survive a migration that declares a
/// column after it.
const INSERT_COLUMNS: &str = "id, tenant_id, gts_type_id, type_key, quantity, window_start, \
     window_end, resource_id, resource_type, subject_id, subject_type, idempotency_key, \
     invalidates, reason_code, origin, entry_type, accepted_at, metadata";

/// The `PostgreSQL` enum `entry_type` is declared as
/// (`migrations/0001_init.sql`), and the cast every bind of that column
/// carries.
///
/// **Measured, not argued.** `sqlx` types a bound `&str` as `text`, and
/// `PostgreSQL` refuses `text` in assignment to an enum column (`42804`, "you
/// will need to rewrite or cast the expression"), so a bare `$n` does not
/// land. An explicit cast on the placeholder does, which is why
/// [`INSERT_COLUMN_TYPES`] names the enum where the DDL does rather than
/// diverging to `text` the way `metadata` does: the batch reads it as
/// `UNNEST($n::usage_entry_type[])` and the single-row path as
/// `$n::usage_entry_type` inside its `VALUES` row.
///
/// `pub(crate)` so the `$filter` translator's `bind_cast`
/// ([`super::query::translate`]) spells it from here too: one spelling in
/// *production* Rust, rather than the name hardcoded a second time there. Test
/// oracles spell it out instead, and deliberately - one derived from this const
/// could not see a rename that moved it, because both sides would move
/// together.
pub(crate) const ENTRY_TYPE_ENUM: &str = "usage_entry_type";

/// Postgres types for [`INSERT_COLUMNS`], **in the same order**, as each write
/// path's `input` source needs them: the batch `UNNEST`s the array of each
/// (`text` becomes `text[]`) and the single-row path casts one placeholder to
/// each. A test pins its length equal to the number of names in
/// [`INSERT_COLUMNS`], which is what fixes both parameter counts.
///
/// Named for the columns rather than for the arrays, because only one of the
/// two uses is an array: an earlier `…_ARRAY_TYPES` was true while the batch
/// path was its only reader.
const INSERT_COLUMN_TYPES: [&str; 18] = [
    "uuid",
    "uuid",
    "text",
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
/// `entry_type` is one of the six, and is named here as the bare column: an
/// arbiter names index columns, never the values compared against them, so the
/// enum needs no cast in this position. Without it a withdrawal would
/// arbitrate against the very entry it withdraws, which repeats its tenant,
/// type, idempotency key and covered period.
const DEDUP_CONFLICT_TARGET: &str =
    "tenant_id, gts_type_id, idempotency_key, window_start, window_end, entry_type, type_key";

/// The first statement of every write transaction whose commit is acknowledged
/// to a caller, per `DESIGN.md` §3.5: *"Every write transaction whose commit is
/// acknowledged to a caller runs `SET LOCAL synchronous_commit = on`, so an
/// operator-level `synchronous_commit` of `off` or `local` cannot weaken an
/// acknowledgement."* The retention sweep's transaction is not one of them, and
/// §3.6's sweep sequence omits this statement deliberately.
///
/// **Why it cannot be a connection parameter like the timeouts.** The bound the
/// plugin owes is per *transaction*, and a session default is what an operator
/// can already set; `SET LOCAL` reverts at `COMMIT`, so it states the write
/// path's own requirement rather than reconfiguring the pool. The server-wide
/// `fsync` and `full_page_writes` are the pair that genuinely cannot be forced
/// here, which is why they are startup checks instead
/// ([`crate::infra::storage::pool`]).
///
/// **It does not cost the guarded statement its place as the transaction's
/// first write**, which `DESIGN.md` §3.6 requires of it and the feed's settled
/// horizon rests on. `SET` writes no tuple and takes no `XID`: `PostgreSQL`
/// assigns one lazily, at the first statement that actually writes, so the
/// transaction is still virtual until the `INSERT` runs. A later reader will
/// ask, which is why this says so here.
const FORCE_SYNCHRONOUS_COMMIT_SQL: &str = "SET LOCAL synchronous_commit = on";

/// The ledger's **time** partition column, named beside the `id` in both
/// conflict read-backs **for pruning and for nothing else**.
///
/// `usage_records` is a hypertable with two range dimensions,
/// `by_range('window_end')` and `by_range('type_key', …)`
/// (`migrations/0001_init.sql`). A predicate constraining neither cannot
/// exclude a chunk at all: `WHERE id = $1` alone probes the
/// `(id, window_end, type_key)` index of *every* chunk the ledger holds, and
/// that count grows with the deployment's retention. Both read-backs run
/// **inside the write transaction**, while it still holds the speculative tuple
/// locks of everything it inserted, and on the ordinary idempotent-retry path
/// rather than an error path — which is the hold time
/// [`crate::infra::storage::pool`]'s `lock_timeout` reasoning is about.
///
/// Constraining this column prunes the **time** dimension, to the chunks of the
/// entry's own period — **one per `type_key` slice**, not one chunk, since the
/// second dimension is left unconstrained. The other dimension could be pruned
/// too: `type_key` is resolved before the write transaction opens and is in
/// scope at both read-backs. It is deliberately not bound here, so that what
/// this constant is about stays one thing; binding it is a separate change with
/// its own measurement to take.
///
/// **It is not a retreat to the 6-tuple, and must not be read as one.** The
/// read-back keys on `id` (`DESIGN.md` §3.6), and this column discriminates
/// nothing `id` does not already: the covered-period end is one of the six
/// inputs the `id` is derived over, so two rows agreeing on `id` agree on it.
/// Naming it changes which chunks are scanned, never which row is selected.
/// Anyone tempted to simplify it back out should reach this paragraph first.
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
/// `abs(…)` is what makes it either direction. Dropped, an entry dated next
/// year is admitted and orders ahead of everything real in acceptance terms;
/// the sign flipped, the past-dated half goes unguarded. `acceptance_slack_pg`
/// carries one assertion per direction, because a single one reaches half of
/// the predicate.
///
/// **One spelling, used by both write paths.** §3.6: *"Nothing before or after
/// the statement computes the guard, since a later statement would run under a
/// later `statement_timestamp()`."* Two spellings would be two clocks to keep
/// agreeing; one is read from here by [`guarded_statement`] and is the whole of
/// what either path computes. It reads `t.accepted_at` off the input relation
/// rather than a placeholder, which is why the single-row path builds its input
/// from a `VALUES` row rather than binding the columns into the INSERT: both
/// paths then present the same relation to the same predicate.
fn admitted_expr() -> String {
    format!(
        "(abs(extract(epoch FROM (t.accepted_at - statement_timestamp()))) <= ${}::bigint) \
         AS admitted",
        slack_placeholder()
    )
}

/// [`RECORD_COLUMNS`] as the outer select reads it back off `ins`.
///
/// The read list's one cast entry was already cast inside `ins`'
/// `RETURNING`, so what `ins` exposes is the **decoded** name of that entry —
/// `entry_type` is a `text` column there. Qualifying every name with `ins.`
/// is what makes the not-won row's columns `NULL` rather than silently
/// picking `input`'s like-named ones up, and it is what `USING (id)` would
/// otherwise decide for `id` on its own: the merged `id` of a left join is
/// the left side's and is never null, so `won` computed off it would be true
/// for every row.
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
/// WITH input AS (SELECT <INSERT_COLUMNS>::jsonb AS metadata, <admitted>
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
/// outside the slack never reaches `ON CONFLICT` at all, so it comes back
/// `admitted = false, won = false` whether or not its six inputs are already
/// stored, and the caller answers `Transient` without reading the ledger. §3.6:
/// *"a row not admitted is `Transient` … even when its identity exists"*.
///
/// **`input.id` is carried out** so the batch path can align each returned row
/// with the input it came from; `ins.id` is null on every row that did not win
/// and cannot do that job. The single-row path returns exactly one row and
/// ignores it.
///
/// **`xact_id` is bound nowhere, and this statement never reads it back
/// either.** [`INSERT_COLUMNS`] does not name it, so the column default
/// stamps it from the inserting transaction; [`RECORD_COLUMNS`] does not name
/// it either, since nothing on the write path needs the feed order's own key
/// — only [`FEED_COLUMNS`]' one reader does.
///
/// The `SELECT` list of `input` is [`INSERT_COLUMNS`] with a trailing
/// `::jsonb AS metadata`, which lands on `metadata` alone because `metadata` is
/// last (see [`INSERT_COLUMNS`]) — so `input`'s columns are that constant's
/// names, exactly, and the inner `SELECT <INSERT_COLUMNS> FROM input` cannot be
/// a transposition of them. The alias is written out rather than left to
/// `PostgreSQL` naming a bare column cast after its column, for the reason
/// [`RECORD_COLUMNS`] gives: here the inherited name would be load-bearing.
// @cpt-algo:cpt-cf-uc-plugin-algo-guarded-insert-statement:p1
// @cpt-dod:cpt-cf-uc-plugin-dod-single-entry-persistence:p1
// @cpt-dod:cpt-cf-uc-plugin-dod-acceptance-slack-refusal:p1
fn guarded_statement(input_source: &str) -> String {
    format!(
        "WITH input AS (\
             SELECT {INSERT_COLUMNS}::jsonb AS metadata, {} FROM {input_source}\
         ), \
         ins AS (\
             INSERT INTO usage_records ({INSERT_COLUMNS}) \
             SELECT {INSERT_COLUMNS} FROM input WHERE admitted \
             ON CONFLICT ({DEDUP_CONFLICT_TARGET}) DO NOTHING \
             RETURNING {RECORD_COLUMNS}\
         ) \
         SELECT input.id AS input_id, input.admitted, (ins.id IS NOT NULL) AS won, {} \
         FROM input LEFT JOIN ins USING (id)",
        admitted_expr(),
        ins_record_columns(),
    )
}

/// The single-row path's `input` source: one `VALUES` row of `$1..$n`, aliased
/// to [`INSERT_COLUMNS`].
///
/// **Every placeholder carries its column's type**, where the previous
/// `INSERT … VALUES` shape needed a cast on `entry_type` alone. A placeholder
/// inside a `VALUES` feeding a CTE has no target column to be inferred from, so
/// `PostgreSQL` refuses the statement outright (`42P18`, "could not determine
/// data type") without one. The types come from [`INSERT_COLUMN_TYPES`],
/// which is the migration's own list — the batch path spells the same types as
/// array element types, so the two paths cannot disagree about what a column
/// is. `metadata` is the one entry that diverges from the DDL there, `text`
/// rather than `jsonb`, and it is bound as `text` here too and cast by the
/// `input` select list ([`InsertColumns`] gives the reason).
fn single_input_source() -> String {
    let values = INSERT_COLUMN_TYPES
        .iter()
        .enumerate()
        .map(|(i, ty)| format!("${}::{ty}", i + 1))
        .collect::<Vec<_>>()
        .join(", ");
    format!("(VALUES ({values})) AS t({INSERT_COLUMNS})")
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

/// The single-row conflict read-back: the row that took this identity's slot.
///
/// Keyed on `id` ([`entry_identity`]), with [`PARTITION_PRUNE_COLUMN`] beside
/// it for pruning and not for selection.
///
/// **Measured on `timescale/timescaledb:2.29.2-pg18`, over a ledger holding
/// eight chunks of one meter type.** `WHERE id = $1` alone plans an `Append`
/// over all eight, one index-only scan of each chunk's primary key. With
/// `AND window_end = $2` the plan is an index scan of the chunks the entry's
/// period falls in — one, on that ledger, because one `type_key` slice existed;
/// the second partition dimension is unconstrained, so a ledger carrying *N*
/// meter types in that period would plan *N* ([`PARTITION_PRUNE_COLUMN`]). That
/// is the whole reason the column is named.
static SINGLE_CONFLICT_READ_SQL: LazyLock<String> = LazyLock::new(|| {
    format!(
        "SELECT {RECORD_COLUMNS} FROM usage_records \
         WHERE id = $1 AND {PARTITION_PRUNE_COLUMN} = $2"
    )
});

/// The batch conflict read-back: the rows that took these identities' slots.
///
/// **Two conjuncts, and neither is redundant in practice.** The row-value `IN`
/// pairs each `id` with its own period, which a second `= ANY` could not — that
/// would match one entry's `id` under another's period. The leading
/// `= ANY` is what the planner can exclude chunks with; it is logically implied
/// by the row-value and is therefore easy to mistake for dead weight.
///
/// **Measured on the same container and ledger as
/// [`SINGLE_CONFLICT_READ_SQL`], two identities out of eight chunks of one
/// meter type.** The row-value `IN` on its own plans a nested loop whose
/// `Append` still lists all eight chunks, probing each one's `window_end` index
/// per outer row: better than keying on `id` alone, which bitmap-scans eight
/// primary keys, and still every chunk. With the `= ANY` conjunct the `Append`
/// lists the chunks the batch's periods fall in — **two** on that ledger, one
/// per period because those two periods landed in different time chunks, and
/// *N* times the number of **time chunks** they span on a ledger carrying *N*
/// meter types, for the reason [`PARTITION_PRUNE_COLUMN`] gives. Delete the
/// conjunct and the read-back goes back to touching every chunk the deployment
/// retains, inside the write transaction and while it holds the batch's
/// speculative tuple locks.
static BATCH_CONFLICT_READ_SQL: LazyLock<String> = LazyLock::new(|| {
    format!(
        "SELECT {RECORD_COLUMNS} FROM usage_records \
         WHERE {PARTITION_PRUNE_COLUMN} = ANY($2::timestamptz[]) \
           AND (id, {PARTITION_PRUNE_COLUMN}) IN \
             (SELECT t1, t2 FROM UNNEST($1::uuid[], $2::timestamptz[]) AS t(t1, t2))"
    )
});

/// The single-row guarded statement ([`guarded_statement`] over
/// [`single_input_source`]).
///
/// Built rather than inlined so a test can read the column list, the
/// placeholder count and the conflict target back out of it — and built
/// **once**, because every input to it is a constant and the alternative is one
/// `format!` per column of [`INSERT_COLUMNS`] on every write.
static SINGLE_GUARDED_SQL: LazyLock<String> =
    LazyLock::new(|| guarded_statement(&single_input_source()));

/// The batch guarded statement ([`guarded_statement`] over
/// [`batch_input_source`]).
///
/// Built once, for the same reason as [`SINGLE_GUARDED_SQL`].
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
/// linked: they are private, and an intra-doc link from this public item's doc
/// to either resolves only under `--document-private-items`.
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
    /// admission guard (see [`admitted_expr`]).
    ///
    /// Held as an `i64` because that is what it binds as; the conversion
    /// happens once here rather than on every write. Config validation caps the
    /// setting far below `i64::MAX`, so the saturation in [`Self::new`] is a
    /// total function rather than a case anything reaches.
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
    /// primary-key `23505` to a non-retryable `Internal`. That stays right for a
    /// collision nothing intercepted: with the derived identity correct it is
    /// unreachable outside this race. Only a write path knows it is in the race,
    /// so only a write path may reclassify it.
    ///
    /// **Re-running resolves the race, and it must be a fresh transaction**:
    /// the winner is committed by the time this error is raised, so the re-run's
    /// `ON CONFLICT` pre-check sees its row and skips that identity, leaving the
    /// batch to resolve it as an ordinary dedup hit. `create_batch`'s bounded
    /// jittered retry ([`with_retry`]) is exactly that, and it already treats a
    /// deadlock victim the same way.
    ///
    /// **It does not resolve the other route**, and the `Transient` it returns
    /// says otherwise: on a mis-derived `id` the re-run's pre-check still finds
    /// nothing, the insert meets the same primary-key entry again, and
    /// [`MAX_BATCH_ATTEMPTS`] is spent before the host is told. The detail
    /// string names the race because the race is what it is sized for; it is
    /// left as it is deliberately, since re-wording it would buy a caller
    /// nothing it can act on differently and the divergence is the accepted
    /// cost [`entry_identity`] sets out.
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
    async fn timed_acquire(&self) -> Result<PoolConnection<Postgres>, UsageCollectorPluginError> {
        let t = Instant::now();
        match self.pool.acquire().await {
            Ok(conn) => {
                self.metrics.record_pool_acquire(t.elapsed().as_secs_f64());
                // A successful acquire re-arms readiness (this crate's
                // readiness contract: `uc_timescaledb_ready` recovers once the
                // pool serves a connection again), but only while not shutting
                // down: once `cancel` fires the shutdown
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

    /// Core single-row write path: the guarded statement
    /// ([`SINGLE_GUARDED_SQL`]), then lost-the-race absorb-vs-conflict
    /// resolution.
    ///
    /// The statement carries its own admission verdict out beside the dedup
    /// outcome, so the three answers `DESIGN.md` §3.6 names — refused, won,
    /// lost — are read off one round trip. A refused row is answered without
    /// reading the ledger at all: §3.6's *"a row not admitted is `Transient` …
    /// even when its identity exists"* is the statement's own `WHERE admitted`,
    /// not an ordering this function imposes.
    ///
    /// **One backend transaction**, holding the insert and — when the insert
    /// wins no slot — the read that resolves the conflict against the committed
    /// row, so the resolution cannot observe a ledger the insert never saw.
    ///
    /// `ON CONFLICT DO NOTHING` is the **write transaction's** only
    /// serialization authority, and the per-scope counter that used to be a
    /// second one is retired. The path still runs one statement that can wait
    /// before that transaction opens, [`TypeKeyCache::resolve`]'s own
    /// `INSERT ... ON CONFLICT`, in autocommit before `begin()`: a
    /// concurrent same-key insert blocks on the in-progress speculative tuple
    /// until the winner commits — bounded by the connection's `lock_timeout`
    /// ([`crate::infra::storage::pool`]), so the wait cannot pin the connection
    /// indefinitely — then its `DO NOTHING` returns no row and it resolves
    /// absorb-vs-conflict against the now-visible committed row.
    ///
    /// This carries the per-row counters (dedup absorbed / idempotency conflict
    /// / invalidation accepted / backend error) so they
    /// are recorded exactly once per row whether the caller is
    /// [`RecordStore::create`] (single) or [`RecordStore::create_batch`]
    /// (per-row loop). The `uc_timescaledb_insert_duration_seconds` histogram is
    /// deliberately NOT recorded here — the public methods time the whole call
    /// and tag it with the correct `mode`.
    async fn create_inner(
        &self,
        record: UsageRecord,
    ) -> Result<UsageRecord, UsageCollectorPluginError> {
        let mut conn = self.timed_acquire().await?;

        // The type's partition key, resolved in autocommit before the write
        // transaction opens (see `TypeKeyCache::resolve`).
        let type_key = self
            .type_keys
            .resolve(&mut conn, record.gts_type_id.as_str())
            .await
            .map_err(|e| self.record_backend_error(&e))?;

        // The write transaction, opened with `SET LOCAL synchronous_commit = on`
        // as its first statement ([`begin_durable_write`]).
        let mut tx = begin_durable_write(&mut conn)
            .await
            .map_err(|e| self.record_backend_error(&e))?;

        // 1. The guarded statement. It answers three ways
        //    ([`Admission`]): refused by the acceptance guard, won the dedup
        //    slot, or lost it. Every column [`INSERT_COLUMNS`] names is bound
        //    here, `accepted_at` and `entry_type` included — the kind is
        //    written from the dispatched entry's own declaration, never
        //    computed for the proposed row — plus the acceptance slack, which
        //    is the statement's last bind and the only one that is not a
        //    column ([`slack_placeholder`]). [`RECORD_COLUMNS`] reads back one
        //    column more than is bound: `xact_id`, which the column default
        //    stamps.
        //
        //    **Inside a SAVEPOINT**, because the statement has a fourth
        //    outcome: a concurrent writer of this same identity can make it
        //    fail on the PRIMARY KEY, which the arbiter does not cover
        //    ([`is_ledger_pk_violation`]). That error aborts the transaction,
        //    and step 2c still has to read the winner's row inside it, so the
        //    statement needs a point to roll back to. Of two faithful
        //    submissions this fires only under concurrency, and serially the
        //    savepoint is two statements of pure overhead; a mis-derived `id`
        //    reaches the same raise serially ([`entry_identity`]).
        //
        //    **The guarded statement does not make it redundant, and this was
        //    measured rather than argued.** Its `LEFT JOIN ins USING (id)`
        //    joins input to the rows *this statement* inserted, so a lost row
        //    comes back with every record column null: the join reports that
        //    the identity was taken and carries nothing about the row that took
        //    it, which is why DESIGN section 3.6 still reads the winner back by
        //    id in a second statement. Nor does the shape move the arbiter, so
        //    two withdrawals of one target -- which derive one id, `reason_code`
        //    being outside the six identity inputs -- still collide on the
        //    PRIMARY KEY. Removing the savepoint and running
        //    `records_ingest_integration_pg::two_concurrent_withdrawals_of_one_target_admit_exactly_one`
        //    failed 3 of 18 runs, the loser answering `Internal` where DESIGN
        //    section 3.3's `dedup-concurrent` row requires an
        //    `IdempotencyConflict`; the same test passed 10 of 10 with it.
        let subject_id = record
            .subject_ref
            .as_ref()
            .map(usage_collector_sdk::SubjectRef::subject_id);
        let subject_type = record.subject_ref.as_ref().and_then(|s| s.subject_type());
        // Rendered to text and cast `::jsonb` by the statement, exactly as the
        // batch path carries it: `metadata` is the one column whose bound type
        // diverges from the DDL's, and one divergence spelled once is what
        // keeps [`INSERT_COLUMN_TYPES`] usable by both paths.
        let metadata = metadata_map_to_jsonb(&record.metadata).to_string();
        // One helper for both columns, so the half-populated pair the read
        // direction refuses is unrepresentable on the way out too.
        let (invalidates, reason_code) = invalidation_to_row(record.invalidation.as_ref());
        let is_invalidation = invalidates.is_some();

        // The savepoint and the statement share one scope so the borrow of `tx`
        // ends before any outer rollback: every error below is carried out of
        // the block and acted on after it, rather than handled inside it.
        let attempted = async {
            let mut sp = tx.begin().await?;
            let row = sqlx::query(AssertSqlSafe(SINGLE_GUARDED_SQL.as_str()))
                .bind(record.id)
                .bind(record.tenant_id)
                .bind(record.gts_type_id.as_str())
                .bind(type_key)
                .bind(record.quantity.as_decimal())
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
                .bind(record.entry_type().as_str())
                .bind(record.accepted_at)
                .bind(metadata)
                .bind(self.acceptance_slack_secs)
                .fetch_one(&mut *sp)
                .await;
            match row {
                // RELEASE SAVEPOINT. Anything the statement wrote stays in the
                // outer transaction.
                Ok(row) => {
                    let admission = admission_of(&row)?;
                    sp.commit().await?;
                    Ok(admission)
                }
                // The sized-for case is a concurrent writer of this very
                // identity that won and committed — Postgres waits on its
                // speculative token before raising this, so
                // by now its row is committed, and step 2c's `SELECT` takes a
                // fresh snapshot and sees it. That last step is what needs
                // `READ COMMITTED`: under a deployer-set
                // `default_transaction_isolation = repeatable read` the read
                // would keep this transaction's original snapshot, the winner's
                // row would be invisible, and the loser would answer a stale
                // `Transient` where DESIGN section 3.3's `dedup-concurrent` row
                // requires it to resolve absorb-vs-conflict. That `Transient` is
                // [`CONFLICT_UNREADABLE_MESSAGE`], which is where this and the
                // other things reaching that arm are set out together.
                // ROLLBACK TO
                // SAVEPOINT clears the aborted state and leaves the outer
                // transaction usable.
                //
                // The outcome is `Lost` and not a guess: a row the guard
                // refused is never inserted, so it cannot collide on any index,
                // and a PRIMARY KEY collision therefore says the row was
                // admitted. Step 2c reads the winner and resolves
                // absorb-vs-conflict, which is what DESIGN section 3.3's
                // `dedup-concurrent` row requires of the loser.
                Err(e) if is_ledger_pk_violation(&e) => {
                    sp.rollback().await?;
                    Ok(Admission::Lost)
                }
                // Dropping `sp` queues its rollback; the outer transaction is
                // rolled back wholesale below in any case.
                Err(e) => Err(e),
            }
        }
        .await;

        let admission = match attempted {
            Ok(admission) => admission,
            Err(e) => {
                rollback(tx).await;
                return Err(self.record_backend_error(&e));
            }
        };

        match admission {
            // 2a. Refused by the guard. Nothing was written, and the ledger was
            //     never consulted: the verdict precedes the identity.
            Admission::Stale => {
                rollback(tx).await;
                self.metrics.inc_stale_acceptance_rejection();
                return Err(write_transient(&record, STALE_ACCEPTANCE_MESSAGE));
            }
            // 2b. Won the slot — fresh insert. Commit it.
            Admission::Won(row) => {
                tx.commit()
                    .await
                    .map_err(|e| self.record_backend_error(&e))?;
                if is_invalidation {
                    self.metrics.inc_invalidation();
                }
                return record_row_to_model(*row);
            }
            // Falls through to step 2c.
            Admission::Lost => {}
        }

        // 2c. Lost the slot — a row carrying this identity already exists. Read
        //     it by `id` and resolve absorb-vs-conflict. DESIGN section 3.6:
        //     "The read-back keys on `id`, never on `(tenant_id, gts_type_id,
        //     idempotency_key, window_start, window_end)`" — once a record is
        //     withdrawn the pair shares those five, so a read over them could
        //     answer a record's retry with its own withdrawal. See
        //     [`entry_identity`] for where the `id` comes from. The
        //     read mutates nothing and the rollback that follows discards
        //     nothing the ledger kept, so an absorbed retry leaves the ledger
        //     exactly as the winning insert left it.
        let stored =
            sqlx::query_as::<_, UsageRecordRow>(AssertSqlSafe(SINGLE_CONFLICT_READ_SQL.as_str()))
                .bind(entry_identity(&record))
                .bind(record.window_end)
                .fetch_optional(&mut *tx)
                .await;
        rollback(tx).await;
        let stored = stored.map_err(|e| self.record_backend_error(&e))?;

        if let Some(row) = stored {
            self.resolve_dedup_hit(row, &record)
        } else {
            self.metrics.inc_dedup_stale();
            Err(write_transient(&record, CONFLICT_UNREADABLE_MESSAGE))
        }
    }

    /// Resolve a hit on a stored dedup identity into absorb or conflict.
    ///
    /// The stored row is mapped into a [`UsageRecord`] and compared with
    /// [`UsageRecord::caller_supplied_eq`], the SDK's one definition of the
    /// comparison, so `origin` and `accepted_at` are ignored and the quantity
    /// compares digit for digit. Equal is absorbed and answers with the stored
    /// entry; different is
    /// [`UsageCollectorPluginError::IdempotencyConflict`] carrying it. A stored
    /// row that cannot be mapped (corrupt metadata, say) is `Internal`.
    ///
    /// **The `id`s are not compared, because they cannot differ.** Every caller
    /// selected this row by the very identity it is being resolved against:
    /// the single path reads `WHERE id = $1` with the record's own
    /// [`entry_identity`], `read_conflict_records` keys its map on `row.id`,
    /// and the in-batch arm passes the row the statement inserted under that
    /// same identity. An equality check here would be a tautology dressed as
    /// defence in depth, and the earlier one was: it was written when the
    /// read-back selected on the stored 6-tuple and could therefore return a
    /// row with some other `id`.
    // @cpt-algo:cpt-cf-uc-plugin-algo-duplicate-identity-resolution:p1
    // @cpt-dod:cpt-cf-uc-plugin-dod-duplicate-resolution:p1
    fn resolve_dedup_hit(
        &self,
        row: UsageRecordRow,
        record: &UsageRecord,
    ) -> Result<UsageRecord, UsageCollectorPluginError> {
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
    /// **A PRIMARY KEY collision is the caller's to handle, not this function's.**
    /// The arbiter covers the dedup UNIQUE only, so a write that reaches the
    /// primary-key index and meets a row there surfaces as a `23505` rather
    /// than as an absent `ins` row - a concurrent writer of one of these
    /// identities is the case that is sized for, and [`entry_identity`] sets
    /// out one that needs no concurrency. Unlike the single-row path there is
    /// nothing to resolve in place - a failed statement reports no verdict at
    /// all, not even for the rows it would have admitted - so
    /// `create_batch_inner` lifts it to a `Transient`
    /// ([`PgRecordStore::record_insert_error`]) and the whole batch re-runs on a
    /// fresh transaction.
    async fn run_guarded_batch_write(
        tx: &mut sqlx::Transaction<'_, Postgres>,
        reps: &[&UsageRecord],
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
            .bind(&cols.gts_type_ids)
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

        // `input.id` names the representative each verdict belongs to. The walk
        // is over `reps` rather than over the returned rows, so a key only ever
        // enters the map built from a representative this batch actually holds;
        // a representative with no row leaves the map without an entry, which
        // `resolve_batch`'s defensive arm answers as retryable rather than as a
        // silent success.
        let mut by_input_id: HashMap<Uuid, &PgRow> = HashMap::with_capacity(rows.len());
        for row in &rows {
            by_input_id.insert(row.try_get("input_id")?, row);
        }
        let mut out: HashMap<Uuid, Admission> = HashMap::with_capacity(reps.len());
        for rep in reps {
            let id = entry_identity(rep);
            if let Some(row) = by_input_id.get(&id) {
                out.insert(id, admission_of(row)?);
            }
        }
        Ok(out)
    }

    /// For the admitted, lost identities, read the existing `usage_records` row
    /// by `id` — the batch analogue of the single path's conflict branch, and
    /// the same key `DESIGN.md` §3.6 prescribes there: the five columns a
    /// withdrawn record shares with its withdrawal cannot tell the pair apart,
    /// and the `id` can, because the entry kind is one of the six inputs it is
    /// derived over ([`entry_identity`]).
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
        lost: &[&UsageRecord],
    ) -> Result<HashMap<Uuid, ConflictRead>, UsageCollectorPluginError> {
        let mut out: HashMap<Uuid, ConflictRead> = HashMap::new();
        if lost.is_empty() {
            return Ok(out);
        }

        let ids: Vec<Uuid> = lost.iter().copied().map(entry_identity).collect();
        let window_ends: Vec<OffsetDateTime> = lost.iter().map(|r| r.window_end).collect();
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

        // Every lost identity resolves to Stored (its conflicting row was read)
        // or Stale (it could not be — [`CONFLICT_UNREADABLE_MESSAGE`]).
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
    /// record, recording the per-row counters exactly as the single path does.
    ///
    /// `admissions` is what the guarded statement said about each distinct key
    /// — refused, won with its stored row, or lost — so there is no separate
    /// `won` set to disagree with it. An earlier shape passed a won set beside
    /// the inserted rows, which made two states representable that the code
    /// cannot produce (a won key with no inserted row) and so two `Internal`
    /// arms that only a hand-built map could reach.
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
        records: &[UsageRecord],
        plan: &BatchPlan<'_>,
        admissions: &HashMap<Uuid, Admission>,
        conflict: &HashMap<Uuid, ConflictRead>,
    ) -> Vec<Result<UsageRecord, UsageCollectorPluginError>> {
        let mut results = Vec::with_capacity(records.len());
        for (i, record) in records.iter().enumerate() {
            let key = entry_identity(record);
            let outcome = match admissions.get(&key) {
                // Refused by the statement's own acceptance guard. Nothing was
                // written for this key and no ledger row was consulted for it,
                // whether or not its identity is already stored.
                Some(Admission::Stale) => {
                    self.metrics.inc_stale_acceptance_rejection();
                    Err(write_transient(record, STALE_ACCEPTANCE_MESSAGE))
                }
                // We won this slot and this input row is its first occurrence:
                // the fresh insert.
                Some(Admission::Won(row)) if plan.first_index.get(&key) == Some(&i) => {
                    if record.invalidation.is_some() {
                        self.metrics.inc_invalidation();
                    }
                    record_row_to_model((**row).clone())
                }
                // We won the slot, but an earlier input row is its winner — so
                // this is an in-batch duplicate, resolved against the row we
                // just wrote exactly as the single path resolves a same-key hit.
                Some(Admission::Won(row)) => self.resolve_dedup_hit((**row).clone(), record),
                // Lost the slot, or — defensively — the statement returned no
                // verdict for this key at all, which `run_guarded_batch_write`
                // cannot produce. Both resolve against the conflict read, and
                // the second finds nothing there and answers retryable rather
                // than silently succeeding.
                Some(Admission::Lost) | None => match conflict.get(&key) {
                    Some(ConflictRead::Stored(row)) => {
                        // Clone the inner row directly; `*row.clone()` would
                        // round-trip through a throwaway `Box` allocation. The
                        // clone itself is required — a not-won key may be
                        // resolved by several input rows against the borrowed map.
                        self.resolve_dedup_hit((**row).clone(), record)
                    }
                    Some(ConflictRead::Stale) => {
                        self.metrics.inc_dedup_stale();
                        Err(write_transient(record, CONFLICT_UNREADABLE_MESSAGE))
                    }
                    None => Err(write_transient(
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
    /// This path carries **no** SAVEPOINT, where the single-row path does. The
    /// statement can fail here too on a unique index its `ON CONFLICT` arbiter
    /// does not cover, but a failed statement reports no verdicts at all, so
    /// there is nothing to resolve in place: the transaction is rolled back, the
    /// failure is lifted to a `Transient`, and the whole batch re-runs on a
    /// fresh transaction (see [`Self::run_guarded_batch_write`]).
    // @cpt-algo:cpt-cf-uc-plugin-algo-batch-transient-retry:p1
    async fn create_batch_inner(
        &self,
        records: &[UsageRecord],
    ) -> Result<Vec<Result<UsageRecord, UsageCollectorPluginError>>, UsageCollectorPluginError>
    {
        let plan = plan_batch(records);

        let mut conn = self.timed_acquire().await?;

        // One partition key per representative, aligned to `plan.reps`, resolved
        // in autocommit before the write transaction opens. A type already seen
        // costs no round trip.
        let mut type_keys: Vec<i32> = Vec::with_capacity(plan.reps.len());
        for rep in &plan.reps {
            let key = self
                .type_keys
                .resolve(&mut conn, rep.gts_type_id.as_str())
                .await
                .map_err(|e| self.record_backend_error(&e))?;
            type_keys.push(key);
        }

        // Same transaction shape as the single-row path, for the same reason
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
        let lost: Vec<&UsageRecord> = plan
            .reps
            .iter()
            .copied()
            .filter(|r| matches!(admissions.get(&entry_identity(r)), Some(Admission::Lost)))
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
    /// Seven parameters over clippy's default of seven: each is a distinct
    /// input the six-step protocol names (the connection, the built
    /// statement and its binds, the subscription for the mark check, the two
    /// optional bounds and the limit), this is a private helper with the one
    /// call site [`RecordStore::feed_page`] already builds, and bundling them
    /// into a struct for that alone would be a layer of indirection over
    /// values that are already named by the protocol steps' own docs.
    #[allow(clippy::too_many_arguments)]
    // @cpt-algo:cpt-cf-uc-plugin-algo-next-position-selection:p1
    // @cpt-dod:cpt-cf-uc-plugin-dod-snapshot-and-bounded-replay:p1
    // @cpt-dod:cpt-cf-uc-plugin-dod-live-head-position:p1
    async fn feed_page_in_transaction(
        &self,
        conn: &mut PgConnection,
        sql: &str,
        binds: &[SqlBind],
        types: &[&str],
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

        // Step 4. `persistent(false)` so this call never *inserts* a plan
        // into the connection's statement cache — `sqlx` 0.9.0's
        // `get_or_prepare` still consults the cache unconditionally either
        // way, but nothing else in this crate prepares this exact SQL text
        // persistently, so there is never a stale entry there to find. The
        // statement is parsed and planned fresh after step 2. TimescaleDB
        // excludes chunks at plan time against the catalog it then sees; a
        // plan cached before a chunk existed could silently skip that
        // chunk's rows, which is the failure §3.6's "Why" paragraph rules
        // out.
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
        // below: the SDK model carries no `xact_id`, and `xact_id` lives on
        // this `FeedRecordRow` alone (its own doc says why) rather than on
        // the `UsageRecordRow` that function consumes, so the last row's
        // position has to be read off the raw `FeedRecordRow` here.
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
        // "Reached `until`" is not the same test as "short": a page can fill
        // to exactly `limit` rows and have its last row's own position equal
        // `until` — the range's last entry lands exactly on the limit
        // boundary. Testing `row_count < limit_as_usize` alone would miss
        // that case and fall through to the filled-to-limit arm, minting a
        // continuation for a replay that has already closed.
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
    /// the authority (`RecordStore::feed_page`, step 6) — both against the
    /// very same pooled connection, before and after its `COMMIT`, so one
    /// function serves both call sites rather than two statements of one SQL
    /// string under two names.
    // @cpt-flow:cpt-cf-uc-plugin-flow-resume-after-retention:p2
    // @cpt-algo:cpt-cf-uc-plugin-algo-retention-mark-check:p1
    // @cpt-dod:cpt-cf-uc-plugin-dod-retention-refusal:p1
    async fn mark_stands_above(
        &self,
        conn: &mut PgConnection,
        types: &[&str],
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
    entries: Vec<UsageRecord>,
    /// The continuation, as a position pair. `None` only when `marked` is
    /// `true` (mirroring `entries`) or when a bounded replay reached its
    /// `until`.
    next: Option<(u64, Uuid)>,
    /// Whether step 3's fast-path check already found a mark above the
    /// position. `RecordStore::feed_page` short-circuits step 6 on this: a
    /// mark the fast path already found is certain, so the authoritative
    /// re-check is not needed to confirm it.
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
/// rows. Keeping them in one struct built by one function keeps the column
/// list, the `UNNEST` list and the bind order readable side by side instead of
/// spread across that many locals in the middle of the query. The count is not
/// written out here: it moves whenever the ledger grows or loses a written
/// column, and a stale number reads as an assertion about this struct.
struct InsertColumns {
    ids: Vec<Uuid>,
    tenants: Vec<Uuid>,
    gts_type_ids: Vec<String>,
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
    /// `reps`, so it is the same length. A shorter one would be a caller
    /// invariant break, and panicking beats silently writing a wrong partition
    /// key — which would put the row in the wrong chunk and out of reach of the
    /// unique constraints that carry it.
    fn build(reps: &[&UsageRecord], type_keys: &[i32]) -> Self {
        assert_eq!(
            reps.len(),
            type_keys.len(),
            "one partition key must be resolved per batch representative"
        );
        let mut cols = Self {
            ids: Vec::with_capacity(reps.len()),
            tenants: Vec::with_capacity(reps.len()),
            gts_type_ids: Vec::with_capacity(reps.len()),
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
        for r in reps {
            let (invalidates, reason_code) = invalidation_to_row(r.invalidation.as_ref());
            cols.ids.push(r.id);
            cols.tenants.push(r.tenant_id);
            cols.gts_type_ids.push(r.gts_type_id.as_str().to_owned());
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
/// One helper rather than two call sites, so the two write paths cannot drift
/// in whether they issue [`FORCE_SYNCHRONOUS_COMMIT_SQL`] or in what they issue
/// it before. Both paths resolve their type keys in autocommit first, so the
/// transaction this opens carries the guarded statement and nothing ahead of it
/// but the `SET LOCAL`.
///
/// Errors come back as the raw `sqlx::Error`: the caller owns the mapping, and
/// on this path a failed `SET` leaves a transaction to roll back.
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
/// queued and sent when the connection is next used. Every caller here rolls
/// back precisely because it is about to return the connection to the pool
/// after a failure, so "now" is the property that matters. A
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
/// binds the `id` first, before these, for that reason. **This is the only
/// seeded caller on a production path** — `aggregate_tests.rs` and
/// `translate_tests.rs` seed as fixtures, exercising the offset itself. Both
/// collection paths seed at 1: their leading
/// value is the meter, and it goes through the same [`SqlCtx`] as everything
/// else (`push_meter_and_range_clauses`), so there is no bind outside the
/// counter for them to seed past. Only the ordered bind values are returned,
/// not the [`SqlCtx`]: the statement is finished, so there is nothing left for a
/// caller to legitimately push.
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

/// The key both write paths decide identity on: the entry's own `id`.
///
/// **Where it comes from, and what this plugin does not do with it.** The `id`
/// arrives on the wire, derived by the Ingestion Gateway as a `UUIDv5` over the
/// six dedup-identity inputs — tenant, GTS type, idempotency key, the two
/// covered-period bounds and the entry kind
/// (`cpt-cf-usage-collector-adr-record-identity-derivation`). **This plugin
/// neither re-derives nor validates it**: `derive_usage_record_id` is the SDK's
/// and is called above the SPI, never here. Everything below therefore rests on
/// the gateway having derived it faithfully, and that is a trust boundary worth
/// naming rather than a fact worth assuming silently.
///
/// What rests on it, each chosen rather than inherited:
///
/// * **The in-batch comparison.** [`plan_batch`] collapses a batch to one
///   representative per `id` and [`resolve_batch`](PgRecordStore::resolve_batch)
///   resolves every later row against it. Keying on the `id` rather than on the
///   6-tuple is what makes "two entries of one batch are the same entry" and
///   "two entries of one batch take one ledger row" the *same* statement: the
///   guarded statement joins its inserted rows back to its input on `id`
///   (`LEFT JOIN ins USING (id)`), so a batch carrying two rows that agree on
///   `id` and differ in their six inputs has no coherent reading at all. Under
///   this key it does not arise — they are one identity by definition, and the
///   second is resolved against the first.
/// * **The conflict read-back.** Both paths read the row that took a lost slot
///   by `id`, which is what `DESIGN.md` §3.6 prescribes: *"The read-back keys
///   on `id`, never on `(tenant_id, gts_type_id, idempotency_key,
///   window_start, window_end)`"* — the five alone are ambiguous once a record
///   is withdrawn, because the pair shares them.
///
/// What does **not** rest on it is the ledger's own uniqueness. The
/// `ON CONFLICT` arbiter is [`DEDUP_CONFLICT_TARGET`], the six columns plus the
/// partition key, so the database decides what collides from the row's stored
/// values and not from anything a caller asserted about them.
///
/// **What a mis-derived `id` costs, because this is the case the trust above
/// accepts the risk of.** What happens depends on what the ledger already
/// holds, and the outcomes below are the ones worth a reader's attention
/// rather than a closed account of every route:
///
/// * **Its six inputs are already stored.** It collides on the arbiter, which
///   reads those six off the row, and `DO NOTHING` then skips the insert
///   outright — so no other unique index is reached, the PRIMARY KEY included
///   ([`is_ledger_pk_violation`] states that suppression rule). The read-back
///   then looks for the winner by the mis-derived `id`, and **what it finds
///   decides the answer**. The mis-derivation guarantees no row carries the
///   `id` this entry *ought* to have; it guarantees nothing about the one it
///   does carry:
///   * **No row carries it**, which is the ordinary case. The read finds
///     nothing and answers a `Transient` reading
///     [`CONFLICT_UNREADABLE_MESSAGE`], counted on
///     `uc_timescaledb_dedup_stale_total`. That **does not self-heal**: every
///     retry re-derives the same `id` and lands in the same arm. It is also
///     indistinguishable, at the call site and in the log line, from the other
///     things that reach that arm.
///   * **Some other entry carries it**, under the same covered-period end.
///     The read returns that **foreign row**, and
///     [`PgRecordStore::resolve_dedup_hit`] compares caller-supplied fields
///     against an entry that is not this submission's — so the caller receives
///     an `IdempotencyConflict` carrying another identity's stored entry as
///     `existing`, possibly another tenant's, since both read-backs bind the
///     `id` and the period and nothing else ([`SINGLE_CONFLICT_READ_SQL`],
///     [`BATCH_CONFLICT_READ_SQL`]). Nothing on either path detects it.
/// * **Its six inputs are novel.** Nothing collides *on the arbiter*, so the
///   insert proceeds — and only then is every other unique index checked. Two
///   ends to that:
///   * Should the `id` collide with a stored row's
///     `(id, window_end, type_key)`, the raise is on the PRIMARY KEY, which the
///     arbiter does not cover and `DO NOTHING` therefore does not suppress.
///     The single path rolls back to its savepoint and resolves against that
///     row — which is a foreign row, resolved as the sub-case above resolves
///     one. The batch lifts it to a `Transient` and re-runs, and **that
///     re-run does not clear it**: the arbiter pre-check still finds nothing,
///     the insert still meets the same primary-key entry, and
///     [`MAX_BATCH_ATTEMPTS`] is spent before the error reaches the host
///     ([`PgRecordStore::record_insert_error`]). Here the ledger does at least
///     refuse the row.
///   * Otherwise it is **stored**, and nothing catches it. The row carries an
///     `id` no faithful derivation of its own columns produces, which makes it
///     unreachable by the identity it ought to have: every later faithful
///     submission of that entry collides on the arbiter and then cannot find
///     it, so that entry takes the never-clearing `Transient` above from then
///     on. One mis-derivation is permanent for one identity, not transient.
///
/// This is the accepted cost of not re-deriving, and it is the shape a faithful
/// gateway never produces. Re-deriving here to close it would put a second
/// derivation beside the SDK's, which
/// `cpt-cf-usage-collector-adr-record-identity-derivation` places above the SPI.
/// Splitting the counter is the metric inventory's to do, not this path's, so
/// nothing here adds a counter or a label for it.
///
/// Called rather than written out at each site so that every identity decision
/// on this path is visibly one, and so this doc is what a reader meets first.
// @cpt-state:cpt-cf-uc-plugin-state-dedup-identity:p2
const fn entry_identity(record: &UsageRecord) -> Uuid {
    record.id
}

/// What the guarded statement said about one input row: the two flags it
/// carries out, decoded into the three outcomes `DESIGN.md` §3.6 names.
///
/// A sum type rather than the flags themselves, because only three of their
/// four combinations are reachable: `WHERE admitted` is what feeds the insert,
/// so a refused row cannot have won. Carrying the flags would leave the fourth
/// representable and every reader to rule it out again.
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
/// **`admitted` is read first and short-circuits**, which is the ordering rule
/// §3.6 states as *"a row not admitted is `Transient` … even when its identity
/// exists"*. The statement already enforces it — a refused row is never
/// offered to `ON CONFLICT` — and reading it in this order means no decode path
/// can reintroduce the other order.
///
/// The record columns are decoded only on the won arm. Every one of them is
/// `NULL` on the other two, `ins` having contributed no row to the left join,
/// so [`UsageRecordRow`] could not be built from them at all.
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
/// should be added: an enumeration of this arm's population has been written
/// twice and been wrong both times. Those worth a reader's attention, in
/// descending order of how well a retry serves them:
///
/// * The **retention race** the arm is named for: the conflicting row's chunk
///   was dropped between the conflicting insert and this read. Near-impossible
///   against the retention boundary, and genuinely retryable — the unique entry
///   went with the chunk, so a retry wins the freed slot as a fresh insert.
/// * A deployer-set **`default_transaction_isolation = repeatable read`**,
///   under which the read-back keeps the write transaction's original snapshot
///   and cannot see a winner that committed after it. The savepoint arm of
///   [`PgRecordStore::create_inner`] is where that is reasoned about. A retry
///   does clear it, because a retry opens a fresh transaction and so takes a
///   fresh snapshot — but it recurs on every contended write for as long as the
///   setting stands, so what an operator sees is a rate rather than one blip.
/// * A **submission whose `id` does not match its own six inputs**, which,
///   *when those six are already stored*, collides on an arbiter that reads
///   them off the row and is then looked for under an `id` that — in the
///   ordinary case, though not in every case — no row carries
///   ([`entry_identity`] sets out the rest). It does **not** clear on a retry,
///   because a retry re-derives the same `id`.
///
/// Nothing here can tell them apart, and separating them at the metric is the
/// metric inventory's work rather than this path's.
const CONFLICT_UNREADABLE_MESSAGE: &str =
    "conflicting record could not be read back during dedup resolution; retry";

/// Log a retryable write-path transient at `warn` with the record's identifiers,
/// then return the matching [`UsageCollectorPluginError::Transient`]. The
/// degraded path must surface at `warn` so an operator can see it — and not
/// every one of them self-heals on a retry, which is one reason it must
/// ([`CONFLICT_UNREADABLE_MESSAGE`]). This helper only logs and builds the
/// error: any counter is the caller's, `inc_dedup_stale` on the
/// unreadable-conflict sites, `inc_stale_acceptance_rejection` on the guard's,
/// and none on the defensive not-found arm, which is unreachable by
/// construction. Stated as a kind rather than a count because the call sites
/// move.
fn write_transient(record: &UsageRecord, msg: &'static str) -> UsageCollectorPluginError {
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
/// [`entry_identity`], **sorted** by it so concurrent batches take the
/// 6-tuple-UNIQUE speculative tuple locks in one global order (deadlock-free).
/// Since the per-scope counter was retired the sort is the whole of this
/// plugin's deadlock-freedom argument rather than one half of it. The partition
/// key assignment that two batches can also meet on is not a second half: it
/// runs in autocommit before either write transaction opens, one row per
/// statement, and is held for the statement rather than to a commit
/// ([`crate::infra::storage::pool`]), so it cannot be an edge of a cycle.
///
/// **Any total order over the key buys deadlock-freedom**, and this one is the
/// `id`'s own `Ord`, which costs nothing to derive and is the same in every
/// process. It is not an order over anything a reader should attach meaning to:
/// a `UUIDv5` orders by its digest bytes, so a record and its withdrawal fall
/// either way round. Nothing observable depends on that — a pair written in one
/// batch shares one transaction, so the `xact_id` feed order ties across it and
/// `id` decides it (this plugin's DESIGN §3.6, which states that feed order
/// within a batch "falls back to `id`"). The gear's DESIGN §3.1 Feed order
/// invariant — "**Correction order**: an invalidation follows its target" — is
/// realized on `xact_id` and not here: the gateway admits an invalidation only
/// once its target has converged, so the withdrawal commits in a later
/// transaction and takes a greater `xact_id`.
///
/// `first_index` maps each identity to the input index of its first occurrence,
/// the only row that can win the slot.
/// Later same-identity rows resolve against the winner's stored row, exactly as
/// the single-row path resolves a same-identity hit.
struct BatchPlan<'a> {
    reps: Vec<&'a UsageRecord>,
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
fn plan_batch(records: &[UsageRecord]) -> BatchPlan<'_> {
    let mut first_index: HashMap<Uuid, usize> = HashMap::new();
    let mut reps: Vec<&UsageRecord> = Vec::new();
    for (i, record) in records.iter().enumerate() {
        if let std::collections::hash_map::Entry::Vacant(slot) =
            first_index.entry(entry_identity(record))
        {
            slot.insert(i);
            reps.push(record);
        }
    }
    reps.sort_by_key(|r| entry_identity(r));
    BatchPlan { reps, first_index }
}

/// Total `create_batch` attempts: one initial try plus two retries. A bounded
/// in-process retry so a transient backend error self-heals transparently
/// instead of bubbling an `Err(Transient)` to the host (see `with_retry` and
/// `is_retryable_batch_error`, both private and so named in plain backticks;
/// the second is where the causes that reach this loop are set against the
/// code, and it points at the normative list in this plugin's DESIGN §3.6).
const MAX_BATCH_ATTEMPTS: u32 = 3;

/// Deterministic pre-jitter backoff base for the `attempt`-th retry (1-based).
/// A short exponential — 5 ms, 10 ms, … — because a deadlock victim can retry
/// almost immediately: the transaction that survived the deadlock has already
/// committed or aborted by the time Postgres aborts the victim, so the
/// contended dedup locks are free. The shift is
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
/// timeouts (`55P03`, which a batch hits when it waits out `lock_timeout` on a
/// contended lock — the locks ingest can meet are
/// [`crate::infra::storage::pool`]'s subject, and the partition-key assignment
/// among them, which `create_batch_inner` runs per representative in autocommit
/// and maps through [`PgRecordStore::record_backend_error`] into this same
/// bucket) and connectivity
/// faults (a backend unreachable or refusing work, and a pool that cannot
/// supply a usable connection) collapse to the same bucket inside the storage
/// helpers. A concurrent writer of one of this batch's own dedup identities
/// colliding on the ledger's PRIMARY KEY reaches it by a different route: the
/// arbiter does not cover that index, so `classify_db` leaves the `23505` in
/// `Other` and `record_insert_error` is what lifts it, so the re-run can
/// resolve against the winner's committed row (this plugin's DESIGN §3.6) -
/// can, rather than will: that function names a route a re-run does not end.
/// Every one of these is **safe** to re-run, this batch being idempotent, which
/// is a weaker claim than that a re-run succeeds and is the only one this
/// predicate needs. `Internal`,
/// `IdempotencyConflict` and the other typed domain outcomes are non-retryable
/// and returned unchanged. Per-row
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
/// attempt leaves no half-written batch behind — and, because the row's
/// `xact_id` comes from the transaction that wrote it, no attempt can leave a
/// feed position a later one then contradicts. There is zero happy-path cost —
/// on success the loop runs the
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

/// Outcome of reading the existing `usage_records` row for a lost identity.
enum ConflictRead {
    /// The conflicting row exists — resolve absorb vs conflict against it.
    Stored(Box<UsageRecordRow>),
    /// It could not be read. More than one thing does that, they answer alike,
    /// and not all of them clear on a retry
    /// ([`CONFLICT_UNREADABLE_MESSAGE`] is where they are set out). The name is
    /// the oldest of them and is kept because the arm is one arm.
    Stale,
}

/// One assembled aggregate statement: the SQL, the binds in placeholder order,
/// and **the dimension count the SELECT list was actually built from**.
///
/// `dim_count` is carried rather than recomputed by the caller. It is the count
/// [`build_aggregate_sql`] used to number the `GROUP BY` ordinals and to place
/// the fold at the end of the SELECT list, and it is what the decoder must read
/// the same number of key columns with. Derived a second time at the call site
/// — from `group_by.len()`, which is equal today — the two could drift with
/// nothing to notice: no unit test executes a statement, so a decoder reading
/// the wrong number of columns is invisible until a live query.
///
/// Returning it makes that **one derivation with one restatement rather than
/// two**, not a construction that cannot be wrong, and
/// `the_builder_reports_the_dimension_count_its_select_list_was_built_from` is
/// what holds the restatement. The distinction matters here more than it
/// usually would: `dim_count` is the one output of [`build_aggregate_sql`] that
/// leaves no trace in the SQL string. `sql` is pinned by hand-transcribed
/// oracles and `binds` by value in placeholder order, but a count that merely
/// *describes* the statement without appearing in it is beyond the reach of any
/// text oracle — which is why a mutation setting it to a constant left every
/// SQL assertion in the crate green.
///
/// Contrast `dimension_presence_guard`, which really is by construction: the
/// guard string literally contains the select expression, so making the two
/// disagree means making the SQL wrong, and the SQL is pinned.
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
/// WHERE r.gts_type_id = $1 AND r.type_key = (…) AND r.window_end >= $2 AND r.window_end < $3
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
/// **No slot of `query` beyond `filter` is read.**
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
) -> Result<AggregateStatement, String> {
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

    Ok(AggregateStatement {
        sql: format!(
            "SELECT {select_list} FROM {} WHERE {}{group_by_sql}{}",
            // Called, never spelled: with a literal here the shared constant
            // would be decorative, and an alias change would red a test that
            // then gets "fixed" by editing the literal back.
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
    /// A `55P03` is reachable here, and the wait this path is shaped around is
    /// a concurrent same-key insert's speculative tuple, waited out to
    /// `lock_timeout`. That one needs a same-key write in flight, so it is a
    /// rarity rather than an ordinary outcome on a merely busy scope: on the
    /// dedup tuple a write contends only with another writer of the very same
    /// entry. It is returned unretried, because a `Transient` lifts to
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
    // @cpt-flow:cpt-cf-uc-plugin-flow-persist-single-entry:p1
    // @cpt-flow:cpt-cf-uc-plugin-flow-persist-withdrawal:p1
    // @cpt-state:cpt-cf-uc-plugin-state-entry-withdrawal:p2
    // @cpt-dod:cpt-cf-uc-plugin-dod-withdrawal-as-appended-entry:p1
    // @cpt-dod:cpt-cf-uc-plugin-dod-no-mutation-of-target:p1
    // @cpt-dod:cpt-cf-uc-plugin-dod-linearizable-dedup-level:p1
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
    // @cpt-flow:cpt-cf-uc-plugin-flow-persist-entry-batch:p1
    // @cpt-dod:cpt-cf-uc-plugin-dod-record-and-withdrawal-coexist:p1
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
        // Wrap the whole call in a bounded retry: on an outer `Transient`
        // re-run the operation up to `MAX_BATCH_ATTEMPTS` times.
        // `is_retryable_batch_error` is where the causes that reach this are
        // listed against the code, and it points at the normative list in this
        // plugin's DESIGN section 3.6, so this comment carries no third copy to
        // fall behind either of them.
        // Each attempt acquires a
        // fresh connection and opens a fresh transaction on it
        // (`create_batch_inner` does both), so a rolled-back attempt leaves no
        // state behind at all. Re-running is safe: that
        // transaction is atomic and the identities make it idempotent, so a
        // re-run cannot double-write. What it resolves *to* is not enumerated
        // here — re-claiming the slots and resolving against a committed
        // survivor are the ordinary endings, and `record_insert_error` names
        // one that a re-run does not end at all. `Ok(vec)` is never retried —
        // per-row
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
                // self-healed transient can be told apart from a returned one,
                // which moves this counter not at all (most
                // move the backend-error counter instead).
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
        // `UNIQUE` there must include every partition column, so only the
        // composite PK `(id, window_end, type_key)` is enforced.
        // `fetch_optional` therefore returns the first matching row.
        //
        // `id` is a `UUIDv5` over the 6-tuple dedup identity
        // `(tenant_id, gts_type_id, idempotency_key, window_start, window_end,
        // entry_type)`
        // (`cpt-cf-usage-collector-adr-record-identity-derivation`), which is
        // exactly this plugin's dedup identity — the same six inputs
        // `usage_records_dedup_uniq` is built over — so each stored row carries
        // a distinct `id` and `WHERE id = $1` matches at most one row. A
        // withdrawn record and the withdrawal that named it are two `id`s, not
        // one, because they differ in that sixth input.
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

    /// One feed page, run as the six-step protocol this plugin's
    /// `docs/DESIGN.md` §3.6 `cpt-cf-uc-plugin-seq-feed-page` sets out.
    ///
    /// Steps 1-5 run on one connection, inside a real [`sqlx::Transaction`]
    /// opened with [`sqlx::Connection::begin_with`] rather than a raw `BEGIN`
    /// statement sent over the bare connection. That is not cosmetic:
    /// `sqlx` tracks an open `Transaction` and, if this `async fn`'s future
    /// is ever dropped before [`sqlx::Transaction::commit`] runs — a
    /// host-side timeout or a client disconnect, neither of which this
    /// plugin controls or can observe — the drop queues a `ROLLBACK` sent
    /// the next time the connection is used ([`rollback`]'s own doc
    /// describes the same lazy-queued mechanism for the explicit case).
    /// Without that tracking the connection would return to the pool still
    /// holding an open `REPEATABLE READ READ ONLY` transaction with no
    /// queued cleanup at all, and the next caller to acquire it would run
    /// inside an abandoned snapshot. The other multi-statement write
    /// transactions in this file ([`Self::create_inner`] and
    /// `create_batch_inner`, both through `begin_durable_write`) already use
    /// this idiom, and so does the retention sweep's drop transaction in
    /// `retention_sweep.rs`; the feed page now does too.
    ///
    /// The transaction borrows `conn` only until [`sqlx::Transaction::commit`]
    /// consumes it, so step 6 — the authoritative autocommit mark re-check —
    /// still runs on the very same pooled connection afterward, and both it
    /// and step 3's fast-path check reach one [`Self::mark_stands_above`]
    /// (ruling D8: one function, one connection, never two).
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
        subscription: &[MeterTypeId],
        scope: &ast::Expr,
        after: Option<(u64, Uuid)>,
        until: Option<(u64, Uuid)>,
        limit: u64,
    ) -> Result<FeedPageRows, UsageCollectorPluginError> {
        // Translated before a connection is acquired, so a scope that cannot
        // be rendered never opens a transaction — the discipline `get`
        // already keeps.
        let (sql, binds) = build_feed_page_sql(after, until, scope, limit)
            .map_err(UsageCollectorPluginError::internal)?;
        let types: Vec<&str> = subscription.iter().map(MeterTypeId::as_str).collect();

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

        // Step 5, on the success path only. `?` below propagates a
        // steps-2-4 error before `tx` is committed; dropping an uncommitted
        // `Transaction` queues its `ROLLBACK` rather than committing it
        // (`sqlx::Transaction`'s own contract), so an error here is rolled
        // back rather than left for an unconditional `COMMIT` to paper over
        // — strictly safer than this method's previous raw-SQL shape, which
        // sent `COMMIT` on both the success and the error path alike.
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

        // `_timer` records `uc_timescaledb_query_duration_seconds` on drop
        // (success and error alike).
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
    ///   ([`usage_collector_sdk::AggregationBucket::value`]). The two zeroes are
    ///   not reached the same way, and the difference is the part worth stating:
    ///   `COUNT` is `SELECT COUNT(*)`'s own answer, while `SUM` over zero rows
    ///   is `NULL` in `PostgreSQL` and is made `0` by a `COALESCE` in the
    ///   statement — [`fold_select_expr`] on the scan path,
    ///   [`build_rollup_aggregate_sql`] on the rollup one. So neither is a
    ///   special case in this method, but only one of them is the backend's
    ///   unaided answer.
    ///
    /// Both are held rather than asserted, by one test each against a lazy pool
    /// at a dead DSN — where reaching the pool answers `Transient` and stopping
    /// before it answers `Internal`.
    /// `a_fold_that_cannot_be_built_never_reaches_the_pool` gives the fold a
    /// filter naming a field the allowlist refuses and requires the `Internal`,
    /// which is the ordering claim: a statement that cannot be rendered must
    /// not have acquired a connection first.
    /// `the_ungrouped_fold_still_reaches_the_pool` gives it a renderable one and
    /// requires the `Transient`, so the ungrouped fold cannot answer without
    /// asking. `Transient` alone would not have pinned the order — it is
    /// consistent with acquiring first — which is why the pair is needed and
    /// not either half.
    ///
    /// The dimension columns read positionally as `Option<String>` and the fold
    /// at index `k` as `Option<BigDecimal>` — arbitrary precision, so a wide
    /// `SUM` cannot overflow on decode.
    ///
    /// **`LATEST` materializes before it picks, and the peak is O(largest
    /// group).** Its expression is `(ARRAY_AGG(r.quantity ORDER BY …))[1]`, so
    /// `PostgreSQL` builds a group's values into an array before taking the
    /// head. Measured on `timescale/timescaledb:2.29.2-pg18` (`PostgreSQL`
    /// 18.6) at the **image's tuned** `work_mem` — the image runs
    /// `001_timescaledb_tune.sh` at initdb, so a fresh container reports
    /// `work_mem = 7837kB` and `shared_buffers = 1959MB` on the measuring host
    /// rather than `PostgreSQL`'s compiled 4 MB default. No `SET` was issued;
    /// the tuned values are what these numbers ran under.
    ///
    /// **Read the deltas, not the absolutes.** Every figure below is from one
    /// host and one fixture table whose shape is not published here, and the
    /// absolutes move with both — an independent replication of the same four
    /// queries reported peak RSS an order of magnitude lower throughout,
    /// because peak RSS counts the shared buffers a backend has touched. What
    /// reproduced exactly is the differences between the four, and the
    /// differences are the whole claim.
    ///
    /// * **The planner does not choose between a hash and a sorted plan here;
    ///   there is nothing to choose.** An aggregate carrying its own `ORDER BY`
    ///   takes the grouped node off the hash path entirely: with `enable_sort`
    ///   *and* `enable_incremental_sort` off, the plan is still `Sort →
    ///   GroupAggregate` with the `Sort` reported `Disabled: true` — and a
    ///   disabled node is chosen only when no alternative path exists, while
    ///   `HashAggregate` was never disabled. The same statement with the inner
    ///   `ORDER BY` removed plans as a `HashAggregate` immediately; adding one
    ///   ordered aggregate beside a plain `MAX` takes that query off the hash
    ///   path too; and `COUNT(DISTINCT …)` behaves the same way. So it is a
    ///   property of ordered and distinct aggregation generally, not of this
    ///   expression, this data or this row count.
    /// * So exactly one array is live at a time, and the peak is **O(largest
    ///   group)**. On the worst case for it — 1 000 000 rows in a single group,
    ///   parallelism off — peak backend RSS ran **+34 MB over `MAX(r.quantity)`**
    ///   on the same rows (1 022.7 MB against 988.6 MB here), reproducible to
    ///   ±0.2 MB across runs and to ±0.6 MB against an independent replication
    ///   on another host. That is **about 34 bytes per row in the largest
    ///   group**, and the array does not spill.
    /// * The `Sort` beneath it does materialize the whole selection, but it is
    ///   `work_mem`-bounded and spills rather than growing: `external merge`,
    ///   and the size is scan-sized. At 1 000 000 rows it was ~10 MB in each of
    ///   four workers under the image's default parallelism, and 41 MB as a
    ///   single sort with `max_parallel_workers_per_gather = 0` — the absolute
    ///   is fixture-dependent (an independent replication saw 33 MB), the shape
    ///   is not. **Every candidate formulation needs that same sort**, so it is
    ///   not a cost of this one.
    ///
    /// [`aggregate_limit_clause`] offers no protection, because it bounds
    /// groups and never the rows within one; the only row bound is the
    /// `time_range`, which is a request parameter. A `LATEST` meter read whose
    /// largest group is wide is therefore a server-side allocation sized by
    /// caller input.
    ///
    /// **Both alternatives were measured and both were kept out.** On that same
    /// single-group worst case, `DISTINCT ON` and
    /// `ROW_NUMBER() OVER (PARTITION BY …) = 1` both peaked **~25 MB below**
    /// this form (997.6 MB and 997.4 MB here; 25.8 MB and 26.1 MB below in the
    /// independent replication), and both are O(1) per group rather than
    /// O(largest group) — with execution times inside the run-to-run noise of
    /// the parallel plan (86-111 ms at 100 000 rows, 257-293 ms at 1 000 000,
    /// all three formulations). The 25 MB does not buy the composition it would
    /// cost: neither is a `SELECT`-list expression a caller can drop in beside
    /// `SUM`, and neither can express the ungrouped fold — `DISTINCT ON ()` is
    /// a syntax error and `PARTITION BY` nothing, like the `ORDER BY … LIMIT 1`
    /// rewrite, answers **zero** rows over an empty selection where this method
    /// owes exactly one empty-keyed bucket. Adopting either means a second
    /// statement shape for one fold, with its own empty-selection special case.
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
                let st =
                    build_rollup_aggregate_sql(&gts_type_id, split, fold, query.filter(), group_by)
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
                    &gts_type_id,
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

        // One bucket per row, in the order `PostgreSQL` emitted them. The
        // `map` is what keeps the no-grouping case honest: no branch on
        // `group_by.is_empty()` exists to answer an empty bucket list with.
        //
        // The count comes from the statement that was built, not from a second
        // reading of `group_by` here, so the decoder cannot read a different
        // number of key columns than the SELECT list emits. From this call site
        // that is structural; that the builder reports the count it actually
        // used is the other half, and
        // `the_builder_reports_the_dimension_count_its_select_list_was_built_from`
        // is what holds it.
        //
        // What no unit test can still see is a short circuit placed *after* the
        // fetch, where the branch is a no-op on an empty row set. One placed
        // where it would actually be written — above the acquire, to skip the
        // query — is covered by `the_ungrouped_fold_still_reaches_the_pool`.
        let buckets = rows
            .iter()
            .map(|row| aggregate_bucket(row, statement.dim_count))
            .collect::<Result<Vec<_>, _>>()?;

        // `_timer` records `uc_timescaledb_query_duration_seconds` on drop
        // (success and error alike).
        Ok(AggregationResult { buckets })
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "record_store_tests.rs"]
mod record_store_tests;
