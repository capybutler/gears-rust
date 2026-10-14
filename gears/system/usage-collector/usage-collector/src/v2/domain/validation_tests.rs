//! Unit tests for the metadata shape-validation algorithm.

use std::collections::BTreeMap;
use std::sync::Arc;
use toolkit_gts::gts_id;

use serde_json::json;
use types_registry_sdk::{GtsTypeId, GtsTypeSchema};
use usage_collector_sdk::v2::{MetadataKey, MeterTypeId, UsageCollectorError, ValidationReason};

use super::{DEFAULT_METADATA_SIZE_CAP_BYTES, validate_submit_record_metadata};
use crate::v2::domain::type_resolver::ResolvedDeclaration;

fn mk_key(value: &str) -> MetadataKey {
    MetadataKey::new(value).expect("test fixture supplies a valid metadata key")
}

const SAMPLE_METER_ID: &str = gts_id!("cf.core.uc.usage_record.v2~tenant.example._.foo.v1~");
const BASE_ID: &str = gts_id!("cf.core.uc.usage_record.v2~");

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

/// A declaration whose single declared key `region` carries a per-value
/// constraint, so a *declared* key can be made to fail on its value alone —
/// the case that must not be confused with an undeclared key.
fn declaration_with_constrained_metadata() -> ResolvedDeclaration {
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
                    "properties": {
                        "region": { "type": "string", "minLength": 1 }
                    }
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
//
// The REASON CODE is pinned here, not just the message. `usage-collector-v2.yaml`
// publishes `UNKNOWN_METADATA_KEY` on `metadata` as the ingest answer for a key
// the resolved declaration does not carry, and `METADATA_VALIDATION` as the
// serialized-size cap alone. An earlier revision of this test asserted
// `MetadataValidation` here, which is what let the two drift apart unnoticed:
// swapping the arm in `domain::error`'s lift back to `MetadataValidation` MUST
// fail this test.
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
    match err {
        UsageCollectorError::InvalidArgument {
            reason: ValidationReason::UnknownMetadataKey,
            ref field,
            ref resource_name,
            ref detail,
            ..
        } => {
            assert_eq!(field, "metadata", "attributed to the record's metadata");
            assert_eq!(
                resource_name.as_deref(),
                Some(SAMPLE_METER_ID),
                "the contract puts the offending `gts_type_id` on `resource_name`"
            );
            assert!(
                detail.contains("region"),
                "the rejection must name the offending key: {detail}"
            );
        }
        other => unreachable!("expected UnknownMetadataKey, got {other:?}"),
    }
}

/// Every undeclared key is named in one rejection, not just the first — the
/// aggregation `CompiledMetadataSchema` performs survives the reason-code
/// classification.
#[test]
fn every_undeclared_key_is_named_in_one_unknown_metadata_key_rejection() {
    let declaration = declaration_with_metadata_keys(&["region"]);
    let mut bad = BTreeMap::new();
    bad.insert(mk_key("tier"), "gold".to_owned());
    bad.insert(mk_key("zone"), "a".to_owned());
    let err = validate_submit_record_metadata(&declaration, &bad, DEFAULT_METADATA_SIZE_CAP_BYTES)
        .expect_err("undeclared keys must be rejected");
    match err {
        UsageCollectorError::InvalidArgument {
            reason: ValidationReason::UnknownMetadataKey,
            ref detail,
            ..
        } => {
            assert!(
                detail.contains("tier") && detail.contains("zone"),
                "both undeclared keys must be named in one round-trip: {detail}"
            );
        }
        other => unreachable!("expected UnknownMetadataKey, got {other:?}"),
    }
}

/// A *declared* key whose value breaks a declared constraint is NOT an unknown
/// key, and is not the size cap either. `METADATA_VALIDATION` is published as
/// the size cap alone, so this falls to the contract's documented catch-all.
#[test]
fn declared_value_constraint_violation_is_the_generic_validation_reason() {
    let declaration = declaration_with_constrained_metadata();
    let mut bad = BTreeMap::new();
    bad.insert(mk_key("region"), String::new());
    let err = validate_submit_record_metadata(&declaration, &bad, DEFAULT_METADATA_SIZE_CAP_BYTES)
        .expect_err("an empty value violates the declared minLength");
    match err {
        UsageCollectorError::InvalidArgument {
            reason: ValidationReason::Validation,
            ref field,
            ..
        } => assert_eq!(field, "metadata"),
        other => unreachable!("expected the generic Validation reason, got {other:?}"),
    }
}

/// Mixed violations are not purely an unknown-key rejection, so they take the
/// catch-all — and still report both problems at once.
#[test]
fn mixed_violations_take_the_generic_reason_and_report_every_one() {
    let declaration = declaration_with_constrained_metadata();
    let mut bad = BTreeMap::new();
    bad.insert(mk_key("region"), String::new());
    bad.insert(mk_key("tier"), "gold".to_owned());
    let err = validate_submit_record_metadata(&declaration, &bad, DEFAULT_METADATA_SIZE_CAP_BYTES)
        .expect_err("both violations must be rejected");
    match err {
        UsageCollectorError::InvalidArgument {
            reason: ValidationReason::Validation,
            ref detail,
            ..
        } => {
            assert!(
                detail.contains("tier"),
                "the undeclared key must still be named: {detail}"
            );
            assert!(
                detail.contains("shorter than 1 character"),
                "the declared-constraint violation must also be reported: {detail}"
            );
        }
        other => unreachable!("expected the generic Validation reason, got {other:?}"),
    }
}

#[test]
fn declared_metadata_key_is_accepted() {
    let declaration = declaration_with_metadata_keys(&["region"]);
    let mut metadata = BTreeMap::new();
    metadata.insert(mk_key("region"), "eu-west-1".to_owned());
    validate_submit_record_metadata(&declaration, &metadata, DEFAULT_METADATA_SIZE_CAP_BYTES)
        .expect("a declared key must be accepted");
}

// Metadata size-cap enforcement (`cpt-cf-usage-collector-algo-metadata-validation`)
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
        other => unreachable!("expected MetadataValidation size-cap rejection, got {other:?}"),
    }
}
