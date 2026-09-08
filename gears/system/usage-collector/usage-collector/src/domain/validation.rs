//! Pure shape-validation algorithms for the ingest boundary.
//!
//! One function lives here: [`validate_submit_record_metadata`] enforces the
//! ingest-time closed-shape membership and the configurable size cap against
//! an already-typed `BTreeMap<MetadataKey, String>`, validating against the
//! meter's resolved [`ResolvedDeclaration`] rather than a plugin-owned
//! catalog row. The declaration's compiled `metadata_schema` is the closed
//! surface, enforced in code per
//! [`crate::domain::type_resolver::CompiledMetadataSchema::validate`].
//!
//! This module validates a submission against a **declaration**. The rules
//! that hold a submission against *another entry* — the faithful copy an
//! invalidation must be of the record it withdraws — are
//! [`crate::domain::invalidation`]: different input, different failure
//! vocabulary.
//!
//! `gts_type_id` is NOT re-validated here: [`usage_collector_sdk::MeterTypeId`]
//! is a validating newtype that already rejects empty values, ids missing
//! the reserved-prefix `~` segment, and ids that do not derive from the
//! reserved usage-record base type.
//!
//! Nothing here reads a meter's counter/gauge classification. Usage-type
//! ownership moved to `types-registry`, so there is no caller-visible `kind`
//! left to enforce a value-sign rule against, and no value-sign rule
//! survives anywhere: the sign of a quantity carries no structural meaning
//! (`cpt-cf-usage-collector-adr-append-only-invalidation`).

use std::collections::BTreeMap;

use usage_collector_sdk::{MetadataKey, UsageCollectorError};

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

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "validation_tests.rs"]
mod validation_tests;
