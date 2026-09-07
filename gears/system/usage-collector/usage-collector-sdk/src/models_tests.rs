//! Structural unit tests for the foundation SDK models.

use std::str::FromStr;

use bigdecimal::BigDecimal;
use rust_decimal::Decimal;
use serde_json::json;
use toolkit_gts::gts_id;
use uuid::Uuid;

use std::collections::BTreeMap;

use super::{
    AggregationBucket, AggregationDimension, AggregationFold, AggregationResult, CreateUsageRecord,
    IdempotencyKey, MetadataFilter, MetadataKey, MeterTypeId, ResourceRef, SubjectRef, UsageRecord,
    UsageRecordStatus, WINDOW_START_FIELD, is_keyset_safe_record_field,
};
use crate::error::UsageCollectorError;
use crate::reason::ValidationReason;

fn metadata_key(value: &str) -> MetadataKey {
    MetadataKey::new(value).expect("test fixture supplies a valid metadata key")
}

fn metadata_map<const N: usize>(entries: [(&str, &str); N]) -> BTreeMap<MetadataKey, String> {
    entries
        .into_iter()
        .map(|(k, v)| (metadata_key(k), v.to_owned()))
        .collect()
}

const SAMPLE_METER_TYPE_ID: &str =
    gts_id!("cf.core.uc.usage_record.v1~cf.mini_chat._.tokens_consumed.v1~");

fn sample_meter_id() -> MeterTypeId {
    MeterTypeId::new(SAMPLE_METER_TYPE_ID).expect("valid usage_record-derived meter type id")
}

/// `1970-01-01T00:00:00Z` — the inclusive start of the fixture period.
const SAMPLE_WINDOW_START: time::OffsetDateTime = time::OffsetDateTime::UNIX_EPOCH;
/// `1970-01-01T01:00:00Z` — one hour later, so the two bounds are distinct
/// and a test that confused them would fail rather than pass by symmetry.
const SAMPLE_WINDOW_END: time::OffsetDateTime =
    SAMPLE_WINDOW_START.saturating_add(time::Duration::hours(1));

fn sample_usage_record(subject_ref: Option<SubjectRef>, corrects_id: Option<Uuid>) -> UsageRecord {
    UsageRecord {
        id: Uuid::parse_str("11111111-1111-1111-1111-111111111111").expect("record id"),
        gts_type_id: sample_meter_id(),
        tenant_id: Uuid::parse_str("22222222-2222-2222-2222-222222222222").expect("tenant uuid"),
        resource_ref: ResourceRef::new("vm-1", "compute.vm").expect("valid resource ref"),
        subject_ref,
        metadata: metadata_map([("region", "eu"), ("tier", "gold")]),
        value: Decimal::from(42),
        idempotency_key: IdempotencyKey::new("k-1").expect("valid idempotency key"),
        corrects_id,
        status: UsageRecordStatus::Active,
        window_start: SAMPLE_WINDOW_START,
        window_end: SAMPLE_WINDOW_END,
    }
}

fn sample_create_usage_record(
    subject_ref: Option<SubjectRef>,
    corrects_id: Option<Uuid>,
) -> CreateUsageRecord {
    CreateUsageRecord {
        gts_type_id: sample_meter_id(),
        tenant_id: Uuid::parse_str("22222222-2222-2222-2222-222222222222").expect("tenant uuid"),
        resource_ref: ResourceRef::new("vm-1", "compute.vm").expect("valid resource ref"),
        subject_ref,
        metadata: metadata_map([("region", "eu"), ("tier", "gold")]),
        value: Decimal::from(42),
        idempotency_key: IdempotencyKey::new("k-1").expect("valid idempotency key"),
        corrects_id,
        window_start: SAMPLE_WINDOW_START,
        window_end: SAMPLE_WINDOW_END,
    }
}

// ---------------------------------------------------------------------------
// CreateUsageRecord::try_into_usage_record — period validation and the
// identity stamp on create
// ---------------------------------------------------------------------------

// `try_into_usage_record` is the single point where a submission acquires its
// identity: it validates the covered period, stamps the deterministic derived
// `id`, initializes `status` to `Active`, and forwards every caller-supplied
// field verbatim.
#[test]
fn try_into_usage_record_stamps_derived_id_and_active_status() {
    let subject = SubjectRef::new("sub-1", Some("user".to_owned())).expect("valid subject ref");
    let corrects = Uuid::parse_str("33333333-3333-3333-3333-333333333333").expect("corrects uuid");
    let input = sample_create_usage_record(Some(subject), Some(corrects));

    let record = input
        .clone()
        .try_into_usage_record()
        .expect("the fixture period is valid");

    assert_eq!(
        record.status,
        UsageRecordStatus::Active,
        "a fresh submission must be stamped Active",
    );
    // Every caller-supplied field is forwarded verbatim.
    assert_eq!(record.gts_type_id, input.gts_type_id);
    assert_eq!(record.tenant_id, input.tenant_id);
    assert_eq!(record.resource_ref, input.resource_ref);
    assert_eq!(record.subject_ref, input.subject_ref);
    assert_eq!(record.metadata, input.metadata);
    assert_eq!(record.value, input.value);
    assert_eq!(record.idempotency_key, input.idempotency_key);
    assert_eq!(record.corrects_id, input.corrects_id);
    assert_eq!(record.window_start, input.window_start);
    assert_eq!(record.window_end, input.window_end);
}

#[test]
fn try_into_usage_record_derives_the_id_over_the_five_tuple() {
    let submission = sample_create_usage_record(None, None);
    let expected = crate::id::derive_usage_record_id(
        submission.tenant_id,
        &submission.gts_type_id,
        &submission.idempotency_key,
        submission.window_start,
        submission.window_end,
    );
    let record = submission
        .try_into_usage_record()
        .expect("the fixture period is valid");
    assert_eq!(record.id, expected);
}

// A submission whose dedup identity matches an existing `UsageRecord`
// projects to the SAME `id` that record carries — the derivation is a pure
// function of the 5-tuple, so the create input and the persisted shape agree
// on identity without the caller ever supplying it.
#[test]
fn try_into_usage_record_id_matches_full_record_with_same_dedup_identity() {
    let input = sample_create_usage_record(None, None);
    let persisted = sample_usage_record(None, None);
    // `sample_usage_record` shares the whole dedup identity.
    assert_eq!(input.tenant_id, persisted.tenant_id);
    assert_eq!(input.gts_type_id, persisted.gts_type_id);
    assert_eq!(input.idempotency_key, persisted.idempotency_key);
    assert_eq!(input.window_start, persisted.window_start);
    assert_eq!(input.window_end, persisted.window_end);

    assert_eq!(
        input
            .try_into_usage_record()
            .expect("the fixture period is valid")
            .id,
        crate::id::derive_usage_record_id(
            persisted.tenant_id,
            &persisted.gts_type_id,
            &persisted.idempotency_key,
            persisted.window_start,
            persisted.window_end,
        ),
        "the create-input identity must equal the derivation of the same dedup identity",
    );
}

#[test]
fn try_into_usage_record_rejects_a_sub_microsecond_window_start() {
    // `cpt-cf-usage-collector-adr-record-identity-derivation`: a bound finer
    // than the microsecond is REJECTED, not truncated. Truncating would
    // persist a period whose read-back derives
    // an id different from the one the entry carries, which breaks offline
    // reproduction at the point an emitter needs it.
    let mut submission = sample_create_usage_record(None, None);
    submission.window_start = submission.window_start.replace_nanosecond(500).unwrap();
    let err = submission
        .try_into_usage_record()
        .expect_err("sub-microsecond bound must be rejected");
    let UsageCollectorError::InvalidArgument { field, .. } = err else {
        panic!("expected InvalidArgument, got {err:?}");
    };
    assert_eq!(field, "window_start");
}

#[test]
fn try_into_usage_record_rejects_a_sub_microsecond_window_end() {
    let mut submission = sample_create_usage_record(None, None);
    submission.window_end = submission.window_end.replace_nanosecond(1).unwrap();
    let err = submission
        .try_into_usage_record()
        .expect_err("sub-microsecond bound must be rejected");
    let UsageCollectorError::InvalidArgument { field, .. } = err else {
        panic!("expected InvalidArgument, got {err:?}");
    };
    assert_eq!(field, "window_end");
}

#[test]
fn a_sub_microsecond_rejection_reads_the_same_in_every_offset() {
    // Two claims in one, both about the hoisted UTC normalization that runs
    // BEFORE the precision checks.
    //
    // Behaviour: the same instant submitted in three offsets is rejected
    // all three times — normalization moves the offset, never the
    // nanosecond, so it cannot change what the check accepts.
    //
    // Diagnostics: all three rejections echo ONE rendering. Before the
    // hoist, this bound echoed the caller's offset while the sibling
    // inverted-period error echoed UTC, and an offset carrying non-zero
    // seconds (which RFC 3339 cannot express) fell through to
    // `OffsetDateTime`'s space-separated `Display`. Comparing the details
    // to each other rather than to a literal pins the property without
    // pinning the wording.
    let sub_us = SAMPLE_WINDOW_START.replace_nanosecond(500).unwrap();
    let details: Vec<String> = [
        time::UtcOffset::UTC,
        time::UtcOffset::from_hms(5, 30, 0).unwrap(),
        time::UtcOffset::from_hms(5, 30, 30).unwrap(),
    ]
    .into_iter()
    .map(|offset| {
        let mut submission = sample_create_usage_record(None, None);
        submission.window_start = sub_us.to_offset(offset);
        let err = submission
            .try_into_usage_record()
            .expect_err("a sub-microsecond bound must be rejected in ANY offset");
        let UsageCollectorError::InvalidArgument { field, detail, .. } = err else {
            panic!("expected InvalidArgument, got {err:?}");
        };
        assert_eq!(field, WINDOW_START_FIELD);
        detail
    })
    .collect();

    assert!(
        details.windows(2).all(|pair| pair[0] == pair[1]),
        "one instant must produce one diagnostic whatever offset it arrived \
         in; got {details:#?}",
    );
    assert!(
        details[0].contains("Z,"),
        "the echoed bound must be the UTC rendering, not the caller's \
         offset; got {}",
        details[0],
    );
}

#[test]
fn try_into_usage_record_accepts_a_whole_microsecond_bound() {
    // The ceiling is the microsecond, not the millisecond or the second: a
    // bound carrying a non-zero microsecond is ordinary valid input. Without
    // this, relaxing the precondition to a coarser unit would pass every
    // other test in the file, because the fixture bounds land on whole
    // seconds.
    let mut submission = sample_create_usage_record(None, None);
    submission.window_start = submission.window_start.replace_nanosecond(1_000).unwrap();
    submission.window_end = submission
        .window_end
        .replace_nanosecond(999_999_000)
        .unwrap();
    let record = submission
        .try_into_usage_record()
        .expect("whole-microsecond bounds are valid");
    assert_eq!(record.window_start.nanosecond(), 1_000);
    assert_eq!(record.window_end.nanosecond(), 999_999_000);
}

#[test]
fn try_into_usage_record_accepts_equal_bounds_as_a_point_event() {
    let mut submission = sample_create_usage_record(None, None);
    submission.window_end = submission.window_start;
    let record = submission
        .try_into_usage_record()
        .expect("point event is valid input");
    assert_eq!(record.window_start, record.window_end);
}

#[test]
fn try_into_usage_record_rejects_an_inverted_covered_period() {
    let mut submission = sample_create_usage_record(None, None);
    submission.window_end = submission.window_start - time::Duration::seconds(1);
    let err = submission
        .try_into_usage_record()
        .expect_err("window_end < window_start must be rejected");
    let UsageCollectorError::InvalidArgument { field, .. } = err else {
        panic!("expected InvalidArgument, got {err:?}");
    };
    assert_eq!(field, "window_end");
}

#[test]
fn try_into_usage_record_normalizes_both_bounds_to_utc() {
    let offset = time::UtcOffset::from_hms(-7, 0, 0).unwrap();
    let mut submission = sample_create_usage_record(None, None);
    let start = submission.window_start;
    let end = submission.window_end;
    submission.window_start = start.to_offset(offset);
    submission.window_end = end.to_offset(offset);
    let record = submission
        .try_into_usage_record()
        .expect("the fixture period is valid");
    assert_eq!(record.window_start.offset(), time::UtcOffset::UTC);
    assert_eq!(record.window_end.offset(), time::UtcOffset::UTC);
    // Normalization moves the offset, never the instant, and it must not
    // move either bound onto the other.
    assert_eq!(record.window_start, start);
    assert_eq!(record.window_end, end);
}

#[test]
fn one_instant_in_two_offsets_derives_one_id() {
    // The retry invariant: an emitter that resends the same period in a
    // different offset must not surface a false IdempotencyConflict.
    let offset = time::UtcOffset::from_hms(2, 0, 0).unwrap();
    let utc = sample_create_usage_record(None, None)
        .try_into_usage_record()
        .expect("valid");
    let mut shifted = sample_create_usage_record(None, None);
    shifted.window_start = shifted.window_start.to_offset(offset);
    shifted.window_end = shifted.window_end.to_offset(offset);
    assert_eq!(utc.id, shifted.try_into_usage_record().expect("valid").id,);
}

// ---------------------------------------------------------------------------
// UsageRecord — wire shape (RFC-3339 period bounds, optional skipping,
// `status` defaulting)
// ---------------------------------------------------------------------------

#[test]
fn usage_record_serde_round_trip_omits_none_optionals_and_uses_rfc3339_period_bounds() {
    let record = sample_usage_record(None, None);
    let value = serde_json::to_value(&record).expect("serialize UsageRecord");
    let object = value
        .as_object()
        .expect("UsageRecord serializes as a JSON object");
    assert!(
        !object.contains_key("subject_ref"),
        "subject_ref must be omitted when None; got {object:?}"
    );
    assert!(
        !object.contains_key("corrects_id"),
        "corrects_id must be omitted when None; got {object:?}"
    );
    assert!(
        !object.contains_key("created_at"),
        "the single instant is gone: an entry carries a covered period and \
         no other emitter-supplied time attribution; got {object:?}"
    );
    assert_eq!(
        object.get("window_start").and_then(|v| v.as_str()),
        Some("1970-01-01T00:00:00Z"),
        "window_start must serialize in RFC-3339 form with `Z` UTC marker; got {object:?}"
    );
    assert_eq!(
        object.get("window_end").and_then(|v| v.as_str()),
        Some("1970-01-01T01:00:00Z"),
        "window_end must serialize in RFC-3339 form with `Z` UTC marker; got {object:?}"
    );
    assert_eq!(
        object.get("status").and_then(|v| v.as_str()),
        Some("active"),
        "status must serialize as lowercase; got {object:?}"
    );
    let round_tripped: UsageRecord = serde_json::from_value(value).expect("UsageRecord round-trip");
    assert_eq!(record, round_tripped);
}

#[test]
fn usage_record_deserialize_requires_both_period_bounds() {
    // Neither bound is optional and neither has a serde default: a payload
    // missing one is a malformed entry, not a point event. A `#[serde(
    // default)]` slipped onto either would silently fabricate the epoch.
    let full = serde_json::to_value(sample_usage_record(None, None)).expect("serialize");
    for missing in ["window_start", "window_end"] {
        let mut value = full.clone();
        value
            .as_object_mut()
            .expect("object")
            .remove(missing)
            .expect("fixture carries the field");
        let err = serde_json::from_value::<UsageRecord>(value)
            .expect_err("a missing period bound must be rejected");
        assert!(
            err.to_string().contains(missing),
            "deserialize error MUST name the missing bound `{missing}`; got {err}"
        );
    }
}

#[test]
fn usage_record_serde_round_trip_carries_subject_ref_and_corrects_id_when_some() {
    let subject = SubjectRef::new("principal-1", Some("user")).expect("valid subject ref");
    let correction =
        Uuid::parse_str("33333333-3333-3333-3333-333333333333").expect("correction uuid");
    let record = sample_usage_record(Some(subject), Some(correction));
    let value = serde_json::to_value(&record).expect("serialize UsageRecord");
    let object = value
        .as_object()
        .expect("UsageRecord serializes as a JSON object");
    assert!(
        object.contains_key("subject_ref"),
        "subject_ref must be present when Some; got {object:?}"
    );
    assert_eq!(
        object.get("corrects_id").and_then(|v| v.as_str()),
        Some("33333333-3333-3333-3333-333333333333"),
        "corrects_id must serialize as a UUID string; got {object:?}"
    );
    let round_tripped: UsageRecord = serde_json::from_value(value).expect("UsageRecord round-trip");
    assert_eq!(record, round_tripped);
}

// ---------------------------------------------------------------------------
// MetadataFilter — validated construction and wire shape
// ---------------------------------------------------------------------------

#[test]
fn metadata_filter_new_accepts_non_empty_key_and_values() {
    let f =
        MetadataFilter::new("region", ["us-east-1", "eu-west-1"]).expect("valid metadata filter");
    assert_eq!(f.key().as_str(), "region");
    assert_eq!(
        f.values(),
        &["us-east-1".to_owned(), "eu-west-1".to_owned()]
    );
}

#[test]
fn metadata_filter_new_rejects_empty_key() {
    let err = MetadataFilter::new("", ["v"]).expect_err("empty key must be rejected");
    assert!(
        matches!(err, UsageCollectorError::InvalidArgument { ref field, .. } if field.as_str() == "metadata_filter"),
        "expected InvalidMetadataFilter, got {err:?}"
    );
}

#[test]
fn metadata_filter_new_rejects_nul_byte_in_key() {
    let err = MetadataFilter::new("region\0", ["v"])
        .expect_err("NUL bytes in key must be rejected for jsonb compatibility");
    assert!(matches!(
        err,
        UsageCollectorError::InvalidArgument { ref field, .. } if field.as_str() == "metadata_filter"
    ));
}

#[test]
fn metadata_filter_new_rejects_empty_values() {
    let err = MetadataFilter::new("region", Vec::<&str>::new())
        .expect_err("empty values must be rejected");
    assert!(matches!(
        err,
        UsageCollectorError::InvalidArgument { ref field, .. } if field.as_str() == "metadata_filter"
    ));
}

#[test]
fn metadata_filter_serde_round_trips_wire_shape() {
    let f = MetadataFilter::new("region", ["us-east-1"]).expect("valid metadata filter");
    let value = serde_json::to_value(&f).expect("serialize MetadataFilter");
    assert_eq!(
        value,
        json!({"key": "region", "values": ["us-east-1"]}),
        "wire shape must be {{key, values}}; got {value}"
    );
    let decoded: MetadataFilter = serde_json::from_value(value).expect("round-trip");
    assert_eq!(decoded, f);
}

#[test]
fn metadata_filter_deserialize_routes_through_new_and_surfaces_validation() {
    let err = serde_json::from_value::<MetadataFilter>(json!({"key": "", "values": ["v"]}))
        .expect_err("empty key must surface as a serde error");
    assert!(
        err.to_string().contains("metadata key must not be empty"),
        "serde error must carry the MetadataKey validation detail; got {err}"
    );

    let err = serde_json::from_value::<MetadataFilter>(json!({"key": "region", "values": []}))
        .expect_err("empty values must surface as a serde error");
    assert!(
        err.to_string().contains("must carry at least one value"),
        "serde error must carry the Validation detail; got {err}"
    );
}

#[test]
fn metadata_filter_deserialize_rejects_unknown_fields() {
    let err = serde_json::from_value::<MetadataFilter>(
        json!({"key": "region", "values": ["v"], "op": "eq"}),
    )
    .expect_err("unknown field must be rejected at the wire boundary");
    assert!(err.to_string().contains("unknown field"));
}

// ---------------------------------------------------------------------------
// AggregationDimension — wire shapes
// ---------------------------------------------------------------------------

#[test]
fn aggregation_dimension_serializes_unit_variants_as_snake_case_strings() {
    let value = serde_json::to_value(AggregationDimension::TenantId).expect("serialize");
    assert_eq!(value, json!("tenant_id"));
    let decoded: AggregationDimension = serde_json::from_value(value).expect("round-trip");
    assert_eq!(decoded, AggregationDimension::TenantId);
}

#[test]
fn aggregation_dimension_serializes_metadata_variant_as_tagged_object() {
    let value = serde_json::to_value(AggregationDimension::Metadata(metadata_key("region")))
        .expect("serialize Metadata variant");
    assert_eq!(
        value,
        json!({"metadata": "region"}),
        "Metadata variant must serialize as a tagged object; got {value}"
    );
    let decoded: AggregationDimension = serde_json::from_value(value).expect("round-trip");
    assert_eq!(
        decoded,
        AggregationDimension::Metadata(metadata_key("region"))
    );
}

// ---------------------------------------------------------------------------
// AggregationResult / AggregationBucket — wire shape
// ---------------------------------------------------------------------------

#[test]
fn aggregation_result_carries_empty_key_for_no_grouping() {
    let result = AggregationResult {
        buckets: vec![AggregationBucket {
            key: Vec::new(),
            value: Some(BigDecimal::from(42)),
        }],
    };
    let value = serde_json::to_value(&result).expect("serialize");
    assert_eq!(
        value,
        json!({"buckets": [{"value": "42"}]}),
        "empty key must be skipped on the wire; got {value}"
    );
    let decoded: AggregationResult = serde_json::from_value(value).expect("round-trip");
    assert_eq!(decoded, result);
}

#[test]
fn aggregation_bucket_key_is_vec_of_strings() {
    // One UUID-shaped string, one plain string. Both must round-trip
    // verbatim as raw JSON strings — no envelope, no discriminator.
    let bucket = AggregationBucket {
        key: vec![
            "00000000-0000-0000-0000-000000000001".to_owned(),
            "us-east-1".to_owned(),
        ],
        value: Some(BigDecimal::from(7)),
    };
    let value = serde_json::to_value(&bucket).expect("serialize");
    assert_eq!(
        value,
        json!({
            "key": ["00000000-0000-0000-0000-000000000001", "us-east-1"],
            "value": "7",
        }),
        "AggregationBucket.key must serialize as a JSON array of raw strings; got {value}"
    );
    let decoded: AggregationBucket = serde_json::from_value(value).expect("round-trip");
    assert_eq!(decoded, bucket);
}

#[test]
fn aggregation_bucket_tenant_id_uses_canonical_uuid_string() {
    // Pins the per-spec encoding rule for the TenantId dimension: plugins
    // MUST emit the tenant UUID via `Uuid::to_string()` (lowercase,
    // hyphenated). The bucket itself stores a plain String — this test
    // guards that the canonical form is what producers send.
    let tenant = Uuid::parse_str("0123456789ABCDEF0123456789ABCDEF").expect("tenant uuid");
    let bucket = AggregationBucket {
        key: vec![tenant.to_string()],
        value: Some(BigDecimal::from(1)),
    };
    let value = serde_json::to_value(&bucket).expect("serialize");
    assert_eq!(
        value,
        json!({
            "key": ["01234567-89ab-cdef-0123-456789abcdef"],
            "value": "1",
        }),
        "TenantId dimension must serialize as lowercase hyphenated UUID; got {value}"
    );
}

#[test]
fn aggregation_bucket_carries_none_value_for_empty_aggregation() {
    let bucket = AggregationBucket {
        key: Vec::new(),
        value: None,
    };
    let value = serde_json::to_value(&bucket).expect("serialize");
    assert_eq!(value, json!({"value": null}));
    let decoded: AggregationBucket = serde_json::from_value(value).expect("round-trip");
    assert_eq!(decoded, bucket);
}

#[test]
fn aggregation_bucket_value_above_rust_decimal_ceiling_round_trips() {
    // 2^96 rounded up — beyond rust_decimal's ~7.9e28 ceiling, so this would
    // 500 under the old `Decimal` carrier. It must round-trip exactly now.
    let big = "79228162514264337593543950400";
    let bucket = AggregationBucket {
        key: Vec::new(),
        value: Some(big.parse::<BigDecimal>().expect("bigdecimal parses")),
    };
    let value = serde_json::to_value(&bucket).expect("serialize");
    assert_eq!(value, json!({ "value": "79228162514264337593543950400" }));
    let decoded: AggregationBucket = serde_json::from_value(value).expect("round-trip");
    assert_eq!(decoded, bucket);
}

#[test]
fn aggregation_bucket_negative_value_round_trips() {
    // Compensation rows carry negative magnitudes (and can net to zero) —
    // widening the carrier to BigDecimal is motivated exactly by this path.
    // The sign must survive the string wire encoding round-trip.
    let bucket = AggregationBucket {
        key: Vec::new(),
        value: Some(BigDecimal::from(-42)),
    };
    let value = serde_json::to_value(&bucket).expect("serialize");
    assert_eq!(value, json!({ "value": "-42" }));
    let decoded: AggregationBucket = serde_json::from_value(value).expect("round-trip");
    assert_eq!(decoded, bucket);
}

#[test]
fn usage_record_deserialize_defaults_status_to_active_when_missing() {
    let mut value =
        serde_json::to_value(sample_usage_record(None, None)).expect("serialize seed UsageRecord");
    value.as_object_mut().expect("object").remove("status");
    let decoded: UsageRecord = serde_json::from_value(value)
        .expect("UsageRecord without status field deserializes via #[serde(default)]");
    assert_eq!(decoded.status, UsageRecordStatus::Active);
}

// ---------------------------------------------------------------------------
// MetadataKey — validated construction and serde routing
// ---------------------------------------------------------------------------

#[test]
fn metadata_key_new_accepts_well_formed_string() {
    let key = MetadataKey::new("region").expect("valid key");
    assert_eq!(key.as_str(), "region");
    assert_eq!(key.to_string(), "region");
}

#[test]
fn metadata_key_new_rejects_empty_string() {
    let err = MetadataKey::new("").expect_err("empty key must be rejected");
    assert!(matches!(
        err,
        UsageCollectorError::InvalidArgument { ref field, .. } if field.as_str() == "metadata"
    ));
}

#[test]
fn metadata_key_new_rejects_nul_byte() {
    let err = MetadataKey::new("bad\0key").expect_err("NUL byte must be rejected");
    assert!(matches!(
        err,
        UsageCollectorError::InvalidArgument { ref field, .. } if field.as_str() == "metadata"
    ));
}

#[test]
fn metadata_key_deserialize_routes_through_new() {
    let err = serde_json::from_value::<MetadataKey>(json!(""))
        .expect_err("empty key must surface as a serde error");
    assert!(err.to_string().contains("metadata key must not be empty"));
}

#[test]
fn metadata_key_serializes_transparently() {
    let key = MetadataKey::new("region").expect("valid key");
    let value = serde_json::to_value(&key).expect("serialize");
    assert_eq!(value, json!("region"));
}

// ---------------------------------------------------------------------------
// UsageRecord.metadata wire shape (BTreeMap<MetadataKey, String>)
// ---------------------------------------------------------------------------

#[test]
fn usage_record_metadata_serializes_as_string_to_string_map() {
    let record = sample_usage_record(None, None);
    let value = serde_json::to_value(&record).expect("serialize UsageRecord");
    let metadata = value
        .get("metadata")
        .expect("metadata present when non-empty");
    assert_eq!(metadata, &json!({"region": "eu", "tier": "gold"}));
}

#[test]
fn usage_record_metadata_omitted_from_wire_when_empty() {
    let mut record = sample_usage_record(None, None);
    record.metadata = BTreeMap::new();
    let value = serde_json::to_value(&record).expect("serialize UsageRecord");
    assert!(
        value.get("metadata").is_none(),
        "empty metadata map must be skipped on the wire; got {value}"
    );
}

#[test]
fn usage_record_metadata_deserialize_rejects_non_string_value() {
    let mut value =
        serde_json::to_value(sample_usage_record(None, None)).expect("serialize seed UsageRecord");
    value
        .as_object_mut()
        .expect("object")
        .insert("metadata".to_owned(), json!({"region": 42}));
    let err = serde_json::from_value::<UsageRecord>(value)
        .expect_err("non-string metadata value must be rejected at the type boundary");
    assert!(err.to_string().contains("invalid type"));
}

#[test]
fn usage_record_metadata_deserialize_defaults_to_empty_when_missing() {
    let mut value =
        serde_json::to_value(sample_usage_record(None, None)).expect("serialize seed UsageRecord");
    value.as_object_mut().expect("object").remove("metadata");
    let decoded: UsageRecord = serde_json::from_value(value)
        .expect("UsageRecord without metadata field deserializes via #[serde(default)]");
    assert!(decoded.metadata.is_empty());
}

#[test]
fn usage_record_rejects_unknown_fields() {
    let mut value =
        serde_json::to_value(sample_usage_record(None, None)).expect("serialize seed UsageRecord");
    value
        .as_object_mut()
        .expect("object")
        .insert("legacy_schema_field".to_owned(), json!({"type": "object"}));
    let err = serde_json::from_value::<UsageRecord>(value)
        .expect_err("unknown field must be rejected at the wire boundary");
    assert!(err.to_string().contains("unknown field"));
}

// ---------------------------------------------------------------------------
// IdempotencyKey — validated construction + serde routing
// ---------------------------------------------------------------------------

#[test]
fn idempotency_key_new_accepts_non_empty_string() {
    let k = IdempotencyKey::new("idem-1").expect("valid key");
    assert_eq!(k.as_str(), "idem-1");
    assert_eq!(k.to_string(), "idem-1");
}

#[test]
fn idempotency_key_new_rejects_empty_string() {
    let err = IdempotencyKey::new("").expect_err("empty key must be rejected");
    assert!(
        matches!(err, UsageCollectorError::InvalidArgument { ref field, .. } if field.as_str() == "idempotency_key"),
        "expected InvalidIdempotencyKey, got {err:?}"
    );
}

#[test]
fn idempotency_key_new_rejects_nul_byte() {
    let err = IdempotencyKey::new("bad\0key").expect_err("NUL byte must be rejected");
    assert!(matches!(
        err,
        UsageCollectorError::InvalidArgument { ref field, .. } if field.as_str() == "idempotency_key"
    ));
}

#[test]
fn idempotency_key_rejects_a_unit_separator_del_and_an_over_long_key() {
    // A confirmation of
    // `cpt-cf-usage-collector-adr-record-identity-derivation`: the derivation
    // concatenates the key under a
    // 0x1F separator and the key is not the final field, so a key carrying
    // that byte would inject a separator mid-pre-image. Rejected at
    // construction, before any derivation can run. DEL and the 256-byte
    // ceiling come from the same wire schema pattern.
    IdempotencyKey::new("idem\u{1f}1").expect_err("0x1F in a key must be rejected");
    IdempotencyKey::new("idem\u{7f}1").expect_err("DEL in a key must be rejected");
    IdempotencyKey::new("a".repeat(257)).expect_err("over-long key must be rejected");
    IdempotencyKey::new("a".repeat(256)).expect("256 bytes is the ceiling, not past it");
}

#[test]
fn idempotency_key_serializes_transparently() {
    let k = IdempotencyKey::new("idem-1").expect("valid key");
    let value = serde_json::to_value(&k).expect("serialize");
    assert_eq!(value, json!("idem-1"));
}

#[test]
fn idempotency_key_deserialize_routes_through_new() {
    let err = serde_json::from_value::<IdempotencyKey>(json!(""))
        .expect_err("empty key must surface as a serde error");
    assert!(
        err.to_string()
            .contains("idempotency_key must not be empty"),
        "serde error must carry the Validation detail; got {err}"
    );
}

#[test]
fn idempotency_key_from_str_routes_through_new() {
    assert!(IdempotencyKey::from_str("idem-1").is_ok());
    let err = IdempotencyKey::from_str("").expect_err("empty key must be rejected");
    assert!(matches!(
        err,
        UsageCollectorError::InvalidArgument { ref field, .. } if field.as_str() == "idempotency_key"
    ));
}

// ---------------------------------------------------------------------------
// ResourceRef — validated construction + serde routing
// ---------------------------------------------------------------------------

#[test]
fn resource_ref_new_accepts_non_empty_components() {
    let r = ResourceRef::new("vm-1", "compute.vm").expect("valid ref");
    assert_eq!(r.resource_id(), "vm-1");
    assert_eq!(r.resource_type(), "compute.vm");
}

#[test]
fn resource_ref_new_rejects_empty_resource_id() {
    let err = ResourceRef::new("", "compute.vm").expect_err("empty resource_id must be rejected");
    assert!(
        matches!(err, UsageCollectorError::InvalidArgument { ref field, .. } if field.as_str() == "resource_ref"),
        "expected InvalidResourceRef, got {err:?}"
    );
}

#[test]
fn resource_ref_new_rejects_empty_resource_type() {
    let err = ResourceRef::new("vm-1", "").expect_err("empty resource_type must be rejected");
    assert!(matches!(
        err,
        UsageCollectorError::InvalidArgument { ref field, .. } if field.as_str() == "resource_ref"
    ));
}

#[test]
fn resource_ref_new_rejects_nul_byte_in_resource_id() {
    let err = ResourceRef::new("vm\0bad", "compute.vm")
        .expect_err("NUL byte in resource_id must be rejected");
    assert!(matches!(
        err,
        UsageCollectorError::InvalidArgument { ref field, .. } if field.as_str() == "resource_ref"
    ));
}

#[test]
fn resource_ref_new_rejects_nul_byte_in_resource_type() {
    let err = ResourceRef::new("vm-1", "compute\0vm")
        .expect_err("NUL byte in resource_type must be rejected");
    assert!(matches!(
        err,
        UsageCollectorError::InvalidArgument { ref field, .. } if field.as_str() == "resource_ref"
    ));
}

#[test]
fn resource_ref_serde_round_trips_wire_shape() {
    let r = ResourceRef::new("vm-1", "compute.vm").expect("valid ref");
    let value = serde_json::to_value(&r).expect("serialize");
    assert_eq!(
        value,
        json!({"resource_id": "vm-1", "resource_type": "compute.vm"}),
        "wire shape must be {{resource_id, resource_type}}; got {value}"
    );
    let decoded: ResourceRef = serde_json::from_value(value).expect("round-trip");
    assert_eq!(decoded, r);
}

#[test]
fn resource_ref_deserialize_routes_through_new() {
    let err = serde_json::from_value::<ResourceRef>(json!({
        "resource_id": "",
        "resource_type": "compute.vm",
    }))
    .expect_err("empty resource_id must surface as a serde error");
    assert!(
        err.to_string().contains("resource_id must not be empty"),
        "serde error must carry the Validation detail; got {err}"
    );

    let err = serde_json::from_value::<ResourceRef>(json!({
        "resource_id": "vm-1",
        "resource_type": "",
    }))
    .expect_err("empty resource_type must surface as a serde error");
    assert!(err.to_string().contains("resource_type must not be empty"));
}

#[test]
fn resource_ref_deserialize_rejects_unknown_fields() {
    let err = serde_json::from_value::<ResourceRef>(json!({
        "resource_id": "vm-1",
        "resource_type": "compute.vm",
        "tenant_scope": "main",
    }))
    .expect_err("unknown field must be rejected at the wire boundary");
    assert!(err.to_string().contains("unknown field"));
}

// ---------------------------------------------------------------------------
// SubjectRef — validated construction + serde routing
// ---------------------------------------------------------------------------

#[test]
fn subject_ref_new_accepts_subject_id_only() {
    let s = SubjectRef::new("principal-1", Option::<&str>::None).expect("valid ref");
    assert_eq!(s.subject_id(), "principal-1");
    assert!(s.subject_type().is_none());
}

#[test]
fn subject_ref_new_accepts_subject_id_and_subject_type() {
    let s = SubjectRef::new("principal-1", Some("user")).expect("valid ref");
    assert_eq!(s.subject_id(), "principal-1");
    assert_eq!(s.subject_type(), Some("user"));
}

#[test]
fn subject_ref_new_rejects_empty_subject_id() {
    let err = SubjectRef::new("", Some("user")).expect_err("empty subject_id must be rejected");
    assert!(
        matches!(err, UsageCollectorError::InvalidArgument { ref field, .. } if field.as_str() == "subject_ref")
    );
}

#[test]
fn subject_ref_new_rejects_explicit_empty_subject_type() {
    let err = SubjectRef::new("principal-1", Some(""))
        .expect_err("Some(\"\") subject_type must be rejected");
    assert!(
        matches!(err, UsageCollectorError::InvalidArgument { ref field, .. } if field.as_str() == "subject_ref")
    );
}

#[test]
fn subject_ref_new_rejects_nul_byte_in_subject_id() {
    let err = SubjectRef::new("principal\0bad", Some("user"))
        .expect_err("NUL byte in subject_id must be rejected");
    assert!(
        matches!(err, UsageCollectorError::InvalidArgument { ref field, .. } if field.as_str() == "subject_ref")
    );
}

#[test]
fn subject_ref_new_rejects_nul_byte_in_subject_type() {
    let err = SubjectRef::new("principal-1", Some("user\0bad"))
        .expect_err("NUL byte in subject_type must be rejected");
    assert!(
        matches!(err, UsageCollectorError::InvalidArgument { ref field, .. } if field.as_str() == "subject_ref")
    );
}

#[test]
fn subject_ref_serde_round_trips_omitting_none_subject_type() {
    let s = SubjectRef::new("principal-1", Option::<&str>::None).expect("valid ref");
    let value = serde_json::to_value(&s).expect("serialize");
    assert_eq!(
        value,
        json!({"subject_id": "principal-1"}),
        "subject_type must be omitted when None; got {value}"
    );
    let decoded: SubjectRef = serde_json::from_value(value).expect("round-trip");
    assert_eq!(decoded, s);
}

#[test]
fn subject_ref_serde_round_trips_carrying_subject_type() {
    let s = SubjectRef::new("principal-1", Some("user")).expect("valid ref");
    let value = serde_json::to_value(&s).expect("serialize");
    assert_eq!(
        value,
        json!({"subject_id": "principal-1", "subject_type": "user"}),
    );
    let decoded: SubjectRef = serde_json::from_value(value).expect("round-trip");
    assert_eq!(decoded, s);
}

#[test]
fn subject_ref_deserialize_routes_through_new() {
    let err = serde_json::from_value::<SubjectRef>(json!({
        "subject_id": "",
        "subject_type": "user",
    }))
    .expect_err("empty subject_id must surface as a serde error");
    assert!(err.to_string().contains("subject_id must not be empty"));

    let err = serde_json::from_value::<SubjectRef>(json!({
        "subject_id": "principal-1",
        "subject_type": "",
    }))
    .expect_err("empty subject_type must surface as a serde error");
    assert!(
        err.to_string()
            .contains("subject_type must not be empty when supplied")
    );
}

#[test]
fn subject_ref_deserialize_rejects_unknown_fields() {
    let err = serde_json::from_value::<SubjectRef>(json!({
        "subject_id": "principal-1",
        "subject_type": "user",
        "tenant_id": "00000000-0000-0000-0000-000000000000",
    }))
    .expect_err("unknown field must be rejected at the wire boundary");
    assert!(err.to_string().contains("unknown field"));
}

// ---------------------------------------------------------------------------
// UsageRecordQuery — OData filter surface
// ---------------------------------------------------------------------------
//
// `gts_type_id` is carried as a typed parameter on `list_usage_records` /
// `query_aggregated_usage_records`. The OData filter surface declared by
// `UsageRecordQuery` deliberately omits it so that
// `parse_odata_filter::<UsageRecordFilterField>` rejects any
// `gts_type_id`-touching predicate at parse time — implementations and the
// gateway do not need a runtime reject path.

#[test]
fn usage_record_query_filter_surface_rejects_gts_type_id_eq() {
    let err = toolkit_odata::filter::parse_odata_filter::<crate::models::UsageRecordFilterField>(
        "gts_type_id eq 'gts.cf.core.uc.usage_record.v1~cf.compute._.vcpu_hours.v1~'",
    )
    .expect_err("gts_type_id must not be exposed on the OData filter surface");
    assert!(
        matches!(
            &err,
            toolkit_odata::filter::FilterError::UnknownField(name) if name == "gts_type_id"
        ),
        "expected UnknownField(\"gts_type_id\"), got {err:?}",
    );
}

#[test]
fn usage_record_query_filter_surface_rejects_gts_type_id_in_list() {
    let err = toolkit_odata::filter::parse_odata_filter::<crate::models::UsageRecordFilterField>(
        "gts_type_id in ('a', 'b')",
    )
    .expect_err("gts_type_id must not be exposed on the OData filter surface");
    assert!(
        matches!(
            &err,
            toolkit_odata::filter::FilterError::UnknownField(name) if name == "gts_type_id"
        ),
        "expected UnknownField(\"gts_type_id\"), got {err:?}",
    );
}

#[test]
fn usage_record_query_filter_surface_rejects_gts_type_id_inside_composite() {
    let err = toolkit_odata::filter::parse_odata_filter::<crate::models::UsageRecordFilterField>(
        "tenant_id eq 22222222-2222-2222-2222-222222222222 and gts_type_id eq 'x'",
    )
    .expect_err("gts_type_id-touching predicates must be rejected at parse time");
    assert!(
        matches!(
            &err,
            toolkit_odata::filter::FilterError::UnknownField(name) if name == "gts_type_id"
        ),
        "expected UnknownField(\"gts_type_id\"), got {err:?}",
    );
}

// ---------------------------------------------------------------------------
// Keyset-safe (never-null) order-field classification
// ---------------------------------------------------------------------------

#[test]
fn keyset_safe_record_fields_are_exactly_the_mandatory_columns() {
    // The mandatory (never-null) record attributes are sound leading keys for
    // the plugin's row-value tuple keyset comparison.
    for field in [
        "id",
        "created_at",
        "tenant_id",
        "resource_id",
        "resource_type",
        "status",
    ] {
        assert!(
            is_keyset_safe_record_field(field),
            "`{field}` is a mandatory attribute and must be keyset-safe",
        );
    }
}

#[test]
fn keyset_unsafe_record_fields_are_the_domain_optional_ones() {
    // `subject_ref` (→ subject_id, subject_type) and `corrects_id` are
    // `Option`al on `UsageRecord`, so their columns are nullable. A row-value
    // tuple comparison with a NULL leading key evaluates to NULL in Postgres,
    // silently dropping NULL rows from the page — so they are NOT keyset-safe.
    for field in ["subject_id", "subject_type", "corrects_id"] {
        assert!(
            !is_keyset_safe_record_field(field),
            "`{field}` is a domain-optional attribute and must NOT be keyset-safe",
        );
    }
}

#[test]
fn keyset_safe_record_field_is_fail_closed_for_unknown_names() {
    // Fail-closed allowlist: an unknown field (or a future field someone
    // forgets to classify) is treated as unsafe rather than silently allowed.
    assert!(!is_keyset_safe_record_field("value"));
    assert!(!is_keyset_safe_record_field("definitely_not_a_field"));
    assert!(!is_keyset_safe_record_field(""));
}

// ---------------------------------------------------------------------------
// AggregationFold — declared-fold serde/FromStr surface
// ---------------------------------------------------------------------------

#[test]
fn aggregation_fold_serde_round_trips_screaming_case() {
    for (fold, wire) in [
        (AggregationFold::Sum, "\"SUM\""),
        (AggregationFold::Count, "\"COUNT\""),
        (AggregationFold::Max, "\"MAX\""),
        (AggregationFold::Min, "\"MIN\""),
        (AggregationFold::Latest, "\"LATEST\""),
    ] {
        assert_eq!(serde_json::to_string(&fold).unwrap(), wire);
        assert_eq!(serde_json::from_str::<AggregationFold>(wire).unwrap(), fold);
    }
}

#[test]
fn aggregation_fold_rejects_avg() {
    // AVG is not a declared fold: a declaration naming it must fail
    // resolution rather than silently pick another.
    assert!(serde_json::from_str::<AggregationFold>("\"AVG\"").is_err());

    let err = "AVG"
        .parse::<AggregationFold>()
        .expect_err("AVG must be rejected by FromStr");
    assert!(
        matches!(err, UsageCollectorError::InvalidArgument { ref field, ref detail, .. } if field == "aggregation_fold" && detail.contains("AVG")),
        "expected InvalidArgument on field `aggregation_fold` naming AVG, got {err:?}"
    );
}

#[test]
fn aggregation_fold_from_str_matches_the_wire_shape() {
    assert_eq!(
        "SUM".parse::<AggregationFold>().unwrap(),
        AggregationFold::Sum
    );
    assert_eq!(
        "LATEST".parse::<AggregationFold>().unwrap(),
        AggregationFold::Latest
    );
    // Case-sensitive on purpose: the enum in the trait schema is upper case,
    // and accepting "sum" would admit a declaration the registry rejects.
    assert!("sum".parse::<AggregationFold>().is_err());
}

#[test]
fn aggregation_fold_as_str_matches_the_wire_spelling() {
    assert_eq!(AggregationFold::Sum.as_str(), "SUM");
    assert_eq!(AggregationFold::Count.as_str(), "COUNT");
    assert_eq!(AggregationFold::Max.as_str(), "MAX");
    assert_eq!(AggregationFold::Min.as_str(), "MIN");
    assert_eq!(AggregationFold::Latest.as_str(), "LATEST");
}

#[test]
fn aggregation_fold_display_matches_as_str() {
    assert_eq!(AggregationFold::Latest.to_string(), "LATEST");
}

// ---------------------------------------------------------------------------
// MeterTypeId — construction validation, serde routing, FromStr, Display
// ---------------------------------------------------------------------------

const VALID_METER: &str = "gts.cf.core.uc.usage_record.v1~example.metering._.stored_volume.v1~";

#[test]
fn meter_type_id_accepts_a_single_derivation_of_the_base() {
    let id = MeterTypeId::new(VALID_METER).expect("valid meter type id");
    assert_eq!(id.as_str(), VALID_METER);
}

#[test]
fn meter_type_id_rejects_the_bare_base_type() {
    // The base is abstract. A meter must add exactly one segment.
    assert!(MeterTypeId::new("gts.cf.core.uc.usage_record.v1~").is_err());
}

#[test]
fn meter_type_id_rejects_a_type_outside_the_base() {
    assert!(MeterTypeId::new("gts.cf.core.uc.usage_type.v1~foo.bar._.baz.v1~").is_err());
}

#[test]
fn meter_type_id_rejects_a_missing_terminator() {
    // No trailing `~` makes it an instance id, not a type id.
    assert!(
        MeterTypeId::new("gts.cf.core.uc.usage_record.v1~example.metering._.stored_volume.v1")
            .is_err()
    );
}

// A meter is a leaf: exactly one derivation segment on top of the base. A
// second segment (`base~mid.v1~tail.v1~`, a two-level chain) has the right
// prefix and the right terminator, so only the interior-`~` check inside the
// stripped segment catches it — this is the case a plain `strip_prefix`
// check would let through.
#[test]
fn meter_type_id_rejects_a_deep_derivation_chain() {
    let err = MeterTypeId::new("gts.cf.core.uc.usage_record.v1~a.b._.c.v1~d.e._.f.v1~")
        .expect_err("a meter is a leaf; a second derivation segment must be rejected");
    assert!(matches!(
        err,
        UsageCollectorError::InvalidArgument {
            reason: ValidationReason::InvalidBaseGtsId,
            ..
        }
    ));
}

// Empty inner segment (consecutive `~`) must also be rejected: stripping the
// base prefix leaves the bare terminator `~` with nothing before it, and the
// `segment.is_empty()` check after stripping that trailing `~` is what
// catches it.
#[test]
fn meter_type_id_rejects_consecutive_tildes() {
    let err = MeterTypeId::new("gts.cf.core.uc.usage_record.v1~~")
        .expect_err("consecutive tildes (empty derivation segment) must be rejected");
    assert!(matches!(
        err,
        UsageCollectorError::InvalidArgument {
            reason: ValidationReason::InvalidBaseGtsId,
            ..
        }
    ));
}

#[test]
fn meter_type_id_rejects_control_characters() {
    // ADR-0007 concatenates this value under a 0x1F separator, so a control
    // character would break the injectivity the identifier derivation needs.
    let with_us = "gts.cf.core.uc.usage_record.v1~exa\u{1F}mple._.m.v1~";
    assert!(MeterTypeId::new(with_us).is_err());
    let with_del = "gts.cf.core.uc.usage_record.v1~exa\u{7F}mple._.m.v1~";
    assert!(MeterTypeId::new(with_del).is_err());
}

#[test]
fn meter_type_id_rejects_an_over_long_identifier() {
    let long = format!("gts.cf.core.uc.usage_record.v1~{}.v1~", "a".repeat(600));
    assert!(MeterTypeId::new(long).is_err());
}

#[test]
fn meter_type_id_deserialize_routes_through_validation() {
    let bad = serde_json::json!("gts.cf.core.uc.usage_record.v1~");
    assert!(serde_json::from_value::<MeterTypeId>(bad).is_err());

    let good = serde_json::json!(VALID_METER);
    let parsed: MeterTypeId = serde_json::from_value(good).expect("valid");
    assert_eq!(parsed.as_str(), VALID_METER);
}

#[test]
fn meter_type_id_rejects_control_characters_as_validation_error_naming_the_value() {
    // Pins the concrete variant, the attributed field, and that `detail`
    // names the offending value rather than a generic GTS parse error —
    // matching usage_kind_from_str_rejects_unknown_variant_as_validation_error.
    let with_us = "gts.cf.core.uc.usage_record.v1~exa\u{1F}mple._.m.v1~";
    let err = MeterTypeId::new(with_us).expect_err("control character must be rejected");
    assert!(
        matches!(
            err,
            UsageCollectorError::InvalidArgument {
                ref field,
                ref reason,
                ref detail,
                ..
            } if field == "gts_type_id"
                && matches!(reason, ValidationReason::InvalidBaseGtsId)
                && detail.contains(with_us)
        ),
        "expected InvalidArgument[gts_type_id/InvalidBaseGtsId] naming the offending value, got {err:?}"
    );
}

#[test]
fn meter_type_id_as_str_and_display_match_the_wire_string() {
    let id = MeterTypeId::new(VALID_METER).expect("valid meter type id");
    assert_eq!(id.as_str(), VALID_METER);
    assert_eq!(id.to_string(), VALID_METER);
}

#[test]
fn meter_type_id_from_str_routes_through_new() {
    let id: MeterTypeId = VALID_METER
        .parse()
        .expect("valid meter type id via FromStr");
    assert_eq!(id.as_str(), VALID_METER);

    let err = "gts.cf.core.uc.usage_record.v1~"
        .parse::<MeterTypeId>()
        .expect_err("bare base must be rejected by FromStr");
    assert!(matches!(
        err,
        UsageCollectorError::InvalidArgument { ref field, .. } if field == "gts_type_id"
    ));
}
