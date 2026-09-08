//! Unit tests for the faithful-copy comparator.
//!
//! One test per input shape the comparator distinguishes. A single "some
//! field differs" test cannot tell a comparator that checks one field from
//! one that checks eight, so every compared field gets its own case and the
//! two shapes an implementer skips — subject presence against absence, and a
//! negated quantity — are written out explicitly.

use std::collections::BTreeMap;
use std::str::FromStr;

use rust_decimal::Decimal;
use time::OffsetDateTime;
use toolkit_gts::gts_id;
use usage_collector_sdk::{
    CreateUsageRecord, IdempotencyKey, Invalidation, MetadataKey, MeterTypeId, ReasonCode,
    RecordOrigin, ResourceRef, SubjectRef, UsageCollectorError, UsageRecord, ValidationReason,
    WINDOW_END_FIELD, WINDOW_START_FIELD,
};
use uuid::Uuid;

use super::{COMPARED_FIELDS, verify_invalidation_target};

const SAMPLE_METER_ID: &str = gts_id!("cf.core.uc.usage_record.v1~tenant.example._.foo.v1~");
const SAMPLE_OTHER_METER_ID: &str = gts_id!("cf.core.uc.usage_record.v1~tenant.example._.bar.v1~");

fn meter_id() -> MeterTypeId {
    MeterTypeId::new(SAMPLE_METER_ID).expect("valid meter type id")
}

fn other_meter_id() -> MeterTypeId {
    MeterTypeId::new(SAMPLE_OTHER_METER_ID).expect("valid meter type id")
}

fn dec(value: &str) -> Decimal {
    Decimal::from_str(value).expect("valid decimal literal")
}

fn key(value: &str) -> IdempotencyKey {
    IdempotencyKey::new(value).expect("valid idempotency key")
}

fn resource(id: &str, kind: &str) -> ResourceRef {
    ResourceRef::new(id, kind).expect("valid resource ref")
}

fn subject(id: &str) -> SubjectRef {
    subject_typed(id, "end_user")
}

fn subject_typed(id: &str, kind: &str) -> SubjectRef {
    SubjectRef::new(id, Some(kind)).expect("valid subject ref")
}

fn metadata(pairs: &[(&str, &str)]) -> BTreeMap<MetadataKey, String> {
    pairs
        .iter()
        .map(|(name, value)| {
            (
                MetadataKey::new(*name).expect("valid metadata key"),
                (*value).to_owned(),
            )
        })
        .collect()
}

fn window_start() -> OffsetDateTime {
    OffsetDateTime::UNIX_EPOCH
}

fn window_end() -> OffsetDateTime {
    OffsetDateTime::UNIX_EPOCH + time::Duration::hours(1)
}

/// The measurement every case below withdraws, as its emitter submitted it.
fn base_submission() -> CreateUsageRecord {
    CreateUsageRecord {
        gts_type_id: meter_id(),
        tenant_id: Uuid::from_u128(1),
        resource_ref: resource("rsc-1", "compute.vm"),
        subject_ref: None,
        metadata: metadata(&[("region", "eu-central-1")]),
        value: dec("42.5"),
        idempotency_key: key("idem-target"),
        invalidation: None,
        window_start: window_start(),
        window_end: window_end(),
    }
}

/// Projects a submission into the persisted shape the SPI would return, so
/// the target carries a derived identity rather than a hand-picked one.
fn accepted(submission: CreateUsageRecord) -> UsageRecord {
    submission
        .try_into_usage_record(RecordOrigin::Live)
        .expect("test fixture submits a well-formed covered period")
}

fn ordinary_target() -> UsageRecord {
    accepted(base_submission())
}

/// A faithful withdrawal of `target`: every compared field echoed, the
/// entry's own idempotency key, and the withdrawal reference. Written as a
/// struct literal so a new `CreateUsageRecord` field fails to compile here
/// rather than defaulting to something the comparator then rejects.
fn withdrawal_of(target: &UsageRecord) -> CreateUsageRecord {
    CreateUsageRecord {
        gts_type_id: target.gts_type_id.clone(),
        tenant_id: target.tenant_id,
        resource_ref: target.resource_ref.clone(),
        subject_ref: target.subject_ref.clone(),
        metadata: target.metadata.clone(),
        value: target.value,
        idempotency_key: key("idem-withdrawal"),
        invalidation: Some(Invalidation {
            target: target.id,
            reason: ReasonCode::new("emitter_defect").expect("valid reason code"),
        }),
        window_start: target.window_start,
        window_end: target.window_end,
    }
}

/// Calls the comparator with the withdrawal the entry itself carries.
///
/// The reference is a separate argument on the real signature so that a
/// rejection can only ever echo a uuid the caller supplied; every case here
/// pairs it honestly, and the one that does not
/// (`a_rejection_echoes_the_reference_the_caller_supplied`) calls through
/// directly.
#[track_caller]
fn verify(entry: &CreateUsageRecord, target: &UsageRecord) -> Result<(), UsageCollectorError> {
    let invalidation = entry
        .invalidation
        .as_ref()
        .expect("every fixture here submits a withdrawal");
    verify_invalidation_target(entry, invalidation, target)
}

#[track_caller]
fn assert_mismatch(outcome: &Result<(), UsageCollectorError>, expected_field: &str) {
    match outcome {
        Err(UsageCollectorError::InvalidArgument {
            field,
            reason: ValidationReason::InvalidationFieldMismatch,
            ..
        }) => assert_eq!(
            field, expected_field,
            "the rejection must name the field that differs",
        ),
        other => panic!("expected an InvalidationFieldMismatch, got {other:?}"),
    }
}

#[track_caller]
fn assert_target_not_record(outcome: &Result<(), UsageCollectorError>) {
    match outcome {
        Err(UsageCollectorError::InvalidArgument {
            field,
            reason: ValidationReason::InvalidationTargetNotRecord,
            ..
        }) => assert_eq!(field, "invalidates"),
        other => panic!("expected an InvalidationTargetNotRecord, got {other:?}"),
    }
}

#[track_caller]
fn assert_echoes_only(outcome: &Result<(), UsageCollectorError>, supplied: Uuid, row_id: Uuid) {
    let Err(UsageCollectorError::InvalidArgument {
        resource_name,
        detail,
        ..
    }) = outcome
    else {
        panic!("expected a rejection, got {outcome:?}");
    };
    assert_eq!(
        resource_name.as_deref(),
        Some(supplied.to_string().as_str())
    );
    assert!(
        detail.contains(&supplied.to_string()),
        "the rejection must name the reference the caller supplied: {detail}",
    );
    assert!(
        !detail.contains(&row_id.to_string()),
        "the rejection must not name the row that was read: {detail}",
    );
}

#[test]
fn a_faithful_copy_is_admissible() {
    let target = ordinary_target();
    let entry = withdrawal_of(&target);
    assert!(verify(&entry, &target).is_ok());
}

#[test]
fn a_faithful_copy_carrying_a_subject_is_admissible() {
    // The `Option` is compared whole, so the equal-and-present case has to
    // be asserted as well as the two unequal ones: a comparator that read
    // presence alone would pass all three.
    let mut with_subject = base_submission();
    with_subject.subject_ref = Some(subject("s-1"));
    let target = accepted(with_subject);
    let entry = withdrawal_of(&target);
    assert!(verify(&entry, &target).is_ok());
}

#[test]
fn a_copy_whose_bounds_carry_another_offset_is_admissible() {
    // `time::OffsetDateTime`'s equality compares the instant, not the
    // rendering, so an emitter that echoes the covered period in its own
    // zone is still a faithful copy.
    //
    // Load-bearing for the comparator's signature: it runs against a
    // submission that has not been through
    // `CreateUsageRecord::try_into_usage_record`'s UTC normalization while
    // the target has. A comparison that ever became rendering-based would
    // reject every such copy, and would do it silently, because nothing
    // else here would change.
    let target = ordinary_target();
    let mut entry = withdrawal_of(&target);
    let east = time::UtcOffset::from_hms(5, 0, 0).expect("valid offset");
    entry.window_start = target.window_start.to_offset(east);
    entry.window_end = target.window_end.to_offset(east);
    assert_ne!(
        entry.window_start.offset(),
        target.window_start.offset(),
        "the fixture must change the rendering, or this asserts nothing",
    );
    assert!(verify(&entry, &target).is_ok());
}

#[test]
fn a_quantity_equal_at_another_scale_is_admissible() {
    // `rust_decimal::Decimal`'s equality is numeric, not textual: `42.500`
    // equals `42.5`, and such a withdrawal is admitted. The three grounds
    // for that, and the residue it leaves, are on the `value` comparison
    // itself; this pins the decision.
    let target = ordinary_target();
    let mut entry = withdrawal_of(&target);
    entry.value = dec("42.500");
    assert_ne!(
        entry.value.to_string(),
        target.value.to_string(),
        "the fixture must change the rendering, or this asserts nothing",
    );
    assert!(verify(&entry, &target).is_ok());
}

#[test]
fn a_target_that_is_itself_an_invalidation_is_rejected() {
    // Withdrawal applies to measurements: the entry that withdrew one is
    // not itself withdrawable, and a correction is permanent
    // (`cpt-cf-usage-collector-adr-append-only-invalidation`).
    let measurement = ordinary_target();
    let target = accepted(withdrawal_of(&measurement));
    let entry = withdrawal_of(&target);
    assert_target_not_record(&verify(&entry, &target));
}

#[test]
fn the_target_kind_is_checked_before_the_copy() {
    // A submission that both names an invalidation and departs from it is
    // told about the target, not about the field: the field is not the
    // fault it has to fix. Order is the whole assertion here — swap the two
    // checks and this reports a `value` mismatch instead.
    let measurement = ordinary_target();
    let target = accepted(withdrawal_of(&measurement));
    let mut entry = withdrawal_of(&target);
    entry.value = dec("7");
    assert_target_not_record(&verify(&entry, &target));
}

#[test]
fn a_different_tenant_is_a_mismatch() {
    let target = ordinary_target();
    let mut entry = withdrawal_of(&target);
    entry.tenant_id = Uuid::from_u128(99);
    assert_mismatch(&verify(&entry, &target), "tenant_id");
}

#[test]
fn a_different_meter_is_a_mismatch() {
    let target = ordinary_target();
    let mut entry = withdrawal_of(&target);
    entry.gts_type_id = other_meter_id();
    assert_mismatch(&verify(&entry, &target), "gts_type_id");
}

#[test]
fn a_different_resource_id_is_a_mismatch() {
    let target = ordinary_target();
    let mut entry = withdrawal_of(&target);
    entry.resource_ref = resource("rsc-other", "compute.vm");
    assert_mismatch(&verify(&entry, &target), "resource_ref");
}

#[test]
fn a_different_resource_type_is_a_mismatch() {
    // The composite is compared whole, so either component carries the
    // rejection under the one field name the caller submitted.
    let target = ordinary_target();
    let mut entry = withdrawal_of(&target);
    entry.resource_ref = resource("rsc-1", "storage.volume");
    assert_mismatch(&verify(&entry, &target), "resource_ref");
}

#[test]
fn subject_presence_against_absence_is_a_mismatch() {
    // `cpt-cf-usage-collector-adr-append-only-invalidation` names this case
    // specifically: a comparator written as
    // `a.zip(b).all(|(x, y)| x == y)` passes it, because zipping a `Some`
    // with a `None` yields nothing to disagree about.
    let mut with_subject = base_submission();
    with_subject.subject_ref = Some(subject("s-1"));
    let target = accepted(with_subject);
    let mut entry = withdrawal_of(&target);
    entry.subject_ref = None;
    assert_mismatch(&verify(&entry, &target), "subject_ref");

    let target = ordinary_target();
    let mut entry = withdrawal_of(&target);
    entry.subject_ref = Some(subject("s-1"));
    assert_mismatch(&verify(&entry, &target), "subject_ref");
}

#[test]
fn a_different_subject_is_a_mismatch() {
    let mut with_subject = base_submission();
    with_subject.subject_ref = Some(subject("s-1"));
    let target = accepted(with_subject);
    let mut entry = withdrawal_of(&target);
    entry.subject_ref = Some(subject("s-2"));
    assert_mismatch(&verify(&entry, &target), "subject_ref");
}

#[test]
fn a_different_subject_type_is_a_mismatch() {
    // `SubjectRef` is a two-component composite exactly as `ResourceRef` is,
    // and the discriminator half is the one a comparator reaching for
    // `subject_id` alone would drop.
    let mut with_subject = base_submission();
    with_subject.subject_ref = Some(subject_typed("s-1", "end_user"));
    let target = accepted(with_subject);
    let mut entry = withdrawal_of(&target);
    entry.subject_ref = Some(subject_typed("s-1", "service_account"));
    assert_mismatch(&verify(&entry, &target), "subject_ref");
}

#[test]
fn a_different_window_start_is_a_mismatch() {
    let target = ordinary_target();
    let mut entry = withdrawal_of(&target);
    entry.window_start = target.window_start - time::Duration::hours(1);
    assert_mismatch(&verify(&entry, &target), "window_start");
}

#[test]
fn a_different_window_end_is_a_mismatch() {
    let target = ordinary_target();
    let mut entry = withdrawal_of(&target);
    entry.window_end = target.window_end + time::Duration::hours(1);
    assert_mismatch(&verify(&entry, &target), "window_end");
}

#[test]
fn a_different_quantity_is_a_mismatch() {
    let target = ordinary_target();
    let mut entry = withdrawal_of(&target);
    entry.value = dec("7");
    assert_mismatch(&verify(&entry, &target), "value");
}

#[test]
fn the_quantity_is_echoed_and_never_negated() {
    // Echo, not compensation: the copied quantity restates what is
    // withdrawn so a reader of the withdrawal alone can see what it
    // removes. A negated one is the signed-compensation model this gear
    // rejected, and a fold that admitted it would double-count the
    // measurement it was meant to remove.
    let target = ordinary_target();
    let mut entry = withdrawal_of(&target);
    entry.value = -target.value;
    assert_mismatch(&verify(&entry, &target), "value");
}

#[test]
fn a_changed_metadata_value_is_a_mismatch() {
    let target = ordinary_target();
    let mut entry = withdrawal_of(&target);
    entry.metadata = metadata(&[("region", "us-east-1")]);
    assert_mismatch(&verify(&entry, &target), "metadata");
}

#[test]
fn an_extra_metadata_key_is_a_mismatch() {
    // The copy is exact, not a superset: metadata keeps the withdrawal
    // inside the same grouped and filtered reads that surfaced the target,
    // which an added key would move it out of.
    let target = ordinary_target();
    let mut entry = withdrawal_of(&target);
    entry.metadata = metadata(&[("region", "eu-central-1"), ("tier", "gold")]);
    assert_mismatch(&verify(&entry, &target), "metadata");
}

#[test]
fn a_dropped_metadata_key_is_a_mismatch() {
    // The subset direction, which a containment check would admit. A
    // dropped key moves the withdrawal out of the grouped and filtered
    // reads that surfaced the target exactly as an added one does.
    let mut two_keys = base_submission();
    two_keys.metadata = metadata(&[("region", "eu-central-1"), ("tier", "gold")]);
    let target = accepted(two_keys);
    let mut entry = withdrawal_of(&target);
    entry.metadata = metadata(&[("region", "eu-central-1")]);
    assert_mismatch(&verify(&entry, &target), "metadata");
}

#[test]
fn the_entrys_own_idempotency_key_is_a_permitted_departure() {
    // One of the three closed departures. The entry needs a key of its own:
    // reusing the target's collides on all five dedup attributes.
    let target = ordinary_target();
    let mut entry = withdrawal_of(&target);
    entry.idempotency_key = key("idem-something-else-entirely");
    assert!(verify(&entry, &target).is_ok());
}

#[test]
fn the_entrys_reason_code_is_a_permitted_departure() {
    // The second and third departures arrive together, because the
    // reference and the reason are one field: the target carries neither
    // and the entry carries both, and neither is compared.
    let target = ordinary_target();
    let entry = withdrawal_of(&target);
    assert!(target.invalidation.is_none());
    assert!(entry.invalidation.is_some());
    assert!(verify(&entry, &target).is_ok());
}

#[test]
fn a_rejection_names_what_differs_and_never_what_it_differs_from() {
    // The target is read with no caller scope — a shape check after the
    // submitter's own PDP authorization, not a caller-scoped read — so the
    // target's field values were never authorized to this caller. A
    // diagnostic echoing one turns the 400 into an oracle: submit a
    // faithful copy with one field wrong, read the real value out of the
    // rejection, iterate per field, and a record out of scope is
    // reconstructed. The field name is safe (the caller sent it); the value
    // is not.
    let target = ordinary_target();
    let mut entry = withdrawal_of(&target);
    entry.value = dec("7");
    let Err(UsageCollectorError::InvalidArgument { detail, .. }) = verify(&entry, &target) else {
        panic!("expected the copy rejection");
    };
    assert!(
        !detail.contains("42.5"),
        "the rejection must not echo the target's quantity: {detail}",
    );
}

#[test]
fn a_rejection_echoes_the_reference_the_caller_supplied() {
    // Both rejections carry `invalidation.target` — the uuid the caller
    // sent — and never the id of the row that was read. The lookup that
    // produced that row is unscoped, so a mis-paired one would otherwise
    // hand the caller an identifier it never supplied and has no scope for.
    // Pairing is the caller's obligation; this is what keeps a broken
    // pairing from leaking.
    let supplied = Uuid::from_u128(0xDEAD_BEEF);
    let reference = Invalidation {
        target: supplied,
        reason: ReasonCode::new("emitter_defect").expect("valid reason code"),
    };

    let measurement = ordinary_target();
    let mut entry = withdrawal_of(&measurement);
    entry.value = dec("7");
    assert_echoes_only(
        &verify_invalidation_target(&entry, &reference, &measurement),
        supplied,
        measurement.id,
    );

    let invalidation_target = accepted(withdrawal_of(&measurement));
    let entry = withdrawal_of(&invalidation_target);
    assert_echoes_only(
        &verify_invalidation_target(&entry, &reference, &invalidation_target),
        supplied,
        invalidation_target.id,
    );
}

#[test]
fn the_covered_period_names_are_the_sdk_field_constants() {
    // Six of the eight compared names have no constant to be spelled from,
    // so the list is literals for one consistent reading. These two do have
    // one, and the vocabulary is tied to it here rather than in the
    // spelling, so a wire rename of either bound cannot split the two
    // halves silently.
    assert_eq!(COMPARED_FIELDS[4], WINDOW_START_FIELD);
    assert_eq!(COMPARED_FIELDS[5], WINDOW_END_FIELD);
}

#[test]
fn every_compared_field_has_a_case_above() {
    // The comparator destructures both shapes so a new field is a compile
    // error rather than a silently uncompared one. This asserts the other
    // half: that each compared field is exercised by a case here, by
    // walking the names the comparator can return.
    //
    // Read as a checklist, not a mechanism — it is a literal list, and it
    // is the thing a reviewer diffs against the struct.
    assert_eq!(
        COMPARED_FIELDS,
        [
            "tenant_id",
            "gts_type_id",
            "resource_ref",
            "subject_ref",
            "window_start",
            "window_end",
            "value",
            "metadata",
        ],
    );
}
