//! Unit tests for the batch write's orchestration, all DB-free.
//!
//! Three things are pinned here, each invisible everywhere else: the identity
//! the write path decides on and the plan it collapses a batch into, the
//! per-column pivot that binds the statement (a swapped push there corrupts
//! every batched row while every other test still passes), and the two
//! resolution paths that do not need a connection — `resolve_dedup_hit`'s
//! absorb/conflict/decode arms and `resolve_batch`'s defensive one.
//!
//! The statement text these bind is `super::super::query::write`'s own
//! oracle. The `.bind()` sequence and the live transaction need a backend:
//! `records_ingest_integration_pg` and `acceptance_slack_pg`.

// Test modules using bare `panic!` opt in explicitly
// (clippy.toml allows unwrap/expect in tests, not panic).
#![allow(clippy::panic)]

use std::collections::HashMap;

use usage_collector_sdk::UsageCollectorPluginError;

use super::{Admission, ConflictRead, InsertColumns, entry_identity, plan_batch};
use crate::infra::storage::test_support::{
    VCPU_METER, lazy_store, qty, row_matching, unit_meter, unit_record,
};

/// A withdrawal of `target`: the same shape as [`unit_record`] plus the
/// invalidation pair. A faithful withdrawal copies its target's covered period,
/// and every one built here does.
fn withdrawal(
    tenant: uuid::Uuid,
    idem: &str,
    seq: u128,
    target: uuid::Uuid,
) -> usage_collector_sdk::StoredUsageRecord {
    usage_collector_sdk::StoredUsageRecord {
        invalidation: Some(usage_collector_sdk::Invalidation {
            target,
            reason: usage_collector_sdk::ReasonCode::new("duplicate_submission")
                .expect("valid reason code"),
        }),
        ..unit_record(tenant, idem, seq)
    }
}

/// The `(meter, entry)` pairs the batch planner and the batch insert take,
/// every entry under [`unit_meter`]. A helper rather than a per-test `map`,
/// because what these tests are about is the planning and the pivot, not which
/// meter an entry names.
fn under_unit_meter(
    records: Vec<usage_collector_sdk::StoredUsageRecord>,
) -> Vec<(
    usage_collector_sdk::MeterRef,
    usage_collector_sdk::StoredUsageRecord,
)> {
    records
        .into_iter()
        .map(|record| (unit_meter(), record))
        .collect()
}

// --- The identity the write path decides on ---
//
// `entry_identity` is the entry's own `id`, and the derivation behind it is the
// SDK's, pinned by `usage_collector_sdk::id_tests`. Nothing is restated here,
// because a plugin-side copy of the derivation could only ever agree with
// itself, and `id_uniqueness_integration_pg` pins a different claim (that
// distinct ids are separately addressable in a live ledger).
//
// What *is* this file's to pin is the consequence: two input rows that share an
// identity take one slot, and two that do not take two.

#[test]
fn entry_identity_is_the_entry_id_and_reads_nothing_else() {
    // One assertion, and it is the whole definition. The write path rests on a
    // caller-supplied value: a version of this function that recomputed the
    // identity from the record's fields would be a second derivation beside the
    // gateway's, and the two could disagree about what one entry is.
    let tenant = uuid::Uuid::from_u128(0x2A);
    let mut record = unit_record(tenant, "k", 42);
    assert_eq!(entry_identity(&record), record.id);

    // Moving a field that *is* one of the six derivation inputs does not move
    // the identity, because this function does not re-derive: only the `id`
    // does, and the gateway is what sets it.
    record.window_end += time::Duration::hours(1);
    assert_eq!(
        entry_identity(&record),
        uuid::Uuid::from_u128(42),
        "the identity is the id as dispatched, not a re-derivation of it"
    );
}

#[tokio::test]
async fn resolve_dedup_hit_surfaces_a_corrupt_stored_row_as_internal() {
    let store = lazy_store();
    let record = unit_record(uuid::Uuid::from_u128(7), "k", 700);
    let row = row_matching(&record, serde_json::Value::String("corrupt".to_owned()));
    assert!(
        matches!(
            store.resolve_dedup_hit(row, &record),
            Err(UsageCollectorPluginError::Internal(_))
        ),
        "stored-data corruption must not read as an absorb or a conflict"
    );
}

#[tokio::test]
async fn resolve_dedup_hit_absorbs_an_exact_match_and_returns_the_stored_entry() {
    let store = lazy_store();
    let record = unit_record(uuid::Uuid::from_u128(8), "k", 800);
    let mut row = row_matching(&record, serde_json::Value::Object(serde_json::Map::new()));
    row.accepted_at = record.accepted_at + time::Duration::hours(1);
    let stored_at = row.accepted_at;

    let absorbed = store
        .resolve_dedup_hit(row, &record)
        .expect("equal caller-supplied fields absorb");
    assert_eq!(
        absorbed.accepted_at, stored_at,
        "the stored entry is returned"
    );
}

#[tokio::test]
async fn resolve_dedup_hit_conflicts_on_a_caller_supplied_field_and_carries_the_stored_entry() {
    let store = lazy_store();
    let record = unit_record(uuid::Uuid::from_u128(9), "k", 900);
    let mut row = row_matching(&record, serde_json::Value::Object(serde_json::Map::new()));
    row.quantity = rust_decimal::Decimal::new(999, 0);

    match store.resolve_dedup_hit(row, &record) {
        Err(UsageCollectorPluginError::IdempotencyConflict { existing, .. }) => {
            assert_eq!(existing.id, record.id);
            assert_eq!(existing.quantity.to_string(), "999");
        }
        other => panic!("a differing quantity must conflict, got {other:?}"),
    }
}

#[tokio::test]
async fn resolve_dedup_hit_ignores_origin() {
    let store = lazy_store();
    let record = unit_record(uuid::Uuid::from_u128(11), "k", 1100);
    let mut row = row_matching(&record, serde_json::Value::Object(serde_json::Map::new()));
    row.origin = usage_collector_sdk::RecordOrigin::Backfill
        .as_str()
        .to_owned();
    assert!(
        store.resolve_dedup_hit(row, &record).is_ok(),
        "origin is server-assigned and not compared"
    );
}

#[tokio::test]
async fn resolve_dedup_hit_compares_the_quantity_digit_for_digit() {
    let store = lazy_store();
    let record = unit_record(uuid::Uuid::from_u128(13), "k", 1300);
    let mut row = row_matching(&record, serde_json::Value::Object(serde_json::Map::new()));
    row.quantity = rust_decimal::Decimal::new(1000, 3); // 1.000 against the record's 1
    assert!(
        matches!(
            store.resolve_dedup_hit(row, &record),
            Err(UsageCollectorPluginError::IdempotencyConflict { .. })
        ),
        "1.000 is not 1 digit for digit"
    );
}

#[tokio::test]
async fn resolve_dedup_hit_compares_the_reason_code() {
    let store = lazy_store();
    let record = withdrawal(
        uuid::Uuid::from_u128(12),
        "k",
        1200,
        uuid::Uuid::from_u128(0x1200),
    );
    let mut row = row_matching(&record, serde_json::Value::Object(serde_json::Map::new()));
    row.reason_code = Some("late_correction".to_owned());
    assert!(matches!(
        store.resolve_dedup_hit(row, &record),
        Err(UsageCollectorPluginError::IdempotencyConflict { .. })
    ));
}


/// The ledger's one meter column is written from the call's own `MeterRef`,
/// never from the record beside it.
///
/// Asserted over [`InsertColumns::build`] because this crate has no model-to-row
/// function: the write path pivots through this struct, so this is the one place
/// in-process where the meter column is produced. The stored column is read back
/// against a live ledger by
/// `records_ingest_integration_pg::a_stored_record_round_trips_its_registry_reference`.
#[test]
fn the_meter_column_is_written_from_the_one_meter_ref_it_is_given() {
    // Deliberately **not** `unit_meter()`: this reference could not have been
    // derived from the identifier beside it, so a build that filled
    // `gts_type_uuid` from anything but the `MeterRef` it was handed cannot
    // produce this value by accident.
    let meter = usage_collector_sdk::MeterRef::new(
        uuid::Uuid::from_u128(0x1234),
        usage_collector_sdk::MeterTypeId::new(VCPU_METER).expect("valid meter id"),
    );
    let entry = (
        meter.clone(),
        unit_record(uuid::Uuid::from_u128(0xDD), "meter-column", 0xDD01),
    );

    let cols = InsertColumns::build(&[&entry], &[1]);

    assert_eq!(
        cols.gts_type_uuids,
        vec![meter.uuid],
        "the reference column is the meter's, never the record's"
    );
}

// --- Batch planning ---

#[test]
fn plan_batch_collapses_and_sorts_distinct_identities() {
    let tenant = uuid::Uuid::from_u128(1);
    let mk = |idem: &str, seq: u128| unit_record(tenant, idem, seq);
    // `unit_record` stamps the `id` from its third argument, standing in for the
    // gateway: idx 2 repeats idx 0's, which is what a faithful gateway emits for
    // a resubmission of one entry.
    //
    // **The input order disagrees with the identity order deliberately**: a
    // fixture whose first occurrences already ascend leaves `reps.sort_by_key` a
    // no-op, so deleting the sort would keep it green. This is the only in-process
    // oracle for `plan_batch`'s deadlock-freedom ordering.
    let records = vec![
        mk("kb", 13), // idx 0
        mk("ka", 11), // idx 1
        mk("kb", 13), // idx 2 — same identity as idx 0
        mk("kc", 10), // idx 3
    ];

    let records = under_unit_meter(records);
    let plan = plan_batch(&records);

    // First occurrences arrive as 13, 11, 10 and come out 10, 11, 13. The sort
    // is not by idempotency key and must not be read as one -- a real `id` is a
    // digest and orders arbitrarily, which is why any total order will do.
    let ids: Vec<uuid::Uuid> = plan.reps.iter().map(|(_, r)| r.id).collect();
    assert_eq!(
        ids,
        vec![
            uuid::Uuid::from_u128(10),
            uuid::Uuid::from_u128(11),
            uuid::Uuid::from_u128(13)
        ],
        "distinct, and ascending by identity whatever order they arrived in"
    );

    // `first_index` is the input index, so it is unaffected by the sort: the
    // two are separate facts and a plan that sorted `first_index`'s values too
    // would resolve every in-batch duplicate against the wrong row.
    assert_eq!(
        plan.first_index[&entry_identity(&records[1].1)],
        1,
        "ka first at idx 1"
    );
    assert_eq!(
        plan.first_index[&entry_identity(&records[0].1)],
        0,
        "kb first at idx 0"
    );
    assert_eq!(
        plan.first_index[&entry_identity(&records[3].1)],
        3,
        "kc first at idx 3"
    );

    let (_, kb_rep) = plan
        .reps
        .iter()
        .find(|(_, r)| r.idempotency_key.as_str() == "kb")
        .expect("kb rep present");
    assert_eq!(
        kb_rep.id,
        uuid::Uuid::from_u128(13),
        "kb rep is the first occurrence"
    );
}

#[test]
fn plan_batch_gives_two_rows_that_share_an_id_one_slot_whatever_else_differs() {
    // The identity the whole write path decides on is the dispatched `id`, and the
    // plugin never re-derives it. So "two rows share an identity" means "two rows
    // share an `id`", with no second opinion taken from their other fields —
    // pinned with a pair the gateway could never produce: one `id` over two
    // different covered periods.
    //
    // **Why the pair must collapse rather than be caught.** The guarded statement
    // joins its inserted rows back to its input on `id` (`LEFT JOIN ins USING
    // (id)`), so two input rows carrying one `id` have no coherent reading: both
    // would come back carrying the same verdict, including the same freshly
    // inserted row returned twice, with nothing reporting it. Under the collapse
    // the behaviour is the ordinary same-identity one.
    let tenant = uuid::Uuid::from_u128(0xC3);
    let first = unit_record(tenant, "shared-id", 0xC300);
    let mut second = unit_record(tenant, "shared-id", 0xC300);
    second.window_end += time::Duration::hours(1);
    assert_ne!(
        first.window_end, second.window_end,
        "test setup: the two disagree on a field that is one of the six \
         derivation inputs, so only a faithful gateway could not emit them"
    );

    let records = vec![first.clone(), second];
    let records = under_unit_meter(records);
    let plan = plan_batch(&records);

    assert_eq!(
        plan.reps.len(),
        1,
        "two rows carrying one id are one identity and take one slot"
    );
    assert_eq!(
        plan.first_index[&entry_identity(&first)],
        0,
        "the first occurrence is the one that can win it"
    );
}

/// A withdrawal carrying its target's idempotency key, as the gateway
/// dispatches it: the pair departs in the entry type alone.
fn derived_withdrawal(
    tenant: uuid::Uuid,
    seq: u128,
    target: uuid::Uuid,
    target_key: &str,
    reason: &str,
) -> usage_collector_sdk::StoredUsageRecord {
    usage_collector_sdk::StoredUsageRecord {
        invalidation: Some(usage_collector_sdk::Invalidation {
            target,
            reason: usage_collector_sdk::ReasonCode::new(reason).expect("valid reason code"),
        }),
        ..unit_record(tenant, target_key, seq)
    }
}

#[test]
fn plan_batch_collapses_two_withdrawals_of_one_target_onto_one_slot() {
    let tenant = uuid::Uuid::from_u128(0xC1);
    let target = uuid::Uuid::from_u128(0xC100);
    let records = vec![
        derived_withdrawal(tenant, 0xC101, target, "tgt", "duplicate_submission"),
        unit_record(tenant, "plain", 0xC102),
        derived_withdrawal(tenant, 0xC101, target, "tgt", "late_correction"),
    ];

    let records = under_unit_meter(records);
    let plan = plan_batch(&records);

    assert_eq!(
        entry_identity(&records[0].1),
        entry_identity(&records[2].1),
        "test setup: every withdrawal of one target agrees on all six \
         derivation inputs, so the gateway stamps them one id"
    );
    assert_eq!(
        plan.reps.len(),
        2,
        "one slot for the pair, one for the plain entry"
    );
    assert_eq!(
        plan.first_index[&entry_identity(&records[0].1)],
        0,
        "the earlier withdrawal holds it"
    );
}

#[test]
fn plan_batch_gives_a_record_and_its_withdrawal_two_slots() {
    // The converse of the test above, and the case a key short of the entry type
    // got wrong: a withdrawal repeats its target's tenant, meter, idempotency key
    // and covered period, so a plan keyed on those alone would collapse the pair
    // onto one slot and resolve the withdrawal as an idempotency conflict without
    // the database ever being asked. No order between the two reps is asserted: a
    // `UUIDv5` orders by its digest bytes.
    let tenant = uuid::Uuid::from_u128(0xC2);
    let target = unit_record(tenant, "tgt", 0xC200);
    let records = vec![
        target.clone(),
        derived_withdrawal(tenant, 0xC201, target.id, "tgt", "duplicate_submission"),
    ];

    let records = under_unit_meter(records);
    let plan = plan_batch(&records);

    assert_ne!(
        entry_identity(&records[0].1),
        entry_identity(&records[1].1),
        "the pair departs in the entry type, the sixth derivation input, so the \
         gateway stamps them two ids"
    );
    assert_eq!(
        plan.reps.len(),
        2,
        "a record and the withdrawal that names it are two identities, so two \
         slots -- neither swallows the other"
    );
}

// --- The batch insert's column pivot ---
//
// Pure, and a place a defect is invisible everywhere else: a swapped push in the
// pivot corrupts every batched row while every other test still passes.

#[test]
fn insert_columns_pivots_each_record_into_the_column_it_is_bound_as() {
    // Two records whose leaves are all distinguishable, so no swap of a
    // same-typed adjacent pair can land on an equal value and pass. The pairs that
    // are swappable without a type error — `resource_ids`/`resource_types`,
    // `subject_ids`/`subject_types`, the two bounds — are each given differing
    // values. `invalidates`/`reason_code` cannot exchange at all:
    // `Vec<Option<Uuid>>` against `Vec<Option<String>>`.
    let tenant = uuid::Uuid::from_u128(0xD1);
    let target = uuid::Uuid::from_u128(0xD100);
    let mut plain = unit_record(tenant, "idem-a", 0xD101);
    plain.resource_ref =
        usage_collector_sdk::ResourceRef::new("res-a", "type-a").expect("valid resource_ref");
    plain.subject_ref = Some(
        usage_collector_sdk::SubjectRef::new("subj-a", Some("subjtype-a".to_owned()))
            .expect("valid subject_ref"),
    );
    plain.metadata.insert(
        usage_collector_sdk::MetadataKey::new("region").expect("valid metadata key"),
        "eu-west".to_owned(),
    );
    let with = withdrawal(tenant, "idem-b", 0xD102, target);

    let plain = (unit_meter(), plain);
    let with = (unit_meter(), with);
    let cols = InsertColumns::build(&[&plain, &with], &[3, 4]);
    let (_, plain) = &plain;
    let (_, with) = &with;

    assert_eq!(cols.ids, vec![plain.id, with.id]);
    assert_eq!(cols.tenants, vec![tenant, tenant]);
    assert_eq!(
        cols.gts_type_uuids,
        vec![unit_meter().uuid, unit_meter().uuid]
    );
    assert_eq!(
        cols.type_keys,
        vec![3, 4],
        "each representative's partition key, in order"
    );
    assert_eq!(
        cols.quantities,
        vec![plain.quantity.as_decimal(), with.quantity.as_decimal()]
    );
    assert_eq!(
        cols.window_starts,
        vec![plain.window_start, with.window_start]
    );
    assert_eq!(cols.window_ends, vec![plain.window_end, with.window_end]);
    assert_eq!(
        cols.resource_ids,
        vec!["res-a".to_owned(), "res-1".to_owned()],
        "resource_id, not resource_type"
    );
    assert_eq!(
        cols.resource_types,
        vec!["type-a".to_owned(), "compute.vm".to_owned()],
        "resource_type, not resource_id"
    );
    assert_eq!(
        cols.subject_ids,
        vec![Some("subj-a".to_owned()), None],
        "subject_id, not subject_type"
    );
    assert_eq!(
        cols.subject_types,
        vec![Some("subjtype-a".to_owned()), None],
        "subject_type, not subject_id"
    );
    assert_eq!(
        cols.idem_keys,
        vec!["idem-a".to_owned(), "idem-b".to_owned()]
    );
    assert_eq!(
        cols.invalidates,
        vec![None, Some(target)],
        "only the withdrawal names a target"
    );
    assert_eq!(
        cols.reason_codes,
        vec![None, Some("duplicate_submission".to_owned())],
        "and the pair travels together"
    );
    assert_eq!(cols.origins, vec!["live".to_owned(), "live".to_owned()]);
    assert_eq!(
        cols.entry_types,
        vec!["record", "invalidation"],
        "each representative's own declared kind, and the pair here is one of \
         each so a constant would not pass"
    );
    assert_eq!(
        cols.metadata,
        vec![r#"{"region":"eu-west"}"#.to_owned(), "{}".to_owned()],
        "metadata is carried as text and cast ::jsonb per row in the query"
    );
}

// --- `resolve_batch` defensive arm (DB-free) ---
//
// One arm remains that no happy path reaches: a lost key absent from the
// conflict map. It is pinned here against a hand-built map. `resolve_batch`
// matches on the statement's own verdict, so the other `Internal` states that
// a derived `won` set would have made representable do not exist.

#[tokio::test]
async fn resolve_batch_missing_conflict_entry_falls_through_to_transient() {
    let store = lazy_store();
    let tenant = uuid::Uuid::from_u128(0xB4);
    let records = vec![unit_record(tenant, "absent", 0x40)];
    let records = under_unit_meter(records);
    let plan = plan_batch(&records);

    // Not won, and the conflict map has no entry for the key at all. The
    // defensive `None` fallthrough must still be a retryable Transient — never a
    // silent success and never a panic.
    let admissions: HashMap<uuid::Uuid, Admission> =
        HashMap::from([(entry_identity(&records[0].1), Admission::Lost)]);
    let conflict: HashMap<uuid::Uuid, ConflictRead> = HashMap::new();

    let results = store.resolve_batch(&records, &plan, &admissions, &conflict);

    assert_eq!(results.len(), 1);
    assert!(
        matches!(results[0], Err(UsageCollectorPluginError::Transient { .. })),
        "a key absent from the conflict map must fall through to Transient: {:?}",
        results[0]
    );
}

#[tokio::test]
async fn resolve_batch_conflicts_an_in_batch_duplicate_whose_canonical_fields_differ() {
    // Two input rows share an identity but disagree on `quantity`. Only the first
    // can win the slot; the second must resolve against what was written — an
    // `IdempotencyConflict`, not a second copy of the winner's row. This is what
    // the `plan.first_index` guard buys: without it every input row holding a won
    // identity looks like a fresh insert, and a same-identity submission carrying
    // different data absorbs silently.
    let store = lazy_store();
    let tenant = uuid::Uuid::from_u128(0xB6);
    let mut second = unit_record(tenant, "k", 0xB601);
    second.quantity = qty("999");
    let records = under_unit_meter(vec![unit_record(tenant, "k", 0xB601), second]);
    assert_eq!(
        entry_identity(&records[0].1),
        entry_identity(&records[1].1),
        "test setup: the two rows must share an identity"
    );

    let plan = plan_batch(&records);
    let winner_row = row_matching(
        &records[0].1,
        serde_json::Value::Object(serde_json::Map::new()),
    );
    let admissions: HashMap<uuid::Uuid, Admission> = HashMap::from([(
        entry_identity(&records[0].1),
        Admission::Won(Box::new(winner_row)),
    )]);
    let conflict: HashMap<uuid::Uuid, ConflictRead> = HashMap::new();

    let results = store.resolve_batch(&records, &plan, &admissions, &conflict);

    assert_eq!(results.len(), 2, "one result per input row, in input order");
    assert!(
        results[0].is_ok(),
        "the first occurrence wins its slot: {:?}",
        results[0]
    );
    match &results[1] {
        Err(UsageCollectorPluginError::IdempotencyConflict { existing, .. }) => {
            assert_eq!(
                existing.id, records[0].1.id,
                "the conflict names the row already holding the slot"
            );
        }
        other => {
            panic!("an in-batch duplicate carrying different data must conflict, got {other:?}")
        }
    }
}

#[tokio::test]
async fn resolve_batch_resolves_a_second_withdrawal_against_the_first() {
    let store = lazy_store();
    let tenant = uuid::Uuid::from_u128(0xB5);
    let target = uuid::Uuid::from_u128(0xB500);
    let first = derived_withdrawal(tenant, 0xB501, target, "tgt", "duplicate_submission");
    let records = vec![
        first.clone(),
        first.clone(),
        derived_withdrawal(tenant, 0xB501, target, "tgt", "late_correction"),
    ];
    let records = under_unit_meter(records);
    let plan = plan_batch(&records);
    let admissions: HashMap<uuid::Uuid, Admission> = HashMap::from([(
        entry_identity(&first),
        Admission::Won(Box::new(row_matching(
            &first,
            serde_json::Value::Object(serde_json::Map::new()),
        ))),
    )]);
    let conflict: HashMap<uuid::Uuid, ConflictRead> = HashMap::new();

    let results = store.resolve_batch(&records, &plan, &admissions, &conflict);

    assert_eq!(results.len(), 3, "one result per input row, in input order");
    assert_eq!(
        results[0].as_ref().expect("the first is accepted").id,
        first.id
    );
    assert_eq!(
        results[1]
            .as_ref()
            .expect("the same-reason repeat is absorbed")
            .id,
        first.id
    );
    match &results[2] {
        Err(UsageCollectorPluginError::IdempotencyConflict { existing, .. }) => {
            assert_eq!(existing.id, first.id);
            assert_eq!(
                existing.invalidation.as_ref().map(|i| i.reason.as_str()),
                Some("duplicate_submission")
            );
        }
        other => panic!("a second withdrawal under another reason conflicts, got {other:?}"),
    }
}

