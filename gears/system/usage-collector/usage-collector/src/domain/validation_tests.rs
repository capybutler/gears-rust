//! Unit tests for the shape-validation algorithm.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use toolkit_gts::gts_id;

use rust_decimal::Decimal;
use serde_json::json;
use time::OffsetDateTime;
use types_registry_sdk::{GtsTypeId, GtsTypeSchema};
use usage_collector_sdk::{
    ConflictReason, IdempotencyKey, MetadataKey, MeterTypeId, ResourceRef, SubjectRef,
    UsageCollectorError, UsageRecord, UsageRecordStatus, ValidationReason,
};
use uuid::Uuid;

use super::{
    DEFAULT_METADATA_SIZE_CAP_BYTES, SemanticsOutcome, metadata_fields_from_wire,
    validate_record_semantics, validate_submit_record_metadata, verify_l1_corrects_id,
};
use crate::domain::type_resolver::ResolvedDeclaration;

fn mk_key(value: &str) -> MetadataKey {
    MetadataKey::new(value).expect("test fixture supplies a valid metadata key")
}

fn mk_keys<const N: usize>(values: [&str; N]) -> BTreeSet<MetadataKey> {
    values.into_iter().map(mk_key).collect()
}

const SAMPLE_METER_ID: &str = gts_id!("cf.core.uc.usage_record.v1~tenant.example._.foo.v1~");
const SAMPLE_OTHER_METER_ID: &str = gts_id!("cf.core.uc.usage_record.v1~tenant.example._.bar.v1~");
const BASE_ID: &str = gts_id!("cf.core.uc.usage_record.v1~");

/// The ordinary-record fixtures' meter reference. Named for the pre-Task-9
/// counter/gauge distinction the fixtures used to exercise; `UsageRecord` no
/// longer carries a `kind`, but the two distinct meter ids are still needed
/// to exercise the L1 cross-meter scope check below.
fn counter_id() -> MeterTypeId {
    meter_id()
}

fn gauge_id() -> MeterTypeId {
    MeterTypeId::new(SAMPLE_OTHER_METER_ID).expect("valid meter type id")
}

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

#[test]
fn metadata_fields_from_wire_empty_vec_yields_empty_set() {
    let set = metadata_fields_from_wire(Vec::new()).expect("empty input accepted");
    assert!(set.is_empty());
}

// Repointed at the resolved declaration (Task 9): the closed-shape check now
// runs against `ResolvedDeclaration::metadata_schema`, not a plugin-owned
// `UsageType.metadata_fields` set — but the intent this test pins ("an
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

#[test]
fn metadata_fields_from_wire_legitimate_passes() {
    let set =
        metadata_fields_from_wire(vec!["region".to_owned()]).expect("single declared key accepted");
    assert_eq!(set, mk_keys(["region"]));
}

// `inst-algo-shape-invalid-metadata-fields` — duplicate entry at index `2`
// returns `DuplicateMetadataField` carrying the offending index.
#[test]
fn metadata_fields_from_wire_duplicate_returns_metadata_validation_error() {
    let err = metadata_fields_from_wire(vec![
        "region".to_owned(),
        "az".to_owned(),
        "region".to_owned(),
    ])
    .expect_err("duplicate metadata field must be rejected");
    assert!(
        matches!(
            err,
            UsageCollectorError::InvalidArgument {
                reason: ValidationReason::MetadataFieldDuplicate,
                ref field,
                ..
            } if field == "metadata_fields[2]"
        ),
        "expected DuplicateMetadataField {{ index: 2 }}, got {err:?}"
    );
}

// `inst-algo-shape-invalid-metadata-fields` — first empty string at index `1`
// is rejected before any duplicate-detection pass continues.
#[test]
fn metadata_fields_from_wire_empty_string_rejected() {
    let err = metadata_fields_from_wire(vec!["region".to_owned(), String::new()])
        .expect_err("empty metadata field must be rejected");
    match err {
        UsageCollectorError::InvalidArgument {
            reason: ValidationReason::MetadataFieldEmptyString,
            ref field,
            ..
        } => {
            assert_eq!(field, "metadata_fields[1]", "expected index 1, got {field}");
        }
        other => panic!("expected InvalidMetadataField, got {other:?}"),
    }
}

// `MetadataKey::new` rejects NUL bytes — wire-shape conversion surfaces
// them as a typed `InvalidMetadataField` alongside the other malformed-key
// outcomes, with the offending key's index attached.
#[test]
fn metadata_fields_from_wire_nul_byte_rejected() {
    let err = metadata_fields_from_wire(vec!["bad\0key".to_owned()])
        .expect_err("NUL byte in metadata key must be rejected");
    assert!(
        matches!(
            err,
            UsageCollectorError::InvalidArgument {
                reason: ValidationReason::MetadataFieldInvalidKey,
                ref field,
                ..
            } if field == "metadata_fields[0]"
        ),
        "expected InvalidMetadataField {{ index: 0, .. }}, got {err:?}"
    );
}

// ---------------------------------------------------------------------------
// Metadata size-cap enforcement
// (`cpt-cf-usage-collector-algo-usage-emission-metadata-size-cap-enforcement`)
//
// The serialized-size gate (`size > metadata_size_cap_bytes`) is a `>`, not
// `>=`, comparison — exactly at the cap is accepted; one byte over is
// rejected. The value length is sized off the measured single-entry
// overhead so the boundary holds regardless of `MetadataKey`'s serde shape.
// Task 9 threads the cap in as a parameter (`Service::metadata_size_cap_bytes`,
// itself from `UsageCollectorConfig::metadata_size_cap_bytes`) rather than
// reading a hard-coded constant; these tests pin the DEFAULT cap value
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

// ---------------------------------------------------------------------------
// Semantics-enforcement algorithm
// (`cpt-cf-usage-collector-algo-usage-emission-semantics-enforcement-on-ingest-v2`)
//
// Pre-Task-9 this section covered every cell of a four-cell
// `(MetricSemantics × corrects_id presence)` value matrix keyed off a
// plugin-owned `UsageType.kind`. That matrix — and the six tests that pinned
// its gauge/counter value-sign cells — is deleted outright along with the
// `kind` it was keyed on: `validate_record_semantics` no longer takes a
// `UsageType` at all, so there is no more (kind, op) disagreement left to
// assert on (see that function's doc comment). What remains is the L1
// referential check (`verify_l1_corrects_id`, unaffected by this change) plus
// the two tests below pinning `validate_record_semantics`'s new, simpler
// contract: presence of `corrects_id` alone decides `Valid` vs
// `NeedsL1Lookup`, independent of the record's value sign.
// ---------------------------------------------------------------------------

fn ordinary_counter_record(value: Decimal) -> UsageRecord {
    UsageRecord {
        id: Uuid::new_v4(),
        gts_type_id: counter_id(),
        tenant_id: Uuid::from_u128(1),
        resource_ref: ResourceRef::new("rsc-1", "compute.vm").expect("valid resource ref"),
        subject_ref: None,
        metadata: BTreeMap::new(),
        value,
        idempotency_key: IdempotencyKey::new(format!("idem-validation-test-{value}"))
            .expect("valid idempotency key"),
        corrects_id: None,
        status: UsageRecordStatus::Active,
        created_at: OffsetDateTime::UNIX_EPOCH,
    }
}

fn counter_compensation_record(value: Decimal, corrects_id: Uuid) -> UsageRecord {
    let mut record = ordinary_counter_record(value);
    record.corrects_id = Some(corrects_id);
    record
}

fn referenced_ordinary_row(tenant: Uuid) -> UsageRecord {
    let mut record = ordinary_counter_record(Decimal::from(10));
    record.tenant_id = tenant;
    record
}

#[test]
fn record_without_corrects_id_is_valid_regardless_of_value_sign() {
    // No caller-visible `kind` survives to gate a counter/gauge value-sign
    // rule, so a record with no `corrects_id` is `Valid` whatever its value's
    // sign — negative included, which the pre-Task-9 counter rule used to
    // reject.
    for value in [Decimal::from(-9999), Decimal::ZERO, Decimal::from(42)] {
        let record = ordinary_counter_record(value);
        assert_eq!(
            validate_record_semantics(&record),
            SemanticsOutcome::Valid,
            "value {value} without corrects_id must be Valid",
        );
    }
}

#[test]
fn record_with_corrects_id_needs_l1_lookup_regardless_of_value_sign() {
    // Likewise, presence of `corrects_id` alone routes to the L1 detour now —
    // the pre-Task-9 "counter compensation value must be negative" rule (and
    // the gauge-compensation rejection) is gone.
    for value in [Decimal::from(-5), Decimal::ZERO, Decimal::from(5)] {
        let corrects_id = Uuid::new_v4();
        let record = counter_compensation_record(value, corrects_id);
        assert_eq!(
            validate_record_semantics(&record),
            SemanticsOutcome::NeedsL1Lookup { corrects_id },
            "value {value} with corrects_id must need an L1 lookup",
        );
    }
}

// The `corrects_id not found` translation lives at the service-layer call
// site (`Service::create_usage_record` / `create_usage_records`) where the
// plugin's `UsageRecordNotFound` is re-classified as `CorrectsIdNotFound`;
// `verify_l1_corrects_id` only sees an existing row.

#[test]
fn l1_referenced_row_that_is_compensation_is_rejected() {
    let record = counter_compensation_record(Decimal::from(-1), Uuid::new_v4());
    let mut referenced = referenced_ordinary_row(record.tenant_id);
    referenced.corrects_id = Some(Uuid::new_v4());
    let err = verify_l1_corrects_id(
        &record,
        record.corrects_id.expect("test fixture sets corrects_id"),
        &referenced,
    )
    .expect_err("compensation target rejected");
    assert!(matches!(
        err,
        UsageCollectorError::Conflict {
            reason: ConflictReason::CorrectsIdTargetsCompensation,
            ..
        }
    ));
}

#[test]
fn l1_cross_tenant_reference_is_rejected_as_wrong_scope() {
    let record = counter_compensation_record(Decimal::from(-1), Uuid::new_v4());
    let referenced = referenced_ordinary_row(Uuid::from_u128(42));
    let err = verify_l1_corrects_id(
        &record,
        record.corrects_id.expect("test fixture sets corrects_id"),
        &referenced,
    )
    .expect_err("cross-tenant rejected");
    assert!(matches!(
        err,
        UsageCollectorError::Conflict {
            reason: ConflictReason::CorrectsIdWrongScope,
            ..
        }
    ));
}

#[test]
fn l1_cross_usage_type_reference_is_rejected_as_wrong_scope() {
    let record = counter_compensation_record(Decimal::from(-1), Uuid::new_v4());
    let mut referenced = referenced_ordinary_row(record.tenant_id);
    referenced.gts_type_id = gauge_id();
    let err = verify_l1_corrects_id(
        &record,
        record.corrects_id.expect("test fixture sets corrects_id"),
        &referenced,
    )
    .expect_err("cross-usage-type rejected");
    assert!(matches!(
        err,
        UsageCollectorError::Conflict {
            reason: ConflictReason::CorrectsIdWrongScope,
            ..
        }
    ));
}

#[test]
fn l1_different_resource_is_rejected_as_wrong_scope() {
    let record = counter_compensation_record(Decimal::from(-1), Uuid::new_v4());
    let mut referenced = referenced_ordinary_row(record.tenant_id);
    referenced.resource_ref =
        ResourceRef::new("rsc-other", "compute.vm").expect("valid resource ref");
    let err = verify_l1_corrects_id(
        &record,
        record.corrects_id.expect("test fixture sets corrects_id"),
        &referenced,
    )
    .expect_err("cross-resource compensation rejected");
    assert!(matches!(
        err,
        UsageCollectorError::Conflict {
            reason: ConflictReason::CorrectsIdWrongScope,
            ..
        }
    ));
}

#[test]
fn l1_subject_presence_mismatch_is_rejected_as_wrong_scope() {
    let record = counter_compensation_record(Decimal::from(-1), Uuid::new_v4());
    let mut referenced = referenced_ordinary_row(record.tenant_id);
    referenced.subject_ref =
        Some(SubjectRef::new("user-1", Option::<&str>::None).expect("valid subject ref"));
    let err = verify_l1_corrects_id(
        &record,
        record.corrects_id.expect("test fixture sets corrects_id"),
        &referenced,
    )
    .expect_err("subject-presence mismatch rejected");
    assert!(matches!(
        err,
        UsageCollectorError::Conflict {
            reason: ConflictReason::CorrectsIdWrongScope,
            ..
        }
    ));
}

#[test]
fn l1_different_subject_is_rejected_as_wrong_scope() {
    let subject_a = SubjectRef::new("user-a", Some("end_user")).expect("valid subject ref");
    let subject_b = SubjectRef::new("user-b", Some("end_user")).expect("valid subject ref");
    let mut record = counter_compensation_record(Decimal::from(-1), Uuid::new_v4());
    record.subject_ref = Some(subject_a);
    let mut referenced = referenced_ordinary_row(record.tenant_id);
    referenced.subject_ref = Some(subject_b);
    let err = verify_l1_corrects_id(
        &record,
        record.corrects_id.expect("test fixture sets corrects_id"),
        &referenced,
    )
    .expect_err("cross-subject compensation rejected");
    assert!(matches!(
        err,
        UsageCollectorError::Conflict {
            reason: ConflictReason::CorrectsIdWrongScope,
            ..
        }
    ));
}

#[test]
fn l1_matching_subject_passes_referential_check() {
    let subject = SubjectRef::new("user-1", Some("end_user")).expect("valid subject ref");
    let mut record = counter_compensation_record(Decimal::from(-1), Uuid::new_v4());
    record.subject_ref = Some(subject.clone());
    let mut referenced = referenced_ordinary_row(record.tenant_id);
    referenced.subject_ref = Some(subject);
    assert!(
        verify_l1_corrects_id(
            &record,
            record.corrects_id.expect("test fixture sets corrects_id"),
            &referenced
        )
        .is_ok()
    );
}

#[test]
fn l1_inactive_referenced_row_is_rejected() {
    let record = counter_compensation_record(Decimal::from(-1), Uuid::new_v4());
    let mut referenced = referenced_ordinary_row(record.tenant_id);
    referenced.status = UsageRecordStatus::Inactive;
    let err = verify_l1_corrects_id(
        &record,
        record.corrects_id.expect("test fixture sets corrects_id"),
        &referenced,
    )
    .expect_err("inactive rejected");
    assert!(matches!(
        err,
        UsageCollectorError::Conflict {
            reason: ConflictReason::CorrectsIdInactive,
            ..
        }
    ));
}

#[test]
fn l1_active_in_scope_ordinary_row_passes_referential_check() {
    let record = counter_compensation_record(Decimal::from(-1), Uuid::new_v4());
    let referenced = referenced_ordinary_row(record.tenant_id);
    assert!(
        verify_l1_corrects_id(
            &record,
            record.corrects_id.expect("test fixture sets corrects_id"),
            &referenced
        )
        .is_ok()
    );
}
