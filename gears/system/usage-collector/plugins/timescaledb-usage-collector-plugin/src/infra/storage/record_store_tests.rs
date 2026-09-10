// Test modules using bare `panic!` opt in explicitly
// (clippy.toml allows unwrap/expect in tests, not panic).
#![allow(clippy::panic)]

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use sqlx::postgres::PgPoolOptions;
use tokio_util::sync::CancellationToken;
use toolkit_odata::ast;
use toolkit_odata::{CursorV1, ODataOrderBy, ODataQuery, OrderKey, SortDir};
use usage_collector_sdk::{UsageCollectorPluginError, UsageTypeGtsId};

use super::{
    ConflictRead, DedupKey, INSERT_COLUMNS, INSERT_COLUMN_ARRAY_TYPES, InsertColumns,
    BATCH_INSERT_SQL, MAX_BATCH_ATTEMPTS, PgRecordStore, RECORD_COLUMNS, SINGLE_INSERT_SQL,
    batch_retry_backoff,
    batch_retry_backoff_base, build_get_sql, canonical_equal, dedup_key,
    invalidation_index_slots, is_retryable_batch_error, plan_batch, row_dedup_key, scope_runs,
    sequence_block, with_retry,
};
use crate::domain::ports::RecordStore;
use crate::infra::metrics::Metrics;
use crate::infra::storage::entity::UsageRecordRow;
use crate::infra::storage::query::translate::SqlBind;

/// A valid meter type id: the reserved base plus one derivation segment,
/// `~`-terminated, which is what `MeterTypeId::new` validates.
const VCPU_METER: &str = "gts.cf.core.uc.usage_record.v1~cf.compute._.vcpu_hours.v1~";

/// The covered period every unit record below carries: one hour starting at
/// `2023-11-14T22:13:20Z` (unix `1_700_000_000`).
const WINDOW_START_UNIX: i64 = 1_700_000_000;
const WINDOW_END_UNIX: i64 = 1_700_003_600;

/// The canonical rendering of those two bounds, spelled out rather than
/// computed, so a test that pins the dedup key names the bytes instead of
/// re-deriving them with the function under test.
const WINDOW_START_CANONICAL: &str = "2023-11-14T22:13:20.000000Z";
const WINDOW_END_CANONICAL: &str = "2023-11-14T23:13:20.000000Z";

/// A store over a lazy pool: no connection is opened, so the pre-DB validation
/// paths under test return before any query is issued. The tiny acquire timeout
/// keeps an accidental DB touch from hanging the test.
fn lazy_store() -> PgRecordStore {
    let pool = PgPoolOptions::new()
        .acquire_timeout(Duration::from_millis(50))
        .connect_lazy("postgres://user:pass@localhost/db")
        .expect("a syntactically valid DSN yields a lazy pool without connecting");
    PgRecordStore::new(
        pool.clone(),
        Arc::new(Metrics::new(pool)),
        CancellationToken::new(),
    )
}

/// Minimal in-memory `UsageRecord` for pure (no-DB) unit tests.
fn unit_record(tenant: uuid::Uuid, idem: &str, seq: u128) -> usage_collector_sdk::UsageRecord {
    usage_collector_sdk::UsageRecord {
        id: uuid::Uuid::from_u128(seq),
        gts_type_id: usage_collector_sdk::MeterTypeId::new(VCPU_METER).expect("valid meter id"),
        tenant_id: tenant,
        resource_ref: usage_collector_sdk::ResourceRef::new("res-1", "compute.vm")
            .expect("valid resource_ref"),
        subject_ref: None,
        metadata: std::collections::BTreeMap::new(),
        value: rust_decimal::Decimal::new(1, 0),
        idempotency_key: usage_collector_sdk::IdempotencyKey::new(idem).expect("valid idem key"),
        origin: usage_collector_sdk::RecordOrigin::Live,
        invalidation: None,
        window_start: time::OffsetDateTime::from_unix_timestamp(WINDOW_START_UNIX)
            .expect("valid ts"),
        window_end: time::OffsetDateTime::from_unix_timestamp(WINDOW_END_UNIX).expect("valid ts"),
    }
}

/// A withdrawal of `target`: the same shape as [`unit_record`] plus the
/// invalidation pair. A faithful withdrawal copies its target's covered period,
/// and every one built here does.
fn withdrawal(
    tenant: uuid::Uuid,
    idem: &str,
    seq: u128,
    target: uuid::Uuid,
) -> usage_collector_sdk::UsageRecord {
    usage_collector_sdk::UsageRecord {
        invalidation: Some(usage_collector_sdk::Invalidation {
            target,
            reason: usage_collector_sdk::ReasonCode::new("duplicate_submission")
                .expect("valid reason code"),
        }),
        ..unit_record(tenant, idem, seq)
    }
}

// --- The dedup identity (the 5-tuple) ---

#[test]
fn the_dedup_key_is_the_five_tuple() {
    let tenant = uuid::Uuid::from_u128(2);
    let base = unit_record(tenant, "same", 100);

    // The five components, named rather than re-derived: two entries differing
    // in any one of them are distinct entries, not retries of each other.
    let mut other_tenant = unit_record(uuid::Uuid::from_u128(3), "same", 100);
    other_tenant.window_start = base.window_start;
    assert_ne!(dedup_key(&base), dedup_key(&other_tenant), "tenant_id");

    let mut other_meter = unit_record(tenant, "same", 100);
    other_meter.gts_type_id = usage_collector_sdk::MeterTypeId::new(
        "gts.cf.core.uc.usage_record.v1~cf.storage._.gb_hours.v1~",
    )
    .expect("valid meter id");
    assert_ne!(dedup_key(&base), dedup_key(&other_meter), "gts_type_id");

    assert_ne!(
        dedup_key(&base),
        dedup_key(&unit_record(tenant, "different", 100)),
        "idempotency_key"
    );

    let mut shifted_start = unit_record(tenant, "same", 100);
    shifted_start.window_start = base.window_start - time::Duration::hours(1);
    assert_ne!(
        dedup_key(&base),
        dedup_key(&shifted_start),
        "window_start is one of the five dedup-identity inputs, so two entries \
         differing only in it are distinct entries, not a retry"
    );

    let mut shifted_end = unit_record(tenant, "same", 100);
    shifted_end.window_end = base.window_end + time::Duration::hours(1);
    assert_ne!(
        dedup_key(&base),
        dedup_key(&shifted_end),
        "window_end is the fifth dedup-identity input"
    );

    // ...and nothing else is in the key. `value` is a compared canonical field,
    // not an identity component.
    let mut other_value = unit_record(tenant, "same", 100);
    other_value.value = rust_decimal::Decimal::new(999, 0);
    assert_eq!(
        dedup_key(&base),
        dedup_key(&other_value),
        "value is not part of the dedup key"
    );
}

#[test]
fn the_dedup_key_names_the_canonical_microsecond_bounds() {
    // The bounds enter the key as the SDK's canonical rendering, the same form
    // the entry `id` is derived over. Spelled out here rather than computed
    // with the function under test.
    let tenant = uuid::Uuid::from_u128(0x2A);
    assert_eq!(
        dedup_key(&unit_record(tenant, "k", 42)),
        (
            tenant,
            VCPU_METER.to_owned(),
            "k".to_owned(),
            WINDOW_START_CANONICAL.to_owned(),
            WINDOW_END_CANONICAL.to_owned(),
        )
    );
}

#[test]
fn a_sub_microsecond_bound_keys_the_same_as_what_postgres_stores() {
    // `timestamptz` stores microseconds, so a caller's sub-µs nanos never
    // survive the round trip. If the key carried the raw `OffsetDateTime`, an
    // `INSERT … RETURNING` row would key differently from the record that
    // produced it and the batch path would lose track of its own winners.
    let tenant = uuid::Uuid::from_u128(0x2B);
    let mut sub_micro = unit_record(tenant, "k", 43);
    sub_micro.window_start = time::OffsetDateTime::from_unix_timestamp_nanos(
        i128::from(WINDOW_START_UNIX) * 1_000_000_000 + 750,
    )
    .expect("valid ts");

    assert_eq!(
        dedup_key(&sub_micro),
        dedup_key(&unit_record(tenant, "k", 43)),
        "sub-microsecond nanos are below the precision the ledger stores, so \
         they cannot make two submissions distinct entries"
    );
}

#[test]
fn a_stored_row_keys_the_same_as_the_record_it_holds() {
    // The batch path maps `INSERT … RETURNING` rows back to the records that
    // produced them through these two functions, so a disagreement between
    // them silently turns every winner into an invariant break.
    let tenant = uuid::Uuid::from_u128(0x2C);
    let record = unit_record(tenant, "k", 44);
    let row = row_matching(&record, serde_json::Value::Object(serde_json::Map::new()));

    assert_eq!(row_dedup_key(&row), dedup_key(&record));
    assert_eq!(
        row_dedup_key(&row).3,
        WINDOW_START_CANONICAL,
        "and it is the canonical form on the row side too"
    );
}

/// A `UsageRecordRow` whose canonical fields equal `record`, carrying the
/// given stored `metadata` jsonb verbatim. Used to exercise `canonical_equal`'s
/// absorb/conflict/decode-failure paths without a database.
fn row_matching(
    record: &usage_collector_sdk::UsageRecord,
    metadata: serde_json::Value,
) -> UsageRecordRow {
    let (invalidates, reason_code) = match record.invalidation.as_ref() {
        Some(i) => (Some(i.target), Some(i.reason.as_str().to_owned())),
        None => (None, None),
    };
    UsageRecordRow {
        id: record.id,
        tenant_id: record.tenant_id,
        gts_type_id: record.gts_type_id.as_str().to_owned(),
        value: record.value,
        window_start: record.window_start,
        window_end: record.window_end,
        resource_id: record.resource_ref.resource_id().to_owned(),
        resource_type: record.resource_ref.resource_type().to_owned(),
        subject_id: None,
        subject_type: None,
        idempotency_key: record.idempotency_key.as_str().to_owned(),
        invalidates,
        reason_code,
        origin: record.origin.as_str().to_owned(),
        acceptance_sequence: 1,
        metadata,
        ingested_at: record.window_end,
    }
}

#[test]
fn canonical_equal_surfaces_corrupt_stored_metadata_as_internal() {
    let tenant = uuid::Uuid::from_u128(7);
    let record = unit_record(tenant, "k", 700);
    // Stored metadata that cannot decode back to the typed map (a JSON string,
    // not an object). Every other canonical field matches, so a swallowed
    // decode error would turn stored-data corruption into a silent
    // `IdempotencyConflict`.
    let row = row_matching(&record, serde_json::Value::String("corrupt".to_owned()));

    let err = canonical_equal(&row, &record)
        .expect_err("a corrupt stored metadata blob must surface as an error, not absorb/conflict");

    match err {
        UsageCollectorPluginError::Internal(msg) => {
            assert!(msg.contains("metadata"), "unexpected error message: {msg}");
        }
        other => panic!("expected an Internal stored-metadata-decode error, got {other:?}"),
    }
}

#[test]
fn canonical_equal_absorbs_an_exact_match() {
    let tenant = uuid::Uuid::from_u128(8);
    let record = unit_record(tenant, "k", 800);
    let row = row_matching(&record, serde_json::Value::Object(serde_json::Map::new()));

    assert!(
        canonical_equal(&row, &record).expect("valid metadata decodes"),
        "a row whose canonical fields all match must compare equal"
    );
}

#[test]
fn canonical_equal_reports_a_field_mismatch_as_not_equal() {
    let tenant = uuid::Uuid::from_u128(9);
    let record = unit_record(tenant, "k", 900);
    let mut row = row_matching(&record, serde_json::Value::Object(serde_json::Map::new()));
    row.value = rust_decimal::Decimal::new(999, 0);

    assert!(
        !canonical_equal(&row, &record).expect("valid metadata decodes"),
        "a differing canonical field must compare not-equal (the conflict path)"
    );
}

#[test]
fn canonical_equal_treats_id_as_canonical() {
    // The record `id` is part of the canonical set: a same-key request whose
    // other canonical fields all match but whose stored `id` differs is a
    // fail-closed `IdempotencyConflict`, not a silent absorb. Since `id` is a
    // deterministic projection of the dedup key, a real dedup hit always
    // carries a matching id — so this is a defensive guard against a corrupted
    // stored row rather than a mismatched caller-supplied one.
    let tenant = uuid::Uuid::from_u128(10);
    let record = unit_record(tenant, "k", 1000);
    let mut row = row_matching(&record, serde_json::Value::Object(serde_json::Map::new()));
    row.id = uuid::Uuid::from_u128(0xDEAD_BEEF);
    assert_ne!(row.id, record.id, "test setup: the ids must differ");

    assert!(
        !canonical_equal(&row, &record).expect("valid metadata decodes"),
        "a differing id is a canonical-field mismatch; the request must conflict"
    );
}

#[test]
fn canonical_equal_compares_origin() {
    // `origin` is server-assigned by the gateway from the route the entry
    // arrived on, but it is still supplied to this plugin per entry and is not
    // in the dedup key — so one idempotency key standing for a live entry and
    // a backfilled one is a conflict, not an absorb.
    let tenant = uuid::Uuid::from_u128(11);
    let record = unit_record(tenant, "k", 1100);
    let mut row = row_matching(&record, serde_json::Value::Object(serde_json::Map::new()));
    row.origin = usage_collector_sdk::RecordOrigin::Backfill.as_str().to_owned();

    assert!(
        !canonical_equal(&row, &record).expect("valid metadata decodes"),
        "a differing origin must conflict rather than absorb"
    );
}

#[test]
fn canonical_equal_compares_both_halves_of_the_invalidation_pair() {
    // The invalidation target is deliberately excluded from the identity
    // derivation, which is exactly why it has to be compared here: reusing one
    // idempotency key across an entry and its withdrawal collapses them onto a
    // single dedup slot, and only this comparison makes that collapse loud
    // instead of absorbing the withdrawal as a duplicate of its own target.
    let tenant = uuid::Uuid::from_u128(12);
    let target = uuid::Uuid::from_u128(0x1200);
    let record = withdrawal(tenant, "k", 1200, target);

    let mut plain = row_matching(&record, serde_json::Value::Object(serde_json::Map::new()));
    plain.invalidates = None;
    plain.reason_code = None;
    assert!(
        !canonical_equal(&plain, &record).expect("valid metadata decodes"),
        "an ordinary measurement stored under this key is not this withdrawal"
    );

    let mut other_target = row_matching(&record, serde_json::Value::Object(serde_json::Map::new()));
    other_target.invalidates = Some(uuid::Uuid::from_u128(0x1201));
    assert!(
        !canonical_equal(&other_target, &record).expect("valid metadata decodes"),
        "a withdrawal of a different entry is a different entry"
    );

    let mut other_reason = row_matching(&record, serde_json::Value::Object(serde_json::Map::new()));
    other_reason.reason_code = Some("late_correction".to_owned());
    assert!(
        !canonical_equal(&other_reason, &record).expect("valid metadata decodes"),
        "the reason is caller-supplied and compared, so a differing one conflicts"
    );
}

// --- The insert SQL's column sequences ---
//
// `InsertColumns::build` is tested above; these test the layer under it. The
// hazard is a name transposed between the column list, the `SELECT` list and
// the `UNNEST` alias: two `text[]` columns exchanged that way bind the wrong
// values, Postgres accepts it, and every row-level test still passes. Hoisting
// all three onto one `INSERT_COLUMNS` makes the transposition unrepresentable;
// these assertions are what keep it that way.
//
// The `.bind()` call sequence is one layer further down and still uncovered
// here — it needs a live backend, and it is Task 15's.

/// The comma-separated names of a SQL column list.
fn names(list: &str) -> Vec<&str> {
    list.split(',').map(str::trim).collect()
}

/// Every inserted column paired with the array element type the batch insert
/// must `UNNEST` it as, **transcribed by hand from `migrations/0001_init.sql`**
/// rather than derived from the code under test.
///
/// This literal is the whole point of the pairing test below. Checking the
/// generated SQL against `INSERT_COLUMN_ARRAY_TYPES` proves only that the SQL
/// was built from that array — transpose two entries and both sides move
/// together, which is exactly how the first version of this test let such a
/// mutation survive. A transposition has to be measured against something that
/// does not move, and the migration is that thing.
///
/// One deliberate divergence from the DDL: `metadata` is `jsonb` in the table
/// but travels as `text[]` and is cast `::jsonb` per row, because `jsonb[]`
/// array encoding is the thing being sidestepped.
const DDL_COLUMN_ARRAY_TYPES: [(&str, &str); 16] = [
    ("id", "uuid"),
    ("tenant_id", "uuid"),
    ("gts_type_id", "text"),
    ("value", "numeric"),
    ("window_start", "timestamptz"),
    ("window_end", "timestamptz"),
    ("resource_id", "text"),
    ("resource_type", "text"),
    ("subject_id", "text"),
    ("subject_type", "text"),
    ("idempotency_key", "text"),
    ("invalidates", "uuid"),
    ("reason_code", "text"),
    ("origin", "text"),
    ("acceptance_sequence", "bigint"),
    ("metadata", "text"),
];

#[test]
fn each_inserted_column_is_unnested_as_the_type_the_migration_declares() {
    let want_names: Vec<&str> = DDL_COLUMN_ARRAY_TYPES.iter().map(|(n, _)| *n).collect();
    let want_types: Vec<&str> = DDL_COLUMN_ARRAY_TYPES.iter().map(|(_, t)| *t).collect();

    assert_eq!(
        names(INSERT_COLUMNS),
        want_names,
        "the inserted column sequence must be the migration's, in its order"
    );
    assert_eq!(
        INSERT_COLUMN_ARRAY_TYPES.to_vec(),
        want_types,
        "each column's array type must be the one the migration declares for it \
         (metadata excepted: jsonb in the table, carried as text and cast per row)"
    );
}

#[test]
fn record_columns_is_the_insert_columns_plus_the_defaulted_ingested_at() {
    // The read list and the write list are one sequence with one difference:
    // `ingested_at` defaults to `now()` and is never written. If they drift,
    // `RETURNING {RECORD_COLUMNS}` decodes a row the insert did not write.
    assert_eq!(
        RECORD_COLUMNS,
        format!("{INSERT_COLUMNS}, ingested_at"),
        "the only column the insert omits is the one the table defaults"
    );
    assert_eq!(
        names(INSERT_COLUMNS).len(),
        INSERT_COLUMN_ARRAY_TYPES.len(),
        "one array type per inserted column, in the same order"
    );
    assert!(
        !names(RECORD_COLUMNS).contains(&"entry_type"),
        "entry_type is a generated column; nothing reads or writes it"
    );
    assert_eq!(
        names(INSERT_COLUMNS).last(),
        Some(&"metadata"),
        "the batch SELECT appends ::jsonb to the whole column list, so the cast \
         lands on metadata only while metadata is last"
    );
}

#[test]
fn the_single_insert_binds_one_placeholder_per_inserted_column() {
    let sql = SINGLE_INSERT_SQL.as_str();
    let cols = sql
        .split_once("usage_records (")
        .and_then(|(_, rest)| rest.split_once(')'))
        .expect("the insert names its column list")
        .0;
    assert_eq!(names(cols), names(INSERT_COLUMNS));

    let values = sql
        .split_once("VALUES (")
        .and_then(|(_, rest)| rest.split_once(')'))
        .expect("the insert has a VALUES list")
        .0;
    let want: Vec<String> = (1..=names(INSERT_COLUMNS).len())
        .map(|i| format!("${i}"))
        .collect();
    assert_eq!(
        names(values),
        want,
        "$n must be column n, numbered from 1 with no gap"
    );
    assert!(
        sql.contains(
            "ON CONFLICT (tenant_id, gts_type_id, idempotency_key, window_start, window_end)"
        ),
        "the arbiter is the dedup 5-tuple: {sql}"
    );
}

#[test]
fn the_batch_insert_names_one_column_sequence_in_all_three_places() {
    let sql = BATCH_INSERT_SQL.as_str();
    let expected = names(INSERT_COLUMNS);

    let cols = sql
        .split_once("usage_records (")
        .and_then(|(_, rest)| rest.split_once(')'))
        .expect("the insert names its column list")
        .0;
    assert_eq!(names(cols), expected, "INSERT column list");

    let select = sql
        .split_once("SELECT ")
        .and_then(|(_, rest)| rest.split_once(" FROM UNNEST("))
        .expect("the insert has a SELECT list")
        .0;
    assert_eq!(
        select,
        format!("{INSERT_COLUMNS}::jsonb"),
        "the SELECT list is the column list with the trailing metadata cast, \
         which only works while `metadata` is last"
    );

    let alias = sql
        .split_once("AS t(")
        .and_then(|(_, rest)| rest.split_once(')'))
        .expect("the UNNEST has an alias list")
        .0;
    assert_eq!(names(alias), expected, "UNNEST alias list");

    let unnest = sql
        .split_once(" FROM UNNEST(")
        .and_then(|(_, rest)| rest.split_once(") AS t("))
        .expect("the insert has an UNNEST parameter list")
        .0;
    let params = names(unnest);
    assert_eq!(
        params.len(),
        expected.len(),
        "one UNNEST array per column: {unnest}"
    );
    for (i, (param, ty)) in params.iter().zip(INSERT_COLUMN_ARRAY_TYPES).enumerate() {
        assert_eq!(
            *param,
            format!("${}::{ty}[]", i + 1),
            "UNNEST parameter {} must be column {}'s array type",
            i + 1,
            i + 1
        );
    }

    // Asserted here as well as on the single insert, against the same literal.
    // Both builders read `DEDUP_CONFLICT_TARGET` so they cannot differ today,
    // but an arbiter hardcoded into this one alone would otherwise pass.
    assert!(
        sql.contains(
            "ON CONFLICT (tenant_id, gts_type_id, idempotency_key, window_start, window_end)"
        ),
        "the batch arbiter is the dedup 5-tuple too: {sql}"
    );
}

// --- Batch planning ---

#[test]
fn plan_batch_collapses_and_sorts_distinct_keys() {
    let tenant = uuid::Uuid::from_u128(1);
    let mk = |idem: &str, seq: u128| unit_record(tenant, idem, seq);
    let records = vec![
        mk("kb", 10), // idx 0
        mk("ka", 11), // idx 1
        mk("kb", 12), // idx 2 — duplicate of idx 0's key
        mk("kc", 13), // idx 3
    ];

    let plan = plan_batch(&records);

    let idems: Vec<&str> = plan
        .reps
        .iter()
        .map(|r| r.idempotency_key.as_str())
        .collect();
    assert_eq!(idems, vec!["ka", "kb", "kc"], "distinct, sorted by key");

    assert_eq!(
        plan.first_index[&dedup_key(&records[1])],
        1,
        "ka first at idx 1"
    );
    assert_eq!(
        plan.first_index[&dedup_key(&records[0])],
        0,
        "kb first at idx 0"
    );
    assert_eq!(
        plan.first_index[&dedup_key(&records[3])],
        3,
        "kc first at idx 3"
    );
    assert!(
        plan.duplicate_withdrawals.is_empty(),
        "a batch of ordinary measurements pre-rejects nothing"
    );

    let kb_rep = plan
        .reps
        .iter()
        .find(|r| r.idempotency_key.as_str() == "kb")
        .expect("kb rep present");
    assert_eq!(
        kb_rep.id,
        uuid::Uuid::from_u128(10),
        "kb rep is the first occurrence"
    );
}

#[test]
fn plan_batch_pre_rejects_a_second_withdrawal_of_one_target() {
    // Both withdrawals would otherwise go into one multi-row INSERT, where the
    // at-most-one index rejects the whole *statement* — taking the unrelated
    // entry at idx 1 down with it. The SPI wants exactly one accepted, the
    // other rejected, and every other row's outcome intact.
    let tenant = uuid::Uuid::from_u128(0xC1);
    let target = uuid::Uuid::from_u128(0xC100);
    let records = vec![
        withdrawal(tenant, "w1", 0xC101, target), // idx 0 — wins
        unit_record(tenant, "plain", 0xC102),     // idx 1 — unrelated
        withdrawal(tenant, "w2", 0xC103, target), // idx 2 — pre-rejected
    ];

    let plan = plan_batch(&records);

    assert_eq!(
        plan.duplicate_withdrawals.get(&2),
        Some(&uuid::Uuid::from_u128(0xC101)),
        "the second withdrawal of one target is pre-rejected, naming the first"
    );
    assert!(
        !plan.duplicate_withdrawals.contains_key(&0),
        "the first withdrawal is admitted"
    );
    let rep_ids: Vec<uuid::Uuid> = plan.reps.iter().map(|r| r.id).collect();
    assert!(
        !rep_ids.contains(&uuid::Uuid::from_u128(0xC103)),
        "a pre-rejected row never reaches the insert"
    );
    assert!(
        rep_ids.contains(&uuid::Uuid::from_u128(0xC102)),
        "the unrelated entry keeps its slot in the insert"
    );
}

#[test]
fn plan_batch_leaves_an_identical_repeat_withdrawal_to_the_dedup_path() {
    // Two rows carrying the *same* withdrawal — same derived id, so all five
    // dedup attributes match — are an at-least-once redelivery, not a second
    // withdrawal. Rejecting the repeat would turn an idempotent retry into a
    // hard error.
    let tenant = uuid::Uuid::from_u128(0xC2);
    let target = uuid::Uuid::from_u128(0xC200);
    let records = vec![
        withdrawal(tenant, "w", 0xC201, target),
        withdrawal(tenant, "w", 0xC201, target),
    ];

    let plan = plan_batch(&records);

    assert!(
        plan.duplicate_withdrawals.is_empty(),
        "a repeat of one withdrawal is absorbed by dedup, not pre-rejected"
    );
    assert_eq!(plan.reps.len(), 1, "and it collapses to a single slot");
}

#[test]
fn invalidation_index_slots_names_only_the_withdrawals() {
    let tenant = uuid::Uuid::from_u128(0xC3);
    let target = uuid::Uuid::from_u128(0xC300);
    let plain = unit_record(tenant, "plain", 0xC301);
    let with = withdrawal(tenant, "w", 0xC302, target);

    assert_eq!(
        invalidation_index_slots(&[&plain, &with]),
        vec![(tenant, target, with.window_end)],
        "the partial index covers `invalidates IS NOT NULL` only, so an ordinary \
         measurement contributes no slot to look up, and the withdrawal's slot \
         carries the tenant its diagnostic lookup is scoped by"
    );
    assert!(invalidation_index_slots(&[&plain]).is_empty());
}

// --- The batch insert's column pivot and sequence-block arithmetic ---
//
// Both are pure and both are places a defect is invisible everywhere else: a
// swapped push in the pivot corrupts every batched row while every other test
// still passes, and an off-by-one in the block expansion reuses or skips a
// sequence value that no constraint can object to (the counter row is the sole
// authority, `migrations/0001_init.sql`).

#[test]
fn insert_columns_pivots_each_record_into_the_column_it_is_bound_as() {
    // Two records whose leaves are all distinguishable from one another, so no
    // swap of a same-typed adjacent pair can land on an equal value and pass.
    // Three pairs are actually swappable without a type error —
    // `resource_ids`/`resource_types`, `subject_ids`/`subject_types`, and the
    // two bounds — and each is given differing values here. The invalidation
    // pair is not among them: `Vec<Option<Uuid>>` and `Vec<Option<String>>`
    // do not exchange.
    let tenant = uuid::Uuid::from_u128(0xD1);
    let target = uuid::Uuid::from_u128(0xD100);
    let mut plain = unit_record(tenant, "idem-a", 0xD101);
    plain.resource_ref = usage_collector_sdk::ResourceRef::new("res-a", "type-a")
        .expect("valid resource_ref");
    plain.subject_ref = Some(
        usage_collector_sdk::SubjectRef::new("subj-a", Some("subjtype-a".to_owned()))
            .expect("valid subject_ref"),
    );
    plain.metadata.insert(
        usage_collector_sdk::MetadataKey::new("region").expect("valid metadata key"),
        "eu-west".to_owned(),
    );
    let with = withdrawal(tenant, "idem-b", 0xD102, target);

    let cols = InsertColumns::build(&[&plain, &with], &[7, 8]);

    assert_eq!(cols.ids, vec![plain.id, with.id]);
    assert_eq!(cols.tenants, vec![tenant, tenant]);
    assert_eq!(
        cols.gts_type_ids,
        vec![VCPU_METER.to_owned(), VCPU_METER.to_owned()]
    );
    assert_eq!(cols.values, vec![plain.value, with.value]);
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
        cols.sequences,
        vec![7, 8],
        "the claimed acceptance sequences, in representative order"
    );
    assert_eq!(
        cols.metadata,
        vec![r#"{"region":"eu-west"}"#.to_owned(), "{}".to_owned()],
        "metadata is carried as text and cast ::jsonb per row in the query"
    );
}

#[test]
fn scope_runs_groups_the_contiguous_same_scope_representatives() {
    // `reps` reach this sorted by DedupKey, whose first two components are the
    // scope — so same-scope entries are contiguous and one pass finds them.
    let t1 = uuid::Uuid::from_u128(0xD2);
    let t2 = uuid::Uuid::from_u128(0xD3);
    let other_meter =
        usage_collector_sdk::MeterTypeId::new("gts.cf.core.uc.usage_record.v1~cf.storage._.gb_hours.v1~")
            .expect("valid meter id");

    let a1 = unit_record(t1, "a1", 0xD201);
    let a2 = unit_record(t1, "a2", 0xD202);
    let mut b = unit_record(t1, "b", 0xD203);
    b.gts_type_id = other_meter;
    let c = unit_record(t2, "c", 0xD204);

    assert_eq!(
        scope_runs(&[&a1, &a2, &b, &c]),
        vec![(0, 2), (2, 3), (3, 4)],
        "one run per scope: two entries on (t1, vcpu), then (t1, gb_hours), then (t2, vcpu)"
    );
    assert_eq!(scope_runs(&[]), vec![], "an empty batch claims nothing");
    assert_eq!(scope_runs(&[&a1]), vec![(0, 1)]);
}

#[test]
fn a_claimed_block_expands_to_the_values_below_its_returned_last() {
    // `claim_acceptance_sequence` returns the block's LAST value, because
    // `RETURNING next_value` yields the counter after adding `count`. On a
    // scope's first claim the counter goes 0 -> 3 and the block is 1, 2, 3.
    assert_eq!(sequence_block(3, 3), vec![1, 2, 3]);
    assert_eq!(sequence_block(1, 1), vec![1], "the single-row case");
    assert_eq!(
        sequence_block(10, 3),
        vec![8, 9, 10],
        "a later claim continues from wherever the counter stood"
    );
    assert!(
        sequence_block(5, 0).is_empty(),
        "an empty scope run claims no values"
    );
}

// --- `resolve_batch` defensive arm (DB-free) ---
//
// One arm remains that no happy path reaches: a not-won key absent from the
// conflict map. It is pinned here against a hand-built map.
//
// Two further `Internal` arms used to live here — "won the slot but no
// inserted record" and "intra-batch duplicate of a won key with no inserted
// record". Both were artefacts of passing `resolve_batch` a `won` set derived
// from `inserted`, which made a state representable that the code could not
// produce; their tests could only reach them through maps the code cannot
// build. `resolve_batch` now matches on `inserted` directly, so neither state
// nor arm nor test exists.

#[tokio::test]
async fn resolve_batch_missing_conflict_entry_falls_through_to_transient() {
    let store = lazy_store();
    let tenant = uuid::Uuid::from_u128(0xB4);
    let records = vec![unit_record(tenant, "absent", 0x40)];
    let plan = plan_batch(&records);

    // Not won, and the conflict map has no entry for the key at all. The
    // defensive `None` fallthrough must still be a retryable Transient — never a
    // silent success and never a panic.
    let inserted: HashMap<DedupKey, UsageRecordRow> = HashMap::new();
    let conflict: HashMap<DedupKey, ConflictRead> = HashMap::new();

    let results = store.resolve_batch(&records, &plan, &inserted, &conflict);

    assert_eq!(results.len(), 1);
    assert!(
        matches!(results[0], Err(UsageCollectorPluginError::Transient { .. })),
        "a key absent from the conflict map must fall through to Transient: {:?}",
        results[0]
    );
}

#[tokio::test]
async fn resolve_batch_conflicts_an_in_batch_duplicate_whose_canonical_fields_differ() {
    // Two input rows share a dedup key but disagree on `value`. Only the first
    // can win the slot; the second must resolve against what was written, which
    // means `canonical_equal` and an `IdempotencyConflict` — not a second copy
    // of the winner's row handed back as though it were this row's insert.
    //
    // This is what the `plan.first_index` guard buys. Without it every input row
    // holding a won key looks like a fresh insert, and a same-key submission
    // carrying different data absorbs silently.
    let store = lazy_store();
    let tenant = uuid::Uuid::from_u128(0xB6);
    let mut second = unit_record(tenant, "k", 0xB602);
    second.value = rust_decimal::Decimal::new(999, 0);
    let records = vec![unit_record(tenant, "k", 0xB601), second];
    assert_eq!(
        dedup_key(&records[0]),
        dedup_key(&records[1]),
        "test setup: the two rows must share a dedup key"
    );

    let plan = plan_batch(&records);
    let winner_row = row_matching(&records[0], serde_json::Value::Object(serde_json::Map::new()));
    let inserted: HashMap<DedupKey, UsageRecordRow> =
        HashMap::from([(dedup_key(&records[0]), winner_row)]);
    let conflict: HashMap<DedupKey, ConflictRead> = HashMap::new();

    let results = store.resolve_batch(&records, &plan, &inserted, &conflict);

    assert_eq!(results.len(), 2, "one result per input row, in input order");
    assert!(
        results[0].is_ok(),
        "the first occurrence wins its slot: {:?}",
        results[0]
    );
    match &results[1] {
        Err(UsageCollectorPluginError::IdempotencyConflict { existing_id, .. }) => {
            assert_eq!(
                *existing_id, records[0].id,
                "the conflict names the row already holding the slot"
            );
        }
        other => panic!("an in-batch duplicate carrying different data must conflict, got {other:?}"),
    }
}

#[tokio::test]
async fn resolve_batch_reports_a_pre_rejected_withdrawal_as_already_invalidated() {
    let store = lazy_store();
    let tenant = uuid::Uuid::from_u128(0xB5);
    let target = uuid::Uuid::from_u128(0xB500);
    let records = vec![
        withdrawal(tenant, "w1", 0xB501, target),
        withdrawal(tenant, "w2", 0xB502, target),
    ];
    let plan = plan_batch(&records);

    // Only the first withdrawal reaches the insert, and it wins its slot.
    let winner_row = row_matching(&records[0], serde_json::Value::Object(serde_json::Map::new()));
    let inserted: HashMap<DedupKey, UsageRecordRow> =
        HashMap::from([(dedup_key(&records[0]), winner_row)]);
    let conflict: HashMap<DedupKey, ConflictRead> = HashMap::new();

    let results = store.resolve_batch(&records, &plan, &inserted, &conflict);

    assert_eq!(results.len(), 2, "one result per input row, in input order");
    assert!(
        results[0].is_ok(),
        "the first withdrawal is accepted: {:?}",
        results[0]
    );
    match &results[1] {
        Err(UsageCollectorPluginError::AlreadyInvalidated { id, invalidated_by }) => {
            assert_eq!(*id, target, "the rejection names the target it tried to withdraw");
            assert_eq!(
                *invalidated_by,
                uuid::Uuid::from_u128(0xB501),
                "and the entry that already withdrew it"
            );
        }
        other => panic!("a second in-batch withdrawal must be AlreadyInvalidated, got {other:?}"),
    }
}

// --- Bounded-retry combinator (`with_retry`) — DB-free mechanics ---
//
// The combinator wraps the whole `create_batch_inner` call so a rare deadlock
// victim (`40P01` → outer `Transient`) self-heals. These tests pin its
// mechanics without a database; the `should_retry` predicate and the zero
// backoff are injected so each case is deterministic and instant.

/// Backoff fed to the combinator in tests: never actually sleep.
fn no_backoff(_attempt: u32) -> Duration {
    Duration::ZERO
}

#[tokio::test]
async fn with_retry_calls_operation_once_on_immediate_success() {
    let calls = AtomicU32::new(0);
    let retries = AtomicU32::new(0);
    let result: Result<u32, u32> = with_retry(
        3,
        no_backoff,
        |_err| true, // would retry, but the operation succeeds first try
        |_attempt, _err| {
            retries.fetch_add(1, Ordering::SeqCst);
        },
        || {
            calls.fetch_add(1, Ordering::SeqCst);
            std::future::ready(Ok::<u32, u32>(7))
        },
    )
    .await;

    assert_eq!(result, Ok(7), "first-try success is returned verbatim");
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "success on the first attempt runs the operation exactly once"
    );
    assert_eq!(
        retries.load(Ordering::SeqCst),
        0,
        "on_retry must not fire when the first attempt succeeds (no false retry signal)"
    );
}

#[tokio::test]
async fn with_retry_retries_a_retryable_error_then_returns_the_eventual_ok() {
    let calls = AtomicU32::new(0);
    // Fail (retryably) twice, then succeed on the third attempt.
    let result: Result<u32, u32> = with_retry(
        5,
        no_backoff,
        |_err| true,
        |_attempt, _err| {},
        || {
            let n = calls.fetch_add(1, Ordering::SeqCst) + 1;
            std::future::ready(if n < 3 { Err(n) } else { Ok(n) })
        },
    )
    .await;

    assert_eq!(result, Ok(3), "the eventual Ok is returned");
    assert_eq!(
        calls.load(Ordering::SeqCst),
        3,
        "two retryable failures then success -> N+1 = 3 calls"
    );
}

#[tokio::test]
async fn with_retry_invokes_on_retry_once_before_each_retry() {
    let retries = std::sync::Mutex::new(Vec::<u32>::new());
    // Fail (retryably) twice, then succeed on the third attempt.
    let calls = AtomicU32::new(0);
    let result: Result<u32, u32> = with_retry(
        5,
        no_backoff,
        |_err| true,
        |attempt, err| retries.lock().unwrap().push(attempt * 10 + *err),
        || {
            let n = calls.fetch_add(1, Ordering::SeqCst) + 1;
            std::future::ready(if n < 3 { Err(n) } else { Ok(n) })
        },
    )
    .await;

    assert_eq!(result, Ok(3), "the eventual Ok is returned");
    // on_retry fires once per retry, receiving the failed 1-based attempt number
    // and the error it failed with (encoded here as attempt*10 + err: attempt 1
    // failed with err 1 -> 11; attempt 2 failed with err 2 -> 22).
    assert_eq!(
        *retries.lock().unwrap(),
        vec![11, 22],
        "on_retry fires before each retry with the failed attempt number and error"
    );
}

#[tokio::test]
async fn with_retry_stops_at_max_attempts_and_returns_the_last_error() {
    let calls = AtomicU32::new(0);
    let result: Result<u32, u32> = with_retry(
        3,
        no_backoff,
        |_err| true, // always retryable, but the cap bounds it
        |_attempt, _err| {},
        || {
            let n = calls.fetch_add(1, Ordering::SeqCst) + 1;
            std::future::ready(Err::<u32, u32>(n))
        },
    )
    .await;

    assert_eq!(result, Err(3), "the last error is returned unchanged");
    assert_eq!(
        calls.load(Ordering::SeqCst),
        3,
        "a forever-retryable error runs exactly max_attempts times"
    );
}

#[tokio::test]
async fn with_retry_does_not_retry_a_non_retryable_error() {
    let calls = AtomicU32::new(0);
    let retries = AtomicU32::new(0);
    let result: Result<u32, u32> = with_retry(
        3,
        no_backoff,
        |_err| false, // predicate rejects every error → no retry
        |_attempt, _err| {
            retries.fetch_add(1, Ordering::SeqCst);
        },
        || {
            calls.fetch_add(1, Ordering::SeqCst);
            std::future::ready(Err::<u32, u32>(42))
        },
    )
    .await;

    assert_eq!(result, Err(42), "the non-retryable error is returned as-is");
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "a non-retryable error returns after a single attempt"
    );
    assert_eq!(
        retries.load(Ordering::SeqCst),
        0,
        "on_retry must not fire when the error is not retried"
    );
}

// --- The `create_batch` retry predicate ---

#[test]
fn batch_retry_predicate_retries_only_transient() {
    // Transient (the deadlock victim, serialization failure, connection blip
    // all collapse to this) → retry.
    assert!(
        is_retryable_batch_error(&UsageCollectorPluginError::transient(
            "deadlock victim; whole txn rolled back"
        )),
        "an outer Transient must be retried"
    );

    // Non-retryable buckets → no retry.
    assert!(
        !is_retryable_batch_error(&UsageCollectorPluginError::internal("invariant break")),
        "Internal must not be retried"
    );
    assert!(
        !is_retryable_batch_error(&UsageCollectorPluginError::IdempotencyConflict {
            idempotency_key: "k".to_owned(),
            existing_id: uuid::Uuid::from_u128(1),
        }),
        "IdempotencyConflict must not be retried"
    );
}

// --- The `create_batch` backoff schedule ---

#[test]
fn batch_retry_backoff_base_is_short_and_non_decreasing() {
    // The deterministic pre-jitter schedule: a deadlock victim can retry almost
    // immediately, so it stays small and never shrinks between attempts.
    let mut prev = Duration::ZERO;
    for attempt in 1..MAX_BATCH_ATTEMPTS {
        let d = batch_retry_backoff_base(attempt);
        assert!(
            d >= prev,
            "base backoff must not decrease (attempt {attempt})"
        );
        assert!(
            d <= Duration::from_millis(100),
            "base backoff stays small for a deadlock victim (attempt {attempt}): {d:?}"
        );
        prev = d;
    }
}

#[test]
fn batch_retry_backoff_applies_full_jitter_within_base() {
    // Full jitter: every sampled backoff lands in `[0, base]`, so batches that
    // deadlocked together spread across the window instead of retrying in
    // lockstep (thundering herd). Sampled repeatedly since the value is random
    // per call; the run must also observe at least one sub-base value, proving
    // the jitter is actually applied and not a no-op.
    for attempt in 1..MAX_BATCH_ATTEMPTS {
        let base = batch_retry_backoff_base(attempt);
        let mut seen_below_base = false;
        for _ in 0..1_000 {
            let d = batch_retry_backoff(attempt);
            assert!(
                d <= base,
                "jittered backoff must not exceed its base (attempt {attempt}): {d:?} > {base:?}"
            );
            if d < base {
                seen_below_base = true;
            }
        }
        assert!(
            seen_below_base,
            "full jitter must sometimes back off less than the base (attempt {attempt})"
        );
    }
}

#[tokio::test]
async fn acquire_failure_clears_ready_gauge() {
    use opentelemetry::metrics::MeterProvider;
    use opentelemetry_sdk::metrics::data::{AggregatedMetrics, MetricData};
    use opentelemetry_sdk::metrics::{InMemoryMetricExporter, PeriodicReader, SdkMeterProvider};

    // Local in-memory meter provider so the gauge read is parallel-safe (never
    // touches opentelemetry::global), mirroring metrics_tests.
    let exporter = InMemoryMetricExporter::default();
    let provider = SdkMeterProvider::builder()
        .with_reader(PeriodicReader::builder(exporter.clone()).build())
        .build();

    // A lazy pool pointed at a dead port: the first acquire is refused fast.
    let pool = PgPoolOptions::new()
        .acquire_timeout(Duration::from_millis(200))
        .connect_lazy("postgres://user:pass@127.0.0.1:1/db")
        .expect("a syntactically valid DSN yields a lazy pool without connecting");
    let metrics = Arc::new(Metrics::with_meter(
        &provider.meter("uc.timescaledb"),
        pool.clone(),
    ));
    let store = PgRecordStore::new(pool, metrics, CancellationToken::new());

    // Every operation routes through timed_acquire; call it directly.
    let result = store.timed_acquire().await;
    assert!(result.is_err(), "acquire against a dead port must fail");

    provider.force_flush().expect("flush in-memory metrics");

    // Read the last value of the `uc_timescaledb_ready` gauge.
    let ready = {
        let metrics = exporter.get_finished_metrics().expect("collected metrics");
        let mut found = None;
        for rm in &metrics {
            for sm in rm.scope_metrics() {
                for m in sm.metrics() {
                    if m.name() == "uc_timescaledb_ready"
                        && let AggregatedMetrics::U64(MetricData::Gauge(g)) = m.data()
                    {
                        found = g
                            .data_points()
                            .next()
                            .map(opentelemetry_sdk::metrics::data::GaugeDataPoint::value);
                    }
                }
            }
        }
        found
    };
    assert_eq!(
        ready,
        Some(0),
        "a pool-acquire failure must clear the readiness gauge to 0"
    );
}

// --- Task 10: the point lookup intersects the compiled scope -----------------
//
// The SPI hands `get` the caller's compiled PDP scope and no caller `$filter`,
// so the scope is the whole filter beyond the `id`. Slice 3 retired the
// gateway's in-process per-record attribution check, which means nothing above
// the SPI re-reads the row that comes back: everything below tests the only
// remaining place "exists but not yours reads as NotFound" is kept.

/// The tenant the scopes below admit.
const SCOPE_TENANT_A: &str = "11111111-1111-1111-1111-111111111111";
/// A second admitted tenant, so a scope can take the disjunctive shape
/// `authz::scope_to_odata_filter` actually compiles.
const SCOPE_TENANT_B: &str = "22222222-2222-2222-2222-222222222222";

/// Compile a scope the way the gateway hands one over: an
/// `toolkit_odata::ast::Expr`. `authz::scope_to_odata_filter` builds the same
/// AST from PDP constraints; a filter string is just its readable spelling, and
/// keeps these tests naming the predicate rather than assembling `Box<Expr>`
/// towers.
fn parse_scope(raw: &str) -> ast::Expr {
    toolkit_odata::parse_filter_string(raw)
        .unwrap_or_else(|e| panic!("the test's own scope must parse: {e}"))
        .into_expr()
}

/// The `WHERE …` tail of a built statement, so an expectation can be written
/// out by hand without restating [`RECORD_COLUMNS`] — which has its own,
/// independent, oracle elsewhere.
fn where_clause(sql: &str) -> String {
    sql.split_once(" FROM usage_records ")
        .unwrap_or_else(|| panic!("the point lookup must read `usage_records`. got: {sql}"))
        .1
        .to_owned()
}

#[test]
fn the_point_lookup_renders_the_scope_as_its_whole_where_clause() {
    let scope = parse_scope(&format!("tenant_id eq {SCOPE_TENANT_A}"));

    let (sql, _binds) = build_get_sql(&scope).expect("a scope must render");

    assert!(
        sql.contains("WHERE id = $1 AND ("),
        "the point lookup carries no caller filter, so the compiled scope is \
         the whole filter the row must satisfy; a lookup that selects on id \
         alone is an existence oracle. got: {sql}"
    );
}

#[test]
fn the_point_lookup_binds_the_scope_values_after_the_id() {
    // Two conjuncts of different value types, so a dropped or reordered bind
    // shows up as a different variant rather than a different uuid.
    let scope = parse_scope(&format!(
        "tenant_id eq {SCOPE_TENANT_A} and resource_type eq 'vm'"
    ));

    let (sql, binds) = build_get_sql(&scope).expect("a scope must render");

    // The `id` owns `$1`, so the scope's own binds start at `$2`. If they
    // started at `$1` the tenant predicate would read the id bind and the
    // statement would carry one more parameter than it names.
    assert_eq!(
        where_clause(&sql),
        "WHERE id = $1 AND ((tenant_id = $2 AND resource_type = $3))",
        "the scope's binds follow the id, which occupies $1"
    );
    assert_eq!(
        binds.len(),
        2,
        "both scope operands must be bound, in placeholder order. got: {binds:?}"
    );
    assert!(
        matches!(&binds[0], SqlBind::Uuid(u) if u.to_string() == SCOPE_TENANT_A),
        "$2 is the tenant the scope pins. got: {:?}",
        binds[0]
    );
    assert!(
        matches!(&binds[1], SqlBind::Str(s) if s == "vm"),
        "$3 is the resource type the scope pins. got: {:?}",
        binds[1]
    );
}

#[test]
fn the_point_lookup_wraps_a_disjunctive_scope_in_its_own_parentheses() {
    // The shape `authz::scope_to_odata_filter` compiles: a disjunction of
    // tenant-pinned conjunctions. Conjoined without parentheses of its own,
    // `id = $1 AND A OR B` binds as `(id = $1 AND A) OR B` and answers every
    // row matching the last disjunct, whatever id was asked for.
    let scope = parse_scope(&format!(
        "(tenant_id eq {SCOPE_TENANT_A} and resource_type eq 'vm') or \
         (tenant_id eq {SCOPE_TENANT_B} and resource_type eq 'vm')"
    ));

    let (sql, binds) = build_get_sql(&scope).expect("a scope must render");

    // Transcribed by hand, not derived from anything the builder produces.
    assert_eq!(
        where_clause(&sql),
        "WHERE id = $1 AND (((tenant_id = $2 AND resource_type = $3) \
         OR (tenant_id = $4 AND resource_type = $5)))",
        "every disjunct of the scope has to survive the conjunction with the id"
    );
    assert_eq!(binds.len(), 4, "four operands, four binds");
}

#[test]
fn the_point_lookup_carries_no_invalidation_predicate() {
    // Step 4's obligation, as a test rather than only a comment. The fold
    // excludes a withdrawn pair; this path is the ledger and MUST return one as
    // persisted. The scope below names neither column, so anything matching in
    // the predicate was added by the query builder. The select list is exempt
    // and has to be: `invalidates` is one of the columns a withdrawn entry is
    // read back *through*.
    let scope = parse_scope(&format!("tenant_id eq {SCOPE_TENANT_A}"));

    // This covers the SQL half only. The other way to withhold a withdrawn
    // entry is to read the row and then drop it — `if row.invalidates.is_some()
    // { return NotFound }` in `get`'s `Some(row)` arm — which no unit test here
    // can see, because none executes a statement. Task 15 covers that half,
    // against a stored pair.
    let (sql, _binds) = build_get_sql(&scope).expect("a scope must render");
    let predicate = where_clause(&sql);

    assert!(
        !predicate.contains("invalidates"),
        "a withdrawn entry MUST be returned as persisted here; an `invalidates` \
         predicate on the ledger path destroys the audit trail the append-only \
         model exists to keep, and the exclusion belongs to the fold. \
         got: {predicate}"
    );
    assert!(
        !predicate.contains("entry_type"),
        "restricting the point lookup to `entry_type = 'record'` withholds the \
         invalidation half of a withdrawn pair, which is the same data loss by \
         another spelling. got: {predicate}"
    );
}

#[test]
fn the_point_lookup_renders_a_membership_scope_over_several_tenants() {
    // `scope_to_odata_filter` pins the owning tenant with `Eq` *or* `In`, so a
    // multi-tenant grant reaches the plugin as a membership test. Every other
    // test here only ever hands it `Eq`.
    let scope = parse_scope(&format!("tenant_id in ({SCOPE_TENANT_A}, {SCOPE_TENANT_B})"));

    let (sql, binds) = build_get_sql(&scope).expect("a scope must render");

    assert_eq!(
        where_clause(&sql),
        "WHERE id = $1 AND (tenant_id IN ($2, $3))",
        "a membership scope narrows the lookup to the tenants it names"
    );
    assert_eq!(binds.len(), 2, "one bind per named tenant");
}

#[test]
fn a_scope_naming_a_field_off_the_allowlist_is_refused() {
    // `gts_type_id` is a typed SPI parameter, deliberately absent from the
    // filterable schema and from `record_column`. A scope naming it must be an
    // error: dropping the unrenderable conjunct would leave `WHERE id = $1`,
    // turning a translation failure into an authorization bypass.
    let scope = parse_scope("gts_type_id eq 'gts.cf.core.uc.usage_record.v1~'");

    let Err(err) = build_get_sql(&scope) else {
        panic!("a scope naming a field off the allowlist must not render");
    };

    assert!(
        err.contains("gts_type_id"),
        "the refusal names the field it refused. got: {err}"
    );
}

#[tokio::test]
async fn a_scope_that_fails_to_translate_never_reaches_the_pool() {
    // The half the pure test above cannot see: that `get` propagates the
    // refusal instead of reading without it. The store's pool is lazy and
    // points at nothing, so any path that got as far as acquiring a connection
    // answers `Transient` (a pool timeout). An `Internal` naming the scope is
    // therefore proof the lookup stopped before it read anything.
    let store = lazy_store();
    let scope = parse_scope("gts_type_id eq 'gts.cf.core.uc.usage_record.v1~'");

    let Err(err) = store.get(uuid::Uuid::from_u128(1), &scope).await else {
        panic!("an untranslatable scope must not yield a row");
    };

    match err {
        UsageCollectorPluginError::Internal(message) => assert!(
            message.contains("gts_type_id"),
            "the refusal reaches the caller as-is. got: {message}"
        ),
        other => panic!(
            "an untranslatable scope must stop the lookup before it acquires a \
             connection; reaching the pool would answer Transient. got: {other:?}"
        ),
    }
}

// --- Read-half tests (Tasks 11-12) ------------------------------------------
//
// These still exercise the retired column model and the pre-port `list`
// signature. They are `list`'s and `aggregate`'s to bring current; nothing
// below this line is Task 9's or Task 10's.

#[tokio::test]
async fn list_rejects_cursor_whose_sort_order_differs_from_query() {
    let store = lazy_store();
    let gts_id = UsageTypeGtsId::new(VCPU_METER).expect("valid gts id");

    // The live query sorts (created_at asc, id asc); the cursor was minted
    // under a different order (id first). The keys are individually valid, so
    // without the guard the request binds old key strings against new columns —
    // silently wrong pagination. The filter hash agrees (both unset), so only
    // the sort-order guard can reject this.
    let query = ODataQuery::new()
        .with_order(ODataOrderBy(vec![
            OrderKey {
                field: "created_at".to_owned(),
                dir: SortDir::Asc,
            },
            OrderKey {
                field: "id".to_owned(),
                dir: SortDir::Asc,
            },
        ]))
        .with_cursor(CursorV1 {
            k: vec![
                "2024-01-01T00:00:00Z".to_owned(),
                "00000000-0000-0000-0000-000000000001".to_owned(),
            ],
            o: SortDir::Asc,
            s: "+id,+created_at".to_owned(),
            f: None,
            d: "fwd".to_owned(),
        });

    let err = store
        .list(gts_id, &query, &[])
        .await
        .expect_err("a cursor minted under a different order must be rejected");

    match err {
        UsageCollectorPluginError::Internal(msg) => {
            assert!(
                msg.contains("sort order"),
                "unexpected error message: {msg}"
            );
        }
        other => panic!("expected an Internal sort-order mismatch, got {other:?}"),
    }
}

#[tokio::test]
async fn list_rejects_backward_cursor() {
    let store = lazy_store();
    let gts_id = UsageTypeGtsId::new(VCPU_METER).expect("valid gts id");

    // A backward cursor whose filter hash and sort order both agree with the
    // query, so only the direction guard can reject it. Without the guard the
    // request would page FORWARD (the keyset operator is derived from the sort
    // direction, not `d`) and silently return the wrong page.
    let query = ODataQuery::new()
        .with_order(ODataOrderBy(vec![
            OrderKey {
                field: "created_at".to_owned(),
                dir: SortDir::Asc,
            },
            OrderKey {
                field: "id".to_owned(),
                dir: SortDir::Asc,
            },
        ]))
        .with_cursor(CursorV1 {
            k: vec![
                "2024-01-01T00:00:00Z".to_owned(),
                "00000000-0000-0000-0000-000000000001".to_owned(),
            ],
            o: SortDir::Asc,
            s: "+created_at,+id".to_owned(),
            f: None,
            d: "bwd".to_owned(),
        });

    let err = store
        .list(gts_id, &query, &[])
        .await
        .expect_err("a backward cursor must be rejected before any DB access");

    match err {
        UsageCollectorPluginError::Internal(msg) => {
            assert!(msg.contains("direction"), "unexpected error message: {msg}");
        }
        other => panic!("expected an Internal direction error, got {other:?}"),
    }
}
