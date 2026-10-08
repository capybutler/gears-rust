//! `sqlx` row structs mirroring the `usage_records` hypertable (see
//! `migrations/0001_init.sql`). They exist because [`super::mapper`] turns a
//! whole ledger row into the validated SDK `UsageRecord`, and looking its
//! columns up by field name is what keeps that mapping legible against the
//! DDL. A query that needs only part of some table decodes just that part, at
//! its own call site, and wants no struct here.
//!
//! [`UsageRecordRow`] covers every column some insert writes
//! (`RECORD_COLUMNS` in [`super::record_store`]); [`FeedRecordRow`] flattens
//! it and adds the one column only a feed page read needs (`FEED_COLUMNS`).
//! Each struct's own doc says why the split falls where it does.
//!
//! Column types match the DDL: `uuid` → [`Uuid`], `text` → [`String`], `int`
//! → `i32`, `numeric` → [`Decimal`], `timestamptz` → [`OffsetDateTime`],
//! `jsonb` → [`serde_json::Value`], nullable → `Option<…>`. `xid8` and
//! `usage_entry_type` are not mappings at all: `sqlx` has no `xid8`
//! implementation, and [`String`] declares itself `TEXT`, which `sqlx` holds
//! incompatible with a `PostgreSQL` enum — so a read list names each as a
//! `::text` cast and both arrive as [`String`]s.

use rust_decimal::Decimal;
use time::OffsetDateTime;
use uuid::Uuid;

/// One row of the `usage_records` hypertable, over the read list every plain
/// read path shares
/// (`RECORD_COLUMNS` in [`super::record_store`]).
///
/// **It does not carry `xact_id`.** [`FeedRecordRow`] does, and its own doc
/// says why that needs a second row type rather than an `Option<String>` field
/// here.
///
/// Fields are listed in DDL order so this struct and the migration read side
/// by side — a convenience, not a requirement: `sqlx`'s derived
/// [`FromRow`](sqlx::FromRow) looks each column up by field name, so another
/// `SELECT` order still decodes and a missing column fails naming it.
///
/// [`Self::type_key`] has no SDK-model counterpart: it is a partition key
/// rather than an order, and no read path reads it as one. Two btrees sort on
/// it, as the trailing key of the PRIMARY KEY and of
/// `usage_records_dedup_uniq`, because a hypertable requires every partition
/// column in each; being a function of `gts_type_uuid` it separates no two
/// rows the rest of either key would otherwise join.
///
/// [`Self::entry_type`] has none either, for a different reason: the model
/// projects an entry's kind from the
/// [`Invalidation`](usage_collector_sdk::Invalidation) it carries and stores
/// no discriminator beside it
/// (`cpt-cf-usage-collector-adr-append-only-invalidation`), so
/// [`super::mapper::record_row_to_model`] drops the column. The ledger stores
/// it because the dedup identity needs the kind as an indexable column of
/// `usage_records_dedup_uniq`, and `usage_records_invalidation_pairing` keeps
/// the stored kind and the stored pair from disagreeing.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct UsageRecordRow {
    /// `id` — deterministic gateway-derived entry identity (part of the
    /// composite PK).
    pub id: Uuid,
    /// `tenant_id` — owning tenant.
    pub tenant_id: Uuid,
    /// `gts_type_uuid` — the meter's registry reference, and the only meter
    /// identity this ledger stores. The Plugin SPI carries it in both
    /// directions, and `StoredUsageRecord` names its meter by it alone.
    pub gts_type_uuid: Uuid,
    /// `type_key` — the plugin-internal partitioning key of `gts_type_uuid`
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
    /// written, and this struct's read list names every column some insert
    /// writes — `entry_type` among them, unlike `xact_id`, which no insert
    /// writes and which is why that one lives on [`FeedRecordRow`].
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
/// **A second row type, not an `Option<String>` field on
/// [`UsageRecordRow`].** `xact_id` is written by no insert — the column
/// default stamps it from the inserting transaction — so a plain read of
/// [`UsageRecordRow`]'s columns is already a faithful picture of what was
/// written without it. Adding `xact_id: Option<String>` there would make
/// `None` mean "this read did not select the column", a different claim from
/// what `None` already means on `subject_id`. A second row type keeps both
/// readings intact, and this struct's `xact_id` is never `None` because every
/// row it decodes was read off `FEED_COLUMNS`, which always selects it.
///
/// `#[sqlx(flatten)]` decodes [`UsageRecordRow`]'s fields off the very same
/// row rather than a nested value, which is what a `RECORD_COLUMNS`-shaped
/// read list needs.
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
    /// [`crate::domain::feed_position::encode_position`] builds from this and
    /// the row's own `id`.
    ///
    /// A `String` because `xid8` has no `sqlx` decode implementation, so
    /// `FEED_COLUMNS` (in [`super::record_store`]) selects `xact_id::text`; a
    /// reader wanting the order parses it, and comparing the rendered digits
    /// instead would misorder two ids of different lengths.
    ///
    /// **Decoded from the column `xact_id_text`, not `xact_id`.** The feed
    /// page statement orders by the bare name `xact_id`, and `PostgreSQL`
    /// resolves a bare `ORDER BY` name matching both an output and an input
    /// column to the *output* one — so aliasing this cast back to its source
    /// column's name would sort lexicographically over digit strings instead
    /// of numerically over the `xid8`. `#[sqlx(rename)]` lets the field keep
    /// the name its readers expect while decoding from a differently named
    /// column.
    #[sqlx(rename = "xact_id_text")]
    pub xact_id: String,
}
