//! Pure shape-validation algorithms for the ingest boundary.
//!
//! - [`validate_submit_record_metadata`] enforces the ingest-time closed
//!   shape membership and the configurable size cap against an already-typed
//!   `BTreeMap<MetadataKey, String>`, validating against the meter's
//!   resolved [`ResolvedDeclaration`] rather than a plugin-owned catalog row
//!   (Task 9): the declaration's compiled `metadata_schema` is the closed
//!   surface, enforced in code per
//!   [`crate::domain::type_resolver::CompiledMetadataSchema::validate`].
//!
//! `gts_type_id` is NOT re-validated here: [`usage_collector_sdk::MeterTypeId`]
//! is a validating newtype that already rejects empty values, ids missing
//! the reserved-prefix `~` segment, and ids that do not derive from the
//! reserved usage-record base type.
//!
//! [`validate_record_semantics`] no longer takes a catalog row: with usage-type
//! ownership moved to `types-registry`, there is no caller-visible `kind` left
//! to enforce a gauge/counter value-sign rule against, so those branches are
//! deleted outright (not relocated). The `corrects_id` / compensation
//! mechanism itself is unaffected — it is removed in a later slice, not
//! this one.

use std::collections::BTreeMap;

use toolkit_macros::domain_model;
use usage_collector_sdk::{MetadataKey, UsageCollectorError, UsageRecord, UsageRecordStatus};
use uuid::Uuid;

use crate::domain::type_resolver::ResolvedDeclaration;

/// Default per-record metadata payload size cap (8 KiB), enforced on
/// `create_usage_record` before plugin dispatch when the host has no more
/// specific configured value.
///
/// `UsageCollectorConfig::metadata_size_cap_bytes` (Task 7) defaults to the
/// identical `8192`, so a `Service` built without an explicit cap (every
/// `Service::new` / `Service::new_with_metrics` caller not itself threading a
/// configured value) enforces exactly this constant — wiring the config value
/// through is a behavioural no-op for the default deployment. `pub(crate)` so
/// `crate::config` and `crate::domain::service` can both anchor their own
/// defaults on this single constant rather than duplicating the magic number.
// @cpt-begin:cpt-cf-usage-collector-algo-usage-emission-metadata-size-cap-enforcement:p1:inst-algo-metadata-read-cap
pub(crate) const DEFAULT_METADATA_SIZE_CAP_BYTES: usize = 8 * 1024;
// @cpt-end:cpt-cf-usage-collector-algo-usage-emission-metadata-size-cap-enforcement:p1:inst-algo-metadata-read-cap

/// Validates the `metadata` payload of a `UsageRecord` submission against the
/// referenced meter's resolved [`ResolvedDeclaration`] per the usage-emission
/// ingestion contract.
///
/// Runs two checks: a closed-shape / per-value check delegated to the
/// declaration's compiled
/// [`metadata_schema`](ResolvedDeclaration::metadata_schema) (every key MUST
/// be a member of the declared closed surface, and every declared key's value
/// MUST satisfy whatever per-value constraint the schema names — both
/// enforced in code, surfaced as [`UsageCollectorError::InvalidArgument`])
/// and a size cap (serialized metadata ≤ `metadata_size_cap_bytes`, surfaced
/// as [`UsageCollectorError::InvalidArgument`]). `metadata_size_cap_bytes` is
/// the caller's configured cap (`Service::metadata_size_cap_bytes`, itself
/// threaded from `UsageCollectorConfig::metadata_size_cap_bytes`), not a
/// hard-coded constant — see [`DEFAULT_METADATA_SIZE_CAP_BYTES`] for the
/// value it defaults to.
///
/// The "metadata must be a JSON object" and "value must be a string" branches
/// no longer exist here: the SDK / REST DTO now carries `metadata` as a
/// typed `BTreeMap<MetadataKey|String, String>`, so structural / value-shape
/// rejections happen at the deserialize boundary and never reach this
/// function.
// @cpt-algo:cpt-cf-usage-collector-algo-usage-emission-metadata-size-cap-enforcement:p1
// @cpt-dod:cpt-cf-usage-collector-dod-usage-emission-fr-record-metadata:p1
// @cpt-dod:cpt-cf-usage-collector-dod-usage-emission-entity-record-metadata:p1
// @cpt-dod:cpt-cf-usage-collector-dod-usage-emission-constraint-no-business-logic:p1
#[allow(clippy::missing_errors_doc)]
pub fn validate_submit_record_metadata(
    declaration: &ResolvedDeclaration,
    metadata: &BTreeMap<MetadataKey, String>,
    metadata_size_cap_bytes: usize,
) -> Result<(), UsageCollectorError> {
    // `CompiledMetadataSchema::validate` takes plain `String` keys (it is
    // shared with the read-path query surface, which has no `MetadataKey`
    // newtype of its own); the conversion is a cheap per-entry copy, not a
    // second validation pass.
    let plain_metadata: BTreeMap<String, String> = metadata
        .iter()
        .map(|(key, value)| (key.as_str().to_owned(), value.clone()))
        .collect();
    declaration.metadata_schema.validate(&plain_metadata)?;

    // @cpt-begin:cpt-cf-usage-collector-algo-usage-emission-metadata-size-cap-enforcement:p1:inst-algo-metadata-read-input
    // @cpt-begin:cpt-cf-usage-collector-algo-usage-emission-metadata-size-cap-enforcement:p1:inst-algo-metadata-serialize
    // @cpt-begin:cpt-cf-usage-collector-algo-usage-emission-metadata-size-cap-enforcement:p1:inst-algo-metadata-measure
    // @cpt-begin:cpt-cf-usage-collector-algo-usage-emission-metadata-size-cap-enforcement:p1:inst-algo-metadata-exceeds
    // Serialized JSON byte size. The plugin re-serializes the same `metadata`
    // map at persistence time, so this buffer is dropped immediately. The
    // `BTreeMap<MetadataKey, String>` shape (string keys, string values)
    // makes serialization infallible; the defensive arm maps the impossible
    // failure to `Internal` rather than panicking.
    let size = serde_json::to_vec(metadata)
        .map(|bytes| bytes.len())
        .map_err(|err| {
            UsageCollectorError::internal(format!("metadata size measurement failed: {err}"))
        })?;
    if size > metadata_size_cap_bytes {
        return Err(UsageCollectorError::metadata_size_exceeded(
            size,
            metadata_size_cap_bytes,
        ));
    }
    // @cpt-end:cpt-cf-usage-collector-algo-usage-emission-metadata-size-cap-enforcement:p1:inst-algo-metadata-exceeds
    // @cpt-end:cpt-cf-usage-collector-algo-usage-emission-metadata-size-cap-enforcement:p1:inst-algo-metadata-measure
    // @cpt-end:cpt-cf-usage-collector-algo-usage-emission-metadata-size-cap-enforcement:p1:inst-algo-metadata-serialize
    // @cpt-end:cpt-cf-usage-collector-algo-usage-emission-metadata-size-cap-enforcement:p1:inst-algo-metadata-read-input

    // @cpt-begin:cpt-cf-usage-collector-algo-usage-emission-metadata-size-cap-enforcement:p1:inst-algo-metadata-valid
    Ok(())
    // @cpt-end:cpt-cf-usage-collector-algo-usage-emission-metadata-size-cap-enforcement:p1:inst-algo-metadata-valid
}

/// Outcome of the write-path semantics check.
///
/// Pre-Task-9 this was the result of a four-cell `(MetricSemantics ×
/// corrects_id presence)` value-sign matrix keyed off a plugin-owned
/// catalog row's `kind`. With usage-type ownership moved to `types-registry`
/// there is no caller-visible `kind` left, so the matrix — and the
/// gauge/counter value-sign rules it enforced — is deleted outright, not
/// relocated. Only the `corrects_id`-presence half of the check survives:
///
/// * [`validate_record_semantics`] is sync and infallible: it only reads
///   whether the submitted record carries a `corrects_id`. When it does, it
///   returns [`SemanticsOutcome::NeedsL1Lookup`] so the caller dispatches
///   the SPI single-row read and runs [`verify_l1_corrects_id`] against the
///   result.
/// * [`verify_l1_corrects_id`] is sync and runs the L1 referential checks
///   against the referenced row returned by the SPI.
///
/// The `corrects_id` / compensation mechanism itself (this enum included) is
/// unaffected by this change — it is removed in a later slice, not this one.
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SemanticsOutcome {
    /// The submission carries no `corrects_id`; no L1 lookup is needed.
    Valid,
    /// The submission carries a `corrects_id`; the caller MUST dispatch
    /// `get_usage_record(corrects_id)` and run [`verify_l1_corrects_id`]
    /// before persisting.
    NeedsL1Lookup {
        /// `corrects_id` to look up via the storage Plugin SPI.
        corrects_id: Uuid,
    },
}

/// Does this submission carry a `corrects_id`?
///
/// Infallible: with the `kind`-driven value-sign matrix deleted (see
/// [`SemanticsOutcome`]'s doc comment), there is nothing left here that can
/// reject a submission — only presence/absence of `corrects_id` decides
/// whether the caller must follow up with the L1 SPI lookup.
// @cpt-dod:cpt-cf-usage-collector-dod-usage-emission-compensation-no-business-logic:p1
#[must_use]
pub fn validate_record_semantics(record: &UsageRecord) -> SemanticsOutcome {
    match record.corrects_id {
        Some(corrects_id) => SemanticsOutcome::NeedsL1Lookup { corrects_id },
        None => SemanticsOutcome::Valid,
    }
}

/// Run the L1 `corrects_id` referential checks of
/// `cpt-cf-usage-collector-algo-usage-emission-semantics-enforcement-on-ingest-v2`
/// against the row the storage Plugin SPI returned for the caller-supplied
/// `corrects_id`.
///
/// `record` is the incoming compensation submission; `referenced` is the
/// row returned by `get_usage_record(corrects_id)`. The plugin's
/// `UsageRecordNotFound` is re-classified to
/// [`UsageCollectorError::NotFound`] by the caller before this
/// helper runs, so the helper only sees an existing row.
///
/// # Errors
///
/// * [`UsageCollectorError::Conflict`] when the
///   referenced row is itself a compensation.
/// * [`UsageCollectorError::Conflict`] when the referenced row
///   does not share the full identity tuple
///   `(tenant_id, gts_type_id, resource_ref, subject_ref)` with the incoming
///   compensation. `subject_ref` presence is part of the identity — a
///   `None` vs `Some(_)` mismatch is a scope error.
/// * [`UsageCollectorError::Conflict`] when the referenced row is
///   not [`UsageRecordStatus::Active`] (including a row concurrently being
///   deactivated — the same active-status check serialises against the
///   cascade).
// (algo scope marker `cpt-cf-usage-collector-algo-usage-emission-semantics-enforcement-on-ingest-v2`
// is declared on `resolve_l1_lookups` in `domain/service.rs`, not here: the
// kind-driven branches `validate_record_semantics` used to anchor it on were
// deleted in this file along with the marker. This function still realizes
// part of that same algorithm; declaring the scope marker here too would be
// a duplicate.)
// @cpt-dod:cpt-cf-usage-collector-dod-usage-emission-corrects-id-l1:p1
// @cpt-dod:cpt-cf-usage-collector-dod-usage-emission-compensation-concurrency:p1
//
// `corrects_id` is taken as a typed `Uuid` so the `record.corrects_id == Some(_)`
// precondition is encoded at the type level: callers cannot accidentally invoke
// the helper for a non-compensation row.
pub fn verify_l1_corrects_id(
    record: &UsageRecord,
    corrects_id: Uuid,
    referenced: &UsageRecord,
) -> Result<(), UsageCollectorError> {
    // @cpt-begin:cpt-cf-usage-collector-algo-usage-emission-semantics-enforcement-on-ingest-v2:p1:inst-algo-semantics-l1-targets-compensation
    if referenced.corrects_id.is_some() {
        return Err(UsageCollectorError::corrects_id_targets_compensation(
            corrects_id,
        ));
    }
    // @cpt-end:cpt-cf-usage-collector-algo-usage-emission-semantics-enforcement-on-ingest-v2:p1:inst-algo-semantics-l1-targets-compensation

    // @cpt-begin:cpt-cf-usage-collector-algo-usage-emission-semantics-enforcement-on-ingest-v2:p1:inst-algo-semantics-l1-cross-scope
    if referenced.tenant_id != record.tenant_id
        || referenced.gts_type_id != record.gts_type_id
        || referenced.resource_ref != record.resource_ref
        || referenced.subject_ref != record.subject_ref
    {
        return Err(UsageCollectorError::corrects_id_wrong_scope(corrects_id));
    }
    // @cpt-end:cpt-cf-usage-collector-algo-usage-emission-semantics-enforcement-on-ingest-v2:p1:inst-algo-semantics-l1-cross-scope

    // @cpt-begin:cpt-cf-usage-collector-algo-usage-emission-semantics-enforcement-on-ingest-v2:p1:inst-algo-semantics-l1-inactive-or-deactivating
    if referenced.status != UsageRecordStatus::Active {
        return Err(UsageCollectorError::corrects_id_inactive(corrects_id));
    }
    // @cpt-end:cpt-cf-usage-collector-algo-usage-emission-semantics-enforcement-on-ingest-v2:p1:inst-algo-semantics-l1-inactive-or-deactivating

    // @cpt-begin:cpt-cf-usage-collector-algo-usage-emission-semantics-enforcement-on-ingest-v2:p1:inst-algo-semantics-counter-compensation-valid
    Ok(())
    // @cpt-end:cpt-cf-usage-collector-algo-usage-emission-semantics-enforcement-on-ingest-v2:p1:inst-algo-semantics-counter-compensation-valid
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "validation_tests.rs"]
mod validation_tests;
