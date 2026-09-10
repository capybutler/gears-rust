use std::collections::BTreeMap;

use rust_decimal::Decimal;
use serde_json::Value as JsonValue;
use time::OffsetDateTime;
use uuid::Uuid;

use usage_collector_sdk::{MetadataKey, MeterTypeId, RecordOrigin, UsageCollectorPluginError};

use super::super::entity::UsageRecordRow;
use super::{
    invalidation_from_row, invalidation_to_row, metadata_jsonb_to_map, metadata_map_to_jsonb,
    meter_type_id_from_str, parse_origin, record_row_to_model,
};

/// Assert a mapper call failed as [`UsageCollectorPluginError::Internal`].
///
/// `#[track_caller]` keeps the panic pointing at the test that called it, and
/// the message names what was expected and prints what came back — a bare
/// `assert!(matches!(..))` prints neither.
#[track_caller]
fn assert_internal<T: std::fmt::Debug>(result: Result<T, UsageCollectorPluginError>, what: &str) {
    match result {
        Err(UsageCollectorPluginError::Internal(_)) => {}
        other => panic!("{what} must be Internal (a stored-invariant break), got {other:?}"),
    }
}

// ── origin round-trip ────────────────────────────────────────────────────────

#[test]
fn parse_origin_round_trips_through_the_sdk_spelling() {
    for origin in [RecordOrigin::Live, RecordOrigin::Backfill] {
        assert_eq!(parse_origin(origin.as_str()).unwrap(), origin);
    }
}

#[test]
fn parse_origin_rejects_unknown() {
    assert_internal(parse_origin("imported"), "an origin outside the DDL CHECK");
}

// ── metadata jsonb <-> map round-trip ────────────────────────────────────────

#[test]
fn metadata_map_to_jsonb_then_back_round_trips() {
    let mut map = BTreeMap::new();
    map.insert(MetadataKey::new("region").unwrap(), "eu-west".to_owned());
    map.insert(MetadataKey::new("tier").unwrap(), "gold".to_owned());

    let json = metadata_map_to_jsonb(&map);
    let back = metadata_jsonb_to_map(json).unwrap();
    assert_eq!(back, map);
}

#[test]
fn metadata_map_to_jsonb_writes_the_key_spelling_verbatim() {
    let mut map = BTreeMap::new();
    map.insert(MetadataKey::new("region").unwrap(), "eu-west".to_owned());

    let mut expected = serde_json::Map::new();
    expected.insert("region".to_owned(), JsonValue::String("eu-west".to_owned()));

    assert_eq!(
        metadata_map_to_jsonb(&map),
        JsonValue::Object(expected),
        "the stored jsonb key is the MetadataKey verbatim; $filter and every \
         other reader of the column depend on it"
    );
}

#[test]
fn empty_metadata_round_trips() {
    let map: BTreeMap<MetadataKey, String> = BTreeMap::new();
    let json = metadata_map_to_jsonb(&map);
    assert_eq!(json, JsonValue::Object(serde_json::Map::new()));
    assert!(metadata_jsonb_to_map(json).unwrap().is_empty());
}

#[test]
fn metadata_jsonb_null_maps_to_empty() {
    assert!(metadata_jsonb_to_map(JsonValue::Null).unwrap().is_empty());
}

#[test]
fn metadata_jsonb_non_object_is_rejected() {
    assert!(metadata_jsonb_to_map(JsonValue::String("x".to_owned())).is_err());
}

#[test]
fn metadata_jsonb_non_string_value_is_rejected() {
    let mut obj = serde_json::Map::new();
    obj.insert("region".to_owned(), JsonValue::Bool(true));
    assert!(metadata_jsonb_to_map(JsonValue::Object(obj)).is_err());
}

// ── gts_type_id primitive ────────────────────────────────────────────────────

/// A well-formed meter type id: the reserved base
/// (`gts.cf.core.uc.usage_record.v1~`) plus exactly one further derivation
/// segment, `~`-terminated, which is what `MeterTypeId::new` validates.
const VALID_METER_TYPE_ID: &str = "gts.cf.core.uc.usage_record.v1~cf.compute._.vcpu_hours.v1~";

#[test]
fn meter_type_id_from_str_accepts_valid_and_rejects_invalid_as_internal() {
    assert!(meter_type_id_from_str(VALID_METER_TYPE_ID).is_ok());
    // A stored value that no longer validates is a plugin invariant break, not
    // a caller error — it MUST surface as `Internal`.
    assert_internal(
        meter_type_id_from_str("not-a-valid-meter-type-id"),
        "a stored gts_type_id that no longer validates",
    );
}

// ── invalidation pair ────────────────────────────────────────────────────────

#[test]
fn a_reason_without_a_target_is_an_invariant_break() {
    let err = invalidation_from_row(None, Some("duplicate_submission".to_owned()))
        .expect_err("half a pair must not map");

    assert!(
        matches!(err, UsageCollectorPluginError::Internal(_)),
        "a malformed stored row is a plugin invariant break, not a caller error; got {err:?}"
    );
}

#[test]
fn an_unparseable_stored_reason_is_an_invariant_break() {
    // `ReasonCode::new` rejects the empty string. The column is plain `text`
    // with no content check of its own — only the pairing constraint — so
    // the store can hold one.
    assert_internal(
        invalidation_from_row(Some(Uuid::new_v4()), Some(String::new())),
        "a complete pair whose reason_code fails ReasonCode validation",
    );
}

#[test]
fn invalidation_to_row_names_the_two_columns_it_writes() {
    // Deliberately NOT a round trip against `invalidation_from_row`: feeding
    // this function's output straight back into its inverse proves the pair is
    // self-consistent and nothing else. The insert binds these two values into
    // `invalidates` and `reason_code`, so the test names them.
    let target = Uuid::from_u128(0x5115_0000_0000_0051);
    let invalidation = usage_collector_sdk::Invalidation {
        target,
        reason: usage_collector_sdk::ReasonCode::new("duplicate_submission")
            .expect("a valid reason code"),
    };

    assert_eq!(
        invalidation_to_row(Some(&invalidation)),
        (Some(target), Some("duplicate_submission")),
        "a present withdrawal writes both columns: the target it names, and the reason it carries"
    );
    assert_eq!(
        invalidation_to_row(None),
        (None, None),
        "an ordinary measurement writes neither column"
    );
}

// ── record row -> model ──────────────────────────────────────────────────────

fn valid_metadata_json() -> JsonValue {
    let mut obj = serde_json::Map::new();
    obj.insert("region".to_owned(), JsonValue::String("eu-west".to_owned()));
    JsonValue::Object(obj)
}

/// A fully valid `usage_records` row, written out column by column from
/// `migrations/0001_init.sql` rather than produced by any model-to-row
/// direction of the mapper — a fixture the code under test builds would only
/// prove the mapper agrees with itself.
///
/// It is an ordinary measurement: `invalidates` and `reason_code` are both
/// `NULL`, which is one of the two shapes
/// `usage_records_invalidation_pairing` admits. Tests corrupt one field at a
/// time and assert the mapper fails closed with `Internal`.
fn sample_row() -> UsageRecordRow {
    UsageRecordRow {
        id: Uuid::from_u128(1),
        tenant_id: Uuid::from_u128(2),
        gts_type_id: VALID_METER_TYPE_ID.to_owned(),
        value: Decimal::new(425, 1), // 42.5
        window_start: OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap(),
        window_end: OffsetDateTime::from_unix_timestamp(1_700_003_600).unwrap(),
        resource_id: "res-1".to_owned(),
        resource_type: "compute.vm".to_owned(),
        subject_id: Some("subj-1".to_owned()),
        subject_type: Some("user".to_owned()),
        idempotency_key: "idem-1".to_owned(),
        invalidates: None,
        reason_code: None,
        origin: "live".to_owned(),
        acceptance_sequence: 7,
        metadata: valid_metadata_json(),
        ingested_at: OffsetDateTime::from_unix_timestamp(1_700_003_700).unwrap(),
    }
}

#[test]
fn record_row_to_model_maps_a_valid_row_round_trip() {
    let row = sample_row();
    let model = record_row_to_model(row).expect("a fully valid row maps");

    assert_eq!(model.id, Uuid::from_u128(1));
    assert_eq!(model.tenant_id, Uuid::from_u128(2));
    assert_eq!(
        model.gts_type_id,
        MeterTypeId::new(VALID_METER_TYPE_ID).unwrap()
    );
    assert_eq!(model.value, Decimal::new(425, 1));
    assert_eq!(
        model.window_start,
        OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap()
    );
    assert_eq!(
        model.window_end,
        OffsetDateTime::from_unix_timestamp(1_700_003_600).unwrap()
    );
    assert_eq!(model.resource_ref.resource_id(), "res-1");
    assert_eq!(model.resource_ref.resource_type(), "compute.vm");
    let subject = model.subject_ref.as_ref().expect("subject present");
    assert_eq!(subject.subject_id(), "subj-1");
    assert_eq!(subject.subject_type(), Some("user"));
    assert_eq!(model.idempotency_key.as_str(), "idem-1");
    assert_eq!(model.origin, RecordOrigin::Live);
    assert!(model.invalidation.is_none());
    assert_eq!(
        model.metadata.get(&MetadataKey::new("region").unwrap()),
        Some(&"eu-west".to_owned())
    );
}

#[test]
fn a_backfill_row_maps_to_the_backfill_origin() {
    let row = UsageRecordRow {
        origin: "backfill".to_owned(),
        ..sample_row()
    };
    let model = record_row_to_model(row).expect("row must map");
    assert_eq!(model.origin, RecordOrigin::Backfill);
}

#[test]
fn an_invalidation_row_maps_to_a_record_carrying_the_pair() {
    let target = Uuid::new_v4();
    let row = UsageRecordRow {
        invalidates: Some(target),
        reason_code: Some("duplicate_submission".to_owned()),
        ..sample_row()
    };

    let model = record_row_to_model(row).expect("row must map");

    let invalidation = model
        .invalidation
        .expect("a row with invalidates must map to Some(Invalidation)");
    assert_eq!(invalidation.target, target);
    assert_eq!(invalidation.reason.as_str(), "duplicate_submission");
}

#[test]
fn a_row_naming_a_target_without_a_reason_is_an_invariant_break() {
    let row = UsageRecordRow {
        invalidates: Some(Uuid::new_v4()),
        reason_code: None,
        ..sample_row()
    };

    let err = record_row_to_model(row).expect_err("half a pair must not map");

    assert!(
        matches!(err, UsageCollectorPluginError::Internal(_)),
        "a malformed stored row is a plugin invariant break, not a caller error; got {err:?}"
    );
}

#[test]
fn record_row_absent_subject_maps_to_none() {
    let row = UsageRecordRow {
        subject_id: None,
        subject_type: None,
        ..sample_row()
    };
    let model = record_row_to_model(row).expect("a row without a subject maps");
    assert!(model.subject_ref.is_none());
}

#[test]
fn a_subject_without_a_type_maps_to_an_untyped_subject() {
    // The third shape `usage_records_subject_pairing` admits, between both-set
    // and both-NULL: an identified subject whose type is unknown. `SubjectRef`
    // models it as `subject_type: None`, so the mapper must carry the absence
    // rather than invent a placeholder.
    let row = UsageRecordRow {
        subject_type: None,
        ..sample_row()
    };

    let model = record_row_to_model(row).expect("an untyped subject maps");

    let subject = model.subject_ref.as_ref().expect("subject present");
    assert_eq!(subject.subject_id(), "subj-1");
    assert_eq!(
        subject.subject_type(),
        None,
        "a NULL subject_type is an untyped subject, not a fabricated one"
    );
}

#[test]
fn record_row_invalid_gts_type_id_is_internal() {
    let row = UsageRecordRow {
        gts_type_id: "not-a-valid-meter-type-id".to_owned(),
        ..sample_row()
    };
    assert_internal(
        record_row_to_model(row),
        "a stored gts_type_id that no longer validates",
    );
}

#[test]
fn record_row_invalid_resource_ref_is_internal() {
    let row = UsageRecordRow {
        // empty resource_id fails ResourceRef::new
        resource_id: String::new(),
        ..sample_row()
    };
    assert_internal(record_row_to_model(row), "an empty stored resource_id");
}

#[test]
fn record_row_invalid_subject_ref_is_internal() {
    let row = UsageRecordRow {
        // present-but-empty subject_id is rejected
        subject_id: Some(String::new()),
        ..sample_row()
    };
    assert_internal(
        record_row_to_model(row),
        "a present-but-empty stored subject_id",
    );
}

#[test]
fn record_row_invalid_idempotency_key_is_internal() {
    let row = UsageRecordRow {
        idempotency_key: String::new(),
        ..sample_row()
    };
    assert_internal(record_row_to_model(row), "an empty stored idempotency_key");
}

#[test]
fn record_row_non_object_metadata_is_internal() {
    let row = UsageRecordRow {
        metadata: JsonValue::String("not-an-object".to_owned()),
        ..sample_row()
    };
    assert_internal(
        record_row_to_model(row),
        "stored metadata that is not a JSON object",
    );
}

#[test]
fn record_row_unknown_origin_is_internal() {
    let row = UsageRecordRow {
        origin: "imported".to_owned(),
        ..sample_row()
    };
    assert_internal(record_row_to_model(row), "an origin outside the DDL CHECK");
}
