#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::collections::BTreeMap;

use uuid::Uuid;

use super::MeterRef;
use crate::derive_usage_record_id;
use crate::models::{
    EntryType, IdempotencyKey, MeterTypeId, RecordOrigin, ResourceRef, UsageRecord,
};
use crate::quantity::UsageQuantity;

const METER: &str = "gts.cf.core.uc.usage_record.v1~example.metering._.stored_volume.v1~";
const OTHER_METER: &str = "gts.cf.core.uc.usage_record.v1~example.metering._.network_egress.v1~";

/// An arbitrary reference. **Not** `METER`'s real derivation, and nothing
/// below depends on it being one: this file's subject is that the two UUIDs
/// on a stored record are independent, so using a derived value here would
/// quietly couple the very things the tests assert are uncoupled. The real
/// identifier↔reference derivation is pinned by the gear's own
/// compatibility fixture.
const METER_UUID: Uuid = Uuid::from_u128(0x0000_00a1_0000_4000_8000_0000_0000_0001);

const TENANT: Uuid = Uuid::from_u128(0x0000_0001_0000_4000_8000_0000_0000_0001);

/// `1970-01-01T00:00:00Z`. Built by arithmetic on the epoch rather than
/// with `time::macros::datetime!`, which this crate does not enable — see
/// the same note in `contract/reference.rs`.
const WINDOW_START: time::OffsetDateTime = time::OffsetDateTime::UNIX_EPOCH;
const WINDOW_END: time::OffsetDateTime = WINDOW_START.saturating_add(time::Duration::hours(1));
const ACCEPTED_AT: time::OffsetDateTime = WINDOW_END.saturating_add(time::Duration::minutes(5));

fn meter_id() -> MeterTypeId {
    MeterTypeId::new(METER).expect("the fixture meter id is valid")
}

fn key() -> IdempotencyKey {
    IdempotencyKey::new("fixture-key").expect("valid idempotency key")
}

fn sample_usage_record_for(gts_type_id: MeterTypeId) -> UsageRecord {
    UsageRecord {
        id: derive_usage_record_id(
            TENANT,
            &gts_type_id,
            &key(),
            WINDOW_START,
            WINDOW_END,
            EntryType::Record,
        ),
        gts_type_id,
        tenant_id: TENANT,
        resource_ref: ResourceRef::new("vm-1", "compute.vm").expect("valid resource ref"),
        subject_ref: None,
        metadata: BTreeMap::new(),
        quantity: UsageQuantity::parse("42").expect("valid quantity"),
        idempotency_key: key(),
        accepted_at: ACCEPTED_AT,
        origin: RecordOrigin::Live,
        invalidation: None,
        window_start: WINDOW_START,
        window_end: WINDOW_END,
    }
}

fn sample_usage_record() -> UsageRecord {
    sample_usage_record_for(meter_id())
}

#[test]
fn a_record_round_trips_through_both_conversions_unchanged() {
    let original = sample_usage_record();

    let stored = original.clone().into_stored(METER_UUID);
    let back = stored.into_usage_record(meter_id());

    assert_eq!(
        back, original,
        "the two conversions must be inverses for every field, not just the meter"
    );
}

#[test]
fn into_stored_replaces_the_identifier_with_the_reference_it_is_given() {
    let stored = sample_usage_record().into_stored(METER_UUID);

    assert_eq!(stored.gts_type_uuid, METER_UUID);
}

/// The §3.6 invariant, first half. A stored record carries two UUIDs that
/// are related to each other by nothing, and the reference is **not** an
/// input to entry identity
/// (`cpt-cf-usage-collector-adr-record-identity-derivation`). Re-keying
/// identity onto the reference would silently change every stored `id` in
/// an installation, which is the "simplification" this pair of tests exists
/// to make fail loudly.
#[test]
fn the_entry_identity_ignores_the_reference_entirely() {
    let base = sample_usage_record();

    let under_one = base.clone().into_stored(METER_UUID);
    let under_another = base.into_stored(Uuid::from_u128(0xdead_beef));

    assert_eq!(
        under_one.id, under_another.id,
        "the reference is not an input to entry identity"
    );
    assert_ne!(
        under_one.id, under_one.gts_type_uuid,
        "the two UUIDs on a stored record are unrelated"
    );
}

/// The §3.6 invariant, second half: identity *does* track the identifier,
/// so the first half is not passing merely because `into_stored` happens to
/// leave `id` alone for every input.
#[test]
fn the_entry_identity_tracks_the_meter_identifier() {
    let one = sample_usage_record().into_stored(METER_UUID);
    let other = sample_usage_record_for(MeterTypeId::new(OTHER_METER).expect("valid meter id"))
        .into_stored(METER_UUID);

    assert_ne!(
        one.id, other.id,
        "two meters sharing every other identity input must derive distinct entry identities"
    );
}

/// And identity is the six-tuple derivation itself, not something
/// `into_stored` recomputes on the way past.
#[test]
fn into_stored_leaves_the_six_tuple_derived_identity_alone() {
    let stored = sample_usage_record().into_stored(METER_UUID);

    assert_eq!(
        stored.id,
        derive_usage_record_id(
            TENANT,
            &meter_id(),
            &key(),
            WINDOW_START,
            WINDOW_END,
            EntryType::Record,
        )
    );
}

#[test]
fn caller_supplied_eq_separates_two_records_differing_only_in_their_meter() {
    let one = sample_usage_record().into_stored(METER_UUID);
    let mut two = one.clone();
    two.gts_type_uuid = Uuid::from_u128(0xdead_beef);

    assert!(!one.caller_supplied_eq(&two));
}

#[test]
fn caller_supplied_eq_ignores_the_three_server_assigned_fields() {
    let one = sample_usage_record().into_stored(METER_UUID);
    let mut two = one.clone();
    two.id = Uuid::from_u128(0x1234);
    two.accepted_at = ACCEPTED_AT.saturating_add(time::Duration::days(365));
    two.origin = RecordOrigin::Backfill;

    assert!(
        one.caller_supplied_eq(&two),
        "id, accepted_at and origin are server-assigned and must not be compared"
    );
}

#[test]
fn entry_type_reads_the_withdrawal_the_record_carries() {
    let measurement = sample_usage_record().into_stored(METER_UUID);

    assert_eq!(measurement.entry_type(), EntryType::Record);
}

#[test]
fn a_meter_ref_carries_both_values_and_derives_neither() {
    let meter = MeterRef::new(METER_UUID, meter_id());

    assert_eq!(meter.uuid, METER_UUID);
    assert_eq!(meter.id.as_str(), METER);
}
