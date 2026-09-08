//! Structural unit tests for the foundation SDK models.

use std::str::FromStr;

use bigdecimal::BigDecimal;
use rust_decimal::Decimal;
use serde_json::json;
use toolkit_gts::gts_id;
use toolkit_odata::filter::FilterField as _;
use uuid::Uuid;

use std::collections::BTreeMap;

use super::{
    AggregationBucket, AggregationDimension, AggregationFold, AggregationResult, CreateUsageRecord,
    EntryType, IdempotencyKey, Invalidation, MetadataFilter, MetadataKey, MeterTypeId, ReasonCode,
    RecordOrigin, ResourceRef, SubjectRef, UsageRecord, WINDOW_START_FIELD,
    is_keyset_safe_record_field,
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

/// The entry a fixture invalidation withdraws.
fn target_id() -> Uuid {
    Uuid::parse_str("33333333-3333-3333-3333-333333333333").expect("target uuid")
}

fn reason_code(value: &str) -> ReasonCode {
    ReasonCode::new(value).expect("test fixture supplies a valid reason code")
}

/// The withdrawal a fixture invalidation carries. Both fixtures take an
/// `Option<Uuid>` and build the whole [`Invalidation`] from it, because the
/// type admits no other arrangement: a target without a reason is
/// unrepresentable in Rust and can only be built as JSON.
fn sample_invalidation(target: Uuid) -> Invalidation {
    Invalidation {
        target,
        reason: reason_code("emitter_duplicate"),
    }
}

fn sample_usage_record(subject_ref: Option<SubjectRef>, invalidates: Option<Uuid>) -> UsageRecord {
    UsageRecord {
        id: Uuid::parse_str("11111111-1111-1111-1111-111111111111").expect("record id"),
        gts_type_id: sample_meter_id(),
        tenant_id: Uuid::parse_str("22222222-2222-2222-2222-222222222222").expect("tenant uuid"),
        resource_ref: ResourceRef::new("vm-1", "compute.vm").expect("valid resource ref"),
        subject_ref,
        metadata: metadata_map([("region", "eu"), ("tier", "gold")]),
        value: Decimal::from(42),
        idempotency_key: IdempotencyKey::new("k-1").expect("valid idempotency key"),
        // `live` by default: no test that builds a record by hand has the
        // admitting path as its subject. The ones that do go through
        // `try_into_usage_record`, which is what stamps an origin — or
        // override this field after building, as the metadata cases do.
        origin: RecordOrigin::Live,
        invalidation: invalidates.map(sample_invalidation),
        window_start: SAMPLE_WINDOW_START,
        window_end: SAMPLE_WINDOW_END,
    }
}

fn sample_create_usage_record(
    subject_ref: Option<SubjectRef>,
    invalidates: Option<Uuid>,
) -> CreateUsageRecord {
    CreateUsageRecord {
        gts_type_id: sample_meter_id(),
        tenant_id: Uuid::parse_str("22222222-2222-2222-2222-222222222222").expect("tenant uuid"),
        resource_ref: ResourceRef::new("vm-1", "compute.vm").expect("valid resource ref"),
        subject_ref,
        metadata: metadata_map([("region", "eu"), ("tier", "gold")]),
        value: Decimal::from(42),
        idempotency_key: IdempotencyKey::new("k-1").expect("valid idempotency key"),
        invalidation: invalidates.map(sample_invalidation),
        window_start: SAMPLE_WINDOW_START,
        window_end: SAMPLE_WINDOW_END,
    }
}

// ---------------------------------------------------------------------------
// CreateUsageRecord::try_into_usage_record — period validation and the
// identity stamp on create
// ---------------------------------------------------------------------------

// `try_into_usage_record` is the single point where a submission acquires its
// identity: it validates the submission's own shape and covered period,
// stamps the deterministic derived `id`, and forwards every caller-supplied
// field verbatim — the invalidation reference and its reason included.
#[test]
fn try_into_usage_record_stamps_the_derived_id_and_forwards_every_field() {
    let subject = SubjectRef::new("sub-1", Some("user".to_owned())).expect("valid subject ref");
    let input = sample_create_usage_record(Some(subject), Some(target_id()));

    let record = input
        .clone()
        .try_into_usage_record(RecordOrigin::Live)
        .expect("the fixture period is valid");

    // Every caller-supplied field is forwarded verbatim.
    assert_eq!(record.gts_type_id, input.gts_type_id);
    assert_eq!(record.tenant_id, input.tenant_id);
    assert_eq!(record.resource_ref, input.resource_ref);
    assert_eq!(record.subject_ref, input.subject_ref);
    assert_eq!(record.metadata, input.metadata);
    assert_eq!(record.value, input.value);
    assert_eq!(record.idempotency_key, input.idempotency_key);
    assert_eq!(record.invalidation, input.invalidation);
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
        .try_into_usage_record(RecordOrigin::Live)
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
            .try_into_usage_record(RecordOrigin::Live)
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
        .try_into_usage_record(RecordOrigin::Live)
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
        .try_into_usage_record(RecordOrigin::Live)
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
            .try_into_usage_record(RecordOrigin::Live)
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
        .try_into_usage_record(RecordOrigin::Live)
        .expect("whole-microsecond bounds are valid");
    assert_eq!(record.window_start.nanosecond(), 1_000);
    assert_eq!(record.window_end.nanosecond(), 999_999_000);
}

#[test]
fn try_into_usage_record_accepts_equal_bounds_as_a_point_event() {
    let mut submission = sample_create_usage_record(None, None);
    submission.window_end = submission.window_start;
    let record = submission
        .try_into_usage_record(RecordOrigin::Live)
        .expect("point event is valid input");
    assert_eq!(record.window_start, record.window_end);
}

#[test]
fn try_into_usage_record_rejects_an_inverted_covered_period() {
    let mut submission = sample_create_usage_record(None, None);
    submission.window_end = submission.window_start - time::Duration::seconds(1);
    let err = submission
        .try_into_usage_record(RecordOrigin::Live)
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
        .try_into_usage_record(RecordOrigin::Live)
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
        .try_into_usage_record(RecordOrigin::Live)
        .expect("valid");
    let mut shifted = sample_create_usage_record(None, None);
    shifted.window_start = shifted.window_start.to_offset(offset);
    shifted.window_end = shifted.window_end.to_offset(offset);
    assert_eq!(
        utc.id,
        shifted
            .try_into_usage_record(RecordOrigin::Live)
            .expect("valid")
            .id,
    );
}

// ---------------------------------------------------------------------------
// UsageRecord — wire shape (RFC-3339 period bounds, optional skipping)
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
        !object.contains_key("invalidates"),
        "invalidates must be omitted when None; got {object:?}"
    );
    assert!(
        !object.contains_key("reason_code"),
        "reason_code must be omitted when None; got {object:?}"
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
fn usage_record_serde_round_trip_carries_subject_ref_and_the_withdrawal_pair_when_some() {
    let subject = SubjectRef::new("principal-1", Some("user")).expect("valid subject ref");
    let record = sample_usage_record(Some(subject), Some(target_id()));
    let value = serde_json::to_value(&record).expect("serialize UsageRecord");
    let object = value
        .as_object()
        .expect("UsageRecord serializes as a JSON object");
    assert!(
        object.contains_key("subject_ref"),
        "subject_ref must be present when Some; got {object:?}"
    );
    assert_eq!(
        object.get("invalidates").and_then(|v| v.as_str()),
        Some("33333333-3333-3333-3333-333333333333"),
        "invalidates must serialize as a UUID string; got {object:?}"
    );
    assert_eq!(
        object.get("reason_code").and_then(|v| v.as_str()),
        Some("emitter_duplicate"),
        "reason_code must serialize transparently as its string; got {object:?}"
    );
    let round_tripped: UsageRecord = serde_json::from_value(value).expect("UsageRecord round-trip");
    assert_eq!(record, round_tripped);
}

// ---------------------------------------------------------------------------
// EntryType — the derived record/invalidation discriminator
// ---------------------------------------------------------------------------

#[test]
fn entry_type_is_derived_from_the_reference_it_summarizes() {
    let record = sample_usage_record(None, None);
    assert_eq!(record.entry_type(), EntryType::Record);

    let invalidation = sample_usage_record(None, Some(target_id()));
    assert_eq!(invalidation.entry_type(), EntryType::Invalidation);
}

#[test]
fn an_entry_carries_no_serialized_entry_type() {
    // Derived means derived: a stored discriminator is a second place the
    // kind can be read, and the two can disagree. `deny_unknown_fields`
    // makes the negative assertion sharp — a submitted one is refused.
    // This half guards the shape a plugin returns.
    let json = serde_json::to_value(sample_usage_record(None, None)).expect("serializes");
    assert!(json.get("entry_type").is_none());

    let mut with_marker = json.as_object().expect("object").clone();
    with_marker.insert("entry_type".to_owned(), serde_json::json!("invalidation"));
    serde_json::from_value::<UsageRecord>(serde_json::Value::Object(with_marker))
        .expect_err("a submitted entry_type must be refused as an unknown field");
}

#[test]
fn a_submission_carries_no_entry_type_either() {
    // The wire contract states the rule on the ingestion shape — "the
    // ingestion shape accepts no discriminator field" — and this is the
    // type a request body deserializes into, so it is where a caller could
    // actually try to send one.
    let json = serde_json::to_value(sample_create_usage_record(None, None)).expect("serializes");
    assert!(json.get("entry_type").is_none());

    let mut with_marker = json.as_object().expect("object").clone();
    with_marker.insert("entry_type".to_owned(), serde_json::json!("invalidation"));
    serde_json::from_value::<CreateUsageRecord>(serde_json::Value::Object(with_marker))
        .expect_err("a submitted entry_type must be refused as an unknown field");
}

#[test]
fn entry_type_as_str_matches_the_wire_spelling() {
    assert_eq!(EntryType::Record.as_str(), "record");
    assert_eq!(EntryType::Invalidation.as_str(), "invalidation");
}

#[test]
fn entry_type_serde_agrees_with_as_str_on_both_variants() {
    // Two spellings of one vocabulary: `rename_all = "lowercase"` and
    // `as_str`. A surface may reach for either — the REST projection
    // serializes the value, a metric label takes the `&'static str` — so
    // they must not be able to drift. Without the rename the derived
    // `Serialize` would emit `"Record"` while `as_str` kept `"record"`,
    // and nothing would notice.
    for kind in [EntryType::Record, EntryType::Invalidation] {
        assert_eq!(
            serde_json::to_value(kind).expect("serialize EntryType"),
            json!(kind.as_str()),
            "the serde encoding of {kind:?} must be its `as_str` spelling",
        );
    }
    // Spelled out once as literals too, so the pair cannot drift together.
    assert_eq!(
        serde_json::to_string(&EntryType::Invalidation).expect("serialize"),
        "\"invalidation\"",
    );
}

#[test]
fn a_half_shape_body_is_refused_on_deserialize() {
    // In Rust the pairing is a property of the type: `Invalidation` holds
    // both halves, so neither a submission nor an entry can carry one
    // without the other and there is nothing left for the projection to
    // check. A JSON body is the one place the two can still arrive apart —
    // the wire keeps them flat, per the contract — so the deserialization
    // shadow is where the rule now lives. Each direction is checked
    // separately: dropping either key must be refused, and one arm covers
    // the other's shape in neither direction.
    for (present, missing) in [
        ("invalidates", "reason_code"),
        ("reason_code", "invalidates"),
    ] {
        let full =
            serde_json::to_value(sample_usage_record(None, Some(target_id()))).expect("serialize");
        let mut half = full.as_object().expect("object").clone();
        half.remove(missing).expect("the pair serializes flat");
        assert!(
            half.contains_key(present),
            "the surviving half `{present}` must still be on the body",
        );
        let err = serde_json::from_value::<UsageRecord>(serde_json::Value::Object(half))
            .expect_err("a half-shape entry body must be refused");
        assert!(
            err.to_string().contains(missing),
            "the refusal must name the missing `{missing}`; got {err}",
        );

        let full = serde_json::to_value(sample_create_usage_record(None, Some(target_id())))
            .expect("serialize");
        let mut half = full.as_object().expect("object").clone();
        half.remove(missing).expect("the pair serializes flat");
        let err = serde_json::from_value::<CreateUsageRecord>(serde_json::Value::Object(half))
            .expect_err("a half-shape submission body must be refused");
        assert!(
            err.to_string().contains(missing),
            "the refusal must name the missing `{missing}`; got {err}",
        );
    }

    // Both halves present, and neither present, both decode.
    sample_create_usage_record(None, None)
        .try_into_usage_record(RecordOrigin::Live)
        .expect("an ordinary record");
    sample_create_usage_record(None, Some(target_id()))
        .try_into_usage_record(RecordOrigin::Live)
        .expect("an invalidation");
}

#[test]
fn both_entry_shapes_round_trip_through_their_own_codecs() {
    // Both shapes hand-write `Serialize` and route `Deserialize` through a
    // shadow, so the two halves of each codec are written twice and only
    // this test makes them agree. It bites in both directions because the
    // read shadows carry `deny_unknown_fields`: a key the write half
    // renames or invents is refused as unknown, and a required key it
    // stops emitting is refused as missing. Neither failure is one the
    // compiler can see — the exhaustive destructure in each `Serialize`
    // catches a *dropped* field, not a *renamed* one.
    //
    // `CreateUsageRecord` is the emitter-facing ingestion shape, so drift
    // there silently changes what an SDK client puts on the wire. It went
    // unguarded while the entry shape was covered incidentally by its
    // omit-optionals test; both are named here.
    // The empty-metadata arm is not decoration: an omitted `metadata` is
    // the common submission, it is the one field whose absence genuinely
    // needs `#[serde(default)]` on the read shadows, and the ingestion
    // shape has no other test that exercises the map-empty → key-absent →
    // default path.
    let subject = SubjectRef::new("principal-1", Some("user")).expect("valid subject ref");
    for invalidates in [None, Some(target_id())] {
        for subject_ref in [None, Some(subject.clone())] {
            for metadata in [metadata_map([("region", "eu")]), BTreeMap::new()] {
                let mut record = sample_usage_record(subject_ref.clone(), invalidates);
                record.metadata = metadata.clone();
                let encoded = serde_json::to_value(&record).expect("serialize UsageRecord");
                assert_eq!(
                    serde_json::from_value::<UsageRecord>(encoded.clone())
                        .unwrap_or_else(|e| panic!("UsageRecord must decode its own output: {e}")),
                    record,
                    "UsageRecord round-trip must be lossless; encoded as {encoded}",
                );

                let mut submission = sample_create_usage_record(subject_ref.clone(), invalidates);
                submission.metadata = metadata;
                let encoded =
                    serde_json::to_value(&submission).expect("serialize CreateUsageRecord");
                assert_eq!(
                    serde_json::from_value::<CreateUsageRecord>(encoded.clone()).unwrap_or_else(
                        |e| { panic!("CreateUsageRecord must decode its own output: {e}") }
                    ),
                    submission,
                    "CreateUsageRecord round-trip must be lossless; encoded as {encoded}",
                );
            }
        }
    }
}

#[test]
fn the_withdrawal_pair_stays_flat_on_the_wire() {
    // The grouping is a Rust-side shape only. `usage-collector-v1.yaml`
    // declares `invalidates` and `reason_code` as two sibling properties on
    // both shapes, so a nested `invalidation` object would be a silent
    // wire break — invisible to every in-process test that round-trips
    // through the same type.
    let invalidation = serde_json::to_value(sample_usage_record(None, Some(target_id())))
        .expect("serialize an invalidation");
    let object = invalidation.as_object().expect("object");
    assert!(
        object.contains_key("invalidates") && object.contains_key("reason_code"),
        "the pair must serialize as two flat siblings; got {object:?}",
    );
    assert!(
        !object.contains_key("invalidation"),
        "the Rust grouping must not reach the wire; got {object:?}",
    );

    let submission = serde_json::to_value(sample_create_usage_record(None, Some(target_id())))
        .expect("serialize an invalidating submission");
    let object = submission.as_object().expect("object");
    assert!(
        object.contains_key("invalidates") && object.contains_key("reason_code"),
        "the ingestion shape must carry the same two flat siblings; got {object:?}",
    );
    assert!(!object.contains_key("invalidation"));

    for ordinary in [
        serde_json::to_value(sample_usage_record(None, None)).expect("serialize"),
        serde_json::to_value(sample_create_usage_record(None, None)).expect("serialize"),
    ] {
        let object = ordinary.as_object().expect("object");
        for absent in ["invalidates", "reason_code", "invalidation"] {
            assert!(
                !object.contains_key(absent),
                "an ordinary measurement carries no `{absent}`; got {object:?}",
            );
        }
    }
}

#[test]
fn each_entry_shape_serializes_the_exact_wire_key_set() {
    // A round-trip proves the two halves of one codec agree with each
    // other; it cannot prove either agrees with the contract. Drift applied
    // to both shadows of a type — a rename, or a changed value encoding —
    // round-trips perfectly and still breaks every client. These
    // assertions are against literals for that reason, and they are the
    // only thing standing between the wire contract and a symmetric edit.
    let record = serde_json::to_value(sample_usage_record(None, Some(target_id())))
        .expect("serialize an entry");
    let mut keys: Vec<&str> = record
        .as_object()
        .expect("object")
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        [
            "gts_type_id",
            "id",
            "idempotency_key",
            "invalidates",
            "metadata",
            "origin",
            "reason_code",
            "resource_ref",
            "tenant_id",
            "value",
            "window_end",
            "window_start",
        ],
        "the entry shape's wire key set is the contract; got {record}",
    );

    let submission = serde_json::to_value(sample_create_usage_record(None, Some(target_id())))
        .expect("serialize a submission");
    let mut keys: Vec<&str> = submission
        .as_object()
        .expect("object")
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        [
            "gts_type_id",
            "idempotency_key",
            "invalidates",
            "metadata",
            "reason_code",
            "resource_ref",
            "tenant_id",
            "value",
            "window_end",
            "window_start",
        ],
        "the ingestion shape is the entry shape minus the two the server \
         assigns, `id` and `origin`; got {submission}",
    );

    // The quantity is a JSON *string*, never a JSON number, on both shapes.
    // `UsageRecord::value`'s own doc is where this rule is stated: a number
    // round-trips through a client's float and silently loses precision.
    // The encoding is declared twice per type now, so a symmetric removal
    // of `rust_decimal::serde::str` would pass every round-trip.
    for (shape, encoded) in [("entry", &record), ("submission", &submission)] {
        let value = encoded.get("value").expect("every shape carries a value");
        assert_eq!(
            value,
            &json!("42"),
            "the {shape} shape must encode `value` as a decimal string, never a \
             JSON number; got {value}",
        );
        assert!(
            value.is_string(),
            "`value` must be a JSON string on {shape}"
        );
    }

    // The covered-period bounds are RFC 3339 strings on both shapes, for
    // the same reason: the encoding is declared once per shadow.
    for (shape, encoded) in [("entry", &record), ("submission", &submission)] {
        assert_eq!(
            encoded.get("window_start"),
            Some(&json!("1970-01-01T00:00:00Z")),
            "the {shape} shape must encode window_start as RFC 3339",
        );
        assert_eq!(
            encoded.get("window_end"),
            Some(&json!("1970-01-01T01:00:00Z")),
            "the {shape} shape must encode window_end as RFC 3339",
        );
    }
}

#[test]
fn the_reference_does_not_reach_the_derived_identity() {
    // `cpt-cf-usage-collector-adr-record-identity-derivation` excludes the
    // entry type, so an invalidation derives its id from the same five
    // inputs as its target and departs only through its own idempotency
    // key. That is what makes a key reused across the pair collide loudly
    // instead of silently producing two entries.
    let target = sample_create_usage_record(None, None);
    let mut withdrawal = target.clone();
    withdrawal.invalidation = Some(sample_invalidation(target_id()));

    assert_eq!(
        target
            .clone()
            .try_into_usage_record(RecordOrigin::Live)
            .expect("record")
            .id,
        withdrawal
            .try_into_usage_record(RecordOrigin::Live)
            .expect("invalidation")
            .id,
        "adding a reference must not move the derived identity",
    );

    let mut rekeyed = target.clone();
    rekeyed.idempotency_key = IdempotencyKey::new("a-different-key").expect("valid");
    assert_ne!(
        target
            .try_into_usage_record(RecordOrigin::Live)
            .expect("record")
            .id,
        rekeyed
            .try_into_usage_record(RecordOrigin::Live)
            .expect("re-keyed")
            .id,
        "the idempotency key is the one departure that does move it",
    );
}

// ---------------------------------------------------------------------------
// ReasonCode — validated construction and serde routing
// ---------------------------------------------------------------------------

#[test]
fn reason_code_rejects_the_wire_bounds_and_control_characters() {
    // The length bounds are the wire contract's `ReasonCode` schema
    // (`minLength: 1`, `maxLength: 128`), read as bytes rather than code
    // points — the same reading `IdempotencyKey` gives its own 256, and
    // the stricter of the two.
    ReasonCode::new("").expect_err("empty");
    ReasonCode::new("x".repeat(129)).expect_err("over 128 bytes");
    // U+00E9, two bytes in UTF-8: 65 code points, 130 bytes. The cap
    // counts bytes, so this is over it — and an implementation counting
    // code points would accept it, which is the whole content of the
    // divergence `MAX_REASON_CODE_LEN` documents. Every ASCII case above
    // passes under either reading.
    ReasonCode::new("\u{e9}".repeat(65)).expect_err("130 bytes in 65 code points");
    // The schema states no pattern for a reason code, so the control
    // character exclusion is this newtype's own, and stricter: wire
    // hygiene for a value that reaches a plugin, not pre-image safety —
    // the code is no input to the identity derivation.
    ReasonCode::new("emitter\u{7f}duplicate").expect_err("DEL is a control character");
    ReasonCode::new("emitter\u{1f}duplicate").expect_err("0x1F is a control character");
    ReasonCode::new("x".repeat(128)).expect("128 bytes is the boundary, inclusive");
    ReasonCode::new("emitter_duplicate").expect("an ordinary code");
}

#[test]
fn reason_code_deserialize_routes_through_new() {
    let err = serde_json::from_value::<ReasonCode>(json!(""))
        .expect_err("empty code must surface as a serde error");
    assert!(
        err.to_string().contains("reason_code must not be empty"),
        "serde error must carry the Validation detail; got {err}"
    );
}

#[test]
fn reason_code_serializes_transparently() {
    let code = reason_code("emitter_duplicate");
    assert_eq!(
        serde_json::to_value(&code).expect("serialize"),
        json!("emitter_duplicate")
    );
    assert_eq!(code.as_str(), "emitter_duplicate");
    assert_eq!(code.to_string(), "emitter_duplicate");
}

#[test]
fn reason_code_from_str_routes_through_new() {
    assert!(ReasonCode::from_str("emitter_duplicate").is_ok());
    let err = ReasonCode::from_str("").expect_err("empty code must be rejected");
    assert!(matches!(
        err,
        UsageCollectorError::InvalidArgument { ref field, .. } if field.as_str() == "reason_code"
    ));
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
    // A measured decrease is an ordinary entry with a negative quantity, so
    // a bucket can carry one (and a set of them can net to zero). The sign
    // must survive the string wire encoding round-trip.
    let bucket = AggregationBucket {
        key: Vec::new(),
        value: Some(BigDecimal::from(-42)),
    };
    let value = serde_json::to_value(&bucket).expect("serialize");
    assert_eq!(value, json!({ "value": "-42" }));
    let decoded: AggregationBucket = serde_json::from_value(value).expect("round-trip");
    assert_eq!(decoded, bucket);
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
fn the_keyset_safe_allowlist_is_exactly_the_mandatory_record_attributes() {
    // The anchor for the exported set. `is_keyset_safe_record_field` reads
    // `KEYSET_SAFE_RECORD_FIELDS`, and the caller-facing `$orderby`
    // rejection quotes it verbatim, so the constant IS the wire contract
    // and a silent addition to it would widen the admissible order surface
    // with nothing noticing. Spelled out as literals for that reason.
    assert_eq!(
        crate::models::KEYSET_SAFE_RECORD_FIELDS,
        [
            "id",
            "window_start",
            "window_end",
            "tenant_id",
            "resource_id",
            "resource_type",
        ],
    );
}

#[test]
fn every_admissible_order_key_resolves_to_a_column() {
    // Being on the allowlist is necessary but not sufficient: the plugin
    // resolves an order key through `UsageRecordFilterField`, so a name
    // admissible here but absent from the filterable schema is an
    // unmappable order key the gateway happily forwards.
    //
    // Two independently maintained spellings of one vocabulary, and this is
    // the only thing checking they agree in this direction; the converse —
    // a name on the filterable schema that must never be an order key — is
    // `a_derived_field_is_filterable_but_never_an_order_key` below, and
    // neither guard catches the other's case.
    //
    // This replaces a test that re-asserted the same literals the anchor
    // above pins, through a predicate that reads that very constant: it
    // could not fail unless the anchor failed first.
    for field in crate::models::KEYSET_SAFE_RECORD_FIELDS {
        assert!(
            is_keyset_safe_record_field(field),
            "`{field}` is on the allowlist, so the predicate reading it must agree",
        );
        assert!(
            crate::models::UsageRecordFilterField::from_name(field).is_some(),
            "`{field}` is admissible as an order key but has no \
             field-to-column mapping on the filterable schema, so a plugin \
             cannot resolve it",
        );
    }
}

#[test]
fn a_derived_field_is_filterable_but_never_an_order_key() {
    // The converse of the guard above, and the half it cannot supply.
    // Resolving to a column is necessary but not sufficient: `entry_type`
    // is on the filterable schema (the wire contract's `$filter` parameter
    // names it) and therefore resolves through `from_name`, while being a
    // function of the optional `invalidates` that the SDK guarantees no
    // key for. The guard above passes on it. This one does not.
    assert!(
        crate::models::UsageRecordFilterField::from_name("entry_type").is_some(),
        "`entry_type` is a filterable field per the wire contract",
    );
    assert!(
        !is_keyset_safe_record_field("entry_type"),
        "`entry_type` is a function of the optional `invalidates`, so the SDK \
         promises no keyset key over it however present the value is",
    );
}

#[test]
fn the_order_key_refusal_names_the_derived_ground_alongside_the_optional_one() {
    // The refusal detail is caller-facing and interpolates
    // `KEYSET_SAFE_RECORD_FIELDS` verbatim, so it is the wire contract for
    // what may be ordered by. `entry_type` made a third refusal reachable:
    // a recognised filter field that is neither domain-optional nor
    // unrecognised, refused because it is derived. A detail naming only
    // the optional and unknown grounds would misdescribe it, and this is
    // the only thing pinning that the wording covers the case. Lives here
    // rather than beside the constructor because it is a claim about the
    // keyset vocabulary this module owns; `error.rs` carries no sibling
    // test file.
    let err = UsageCollectorError::inadmissible_order_key("entry_type");
    let UsageCollectorError::InvalidArgument {
        ref field,
        ref detail,
        ..
    } = err
    else {
        panic!("expected InvalidArgument, got {err:?}");
    };
    assert_eq!(field, "$orderby");
    assert!(
        detail.contains("entry_type"),
        "the refusal must echo the key the caller sent; got {detail}",
    );
    assert!(
        detail.contains("derived"),
        "a derived key is refused on its own ground, not as an optional or \
         unrecognised one; got {detail}",
    );
    assert!(
        detail.contains("resource_type"),
        "the refusal must name the admissible set so the caller is told what \
         they may order by; got {detail}",
    );
}

#[test]
fn created_at_is_no_longer_a_record_field() {
    // An entry carries a covered period, not a creation instant, so
    // `created_at` has to fail closed on both halves of the read surface
    // rather than resolve to something: it is not an admissible order key,
    // and it is not on the filterable-field schema at all. A stale
    // `$orderby=created_at` must be rejected, never silently reinterpreted.
    assert!(
        !is_keyset_safe_record_field("created_at"),
        "`created_at` is not a record attribute and must not be an order key",
    );
    let err = toolkit_odata::filter::parse_odata_filter::<crate::models::UsageRecordFilterField>(
        "created_at eq 2026-01-01T00:00:00Z",
    )
    .expect_err("created_at is no longer on the filterable-field schema");
    assert!(
        matches!(
            &err,
            toolkit_odata::filter::FilterError::UnknownField(name) if name == "created_at"
        ),
        "expected UnknownField(\"created_at\"), got {err:?}",
    );
}

#[test]
fn the_covered_period_bounds_resolve_on_the_filterable_field_schema() {
    // The bounds are on this schema although a `$filter` may never name
    // them, because the schema is also the plugin's field-to-column
    // mapping and the `$orderby` / cursor-token vocabulary: `window_end`
    // has to resolve to a column for the canonical `(window_end, id)`
    // keyset to mean anything. Parsing succeeding here is therefore the
    // intended state, and the host crate's reserved-filter-field guard —
    // not an `UnknownField` from this schema — is what keeps a predicate
    // off them.
    for expr in [
        "window_start eq 2026-01-01T00:00:00Z",
        "window_end eq 2026-01-01T00:00:00Z",
    ] {
        toolkit_odata::filter::parse_odata_filter::<crate::models::UsageRecordFilterField>(expr)
            .unwrap_or_else(|e| {
                panic!("`{expr}` must resolve against the filterable-field schema: {e:?}")
            });
    }
}

#[test]
fn keyset_unsafe_record_fields_are_the_domain_optional_ones() {
    // `subject_ref` (→ subject_id, subject_type) is `Option`al on
    // `UsageRecord`, and the `invalidates` filter field reads a target that
    // is present only on an invalidation, so all three can be absent and
    // their columns are nullable. A row-value tuple comparison with a NULL
    // leading key evaluates to NULL in Postgres, silently dropping NULL
    // rows from the page — so they are NOT keyset-safe.
    for field in ["subject_id", "subject_type", "invalidates"] {
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
    // Exact match, not a prefix or case-folded one: the allowlist is the
    // whole gate on the `$orderby` surface.
    assert!(!is_keyset_safe_record_field("window_en"));
    assert!(!is_keyset_safe_record_field("window_endd"));
    assert!(!is_keyset_safe_record_field("WINDOW_END"));
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
    // `cpt-cf-usage-collector-adr-record-identity-derivation` concatenates
    // this value under a 0x1F separator, so a control character would break
    // the injectivity the identifier derivation needs.
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

#[test]
fn record_origin_wire_spellings_are_the_two_the_contract_declares() {
    // `RecordOrigin` in usage-collector-v1.yaml is `enum: [live, backfill]`.
    // These strings are a wire contract and a bounded metric-label
    // vocabulary at once, so they are asserted against literals rather
    // than against the enum.
    assert_eq!(RecordOrigin::Live.as_str(), "live");
    assert_eq!(RecordOrigin::Backfill.as_str(), "backfill");
}

#[test]
fn record_origin_serialises_to_its_wire_spelling() {
    assert_eq!(
        serde_json::to_value(RecordOrigin::Live).expect("serializes"),
        serde_json::json!("live"),
    );
    assert_eq!(
        serde_json::to_value(RecordOrigin::Backfill).expect("serializes"),
        serde_json::json!("backfill"),
    );
}

#[test]
fn record_origin_deserialises_from_its_wire_spelling_and_refuses_anything_else() {
    // Both variants, not just one: the read direction is what a consumer
    // decoding a persisted entry depends on, and a value that only
    // round-trips in one direction is the shape a `rename` on a single
    // variant would produce.
    assert_eq!(
        serde_json::from_value::<RecordOrigin>(serde_json::json!("live")).expect("declared value"),
        RecordOrigin::Live,
    );
    assert_eq!(
        serde_json::from_value::<RecordOrigin>(serde_json::json!("backfill"))
            .expect("declared value"),
        RecordOrigin::Backfill,
    );
    // The marker is closed. A third value is a contract violation, not a
    // forward-compatible extension: a consumer that cannot tell imported
    // history from live consumption is the gap the marker exists to close.
    serde_json::from_value::<RecordOrigin>(serde_json::json!("imported"))
        .expect_err("RecordOrigin is closed");
    serde_json::from_value::<RecordOrigin>(serde_json::json!("Live"))
        .expect_err("the wire spelling is lowercase");
}

#[test]
fn the_projection_stamps_the_origin_it_is_handed() {
    let submission = sample_create_usage_record(None, None);
    let live = submission
        .clone()
        .try_into_usage_record(RecordOrigin::Live)
        .expect("valid submission");
    let backfilled = submission
        .try_into_usage_record(RecordOrigin::Backfill)
        .expect("valid submission");

    assert_eq!(live.origin, RecordOrigin::Live);
    assert_eq!(backfilled.origin, RecordOrigin::Backfill);
}

#[test]
fn origin_is_not_an_input_to_the_derived_identity() {
    // The dedup identity is the 5-tuple
    // (tenant, gts_type, key, window_start, window_end) and `origin` is not
    // one of its five members
    // (`cpt-cf-usage-collector-adr-record-identity-derivation`). This is
    // load-bearing rather than incidental: re-importing history that was
    // once emitted live has to collide with the entry it re-creates so the
    // store can absorb it as a duplicate, and it can only collide if the
    // identifier ignores the path.
    let submission = sample_create_usage_record(None, None);
    let live = submission
        .clone()
        .try_into_usage_record(RecordOrigin::Live)
        .expect("valid submission");
    let backfilled = submission
        .try_into_usage_record(RecordOrigin::Backfill)
        .expect("valid submission");

    assert_eq!(live.id, backfilled.id);
}

#[test]
fn a_create_submission_cannot_carry_an_origin() {
    // `origin` is server-assigned, so the ingestion shape has no such
    // property and its `deny_unknown_fields` shadow refuses one. A caller
    // that could name its own path could label imported history as live
    // consumption, which is the distinction the marker exists to make.
    let mut json = serde_json::to_value(sample_create_usage_record(None, None))
        .expect("the submission serializes through its own codec");
    json.as_object_mut()
        .expect("object")
        .insert("origin".to_owned(), json!("live"));

    serde_json::from_value::<CreateUsageRecord>(json)
        .expect_err("origin is server-assigned and must be refused on the create shape");
}

#[test]
fn the_persisted_wire_shape_carries_origin_and_requires_it() {
    let record = sample_create_usage_record(None, None)
        .try_into_usage_record(RecordOrigin::Backfill)
        .expect("valid submission");
    let json = serde_json::to_value(&record).expect("serializes");

    assert_eq!(
        json.get("origin"),
        Some(&json!("backfill")),
        "every persisted entry carries its origin on the wire",
    );

    // Required, not defaulted. An entry decoded from a body with no
    // `origin` has no truthful value to fall back on, and defaulting to
    // `live` would silently relabel imported history as current
    // consumption.
    let mut without = json;
    without.as_object_mut().expect("object").remove("origin");
    serde_json::from_value::<UsageRecord>(without)
        .expect_err("origin is mandatory on the persisted shape");
}

#[test]
fn a_backfill_origin_round_trips_through_both_shadows() {
    // `sample_usage_record` pins `live`, so the general round-trip never
    // exercises the other variant across the two hand-written halves.
    let record = sample_create_usage_record(None, None)
        .try_into_usage_record(RecordOrigin::Backfill)
        .expect("valid submission");
    let json = serde_json::to_value(&record).expect("serializes");

    assert_eq!(
        serde_json::from_value::<UsageRecord>(json).expect("round-trips"),
        record,
    );
}
