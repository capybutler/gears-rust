use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use sqlx::postgres::PgPoolOptions;
use toolkit_odata::{CursorV1, ODataOrderBy, ODataQuery, OrderKey, SortDir};
use usage_collector_sdk::{UsageCollectorPluginError, UsageTypeGtsId};

use super::{
    ConflictRead, DedupKey, MAX_BATCH_ATTEMPTS, PgRecordStore, batch_retry_backoff,
    canonical_equal, dedup_key, is_retryable_batch_error, plan_batch, with_retry,
};
use crate::domain::ports::RecordStore;
use crate::infra::metrics::Metrics;
use crate::infra::storage::entity::UsageRecordRow;

const VCPU_GTS: &str = "gts.cf.core.uc.usage_record.v1~cf.compute._.vcpu_hours.v1";

/// A store over a lazy pool: no connection is opened, so the pre-DB validation
/// paths under test return before any query is issued. The tiny acquire timeout
/// keeps an accidental DB touch from hanging the test.
fn lazy_store() -> PgRecordStore {
    let pool = PgPoolOptions::new()
        .acquire_timeout(Duration::from_millis(50))
        .connect_lazy("postgres://user:pass@localhost/db")
        .expect("a syntactically valid DSN yields a lazy pool without connecting");
    PgRecordStore::new(pool.clone(), Arc::new(Metrics::new(pool)))
}

#[tokio::test]
async fn list_rejects_cursor_whose_sort_order_differs_from_query() {
    let store = lazy_store();
    let gts_id = UsageTypeGtsId::new(VCPU_GTS).expect("valid gts id");

    // The live query sorts (created_at asc, uuid asc); the cursor was minted
    // under a different order (uuid first). The keys are individually valid, so
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
                field: "uuid".to_owned(),
                dir: SortDir::Asc,
            },
        ]))
        .with_cursor(CursorV1 {
            k: vec![
                "2024-01-01T00:00:00Z".to_owned(),
                "00000000-0000-0000-0000-000000000001".to_owned(),
            ],
            o: SortDir::Asc,
            s: "+uuid,+created_at".to_owned(),
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
    let gts_id = UsageTypeGtsId::new(VCPU_GTS).expect("valid gts id");

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
                field: "uuid".to_owned(),
                dir: SortDir::Asc,
            },
        ]))
        .with_cursor(CursorV1 {
            k: vec![
                "2024-01-01T00:00:00Z".to_owned(),
                "00000000-0000-0000-0000-000000000001".to_owned(),
            ],
            o: SortDir::Asc,
            s: "+created_at,+uuid".to_owned(),
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

/// Minimal in-memory `UsageRecord` for pure (no-DB) unit tests.
fn unit_record(tenant: uuid::Uuid, idem: &str, seq: u128) -> usage_collector_sdk::UsageRecord {
    usage_collector_sdk::UsageRecord {
        uuid: uuid::Uuid::from_u128(seq),
        gts_id: usage_collector_sdk::UsageTypeGtsId::new(VCPU_GTS).expect("valid gts id"),
        tenant_id: tenant,
        resource_ref: usage_collector_sdk::ResourceRef::new("res-1", "compute.vm")
            .expect("valid resource_ref"),
        subject_ref: None,
        metadata: std::collections::BTreeMap::new(),
        value: rust_decimal::Decimal::new(1, 0),
        idempotency_key: usage_collector_sdk::IdempotencyKey::new(idem).expect("valid idem key"),
        corrects_id: None,
        status: usage_collector_sdk::UsageRecordStatus::Active,
        created_at: time::OffsetDateTime::from_unix_timestamp(1_700_000_000).expect("valid ts"),
    }
}

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

    let kb_rep = plan
        .reps
        .iter()
        .find(|r| r.idempotency_key.as_str() == "kb")
        .expect("kb rep present");
    assert_eq!(
        kb_rep.uuid,
        uuid::Uuid::from_u128(10),
        "kb rep is the first occurrence"
    );
}

/// A `UsageRecordRow` whose canonical fields equal `record`, carrying the
/// given stored `metadata` jsonb verbatim. Used to exercise `canonical_equal`'s
/// absorb/conflict/decode-failure paths without a database.
fn row_matching(
    record: &usage_collector_sdk::UsageRecord,
    metadata: serde_json::Value,
) -> UsageRecordRow {
    UsageRecordRow {
        uuid: record.uuid,
        tenant_id: record.tenant_id,
        gts_id: VCPU_GTS.to_owned(),
        value: record.value,
        created_at: record.created_at,
        resource_id: record.resource_ref.resource_id().to_owned(),
        resource_type: record.resource_ref.resource_type().to_owned(),
        subject_id: None,
        subject_type: None,
        idempotency_key: record.idempotency_key.as_str().to_owned(),
        corrects_id: record.corrects_id,
        status: "active".to_owned(),
        metadata,
        ingested_at: record.created_at,
    }
}

#[test]
fn canonical_equal_surfaces_corrupt_stored_metadata_as_internal() {
    let tenant = uuid::Uuid::from_u128(7);
    let record = unit_record(tenant, "k", 700);
    // Stored metadata that cannot decode back to the typed map (a JSON string,
    // not an object). Every other canonical field matches, so the old `.ok()`
    // swallow turned this stored-data corruption into a silent `IdempotencyConflict`.
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
fn dedup_key_ignores_created_at_and_value() {
    let tenant = uuid::Uuid::from_u128(2);
    let a = unit_record(tenant, "same", 100);
    let mut b = unit_record(tenant, "same", 200);
    b.value = rust_decimal::Decimal::new(999, 0);
    b.created_at = a.created_at + time::Duration::seconds(5);
    assert_eq!(
        dedup_key(&a),
        dedup_key(&b),
        "key is (tenant, gts, idem) only"
    );
}

// --- `resolve_batch` invariant-break / defensive arms (DB-free) ---
//
// `resolve_batch` is a pure function of its (`won`, `inserted`, `conflict`)
// maps, so these arms are exercised over a lazy pool that is never touched. The
// branches below fire only on a broken DB invariant (a won slot with no
// inserted record) or a cleanup race (the dedup pointer vanished between claim
// and read) — unreachable from the happy-path integration tests, hence easy to
// break silently. Each is pinned here against a hand-built map.

#[tokio::test]
async fn resolve_batch_winner_with_no_inserted_record_is_internal() {
    let store = lazy_store();
    let tenant = uuid::Uuid::from_u128(0xB1);
    let records = vec![unit_record(tenant, "win", 0x10)];
    let plan = plan_batch(&records);
    let key = dedup_key(&records[0]);

    // We claimed (won) the slot, but the multi-row insert returned no row for it
    // — a concurrent-insert invariant break, not a normal outcome.
    let won = HashSet::from([key]);
    let inserted: HashMap<DedupKey, UsageRecordRow> = HashMap::new();
    let conflict: HashMap<DedupKey, ConflictRead> = HashMap::new();

    let results = store.resolve_batch(records, &plan, &won, &inserted, &conflict);

    assert_eq!(results.len(), 1, "one result per input row");
    match results.into_iter().next().expect("one result") {
        Err(UsageCollectorPluginError::Internal(msg)) => assert!(
            msg.contains("no inserted record was returned"),
            "unexpected message: {msg}"
        ),
        other => panic!("a won slot with no inserted record must be Internal, got {other:?}"),
    }
}

#[tokio::test]
async fn resolve_batch_intra_batch_dup_of_won_key_with_no_record_is_internal() {
    let store = lazy_store();
    let tenant = uuid::Uuid::from_u128(0xB2);
    // Two rows share one dedup key: idx 0 is the winner, idx 1 the in-batch dup.
    let records = vec![
        unit_record(tenant, "dup", 0x20),
        unit_record(tenant, "dup", 0x21),
    ];
    let plan = plan_batch(&records);
    let key = dedup_key(&records[0]);

    // Won the slot, but no inserted record came back for it.
    let won = HashSet::from([key]);
    let inserted: HashMap<DedupKey, UsageRecordRow> = HashMap::new();
    let conflict: HashMap<DedupKey, ConflictRead> = HashMap::new();

    let results = store.resolve_batch(records, &plan, &won, &inserted, &conflict);

    assert_eq!(results.len(), 2, "one result per input row");
    // idx 0 hits the winner-missing Internal arm...
    assert!(
        matches!(results[0], Err(UsageCollectorPluginError::Internal(_))),
        "winner with no inserted record is Internal: {:?}",
        results[0]
    );
    // ...idx 1 is the in-batch duplicate of that won key — the distinct second
    // Internal arm, identified by its message.
    match &results[1] {
        Err(UsageCollectorPluginError::Internal(msg)) => assert!(
            msg.contains("intra-batch duplicate"),
            "unexpected message: {msg}"
        ),
        other => {
            panic!("intra-batch dup of a won key with no record must be Internal, got {other:?}")
        }
    }
}

#[tokio::test]
async fn resolve_batch_disappeared_pointer_is_transient() {
    let store = lazy_store();
    let tenant = uuid::Uuid::from_u128(0xB3);
    let records = vec![unit_record(tenant, "gone", 0x30)];
    let plan = plan_batch(&records);
    let key = dedup_key(&records[0]);

    // Not won; the conflict read found the dedup pointer gone (cleanup raced
    // between claim and read).
    let won: HashSet<DedupKey> = HashSet::new();
    let inserted: HashMap<DedupKey, UsageRecordRow> = HashMap::new();
    let conflict = HashMap::from([(key, ConflictRead::Disappeared)]);

    let results = store.resolve_batch(records, &plan, &won, &inserted, &conflict);

    assert_eq!(results.len(), 1);
    assert!(
        matches!(results[0], Err(UsageCollectorPluginError::Transient { .. })),
        "a disappeared dedup pointer must be retryable Transient: {:?}",
        results[0]
    );
}

#[tokio::test]
async fn resolve_batch_missing_conflict_entry_falls_through_to_transient() {
    let store = lazy_store();
    let tenant = uuid::Uuid::from_u128(0xB4);
    let records = vec![unit_record(tenant, "absent", 0x40)];
    let plan = plan_batch(&records);

    // Not won, and the conflict map has no entry for the key at all. The
    // defensive `None` fallthrough must still be a retryable Transient — never a
    // silent success and never a panic.
    let won: HashSet<DedupKey> = HashSet::new();
    let inserted: HashMap<DedupKey, UsageRecordRow> = HashMap::new();
    let conflict: HashMap<DedupKey, ConflictRead> = HashMap::new();

    let results = store.resolve_batch(records, &plan, &won, &inserted, &conflict);

    assert_eq!(results.len(), 1);
    assert!(
        matches!(results[0], Err(UsageCollectorPluginError::Transient { .. })),
        "a key absent from the conflict map must fall through to Transient: {:?}",
        results[0]
    );
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
    let result: Result<u32, u32> = with_retry(
        3,
        no_backoff,
        |_err| true, // would retry, but the operation succeeds first try
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
}

#[tokio::test]
async fn with_retry_retries_a_retryable_error_then_returns_the_eventual_ok() {
    let calls = AtomicU32::new(0);
    // Fail (retryably) twice, then succeed on the third attempt.
    let result: Result<u32, u32> = with_retry(
        5,
        no_backoff,
        |_err| true,
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
async fn with_retry_stops_at_max_attempts_and_returns_the_last_error() {
    let calls = AtomicU32::new(0);
    let result: Result<u32, u32> = with_retry(
        3,
        no_backoff,
        |_err| true, // always retryable, but the cap bounds it
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
    let result: Result<u32, u32> = with_retry(
        3,
        no_backoff,
        |_err| false, // predicate rejects every error → no retry
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
            existing_uuid: uuid::Uuid::from_u128(1),
        }),
        "IdempotencyConflict must not be retried"
    );
}

// --- The `create_batch` backoff schedule ---

#[test]
fn batch_retry_backoff_is_short_and_non_decreasing() {
    // A deadlock victim can retry almost immediately, so the schedule stays
    // small and never shrinks between attempts.
    let mut prev = Duration::ZERO;
    for attempt in 1..MAX_BATCH_ATTEMPTS {
        let d = batch_retry_backoff(attempt);
        assert!(d >= prev, "backoff must not decrease (attempt {attempt})");
        assert!(
            d <= Duration::from_millis(100),
            "backoff stays small for a deadlock victim (attempt {attempt}): {d:?}"
        );
        prev = d;
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
    let store = PgRecordStore::new(pool, metrics);

    // Every operation routes through timed_acquire; call it directly.
    let result = store.timed_acquire().await;
    assert!(result.is_err(), "acquire against a dead port must fail");

    provider.force_flush().expect("flush in-memory metrics");

    // Read the last value of the `uc.timescaledb.ready` gauge.
    let ready = {
        let metrics = exporter.get_finished_metrics().expect("collected metrics");
        let mut found = None;
        for rm in &metrics {
            for sm in rm.scope_metrics() {
                for m in sm.metrics() {
                    if m.name() == "uc.timescaledb.ready"
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
