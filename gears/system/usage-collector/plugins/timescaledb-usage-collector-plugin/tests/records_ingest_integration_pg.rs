#![cfg(feature = "postgres")]
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]
//! `PgRecordStore` ingest against a live `TimescaleDB`. Requires Docker.
//!
//! The behavioural home for the write path: dedup (absorb / conflict), the
//! per-scope acceptance sequence, the at-most-one-invalidation guarantee in all
//! three shapes it can be broken in (a later call, one batch, two concurrent
//! calls), and per-row batch outcomes aligned with input order.
//!
//! Two things here exist nowhere else in the crate:
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
//!   * **Batch path** (`insert_records_on_conflict`): all 214 unit tests stay
//!     green, and so does `contract_conformance_pg`. Three tests red, all in
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
//! * **The two at-most-one rejection counters.** Both sit on paths that need a
//!   live backend. The instruments and their descriptions are pinned in
//!   `metrics_tests`; the call sites are observed here.

mod common;

use std::collections::BTreeMap;
use std::sync::Arc;

use opentelemetry::metrics::MeterProvider as _;
use opentelemetry_sdk::metrics::data::{AggregatedMetrics, MetricData};
use opentelemetry_sdk::metrics::{InMemoryMetricExporter, PeriodicReader, SdkMeterProvider};
use rust_decimal::Decimal;
use serde_json::Value as JsonValue;
use time::{Duration, OffsetDateTime};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use usage_collector_sdk::{
    IdempotencyKey, Invalidation, MetadataKey, ReasonCode, RecordOrigin, ResourceRef, SubjectRef,
    UsageCollectorPluginError, UsageRecord,
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
    "uc_timescaledb_invalidation_rejected_statements_total",
    "uc_timescaledb_invalidation_rejected_rows_total",
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
    value: Decimal,
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
    acceptance_sequence: i64,
    metadata: JsonValue,
    entry_type: String,
}

/// The `SELECT` that fills a [`Raw`]: every column an insert writes, plus the
/// generated `entry_type`, named explicitly and read back by name.
const RAW_SELECT_SQL: &str = "SELECT id, tenant_id, gts_type_id, value, window_start, window_end, resource_id, \
     resource_type, subject_id, subject_type, idempotency_key, invalidates, reason_code, \
     origin, acceptance_sequence, metadata, entry_type FROM usage_records WHERE id = $1";

/// The acceptance sequences stored for one scope, in insertion order.
async fn sequences_for(pool: &sqlx::PgPool, tenant: Uuid, meter: &str) -> Vec<i64> {
    sqlx::query_scalar(
        "SELECT acceptance_sequence FROM usage_records \
         WHERE tenant_id = $1 AND gts_type_id = $2 ORDER BY acceptance_sequence",
    )
    .bind(tenant)
    .bind(meter)
    .fetch_all(pool)
    .await
    .expect("acceptance sequence query")
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
    assert_eq!(stored.value, record.value);
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
    assert_eq!(second.value, record.value);

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

    // The absorbed retry consumed an acceptance sequence it did not store, and
    // that is permitted: the obligation is monotonicity, not density. What is
    // asserted is that it did not store a *second* one.
    assert_eq!(
        sequences_for(&h.pool, tenant, common::VCPU_METER)
            .await
            .len(),
        1
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

    // Same five dedup inputs, different quantity: canonically unequal.
    let divergent = common::entry(&meter, tenant, "idem-conflict", Decimal::new(2, 0));
    assert_eq!(
        divergent.id, first.id,
        "the two share all five dedup-identity inputs, so they share an identifier"
    );

    let err = store
        .create(divergent)
        .await
        .expect_err("a divergent same-key write must be refused");
    match err {
        UsageCollectorPluginError::IdempotencyConflict {
            idempotency_key,
            existing_id,
        } => {
            assert_eq!(idempotency_key, "idem-conflict");
            assert_eq!(existing_id, first.id);
        }
        other => panic!("expected IdempotencyConflict, got {other:?}"),
    }

    let stored_value: Decimal = sqlx::query_scalar("SELECT value FROM usage_records WHERE id = $1")
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
// The acceptance sequence
// ---------------------------------------------------------------------------

/// Strictly monotonic per `(tenant_id, gts_type_id)`, and two scopes do not
/// share a sequence.
///
/// Both halves matter and neither implies the other. A single global counter
/// would satisfy monotonicity within each scope while making the second scope's
/// first value depend on the first scope's traffic — which is what makes feed
/// order per scope deterministic, and what a global `SEQUENCE` would cost. The
/// schema comment says why a Postgres `SEQUENCE` cannot be used: per-scope
/// monotonicity would need one sequence per `(tenant, meter)`, i.e. unbounded
/// DDL driven by tenant data.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_acceptance_sequence_is_strictly_monotonic_per_scope_and_not_shared() {
    let (h, store) = setup().await;
    let vcpu = common::meter(common::VCPU_METER);
    let gb = common::meter(common::GB_METER);
    let tenant_a = Uuid::from_u128(0x5EA1);
    let tenant_b = Uuid::from_u128(0x5EA2);

    // Interleave the three scopes so a shared counter would show up as gaps in
    // each of them rather than as three independent runs.
    for i in 0..4_i64 {
        let start = common::fixture_window_start() + Duration::hours(i);
        let end = common::fixture_window_end() + Duration::hours(i);
        for (tenant, meter) in [(tenant_a, &vcpu), (tenant_a, &gb), (tenant_b, &vcpu)] {
            let rec = common::entry_over(
                meter,
                tenant,
                &format!("idem-seq-{i}"),
                Decimal::from(i + 1),
                start,
                end,
            );
            store.create(rec).await.expect("create");
        }
    }

    for (tenant, meter, label) in [
        (tenant_a, common::VCPU_METER, "tenant A / vcpu"),
        (tenant_a, common::GB_METER, "tenant A / gb"),
        (tenant_b, common::VCPU_METER, "tenant B / vcpu"),
    ] {
        let seqs = sequences_for(&h.pool, tenant, meter).await;
        assert_eq!(seqs.len(), 4, "{label}: four entries");
        assert!(
            seqs.windows(2).all(|w| w[0] < w[1]),
            "{label}: acceptance_sequence must be strictly increasing, got {seqs:?}"
        );
        assert_eq!(
            seqs,
            vec![1, 2, 3, 4],
            "{label}: each scope counts from its own start - a value here that reflects \
             another scope's traffic means the counter is shared"
        );
    }
}

// ---------------------------------------------------------------------------
// At most one invalidation per entry
// ---------------------------------------------------------------------------

/// A second withdrawal of one target, arriving in a **later call**, is refused
/// by `usage_records_one_invalidation_uniq` and named.
///
/// This is also where `uc_timescaledb_invalidation_rejected_statements_total`'s
/// call site is observed: the index aborts the statement, `map_insert_error`
/// classifies it, and the increment happens there — on a path no unit test can
/// reach, because it needs the index to fire.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_second_withdrawal_in_a_later_call_is_already_invalidated_and_counted() {
    let (h, store, provider, exporter) = setup_metered().await;
    let meter = common::meter(common::VCPU_METER);
    let tenant = Uuid::from_u128(0x1_A710);

    let target = common::entry(&meter, tenant, "idem-target", Decimal::new(10, 0));
    let target = store.create(target).await.expect("create the target");

    let first = common::withdrawal_of(&target, "idem-w1");
    let first = store
        .create(first)
        .await
        .expect("the first withdrawal is accepted");

    let second = common::withdrawal_of(&target, "idem-w2");
    let err = store
        .create(second)
        .await
        .expect_err("a second withdrawal of one target must be refused");
    match err {
        UsageCollectorPluginError::AlreadyInvalidated { id, invalidated_by } => {
            assert_eq!(
                id, target.id,
                "the refusal names the entry already withdrawn"
            );
            assert_eq!(
                invalidated_by, first.id,
                "and the withdrawal that already withdrew it"
            );
        }
        other => panic!("expected AlreadyInvalidated, got {other:?}"),
    }

    let withdrawals: i64 =
        sqlx::query_scalar("SELECT count(*) FROM usage_records WHERE invalidates = $1")
            .bind(target.id)
            .fetch_one(&h.pool)
            .await
            .expect("count withdrawals");
    assert_eq!(withdrawals, 1, "exactly one withdrawal is admitted");

    provider.force_flush().expect("flush metrics");
    assert_eq!(
        counter_sum(
            &exporter,
            "uc_timescaledb_invalidation_rejected_statements_total"
        ),
        1,
        "the index's refusal of a whole statement is counted once, in map_insert_error"
    );
    assert_eq!(
        counter_sum(&exporter, "uc_timescaledb_invalidation_rejected_rows_total"),
        0,
        "the in-batch counter is a different instrument and must not move here - the two \
         count different units and are deliberately not summable"
    );
    assert_eq!(
        counter_sum(&exporter, "uc_timescaledb_invalidations_total"),
        1,
        "one withdrawal was accepted"
    );
}

/// The same rule when both withdrawals arrive in **one `create_batch`** — the
/// case the SPI singles out, because the two would otherwise land in one
/// multi-row `INSERT` where the index rejects the whole statement and every
/// other row of the batch loses its outcome.
///
/// So the outcome asserted is per-row: the first withdrawal is accepted, the
/// second is `AlreadyInvalidated`, and the unrelated row beside them is
/// unaffected. This is also where
/// `uc_timescaledb_invalidation_rejected_rows_total`'s call site is observed —
/// **rows**, against the sibling instrument's **statements**.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_withdrawals_of_one_target_in_one_batch_yield_exactly_one_acceptance() {
    let (h, store, provider, exporter) = setup_metered().await;
    let meter = common::meter(common::VCPU_METER);
    let tenant = Uuid::from_u128(0x1_A711);

    let target = common::entry(&meter, tenant, "idem-target", Decimal::new(10, 0));
    let target = store.create(target).await.expect("create the target");

    let w1 = common::withdrawal_of(&target, "idem-w1");
    let w2 = common::withdrawal_of(&target, "idem-w2");
    let unrelated = common::entry(&meter, tenant, "idem-unrelated", Decimal::new(3, 0));

    let results = store
        .create_batch(vec![w1.clone(), w2.clone(), unrelated.clone()])
        .await
        .expect("the batch as a whole succeeds and answers per row");

    assert_eq!(
        results.len(),
        3,
        "one outcome per input row, in input order"
    );
    assert_eq!(
        results[0]
            .as_ref()
            .expect("the first withdrawal is accepted")
            .id,
        w1.id
    );
    match results[1].as_ref() {
        Err(UsageCollectorPluginError::AlreadyInvalidated { id, invalidated_by }) => {
            assert_eq!(*id, target.id);
            assert_eq!(
                *invalidated_by, w1.id,
                "the refusal names the earlier row of this same batch"
            );
        }
        other => panic!("row 1 must be AlreadyInvalidated, got {other:?}"),
    }
    assert_eq!(
        results[2]
            .as_ref()
            .expect("the unrelated row keeps its outcome")
            .id,
        unrelated.id,
        "a refused withdrawal must not cost the rest of the batch their outcomes"
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
        counter_sum(&exporter, "uc_timescaledb_invalidation_rejected_rows_total"),
        1,
        "one refused row, counted in resolve_batch"
    );
    assert_eq!(
        counter_sum(
            &exporter,
            "uc_timescaledb_invalidation_rejected_statements_total"
        ),
        0,
        "no statement was refused: the in-batch pre-rejection is what keeps the outcome \
         per-row, so the index never fired"
    );
}

/// Two `create` calls withdrawing one target, issued concurrently **in one
/// scope**: exactly one is accepted.
///
/// **This one does not race the index, and the reason is worth stating rather
/// than leaving as an unearned concurrency claim.** `create_inner` claims the
/// entry's `acceptance_sequence` as step 1, inside the transaction that will
/// insert it, and both withdrawals below share a `(tenant_id, gts_type_id)`.
/// So the second blocks on that counter row until the first commits and only
/// then reaches `usage_records_one_invalidation_uniq` — the calls are
/// concurrent at the caller and serialised at the backend. A hypothetical
/// pre-read implementation would pass this test, because there is nothing left
/// to race.
///
/// What it does assert, and what is worth asserting, is that the serialised
/// second writer is refused rather than admitted, and refused by name. The
/// case that genuinely races the index is the next test, which crosses scopes
/// so the counter cannot serialise it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_withdrawals_of_one_target_in_one_scope_admit_exactly_one() {
    let (h, store) = setup().await;
    let meter = common::meter(common::VCPU_METER);
    let tenant = Uuid::from_u128(0x1_A712);

    let target = common::entry(&meter, tenant, "idem-target", Decimal::new(10, 0));
    let target = store.create(target).await.expect("create the target");

    let a = common::withdrawal_of(&target, "idem-race-a");
    let b = common::withdrawal_of(&target, "idem-race-b");
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
        "exactly one concurrent withdrawal of one target may be admitted; got a={ra:?} b={rb:?}"
    );
    let loser = if ra.is_ok() { &rb } else { &ra };
    match loser {
        Err(UsageCollectorPluginError::AlreadyInvalidated { id, .. }) => {
            assert_eq!(*id, target.id);
        }
        other => panic!("the losing withdrawal must be AlreadyInvalidated, got {other:?}"),
    }

    let withdrawals: i64 =
        sqlx::query_scalar("SELECT count(*) FROM usage_records WHERE invalidates = $1")
            .bind(target.id)
            .fetch_one(&h.pool)
            .await
            .expect("count withdrawals");
    assert_eq!(withdrawals, 1, "and the ledger holds one, not two");
}

/// Two withdrawals of one target from **different `(tenant_id, gts_type_id)`
/// scopes**, concurrently: exactly one is accepted, and the index is the only
/// thing that can have decided it.
///
/// **This is the test the atomicity obligation is for.** The SPI says the
/// refusal must be atomic with the entry it admits — a read followed by a write
/// will not do — and the test above cannot demonstrate that, because the
/// per-scope acceptance-sequence row serialises two same-scope writers before
/// either reaches the index. Crossing scopes removes that serialisation: the
/// two transactions claim different counter rows, proceed concurrently, and
/// meet for the first time at `usage_records_one_invalidation_uniq`, which is
/// **not** scope-partitioned — it is over `(invalidates, window_end)` alone.
/// A pre-read implementation fails here, and passes the test above.
///
/// **The shape is deliberately one the Ingestion Gateway would never emit.** A
/// withdrawal naming a target in another tenant's scope is not a request any
/// caller should be able to make, and the gateway is what stops it. This test
/// reaches the SPI directly precisely because the store's own obligation is to
/// the index rather than to the gateway's shape rules: the store must not admit
/// two withdrawals of one entry whatever route they arrive by, and this is the
/// only route on which both can be in flight at once.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_withdrawals_of_one_target_from_different_scopes_race_on_the_index_alone() {
    let (h, store) = setup().await;
    let vcpu = common::meter(common::VCPU_METER);
    let gb = common::meter(common::GB_METER);
    let owner = Uuid::from_u128(0x1_A713);
    let other = Uuid::from_u128(0x1_A714);

    let target = common::entry(&vcpu, owner, "idem-target", Decimal::new(10, 0));
    let target = store.create(target).await.expect("create the target");

    // Two withdrawals of that one entry, in two scopes that share neither the
    // tenant nor the meter — so neither the counter row nor anything else
    // serialises them. Both copy the target's `window_end`, which is what puts
    // them in the same index slot.
    let mut a = common::withdrawal_of(&target, "idem-race-a");
    a.tenant_id = owner;
    let a = common::rederive(a);
    let mut b = common::withdrawal_of(&target, "idem-race-b");
    b.tenant_id = other;
    b.gts_type_id = gb.clone();
    let b = common::rederive(b);
    assert_ne!(
        (a.tenant_id, a.gts_type_id.as_str()),
        (b.tenant_id, b.gts_type_id.as_str()),
        "the two withdrawals must sit in different acceptance-sequence scopes, or the \
         counter row serialises them and the index is never asked"
    );
    assert_eq!(a.window_end, b.window_end, "same index slot");

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
        "exactly one withdrawal of one target may be admitted, across scopes as within \
         one; got a={ra:?} b={rb:?}"
    );
    let loser = if ra.is_ok() { &rb } else { &ra };
    match loser {
        Err(UsageCollectorPluginError::AlreadyInvalidated { id, .. }) => {
            assert_eq!(*id, target.id);
        }
        // The index refused it and the diagnostic read could not name the
        // winner in time; still a refusal, and still not an acceptance.
        Err(UsageCollectorPluginError::Transient { .. }) => {}
        other => panic!("the losing withdrawal must be refused, got {other:?}"),
    }

    let withdrawals: i64 =
        sqlx::query_scalar("SELECT count(*) FROM usage_records WHERE invalidates = $1")
            .bind(target.id)
            .fetch_one(&h.pool)
            .await
            .expect("count withdrawals");
    assert_eq!(withdrawals, 1, "and the ledger holds one, not two");
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
        Err(UsageCollectorPluginError::IdempotencyConflict { existing_id, .. }) => {
            assert_eq!(*existing_id, seeded.id);
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
        Err(UsageCollectorPluginError::IdempotencyConflict { existing_id, .. }) => {
            assert_eq!(*existing_id, first.id);
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
    let sequences_before = sequences_for(&h.pool, tenant, common::VCPU_METER).await;
    assert_eq!(
        sequences_before,
        vec![1, 2, 3],
        "three seeded entries, one block each"
    );

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
                existing_id,
            }) => {
                assert_eq!(
                    idempotency_key,
                    &format!("all-conf-{i}"),
                    "row {i}'s conflict must name row {i}'s key"
                );
                assert_eq!(
                    *existing_id, seeded[i].id,
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
        "SELECT value FROM usage_records WHERE tenant_id = $1 ORDER BY window_end",
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

    // Nor did the refused batch leave an acceptance sequence behind. It claimed
    // a block - the claim happens before the insert and cannot know the insert
    // will win nothing - and rolling the transaction back is what returns it.
    // Gaps are permitted by the contract, so this is not "the values are
    // dense"; it is "no *stored* entry acquired a new one", which is the part a
    // failed batch could get wrong.
    assert_eq!(
        sequences_for(&h.pool, tenant, common::VCPU_METER).await,
        sequences_before,
        "an all-conflict batch must store no acceptance sequence of its own"
    );
}

/// A batch of distinct entries all insert, and the sequence they were assigned
/// is strictly monotonic across the whole block.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_hundred_distinct_entries_all_insert_under_one_monotonic_block() {
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

    // `sequences_for` reads with `ORDER BY acceptance_sequence`, so a
    // `windows(2)` monotonicity check can only ever fail on a duplicate - it is
    // very nearly unfalsifiable as an assertion. The scope is fresh and this
    // batch claimed one block, so the values are knowable exactly, and naming
    // them is what makes a block claimed twice, claimed short, or expanded with
    // the off-by-one in `sequence_block` visible.
    let seqs = sequences_for(&h.pool, tenant, common::VCPU_METER).await;
    assert_eq!(
        seqs,
        (1..=100).collect::<Vec<i64>>(),
        "one batch into a fresh scope claims one contiguous block, 1..=100"
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
/// reaches it.** `claim_batch_sequences` walks `plan.reps` and takes one
/// `usage_acceptance_sequence` row lock per `(tenant_id, gts_type_id)` run, in
/// the order the runs appear. Sorted by dedup key, whose first two components
/// *are* the scope, every batch in the process takes those locks in one global
/// order. Unsorted, batch A takes vcpu then gb while batch B takes gb then
/// vcpu - the ABBA deadlock, which `PostgreSQL` breaks by aborting a victim
/// after `deadlock_timeout`.
///
/// **The assertion is on the retry counter, not on the outcome**, because the
/// outcome hides the defect: `create_batch` wraps itself in a bounded retry, so
/// a deadlock victim re-runs and succeeds, and every row still comes back
/// `Ok`. `uc_timescaledb_batch_retries_total` is what distinguishes "took the
/// locks in one order" from "deadlocked and recovered". It must be **zero**:
/// the sort is supposed to make the deadlock unreachable, not survivable.
///
/// The counter is a slightly wider oracle than the property, and the failure
/// message says so rather than over-claiming: `is_retryable_batch_error` admits
/// **any** `Transient`, so this really asserts "no transient at all". The one
/// realistic alternative on a loaded runner is `55P03 lock_not_available`
/// waiting on that same counter row, which needs the whole `statement_timeout`
/// to elapse first - unlikely, but it would retry identically, and a message
/// confidently naming a deadlock that did not happen is worse than a wider one.
///
/// Several rounds rather than one, because a deadlock needs the two
/// transactions to interleave and one round can miss.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_batches_taking_two_scopes_in_opposite_orders_never_deadlock() {
    let (h, store, provider, exporter) = setup_metered().await;
    let vcpu = common::meter(common::VCPU_METER);
    let gb = common::meter(common::GB_METER);
    let tenant = Uuid::from_u128(0x0DEA_D10D);

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

    for round in 0..6_i64 {
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
    assert_eq!(
        counter_sum(&exporter, "uc_timescaledb_batch_retries_total"),
        0,
        "a batch was retried. The expected cause is the one this test exists for: two \
         concurrent batches took the per-scope acceptance-sequence row locks in \
         different orders and one was aborted as a deadlock victim, which plan_batch's \
         reps.sort_by is supposed to make unreachable rather than survivable. \
         `is_retryable_batch_error` admits any Transient, though, so check the run's \
         logs before concluding that: a 55P03 lock_not_available on the counter row \
         retries identically, and on a loaded box that is the one other way to get \
         here - it needs the whole statement_timeout to elapse first, so it is \
         unlikely, not impossible."
    );

    // And the ledger holds one row per key, in both scopes.
    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM usage_records WHERE tenant_id = $1")
        .bind(tenant)
        .fetch_one(&h.pool)
        .await
        .expect("count");
    assert_eq!(rows, 48, "6 rounds x 4 keys x 2 scopes, each written once");
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
/// path is where the sixteen binds are arrays: a transposition there is one
/// array bound to the wrong column, which is the harder case, and the
/// invalidation pair (`invalidates`, `reason_code`) needs a target to point at.
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
        )
    );

    let withdrawal = UsageRecord {
        invalidation: Some(Invalidation {
            target: target.id,
            reason: ReasonCode::new("late_correction").expect("valid reason code"),
        }),
        ..common::entry_over(
            &meter,
            tenant,
            "bind-order-withdrawal",
            target.value,
            window_start,
            window_end,
        )
    };

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
    assert_eq!(row.value, Decimal::new(12_345, 3), "value");
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
    assert!(row.acceptance_sequence > 0, "acceptance_sequence");
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
        w.idempotency_key, "bind-order-withdrawal",
        "idempotency_key"
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
    assert!(
        w.acceptance_sequence > row.acceptance_sequence,
        "the withdrawal was accepted after its target"
    );

    // And the same through the single-row insert, whose sixteen binds are a
    // separate sequence with the same hazard.
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
    assert_eq!(s.value, Decimal::new(-42, 1), "value");
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
