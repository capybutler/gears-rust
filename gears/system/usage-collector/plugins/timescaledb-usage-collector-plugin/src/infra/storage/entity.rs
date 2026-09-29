//! `sqlx` row struct mirroring the `usage_records` hypertable (see
//! `migrations/0001_init.sql`). It exists because [`super::mapper`] turns a
//! whole ledger row into the validated SDK `UsageRecord`, and looking its
//! columns up by field name is what keeps that mapping legible against the
//! DDL. A query that needs only part of some table decodes just that part, at
//! its own call site, and wants no struct here.
//!
//! It carries the raw storage-typed columns; [`super::mapper`] turns a row
//! into the validated SDK model (and back where needed: the same module holds
//! the model-to-SQL helpers the insert binds through). Column types match the
//! DDL: `uuid` → [`Uuid`], `text` → [`String`], `int` → `i32`, `numeric` →
//! [`Decimal`], `timestamptz` → [`OffsetDateTime`], `jsonb` →
//! [`serde_json::Value`], and a nullable `text` / `uuid` → `Option<…>`.
//! `xid8` and `usage_entry_type` are exceptions, and neither is a mapping at
//! all: `sqlx` decodes neither into anything this struct could carry — it has
//! no `xid8` implementation, and [`String`] declares itself `TEXT`, which
//! `sqlx` holds incompatible with a `PostgreSQL` enum — so the read list casts
//! both to `text` and both arrive as [`String`]s.

use rust_decimal::Decimal;
use time::OffsetDateTime;
use uuid::Uuid;

/// One row of the `usage_records` hypertable.
///
/// Fields are listed in DDL order so this struct and the migration can be read
/// side by side. That is a reading convenience, not a correctness requirement:
/// `sqlx`'s derived [`FromRow`](sqlx::FromRow) for a named-field struct looks
/// each column up by its own field name, so a `SELECT` list in another order
/// still decodes correctly and one missing a column fails naming it.
///
/// [`Self::xact_id`] has no counterpart on the SDK's `UsageRecord`: it is an
/// ordering value assigned where the entry is stored rather than one the gear
/// submits. The database stamps it from the inserting transaction and it orders
/// the feed (this plugin's DESIGN §3.6). It is decoded rather than left out of
/// the struct so a row is a faithful picture of what was stored.
///
/// The plugin assigns no order of its own: the `LATEST` fold orders on
/// `window_end`, `accepted_at` and `id`, all three of them gear-supplied or
/// gear-derived, and the feed orders on a value the database owns.
/// [`Self::type_key`] is plugin-assigned and has no counterpart either, but it
/// is a partition key rather than an order — it names the type, and no read
/// path reads it as an order. Two btrees do sort on it, as the trailing key of
/// the PRIMARY KEY and of `usage_records_dedup_uniq`; both carry it because a
/// hypertable requires every partition column in each, and because it is a
/// function of `gts_type_id` it separates no two rows the rest of either key
/// would otherwise join.
///
/// [`Self::entry_type`] has no counterpart on the SDK's `UsageRecord` either,
/// for a different reason: the model projects an entry's kind from the
/// [`Invalidation`](usage_collector_sdk::Invalidation) it carries and
/// deliberately stores no discriminator beside it
/// (`cpt-cf-usage-collector-adr-append-only-invalidation`), so
/// [`super::mapper::record_row_to_model`] has nowhere to put the column and
/// does not carry it across. The ledger stores it all the same, because the
/// dedup identity needs the kind as an indexable column of
/// `usage_records_dedup_uniq`; `usage_records_invalidation_pairing` is what
/// keeps the stored kind and the stored pair from disagreeing.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct UsageRecordRow {
    /// `id` — deterministic gateway-derived entry identity (part of the
    /// composite PK).
    pub id: Uuid,
    /// `tenant_id` — owning tenant.
    pub tenant_id: Uuid,
    /// `gts_type_id` — the meter this entry was submitted against.
    pub gts_type_id: String,
    /// `type_key` — the plugin-internal partitioning key of `gts_type_id`
    /// (`usage_type_key`). Not carried on the SDK model; see the struct doc.
    pub type_key: i32,
    /// `quantity` — signed `numeric` quantity.
    pub quantity: Decimal,
    /// `window_start` — inclusive start of the covered period.
    pub window_start: OffsetDateTime,
    /// `window_end` — exclusive end of the covered period, and the hypertable
    /// time dimension. The time-range predicate reads this bound alone,
    /// `from <= window_end < to` and never overlap or containment
    /// (`cpt-cf-usage-collector-adr-window-end-selection`);
    /// `get_usage_record` carries no range at all and looks up by `id`
    /// instead.
    pub window_end: OffsetDateTime,
    /// `resource_id` — resource attribution leaf.
    pub resource_id: String,
    /// `resource_type` — resource attribution leaf.
    pub resource_type: String,
    /// `subject_id` — optional subject attribution leaf. `NULL` means the
    /// entry has no subject at all, which is `UsageRecord::subject_ref`
    /// being `None`.
    pub subject_id: Option<String>,
    /// `subject_type` — the subject's type, optional *within* a subject.
    /// `NULL` alongside a present `subject_id` is an untyped subject. The
    /// reverse is unrepresentable: `SubjectRef` requires a `subject_id` and
    /// makes only the type an `Option`, and the
    /// `usage_records_subject_pairing` constraint refuses a stored type
    /// without an id.
    pub subject_type: Option<String>,
    /// `idempotency_key` — caller-supplied dedup key.
    pub idempotency_key: String,
    /// `invalidates` — the entry this one withdraws, when it is an
    /// invalidation. `NULL` on an ordinary measurement.
    pub invalidates: Option<Uuid>,
    /// `reason_code` — why the withdrawal was issued. Present exactly when
    /// `invalidates` is, by table constraint.
    pub reason_code: Option<String>,
    /// `origin` — the ingestion path that admitted this entry, stored in the
    /// spelling [`RecordOrigin`](usage_collector_sdk::RecordOrigin) owns and
    /// the DDL `CHECK` pins.
    pub origin: String,
    /// `entry_type` — the entry's declared kind, as the dispatched entry
    /// declared it, and not carried on the SDK model (see the struct doc).
    ///
    /// **Nothing in production reads it off this struct**, and it is decoded
    /// anyway for the reason [`Self::xact_id`] is: a row is a faithful picture
    /// of what was stored, and the read list names every written column. It had
    /// one reader while the write path kept an in-process dedup key built from
    /// a stored row's columns; that key is now the entry `id`, which the row
    /// already carries, so the column is read and dropped exactly as
    /// [`super::mapper::record_row_to_model`] drops it.
    ///
    /// A `String` because `usage_entry_type` is a `PostgreSQL` enum and
    /// `sqlx` refuses to decode one into a `TEXT`-declared Rust type, so the
    /// read list selects `entry_type::text`.
    pub entry_type: String,
    /// `accepted_at` — gear-assigned acceptance instant, and the `LATEST`
    /// fold's second ordering key (the gear's DESIGN §3.1).
    pub accepted_at: OffsetDateTime,
    /// `xact_id` — the inserting transaction's id, and the feed order's first
    /// key. Stamped by the column default and never bound by the Record Store,
    /// so it appears in [`super::record_store`]'s read column list and not in
    /// its insert column list. Not carried on the SDK model: it reaches a
    /// caller only inside a `FeedPosition`, which
    /// [`super::feed_position::encode_position`] builds from this and the
    /// row's own `id`.
    ///
    /// A `String` because `xid8` has no `sqlx` decode implementation, so the
    /// read list selects `xact_id::text`; a reader wanting the order parses
    /// it, and comparing the rendered digits instead would misorder two ids of
    /// different lengths.
    pub xact_id: String,
    /// `metadata` — `jsonb` object of declared metadata keys → string values.
    pub metadata: serde_json::Value,
}
