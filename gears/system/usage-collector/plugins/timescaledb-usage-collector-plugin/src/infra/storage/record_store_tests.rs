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
use usage_collector_sdk::{
    AggregationDimension, AggregationFold, MetadataFilter, MetadataKey, UsageCollectorPluginError,
};

use super::{
    AggregateStatement, BATCH_INSERT_SQL, ConflictRead, DedupKey, INSERT_COLUMN_ARRAY_TYPES,
    INSERT_COLUMNS, InsertColumns, MAX_BATCH_ATTEMPTS, PgRecordStore, RECORD_COLUMNS,
    SINGLE_INSERT_SQL, batch_retry_backoff, batch_retry_backoff_base, build_aggregate_sql,
    build_get_sql, build_list_page, build_list_sql, canonical_equal, dedup_key,
    invalidation_index_slots, is_retryable_batch_error, plan_batch, record_row_key, row_dedup_key,
    scope_runs, sequence_block, with_retry,
};
use crate::domain::ports::RecordStore;
use crate::infra::metrics::Metrics;
use crate::infra::storage::entity::UsageRecordRow;
use crate::infra::storage::migration_probe;
use crate::infra::storage::query::aggregate::{fold_select_expr, withdrawal_exclusion_clause};
use crate::infra::storage::query::ledger_from_clause;
use crate::infra::storage::query::translate::{SqlBind, record_column};

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
    row.origin = usage_collector_sdk::RecordOrigin::Backfill
        .as_str()
        .to_owned();

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
/// must `UNNEST` it as, **derived from `migrations/0001_init.sql`** by
/// [`migration_probe::insertable_columns`] rather than from the code under
/// test.
///
/// This is the whole point of the pairing test below. Checking the generated
/// SQL against `INSERT_COLUMN_ARRAY_TYPES` proves only that the SQL was built
/// from that array — transpose two entries and both sides move together, which
/// is exactly how the first version of this test let such a mutation survive. A
/// transposition has to be measured against something that does not move, and
/// the migration is that thing.
///
/// It was a hand transcription of the migration until this task, which is the
/// same failure one step removed: a second spelling of the schema, kept by
/// hand, silently stale the first time the DDL moves without it. The parse has
/// nothing of its own to forget.
///
/// One deliberate divergence from the DDL, and the only entry still written by
/// hand here: `metadata` is `jsonb` in the table but travels as `text[]` and is
/// cast `::jsonb` per row, because `jsonb[]` array encoding is the thing being
/// sidestepped.
fn ddl_column_array_types() -> Vec<(&'static str, &'static str)> {
    migration_probe::insertable_columns()
        .into_iter()
        .map(|(name, ty)| match name {
            "metadata" => (name, "text"),
            _ => (name, ty),
        })
        .collect()
}

#[test]
fn each_inserted_column_is_unnested_as_the_type_the_migration_declares() {
    let ddl = ddl_column_array_types();
    let want_names: Vec<&str> = ddl.iter().map(|(n, _)| *n).collect();
    let want_types: Vec<&str> = ddl.iter().map(|(_, t)| *t).collect();

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
    let other_meter = usage_collector_sdk::MeterTypeId::new(
        "gts.cf.core.uc.usage_record.v1~cf.storage._.gb_hours.v1~",
    )
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
    let winner_row = row_matching(
        &records[0],
        serde_json::Value::Object(serde_json::Map::new()),
    );
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
        other => {
            panic!("an in-batch duplicate carrying different data must conflict, got {other:?}")
        }
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
    let winner_row = row_matching(
        &records[0],
        serde_json::Value::Object(serde_json::Map::new()),
    );
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
            assert_eq!(
                *id, target,
                "the rejection names the target it tried to withdraw"
            );
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

/// Everything a built statement says after its `FROM`, so an expectation can be
/// written out by hand without restating [`RECORD_COLUMNS`] — which has its
/// own, independent, oracle elsewhere.
///
/// For the point lookup that is the `WHERE` alone; for the keyset page it also
/// carries the table alias, the `ORDER BY` and the `LIMIT`, which is what lets
/// one hand-transcribed string pin that the keyset tuple and the `ORDER BY`
/// read the same order. Named for what it returns rather than for the clause it
/// started out returning.
fn statement_tail(sql: &str) -> String {
    sql.split_once(" FROM usage_records ")
        .unwrap_or_else(|| panic!("a built statement must read `usage_records`. got: {sql}"))
        .1
        .to_owned()
}

#[test]
fn the_point_lookup_renders_the_scope_as_its_whole_statement_tail() {
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
        statement_tail(&sql),
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
fn the_point_lookups_disjunctive_scope_survives_the_conjunction_with_the_id() {
    // The shape a multi-constraint grant compiles to through
    // `authz::scope_to_odata_filter`: a disjunction of tenant-pinned
    // conjunctions. Conjoined without parentheses of its own,
    // `id = $1 AND A OR B` binds as `(id = $1 AND A) OR B` and answers every
    // row matching the last disjunct, whatever id was asked for. The wrap that
    // prevents that is `translate_scope`'s, not this builder's.
    let scope = parse_scope(&format!(
        "(tenant_id eq {SCOPE_TENANT_A} and resource_type eq 'vm') or \
         (tenant_id eq {SCOPE_TENANT_B} and resource_type eq 'vm')"
    ));

    let (sql, binds) = build_get_sql(&scope).expect("a scope must render");

    // Transcribed by hand, not derived from anything the builder produces.
    assert_eq!(
        statement_tail(&sql),
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
    let predicate = statement_tail(&sql);

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
    let scope = parse_scope(&format!(
        "tenant_id in ({SCOPE_TENANT_A}, {SCOPE_TENANT_B})"
    ));

    let (sql, binds) = build_get_sql(&scope).expect("a scope must render");

    assert_eq!(
        statement_tail(&sql),
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

// --- The cursor key on the row (Task 11) ------------------------------------
//
// `record_row_key` is the inverse of `record_column` on the pagination path:
// one resolves an order field to the column the `ORDER BY` and the keyset tuple
// are rendered from, the other resolves the same field to the boundary value
// the minted cursor carries. Nothing in the type system couples them, and after
// the row model was ported the two disagreed silently — `record_column`
// resolved `window_start`, `window_end` and `origin` while `record_row_key`
// answered `None` for all three, which is a `500` at the mint on the canonical
// `(window_end, id)` order and no compile error anywhere.

/// A row whose every keyset-safe column carries a value distinguishable from
/// every other one, so an arm reading a neighbouring column produces a visibly
/// wrong string rather than a plausible one.
///
/// `subject_id` and `subject_type` are deliberately *present*: a `None` from
/// `record_row_key` on either has to be "not a keyset key", not "the column was
/// NULL".
fn keyed_row() -> UsageRecordRow {
    UsageRecordRow {
        id: uuid::Uuid::from_u128(0xA1),
        tenant_id: uuid::Uuid::from_u128(0xB2),
        gts_type_id: VCPU_METER.to_owned(),
        value: rust_decimal::Decimal::new(7, 0),
        window_start: time::OffsetDateTime::from_unix_timestamp(WINDOW_START_UNIX)
            .expect("valid ts"),
        window_end: time::OffsetDateTime::from_unix_timestamp(WINDOW_END_UNIX).expect("valid ts"),
        resource_id: "res-keyed".to_owned(),
        resource_type: "compute.vm".to_owned(),
        subject_id: Some("subject-keyed".to_owned()),
        subject_type: Some("user".to_owned()),
        idempotency_key: "idem-keyed".to_owned(),
        invalidates: None,
        reason_code: None,
        origin: "backfill".to_owned(),
        acceptance_sequence: 9,
        metadata: serde_json::json!({}),
        ingested_at: time::OffsetDateTime::from_unix_timestamp(WINDOW_END_UNIX).expect("valid ts"),
    }
}

/// Every keyset-safe order field paired with the boundary value
/// [`keyed_row`] must yield for it, transcribed by hand rather than read back
/// out of the row.
///
/// The field names are asserted against the SDK's own list below, so this
/// pairing cannot quietly cover a subset of it.
const KEYED_ROW_BOUNDARIES: [(&str, &str); 7] = [
    ("id", "00000000-0000-0000-0000-0000000000a1"),
    ("window_start", "2023-11-14T22:13:20Z"),
    ("window_end", "2023-11-14T23:13:20Z"),
    ("tenant_id", "00000000-0000-0000-0000-0000000000b2"),
    ("resource_id", "res-keyed"),
    ("resource_type", "compute.vm"),
    ("origin", "backfill"),
];

#[test]
fn every_keyset_safe_order_field_has_a_cursor_key_on_the_row() {
    // Driven off the SDK's list rather than a local copy, so a field added
    // there fails here — at build time, on a named arm — instead of at the
    // mint, in production, on whichever caller first ordered by it.
    let row = keyed_row();

    for field in usage_collector_sdk::KEYSET_SAFE_RECORD_FIELDS {
        assert!(
            record_row_key(&row, field).is_some(),
            "`{field}` is an admissible $orderby key, so a page ending on this \
             row has to be able to mint a boundary for it"
        );
    }
}

#[test]
fn each_cursor_key_reads_the_column_its_order_field_names() {
    // The half an `is_some()` sweep cannot see. Every arm returns *a* string
    // either way; only a named value tells `window_end` reading `window_start`
    // apart from `window_end` reading `window_end`, and that mis-pointing
    // renders a boundary the `ORDER BY` never sorted by — the page resumes
    // somewhere else and reports nothing.
    let row = keyed_row();

    let named: Vec<&str> = KEYED_ROW_BOUNDARIES
        .iter()
        .map(|(field, _)| *field)
        .collect();
    assert_eq!(
        named.as_slice(),
        usage_collector_sdk::KEYSET_SAFE_RECORD_FIELDS,
        "the hand-written pairing has to cover the SDK's whole keyset vocabulary"
    );

    for (field, expected) in KEYED_ROW_BOUNDARIES {
        assert_eq!(
            record_row_key(&row, field).as_deref(),
            Some(expected),
            "`{field}` must read the column `record_column` resolves it to"
        );
    }
}

#[test]
fn a_field_that_is_not_a_keyset_key_mints_no_boundary() {
    // All four resolve through `record_column`, so all four can appear in an
    // `ORDER BY` that renders. None is keyset-safe: three are domain-optional
    // and `entry_type` is derived from an optional attribute, so a row-value
    // tuple over any of them can compare as NULL and drop rows out of the page
    // silently. Refusing to mint is the fail-closed answer, and `subject_id` /
    // `subject_type` are populated on this row so the refusal cannot be read as
    // a NULL column.
    let row = keyed_row();

    for field in ["subject_id", "subject_type", "invalidates", "entry_type"] {
        assert!(
            record_column(field).is_some(),
            "the premise of this test: `{field}` is an allowlisted column"
        );
        assert!(
            !usage_collector_sdk::is_keyset_safe_record_field(field),
            "the premise of this test: `{field}` is not keyset-safe"
        );
        assert!(
            record_row_key(&row, field).is_none(),
            "`{field}` is not a keyset key, so it must not seed a boundary"
        );
    }
}

// --- The keyset page (Task 11) ----------------------------------------------
//
// `build_list_sql` and `build_list_page` are the two pure halves of `list`;
// what is left in `list` itself is the round trip between them. Everything
// below is asserted on those two, without a database. That matters here
// because the fingerprint obligation has no compiler backstop and no visible
// effect until page two: a unit test is what makes the mint observable at the
// moment it happens, rather than a page later. (The live walk in
// `tests/records_query_integration_pg.rs` decodes a `next_cursor` too — it is
// stale and Task 15's, and it is a page later and behind Docker.)

/// A stand-in for the gateway's read fingerprint. Opaque to the plugin: it
/// covers the caller's `$filter` together with all three typed parameters, and
/// nothing here may interpret or recompute it.
///
/// **Deliberately not the shape the gateway mints** — `read_fingerprint` emits
/// sixteen bare hex characters, and this carries a prefix and is longer. That
/// is the point: a fixture in the real shape would let an implementation start
/// depending on that shape with no test noticing, and the SPI says the value is
/// opaque and "its shape is the gateway's to change".
const READ_FINGERPRINT: &str = "sha256:0f1e2d3c4b5a69788796a5b4c3d2e1f0";

/// `[2023-11-14T00:00:00Z, 2023-11-15T00:00:00Z)` — the day that contains
/// every fixture's covered period.
const RANGE_FROM_UNIX: i64 = 1_699_920_000;
const RANGE_TO_UNIX: i64 = 1_700_006_400;

fn list_meter() -> usage_collector_sdk::MeterTypeId {
    usage_collector_sdk::MeterTypeId::new(VCPU_METER).expect("valid meter id")
}

fn list_range() -> usage_collector_sdk::TimeRange {
    usage_collector_sdk::TimeRange::new(
        time::OffsetDateTime::from_unix_timestamp(RANGE_FROM_UNIX).expect("valid ts"),
        time::OffsetDateTime::from_unix_timestamp(RANGE_TO_UNIX).expect("valid ts"),
    )
    .expect("a strictly ordered range")
}

/// The gateway's default page order: `(window_end, id)`, both ascending.
fn canonical_order() -> ODataOrderBy {
    ODataOrderBy(vec![
        OrderKey {
            field: "window_end".to_owned(),
            dir: SortDir::Asc,
        },
        OrderKey {
            field: "id".to_owned(),
            dir: SortDir::Asc,
        },
    ])
}

/// A first-page query in the shape the gateway guarantees: a non-empty
/// single-direction order naming both canonical fields, and a fingerprint.
fn list_query() -> ODataQuery {
    ODataQuery::new()
        .with_order(canonical_order())
        .with_filter_hash(READ_FINGERPRINT.to_owned())
}

/// One stored row, distinct by `id`, mappable back to the SDK model.
fn list_row(seq: u128) -> UsageRecordRow {
    row_matching(
        &unit_record(uuid::Uuid::from_u128(0xD0), &format!("idem-{seq}"), seq),
        serde_json::json!({}),
    )
}

#[test]
fn selection_reads_the_period_end_alone() {
    // `from <= window_end < to`
    // (`cpt-cf-usage-collector-adr-window-end-selection`). Not overlap, which
    // selects one entry into two adjacent ranges, and not containment, which
    // drops it out of both — a covered period longer than the range is what
    // tells the three apart. The predicate therefore never reads
    // `window_start`, and the negative half of this assertion is the half that
    // says so.
    let query = list_query();

    let (sql, binds) = build_list_sql(&list_meter(), list_range(), &query, &[], 25)
        .expect("the canonical first page must render");
    let tail = statement_tail(&sql);

    assert!(
        tail.contains("r.window_end >= $2"),
        "the lower bound is inclusive on the period end. got: {tail}"
    );
    assert!(
        tail.contains("r.window_end < $3"),
        "the upper bound is exclusive on the period end. got: {tail}"
    );
    assert!(
        !tail.contains("window_start"),
        "the time-range predicate must not read the period start: selecting on \
         it fails window-end-selection and quantity-round-trip alike, the \
         latter because its read-back range is one second wide at the period \
         end while the period began an hour earlier. got: {tail}"
    );
    // The clause text above pins the predicate; these pin what is bound into
    // it. Without them the two bounds can be swapped — inverting the range to
    // `window_end >= to AND window_end < from`, which selects nothing, ever —
    // or bound from one end twice, or the meter can be bound as an empty
    // string, which drops the scoping to one meter without changing a
    // character of the SQL.
    assert_eq!(binds.len(), 3, "the meter and the two range bounds");
    assert!(
        matches!(&binds[0], SqlBind::Str(s) if s == VCPU_METER),
        "$1 is the meter this page reads. got: {:?}",
        binds[0]
    );
    assert!(
        matches!(&binds[1], SqlBind::DateTime(t) if t.unix_timestamp() == RANGE_FROM_UNIX),
        "$2 is the range's inclusive lower bound. got: {:?}",
        binds[1]
    );
    assert!(
        matches!(&binds[2], SqlBind::DateTime(t) if t.unix_timestamp() == RANGE_TO_UNIX),
        "$3 is the range's exclusive upper bound. got: {:?}",
        binds[2]
    );
}

#[test]
fn the_minted_cursor_carries_the_gateways_filter_hash_verbatim() {
    // The one SPI requirement with no compiler backstop of its own: a plugin
    // that drops it recompiles clean and paginates exactly once. The gateway
    // recomputes the same string from the follow-up request and refuses a token
    // carrying a different one, or none, with FilterMismatch.
    //
    // Two rows for a page of one: the look-ahead row is what makes a cursor get
    // minted at all.
    let query = list_query();

    let page = build_list_page(vec![list_row(1), list_row(2)], &query, 1)
        .expect("a look-ahead page must assemble");

    assert_eq!(
        page.items.len(),
        1,
        "the look-ahead row is dropped, not served"
    );
    let token = page
        .page_info
        .next_cursor
        .expect("a look-ahead row means a next page");
    let decoded = CursorV1::decode(&token).expect("the minted token round-trips");
    assert_eq!(
        decoded.f.as_deref(),
        Some(READ_FINGERPRINT),
        "the fingerprint travels through untouched: not recomputed, not \
         dropped, not replaced"
    );
}

#[test]
fn a_page_that_did_not_fill_mints_no_continuation() {
    // No look-ahead row, so there is nothing after this page and a cursor would
    // invite the caller to read an empty one.
    let query = list_query();

    let page = build_list_page(vec![list_row(1)], &query, 25).expect("a short page must assemble");

    assert_eq!(page.items.len(), 1);
    assert!(page.page_info.next_cursor.is_none());
    assert_eq!(
        page.page_info.limit, 25,
        "the clamped page size, not the row count"
    );
}

#[test]
fn an_exactly_full_page_mints_no_continuation() {
    // The boundary case the look-ahead comparison decides, and the only one
    // that tells `>` from `>=`. A page holding exactly `limit` rows had no
    // look-ahead row, so there is nothing after it. Under `>=` it would be
    // treated as overfull: the last row would be truncated away — dropped from
    // the caller's results entirely — and a continuation minted from the row
    // before it.
    let query = list_query();

    let page =
        build_list_page(vec![list_row(1)], &query, 1).expect("an exactly-full page must assemble");

    assert_eq!(
        page.items.len(),
        1,
        "an exactly-full page serves every row it read"
    );
    assert!(
        page.page_info.next_cursor.is_none(),
        "no look-ahead row was read, so nothing follows this page"
    );
}

#[test]
fn the_minted_boundary_is_read_in_the_order_it_was_handed() {
    // `window_end` and `id` are guaranteed present in the order, not guaranteed
    // last: a caller ordering by `id` is handed on as `(id, window_end)`. The
    // keys are read in that order, so a mint that assumed a canonical slot
    // would encode them transposed and the continuation would compare a uuid
    // against a timestamptz.
    let query = ODataQuery::new()
        .with_order(ODataOrderBy(vec![
            OrderKey {
                field: "id".to_owned(),
                dir: SortDir::Asc,
            },
            OrderKey {
                field: "window_end".to_owned(),
                dir: SortDir::Asc,
            },
        ]))
        .with_filter_hash(READ_FINGERPRINT.to_owned());

    // Three rows for a page of two, so the page's last row is not also its
    // first: with two rows at a limit of one the two coincide and a boundary
    // minted from `first()` reads exactly like one minted from `last()`. The
    // boundary has to be the row the caller last saw, or the continuation
    // re-serves rows this page already delivered.
    let page = build_list_page(vec![list_row(1), list_row(2), list_row(3)], &query, 2)
        .expect("an id-led page must assemble");

    assert_eq!(
        page.items
            .iter()
            .map(|r| r.id.to_string())
            .collect::<Vec<_>>(),
        vec![
            "00000000-0000-0000-0000-000000000001".to_owned(),
            "00000000-0000-0000-0000-000000000002".to_owned(),
        ],
        "the look-ahead row is dropped, and the page serves the rest in the \
         order it read them, so the boundary below is the key of the last row \
         the caller actually saw. Counting the items instead leaves the two \
         unrelated: reverse them and every continuation re-serves or skips \
         rows while the boundary assertion goes on passing"
    );
    let token = page
        .page_info
        .next_cursor
        .expect("a look-ahead row means a next page");
    let decoded = CursorV1::decode(&token).expect("the minted token round-trips");

    // Transcribed by hand: `list_row(2)`'s id — the last row of the page, not
    // its first — then the covered-period end every fixture shares.
    assert_eq!(
        decoded.k,
        vec![
            "00000000-0000-0000-0000-000000000002".to_owned(),
            "2023-11-14T23:13:20Z".to_owned(),
        ],
        "one key per order field, in the order's own field order, read off the \
         last row of the page"
    );
    assert_eq!(
        decoded.s, "+id,+window_end",
        "the token is bound to that order"
    );
}

#[test]
fn a_page_minted_without_a_fingerprint_is_refused_rather_than_shipped() {
    // An absent `query.filter_hash` is a gateway breach, not a case to paper
    // over. Papering over it means minting `f: None` — a token the gateway
    // refuses on page two, from a page the plugin reported as healthy.
    let query = ODataQuery::new().with_order(canonical_order());

    let err = build_list_page(vec![list_row(1), list_row(2)], &query, 1)
        .expect_err("a mint without a fingerprint must fail loudly");

    match err {
        UsageCollectorPluginError::Internal(msg) => assert!(
            msg.contains("filter_hash"),
            "the refusal names what was missing. got: {msg}"
        ),
        other => panic!("expected an Internal gateway-breach error, got {other:?}"),
    }
}

#[test]
fn the_composed_filter_survives_the_conjunction_with_the_range() {
    // What arrives in `query.filter` is the caller's filter `And`-composed with
    // the compiled PDP scope, or — as here — the scope alone, which is what the
    // gateway passes on when the caller supplied no `$filter`. A
    // multi-constraint grant then puts an `Or` of tenant-pinned conjunctions at
    // the outermost node. Pushed into the `AND` join without parentheses of its
    // own, `… AND A OR B` binds as `(… AND A) OR B` and answers every row
    // matching the last disjunct, across every tenant the range covers.
    //
    // It pins the other half too, which nothing structural does: that the
    // fragment is pushed at all. Dropped, the read runs unscoped over the whole
    // meter and this assertion is the only thing that notices.
    let query = list_query().with_filter(parse_scope(&format!(
        "(tenant_id eq {SCOPE_TENANT_A} and resource_type eq 'vm') or \
         (tenant_id eq {SCOPE_TENANT_B} and resource_type eq 'vm')"
    )));

    let (sql, binds) = build_list_sql(&list_meter(), list_range(), &query, &[], 25)
        .expect("a disjunctive composed filter must render");

    // Transcribed by hand, not derived from anything the builder produces.
    assert_eq!(
        statement_tail(&sql),
        "r WHERE r.gts_type_id = $1 AND r.window_end >= $2 AND r.window_end < $3 \
         AND (((tenant_id = $4 AND resource_type = $5) \
         OR (tenant_id = $6 AND resource_type = $7))) \
         ORDER BY window_end ASC, id ASC LIMIT 26",
        "every disjunct has to survive the conjunction with the meter and the range"
    );
    assert_eq!(
        binds.len(),
        7,
        "the meter, the two bounds, and four operands"
    );
}

#[test]
fn a_composed_filter_that_cannot_be_translated_is_refused_never_dropped() {
    // `gts_type_id` is a typed SPI parameter, deliberately absent from the
    // filterable schema. Dropping the unrenderable conjunct would leave the
    // meter and the range as the whole `WHERE` — a translation failure turned
    // into an authorization bypass across every tenant in that range.
    let query = list_query().with_filter(parse_scope(
        "gts_type_id eq 'gts.cf.core.uc.usage_record.v1~'",
    ));

    let Err(err) = build_list_sql(&list_meter(), list_range(), &query, &[], 25) else {
        panic!("a filter naming a field off the schema must not render");
    };

    assert!(
        err.contains("gts_type_id"),
        "the refusal names the field it refused. got: {err}"
    );
}

#[test]
fn the_keyset_tuple_and_the_order_by_read_one_order() {
    // Non-canonical in both respects the SPI warns about: `id` leads, and the
    // direction is descending. `render_order_by` and `keyset_predicate` consume
    // `query.order` separately, and if they disagreed on field order or on
    // direction the page would resume from the wrong boundary and report
    // nothing — so one hand-written string pins the tuple, its comparison
    // operator and the `ORDER BY` together.
    let query = ODataQuery::new()
        .with_order(ODataOrderBy(vec![
            OrderKey {
                field: "id".to_owned(),
                dir: SortDir::Desc,
            },
            OrderKey {
                field: "window_end".to_owned(),
                dir: SortDir::Desc,
            },
        ]))
        .with_filter_hash(READ_FINGERPRINT.to_owned())
        .with_cursor(CursorV1 {
            k: vec![
                "00000000-0000-0000-0000-000000000001".to_owned(),
                "2023-11-14T23:13:20Z".to_owned(),
            ],
            o: SortDir::Desc,
            s: "-id,-window_end".to_owned(),
            f: Some(READ_FINGERPRINT.to_owned()),
            d: "fwd".to_owned(),
        });

    let (sql, binds) = build_list_sql(&list_meter(), list_range(), &query, &[], 25)
        .expect("a descending id-led continuation must render");

    // Transcribed by hand.
    assert_eq!(
        statement_tail(&sql),
        "r WHERE r.gts_type_id = $1 AND r.window_end >= $2 AND r.window_end < $3 \
         AND (id, window_end) < ($4, $5) \
         ORDER BY id DESC, window_end DESC LIMIT 26",
        "the tuple's columns, its operator and the ORDER BY all read one order"
    );
    assert_eq!(
        binds.len(),
        5,
        "the meter, the two bounds, and two cursor keys"
    );
}

#[test]
fn the_metadata_side_channel_is_bound_after_the_range() {
    // AND across filters, OR within one filter's values. Asserted here because
    // the side channel shares its bind context with the range and the filter,
    // so a builder that seeded it wrongly would renumber every placeholder
    // after the third.
    let query = list_query();
    let filters = [
        usage_collector_sdk::MetadataFilter::new("region", ["eu-west-1", "eu-west-2"])
            .expect("valid metadata filter"),
    ];

    let (sql, binds) = build_list_sql(&list_meter(), list_range(), &query, &filters, 25)
        .expect("a metadata-filtered page must render");

    assert_eq!(
        statement_tail(&sql),
        "r WHERE r.gts_type_id = $1 AND r.window_end >= $2 AND r.window_end < $3 \
         AND r.metadata ->> $4 IN ($5, $6) \
         ORDER BY window_end ASC, id ASC LIMIT 26"
    );
    assert_eq!(
        binds.len(),
        6,
        "the meter, the two bounds, the key and two values"
    );
}

#[test]
fn the_ledger_page_withholds_no_withdrawn_entry() {
    // The asymmetry with the fold, as a test rather than only a comment. A
    // total that counts a withdrawn entry is a wrong total, so `aggregate`
    // excludes the pair; that is a derived view and this is the ledger itself.
    // The SPI says it for this method in as many words — "a withdrawn pair MUST
    // likewise be returned as persisted here" — and hiding either half destroys
    // the audit trail the append-only model exists to keep
    // (`cpt-cf-usage-collector-adr-append-only-invalidation`).
    let query = list_query();

    let (sql, _) = build_list_sql(&list_meter(), list_range(), &query, &[], 25)
        .expect("the canonical first page must render");
    let tail = statement_tail(&sql);

    assert!(
        !tail.contains("invalidates"),
        "no withdrawal exclusion belongs on this path. got: {tail}"
    );
    assert!(
        !tail.contains("entry_type"),
        "nor an entry-type restriction, which withholds the same rows by \
         another name. got: {tail}"
    );
}

#[test]
fn a_cursor_is_refused_when_the_query_carries_no_fingerprint() {
    // The hole an `Option`-to-`Option` comparison leaves: an absent `cursor.f`
    // and an absent `query.filter_hash` compare *equal*, so the guard passed on
    // exactly the breach it exists to catch and left the gateway to refuse the
    // token a page later. Resolving the live fingerprint first turns it into a
    // refusal here.
    let query = ODataQuery::new()
        .with_order(canonical_order())
        .with_cursor(CursorV1 {
            k: vec![
                "2023-11-14T23:13:20Z".to_owned(),
                "00000000-0000-0000-0000-000000000001".to_owned(),
            ],
            o: SortDir::Asc,
            s: "+window_end,+id".to_owned(),
            f: None,
            d: "fwd".to_owned(),
        });

    let Err(err) = build_list_sql(&list_meter(), list_range(), &query, &[], 25) else {
        panic!("a continuation without a live fingerprint must be refused");
    };

    assert!(
        err.contains("filter_hash"),
        "the refusal names what was missing. got: {err}"
    );
}

#[test]
fn a_cursor_minted_under_a_different_filter_is_refused() {
    // The guard's ordinary case: the caller changed their query between pages,
    // so the boundary the token carries was read under a predicate that no
    // longer applies.
    let query = list_query().with_cursor(CursorV1 {
        k: vec![
            "2023-11-14T23:13:20Z".to_owned(),
            "00000000-0000-0000-0000-000000000001".to_owned(),
        ],
        o: SortDir::Asc,
        s: "+window_end,+id".to_owned(),
        f: Some("sha256:a-fingerprint-of-some-other-query".to_owned()),
        d: "fwd".to_owned(),
    });

    let Err(err) = build_list_sql(&list_meter(), list_range(), &query, &[], 25) else {
        panic!("a cursor minted under a different filter must be refused");
    };

    assert!(err.contains("filter hash"), "unexpected message: {err}");
}

#[test]
fn a_cursor_minted_under_a_different_sort_order_is_refused() {
    // The live query sorts `(window_end, id)`; the cursor was minted under
    // `(id, window_end)`. The keys are individually valid and the arity agrees,
    // so without this guard the old key strings bind against the new columns —
    // silently wrong pagination. The fingerprints agree, so only the sort-order
    // guard can reject this.
    let query = list_query().with_cursor(CursorV1 {
        k: vec![
            "00000000-0000-0000-0000-000000000001".to_owned(),
            "2023-11-14T23:13:20Z".to_owned(),
        ],
        o: SortDir::Asc,
        s: "+id,+window_end".to_owned(),
        f: Some(READ_FINGERPRINT.to_owned()),
        d: "fwd".to_owned(),
    });

    let Err(err) = build_list_sql(&list_meter(), list_range(), &query, &[], 25) else {
        panic!("a cursor minted under a different order must be refused");
    };

    assert!(err.contains("sort order"), "unexpected message: {err}");
}

#[test]
fn a_backward_cursor_is_refused() {
    // The keyset operator is derived from the sort direction, not from
    // `cursor.d`, so a backward cursor would silently page forward and return
    // the wrong page. Its fingerprint and its order both agree with the query,
    // so only the direction guard can reject it.
    let query = list_query().with_cursor(CursorV1 {
        k: vec![
            "2023-11-14T23:13:20Z".to_owned(),
            "00000000-0000-0000-0000-000000000001".to_owned(),
        ],
        o: SortDir::Asc,
        s: "+window_end,+id".to_owned(),
        f: Some(READ_FINGERPRINT.to_owned()),
        d: "bwd".to_owned(),
    });

    let Err(err) = build_list_sql(&list_meter(), list_range(), &query, &[], 25) else {
        panic!("a backward cursor must be refused");
    };

    assert!(err.contains("direction"), "unexpected message: {err}");
}

#[test]
fn an_empty_order_is_refused_rather_than_served_unpaginated() {
    // A gateway breach: the SPI guarantees `query.order` is non-empty on every
    // surface, because it is the keyset the continuation is built from. Served
    // with no `ORDER BY`, the pages would overlap and drop rows against a
    // backend free to return them in any order.
    let query = ODataQuery::new().with_filter_hash(READ_FINGERPRINT.to_owned());

    let Err(err) = build_list_sql(&list_meter(), list_range(), &query, &[], 25) else {
        panic!("an empty order must be refused");
    };

    assert!(err.contains("empty"), "unexpected message: {err}");
}

#[tokio::test]
async fn a_page_that_cannot_be_built_never_reaches_the_pool() {
    // The half the pure tests above cannot see: that `list` propagates the
    // refusal instead of reading without it. The store's pool is lazy and
    // points at nothing, so any path that got as far as acquiring a connection
    // answers `Transient` (a pool timeout). An `Internal` naming the field is
    // therefore proof the read stopped before it touched anything.
    let store = lazy_store();
    let query = list_query().with_filter(parse_scope(
        "gts_type_id eq 'gts.cf.core.uc.usage_record.v1~'",
    ));

    let Err(err) = store.list(list_meter(), list_range(), &query, &[]).await else {
        panic!("an untranslatable filter must not yield a page");
    };

    match err {
        UsageCollectorPluginError::Internal(message) => assert!(
            message.contains("gts_type_id"),
            "the refusal reaches the caller as-is. got: {message}"
        ),
        other => panic!(
            "an untranslatable filter must stop the read before it acquires a \
             connection; reaching the pool would answer Transient. got: {other:?}"
        ),
    }
}

// --- The fold: `build_aggregate_sql` (Task 12) ---

/// The fold's own query slot: no order, no page size, no fingerprint. The fold
/// paginates nothing, so [`list_query`]'s canonical order would be noise here.
fn fold_query() -> ODataQuery {
    ODataQuery::new()
}

/// The metadata key a grouped metadata dimension reads, distinct from the side
/// channel's `region`, so a statement carrying both cannot pass by binding one
/// key where the other belongs.
const GROUPED_METADATA_KEY: &str = "tier";

fn metadata_dimension(key: &str) -> AggregationDimension {
    AggregationDimension::Metadata(MetadataKey::new(key).expect("a valid metadata key"))
}

/// Every dimension the SDK declares, in variant order — an exhaustiveness
/// witness for a test that claims to cover "every dimension": adding a variant
/// fails to compile in [`dimension_presence_guard`], and this array is what a
/// reader checks the claim against.
fn every_dimension() -> Vec<AggregationDimension> {
    let dims = vec![
        AggregationDimension::TenantId,
        AggregationDimension::ResourceId,
        AggregationDimension::ResourceType,
        AggregationDimension::SubjectId,
        AggregationDimension::SubjectType,
        metadata_dimension(GROUPED_METADATA_KEY),
    ];
    // The witness, and nothing else: a new variant fails to compile *here*,
    // next to the array that needs its entry. Without it a new dimension reds
    // `dimension_select_expr`'s own match in another file, nothing points the
    // author at this list, and the tests below go on claiming a coverage they
    // have silently stopped having.
    for dim in &dims {
        match dim {
            AggregationDimension::TenantId
            | AggregationDimension::ResourceId
            | AggregationDimension::ResourceType
            | AggregationDimension::SubjectId
            | AggregationDimension::SubjectType
            | AggregationDimension::Metadata(_) => {}
        }
    }
    dims
}

/// Every fold the SDK declares, in variant order.
fn every_fold() -> Vec<AggregationFold> {
    let folds = vec![
        AggregationFold::Sum,
        AggregationFold::Count,
        AggregationFold::Min,
        AggregationFold::Max,
        AggregationFold::Latest,
    ];
    // Same witness, same reason: a sixth fold would otherwise red
    // `fold_select_expr` in another file and leave the two "every fold" tests
    // below quietly covering five of six.
    for fold in &folds {
        match fold {
            AggregationFold::Sum
            | AggregationFold::Count
            | AggregationFold::Min
            | AggregationFold::Max
            | AggregationFold::Latest => {}
        }
    }
    folds
}

/// One grouped statement carrying every moving part at once: the meter and
/// range, the withdrawal exclusion, a two-conjunct compiled scope, a
/// side-channel filter, and two grouped dimensions — one of them the metadata
/// escape hatch, whose key is bound and whose presence guard reads the same
/// placeholder.
///
/// Two tests read it, because the statement text and the bind vector are two
/// halves of one oracle and the text is the weaker half: it reads identically
/// no matter which value lands in which placeholder.
fn full_fold_statement() -> AggregateStatement {
    let query = fold_query().with_filter(parse_scope(&format!(
        "tenant_id eq {SCOPE_TENANT_A} and resource_type eq 'vm'"
    )));
    let group_by = vec![
        AggregationDimension::SubjectType,
        metadata_dimension(GROUPED_METADATA_KEY),
    ];

    build_aggregate_sql(
        &list_meter(),
        list_range(),
        AggregationFold::Sum,
        &query,
        &[MetadataFilter::new("region", ["eu-west-1"]).expect("a valid side channel filter")],
        &group_by,
    )
    .expect("a well-formed fold must render")
}

#[test]
fn the_fold_assembles_one_statement_from_the_builders_both_read_paths_share() {
    let sql = full_fold_statement().sql;

    // Transcribed by hand, from the shape the SPI describes rather than from
    // anything a builder produces — including the `100001`, which is
    // `MAX_AGGREGATION_BUCKETS + 1` written out. `aggregate_tests.rs` pins the
    // clause against the constant; if this line derived the number from the
    // same constant the two layers would only be agreeing with each other.
    assert_eq!(
        sql,
        "SELECT r.subject_type, r.metadata ->> $8, SUM(r.value)::numeric \
         FROM usage_records r \
         WHERE r.gts_type_id = $1 AND r.window_end >= $2 AND r.window_end < $3 \
         AND r.invalidates IS NULL \
         AND NOT EXISTS (SELECT 1 FROM usage_records w WHERE w.invalidates = r.id) \
         AND ((tenant_id = $4 AND resource_type = $5)) \
         AND r.metadata ->> $6 IN ($7) \
         AND r.subject_type IS NOT NULL \
         AND r.metadata ->> $8 IS NOT NULL \
         GROUP BY 1, 2 LIMIT 100001"
    );
}

#[test]
fn the_fold_binds_the_meter_and_both_range_bounds_ahead_of_everything() {
    // The statement text is the weaker half of the oracle: it reads identically
    // however the values are permuted across its placeholders. A wrong page is
    // noticeable; a wrong total is a billing figure that looks fine.
    //
    // `query_tests.rs` pins what `push_meter_and_range_clauses` binds in
    // isolation. What is this builder's is that it calls that builder first, so
    // the three land at $1..=$3 with the caller's own values behind them.
    let binds = full_fold_statement().binds;

    assert!(
        matches!(&binds[0], SqlBind::Str(s) if s == VCPU_METER),
        "$1 is the meter this fold reads; an empty string here would drop the \
         scoping to one meter without changing a character of the SQL. got: {:?}",
        binds[0]
    );
    assert!(
        matches!(&binds[1], SqlBind::DateTime(t) if t.unix_timestamp() == RANGE_FROM_UNIX),
        "$2 is the range's inclusive lower bound. got: {:?}",
        binds[1]
    );
    assert!(
        matches!(&binds[2], SqlBind::DateTime(t) if t.unix_timestamp() == RANGE_TO_UNIX),
        "$3 is the range's exclusive upper bound; swapped with $2 the range \
         inverts to `window_end >= to AND window_end < from` and the fold \
         selects nothing, ever. got: {:?}",
        binds[2]
    );
}

#[test]
fn the_fold_binds_its_callers_values_behind_the_range() {
    // The other half of the bind oracle: the compiled scope's two operands, the
    // side channel's key and value, and the grouped metadata key — each pinned
    // by value in placeholder order, with the length assertion a completeness
    // check on that set rather than the oracle itself. All five are `text` or
    // `uuid` binds into a statement whose text cannot tell them apart.
    let binds = full_fold_statement().binds;

    assert_eq!(
        binds.len(),
        8,
        "the meter, both bounds, both scope operands, the side channel key and \
         its one value, and the grouped metadata key. got: {binds:?}"
    );
    assert!(
        matches!(&binds[3], SqlBind::Uuid(u) if u.to_string() == SCOPE_TENANT_A),
        "$4 is the tenant the compiled scope pins. got: {:?}",
        binds[3]
    );
    assert!(
        matches!(&binds[4], SqlBind::Str(s) if s == "vm"),
        "$5 is the resource type the compiled scope pins. got: {:?}",
        binds[4]
    );
    assert!(
        matches!(&binds[5], SqlBind::Str(s) if s == "region"),
        "$6 is the side channel's key. got: {:?}",
        binds[5]
    );
    assert!(
        matches!(&binds[6], SqlBind::Str(s) if s == "eu-west-1"),
        "$7 is the side channel's one value; bound where the key belongs, the \
         fold reads a different facet of every row. got: {:?}",
        binds[6]
    );
    assert!(
        matches!(&binds[7], SqlBind::Str(s) if s == GROUPED_METADATA_KEY),
        "$8 is the grouped metadata key, bound once and read twice: by the \
         SELECT expression and by the presence guard. got: {:?}",
        binds[7]
    );
}

#[test]
fn the_folds_from_clause_is_the_one_constant_both_read_paths_read() {
    let AggregateStatement { sql, .. } = build_aggregate_sql(
        &list_meter(),
        list_range(),
        AggregationFold::Sum,
        &fold_query(),
        &[],
        &[],
    )
    .expect("a well-formed fold must render");

    // Called, never spelled. Every fragment in the statement qualifies its
    // columns with the alias this clause declares, and the withdrawal
    // exclusion's `w.invalidates = r.id` is unresolvable without it — the
    // statement this method built before Task 12 was `FROM usage_records`
    // unaliased beside an already-alias-qualified metadata fragment, which
    // `PostgreSQL` refuses outright.
    assert!(
        sql.contains(ledger_from_clause()),
        "the fold's FROM must be the shared clause. got: {sql}"
    );
}

#[test]
fn no_fold_escapes_the_withdrawal_exclusion() {
    // `aggregate_tests.rs` pins that the clause has no per-fold branch; this
    // pins that the statement builder applies it at all, under each fold in
    // turn. Deleting the one `clauses.push` reds all five arms of this loop.
    for fold in every_fold() {
        let sql = build_aggregate_sql(&list_meter(), list_range(), fold, &fold_query(), &[], &[])
            .expect("a well-formed fold must render")
            .sql;

        assert!(
            sql.contains(withdrawal_exclusion_clause()),
            "an invalidation entry, and the record an accepted invalidation \
             names, contribute nothing under {fold:?} as under every other \
             fold. got: {sql}"
        );
        assert!(
            sql.contains(fold_select_expr(fold)),
            "the statement must fold with the fold it was asked for. got: {sql}"
        );
    }
}

#[test]
fn the_fold_carries_no_status_predicate() {
    // The retired time model's `status = 'active'` was applied unconditionally
    // here and lost its only test in Task 2; the Task 3 schema has no such
    // column, so the clause would now be an error rather than a narrowing.
    //
    // Task 10's trap — a "column X is absent" assertion matching the SELECT
    // list instead of the WHERE clause — does not reach this statement, and
    // that is a property of what builds an aggregate SELECT list rather than of
    // this particular call: it is the grouped dimension expressions followed by
    // the fold expression, never `RECORD_COLUMNS`. Neither those six
    // expressions nor the five folds name `status`. So the whole statement is
    // the right thing to grep, and grouping by every dimension at once is the
    // widest text this builder can produce.
    for fold in every_fold() {
        let AggregateStatement { sql, .. } = build_aggregate_sql(
            &list_meter(),
            list_range(),
            fold,
            &fold_query(),
            &[MetadataFilter::new("region", ["eu-west-1"]).expect("a valid filter")],
            &every_dimension(),
        )
        .expect("a well-formed fold must render");

        assert!(
            !sql.contains("status"),
            "`status` is not a column in the Task 3 schema. got: {sql}"
        );
    }
}

#[test]
fn the_no_grouping_case_is_one_bare_aggregate_row() {
    // The single empty-keyed bucket a conforming plugin owes for an empty
    // `group_by` is `PostgreSQL`'s own answer to a bare aggregate — exactly one
    // row, and `Some(0)` rather than `None` for `COUNT` over an empty
    // selection. That only holds while the statement stays a bare aggregate.
    //
    // Two mutations this kills: emitting the `GROUP BY` unconditionally (with
    // no dimensions the ordinal list is empty, which is a syntax error, and
    // with a `1` bolted on it would group by the fold itself), and emitting the
    // bucket `LIMIT` when there is no group cardinality to bound.
    let AggregateStatement { sql, binds, .. } = build_aggregate_sql(
        &list_meter(),
        list_range(),
        AggregationFold::Count,
        &fold_query(),
        &[],
        &[],
    )
    .expect("an ungrouped fold must render");

    assert!(
        !sql.contains("GROUP BY"),
        "grouping by nothing is what a bare aggregate already does. got: {sql}"
    );
    assert!(
        !sql.contains("LIMIT"),
        "there is one row and no group cardinality to bound. got: {sql}"
    );
    assert!(
        sql.starts_with("SELECT COUNT(*)::numeric FROM "),
        "the SELECT list is the fold alone. got: {sql}"
    );
    // The ungrouped statement binds the meter and the two bounds and nothing
    // else, so a stray dimension bind would show up here as a fourth value.
    assert_eq!(
        binds.len(),
        3,
        "the meter and the two bounds. got: {binds:?}"
    );
    assert!(
        matches!(&binds[0], SqlBind::Str(s) if s == VCPU_METER),
        "$1 is the meter. got: {:?}",
        binds[0]
    );
    assert!(
        matches!(&binds[1], SqlBind::DateTime(t) if t.unix_timestamp() == RANGE_FROM_UNIX),
        "$2 is the inclusive lower bound. got: {:?}",
        binds[1]
    );
    assert!(
        matches!(&binds[2], SqlBind::DateTime(t) if t.unix_timestamp() == RANGE_TO_UNIX),
        "$3 is the exclusive upper bound. got: {:?}",
        binds[2]
    );
}

#[test]
fn grouping_numbers_its_ordinals_from_one_and_bounds_the_bucket_count() {
    // `GROUP BY` by ordinal so a bound metadata expression is written once:
    // repeating the expression would push its key a second time and renumber
    // every placeholder after it. Ordinals are 1-based — a 0-based list is a
    // `PostgreSQL` error rather than a silently wrong grouping, but only for
    // the first ordinal, so the three-dimension case is what pins the shape.
    let AggregateStatement { sql, binds, .. } = build_aggregate_sql(
        &list_meter(),
        list_range(),
        AggregationFold::Max,
        &fold_query(),
        &[],
        &[
            AggregationDimension::TenantId,
            AggregationDimension::ResourceId,
            metadata_dimension(GROUPED_METADATA_KEY),
        ],
    )
    .expect("a grouped fold must render");

    assert!(
        sql.ends_with(" GROUP BY 1, 2, 3 LIMIT 100001"),
        "three dimensions, then the cap plus one. got: {sql}"
    );
    assert!(
        sql.starts_with("SELECT r.tenant_id::text, r.resource_id, r.metadata ->> $4, MAX(r.value)::numeric FROM "),
        "the SELECT list is the dimensions in `group_by` order, then the fold. \
         got: {sql}"
    );
    assert_eq!(
        binds.len(),
        4,
        "the meter, both bounds, and the grouped metadata key once. got: {binds:?}"
    );
    assert!(
        matches!(&binds[3], SqlBind::Str(s) if s == GROUPED_METADATA_KEY),
        "$4 is the grouped metadata key. got: {:?}",
        binds[3]
    );
}

#[test]
fn the_builder_reports_the_dimension_count_its_select_list_was_built_from() {
    // `dim_count` travels with the statement so the decoder reads exactly as
    // many key columns as the SELECT list emits, rather than deriving the
    // number again from `group_by`. That buys nothing unless the reported count
    // and the statement agree: they are two separate outputs of one builder,
    // and pinning the count to a constant leaves every SQL oracle in this file
    // green.
    //
    // Transcribed by hand: k dimensions means k leading key columns, `GROUP BY
    // 1..=k`, and `dim_count == k`. The empty case carries no `GROUP BY` at
    // all, which is why the third assertion is a two-way one rather than a
    // suffix match `""` would satisfy for free.
    for (group_by, count, tail) in [
        (vec![], 0usize, ""),
        (
            vec![AggregationDimension::TenantId],
            1,
            " GROUP BY 1 LIMIT 100001",
        ),
        (
            vec![
                AggregationDimension::TenantId,
                AggregationDimension::ResourceId,
                metadata_dimension(GROUPED_METADATA_KEY),
            ],
            3,
            " GROUP BY 1, 2, 3 LIMIT 100001",
        ),
    ] {
        let statement = build_aggregate_sql(
            &list_meter(),
            list_range(),
            AggregationFold::Sum,
            &fold_query(),
            &[],
            &group_by,
        )
        .expect("a well-formed fold must render");

        assert_eq!(
            statement.dim_count, count,
            "the statement was built from {count} dimensions and must say so. \
             got: {}",
            statement.sql
        );
        assert!(
            statement.sql.ends_with(tail),
            "the GROUP BY must carry one ordinal per dimension. got: {}",
            statement.sql
        );
        assert_eq!(
            statement.sql.contains("GROUP BY"),
            count > 0,
            "grouping by nothing is what a bare aggregate already does. got: {}",
            statement.sql
        );
    }
}

#[test]
fn a_nullable_dimension_drops_the_rows_that_do_not_carry_it() {
    // The absent-dimension rule, uniform across all three dimensions that can
    // be `NULL`: drop the row rather than fold it into a `NULL` bucket. What
    // says so is the SDK itself — `models.rs:1587-1592` for the two subject
    // dimensions, and `contract/reference.rs:801` for a metadata key, which the
    // reference backend reads as an `Option` and drops the row on. Not
    // `DIVERGENCES.md` §G, which still records the question as open and the
    // ruling as a spec owner's to make; that is this port's Task 18
    // (`DIVERGENCES.md` §G, resolved by Task 18).
    //
    // Before Task 12 the two subject dimensions were guarded and the metadata
    // one was not, so five of six dimensions obeyed the rule and the sixth did
    // so by accident of the SDK's own docs.
    //
    // The guard is spelled against the grouping expression itself, so it is the
    // exact negation of "this expression yields NULL". `?` would not be: for a
    // key present with JSON `null` it is true while `->>` is `NULL`, and it is
    // the `NULL` that decides the bucket.
    // Each guard is transcribed by hand, placeholder included: with the meter
    // and the two bounds bound first, a lone grouped metadata key is `$4`.
    for (dim, guard) in [
        (AggregationDimension::SubjectId, "r.subject_id IS NOT NULL"),
        (
            AggregationDimension::SubjectType,
            "r.subject_type IS NOT NULL",
        ),
        (
            metadata_dimension(GROUPED_METADATA_KEY),
            "r.metadata ->> $4 IS NOT NULL",
        ),
    ] {
        let AggregateStatement { sql, .. } = build_aggregate_sql(
            &list_meter(),
            list_range(),
            AggregationFold::Sum,
            &fold_query(),
            &[],
            std::slice::from_ref(&dim),
        )
        .expect("a grouped fold must render");

        assert!(
            sql.contains(guard),
            "{dim:?} can be NULL, so a row missing it must be dropped rather \
             than bucketed under NULL. got: {sql}"
        );
        // The other half, and the one that makes the guard unable to drift:
        // strip ` IS NOT NULL` off the guard and what is left must be the
        // grouping expression itself, the same bound placeholder and all.
        let grouping_expr = guard
            .strip_suffix(" IS NOT NULL")
            .unwrap_or_else(|| panic!("the transcribed guard must end in the predicate"));
        assert!(
            sql.starts_with(&format!("SELECT {grouping_expr}, ")),
            "the guard must negate the grouping expression it guards, not a \
             second spelling of it. got: {sql}"
        );
        assert!(
            !sql.contains(" ? "),
            "the guard is `->> IS NOT NULL`, never the containment operator: \
             they disagree on a key present with JSON null, and a bare `?` \
             collides with placeholder syntax in some drivers. got: {sql}"
        );
    }
}

#[test]
fn the_three_never_null_dimensions_get_no_dead_guard() {
    // `tenant_id`, `resource_id` and `resource_type` are NOT NULL in the
    // schema, so a presence guard on them is dead SQL that the planner still
    // has to carry. The mutation this kills is the tempting uniform one: guard
    // every dimension because three of them need it.
    let AggregateStatement { sql, .. } = build_aggregate_sql(
        &list_meter(),
        list_range(),
        AggregationFold::Sum,
        &fold_query(),
        &[],
        &[
            AggregationDimension::TenantId,
            AggregationDimension::ResourceId,
            AggregationDimension::ResourceType,
        ],
    )
    .expect("a grouped fold must render");

    assert!(
        !sql.contains("IS NOT NULL"),
        "none of these three columns can be NULL, so none needs a guard. \
         got: {sql}"
    );
}

#[test]
fn the_folds_disjunctive_scope_survives_the_conjunction_with_the_range() {
    // The shape a multi-constraint grant compiles to. Pushed into the
    // `join(" AND ")` unparenthesized — which is what the inline
    // `convert_expr_to_filter_node` + `translate_record_filter` pair this
    // builder used to carry produced — `… AND A OR B` binds as
    // `(… AND A) OR B` and folds every row matching the last disjunct,
    // whatever meter or range was asked for. Nothing at this layer can tell
    // how many constraints the PDP returned.
    let query = fold_query().with_filter(parse_scope(&format!(
        "(tenant_id eq {SCOPE_TENANT_A} and resource_type eq 'vm') or \
         (tenant_id eq {SCOPE_TENANT_B} and resource_type eq 'vm')"
    )));

    let AggregateStatement { sql, binds, .. } = build_aggregate_sql(
        &list_meter(),
        list_range(),
        AggregationFold::Sum,
        &query,
        &[],
        &[],
    )
    .expect("a compiled scope must render");

    // Transcribed by hand.
    assert!(
        sql.contains(
            "AND (((tenant_id = $4 AND resource_type = $5) \
             OR (tenant_id = $6 AND resource_type = $7)))"
        ),
        "the whole disjunction must sit inside the conjunction. got: {sql}"
    );
    assert_eq!(
        binds.len(),
        7,
        "the meter, both bounds, four scope operands"
    );
    assert!(
        matches!(&binds[3], SqlBind::Uuid(u) if u.to_string() == SCOPE_TENANT_A),
        "$4 is the first admitted tenant. got: {:?}",
        binds[3]
    );
    assert!(
        matches!(&binds[5], SqlBind::Uuid(u) if u.to_string() == SCOPE_TENANT_B),
        "$6 is the second admitted tenant; both grants must be bound, or the \
         scope silently narrows to one. got: {:?}",
        binds[5]
    );
}

#[tokio::test]
async fn a_fold_that_cannot_be_built_never_reaches_the_pool() {
    // `aggregate` claims the statement is built before a connection is
    // acquired. `the_ungrouped_fold_still_reaches_the_pool` does not pin that:
    // `Transient` is what a pool timeout answers under *either* order, so
    // swapping the two statements leaves it green. This is the half that pins
    // the order, and it is the fold analogue of
    // `a_page_that_cannot_be_built_never_reaches_the_pool`.
    //
    // `gts_type_id` is a real column but not a filterable field, so
    // `record_column` refuses it and `build_aggregate_sql` errs. Against a lazy
    // pool at a dead DSN, an `Internal` naming the field is proof the fold
    // stopped before it touched anything; acquiring first would answer
    // `Transient` and lose the refusal.
    let store = lazy_store();
    let query = fold_query().with_filter(parse_scope(
        "gts_type_id eq 'gts.cf.core.uc.usage_record.v1~'",
    ));

    let Err(err) = store
        .aggregate(
            list_meter(),
            list_range(),
            AggregationFold::Sum,
            &query,
            &[],
            &[],
        )
        .await
    else {
        panic!("an untranslatable filter must not yield an aggregate");
    };

    match err {
        UsageCollectorPluginError::Internal(message) => assert!(
            message.contains("gts_type_id"),
            "the refusal reaches the caller as-is. got: {message}"
        ),
        other => panic!(
            "an untranslatable filter must stop the fold before it acquires a \
             connection; reaching the pool would answer Transient. got: {other:?}"
        ),
    }
}

#[tokio::test]
async fn the_ungrouped_fold_still_reaches_the_pool() {
    // The half every pure test above is blind to: that the single empty-keyed
    // bucket is `PostgreSQL`'s answer to a bare aggregate rather than something
    // this method shortcuts to. The mutation is a `return` whose whole purpose
    // is to skip the query, so its natural home is above the acquire:
    //
    //     if group_by.is_empty() {
    //         return Ok(AggregationResult { buckets: Vec::new() });
    //     }
    //
    // The store's pool is lazy and points at nothing, so a path that got as far
    // as acquiring a connection answers `Transient` (a pool timeout) and one
    // that short-circuited answers `Ok`. The same discriminator
    // `a_page_that_cannot_be_built_never_reaches_the_pool` uses, read the other
    // way round: there an `Internal` proves the read stopped early, here a
    // `Transient` proves it did not.
    //
    // It also pins `aggregate`'s stated order — the statement is built before a
    // connection is acquired — for the ungrouped case, which is where an
    // empty-result guard would otherwise sit unobserved.
    let store = lazy_store();

    let Err(err) = store
        .aggregate(
            list_meter(),
            list_range(),
            AggregationFold::Count,
            &fold_query(),
            &[],
            &[],
        )
        .await
    else {
        panic!(
            "an empty group_by must still be answered by a query: COUNT over an \
             empty selection is 0 rather than absent, and that is the backend's \
             answer, not this method's"
        );
    };

    assert!(
        matches!(err, UsageCollectorPluginError::Transient { .. }),
        "the ungrouped fold must reach the pool; anything else means it \
         answered without asking. got: {err:?}"
    );
}

#[test]
fn the_fold_reads_neither_the_cursor_nor_the_fingerprint_slot() {
    // "An aggregate implementation MUST NOT read the slot": this call
    // paginates nothing, mints no cursor, and the gateway assigns it no
    // fingerprint. The realistic mutation is not a deliberate read — it is the
    // cursor block copied across from `build_list_sql`, which resolves
    // `query.filter_hash` through `require_filter_hash` and errors when it is
    // absent, then pushes a keyset predicate. Under that copy the second and
    // third statements below stop matching the first.
    //
    // What this pins is that neither slot reaches the statement or its binds. A
    // read whose value is then discarded leaves no trace in either, and so is
    // not observable from here.
    let base = fold_query();
    let with_cursor = fold_query().with_cursor(CursorV1 {
        k: vec!["2023-11-14T23:13:20Z".to_owned()],
        o: SortDir::Asc,
        s: "+window_end".to_owned(),
        f: None,
        d: "fwd".to_owned(),
    });
    let with_fingerprint = fold_query().with_filter_hash(READ_FINGERPRINT.to_owned());

    let render = |query: &ODataQuery| {
        let AggregateStatement { sql, binds, .. } = build_aggregate_sql(
            &list_meter(),
            list_range(),
            AggregationFold::Sum,
            query,
            &[],
            &[AggregationDimension::TenantId],
        )
        .expect("a well-formed fold must render");
        (sql, format!("{binds:?}"))
    };

    let expected = render(&base);
    assert_eq!(
        render(&with_cursor),
        expected,
        "a cursor in the slot must change neither the statement nor its binds"
    );
    assert_eq!(
        render(&with_fingerprint),
        expected,
        "a fingerprint in the slot must change neither the statement nor its \
         binds"
    );
}
