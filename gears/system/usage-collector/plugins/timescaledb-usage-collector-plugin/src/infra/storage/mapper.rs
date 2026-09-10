//! Row → SDK-model mapping plus the small pure helpers the stores share.
//!
//! Every conversion that can fail on malformed stored data surfaces as
//! [`UsageCollectorPluginError::Internal`] — a row already in the database that
//! cannot be reconstituted is a plugin invariant break, not a caller error.
//!
//! Of the columns the model carries, `id`, `tenant_id`, `value` and the two
//! period bounds move across unchanged. The rest are validated on the way in:
//! `resource_id` and `resource_type` through [`ResourceRef::new`], `subject_id`
//! and `subject_type` through [`SubjectRef::new`], `idempotency_key` through
//! [`IdempotencyKey::new`], `metadata` through [`metadata_jsonb_to_map`], and
//! these through the helpers below, which take more explaining:
//!
//! - `gts_type_id` becomes a [`MeterTypeId`] via [`meter_type_id_from_str`].
//!   There is no borrowing helper beside it: `MeterTypeId::as_str` is the bind
//!   direction, and a wrapper would be a second spelling of it.
//! - `origin` becomes a [`RecordOrigin`]. There is deliberately no
//!   `origin_to_sql` counterpart to [`parse_origin`]: [`RecordOrigin::as_str`]
//!   already is the SQL form, and [`parse_origin`] compares against that same
//!   accessor rather than against its own literals, so the two directions
//!   cannot drift apart.
//! - `invalidates` and `reason_code` are two nullable columns standing for one
//!   `Option<Invalidation>` field; [`invalidation_from_row`] rejoins them and
//!   [`invalidation_to_row`] splits them again. This is the one pair with a
//!   helper in both directions, because it is the one pair whose halves the
//!   insert could otherwise bind independently.

use std::collections::BTreeMap;

use serde_json::Value as JsonValue;
use uuid::Uuid;

use usage_collector_sdk::{
    IdempotencyKey, Invalidation, MetadataKey, MeterTypeId, ReasonCode, RecordOrigin, ResourceRef,
    SubjectRef, UsageCollectorPluginError, UsageRecord,
};

use super::entity::UsageRecordRow;

/// Reconstruct a validated [`MeterTypeId`] from a stored string.
///
/// This is `MeterTypeId::from_str` plus the lift into
/// [`UsageCollectorPluginError`], and the lift is the whole point: the SDK
/// reports a bad id as a *caller* error, but a value that is already in the
/// database is this plugin's invariant to have broken. The reverse direction
/// needs no helper at all — `MeterTypeId::as_str` is what the insert binds.
///
/// # Errors
///
/// Returns [`UsageCollectorPluginError::Internal`] when the stored value is not
/// a valid meter type id (a stored-data invariant break).
pub fn meter_type_id_from_str(raw: &str) -> Result<MeterTypeId, UsageCollectorPluginError> {
    MeterTypeId::new(raw).map_err(|e| {
        UsageCollectorPluginError::internal(format!("stored gts_type_id `{raw}` invalid: {e}"))
    })
}

/// Parse a stored `origin` string into [`RecordOrigin`].
///
/// The accepted vocabulary is taken from [`RecordOrigin::as_str`] rather than
/// restated here, so this reader and the writer that binds the column cannot
/// disagree. The DDL `CHECK (origin IN ('live', 'backfill'))` pins the same
/// two values on the storage side.
///
/// # Errors
///
/// Returns [`UsageCollectorPluginError::Internal`] for any other value.
pub fn parse_origin(raw: &str) -> Result<RecordOrigin, UsageCollectorPluginError> {
    if raw == RecordOrigin::Live.as_str() {
        Ok(RecordOrigin::Live)
    } else if raw == RecordOrigin::Backfill.as_str() {
        Ok(RecordOrigin::Backfill)
    } else {
        Err(UsageCollectorPluginError::internal(format!(
            "stored origin `{raw}` is not `{}`/`{}`",
            RecordOrigin::Live.as_str(),
            RecordOrigin::Backfill.as_str()
        )))
    }
}

/// Reassemble the stored invalidation pair into an [`Invalidation`].
///
/// The `usage_records_invalidation_pairing` constraint holds the two columns
/// both-present or both-absent, so a half-populated row is a stored-invariant
/// break rather than a shape the model can carry: [`Invalidation`] groups the
/// target and the reason precisely so that half is unrepresentable.
///
/// # Errors
///
/// Returns [`UsageCollectorPluginError::Internal`] when exactly one of the two
/// is present, or when the stored reason fails [`ReasonCode`] validation.
pub fn invalidation_from_row(
    invalidates: Option<Uuid>,
    reason_code: Option<String>,
) -> Result<Option<Invalidation>, UsageCollectorPluginError> {
    match (invalidates, reason_code) {
        (None, None) => Ok(None),
        (Some(target), Some(raw)) => {
            let reason = ReasonCode::new(raw).map_err(|e| {
                UsageCollectorPluginError::internal(format!("stored reason_code invalid: {e}"))
            })?;
            Ok(Some(Invalidation { target, reason }))
        }
        (Some(target), None) => Err(UsageCollectorPluginError::internal(format!(
            "stored entry `{target}` names an invalidation target with no reason_code"
        ))),
        (None, Some(raw)) => Err(UsageCollectorPluginError::internal(format!(
            "stored entry carries reason_code `{raw}` with no invalidation target"
        ))),
    }
}

/// Split an [`Invalidation`] back into the two columns that store it.
///
/// The inverse of [`invalidation_from_row`], and the reason the insert cannot
/// bind `invalidates` and `reason_code` separately: going through one function
/// makes the half-populated pair unrepresentable on the way out, the same way
/// [`Invalidation`] makes it unrepresentable on the way in. Without it the
/// write direction would reintroduce exactly the split shape the read
/// direction exists to refuse, with nothing catching it until Postgres rejects
/// the row on `usage_records_invalidation_pairing` at runtime.
#[must_use]
pub fn invalidation_to_row(invalidation: Option<&Invalidation>) -> (Option<Uuid>, Option<&str>) {
    match invalidation {
        None => (None, None),
        Some(Invalidation { target, reason }) => (Some(*target), Some(reason.as_str())),
    }
}

/// Convert a `jsonb` object of string → string into a typed metadata map.
///
/// `Null` maps to an empty map (defensive — the column defaults to `'{}'`).
///
/// # Errors
///
/// Returns [`UsageCollectorPluginError::Internal`] when the value is not a JSON
/// object, a value is not a JSON string, or a key fails [`MetadataKey::new`].
pub fn metadata_jsonb_to_map(
    value: JsonValue,
) -> Result<BTreeMap<MetadataKey, String>, UsageCollectorPluginError> {
    let obj = match value {
        JsonValue::Null => return Ok(BTreeMap::new()),
        JsonValue::Object(map) => map,
        other => {
            return Err(UsageCollectorPluginError::internal(format!(
                "stored metadata is not a JSON object: {other}"
            )));
        }
    };

    let mut out = BTreeMap::new();
    for (key, val) in obj {
        let value_str = match val {
            JsonValue::String(s) => s,
            other => {
                return Err(UsageCollectorPluginError::internal(format!(
                    "stored metadata value for key `{key}` is not a string: {other}"
                )));
            }
        };
        // `key` is already owned (the loop consumes `obj` by value), so move it
        // into the validation. The message below cannot name the key and does
        // not try: `MetadataKey::new` passes a fixed reason to
        // `invalid_metadata_key`, which drops it into `newtype_validation`
        // with `resource_name: None`, so the rejected value reaches no field
        // of the error — and `key` has been moved by the time `e` exists.
        let metadata_key = MetadataKey::new(key).map_err(|e| {
            UsageCollectorPluginError::internal(format!("stored metadata key invalid: {e}"))
        })?;
        out.insert(metadata_key, value_str);
    }
    Ok(out)
}

/// Serialize a typed metadata map into a `jsonb` object of string → string.
#[must_use]
pub fn metadata_map_to_jsonb(map: &BTreeMap<MetadataKey, String>) -> JsonValue {
    let obj = map
        .iter()
        .map(|(k, v)| (k.as_str().to_owned(), JsonValue::String(v.clone())))
        .collect::<serde_json::Map<String, JsonValue>>();
    JsonValue::Object(obj)
}

/// Map a [`UsageRecordRow`] into a validated [`UsageRecord`].
///
/// Two of the row's columns are read and deliberately dropped, because the
/// model has no field for either: `acceptance_sequence`, which this plugin
/// assigns and orders reads by, and `ingested_at`, the server insert time.
/// Neither travels back out through the SPI. This is not an oversight; see
/// [`UsageRecordRow`]'s own doc for why they are decoded at all.
///
/// The two stored pairs are treated asymmetrically, deliberately. A
/// half-populated invalidation pair is *refused*, because [`Invalidation`] has
/// a shape to reconstruct into and half of it is not that shape. A
/// `subject_type` with no `subject_id` is *dropped* — the `match` on
/// `row.subject_id` never looks at the type — because [`SubjectRef`] requires
/// an id and makes only the type optional, so there is no half-built subject
/// to refuse on behalf of. Both shapes are already refused at the table by
/// `usage_records_invalidation_pairing` and `usage_records_subject_pairing`;
/// the difference here is only in what a mapper can say about them.
///
/// # Errors
///
/// Returns [`UsageCollectorPluginError::Internal`] when any stored component
/// fails its SDK newtype validation (`gts_type_id`, `resource_ref`,
/// `subject_ref`, `idempotency_key`, `metadata`, `origin`), or when the stored
/// invalidation pair is half-populated or carries a `reason_code` that fails
/// [`ReasonCode`] validation (both via [`invalidation_from_row`]).
pub fn record_row_to_model(row: UsageRecordRow) -> Result<UsageRecord, UsageCollectorPluginError> {
    // The composite primary key, captured before the row is picked apart.
    // `ResourceRef::new`, `SubjectRef::new` and `IdempotencyKey::new` all
    // report a fixed reason and never echo the value they rejected, so without
    // this an operator is told a stored row is malformed and not which one.
    //
    // The binding is required rather than tidy: reading `row.id` inside the
    // `map_err` closure would borrow `row` in the same expression that moves
    // `row.resource_id` and `row.resource_type` into `ResourceRef::new`.
    let (id, window_end) = (row.id, row.window_end);

    let gts_type_id = meter_type_id_from_str(&row.gts_type_id)?;

    let resource_ref = ResourceRef::new(row.resource_id, row.resource_type).map_err(|e| {
        UsageCollectorPluginError::internal(format!(
            "stored row `{id}` (window_end {window_end}): resource_ref invalid: {e}"
        ))
    })?;

    let subject_ref = match row.subject_id {
        Some(subject_id) => Some(SubjectRef::new(subject_id, row.subject_type).map_err(|e| {
            UsageCollectorPluginError::internal(format!(
                "stored row `{id}` (window_end {window_end}): subject_ref invalid: {e}"
            ))
        })?),
        None => None,
    };

    let idempotency_key = IdempotencyKey::new(row.idempotency_key).map_err(|e| {
        UsageCollectorPluginError::internal(format!(
            "stored row `{id}` (window_end {window_end}): idempotency_key invalid: {e}"
        ))
    })?;

    let metadata = metadata_jsonb_to_map(row.metadata)?;
    let origin = parse_origin(&row.origin)?;
    let invalidation = invalidation_from_row(row.invalidates, row.reason_code)?;

    Ok(UsageRecord {
        id: row.id,
        gts_type_id,
        tenant_id: row.tenant_id,
        resource_ref,
        subject_ref,
        metadata,
        value: row.value,
        idempotency_key,
        origin,
        invalidation,
        window_start: row.window_start,
        window_end: row.window_end,
    })
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "mapper_tests.rs"]
mod mapper_tests;
