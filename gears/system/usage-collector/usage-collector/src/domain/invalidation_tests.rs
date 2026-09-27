//! Unit tests for the faithful-copy comparator.
//!
//! One test per input shape the comparator distinguishes. A single "some
//! field differs" test cannot tell a comparator that checks one field from
//! one that checks eight, so every compared field gets its own case and the
//! two shapes an implementer skips — subject presence against absence, and a
//! negated quantity — are written out explicitly.

use std::collections::BTreeMap;

use time::OffsetDateTime;
use toolkit_gts::gts_id;
use usage_collector_sdk::{
    CreateUsageRecord, EntryType, IdempotencyKey, Invalidation, MetadataKey, MeterTypeId,
    ReasonCode, RecordOrigin, ResourceRef, SubjectRef, UsageCollectorError, UsageQuantity,
    UsageRecord, ValidationReason, WINDOW_END_FIELD, WINDOW_START_FIELD,
};
use uuid::Uuid;

use super::{COMPARED_FIELDS, derive_invalidation_target, verify_invalidation_target};

const SAMPLE_METER_ID: &str = gts_id!("cf.core.uc.usage_record.v1~tenant.example._.foo.v1~");
const SAMPLE_OTHER_METER_ID: &str = gts_id!("cf.core.uc.usage_record.v1~tenant.example._.bar.v1~");

fn meter_id() -> MeterTypeId {
    MeterTypeId::new(SAMPLE_METER_ID).expect("valid meter type id")
}

fn other_meter_id() -> MeterTypeId {
    MeterTypeId::new(SAMPLE_OTHER_METER_ID).expect("valid meter type id")
}

fn qty(value: &str) -> UsageQuantity {
    UsageQuantity::parse(value).expect("valid quantity literal")
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
        entry_type: EntryType::Record,
        gts_type_id: meter_id(),
        tenant_id: Uuid::from_u128(1),
        resource_ref: resource("rsc-1", "compute.vm"),
        subject_ref: None,
        metadata: metadata(&[("region", "eu-central-1")]),
        quantity: qty("42.5"),
        idempotency_key: Some(key("idem-target")),
        invalidation: None,
        window_start: window_start(),
        window_end: window_end(),
    }
}

/// Projects a submission into the persisted shape the SPI would return, so
/// an entry carries a derived identity rather than a hand-picked one.
///
/// The projection is chosen off the submission's declared kind, the way the
/// gateway chooses it, and a withdrawal's target is derived here the same
/// way — so a fixture cannot mint an entry the gateway would not have minted
/// from the same submission.
fn accepted(submission: CreateUsageRecord) -> UsageRecord {
    let kind = submission.entry_type();
    match kind {
        EntryType::Record => submission.try_into_usage_record(RecordOrigin::Live, window_end()),
        EntryType::Invalidation => {
            let target = derive_invalidation_target(&submission)
                .expect("test fixture withdrawal carries an idempotency key");
            submission.try_into_invalidation_record(RecordOrigin::Live, window_end(), target)
        }
    }
    .expect("test fixture submits a well-formed covered period")
}

fn ordinary_target() -> UsageRecord {
    accepted(base_submission())
}

/// A faithful withdrawal of `target`: every compared field echoed, the
/// target's own idempotency key repeated, and the reason code that is one of
/// the two permitted departures. Written as a struct literal so a new
/// `CreateUsageRecord` field fails to compile here rather than defaulting to
/// something the comparator then rejects.
///
/// The repeated key is what makes the fixture honest rather than merely
/// admissible: it is one of the five inputs the target's identifier is
/// derived from, so a withdrawal built this way resolves to `target` and a
/// withdrawal built any other way resolves to nothing.
fn withdrawal_of(target: &UsageRecord) -> CreateUsageRecord {
    CreateUsageRecord {
        entry_type: EntryType::Invalidation,
        gts_type_id: target.gts_type_id.clone(),
        tenant_id: target.tenant_id,
        resource_ref: target.resource_ref.clone(),
        subject_ref: target.subject_ref.clone(),
        metadata: target.metadata.clone(),
        quantity: target.quantity,
        idempotency_key: Some(target.idempotency_key.clone()),
        invalidation: Some(ReasonCode::new("emitter_defect").expect("valid reason code")),
        window_start: target.window_start,
        window_end: target.window_end,
    }
}

/// Calls the comparator the way the gateway does: the reason the submission
/// states, paired with the target the gateway derives from the submission's
/// own identity inputs.
///
/// The pairing is a separate argument on the real signature because only the
/// gateway can build it; every case here pairs it honestly, and the one that
/// does not (`a_rejection_echoes_the_resolved_reference`) calls through
/// directly.
#[track_caller]
#[allow(
    clippy::result_large_err,
    reason = "UsageCollectorError is 144 bytes because Conflict carries invalidated_by/reason_code (SPEC-DIFF 2.2); callers returning Result<_, UsageCollectorError> outside this crate hit the same lint"
)]
fn verify(entry: &CreateUsageRecord, target: &UsageRecord) -> Result<(), UsageCollectorError> {
    let reason = entry
        .invalidation
        .clone()
        .expect("every fixture here submits a withdrawal");
    let resolved =
        derive_invalidation_target(entry).expect("every fixture here carries an idempotency key");
    verify_invalidation_target(
        entry,
        &Invalidation {
            target: resolved,
            reason,
        },
        target,
    )
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
fn a_quantity_at_another_scale_is_a_mismatch() {
    // SPEC-DIFF decision S-B7: the copy repeats the quantity digit for digit.
    let target = ordinary_target();
    let mut entry = withdrawal_of(&target);
    entry.quantity = qty("42.500");
    assert_mismatch(&verify(&entry, &target), "quantity");
}

#[test]
fn a_withdrawal_resolves_its_own_targets_identity() {
    // The whole of "Target resolution": the identifier is derived from the
    // withdrawal's own five identity inputs with `entry_type = record`, and
    // a faithful copy repeats all five, so it lands on the target's `id`.
    // Every other case in this file rests on it — `verify` pairs the
    // comparator with whatever this returns — so it is asserted once,
    // directly, rather than left implicit.
    let target = ordinary_target();
    let entry = withdrawal_of(&target);
    assert_eq!(
        derive_invalidation_target(&entry).expect("the fixture carries a key"),
        target.id,
    );
}

#[test]
fn a_withdrawal_of_a_withdrawal_cannot_be_expressed() {
    // No-invalidation-of-an-invalidation, as DESIGN §3.1 states it: a
    // property of the derivation rather than a check. A submission built to
    // withdraw an invalidation derives its target with
    // `entry_type = record`, so what it resolves is not that invalidation's
    // identity — it is the identity of the measurement they both copy, which
    // is why the rule needs no enforcement of its own.
    let measurement = ordinary_target();
    let withdrawal = accepted(withdrawal_of(&measurement));
    let entry = withdrawal_of(&withdrawal);
    let resolved = derive_invalidation_target(&entry).expect("the fixture carries a key");
    assert_ne!(
        resolved, withdrawal.id,
        "no identifier a withdrawal resolves may belong to an invalidation",
    );
    assert_eq!(
        resolved, measurement.id,
        "it resolves the measurement both entries copy, under entry_type = record",
    );
}

#[test]
fn a_withdrawal_without_a_key_resolves_nothing() {
    // The key is one of the five inputs, so a submission without one names
    // nothing at all. Refused before any lookup, naming the field the caller
    // has to add.
    let target = ordinary_target();
    let mut entry = withdrawal_of(&target);
    entry.idempotency_key = None;
    match derive_invalidation_target(&entry) {
        Err(UsageCollectorError::InvalidArgument { field, .. }) => {
            assert_eq!(field, "idempotency_key");
        }
        other => panic!("expected a missing-key rejection, got {other:?}"),
    }
}

#[test]
fn a_withdrawal_with_another_key_resolves_another_identity() {
    // The key locates the target, so a typo in it is a missing target rather
    // than a field mismatch (DESIGN §3.1, "Faithful copy"). The comparator
    // never sees such a submission: what it resolves is an identifier the
    // ledger does not hold.
    let target = ordinary_target();
    let mut entry = withdrawal_of(&target);
    entry.idempotency_key = Some(key("idem-typo"));
    assert_ne!(
        derive_invalidation_target(&entry).expect("the fixture carries a key"),
        target.id,
    );
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
    entry.quantity = qty("7");
    assert_mismatch(&verify(&entry, &target), "quantity");
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
    entry.quantity = UsageQuantity::try_from(-target.quantity.as_decimal())
        .expect("negated target quantity is still in range");
    assert_mismatch(&verify(&entry, &target), "quantity");
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
fn the_comparator_never_reads_the_idempotency_key() {
    // Not a permitted departure — a withdrawal repeats its target's key —
    // but not a compared field either, and the distinction matters. The key
    // is an input to the derivation that found this row, so by the time the
    // comparator runs the two already agree; a submission whose key differs
    // resolves a different identifier and never reaches here at all
    // (`a_withdrawal_with_another_key_resolves_another_identity`). Handed a
    // mismatched pair directly, the comparator passes it, because it does
    // not read the field.
    let target = ordinary_target();
    let mut entry = withdrawal_of(&target);
    entry.idempotency_key = Some(key("idem-something-else-entirely"));
    let reason = entry.invalidation.clone().expect("the fixture withdraws");
    assert!(
        verify_invalidation_target(
            &entry,
            &Invalidation {
                target: target.id,
                reason,
            },
            &target,
        )
        .is_ok()
    );
}

#[test]
fn the_entrys_reason_code_is_a_permitted_departure() {
    // The second of the two closed departures — `entry_type` is the first,
    // and it is structural here: a withdrawal carries a reason and its
    // target carries none, and neither side is compared.
    let target = ordinary_target();
    let entry = withdrawal_of(&target);
    assert!(target.invalidation.is_none());
    assert!(entry.invalidation.is_some());
    assert_eq!(entry.entry_type(), EntryType::Invalidation);
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
    entry.quantity = qty("7");
    let Err(UsageCollectorError::InvalidArgument { detail, .. }) = verify(&entry, &target) else {
        panic!("expected the copy rejection");
    };
    assert!(
        !detail.contains("42.5"),
        "the rejection must not echo the target's quantity: {detail}",
    );
}

#[test]
fn a_rejection_echoes_the_resolved_reference() {
    // The rejection carries `invalidation.target` — the identifier the
    // gateway resolved and asked the store for — and never the `id` of the
    // row that came back. Its caller refuses a row whose `id` differs, so on
    // the real path the two agree; handed a mis-paired one directly, this is
    // what keeps a row the caller has no scope for from being named back at
    // it.
    let supplied = Uuid::from_u128(0xDEAD_BEEF);
    let reference = Invalidation {
        target: supplied,
        reason: ReasonCode::new("emitter_defect").expect("valid reason code"),
    };

    let measurement = ordinary_target();
    let mut entry = withdrawal_of(&measurement);
    entry.quantity = qty("7");
    assert_echoes_only(
        &verify_invalidation_target(&entry, &reference, &measurement),
        supplied,
        measurement.id,
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
            "quantity",
            "metadata",
        ],
    );
}
