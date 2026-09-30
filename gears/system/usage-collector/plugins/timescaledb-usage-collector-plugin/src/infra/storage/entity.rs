//! `sqlx` row structs mirroring the `usage_records` hypertable (see
//! `migrations/0001_init.sql`). They exist because [`super::mapper`] turns a
//! whole ledger row into the validated SDK `UsageRecord`, and looking its
//! columns up by field name is what keeps that mapping legible against the
//! DDL. A query that needs only part of some table decodes just that part, at
//! its own call site, and wants no struct here.
//!
//! There are two: [`UsageRecordRow`], over every column some insert writes
//! (`RECORD_COLUMNS` in [`super::record_store`]), and [`FeedRecordRow`],
//! which flattens it and adds the one column only a feed page read needs
//! (`FEED_COLUMNS`). Each struct's own doc says why the split falls where it
//! does. [`super::mapper`] turns a row into the validated SDK model (and back
//! where needed: the same module holds the model-to-SQL helpers the insert
//! binds through). Column types match the DDL: `uuid` → [`Uuid`], `text` →
//! [`String`], `int` → `i32`, `numeric` → [`Decimal`], `timestamptz` →
//! [`OffsetDateTime`], `jsonb` → [`serde_json::Value`], and a nullable
//! `text` / `uuid` → `Option<…>`. `xid8` and `usage_entry_type` are
//! exceptions, and neither is a mapping at all: `sqlx` decodes neither into
//! anything either struct could carry — it has no `xid8` implementation, and
//! [`String`] declares itself `TEXT`, which `sqlx` holds incompatible with a
//! `PostgreSQL` enum — so a read list names each as a `::text` cast and both
//! arrive as [`String`]s. `RECORD_COLUMNS` casts only `entry_type`, onto
//! [`UsageRecordRow::entry_type`]; `FEED_COLUMNS` casts both, the second onto
//! [`FeedRecordRow::xact_id`].

use rust_decimal::Decimal;
use time::OffsetDateTime;
use uuid::Uuid;

/// One row of the `usage_records` hypertable, over the read list every plain
/// read path shares
/// (`RECORD_COLUMNS` in [`super::record_store`]).
///
/// **It does not carry `xact_id`.** [`FeedRecordRow`] does, and its own doc
/// says why a feed page read needs a second row type rather than an
/// `Option<String>` field here. Every column this struct's read list names is
/// also a column some insert writes — see [`Self::entry_type`]'s doc, which is
/// where that ground now lives.
///
/// Fields are listed in DDL order so this struct and the migration can be read
/// side by side. That is a reading convenience, not a correctness requirement:
/// `sqlx`'s derived [`FromRow`](sqlx::FromRow) for a named-field struct looks
/// each column up by its own field name, so a `SELECT` list in another order
/// still decodes correctly and one missing a column fails naming it.
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
    /// anyway on this ground: a row is a faithful picture of what was
    /// written, and this struct's read list
    /// (`RECORD_COLUMNS` in [`super::record_store`]) names every column some
    /// insert writes — `entry_type` among them, unlike `xact_id`, which no
    /// insert writes at all and which is why that one lives on
    /// [`FeedRecordRow`] instead rather than on this struct. It had one
    /// reader while the write path kept an in-process dedup key built from a
    /// stored row's columns; that key is now the entry `id`, which the row
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
    /// `metadata` — `jsonb` object of declared metadata keys → string values.
    pub metadata: serde_json::Value,
}

/// One row of a feed page read: [`UsageRecordRow`]'s columns, flattened, plus
/// `xact_id`, the feed order's own key
/// (`FEED_COLUMNS` in [`super::record_store`]).
///
/// **A second row type, not an `Option<String>` field on [`UsageRecordRow`],
/// and the choice is stated here because [`UsageRecordRow`]'s own doc leans
/// on it.** That struct decodes a column with no SDK-model counterpart at all
/// — `entry_type` — on the ground that a row is a faithful picture of what
/// was written and its read list names every written column. `xact_id` is not
/// written by any insert; the column default stamps it from the transaction
/// that inserts the row, so a plain read of [`UsageRecordRow`]'s columns is
/// already a faithful picture of what was written without it. Adding
/// `xact_id: Option<String>` there would not preserve that: `None` would then
/// have to mean "this read did not select the column", which is a different
/// claim from what the same struct already lets `None` mean on `subject_id`
/// — "the database holds no such value". A second row type keeps both
/// readings intact: [`UsageRecordRow`] is silent about `xact_id` because it
/// is not a written column at all, and this struct's `xact_id` is never
/// `None` because every row it decodes was read off
/// `FEED_COLUMNS` (in [`super::record_store`]), which always selects it.
///
/// `#[sqlx(flatten)]` decodes [`UsageRecordRow`]'s fields off the very same
/// row rather than off a nested value — `sqlx`'s `FromRow` derive reads a
/// flattened field's whole struct from the columns already in scope, which is
/// what a `RECORD_COLUMNS`-shaped read list needs, since it is not a separate
/// result set to decode.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct FeedRecordRow {
    /// Every column [`UsageRecordRow`] carries, decoded off the same row.
    #[sqlx(flatten)]
    pub record: UsageRecordRow,
    /// `xact_id` — the inserting transaction's id, and the feed order's first
    /// key. Stamped by the column default and never bound by the Record
    /// Store, so it appears only here and not on [`UsageRecordRow`]'s insert
    /// or plain-read paths. Not carried on the SDK model: it reaches a caller
    /// only inside a `FeedPosition`, which
    /// [`super::feed_position::encode_position`] builds from this and the
    /// row's own `id`.
    ///
    /// A `String` because `xid8` has no `sqlx` decode implementation, so
    /// `FEED_COLUMNS` (in [`super::record_store`]) selects `xact_id::text`; a
    /// reader wanting the order parses it, and comparing the rendered digits
    /// instead would misorder two ids of different lengths.
    ///
    /// **Decoded from the column `xact_id_text`, not `xact_id`.** The feed
    /// page statement orders by the bare name `xact_id`
    /// (`query/feed.rs`'s `ORDER BY xact_id, id`, over
    /// `usage_records_feed_idx`), and `PostgreSQL` resolves a bare `ORDER BY`
    /// name that matches both an output column and an input column to the
    /// *output* column — so aliasing this cast back to `xact_id`, its own
    /// source column's name, would make that `ORDER BY` bind to this `text`
    /// column and sort lexicographically over digit strings instead of
    /// numerically over the `xid8` one. `#[sqlx(rename)]` is what lets the
    /// field keep the name every reader of it expects while the column it
    /// decodes from carries a different one.
    #[sqlx(rename = "xact_id_text")]
    pub xact_id: String,
}
