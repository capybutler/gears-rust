//! `sqlx` row struct mirroring the `usage_records` hypertable (see
//! `migrations/0001_init.sql`). The schema's other table,
//! `usage_acceptance_sequence`, is a keyed counter and has no row struct
//! here.
//!
//! It carries the raw storage-typed columns; [`super::mapper`] turns a row
//! into the validated SDK model (and back where needed: the same module holds
//! the model-to-SQL helpers the insert binds through). Column types match the
//! DDL: `uuid` → [`Uuid`], `text` → [`String`], `int` → `i32`, `numeric` →
//! [`Decimal`], `timestamptz` → [`OffsetDateTime`], `bigint` → `i64`, `jsonb`
//! → [`serde_json::Value`], and a nullable `text` / `uuid` → `Option<…>`.

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
/// Two of the columns below have no counterpart on the SDK's `UsageRecord`,
/// so nothing carries them past this struct: `ingested_at` is the server
/// insert time, and `acceptance_sequence` is assigned by this plugin — the
/// gear's DESIGN §3.7 (`gears/system/usage-collector/docs/DESIGN.md`) obliges
/// the plugin to keep it strictly monotonic per `(tenant_id, gts_type_id)` —
/// and orders reads here, never travelling back out through the SPI. They are
/// decoded rather than left out of the struct so a row is a faithful picture
/// of what was stored.
///
/// The ledger's `entry_type` is deliberately *not* a field. It is a stored
/// generated column — `CASE WHEN invalidates IS NULL THEN 'record' ELSE
/// 'invalidation' END` — that exists so `$filter=entry_type eq 'invalidation'`
/// resolves to a real column. Nothing decodes it, because the SDK model
/// derives the same fact from `invalidation.is_some()`; a field here would be
/// a second, redundant spelling of [`Self::invalidates`].
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
    /// `value` — signed `numeric` quantity.
    pub value: Decimal,
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
    /// `acceptance_sequence` — plugin-assigned, strictly monotonic per
    /// `(tenant_id, gts_type_id)`. Not carried on the SDK model; see the
    /// struct doc.
    pub acceptance_sequence: i64,
    /// `metadata` — `jsonb` object of declared metadata keys → string values.
    pub metadata: serde_json::Value,
    /// `ingested_at` — server insert timestamp (`DEFAULT now()`). Not carried
    /// on the SDK model; see the struct doc.
    pub ingested_at: OffsetDateTime,
}
