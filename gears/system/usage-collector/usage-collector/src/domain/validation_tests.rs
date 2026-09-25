//! Unit tests for the metadata shape-validation algorithm.

use std::collections::BTreeMap;
use std::sync::Arc;
use toolkit_gts::gts_id;

use serde_json::json;
use types_registry_sdk::{GtsTypeId, GtsTypeSchema};
use usage_collector_sdk::{MetadataKey, MeterTypeId, UsageCollectorError, ValidationReason};

use super::{DEFAULT_METADATA_SIZE_CAP_BYTES, validate_submit_record_metadata};
use crate::domain::type_resolver::ResolvedDeclaration;

fn mk_key(value: &str) -> MetadataKey {
    MetadataKey::new(value).expect("test fixture supplies a valid metadata key")
}

const SAMPLE_METER_ID: &str = gts_id!("cf.core.uc.usage_record.v1~tenant.example._.foo.v1~");
const BASE_ID: &str = gts_id!("cf.core.uc.usage_record.v1~");

fn meter_id() -> MeterTypeId {
    MeterTypeId::new(SAMPLE_METER_ID).expect("valid meter type id")
}

/// A [`ResolvedDeclaration`] (fold `SUM`, unit `bytes`) whose closed metadata
/// surface admits exactly `keys` — the unit-test analogue of
/// `test_support::fake_meter_schema`, built locally so this file stays
/// self-contained (mirrors the pattern `type_resolver::metadata_tests` and
/// `type_resolver::declaration_tests` already use for their own schema
/// fixtures).
fn declaration_with_metadata_keys(keys: &[&str]) -> ResolvedDeclaration {
    let base = GtsTypeSchema::try_new(
        GtsTypeId::try_new(BASE_ID).expect("base type id"),
        json!({
            "type": "object",
            "x-gts-abstract": true,
            "properties": {
                "metadata": { "type": "object", "additionalProperties": { "type": "string" } }
            }
        }),
        None,
        None,
    )
    .expect("base schema");

    let properties: serde_json::Map<String, serde_json::Value> = keys
        .iter()
        .map(|key| ((*key).to_owned(), json!({ "type": "string" })))
        .collect();

    let schema = GtsTypeSchema::try_new(
        meter_id().as_gts().clone(),
        json!({
            "allOf": [{ "$ref": format!("gts://{BASE_ID}") }],
            "x-gts-traits": {
                "aggregation_fold": "SUM",
                "canonical_unit": "bytes"
            },
            "properties": {
                "metadata": {
                    "type": "object",
                    "additionalProperties": false,
                    "properties": properties
                }
            }
        }),
        None,
        Some(Arc::new(base)),
    )
    .expect("derived schema");

    ResolvedDeclaration::from_schema(meter_id(), &schema).expect("valid declaration")
}

// Repointed at the resolved declaration: the closed-shape check now
// runs against `ResolvedDeclaration::metadata_schema`, not a plugin-owned
// catalog row's `metadata_fields` set — but the intent this test pins ("an
// undeclared metadata key is rejected") is unchanged.
#[test]
fn undeclared_metadata_key_is_rejected_before_persistence() {
    let declaration = declaration_with_metadata_keys(&[]);
    validate_submit_record_metadata(
        &declaration,
        &BTreeMap::new(),
        DEFAULT_METADATA_SIZE_CAP_BYTES,
    )
    .expect("a declaration with no metadata properties must accept an empty payload");

    let mut bad = BTreeMap::new();
    bad.insert(mk_key("region"), "us-east".to_owned());
    let err = validate_submit_record_metadata(&declaration, &bad, DEFAULT_METADATA_SIZE_CAP_BYTES)
        .expect_err("a key outside the declared closed surface must be rejected");
    assert!(
        matches!(
            err,
            UsageCollectorError::InvalidArgument {
                reason: ValidationReason::MetadataValidation,
                ..
            }
        ),
        "expected MetadataValidation, got {err:?}"
    );
    assert!(
        err.to_string().contains("region"),
        "the rejection must name the offending key: {err}"
    );
}

#[test]
fn declared_metadata_key_is_accepted() {
    let declaration = declaration_with_metadata_keys(&["region"]);
    let mut metadata = BTreeMap::new();
    metadata.insert(mk_key("region"), "eu-west-1".to_owned());
    validate_submit_record_metadata(&declaration, &metadata, DEFAULT_METADATA_SIZE_CAP_BYTES)
        .expect("a declared key must be accepted");
}

// ---------------------------------------------------------------------------
// Metadata size-cap enforcement
// (`cpt-cf-usage-collector-algo-usage-emission-metadata-size-cap-enforcement`)
//
// The serialized-size gate (`size > metadata_size_cap_bytes`) is a `>`, not
// `>=`, comparison — exactly at the cap is accepted; one byte over is
// rejected. The value length is sized off the measured single-entry
// overhead so the boundary holds regardless of `MetadataKey`'s serde shape.
// The cap is threaded in as a parameter (`Service::metadata_size_cap_bytes`,
// itself from `UsageCollectorConfig::metadata_size_cap_bytes`) rather than
// read from a hard-coded constant; these tests pin the DEFAULT cap value
// (`DEFAULT_METADATA_SIZE_CAP_BYTES`) behaves exactly as the old hard-coded
// constant did, and a separate test below pins that a non-default configured
// cap is actually honoured.
// ---------------------------------------------------------------------------

fn size_cap_declaration() -> ResolvedDeclaration {
    declaration_with_metadata_keys(&["blob"])
}

/// Serialized overhead of a one-entry `{ "blob": "" }` map, so a value can be
/// sized to land the serialized payload exactly on a cap boundary.
fn single_entry_overhead() -> usize {
    let mut probe = BTreeMap::new();
    probe.insert(mk_key("blob"), String::new());
    serde_json::to_vec(&probe)
        .expect("probe serialization is infallible")
        .len()
}

#[test]
fn metadata_one_byte_over_default_size_cap_is_rejected() {
    let declaration = size_cap_declaration();
    let mut metadata = BTreeMap::new();
    metadata.insert(
        mk_key("blob"),
        "x".repeat(DEFAULT_METADATA_SIZE_CAP_BYTES - single_entry_overhead() + 1),
    );
    let err =
        validate_submit_record_metadata(&declaration, &metadata, DEFAULT_METADATA_SIZE_CAP_BYTES)
            .expect_err("metadata one byte over the default cap must reject");
    assert!(
        matches!(
            err,
            UsageCollectorError::InvalidArgument {
                reason: ValidationReason::MetadataValidation,
                ..
            }
        ),
        "expected MetadataValidation size-cap rejection, got {err:?}",
    );
}

#[test]
fn metadata_exactly_at_default_size_cap_is_accepted() {
    let declaration = size_cap_declaration();
    let mut metadata = BTreeMap::new();
    metadata.insert(
        mk_key("blob"),
        "x".repeat(DEFAULT_METADATA_SIZE_CAP_BYTES - single_entry_overhead()),
    );
    validate_submit_record_metadata(&declaration, &metadata, DEFAULT_METADATA_SIZE_CAP_BYTES)
        .expect("metadata serialized exactly to the default cap must be accepted");
}

/// A configured cap smaller than the default MUST be honoured, not silently
/// widened back to the default — proves the parameter (not a hard-coded
/// constant) drives the check.
#[test]
fn a_configured_non_default_cap_is_honoured() {
    let declaration = size_cap_declaration();
    let mut metadata = BTreeMap::new();
    // Comfortably under the 8 KiB default cap...
    metadata.insert(mk_key("blob"), "x".repeat(64));
    validate_submit_record_metadata(&declaration, &metadata, DEFAULT_METADATA_SIZE_CAP_BYTES)
        .expect("well under the default cap must be accepted");

    // ...but over a much smaller configured cap.
    let configured_cap = 16;
    let err = validate_submit_record_metadata(&declaration, &metadata, configured_cap)
        .expect_err("a configured cap smaller than the payload must reject it");
    match err {
        UsageCollectorError::InvalidArgument {
            reason: ValidationReason::MetadataValidation,
            ref detail,
            ..
        } => {
            assert!(
                detail.contains(&configured_cap.to_string()),
                "the rejection must name the configured cap, not a hard-coded \
                 one: {detail}"
            );
        }
        other => panic!("expected MetadataValidation size-cap rejection, got {other:?}"),
    }
}
