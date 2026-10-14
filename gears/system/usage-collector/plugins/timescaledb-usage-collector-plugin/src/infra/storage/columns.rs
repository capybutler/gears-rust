//! The ledger's column vocabulary: the four column sequences every statement
//! in this crate names its columns from, and the two single-column constants
//! the write path prunes and arbitrates on.
//!
//! **One spelling per sequence, read by every path that needs it.**
//! `RECORD_COLUMNS` is read by the point lookup, the keyset page and both
//! conflict read-backs; `FEED_COLUMNS` by the feed page statement alone;
//! `ENTRY_TYPE_ENUM` by the `$filter` translator's `bind_cast`.
//!
//! What is **not** here: any statement these compose into. The guarded insert
//! is [`super::query::write`], the ledger reads are [`super::query::list`],
//! [`super::query::aggregate`] and [`super::query::feed`].

/// Column list for `get`, `list`, and every write path's conflict read-back
/// and `RETURNING`, in
/// [`UsageRecordRow`](crate::infra::storage::entity::UsageRecordRow) field
/// order. A static const (never caller input), so there is no risk of SQL
/// injection.
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
/// [`UsageRecordRow`](crate::infra::storage::entity::UsageRecordRow) carries a
/// [`String`], which `sqlx` holds incompatible with an enum. Hence
/// `entry_type::text AS entry_type`. The alias is written out, though
/// `PostgreSQL` would supply it, because
/// [`ins_record_columns`](super::query::write::ins_record_columns) parses
/// exactly that decoded name back out of this constant to qualify it with
/// `ins`.
pub const RECORD_COLUMNS: &str = "id, tenant_id, gts_type_uuid, type_key, quantity, \
     window_start, window_end, resource_id, resource_type, subject_id, subject_type, \
     idempotency_key, invalidates, reason_code, origin, entry_type::text AS entry_type, \
     accepted_at, metadata";

/// [`RECORD_COLUMNS`] plus `xact_id`, for the one reader that needs the feed
/// order's own key: [`super::query::feed`]'s page statement.
///
/// **The cast is aliased `xact_id_text`, not `xact_id`, and that is
/// load-bearing.** [`super::query::feed`] orders by the bare name `xact_id`
/// (`ORDER BY xact_id, id`, over `usage_records_feed_idx`), and `PostgreSQL`
/// resolves a bare `ORDER BY` name matching both an output and an input column
/// to the *output* one. Aliasing this cast back to `xact_id` would make that
/// `ORDER BY` sort lexicographically over digit strings rather than numerically
/// over the `xid8` column — the same hazard
/// [`crate::infra::storage::retention_sweep::CHUNK_HIGHEST_POSITIONS_SQL`]
/// avoids with its own `xact_id_text` alias.
///
/// `xid8` has no `sqlx` `Decode` implementation, which is why the column is
/// cast to `text` at all.
pub const FEED_COLUMNS: &str = "id, tenant_id, gts_type_uuid, type_key, quantity, \
     window_start, window_end, resource_id, resource_type, subject_id, subject_type, \
     idempotency_key, invalidates, reason_code, origin, entry_type::text AS entry_type, \
     accepted_at, metadata, xact_id::text AS xact_id_text";

/// The columns every insert writes: every ledger column but `xact_id`, which
/// the database stamps.
///
/// **One spelling for every place a write statement names its columns** — each
/// path's `input` CTE select list, the `INSERT`'s own column list, the `SELECT`
/// that feeds it from `input`, and the alias list of the `VALUES` row or
/// `UNNEST` the input is built from
/// ([`guarded_statement`](super::query::write::guarded_statement)). Spelled out
/// separately, a name transposed in any one of them binds a value to the wrong
/// same-typed column, which Postgres accepts without complaint.
///
/// The order is `0001_init.sql`'s declaration order. `metadata` being last is
/// **not** load-bearing and must not become so:
/// [`input_select_list`](super::query::write::input_select_list) names the cast
/// column explicitly rather than appending `::jsonb AS metadata` to the whole
/// string.
pub const INSERT_COLUMNS: &str = "id, tenant_id, gts_type_uuid, type_key, quantity, \
     window_start, window_end, resource_id, resource_type, subject_id, subject_type, \
     idempotency_key, invalidates, reason_code, origin, entry_type, accepted_at, metadata";

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
/// Read by the `$filter` translator's `bind_cast`
/// ([`super::query::translate`]) too.
pub const ENTRY_TYPE_ENUM: &str = "usage_entry_type";

/// Postgres types for [`INSERT_COLUMNS`], **in the same order**, as the write
/// path's `input` source needs them:
/// [`batch_input_source`](super::query::write::batch_input_source) `UNNEST`s
/// the array of each (`text` becomes `text[]`).
///
/// Named for the columns rather than the arrays: [`super::query::translate`]
/// reads the same names to cast a scalar bind.
pub const INSERT_COLUMN_TYPES: [&str; 18] = [
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
pub const DEDUP_CONFLICT_TARGET: &str =
    "tenant_id, gts_type_uuid, idempotency_key, window_start, window_end, entry_type, type_key";

/// The ledger's **time** partition column, named beside the `id` in both
/// conflict read-backs **for pruning and for nothing else**.
///
/// `usage_records` is a hypertable with two range dimensions,
/// `by_range('window_end')` and `by_range('type_key', …)`
/// (`migrations/0001_init.sql`). A predicate constraining neither cannot
/// exclude a chunk at all: `WHERE id = $1` alone probes the
/// `(id, window_end, type_key)` index of *every* chunk the ledger holds. Both
/// read-backs run **inside the write transaction**, holding its speculative
/// tuple locks — the hold time [`crate::infra::storage::pool`]'s `lock_timeout`
/// reasoning is about.
///
/// Constraining this column prunes the **time** dimension to the chunks of the
/// entry's own period — one per `type_key` slice, since the second dimension is
/// left unconstrained. `type_key` is in scope at both read-backs and could be
/// bound too; that is a separate change with its own measurement to take.
///
/// **It is not a retreat to the 6-tuple.** The read-back keys on `id`, and this
/// column discriminates nothing `id` does not: the covered-period end is one of
/// the six inputs `id` is derived over, so two rows agreeing on `id` agree on
/// it. Naming it changes which chunks are scanned, never which row is selected.
pub const PARTITION_PRUNE_COLUMN: &str = "window_end";

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "columns_tests.rs"]
mod columns_tests;
