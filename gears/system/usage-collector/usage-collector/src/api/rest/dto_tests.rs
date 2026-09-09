//! Wire-shape tests for the foundation REST DTOs.
//!
//! Pins the serde envelopes the OAS yaml relies on plus the
//! DTO ↔ SDK newtype boundary: the register-request DTO accepts any
//! string at deserialize so the handler can synthesise the canonical
//! `invalid_base_gts_id` `Problem` envelope on rejection rather than
//! surfacing axum's default `text/plain` 422.

use std::collections::BTreeMap;
use std::str::FromStr;
use toolkit_gts::gts_uri;

use rust_decimal::Decimal;
use time::OffsetDateTime;
use toolkit_canonical_errors::Problem;
use toolkit_gts::gts_id;
use usage_collector_sdk::{
    IdempotencyKey, Invalidation, MeterTypeId, ReasonCode, RecordOrigin, ResourceRef, UsageRecord,
};
use uuid::Uuid;

use super::{
    AggregationBucketDto, CreateUsageRecordRequest, CreateUsageRecordResultDto, UsageRecordDto,
};

const SAMPLE_METER_TYPE_ID: &str =
    gts_id!("cf.core.uc.usage_record.v1~cf.mini_chat._.tokens_consumed.v1~");

const SAMPLE_WINDOW_START_RFC3339: &str = "2026-06-11T12:34:56Z";
const SAMPLE_WINDOW_END_RFC3339: &str = "2026-06-11T13:34:56Z";
const SAMPLE_RECORD_VALUE: &str = "42.5";
const SAMPLE_IDEMPOTENCY_KEY: &str = "idem-dto-tests-1";
const SAMPLE_REASON_CODE: &str = "emitter_duplicate";
/// Wire spelling of [`sample_target_uuid`].
const SAMPLE_TARGET_ID: &str = "33333333-3333-3333-3333-333333333333";

fn sample_record_uuid() -> Uuid {
    Uuid::from_u128(0x1111_1111_1111_1111_1111_1111_1111_1111_u128)
}

fn sample_tenant_uuid() -> Uuid {
    Uuid::from_u128(0x2222_2222_2222_2222_2222_2222_2222_2222_u128)
}

/// The entry a withdrawal fixture names. Distinct from
/// [`sample_record_uuid`] so a projection that echoed the entry's own id
/// into `invalidates` fails rather than passing by coincidence.
fn sample_target_uuid() -> Uuid {
    Uuid::from_u128(0x3333_3333_3333_3333_3333_3333_3333_3333_u128)
}

/// An ordinary measurement: it carries no withdrawal, so its derived
/// entry type is `record`.
fn sample_persisted_record() -> UsageRecord {
    sample_persisted_entry(None, RecordOrigin::Live)
}

/// A withdrawal of [`SAMPLE_TARGET_ID`]: same payload, plus the one field
/// whose presence makes the entry an invalidation.
fn sample_persisted_invalidation() -> UsageRecord {
    sample_persisted_entry(
        Some(Invalidation {
            target: sample_target_uuid(),
            reason: ReasonCode::new(SAMPLE_REASON_CODE).expect("valid reason code"),
        }),
        RecordOrigin::Live,
    )
}

/// An entry admitted by the backfill route. Separate from
/// [`sample_persisted_record`] because `origin` is the one field of the
/// response projection whose two values are not interchangeable to a
/// consumer: one is current consumption and the other is imported history.
fn sample_backfilled_record() -> UsageRecord {
    sample_persisted_entry(None, RecordOrigin::Backfill)
}

fn sample_persisted_entry(invalidation: Option<Invalidation>, origin: RecordOrigin) -> UsageRecord {
    UsageRecord {
        id: sample_record_uuid(),
        gts_type_id: MeterTypeId::new(SAMPLE_METER_TYPE_ID).expect("valid gts_type_id"),
        tenant_id: sample_tenant_uuid(),
        resource_ref: ResourceRef::new("rsc-dto", "compute.vm").expect("valid resource ref"),
        subject_ref: None,
        metadata: BTreeMap::new(),
        value: Decimal::from_str(SAMPLE_RECORD_VALUE).expect("valid decimal"),
        idempotency_key: IdempotencyKey::new(SAMPLE_IDEMPOTENCY_KEY).expect("valid idem key"),
        origin,
        invalidation,
        window_start: parse_rfc3339(SAMPLE_WINDOW_START_RFC3339),
        window_end: parse_rfc3339(SAMPLE_WINDOW_END_RFC3339),
    }
}

fn parse_rfc3339(raw: &str) -> OffsetDateTime {
    OffsetDateTime::parse(raw, &time::format_description::well_known::Rfc3339)
        .expect("RFC 3339 fixture parses")
}

fn sample_usage_record_dto() -> UsageRecordDto {
    UsageRecordDto::from(sample_persisted_record())
}

#[test]
fn record_dto_names_the_type_reference_gts_type_id() {
    // usage-collector-v1.yaml renamed gts_id to gts_type_id on every shape.
    let json = serde_json::to_value(sample_usage_record_dto()).unwrap();
    assert!(
        json.get("gts_type_id").is_some(),
        "wire field is gts_type_id"
    );
    assert!(json.get("gts_id").is_none(), "the old name must be gone");
}

// ---------------------------------------------------------------------------
// CreateUsageRecordRequest wire-contract pins.
//
// The handler tests in `handlers::usage_records_tests` construct
// `CreateUsageRecordRequest` directly with Rust values, bypassing serde — so a
// regression that drops `deny_unknown_fields`, flips `value` to numeric
// deserialization, drops the RFC 3339 wrapper on a period bound, or removes the
// `Option<…>` / `BTreeMap::is_empty` defaults would still pass CI. These tests
// pin each of those serde attributes directly against the wire shape.
// ---------------------------------------------------------------------------

fn minimal_create_record_json() -> serde_json::Value {
    serde_json::json!({
        "gts_type_id": SAMPLE_METER_TYPE_ID,
        "tenant_id": sample_tenant_uuid().to_string(),
        "resource_ref": {
            "resource_id": "rsc-dto",
            "resource_type": "compute.vm",
        },
        "value": SAMPLE_RECORD_VALUE,
        "idempotency_key": SAMPLE_IDEMPOTENCY_KEY,
        "window_start": SAMPLE_WINDOW_START_RFC3339,
        "window_end": SAMPLE_WINDOW_END_RFC3339,
    })
}

#[test]
fn create_usage_record_request_rejects_unknown_fields() {
    // Pin `#[serde(deny_unknown_fields)]` on the per-record request. A future
    // accidental drop of the attribute would silently accept extra wire
    // members and let unrecognised fields through to the handler unnoticed.
    let mut json = minimal_create_record_json();
    json.as_object_mut()
        .expect("object")
        .insert("extra".to_owned(), serde_json::json!("nope"));
    let err = serde_json::from_value::<CreateUsageRecordRequest>(json)
        .expect_err("deny_unknown_fields must reject extra members");
    assert!(
        err.to_string().contains("extra"),
        "deserialize error MUST identify the unknown field (got `{err}`)",
    );
}

#[test]
fn create_request_rejects_client_supplied_id() {
    // `id` is server-derived; deny_unknown_fields must reject a client-sent id.
    let json = serde_json::json!({
        "id": "11111111-1111-1111-1111-111111111111",
        "gts_type_id": SAMPLE_METER_TYPE_ID,
        "tenant_id": "11111111-1111-1111-1111-111111111111",
        "resource_ref": { "resource_id": "r1", "resource_type": "compute.vm" },
        "value": "1",
        "idempotency_key": "idem-1",
        "window_start": "2026-07-07T00:00:00Z",
        "window_end": "2026-07-07T01:00:00Z"
    });
    let err = serde_json::from_value::<super::CreateUsageRecordRequest>(json).unwrap_err();
    assert!(err.to_string().contains("unknown field"), "got: {err}");
}

#[test]
fn create_usage_record_request_deserialises_value_as_string_only() {
    // Pin `#[serde(with = "rust_decimal::serde::str")]` on `value`: the wire
    // carries the decimal as a JSON string. Accepting a JSON number on input
    // would let callers ship `value: 0.1` and silently lose precision.
    let req: CreateUsageRecordRequest =
        serde_json::from_value(minimal_create_record_json()).expect("string-form value parses");
    assert_eq!(
        req.value,
        Decimal::from_str(SAMPLE_RECORD_VALUE).expect("fixture decimal parses"),
    );

    let mut numeric_json = minimal_create_record_json();
    numeric_json
        .as_object_mut()
        .expect("object")
        .insert("value".to_owned(), serde_json::json!(42.5));
    let err = serde_json::from_value::<CreateUsageRecordRequest>(numeric_json)
        .expect_err("numeric `value` MUST be rejected - wire contract is string-only");
    assert!(
        err.to_string().contains("Decimal"),
        "deserialize error MUST identify the `Decimal` type the codec expected \
         (got `{err}`)",
    );
}

#[test]
fn create_usage_record_request_deserialises_both_period_bounds_as_rfc3339_only() {
    // Pin `#[serde(with = "time::serde::rfc3339")]` on each covered-period
    // bound: the wire carries both as JSON strings in RFC 3339 form. A
    // regression that swapped either for the default `OffsetDateTime` serde
    // codec would accept a numeric Unix timestamp and break every existing
    // client. Both bounds are checked, because the attribute is per-field
    // and a copy-paste that dropped it from one would not surface anywhere
    // else.
    let req: CreateUsageRecordRequest = serde_json::from_value(minimal_create_record_json())
        .expect("RFC 3339 strings for both bounds parse");
    assert_eq!(req.window_start, parse_rfc3339(SAMPLE_WINDOW_START_RFC3339));
    assert_eq!(req.window_end, parse_rfc3339(SAMPLE_WINDOW_END_RFC3339));

    for bound in ["window_start", "window_end"] {
        let mut numeric_json = minimal_create_record_json();
        numeric_json
            .as_object_mut()
            .expect("object")
            .insert(bound.to_owned(), serde_json::json!(1_700_000_000_i64));
        let err = serde_json::from_value::<CreateUsageRecordRequest>(numeric_json)
            .expect_err("a non-string period bound MUST be rejected");
        assert!(
            err.to_string().contains("RFC3339"),
            "deserialize error for `{bound}` MUST identify the RFC 3339 codec \
             the field expected (got `{err}`)",
        );
    }
}

#[test]
fn create_usage_record_request_requires_both_period_bounds() {
    // Neither bound is optional: a submission missing one carries no
    // well-formed covered period, and there is no default that could stand
    // in for it. Pins that no `#[serde(default)]` slipped onto either.
    for missing in ["window_start", "window_end"] {
        let mut json = minimal_create_record_json();
        json.as_object_mut()
            .expect("object")
            .remove(missing)
            .expect("fixture carries the field");
        let err = serde_json::from_value::<CreateUsageRecordRequest>(json)
            .expect_err("a missing period bound MUST be rejected");
        assert!(
            err.to_string().contains(missing),
            "deserialize error MUST name the missing bound `{missing}` (got `{err}`)",
        );
    }
}

#[test]
fn create_usage_record_request_optional_subject_ref_defaults_to_none() {
    // The minimal fixture omits `subject_ref` entirely; pin `#[serde(default,
    // skip_serializing_if = "Option::is_none")]` on the request side by
    // proving the field deserialises to `None` when absent.
    let req: CreateUsageRecordRequest = serde_json::from_value(minimal_create_record_json())
        .expect("missing subject_ref must default to None");
    assert!(
        req.subject_ref.is_none(),
        "absent `subject_ref` MUST deserialise to None (got {:?})",
        req.subject_ref,
    );
}

#[test]
fn create_usage_record_request_optional_metadata_defaults_to_empty_map() {
    // The minimal fixture omits `metadata` entirely; pin `#[serde(default,
    // skip_serializing_if = "BTreeMap::is_empty")]` on the request side by
    // proving the field deserialises to an empty map when absent.
    let req: CreateUsageRecordRequest = serde_json::from_value(minimal_create_record_json())
        .expect("missing metadata must default to empty map");
    assert!(
        req.metadata.is_empty(),
        "absent `metadata` MUST deserialise to an empty BTreeMap (got {:?})",
        req.metadata,
    );
}

#[test]
fn create_usage_record_request_optional_correction_fields_default_to_none() {
    // Pin `#[serde(default)]` on both halves of the correction reference:
    // an ordinary submission carries neither, so a body omitting both must
    // deserialize rather than fail as a missing field.
    let req: CreateUsageRecordRequest = serde_json::from_value(minimal_create_record_json())
        .expect("a body omitting both correction fields must deserialize");
    assert!(
        req.invalidates.is_none(),
        "absent `invalidates` MUST deserialise to None (got {:?})",
        req.invalidates,
    );
    assert!(
        req.reason_code.is_none(),
        "absent `reason_code` MUST deserialise to None (got {:?})",
        req.reason_code,
    );
}

#[test]
fn create_usage_record_request_carries_the_correction_pair_flat() {
    // The pair stays two flat sibling properties on this shape because the
    // OAS declares it that way and `api_dto` emits this struct into the
    // served document. The domain folds them into one field; folding them
    // here would change the published schema.
    let mut json = minimal_create_record_json();
    let obj = json.as_object_mut().expect("object");
    obj.insert(
        "invalidates".to_owned(),
        serde_json::json!(SAMPLE_TARGET_ID),
    );
    obj.insert(
        "reason_code".to_owned(),
        serde_json::json!(SAMPLE_REASON_CODE),
    );
    let req: CreateUsageRecordRequest =
        serde_json::from_value(json).expect("a withdrawal body deserializes");
    assert_eq!(req.invalidates, Some(sample_target_uuid()));
    assert_eq!(req.reason_code.as_deref(), Some(SAMPLE_REASON_CODE));
}

#[test]
fn a_submitted_entry_type_is_refused_as_an_unknown_field() {
    // `entry_type` is `readOnly` on the wire and appears on the read shape
    // alone. `deny_unknown_fields` already refuses one here; this pins that
    // the ingestion shape stays discriminator-free, so a marker can never
    // disagree with the payload it marks.
    let mut json = minimal_create_record_json();
    json.as_object_mut()
        .expect("object")
        .insert("entry_type".to_owned(), serde_json::json!("invalidation"));
    let err = serde_json::from_value::<CreateUsageRecordRequest>(json)
        .expect_err("the ingestion shape accepts no discriminator");
    assert!(
        err.to_string().contains("entry_type"),
        "the failure MUST name the discriminator it refused (got `{err}`)",
    );
}

#[test]
fn create_usage_record_request_accepts_exactly_the_declared_wire_keys() {
    // The literal complement of `deny_unknown_fields`: that attribute
    // proves nothing undeclared gets in, and this proves every declared key
    // does. A rename applied to both sides of a codec round-trips
    // perfectly and still breaks every client, so the accepted key set is
    // asserted against literals rather than against the struct.
    let mut json = minimal_create_record_json();
    {
        let obj = json.as_object_mut().expect("object");
        obj.insert(
            "subject_ref".to_owned(),
            serde_json::json!({ "subject_id": "sub-dto", "subject_type": "user" }),
        );
        obj.insert("metadata".to_owned(), serde_json::json!({ "region": "eu" }));
        obj.insert(
            "invalidates".to_owned(),
            serde_json::json!(SAMPLE_TARGET_ID),
        );
        obj.insert(
            "reason_code".to_owned(),
            serde_json::json!(SAMPLE_REASON_CODE),
        );
    }
    let mut keys: Vec<&str> = json
        .as_object()
        .expect("object")
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        vec![
            "gts_type_id",
            "idempotency_key",
            "invalidates",
            "metadata",
            "reason_code",
            "resource_ref",
            "subject_ref",
            "tenant_id",
            "value",
            "window_end",
            "window_start",
        ],
        "test premise: the fixture spells every declared property once",
    );
    serde_json::from_value::<CreateUsageRecordRequest>(json)
        .expect("every declared wire key MUST be accepted under deny_unknown_fields");
}

// ---------------------------------------------------------------------------
// UsageRecordDto wire-contract pins.
//
// `UsageRecordDto` is the response projection of `UsageRecord`, and it is
// what every REST response body actually serializes — the SDK's own
// `UsageRecord` never reaches an HTTP body, so the literal encoding pinned
// on the SDK shape covers none of what a client receives. The handler tests
// assert the persisted-record UUID makes it through but never check the
// entry-type / value / period-bound / metadata projections — a regression in
// any of `serde(with = "rust_decimal::serde::str")`, `serde(with =
// "time::serde::rfc3339")`, the derived `entry_type`, or the empty-metadata
// skip would not fail any existing test. The pins below cover each, and the
// key-set assertion below covers them against a literal rather than against
// the struct: a rename applied to both halves of a codec round-trips and
// still breaks every client.
// ---------------------------------------------------------------------------

#[test]
fn the_response_carries_a_derived_entry_type_and_no_status() {
    // `entry_type` is `readOnly` on the wire and derived in the domain, so
    // the projection computes it from the record's own reference — there is
    // nothing on `UsageRecord` to copy it from
    // (`cpt-cf-usage-collector-adr-append-only-invalidation`).
    let dto = UsageRecordDto::from(sample_persisted_record());
    assert_eq!(dto.entry_type, "record");
    assert_eq!(dto.invalidates, None);
    assert_eq!(dto.reason_code, None);

    let dto = UsageRecordDto::from(sample_persisted_invalidation());
    assert_eq!(dto.entry_type, "invalidation");
    assert_eq!(dto.invalidates, Some(sample_target_uuid()));
    assert_eq!(dto.reason_code.as_deref(), Some(SAMPLE_REASON_CODE));

    // No lifecycle flag survives anywhere on the wire: the model carries
    // none and there is no row to rewrite. Checked on the serialized body
    // rather than on the struct, because a field re-added under a serde
    // rename would not show up as a compile error here.
    let json =
        serde_json::to_value(UsageRecordDto::from(sample_persisted_record())).expect("serializes");
    assert!(json.get("status").is_none(), "got {json:?}");
    assert!(json.get("corrects_id").is_none(), "got {json:?}");
}

#[test]
fn usage_record_dto_serialises_exactly_the_declared_wire_keys() {
    // The literal key set a client receives, in both entry shapes. An
    // ordinary measurement omits every optional property; a withdrawal adds
    // exactly the correction pair. `entry_type` is `required` on the OAS
    // `UsageRecord`, so it appears in both.
    //
    // This is what the gear emits, not what `usage-collector-v1.yaml`'s
    // `UsageRecord` declares: the contract also requires `accepted_at`
    // and `acceptance_sequence`, and spells `value` as `quantity`. Those
    // gaps are still open; this assertion is what will fail when one
    // closes. `origin` was on that list until the projection started
    // carrying it, which is why it is now a key below.
    let record =
        serde_json::to_value(UsageRecordDto::from(sample_persisted_record())).expect("serializes");
    let mut keys: Vec<&str> = record
        .as_object()
        .expect("object")
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        vec![
            "entry_type",
            "gts_type_id",
            "id",
            "idempotency_key",
            "origin",
            "resource_ref",
            "tenant_id",
            "value",
            "window_end",
            "window_start",
        ],
        "an ordinary measurement's response body MUST carry exactly these keys",
    );

    let invalidation = serde_json::to_value(UsageRecordDto::from(sample_persisted_invalidation()))
        .expect("serializes");
    let mut keys: Vec<&str> = invalidation
        .as_object()
        .expect("object")
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        vec![
            "entry_type",
            "gts_type_id",
            "id",
            "idempotency_key",
            "invalidates",
            "origin",
            "reason_code",
            "resource_ref",
            "tenant_id",
            "value",
            "window_end",
            "window_start",
        ],
        "a withdrawal's response body MUST add exactly the correction pair",
    );

    assert_eq!(
        invalidation
            .get("entry_type")
            .and_then(serde_json::Value::as_str),
        Some("invalidation"),
    );
    assert_eq!(
        invalidation
            .get("invalidates")
            .and_then(serde_json::Value::as_str),
        Some(SAMPLE_TARGET_ID),
        "`invalidates` MUST be emitted as the target's uuid string",
    );
    assert_eq!(
        invalidation
            .get("reason_code")
            .and_then(serde_json::Value::as_str),
        Some(SAMPLE_REASON_CODE),
        "`reason_code` MUST be emitted as a bare string, not the newtype's \
         debug form",
    );
}

#[test]
fn the_response_projection_carries_the_origin_the_entry_was_admitted_under() {
    // `origin` is server-assigned from the route the entry arrived on, so
    // the projection must carry the stored value through rather than
    // defaulting: a consumer separates imported history from current
    // consumption on this key alone.
    let live =
        serde_json::to_value(UsageRecordDto::from(sample_persisted_record())).expect("serializes");
    assert_eq!(live.get("origin"), Some(&serde_json::json!("live")));

    let imported =
        serde_json::to_value(UsageRecordDto::from(sample_backfilled_record())).expect("serializes");
    assert_eq!(imported.get("origin"), Some(&serde_json::json!("backfill")));
}

#[test]
fn usage_record_dto_serialises_value_as_string_and_period_bounds_as_rfc3339() {
    let dto = UsageRecordDto::from(sample_persisted_record());
    let json = serde_json::to_value(&dto).expect("UsageRecordDto serialises");
    assert_eq!(
        json.get("value").and_then(serde_json::Value::as_str),
        Some(SAMPLE_RECORD_VALUE),
        "`value` MUST be emitted as a JSON string (not a number)",
    );
    assert_eq!(
        json.get("window_start").and_then(serde_json::Value::as_str),
        Some(SAMPLE_WINDOW_START_RFC3339),
        "`window_start` MUST be emitted as an RFC 3339 string",
    );
    assert_eq!(
        json.get("window_end").and_then(serde_json::Value::as_str),
        Some(SAMPLE_WINDOW_END_RFC3339),
        "`window_end` MUST be emitted as an RFC 3339 string",
    );
    assert!(
        json.get("created_at").is_none(),
        "the single instant is gone from the response projection; got {json:?}",
    );
}

#[test]
fn usage_record_dto_omits_empty_metadata_and_absent_subject_ref() {
    // Empty `metadata` / `None` `subject_ref` / an absent correction pair
    // MUST be skipped on the wire so the OAS response shape stays minimal. A
    // regression that dropped `skip_serializing_if` would surface them as
    // `metadata: {}` / `subject_ref: null` / `invalidates: null` and break
    // OAS-clients that treat absent and null as distinct.
    let dto = UsageRecordDto::from(sample_persisted_record());
    let json = serde_json::to_value(&dto).expect("UsageRecordDto serialises");
    let obj = json
        .as_object()
        .expect("UsageRecordDto serialises to an object");
    assert!(
        !obj.contains_key("metadata"),
        "empty metadata MUST be omitted (got {obj:?})",
    );
    assert!(
        !obj.contains_key("subject_ref"),
        "absent subject_ref MUST be omitted (got {obj:?})",
    );
    assert!(
        !obj.contains_key("invalidates"),
        "an absent invalidates MUST be omitted (got {obj:?})",
    );
    assert!(
        !obj.contains_key("reason_code"),
        "an absent reason_code MUST be omitted (got {obj:?})",
    );
}

// ---------------------------------------------------------------------------
// CreateUsageRecordResultDto wire-contract pin.
//
// Pin the externally-tagged `outcome` discriminator: `Accepted` MUST surface
// as `outcome: "accepted"` with sibling `index` / `record` fields, and
// `Rejected` as `outcome: "rejected"` with sibling `index` / `error` fields.
// A regression that dropped the `#[toolkit_macros::api_dto(response)]`
// snake-case rename, or flipped the tag attribute, would shift either side
// silently and break every batched-create consumer.
// ---------------------------------------------------------------------------

#[test]
fn create_usage_record_result_dto_serialises_accepted_with_lowercase_tag() {
    let dto = CreateUsageRecordResultDto::Accepted {
        index: 0,
        record: UsageRecordDto::from(sample_persisted_record()),
    };
    let json = serde_json::to_value(&dto).expect("Accepted serialises");
    let obj = json.as_object().expect("Accepted serialises to an object");
    assert_eq!(
        obj.get("outcome").and_then(serde_json::Value::as_str),
        Some("accepted"),
        "Accepted MUST tag as `outcome: \"accepted\"` (got {obj:?})",
    );
    assert_eq!(
        obj.get("index").and_then(serde_json::Value::as_u64),
        Some(0),
        "Accepted MUST carry the per-record `index` as a sibling field",
    );
    let record = obj
        .get("record")
        .and_then(serde_json::Value::as_object)
        .expect("Accepted MUST carry a `record` object sibling");
    assert_eq!(
        record.get("id").and_then(serde_json::Value::as_str),
        Some(sample_record_uuid().to_string().as_str()),
        "Accepted.record.id MUST be the persisted record's id \
         - a regression that dropped the projection would surface here",
    );
}

#[test]
fn create_usage_record_result_dto_serialises_rejected_with_lowercase_tag() {
    let problem = Problem {
        problem_type: gts_uri!("cf.core.errors.err.v1~cf.core.err.invalid_argument.v1~").to_owned(),
        title: "Invalid argument".to_owned(),
        status: Some(400),
        detail: "test rejection".to_owned(),
        instance: None,
        trace_id: None,
        context: serde_json::json!({}),
        error_code: None,
        error_domain: None,
    };
    let dto = CreateUsageRecordResultDto::Rejected {
        index: 1,
        error: problem,
    };
    let json = serde_json::to_value(&dto).expect("Rejected serialises");
    let obj = json.as_object().expect("Rejected serialises to an object");
    assert_eq!(
        obj.get("outcome").and_then(serde_json::Value::as_str),
        Some("rejected"),
        "Rejected MUST tag as `outcome: \"rejected\"` (got {obj:?})",
    );
    assert_eq!(
        obj.get("index").and_then(serde_json::Value::as_u64),
        Some(1),
        "Rejected MUST carry the per-record `index` as a sibling field",
    );
    let error = obj
        .get("error")
        .and_then(serde_json::Value::as_object)
        .expect("Rejected MUST carry an `error` Problem object sibling");
    assert_eq!(
        error.get("status").and_then(serde_json::Value::as_u64),
        Some(400),
        "Rejected.error.status MUST mirror the Problem's HTTP status \
         (a regression that flattened or shadowed the Problem fields would surface here)",
    );
}

#[test]
fn aggregation_bucket_dto_serializes_above_ceiling_value_as_plain_string() {
    use bigdecimal::BigDecimal;
    // Beyond rust_decimal's ceiling — the whole point of the widening.
    let big = "79228162514264337593543950400";
    let dto = AggregationBucketDto {
        key: vec!["eu".to_owned()],
        value: Some(big.parse::<BigDecimal>().expect("bigdecimal parses")),
    };
    let wire = serde_json::to_value(&dto).expect("serialize");
    assert_eq!(wire, serde_json::json!({ "key": ["eu"], "value": big }));
}

#[test]
fn aggregation_bucket_dto_serializes_none_value_as_null() {
    let dto = AggregationBucketDto {
        key: Vec::new(),
        value: None,
    };
    let wire = serde_json::to_value(&dto).expect("serialize");
    assert_eq!(wire, serde_json::json!({ "key": [], "value": null }));
}

// ---------------------------------------------------------------------------
// TimeRangeDto / AggregationRequestDto — the aggregate path's
// carrier for the mandatory read range.
//
// A `POST` with a declared body puts the range there
// (`AggregationRequest.time_range` in `docs/usage-collector-v1.yaml`), so
// the wire-shape guarantees live at this boundary rather than in a query
// parameter parser: absent means rejected, not unbounded.
// ---------------------------------------------------------------------------

/// A minimal well-formed aggregate request body.
fn minimal_aggregate_request_json() -> serde_json::Value {
    serde_json::json!({
        "time_range": {
            "from": SAMPLE_WINDOW_START_RFC3339,
            "to": SAMPLE_WINDOW_END_RFC3339,
        }
    })
}

#[test]
fn aggregate_request_parses_the_mandatory_time_range() {
    let req: super::AggregationRequestDto =
        serde_json::from_value(minimal_aggregate_request_json()).expect("minimal body parses");
    assert_eq!(
        req.time_range.from,
        OffsetDateTime::parse(
            SAMPLE_WINDOW_START_RFC3339,
            &time::format_description::well_known::Rfc3339,
        )
        .expect("fixture bound parses"),
    );
    assert_eq!(
        req.time_range.to,
        OffsetDateTime::parse(
            SAMPLE_WINDOW_END_RFC3339,
            &time::format_description::well_known::Rfc3339,
        )
        .expect("fixture bound parses"),
    );
    assert!(
        req.group_by.is_empty(),
        "group_by stays optional: only the range is mandatory",
    );
}

#[test]
fn aggregate_request_without_a_time_range_is_rejected() {
    // No `#[serde(default)]` on the field: the contract marks it required,
    // and a body omitting it must fail deserialization rather than reach
    // the service as an unbounded aggregation.
    let err = serde_json::from_value::<super::AggregationRequestDto>(
        serde_json::json!({ "group_by": ["resource_type"] }),
    )
    .expect_err("a body without `time_range` MUST be rejected");
    assert!(
        err.to_string().contains("time_range"),
        "the failure MUST name the missing field: {err}",
    );
}

#[test]
fn aggregate_request_rejects_an_unknown_body_field() {
    let mut json = minimal_aggregate_request_json();
    json.as_object_mut()
        .expect("object")
        .insert("op".to_owned(), serde_json::json!("SUM"));
    let err = serde_json::from_value::<super::AggregationRequestDto>(json)
        .expect_err("`deny_unknown_fields` MUST refuse an undeclared body field");
    assert!(
        err.to_string().contains("op"),
        "the failure MUST name the offending field: {err}",
    );
}

#[test]
fn time_range_dto_rejects_an_offsetless_bound() {
    // Same guarantee the raw path's `from` / `to` parser gives: RFC 3339
    // requires an offset, so a bare local time cannot be attributed to
    // whatever offset the server happened to assume.
    for bound in ["from", "to"] {
        let mut json = minimal_aggregate_request_json();
        json["time_range"][bound] = serde_json::json!("2026-06-11T12:34:56");
        assert!(
            serde_json::from_value::<super::AggregationRequestDto>(json).is_err(),
            "an offset-less `{bound}` MUST fail to deserialize rather than \
             being read in some assumed offset",
        );
    }
}

#[test]
fn time_range_dto_rejects_an_unknown_field_inside_the_range() {
    let mut json = minimal_aggregate_request_json();
    json["time_range"]["tz"] = serde_json::json!("Europe/Berlin");
    serde_json::from_value::<super::AggregationRequestDto>(json)
        .expect_err("`deny_unknown_fields` MUST refuse an undeclared range field");
}

#[test]
fn time_range_dto_projects_onto_the_sdk_range_and_rejects_an_inverted_one() {
    use usage_collector_sdk::TimeRange;

    let from = OffsetDateTime::UNIX_EPOCH;
    let to = OffsetDateTime::UNIX_EPOCH + time::Duration::hours(1);

    let range = TimeRange::try_from(super::TimeRangeDto { from, to })
        .expect("an ordered pair projects onto a TimeRange");
    assert_eq!(range.lower_inclusive(), from);
    assert_eq!(range.upper_exclusive(), to);

    // The projection is the only validation gate on this path, so the
    // inverted and empty cases must both fail here rather than reaching a
    // plugin as a guaranteed-empty (or reversed) scan.
    TimeRange::try_from(super::TimeRangeDto { from: to, to: from })
        .expect_err("an inverted body range MUST be rejected");
    TimeRange::try_from(super::TimeRangeDto { from, to: from })
        .expect_err("an empty body range MUST be rejected");
}
