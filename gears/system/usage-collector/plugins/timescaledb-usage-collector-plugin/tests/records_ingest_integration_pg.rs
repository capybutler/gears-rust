#![cfg(feature = "postgres")]
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]
//! `PgRecordStore` ingest against a live `TimescaleDB`. Requires Docker.
//!
//! The behavioural home for the write path: dedup (absorb / conflict), the
//! transaction id every entry is stamped with, at most one invalidation as a
//! dedup outcome (a later call, one batch, two concurrent calls), and per-row
//! batch outcomes aligned with input order.
//!
//! One thing here exists nowhere else in the crate:
//!
//! * **The `.bind()` sequence.** `InsertColumns`' field order reaches the DDL
//!   only through the binds in `record_store.rs`, and nothing in-process
//!   observes that sequence — swap two binds of the same SQL type and
//!   `PostgreSQL` accepts the row, `InsertColumns::build`'s test still passes,
//!   and `migration_probe` says nothing, because every constant it checks is
//!   still correct.
//!
//!   Measured, by swapping `resource_id` and `resource_type` in each of the two
//!   bind sequences and running the whole `--features postgres` suite:
//!
//!   * **Batch path** (`insert_records_on_conflict`): the crate's whole `--lib`
//!     suite stays green, and so does `contract_conformance_pg`. Three tests red, all in
//!     this file, and two of them only *indirectly* — the absorb path compares
//!     the stored attribution for canonical equality, so a transposed write
//!     turns an absorb into a conflict. Narrow that compared field set and
//!     those two stop noticing.
//!     `a_row_written_through_the_batch_insert_reads_back_column_for_column`
//!     is the one that asks the question directly.
//!   * **Single-row path** (`create_inner`): `contract_conformance_pg` reds as
//!     well, because the DESIGN section 3.3 checks round-trip a resource
//!     reference through `create_usage_record`. That half of the hole was
//!     already covered; only the batch half was not.

mod common;

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration as StdDuration;

use opentelemetry::metrics::MeterProvider as _;
use opentelemetry_sdk::metrics::data::{AggregatedMetrics, MetricData};
use opentelemetry_sdk::metrics::{InMemoryMetricExporter, PeriodicReader, SdkMeterProvider};
use rust_decimal::Decimal;
use serde_json::Value as JsonValue;
use time::{Duration, OffsetDateTime};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use usage_collector_sdk::{
    EntryType, IdempotencyKey, Invalidation, MetadataKey, ReasonCode, RecordOrigin, ResourceRef,
    SubjectRef, UsageCollectorPluginError, UsageRecord,
};

use timescaledb_usage_collector_plugin::domain::ports::RecordStore;
use timescaledb_usage_collector_plugin::infra::metrics::Metrics;
use timescaledb_usage_collector_plugin::infra::storage::record_store::PgRecordStore;

/// A container plus a store over it.
async fn setup() -> (common::TsHarness, PgRecordStore) {
    let h = common::bring_up()
        .await
        .expect("timescaledb container (Docker required)");
    let store = common::record_store(&h.pool);
    (h, store)
}

/// A container plus a store whose metric inventory writes to a **local**
/// in-memory exporter.
///
/// Local rather than the process-global provider, for the reason
/// `metrics_tests` gives: `Metrics::with_meter` takes the meter explicitly, so
/// a recording assertion never depends on global state another test binary is
/// also writing to.
async fn setup_metered() -> (
    common::TsHarness,
    PgRecordStore,
    SdkMeterProvider,
    InMemoryMetricExporter,
) {
    let h = common::bring_up()
        .await
        .expect("timescaledb container (Docker required)");
    let exporter = InMemoryMetricExporter::default();
    let provider = SdkMeterProvider::builder()
        .with_reader(PeriodicReader::builder(exporter.clone()).build())
        .build();
    let metrics = Arc::new(Metrics::with_meter(
        &provider.meter("uc.timescaledb"),
        h.pool.clone(),
    ));
    let store = PgRecordStore::new(h.pool.clone(), metrics, CancellationToken::new());
    (h, store, provider, exporter)
}

/// The counters this file asserts on, positively or negatively.
///
/// Named in one place so [`the_asserted_counter_names_are_all_declared`] can
/// check every one of them against the crate's own inventory.
///
/// **It checks the names listed here, not the names actually passed to
/// [`counter_sum`]**, so a new call site naming an instrument that was never
/// added to this list is still invisible. Closing that would need the call
/// sites to go through a wrapper taking a checked name, which buys less than
/// it costs at four assertions; add the name here when you add the call.
const ASSERTED_COUNTERS: &[&str] = &[
    "uc_timescaledb_dedup_absorbed_total",
    "uc_timescaledb_idempotency_conflicts_total",
    "uc_timescaledb_invalidations_total",
    "uc_timescaledb_batch_retries_total",
];

/// Every counter this file names must be one the crate actually declares.
///
/// [`counter_sum`] answers **0** for an instrument that was never recorded and
/// **0** for one that does not exist, and the negative assertions in this file
/// depend on the first meaning. A rename would turn every one of them into a
/// tautology while leaving them green, so the ambiguity is resolved here
/// instead: this is the single place that fails on a rename, and it fails
/// naming the instrument.
// `#[tokio::test]` and not `#[test]`: `connect_lazy` opens no connection but
// does spawn the pool's background maintenance task, which needs a runtime.
// No Docker, no container - this is the one test in the file that touches
// neither.
#[tokio::test]
async fn the_asserted_counter_names_are_all_declared() {
    let pool = sqlx::postgres::PgPoolOptions::new()
        .connect_lazy("postgres://user:pass@localhost/db")
        .expect("a syntactically valid DSN yields a lazy pool without connecting");
    let declared = Metrics::new(pool).declared_instrument_names();
    for name in ASSERTED_COUNTERS {
        assert!(
            declared.contains(name),
            "`{name}` is asserted on in this file but is not among the crate's declared \
             instruments, so every assertion naming it reads 0 whatever the code does. \
             Declared: {declared:?}"
        );
    }
}

/// How many deadlock victims `PostgreSQL` has aborted in this database.
///
/// The deadlock detector is the only writer of this counter, which is why it can
/// answer a question `uc_timescaledb_batch_retries_total` cannot: that counter
/// records that a batch was re-run, not why. Read as a before/after pair rather
/// than compared against zero, because a harness that ever deadlocks outside the
/// section under test would otherwise be charged to it.
async fn deadlock_count(pool: &sqlx::PgPool) -> i64 {
    sqlx::query_scalar("SELECT deadlocks FROM pg_stat_database WHERE datname = current_database()")
        .fetch_one(pool)
        .await
        .expect("pg_stat_database carries a row for the current database")
}

/// Total of the `u64` counter data points named `name`.
///
/// **0 for an instrument that exists and was never recorded, and 0 for one that
/// does not exist.** That ambiguity is closed by
/// [`the_asserted_counter_names_are_all_declared`] rather than here, because
/// an OpenTelemetry counter with no recorded value need not be exported at all,
/// so "absent" is the *normal* reading of a legitimate zero and cannot be made
/// an error at this seam.
fn counter_sum(exporter: &InMemoryMetricExporter, name: &str) -> u64 {
    let metrics = exporter.get_finished_metrics().expect("exported metrics");
    for resource_metrics in &metrics {
        for scope_metrics in resource_metrics.scope_metrics() {
            for metric in scope_metrics.metrics() {
                if metric.name() == name
                    && let AggregatedMetrics::U64(MetricData::Sum(sum)) = metric.data()
                {
                    return sum
                        .data_points()
                        .map(opentelemetry_sdk::metrics::data::SumDataPoint::value)
                        .sum();
                }
            }
        }
    }
    0
}

/// Every insertable column of one ledger row, decoded by name.
///
/// Module scope rather than inside the test that reads it, because an item
/// after a statement is confusing and clippy says so; the type is only used
/// there.
#[derive(sqlx::FromRow)]
struct Raw {
    id: Uuid,
    tenant_id: Uuid,
    gts_type_id: String,
    quantity: Decimal,
    window_start: OffsetDateTime,
    window_end: OffsetDateTime,
    resource_id: String,
    resource_type: String,
    subject_id: Option<String>,
    subject_type: Option<String>,
    idempotency_key: String,
    invalidates: Option<Uuid>,
    reason_code: Option<String>,
    origin: String,
    metadata: JsonValue,
    entry_type: String,
}

/// The `SELECT` that fills a [`Raw`], naming every column explicitly and
/// reading each back by name.
///
/// `entry_type` is cast because `usage_entry_type` is a `PostgreSQL` enum and
/// `sqlx` will not decode one into a `String`; the production read list casts
/// it for the same reason.
const RAW_SELECT_SQL: &str = "SELECT id, tenant_id, gts_type_id, quantity, window_start, window_end, resource_id, \
     resource_type, subject_id, subject_type, idempotency_key, invalidates, reason_code, \
     origin, metadata, entry_type::text AS entry_type \
     FROM usage_records WHERE id = $1";

/// How many entries one `(tenant, meter)` scope holds.
///
/// A row count is the direct statement of "nothing else was written", and it is
/// what replaced the per-scope sequence reads in the two tests that used to ask
/// the retired counter the same question indirectly.
async fn scope_row_count(pool: &sqlx::PgPool, tenant: Uuid, meter: &str) -> i64 {
    sqlx::query_scalar(
        "SELECT count(*) FROM usage_records WHERE tenant_id = $1 AND gts_type_id = $2",
    )
    .bind(tenant)
    .bind(meter)
    .fetch_one(pool)
    .await
    .expect("count the scope's rows")
}

// ---------------------------------------------------------------------------
// Dedup: absorb and conflict
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_fresh_entry_round_trips_through_create() {
    let (h, store) = setup().await;
    let meter = common::meter(common::VCPU_METER);
    let tenant = Uuid::from_u128(0x1001);

    let record = common::entry(&meter, tenant, "idem-new", Decimal::new(5, 0));
    let stored = store
        .create(record.clone())
        .await
        .expect("create a fresh entry");

    assert_eq!(stored.id, record.id, "the derived identity round-trips");
    assert_eq!(stored.quantity, record.quantity);
    assert_eq!(stored.window_start, record.window_start);
    assert_eq!(stored.window_end, record.window_end);
    assert_eq!(stored.origin, RecordOrigin::Live);
    assert!(
        stored.invalidation.is_none(),
        "an ordinary measurement carries no withdrawal"
    );

    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM usage_records WHERE id = $1")
        .bind(record.id)
        .fetch_one(&h.pool)
        .await
        .expect("count");
    assert_eq!(rows, 1);
}

/// An exact-equality retry under the same idempotency key is absorbed, and what
/// comes back is the **previously persisted** row rather than the submission.
///
/// "Returns the persisted row" cannot be shown by comparing fields, because an
/// absorb only happens when the submission and the stored row are canonically
/// equal — any field that could differ makes it a conflict instead. What is
/// observable is the ledger: the call succeeds twice and one row exists.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_exact_retry_is_absorbed_and_returns_the_persisted_row() {
    let (h, store) = setup().await;
    let meter = common::meter(common::VCPU_METER);
    let tenant = Uuid::from_u128(0x1002);

    let record = common::entry(&meter, tenant, "idem-retry", Decimal::new(7, 0));
    let first = store.create(record.clone()).await.expect("first create");
    let second = store
        .create(record.clone())
        .await
        .expect("an exact retry is absorbed, not refused");

    assert_eq!(first.id, second.id, "both calls answer with one entry");
    assert_eq!(second.id, record.id);
    assert_eq!(second.quantity, record.quantity);

    let rows: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM usage_records WHERE tenant_id = $1 AND idempotency_key = $2",
    )
    .bind(tenant)
    .bind("idem-retry")
    .fetch_one(&h.pool)
    .await
    .expect("count");
    assert_eq!(
        rows, 1,
        "the retry was absorbed against the stored row, not written beside it"
    );

    // The `rows == 1` above is the whole of it. There is no per-scope counter
    // left for an absorbed retry to advance, so "it stored no second row" is
    // the only thing the absorb can get wrong here.
}

/// An absorbed retry reports the *stored* entry's acceptance instant, not
/// whatever the caller's retry carried: `accepted_at` is ignored by
/// `UsageRecord::caller_supplied_eq`, so a retry stamped afresh by the gateway (a later
/// instant than the first write) still absorbs rather than conflicts, and
/// the row the caller gets back is the one already on disk.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_absorbed_retry_returns_the_first_acceptance_instant() {
    let (_h, store) = setup().await;
    let meter = common::meter(common::VCPU_METER);
    let first = common::entry(
        &meter,
        Uuid::from_u128(71),
        "accepted-at-retry",
        Decimal::from(5),
    );
    let first_at = first.accepted_at;
    let stored = store.create(first.clone()).await.expect("first write");
    assert_eq!(stored.accepted_at, first_at);

    let retry = UsageRecord {
        accepted_at: first_at + Duration::minutes(3),
        ..first
    };
    let absorbed = store.create(retry).await.expect("absorbed retry");
    assert_eq!(
        absorbed.accepted_at, first_at,
        "a replay reports the stored acceptance instant"
    );
}

/// A divergent write under a key already bound fails closed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_divergent_same_key_write_is_an_idempotency_conflict() {
    let (h, store) = setup().await;
    let meter = common::meter(common::VCPU_METER);
    let tenant = Uuid::from_u128(0x1003);

    let first = common::entry(&meter, tenant, "idem-conflict", Decimal::new(1, 0));
    let first = store.create(first).await.expect("first create");

    // Same six dedup inputs, different quantity: canonically unequal.
    let divergent = common::entry(&meter, tenant, "idem-conflict", Decimal::new(2, 0));
    assert_eq!(
        divergent.id, first.id,
        "the two share all six dedup-identity inputs, so they share an identifier"
    );

    let err = store
        .create(divergent)
        .await
        .expect_err("a divergent same-key write must be refused");
    match err {
        UsageCollectorPluginError::IdempotencyConflict {
            idempotency_key,
            existing,
        } => {
            assert_eq!(idempotency_key, "idem-conflict");
            assert_eq!(existing.id, first.id);
        }
        other => panic!("expected IdempotencyConflict, got {other:?}"),
    }

    let stored_value: Decimal =
        sqlx::query_scalar("SELECT quantity FROM usage_records WHERE id = $1")
            .bind(first.id)
            .fetch_one(&h.pool)
            .await
            .expect("read the stored value");
    assert_eq!(
        stored_value,
        Decimal::new(1, 0),
        "fail-closed means the stored entry is untouched"
    );
}

// ---------------------------------------------------------------------------
// The transaction id
// ---------------------------------------------------------------------------

/// Every entry of one batch carries one `xact_id`, and a call that commits
/// afterwards carries a greater one.
///
/// `DESIGN.md` §3.6 `cpt-cf-uc-plugin-seq-ingest-batch`: "Every entry of one
/// batch shares the batch's `xact_id`; feed order within it falls back to
/// `id`." And §3.6 Correction order: an invalidation the gateway accepts only
/// once its target has converged has its transaction "assigned its id after
/// the target's committed", so `xact_id(invalidation) > xact_id(target)`.
/// Correction order is that second property read on a withdrawal, which is why
/// it is asserted over two ordinary calls here rather than over a pair: the
/// property is about the commit order, not about the kind of entry.
///
/// This is what separates a database-stamped transaction id from a Rust-side
/// counter: no per-entry counter would give two entries of one call the same
/// value, and the equality half is the only assertion that can tell them
/// apart. **Nothing *orders* on `xact_id` yet** — every read path returns it,
/// because `RECORD_COLUMNS` selects it, but the feed page that would page on it
/// is slice 3 — so without this test the column would be one the schema declares
/// and no behaviour holds to an order.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn one_batch_shares_one_transaction_id_and_a_later_call_takes_a_greater_one() {
    let (h, store) = setup().await;
    let meter = common::meter(common::VCPU_METER);
    let tenant = Uuid::from_u128(0x7AC7);

    let a = common::entry(&meter, tenant, "xact-batch-a", Decimal::from(1));
    let b = common::entry(&meter, tenant, "xact-batch-b", Decimal::from(2));
    let batch = store
        .create_batch(vec![a.clone(), b.clone()])
        .await
        .expect("the batch as a whole succeeds");
    assert!(
        batch.iter().all(Result::is_ok),
        "both entries must be accepted: {batch:?}"
    );

    let xa = common::xact_id_of(&h.pool, a.id).await;
    let xb = common::xact_id_of(&h.pool, b.id).await;
    assert_eq!(
        xa, xb,
        "one batch is one transaction, so one xact_id: got {xa} and {xb}"
    );

    let later = common::entry(&meter, tenant, "xact-later", Decimal::from(3));
    store.create(later.clone()).await.expect("the later write");
    let xl = common::xact_id_of(&h.pool, later.id).await;
    assert!(
        xl > xa,
        "a call that commits after the batch takes a greater transaction id: \
         batch {xa}, later {xl}"
    );
}

// ---------------------------------------------------------------------------
// At most one invalidation per entry: a dedup outcome of the derived key
// ---------------------------------------------------------------------------

/// A second withdrawal with the **same** reason code is an exact retry of the
/// first: absorbed, answering with the stored invalidation.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_second_withdrawal_with_the_same_reason_is_absorbed() {
    let (h, store, provider, exporter) = setup_metered().await;
    let meter = common::meter(common::VCPU_METER);
    let tenant = Uuid::from_u128(0x1_A710);

    let target = store
        .create(common::entry(
            &meter,
            tenant,
            "idem-target",
            Decimal::new(10, 0),
        ))
        .await
        .expect("create the target");
    let first = store
        .create(common::withdrawal_of(&target))
        .await
        .expect("the first withdrawal is accepted");

    let retry = UsageRecord {
        accepted_at: first.accepted_at + Duration::minutes(1),
        ..common::withdrawal_of(&target)
    };
    let absorbed = store
        .create(retry)
        .await
        .expect("a same-reason withdrawal is absorbed");
    assert_eq!(absorbed.id, first.id);
    assert_eq!(
        absorbed.accepted_at, first.accepted_at,
        "the stored invalidation is returned"
    );

    let withdrawals: i64 =
        sqlx::query_scalar("SELECT count(*) FROM usage_records WHERE invalidates = $1")
            .bind(target.id)
            .fetch_one(&h.pool)
            .await
            .expect("count withdrawals");
    assert_eq!(withdrawals, 1);

    provider.force_flush().expect("flush metrics");
    assert_eq!(
        counter_sum(&exporter, "uc_timescaledb_dedup_absorbed_total"),
        1
    );
    assert_eq!(
        counter_sum(&exporter, "uc_timescaledb_invalidations_total"),
        1
    );
}

/// A second withdrawal under **another** reason code repeats the first's six
/// identity inputs -- the target's five plus `entry_type = invalidation` -- so
/// it collides and conflicts, carrying the accepted withdrawal.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_second_withdrawal_with_another_reason_conflicts_carrying_the_first() {
    let (h, store, provider, exporter) = setup_metered().await;
    let meter = common::meter(common::VCPU_METER);
    let tenant = Uuid::from_u128(0x1_A711);

    let target = store
        .create(common::entry(
            &meter,
            tenant,
            "idem-target",
            Decimal::new(10, 0),
        ))
        .await
        .expect("create the target");
    let first = store
        .create(common::withdrawal_of(&target))
        .await
        .expect("the first withdrawal is accepted");

    let err = store
        .create(common::withdrawal_of_with_reason(
            &target,
            "second_withdrawal",
        ))
        .await
        .expect_err("a divergent second withdrawal is refused");
    match err {
        UsageCollectorPluginError::IdempotencyConflict { existing, .. } => {
            assert_eq!(existing.id, first.id);
            assert_eq!(
                existing.invalidation.as_ref().map(|i| i.reason.as_str()),
                Some("duplicate_submission"),
                "the stored reason code travels with the conflict"
            );
        }
        other => panic!("expected IdempotencyConflict, got {other:?}"),
    }

    let withdrawals: i64 =
        sqlx::query_scalar("SELECT count(*) FROM usage_records WHERE invalidates = $1")
            .bind(target.id)
            .fetch_one(&h.pool)
            .await
            .expect("count withdrawals");
    assert_eq!(withdrawals, 1);

    provider.force_flush().expect("flush metrics");
    assert_eq!(
        counter_sum(&exporter, "uc_timescaledb_idempotency_conflicts_total"),
        1
    );
}

/// A batch carrying a withdrawal of a target an **earlier call** already
/// withdrew answers per row: the withdrawal conflicts and the rest of the batch
/// keeps its outcomes. With the store-side index gone nothing can abort the
/// whole statement.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_batch_withdrawing_an_already_withdrawn_target_answers_per_row() {
    let (h, store) = setup().await;
    let meter = common::meter(common::VCPU_METER);
    let tenant = Uuid::from_u128(0x1_A712);

    let target = store
        .create(common::entry(
            &meter,
            tenant,
            "idem-target",
            Decimal::new(10, 0),
        ))
        .await
        .expect("create the target");
    let first = store
        .create(common::withdrawal_of(&target))
        .await
        .expect("the first withdrawal is accepted");
    let unrelated = common::entry(&meter, tenant, "idem-unrelated", Decimal::new(3, 0));

    let results = store
        .create_batch(vec![
            common::withdrawal_of_with_reason(&target, "second_withdrawal"),
            unrelated.clone(),
        ])
        .await
        .expect("the batch as a whole succeeds and answers per row");

    match &results[0] {
        Err(UsageCollectorPluginError::IdempotencyConflict { existing, .. }) => {
            assert_eq!(existing.id, first.id);
        }
        other => panic!("row 0 must be IdempotencyConflict, got {other:?}"),
    }
    assert_eq!(
        results[1]
            .as_ref()
            .expect("the unrelated row is accepted")
            .id,
        unrelated.id
    );

    let withdrawals: i64 =
        sqlx::query_scalar("SELECT count(*) FROM usage_records WHERE invalidates = $1")
            .bind(target.id)
            .fetch_one(&h.pool)
            .await
            .expect("count withdrawals");
    assert_eq!(withdrawals, 1);
}

/// Withdrawals of one target inside **one batch** resolve later against
/// earlier: an identical repeat is absorbed, one under another reason code
/// conflicts, and an unrelated row is unaffected.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn withdrawals_of_one_target_in_one_batch_resolve_later_against_earlier() {
    let (h, store, provider, exporter) = setup_metered().await;
    let meter = common::meter(common::VCPU_METER);
    let tenant = Uuid::from_u128(0x1_A713);

    let target = store
        .create(common::entry(
            &meter,
            tenant,
            "idem-target",
            Decimal::new(10, 0),
        ))
        .await
        .expect("create the target");
    let w1 = common::withdrawal_of(&target);
    let w2 = common::withdrawal_of_with_reason(&target, "second_withdrawal");
    let unrelated = common::entry(&meter, tenant, "idem-unrelated", Decimal::new(3, 0));

    let results = store
        .create_batch(vec![w1.clone(), w1.clone(), w2, unrelated.clone()])
        .await
        .expect("per-row outcomes");

    assert_eq!(results.len(), 4);
    assert_eq!(results[0].as_ref().expect("w1 accepted").id, w1.id);
    assert_eq!(
        results[1].as_ref().expect("the repeat is absorbed").id,
        w1.id
    );
    match &results[2] {
        Err(UsageCollectorPluginError::IdempotencyConflict { existing, .. }) => {
            assert_eq!(existing.id, w1.id);
        }
        other => panic!("row 2 must be IdempotencyConflict, got {other:?}"),
    }
    assert_eq!(
        results[3].as_ref().expect("unrelated accepted").id,
        unrelated.id
    );

    let withdrawals: i64 =
        sqlx::query_scalar("SELECT count(*) FROM usage_records WHERE invalidates = $1")
            .bind(target.id)
            .fetch_one(&h.pool)
            .await
            .expect("count withdrawals");
    assert_eq!(withdrawals, 1);

    provider.force_flush().expect("flush metrics");
    assert_eq!(
        counter_sum(&exporter, "uc_timescaledb_dedup_absorbed_total"),
        1
    );
    assert_eq!(
        counter_sum(&exporter, "uc_timescaledb_idempotency_conflicts_total"),
        1
    );
    assert_eq!(
        counter_sum(&exporter, "uc_timescaledb_invalidations_total"),
        1
    );
}

/// Two `create` calls withdrawing one target concurrently, under different
/// reason codes: exactly one is accepted, and the loser conflicts carrying the
/// winner. `ON CONFLICT DO NOTHING` on the dedup identity serialises them.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_concurrent_withdrawals_of_one_target_admit_exactly_one() {
    let (h, store) = setup().await;
    let meter = common::meter(common::VCPU_METER);
    let tenant = Uuid::from_u128(0x1_A714);

    let target = store
        .create(common::entry(
            &meter,
            tenant,
            "idem-target",
            Decimal::new(10, 0),
        ))
        .await
        .expect("create the target");

    let a = common::withdrawal_of(&target);
    let b = common::withdrawal_of_with_reason(&target, "second_withdrawal");
    let (sa, sb) = (store.clone(), store.clone());
    let (ra, rb) = tokio::join!(
        tokio::spawn(async move { sa.create(a).await }),
        tokio::spawn(async move { sb.create(b).await }),
    );
    let ra = ra.expect("task a did not panic");
    let rb = rb.expect("task b did not panic");

    let accepted = usize::from(ra.is_ok()) + usize::from(rb.is_ok());
    assert_eq!(
        accepted, 1,
        "exactly one is admitted; got a={ra:?} b={rb:?}"
    );
    let (winner, loser) = if let Ok(winner) = &ra {
        (winner, &rb)
    } else {
        (rb.as_ref().expect("exactly one of the two is Ok"), &ra)
    };
    match loser {
        Err(UsageCollectorPluginError::IdempotencyConflict { existing, .. }) => {
            assert_eq!(existing.id, winner.id, "the conflict carries the winner");
        }
        other => panic!("the losing withdrawal must be IdempotencyConflict, got {other:?}"),
    }

    let withdrawals: i64 =
        sqlx::query_scalar("SELECT count(*) FROM usage_records WHERE invalidates = $1")
            .bind(target.id)
            .fetch_one(&h.pool)
            .await
            .expect("count withdrawals");
    assert_eq!(withdrawals, 1);
}

/// SPEC-DIFF 2.5: `origin` is server-assigned, so one entry arriving first live
/// and then through backfill is one entry.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_identical_entry_from_live_then_backfill_is_absorbed() {
    let (_h, store) = setup().await;
    let meter = common::meter(common::VCPU_METER);
    let live = common::entry(
        &meter,
        Uuid::from_u128(0x1_A715),
        "idem-origin",
        Decimal::new(4, 0),
    );

    let stored = store.create(live.clone()).await.expect("live write");
    let absorbed = store
        .create(UsageRecord {
            origin: RecordOrigin::Backfill,
            ..live
        })
        .await
        .expect("same caller-supplied fields absorb whatever the route");
    assert_eq!(absorbed.id, stored.id);
    assert_eq!(
        absorbed.origin,
        RecordOrigin::Live,
        "the stored entry is returned"
    );
}

/// Decision S-B7: `42.5` then `42.500` under one identity is a conflict.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_quantity_at_another_scale_under_one_identity_is_a_conflict() {
    let (_h, store) = setup().await;
    let meter = common::meter(common::VCPU_METER);
    let tenant = Uuid::from_u128(0x1_A716);

    let first = common::entry(&meter, tenant, "idem-scale", Decimal::new(425, 1));
    store.create(first.clone()).await.expect("first write");
    let rescaled = common::entry(&meter, tenant, "idem-scale", Decimal::new(42_500, 3));
    assert_eq!(rescaled.id, first.id, "one identity");

    match store.create(rescaled).await {
        Err(UsageCollectorPluginError::IdempotencyConflict { existing, .. }) => {
            assert_eq!(existing.quantity.to_string(), "42.5");
        }
        other => panic!("expected IdempotencyConflict, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Batch outcomes
// ---------------------------------------------------------------------------

/// Per-record outcomes are aligned with input order, and a conflict is isolated
/// to its own slot.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn batch_outcomes_are_aligned_with_input_order() {
    let (_h, store) = setup().await;
    let meter = common::meter(common::VCPU_METER);
    let tenant = Uuid::from_u128(0x1005);

    let seeded = common::entry(&meter, tenant, "batch-dup", Decimal::new(1, 0));
    let seeded = store.create(seeded).await.expect("seed the duplicate key");

    let row0 = common::entry(&meter, tenant, "batch-0", Decimal::new(2, 0));
    let row1 = common::entry(&meter, tenant, "batch-dup", Decimal::new(42, 0));
    let row2 = common::entry(&meter, tenant, "batch-2", Decimal::new(3, 0));

    let results = store
        .create_batch(vec![row0.clone(), row1, row2.clone()])
        .await
        .expect("batch returns per-row outcomes");

    assert_eq!(results.len(), 3);
    assert_eq!(results[0].as_ref().expect("row 0 inserted").id, row0.id);
    match results[1].as_ref() {
        Err(UsageCollectorPluginError::IdempotencyConflict { existing, .. }) => {
            assert_eq!(existing.id, seeded.id);
        }
        other => panic!("row 1 must be IdempotencyConflict, got {other:?}"),
    }
    assert_eq!(results[2].as_ref().expect("row 2 inserted").id, row2.id);
}

/// One batch carrying a fresh key, an exact retry of it, and a divergent write
/// under it: insert, absorb, conflict — resolved against the row this same
/// batch wrote.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_in_batch_duplicate_resolves_against_the_row_the_batch_wrote() {
    let (h, store) = setup().await;
    let meter = common::meter(common::VCPU_METER);
    let tenant = Uuid::from_u128(0x001B_A7C0);

    let first = common::entry(&meter, tenant, "intra-dup", Decimal::new(5, 0));
    let exact = first.clone();
    let divergent = common::entry(&meter, tenant, "intra-dup", Decimal::new(9, 0));

    let results = store
        .create_batch(vec![first.clone(), exact, divergent])
        .await
        .expect("batch returns per-row outcomes");

    assert_eq!(results.len(), 3);
    assert_eq!(
        results[0].as_ref().expect("first occurrence inserted").id,
        first.id
    );
    assert_eq!(
        results[1].as_ref().expect("exact duplicate absorbed").id,
        first.id,
        "the absorb answers with the winner's stored row"
    );
    match results[2].as_ref() {
        Err(UsageCollectorPluginError::IdempotencyConflict { existing, .. }) => {
            assert_eq!(existing.id, first.id);
        }
        other => panic!("row 2 must be IdempotencyConflict, got {other:?}"),
    }

    let rows: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM usage_records WHERE tenant_id = $1 AND idempotency_key = $2",
    )
    .bind(tenant)
    .bind("intra-dup")
    .fetch_one(&h.pool)
    .await
    .expect("count");
    assert_eq!(rows, 1, "one dedup key, one row");
}

/// A batch in which **every** row conflicts: nothing is inserted at all, and
/// each outcome still names its own seed.
///
/// Its own case rather than a weaker `batch_outcomes_are_aligned_with_input_order`
/// - that one always wins at least one slot, so the multi-row `INSERT` always
/// returns rows and `read_conflict_records` is always handed a proper subset.
/// Here the insert wins nothing, `inserted` is empty, and every key goes down
/// the conflict-read path at once. Two things could hide in that shape and in
/// no other: an empty-`RETURNING` path that mistook "won no slot" for "found no
/// row", and an off-by-one in the alignment, which needs each row to conflict
/// against a *different* seed to be visible. So the three keys are distinct
/// rather than three divergent writes of one.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_batch_in_which_every_row_conflicts_inserts_nothing_and_stays_aligned() {
    let (h, store) = setup().await;
    let meter = common::meter(common::VCPU_METER);
    let tenant = Uuid::from_u128(0x000A_11C0);

    // Three distinct keys, each seeded at its own quantity and its own period.
    let mut seeded = Vec::with_capacity(3);
    for i in 0..3_i64 {
        let rec = common::entry_over(
            &meter,
            tenant,
            &format!("all-conf-{i}"),
            Decimal::from(i + 1),
            common::fixture_window_start() + Duration::hours(i),
            common::fixture_window_end() + Duration::hours(i),
        );
        seeded.push(store.create(rec).await.expect("seed"));
    }
    let rows_before = scope_row_count(&h.pool, tenant, common::VCPU_METER).await;
    assert_eq!(rows_before, 3, "three seeded entries");

    // The same three keys and periods, every one at a divergent quantity.
    let batch: Vec<UsageRecord> = (0..3_i64)
        .map(|i| {
            common::entry_over(
                &meter,
                tenant,
                &format!("all-conf-{i}"),
                Decimal::from(i + 100),
                common::fixture_window_start() + Duration::hours(i),
                common::fixture_window_end() + Duration::hours(i),
            )
        })
        .collect();

    let results = store
        .create_batch(batch)
        .await
        .expect("a batch every row of which conflicts is still a successful call");

    assert_eq!(results.len(), 3, "one outcome per input row");
    for (i, r) in results.iter().enumerate() {
        match r {
            Err(UsageCollectorPluginError::IdempotencyConflict {
                idempotency_key,
                existing,
            }) => {
                assert_eq!(
                    idempotency_key,
                    &format!("all-conf-{i}"),
                    "row {i}'s conflict must name row {i}'s key"
                );
                assert_eq!(
                    existing.id, seeded[i].id,
                    "row {i}'s conflict must name row {i}'s seed, not a neighbour's"
                );
            }
            other => panic!("row {i} must be IdempotencyConflict, got {other:?}"),
        }
    }

    // Nothing was written, and nothing was overwritten.
    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM usage_records WHERE tenant_id = $1")
        .bind(tenant)
        .fetch_one(&h.pool)
        .await
        .expect("count");
    assert_eq!(rows, 3, "an all-conflict batch inserts nothing");
    let stored: Vec<Decimal> = sqlx::query_scalar(
        "SELECT quantity FROM usage_records WHERE tenant_id = $1 ORDER BY window_end",
    )
    .bind(tenant)
    .fetch_all(&h.pool)
    .await
    .expect("read the stored quantities");
    assert_eq!(
        stored,
        vec![Decimal::from(1), Decimal::from(2), Decimal::from(3)],
        "fail-closed: the seeded quantities are untouched by the divergent batch"
    );

    // Nor did the refused batch leave a row behind. Asserted over the whole
    // scope rather than over the three seeded quantities above, because those
    // pin what the surviving rows hold and this pins that there are no others:
    // a fourth row carrying a divergent quantity under a fresh identity would
    // satisfy the quantity assertion and fail this one.
    assert_eq!(
        scope_row_count(&h.pool, tenant, common::VCPU_METER).await,
        rows_before,
        "an all-conflict batch must store no row of its own"
    );
}

/// A hundred distinct entries all insert in one batch, and every one of them
/// carries the same `xact_id`.
///
/// The scale is the point of the first half: a multi-row `INSERT … SELECT FROM
/// UNNEST` that mis-sizes one of its arrays fails on a long batch where a pair
/// would not notice.
///
/// The second half is the property the retired per-scope block used to stand in
/// for, stated over the mechanism that survives. "One batch is one transaction"
/// is asserted at a pair by
/// `one_batch_shares_one_transaction_id_and_a_later_call_takes_a_greater_one`;
/// what this adds is that the batch size does not break it, which is the way a
/// per-row transaction or a chunked insert would show up.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_hundred_distinct_entries_all_insert_in_one_batch_under_one_xact_id() {
    let (h, store) = setup().await;
    let meter = common::meter(common::VCPU_METER);
    let tenant = Uuid::from_u128(0x1000);

    let mut batch = Vec::with_capacity(100);
    for i in 0..100_i64 {
        let mut rec = common::entry_over(
            &meter,
            tenant,
            &format!("bulk-{i}"),
            Decimal::from(i + 1),
            common::fixture_window_start() + Duration::minutes(i),
            common::fixture_window_end() + Duration::minutes(i),
        );
        if i % 3 == 0 {
            rec.subject_ref =
                Some(SubjectRef::new(format!("subj-{i}"), Some("user")).expect("valid subject"));
        }
        if i % 5 == 0 {
            rec.metadata.insert(
                MetadataKey::new("region").expect("valid metadata key"),
                "eu-west-1".to_owned(),
            );
        }
        batch.push(rec);
    }

    let results = store.create_batch(batch).await.expect("batch ok");
    assert_eq!(results.len(), 100);
    for (i, r) in results.iter().enumerate() {
        assert!(r.is_ok(), "row {i} must insert: {r:?}");
    }

    // All hundred rows landed, and under exactly one transaction id. Counted as
    // *distinct* values rather than compared pairwise: the failure this guards
    // against is the batch splitting across transactions, and the number of
    // pieces it split into is what a reader needs to see.
    let (stored, distinct_xacts): (i64, i64) = sqlx::query_as(
        "SELECT count(*), count(DISTINCT xact_id) FROM usage_records \
         WHERE tenant_id = $1 AND gts_type_id = $2",
    )
    .bind(tenant)
    .bind(common::VCPU_METER)
    .fetch_one(&h.pool)
    .await
    .expect("count the scope's rows and transaction ids");
    assert_eq!(stored, 100, "every entry of the batch is stored");
    assert_eq!(
        distinct_xacts, 1,
        "one batch is one transaction, so one xact_id across all hundred rows"
    );
}

/// An empty batch is a host-contract breach, not an empty answer.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_empty_batch_is_a_host_contract_breach() {
    let (_h, store) = setup().await;

    let err = store
        .create_batch(Vec::new())
        .await
        .expect_err("an empty batch must not be served");
    assert!(
        matches!(err, UsageCollectorPluginError::Internal(_)),
        "an empty batch must surface as Internal, got {err:?}"
    );
}

/// Two batches whose key sets overlap, run concurrently **in one scope**,
/// complete and leave one row per dedup key.
///
/// **This one does not exercise `plan_batch`'s lock ordering, and saying it did
/// would be an unearned claim.** Both batches sit in a single
/// `(tenant_id, gts_type_id)`, so `claim_batch_sequences` takes that one
/// counter row first and the second batch waits there: delete
/// `plan_batch`'s `reps.sort_by` and this test still passes. What it does
/// assert is the per-row outcome of an overlap - every shared key absorbs
/// against the batch that won it, rather than conflicting or duplicating.
///
/// The deadlock-freedom the sort exists for needs two scopes taken in opposite
/// orders, which is the next test.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_overlapping_batches_leave_one_row_per_key() {
    let (h, store) = setup().await;
    let meter = common::meter(common::VCPU_METER);
    let tenant = Uuid::from_u128(0x0DEA_D10C);

    // Every field is a function of the key's own index, not of the position in
    // the batch: the ten shared keys must carry *identical* entries on both
    // sides, or the overlap is a canonical mismatch and the answer is
    // `IdempotencyConflict` rather than the absorb this test is about.
    let build = |offset: i64| {
        (0..20_i64)
            .map(|i| {
                let n = i + offset;
                common::entry_over(
                    &meter,
                    tenant,
                    &format!("overlap-{n}"),
                    Decimal::from(n + 1),
                    common::fixture_window_start() + Duration::minutes(n),
                    common::fixture_window_end() + Duration::minutes(n),
                )
            })
            .collect::<Vec<_>>()
    };
    // Offsets 0 and 10: ten keys in common, ten unique to each side.
    let (left, right) = (build(0), build(10));
    let (sa, sb) = (store.clone(), store.clone());
    let (ra, rb) = tokio::join!(
        tokio::spawn(async move { sa.create_batch(left).await }),
        tokio::spawn(async move { sb.create_batch(right).await }),
    );
    let ra = ra
        .expect("task a did not panic")
        .expect("batch a completed");
    let rb = rb
        .expect("task b did not panic")
        .expect("batch b completed");
    for (i, r) in ra.iter().chain(rb.iter()).enumerate() {
        assert!(
            r.is_ok(),
            "every row of both batches must resolve to a stored entry; row {i}: {r:?}"
        );
    }

    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM usage_records WHERE tenant_id = $1")
        .bind(tenant)
        .fetch_one(&h.pool)
        .await
        .expect("count");
    assert_eq!(rows, 30, "twenty plus twenty with ten keys in common");
}

/// Two batches spanning **two scopes in opposite input order**, run
/// concurrently: no deadlock victim is ever produced.
///
/// **This is what `plan_batch`'s `reps.sort_by` is for, and the only test that
/// reaches it.** The lock it orders is the **speculative tuple** an
/// `INSERT … ON CONFLICT … DO NOTHING` takes on each dedup 6-tuple it is
/// inserting, held until that transaction commits. The multi-row insert
/// processes `plan.reps` in array order, so that is the order the locks are
/// taken in; sorted by dedup key, every batch in the process takes the locks of
/// the keys it shares with another batch in one global order. Unsorted, batch A
/// takes vcpu's keys then gb's while batch B takes gb's then vcpu's - the ABBA
/// deadlock, which `PostgreSQL` breaks by aborting a victim after
/// `deadlock_timeout`.
///
/// It was written on a per-scope counter row lock, taken once per
/// `(tenant_id, gts_type_id)` run rather than once per key. Retiring that counter
/// left the speculative tuple as the only lock a batch contends for, so this test
/// is re-aimed rather than retired: the surviving serialisation mechanism would
/// otherwise have no deadlock-freedom test at all.
/// **The fixture needed no change, and that is why the re-aim is sound.** Both
/// batches already carry byte-identical rows, so the two sides share every key
/// and not merely every scope - which is exactly what the tuple lock needs to be
/// contended at all. A fixture sharing only scopes would have gone green here
/// while asserting nothing.
///
/// **The retry counter cannot be the oracle, because it counts a retry and not
/// its cause.** `is_retryable_batch_error` admits **any** `Transient`, and three
/// things reach it here: the ABBA deadlock victim (`40P01`) this test exists
/// for, a `55P03 lock_not_available` that waited out `lock_timeout` on one of
/// those same tuples, and - since the per-scope counter was retired - a
/// concurrent writer of one of the batch's own identities colliding on the
/// PRIMARY KEY, which `PgRecordStore::record_insert_error` lifts to a
/// `Transient` so the batch re-runs against the winner's committed row.
///
/// **That third one is not incidental here: this fixture guarantees it.** Both
/// batches carry byte-identical rows, so both sides submit the *same derived
/// identities*, and the arbiter covers only the dedup UNIQUE. The rows are
/// identical for a reason that still holds and is worth keeping: a divergent
/// overlap would resolve as an `IdempotencyConflict` and would say nothing about
/// lock order at all. So the fixture that makes the lock-order question askable
/// is the same fixture that makes an identity-collision retry routine, and zero
/// retries is not an assertable bound.
///
/// **The oracle is `PostgreSQL`'s own deadlock counter instead.** The deadlock
/// detector increments `pg_stat_database.deadlocks` when it aborts a victim, and
/// nothing else does, so it separates the cause this test is about from the two
/// it is not - which no bound on the retry count can do. Every `bring_up` starts
/// its own container, so that counter belongs to this test alone.
///
/// Cumulative statistics are flushed by the reporting backend rather than
/// written synchronously, and a backend does not flush more often than about
/// once a second, so the read waits before it asks. That latency can only
/// **under**-count, never over-count, which is the asymmetry that makes the
/// assertion safe: a scheduling accident cannot turn it red, and only a real
/// victim can.
///
/// What this test establishes:
///
/// * both batches complete - no row is lost to a retry budget running out;
/// * every row resolves `Ok`, in both scopes, with one stored row per key;
/// * **no deadlock victim was produced at all**, which is the property
///   `reps.sort_by` exists to provide.
///
/// The retry count is read alongside it and reported in the failure message as
/// context, deliberately **not** asserted on: measured on this fixture it sits
/// at 0 or 1 sorted and at 3 or 4 unsorted, so a threshold between them would be
/// a tuned literal standing in for an instrument that already exists.
///
/// Several rounds rather than one, because a deadlock needs the two
/// transactions to interleave and one round can miss.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_batches_taking_two_scopes_in_opposite_orders_never_deadlock() {
    let (h, store, provider, exporter) = setup_metered().await;
    let vcpu = common::meter(common::VCPU_METER);
    let gb = common::meter(common::GB_METER);
    let tenant = Uuid::from_u128(0x0DEA_D10D);
    let deadlocks_before = deadlock_count(&h.pool).await;

    // One round's rows for one scope. Both batches carry byte-identical rows,
    // so every overlap is an exact retry - a divergent one would be an
    // `IdempotencyConflict` and would say nothing about lock order.
    let rows_for = |meter: &_, round: i64| -> Vec<UsageRecord> {
        (0..4_i64)
            .map(|i| {
                let n = round * 4 + i;
                common::entry_over(
                    meter,
                    tenant,
                    &format!("deadlock-{n}"),
                    Decimal::from(n + 1),
                    common::fixture_window_start() + Duration::minutes(n),
                    common::fixture_window_end() + Duration::minutes(n),
                )
            })
            .collect()
    };

    for round in 0..DEADLOCK_ROUNDS {
        let (vcpu_rows, gb_rows) = (rows_for(&vcpu, round), rows_for(&gb, round));
        // A: vcpu then gb. B: gb then vcpu. Same rows, opposite input order.
        let mut a = vcpu_rows.clone();
        a.extend(gb_rows.clone());
        let mut b = gb_rows;
        b.extend(vcpu_rows);

        let (sa, sb) = (store.clone(), store.clone());
        let (ra, rb) = tokio::join!(
            tokio::spawn(async move { sa.create_batch(a).await }),
            tokio::spawn(async move { sb.create_batch(b).await }),
        );
        let ra = ra
            .expect("task a did not panic")
            .expect("batch a completed");
        let rb = rb
            .expect("task b did not panic")
            .expect("batch b completed");
        for (i, r) in ra.iter().chain(rb.iter()).enumerate() {
            assert!(r.is_ok(), "round {round} row {i}: {r:?}");
        }
    }

    provider.force_flush().expect("flush metrics");
    // Give the reporting backends time to flush their pending statistics; see
    // the doc comment for why waiting can only under-count.
    tokio::time::sleep(StdDuration::from_secs(2)).await;
    let deadlocks = deadlock_count(&h.pool).await - deadlocks_before;
    let calls = u64::try_from(DEADLOCK_ROUNDS).expect("a small round count") * 2;
    let retries = counter_sum(&exporter, "uc_timescaledb_batch_retries_total");
    assert_eq!(
        deadlocks, 0,
        "PostgreSQL aborted {deadlocks} deadlock victim(s) over {calls} concurrent \
         batch calls. Every call still returned Ok, because the batch retry absorbs \
         a victim - so the ledger is intact and it is the lock order that regressed. \
         plan_batch's reps.sort_by is the first thing to check: unsorted, the two \
         sides take the shared keys' speculative tuple locks in opposite orders. \
         ({retries} batch retries were counted, of which a deadlock victim is only \
         one possible cause - see the doc comment.)"
    );

    // And the ledger holds one row per key, in both scopes.
    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM usage_records WHERE tenant_id = $1")
        .bind(tenant)
        .fetch_one(&h.pool)
        .await
        .expect("count");
    assert_eq!(
        rows,
        DEADLOCK_ROUNDS * 4 * 2,
        "one stored row per key: DEADLOCK_ROUNDS rounds x 4 keys x 2 scopes"
    );
}

/// How many rounds the two-scope deadlock-freedom test drives.
///
/// Several rather than one, because a deadlock needs the two transactions to
/// interleave and a single round can miss. It is named so the retry bound and
/// the stored-row count are both derived from it instead of restating `6`.
const DEADLOCK_ROUNDS: i64 = 6;

/// How many concurrent rounds the two same-identity race tests drive.
///
/// A race needs the two transactions to interleave, and one round can miss. The
/// cost of a round is two concurrent calls; the cost of another *run* is a
/// container bring-up, which is about as expensive as twenty rounds - so this is
/// where attempts are bought cheaply. Twenty was enough to turn the batch path's
/// window from "never observed in twelve runs" into a reproduction.
const ROUNDS_PER_RACE: i64 = 20;

/// Two concurrent **batches** whose row sets share one derived identity resolve
/// to one acceptance and one `IdempotencyConflict`, the same as the single-row
/// pair above.
///
/// **This exists because the batch path shares the single path's exposure and
/// nothing drove it.** Both paths insert with
/// `ON CONFLICT (<dedup 6-tuple>, type_key) DO NOTHING`, and that arbiter does
/// not cover `PRIMARY KEY (id, window_end, type_key)`. Two writers of one
/// derived `id` therefore collide on a unique index `DO NOTHING` does not
/// suppress, and the loser takes a raw `23505` instead of the empty
/// `RETURNING` it expects. `concurrent_overlapping_batches_leave_one_row_per_key`
/// shares *keys* but never the divergent content that makes the loser's outcome
/// observable, so it passed throughout while the window was open.
///
/// The batch analogue of the single-row conflict, so it needs the same
/// ingredients: one shared identity (a withdrawal of one target, which derives
/// its `id` from the target's own five inputs plus the invalidation kind) and
/// **divergent content** under it (two different reason codes, which are not
/// identity inputs). A shared-and-identical pair would absorb and say nothing
/// about the losing branch.
///
/// Each side carries a private filler row as well, so this is a multi-row
/// `UNNEST` insert rather than a one-row batch wearing the batch path's clothes.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_concurrent_batches_sharing_one_identity_admit_exactly_one() {
    let (h, store) = setup().await;
    let meter = common::meter(common::VCPU_METER);
    let tenant = Uuid::from_u128(0x0BA7_C401);

    // **Several rounds, for the reason the deadlock test gives**: a race needs the
    // two transactions to interleave and one round can miss it. One container
    // bring-up costs about as much as this whole loop, so rounds are where the
    // attempts are cheap. Each round uses a fresh target, so no round resolves
    // against a previous round's stored withdrawal.
    for round in 0..ROUNDS_PER_RACE {
        let target = store
            .create(common::entry_over(
                &meter,
                tenant,
                &format!("batch-conflict-target-{round}"),
                Decimal::new(10, 0),
                common::fixture_window_start() + Duration::minutes(round),
                common::fixture_window_end() + Duration::minutes(round),
            ))
            .await
            .expect("create the target");

        // The shared identity, with divergent content on each side.
        let wa = common::withdrawal_of(&target);
        let wb = common::withdrawal_of_with_reason(&target, "second_withdrawal");
        assert_eq!(
            wa.id, wb.id,
            "round {round}: the two withdrawals must share one derived identity, or \
             this test drives no collision at all: reason_code is not one of the six \
             identity inputs"
        );

        // A private row per side, so each call is a genuine multi-row `UNNEST`
        // insert rather than a one-row batch wearing the batch path's clothes.
        let filler = |n: i64| {
            common::entry_over(
                &meter,
                tenant,
                &format!("batch-conflict-filler-{round}-{n}"),
                Decimal::from(n + 1),
                common::fixture_window_start() + Duration::minutes(round),
                common::fixture_window_end() + Duration::minutes(round),
            )
        };

        let (sa, sb) = (store.clone(), store.clone());
        let (left, right) = (vec![wa, filler(1)], vec![wb, filler(2)]);
        let (ra, rb) = tokio::join!(
            tokio::spawn(async move { sa.create_batch(left).await }),
            tokio::spawn(async move { sb.create_batch(right).await }),
        );
        let ra = ra
            .expect("task a did not panic")
            .unwrap_or_else(|e| panic!("round {round}: batch a failed as a whole: {e:?}"));
        let rb = rb
            .expect("task b did not panic")
            .unwrap_or_else(|e| panic!("round {round}: batch b failed as a whole: {e:?}"));

        // Each side's filler is its own identity and must be stored either way.
        for (side, rows) in [("a", &ra), ("b", &rb)] {
            assert_eq!(
                rows.len(),
                2,
                "round {round} side {side} answers per input row"
            );
            assert!(
                rows[1].is_ok(),
                "round {round} side {side}'s private filler row collides with \
                 nothing: {:?}",
                rows[1]
            );
        }

        // The shared identity: one acceptance, one IdempotencyConflict. Asserted
        // over the pair rather than per side, because which side wins is the
        // race's to decide.
        let (left_shared, right_shared) = (&ra[0], &rb[0]);
        let accepted = usize::from(left_shared.is_ok()) + usize::from(right_shared.is_ok());
        assert_eq!(
            accepted, 1,
            "round {round}: exactly one of the two withdrawals is admitted; got \
             a={left_shared:?} b={right_shared:?}"
        );
        let (winner, loser) = if let Ok(winner) = left_shared {
            (winner, right_shared)
        } else {
            (
                right_shared.as_ref().expect("exactly one of the two is Ok"),
                left_shared,
            )
        };
        match loser {
            Err(UsageCollectorPluginError::IdempotencyConflict { existing, .. }) => {
                assert_eq!(
                    existing.id, winner.id,
                    "round {round}: the conflict carries the winner"
                );
            }
            other => panic!(
                "round {round}: the losing withdrawal must be IdempotencyConflict, got \
                 {other:?}. An Internal here is the non-arbiter PRIMARY KEY collision \
                 this test exists for; a Transient is the same collision classified \
                 differently"
            ),
        }

        let withdrawals: i64 =
            sqlx::query_scalar("SELECT count(*) FROM usage_records WHERE invalidates = $1")
                .bind(target.id)
                .fetch_one(&h.pool)
                .await
                .expect("count withdrawals");
        assert_eq!(
            withdrawals, 1,
            "round {round}: one stored withdrawal, whichever side won"
        );
    }
}

// ---------------------------------------------------------------------------
// The bind sequence
// ---------------------------------------------------------------------------

/// Every column a batch insert writes reads back carrying the value it was
/// given.
///
/// **This is the test that asks about the `.bind()` sequence directly**, and on
/// the batch path it is the only one that observes it other than through a side
/// effect — the module header carries the measurement.
/// `InsertColumns`' field order reaches the DDL through those binds and nothing
/// else does: swap two binds of the same SQL type — `resource_id` and
/// `resource_type` are both `text NOT NULL` — and `PostgreSQL` accepts the row,
/// `InsertColumns::build`'s field-by-field test still passes, and
/// `migration_probe` says nothing, because every constant it checks is still
/// correct.
///
/// So every same-typed column here carries a value distinguishable from every
/// other column of that type, and each is read back **by name** rather than by
/// position. The two rows are written in one `create_batch` because the batch
/// path is where every bind of `INSERT_COLUMNS` is an array: a transposition
/// there is one array bound to the wrong column, which is the harder case, and
/// the invalidation pair (`invalidates`, `reason_code`) needs a target to point
/// at. The number of binds is not written out - it is one per column of that
/// constant, and it moves whenever the ledger's written columns do.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_row_written_through_the_batch_insert_reads_back_column_for_column() {
    let (h, store) = setup().await;
    let meter = common::meter(common::VCPU_METER);
    let tenant = Uuid::from_u128(0x00B1_9DDE);

    let window_start = common::fixture_window_start();
    // Distinct from the start, so a transposition of the two `timestamptz`
    // binds is visible rather than a no-op. The ordering CHECK would also catch
    // a straight swap; a test that relies on the constraint to notice is not
    // observing the bind sequence, so both are asserted by value below.
    let window_end = common::fixture_window_end();

    let mut target = common::entry_over(
        &meter,
        tenant,
        "bind-order-target",
        Decimal::new(12_345, 3),
        window_start,
        window_end,
    );
    target.resource_ref =
        ResourceRef::new("resource-id-value", "resource-type-value").expect("valid resource");
    target.subject_ref = Some(
        SubjectRef::new("subject-id-value", Some("subject-type-value")).expect("valid subject"),
    );
    target.origin = RecordOrigin::Backfill;
    target.metadata.insert(
        MetadataKey::new("region").expect("valid metadata key"),
        "metadata-value".to_owned(),
    );
    // Re-derive: `entry_over` stamped the identity from the key it was handed,
    // and none of the fields set above is an input to it, so the id still
    // stands. Asserted rather than assumed, because the whole point of this
    // test is that a value ends up under the column it belongs to.
    assert_eq!(
        target.id,
        usage_collector_sdk::derive_usage_record_id(
            tenant,
            &meter,
            &IdempotencyKey::new("bind-order-target").expect("valid key"),
            window_start,
            window_end,
            EntryType::Record,
        )
    );

    // A faithful withdrawal: it repeats its target's idempotency key and
    // covered period, so the two rows agree on five of the six identity
    // components and depart in the entry type alone. `rederive` restamps the
    // identity over that sixth input, which is what keeps the pair two rows
    // rather than one. Its attribution and metadata are deliberately left at
    // the fixture defaults rather than copied from the target, so a `text[]`
    // bind transposed between the two rows still shows up by value below.
    let withdrawal = common::rederive(UsageRecord {
        invalidation: Some(Invalidation {
            target: target.id,
            reason: ReasonCode::new("late_correction").expect("valid reason code"),
        }),
        ..common::entry_over(
            &meter,
            tenant,
            "bind-order-target",
            target.quantity.as_decimal(),
            window_start,
            window_end,
        )
    });
    assert_ne!(
        withdrawal.id, target.id,
        "the entry type is the sixth identity input, so the pair are two rows"
    );

    let results = store
        .create_batch(vec![target.clone(), withdrawal.clone()])
        .await
        .expect("batch ok");
    assert!(results[0].is_ok() && results[1].is_ok(), "{results:?}");

    // Read every insertable column back by name.
    let row: Raw = sqlx::query_as(RAW_SELECT_SQL)
        .bind(target.id)
        .fetch_one(&h.pool)
        .await
        .expect("read the measurement back");
    assert_eq!(row.id, target.id, "id");
    assert_eq!(row.tenant_id, tenant, "tenant_id");
    assert_eq!(row.gts_type_id, common::VCPU_METER, "gts_type_id");
    assert_eq!(row.quantity, Decimal::new(12_345, 3), "quantity");
    assert_eq!(row.window_start, window_start, "window_start");
    assert_eq!(row.window_end, window_end, "window_end");
    assert_eq!(row.resource_id, "resource-id-value", "resource_id");
    assert_eq!(row.resource_type, "resource-type-value", "resource_type");
    assert_eq!(
        row.subject_id.as_deref(),
        Some("subject-id-value"),
        "subject_id"
    );
    assert_eq!(
        row.subject_type.as_deref(),
        Some("subject-type-value"),
        "subject_type"
    );
    assert_eq!(row.idempotency_key, "bind-order-target", "idempotency_key");
    assert_eq!(row.invalidates, None, "invalidates");
    assert_eq!(row.reason_code, None, "reason_code");
    assert_eq!(row.origin, "backfill", "origin");
    assert_eq!(
        row.metadata,
        serde_json::json!({ "region": "metadata-value" }),
        "metadata"
    );
    assert_eq!(row.entry_type, "record", "entry_type");

    let w: Raw = sqlx::query_as(RAW_SELECT_SQL)
        .bind(withdrawal.id)
        .fetch_one(&h.pool)
        .await
        .expect("read the withdrawal back");
    assert_eq!(w.invalidates, Some(target.id), "invalidates");
    assert_eq!(
        w.reason_code.as_deref(),
        Some("late_correction"),
        "reason_code"
    );
    assert_eq!(
        w.idempotency_key, "bind-order-target",
        "idempotency_key: a withdrawal repeats its target's"
    );
    assert_eq!(w.origin, "live", "origin");
    assert_eq!(w.subject_id, None, "subject_id");
    assert_eq!(w.subject_type, None, "subject_type");
    assert_eq!(
        w.metadata,
        JsonValue::Object(serde_json::Map::new()),
        "metadata"
    );
    assert_eq!(w.entry_type, "invalidation", "entry_type");
    // The pair's order within the batch used to be readable off the ledger,
    // through the acceptance sequence `entry_type_rank` decided. It is not any
    // more, and deliberately: both rows were written by one transaction, so they
    // share one `xact_id` and the ledger records nothing that separates them.
    // `record_store_tests`' `plan_batch` assertion is where the rank's direction
    // is pinned now - in process, over the comparator, which is the only place
    // the property still exists.

    // And the same through the single-row insert, whose binds are a separate
    // sequence over the same columns, with the same hazard.
    let single = {
        let mut r = common::entry_over(
            &meter,
            tenant,
            "bind-order-single",
            Decimal::new(-42, 1),
            window_start,
            window_end,
        );
        r.resource_ref =
            ResourceRef::new("single-resource-id", "single-resource-type").expect("valid resource");
        r.subject_ref =
            Some(SubjectRef::new("single-subject-id", Some("single-subject-type")).expect("ok"));
        r.metadata.insert(
            MetadataKey::new("zone").expect("valid metadata key"),
            "single-metadata".to_owned(),
        );
        r
    };
    store.create(single.clone()).await.expect("single insert");
    let s: Raw = sqlx::query_as(RAW_SELECT_SQL)
        .bind(single.id)
        .fetch_one(&h.pool)
        .await
        .expect("read the single-path row back");
    assert_eq!(s.resource_id, "single-resource-id", "resource_id");
    assert_eq!(s.resource_type, "single-resource-type", "resource_type");
    assert_eq!(
        s.subject_id.as_deref(),
        Some("single-subject-id"),
        "subject_id"
    );
    assert_eq!(
        s.subject_type.as_deref(),
        Some("single-subject-type"),
        "subject_type"
    );
    assert_eq!(s.idempotency_key, "bind-order-single", "idempotency_key");
    assert_eq!(s.gts_type_id, common::VCPU_METER, "gts_type_id");
    assert_eq!(s.quantity, Decimal::new(-42, 1), "quantity");
    assert_eq!(s.window_start, window_start, "window_start");
    assert_eq!(s.window_end, window_end, "window_end");
    assert_eq!(
        s.metadata,
        serde_json::json!({ "zone": "single-metadata" }),
        "metadata"
    );
}

/// The metadata map survives a round trip through `jsonb` on both write paths.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn metadata_round_trips_through_the_model() {
    let (_h, store) = setup().await;
    let meter = common::meter(common::VCPU_METER);
    let tenant = Uuid::from_u128(0x1_ADA7);
    let scope = common::tenant_scope(tenant);

    let mut rec = common::entry(&meter, tenant, "idem-meta", Decimal::ONE);
    let mut expected = BTreeMap::new();
    for (k, v) in [("region", "eu-west-1"), ("tier", "gold")] {
        let key = MetadataKey::new(k).expect("valid metadata key");
        rec.metadata.insert(key.clone(), v.to_owned());
        expected.insert(key, v.to_owned());
    }
    let id = rec.id;
    store.create(rec).await.expect("create");

    let got = store.get(id, &scope).await.expect("get");
    assert_eq!(got.metadata, expected);
}
