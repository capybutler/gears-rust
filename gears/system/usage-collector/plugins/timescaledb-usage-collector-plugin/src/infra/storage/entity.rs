//! `sqlx` row structs mirroring the `usage_records` hypertable (see
//! `migrations/0001_init.sql`). The schema's other table,
//! `usage_acceptance_sequence`, is a keyed counter and has no row struct
//! here.
//!
//! These carry the raw storage-typed columns; [`super::mapper`] turns a row
//! into the validated SDK model. Column types match the DDL: `uuid` →
//! [`Uuid`], `text` → [`String`], `numeric` → [`Decimal`], `timestamptz` →
//! [`OffsetDateTime`], `bigint` → `i64`, `jsonb` → [`serde_json::Value`], and
//! a nullable `text` / `uuid` → `Option<…>`.

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
/// so the mapper decodes them and drops them: `ingested_at` is the server
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
    /// `value` — signed `numeric` quantity.
    pub value: Decimal,
    /// `window_start` — inclusive start of the covered period.
    pub window_start: OffsetDateTime,
    /// `window_end` — exclusive end of the covered period, and the hypertable
    /// time dimension. Every selection predicate reads this.
    pub window_end: OffsetDateTime,
    /// `resource_id` — resource attribution leaf.
    pub resource_id: String,
    /// `resource_type` — resource attribution leaf.
    pub resource_type: String,
    /// `subject_id` — optional subject attribution leaf.
    pub subject_id: Option<String>,
    /// `subject_type` — optional subject attribution leaf.
    pub subject_type: Option<String>,
    /// `idempotency_key` — caller-supplied dedup key.
    pub idempotency_key: String,
    /// `invalidates` — the entry this one withdraws, when it is an
    /// invalidation. `NULL` on an ordinary measurement.
    pub invalidates: Option<Uuid>,
    /// `reason_code` — why the withdrawal was issued. Present exactly when
    /// `invalidates` is, by table constraint.
    pub reason_code: Option<String>,
    /// `origin` — `'live'` / `'backfill'`; the ingestion path that admitted
    /// this entry.
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
