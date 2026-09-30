#![cfg(feature = "postgres")]
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]
//! `StorageAdapter::read_feed_page` against a live `TimescaleDB`. Requires
//! Docker.
//!
//! This is the plugin's own behavioural coverage of the page protocol
//! (`docs/DESIGN.md` §3.6 `cpt-cf-uc-plugin-seq-feed-page`), alongside the
//! DESIGN section 3.3 contract suite's more abstract obligations
//! (`tests/contract_conformance_pg.rs`). Every read goes through
//! [`StorageAdapter::read_feed_page`] rather than `PgRecordStore::feed_page`
//! directly, so the position codec (`src/infra/storage/feed_position.rs`) and
//! the limit / start-mode validation the adapter owns are exercised along
//! with the store.
//!
//! Each test starts its own container ([`common::start_backend`]), so no test
//! here shares a ledger with another and tenants and idempotency keys are
//! reused freely across them.

mod common;

use std::sync::Arc;

use opentelemetry::metrics::MeterProvider as _;
use opentelemetry_sdk::metrics::data::{AggregatedMetrics, MetricData};
use opentelemetry_sdk::metrics::{InMemoryMetricExporter, PeriodicReader, SdkMeterProvider};
use rust_decimal::Decimal;
use tokio_util::sync::CancellationToken;
use toolkit_odata::ast;
use uuid::Uuid;

use usage_collector_sdk::{
    FeedPosition, FeedStart, UsageCollectorPluginError, UsageCollectorPluginV1,
};

use timescaledb_usage_collector_plugin::domain::adapter::StorageAdapter;
use timescaledb_usage_collector_plugin::domain::ports::RecordStore;
use timescaledb_usage_collector_plugin::infra::metrics::Metrics;
use timescaledb_usage_collector_plugin::infra::storage::feed_horizon::FeedHorizonMonitor;
use timescaledb_usage_collector_plugin::infra::storage::feed_position::encode_position;
use timescaledb_usage_collector_plugin::infra::storage::query::MAX_PAGE_SIZE;
use timescaledb_usage_collector_plugin::infra::storage::record_store::PgRecordStore;
use timescaledb_usage_collector_plugin::infra::storage::retention_sweep::RAISE_MARKS_SQL;

/// Like [`common::start_backend`], but the metric inventory writes to a
/// **local** in-memory exporter instead of the process-global provider, so a
/// test can read a counter back — the same reason `metrics_tests` and
/// `records_ingest_integration_pg`'s `setup_metered` give: `Metrics::with_meter`
/// takes the meter explicitly, so the assertion never depends on global state
/// another test binary is also writing to.
async fn start_backend_metered() -> (
    common::TsHarness,
    StorageAdapter,
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
    let store: Arc<dyn RecordStore> = Arc::new(PgRecordStore::new(
        h.pool.clone(),
        metrics,
        CancellationToken::new(),
        h.cfg.feed_acceptance_slack_secs,
    ));
    (h, StorageAdapter::new(store), provider, exporter)
}

/// Every instrument name the meter exported, sorted. Mirrors
/// `metrics_tests::exported_names`; duplicated here rather than shared
/// because this file's exporter belongs to a separate integration-test
/// crate, not a sibling of `metrics.rs`'s own unit tests.
fn exported_names(exporter: &InMemoryMetricExporter) -> Vec<String> {
    let metrics = exporter.get_finished_metrics().expect("exported metrics");
    let mut names: Vec<String> = metrics
        .iter()
        .flat_map(opentelemetry_sdk::metrics::data::ResourceMetrics::scope_metrics)
        .flat_map(opentelemetry_sdk::metrics::data::ScopeMetrics::metrics)
        .map(|m| m.name().to_owned())
        .collect();
    names.sort();
    names.dedup();
    names
}

/// Total of the `u64` counter data points named `name`. 0 for an instrument
/// that exists and was never recorded, and 0 for one that does not exist —
/// the same ambiguity `records_ingest_integration_pg::counter_sum` documents;
/// this file only ever reads a counter this task's own code drives.
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

/// Total observation count across the `f64` Histogram data points named
/// `name`. Mirrors `metrics_tests::histogram_count`; duplicated here for the
/// same reason `exported_names`/`counter_sum` above are.
fn histogram_count(exporter: &InMemoryMetricExporter, name: &str) -> u64 {
    let metrics = exporter.get_finished_metrics().expect("exported metrics");
    for resource_metrics in &metrics {
        for scope_metrics in resource_metrics.scope_metrics() {
            for metric in scope_metrics.metrics() {
                if metric.name() == name
                    && let AggregatedMetrics::F64(MetricData::Histogram(h)) = metric.data()
                {
                    return h
                        .data_points()
                        .map(opentelemetry_sdk::metrics::data::HistogramDataPoint::count)
                        .sum();
                }
            }
        }
    }
    0
}

/// Raise a retention mark directly, via the sweep's own [`RAISE_MARKS_SQL`]
/// statement — the same "raise, never lower" write the production sweep
/// issues, bound over exactly one type.
///
/// Every caller of this helper is testing the page's *read* of a mark, not
/// the sweep's *write* of one (ruling D7): the sweep's write is pinned by
/// `retention_sweep_tests::the_mark_raise_only_ever_raises`, and what joins
/// the write to a real chunk drop is Task 5's driven dispatch. Calling this
/// directly is what lets a page-side test land before that drive exists,
/// without duplicating the sweep's own SQL.
async fn raise_mark(pool: &sqlx::PgPool, gts_type_id: &str, xact_id: u64, id: Uuid) {
    sqlx::query(RAISE_MARKS_SQL)
        .bind(vec![gts_type_id.to_owned()])
        .bind(vec![xact_id.to_string()])
        .bind(vec![id])
        .execute(pool)
        .await
        .expect("raise a retention mark directly");
}

// 1. A first read begins at the oldest entry and carries a continuation.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_first_read_begins_at_the_oldest_entry_and_carries_a_continuation() {
    let (_h, adapter) = common::start_backend().await;
    let meter = common::meter(common::VCPU_METER);
    let tenant = Uuid::from_u128(1);
    let scope = common::tenant_scope(tenant);
    let subscription = [meter.clone()];

    let first = common::entry(&meter, tenant, "feed-first", Decimal::ONE);
    let second = common::entry(&meter, tenant, "feed-second", Decimal::ONE);
    adapter
        .create_usage_record(first.clone())
        .await
        .expect("the first entry is accepted");
    adapter
        .create_usage_record(second.clone())
        .await
        .expect("the second entry is accepted");

    let page = adapter
        .read_feed_page(&subscription, &scope, FeedStart::Oldest, None, 10)
        .await
        .expect("a first read is served");

    assert_eq!(
        page.entries.first().map(|entry| entry.id),
        Some(first.id),
        "a first read begins at the oldest entry the subscription retains: {:?}",
        page.entries
            .iter()
            .map(|entry| entry.id)
            .collect::<Vec<_>>(),
    );
    assert!(
        page.next.is_some(),
        "a live read carries a continuation on every page"
    );
}

// 2. A first read over a meter nothing was written to is served, carries no
//    entry, and carries a continuation. <-- Review Focus 3
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_first_read_over_an_empty_meter_is_served_and_carries_a_head_cursor() {
    let (_h, adapter) = common::start_backend().await;
    let meter = common::meter(common::VCPU_METER);
    let tenant = Uuid::from_u128(1);
    let scope = common::tenant_scope(tenant);
    let subscription = [meter];

    let page = adapter
        .read_feed_page(&subscription, &scope, FeedStart::Oldest, None, 10)
        .await
        .expect("a subscription retaining nothing is served, never refused");

    assert!(
        page.entries.is_empty(),
        "nothing has ever been written to this meter: {:?}",
        page.entries
            .iter()
            .map(|entry| entry.id)
            .collect::<Vec<_>>(),
    );
    assert!(
        page.next.is_some(),
        "an empty page still carries a head cursor, never `None`"
    );
}

// 3. A subscription naming an empty meter beside a busy one delivers exactly
//    what the busy meter alone delivers. <-- Review Focus 3
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_empty_meter_named_beside_a_busy_one_changes_nothing() {
    let (_h, adapter) = common::start_backend().await;
    let busy = common::meter(common::VCPU_METER);
    let empty = common::meter(common::GB_METER);
    let tenant = Uuid::from_u128(1);
    let scope = common::tenant_scope(tenant);

    let entry = common::entry(&busy, tenant, "feed-busy", Decimal::ONE);
    adapter
        .create_usage_record(entry.clone())
        .await
        .expect("the entry is accepted");

    let alone = adapter
        .read_feed_page(
            std::slice::from_ref(&busy),
            &scope,
            FeedStart::Oldest,
            None,
            10,
        )
        .await
        .expect("the busy meter alone is served");
    let beside = adapter
        .read_feed_page(&[busy, empty], &scope, FeedStart::Oldest, None, 10)
        .await
        .expect("the busy meter beside an empty one is served");

    assert_eq!(
        alone.entries.iter().map(|r| r.id).collect::<Vec<_>>(),
        beside.entries.iter().map(|r| r.id).collect::<Vec<_>>(),
        "an empty meter named beside a busy one contributes nothing of its own"
    );
}

// 3b. A wholly empty subscription array behaves as it does today: an empty
// page, never an error. Thing to get right #4 of the lateral-join reshaping
// -- `unnest($1::text[])` of an empty array joins to nothing, the same as
// `gts_type_id = ANY('{}')` did before it, and this is the one behaviour the
// per-type `UNION ALL` alternative would have needed an explicit guard for.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_wholly_empty_subscription_is_served_as_an_empty_page_not_an_error() {
    let (_h, adapter) = common::start_backend().await;
    let meter = common::meter(common::VCPU_METER);
    let tenant = Uuid::from_u128(1);
    let scope = common::tenant_scope(tenant);

    // Something exists in the ledger, so an empty result is provably the
    // subscription's own emptiness and not merely an empty database.
    adapter
        .create_usage_record(common::entry(
            &meter,
            tenant,
            "feed-unsubscribed",
            Decimal::ONE,
        ))
        .await
        .expect("the entry is accepted");

    let page = adapter
        .read_feed_page(&[], &scope, FeedStart::Oldest, None, 10)
        .await
        .expect("an empty subscription is served, never refused or errored");

    assert!(
        page.entries.is_empty(),
        "no type is subscribed, so nothing may be delivered: {:?}",
        page.entries
            .iter()
            .map(|entry| entry.id)
            .collect::<Vec<_>>(),
    );
    assert!(
        page.next.is_some(),
        "an empty page still carries a head cursor, never `None`, the same as any other \
         empty page"
    );
}

// 4. Two entries of one batch share an `xact_id` and are ordered by `id`.
//
// **Not built on `create_usage_records`.** `run_guarded_batch_write`'s own
// doc requires its representatives sorted by `entry_identity` before the
// write, for deadlock avoidance — so a batch's own rows are *always*
// physically inserted in id order regardless of the caller's order, and a
// tie built through it would match the `ORDER BY xact_id, id` tiebreak by
// coincidence even with the tiebreak deleted. Measured, not assumed: an
// earlier version of this test built its tie through `create_usage_records`
// and stayed green under `ORDER BY xact_id, id` mutated to `ORDER BY
// xact_id` alone — the mutation this test exists to catch. Two rows inserted
// directly, in one transaction, in the *reverse* of their id order, is what
// makes physical order and id order disagree, so only the tiebreak itself
// can produce the expected result.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_entries_sharing_a_transaction_id_are_ordered_by_id_not_insertion_order() {
    let (h, adapter) = common::start_backend().await;
    let meter = common::meter(common::VCPU_METER);
    let tenant = Uuid::from_u128(1);
    let scope = common::tenant_scope(tenant);
    let subscription = [meter.clone()];

    let e1 = common::entry(&meter, tenant, "feed-batch-1", Decimal::ONE);
    let e2 = common::entry(&meter, tenant, "feed-batch-2", Decimal::ONE);
    let mut expected = [e1.id, e2.id];
    expected.sort_unstable();
    // Insert the *greater* id first and the *lesser* second: the reverse of
    // the order the tiebreak must produce.
    let (first, second) = if e1.id == expected[1] {
        (&e1, &e2)
    } else {
        (&e2, &e1)
    };

    let mut tx = h.pool.begin().await.expect("begin the shared transaction");
    insert_raw_within(&mut tx, first).await;
    insert_raw_within(&mut tx, second).await;
    tx.commit()
        .await
        .expect("commit both rows under one transaction id");

    let x1 = common::xact_id_of(&h.pool, e1.id).await;
    let x2 = common::xact_id_of(&h.pool, e2.id).await;
    assert_eq!(x1, x2, "both rows were committed under one transaction id");

    let page = adapter
        .read_feed_page(&subscription, &scope, FeedStart::Oldest, None, 10)
        .await
        .expect("a first read is served");

    assert_eq!(
        page.entries.iter().map(|r| r.id).collect::<Vec<_>>(),
        expected.to_vec(),
        "two entries sharing an xact_id are ordered by id, the feed order's tiebreak, \
         even though they were inserted in the opposite order"
    );
}

// 4b. A cross-type `xact_id` tie, walked page by page to the head. The
// whole-slice review's owner-authorised statement reshaping
// (`query/feed.rs`'s lateral join, one iteration per subscribed type) merges
// each type's own ordered stream through an outer sort -- and every tie test
// before this one named exactly one meter, so none of them ever exercised
// that outer merge across more than one lateral iteration. This is the
// spike's own equivalence fixture (see
// `.superpowers/sdd/2026-09-27-usage-collector-spec-complete/
// spike-feed-page-index-order.md`, Finding 3), landed here since the suite
// had no cross-type version of the same-type tie test above.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_cross_type_xact_id_tie_walks_to_the_head_in_id_order() {
    let (h, adapter) = common::start_backend().await;
    let vcpu = common::meter(common::VCPU_METER);
    let gb = common::meter(common::GB_METER);
    let tenant = Uuid::from_u128(1);
    let scope = common::tenant_scope(tenant);
    let subscription = [vcpu.clone(), gb.clone()];

    // Uneven counts across the two types, each inserted through the
    // production write path so every one gets its own committed xact_id.
    let mut ids = Vec::new();
    for i in 0..7 {
        let e = adapter
            .create_usage_record(common::entry(
                &vcpu,
                tenant,
                &format!("cross-vcpu-{i}"),
                Decimal::ONE,
            ))
            .await
            .expect("vcpu entry accepted");
        ids.push(e.id);
    }
    for i in 0..5 {
        let e = adapter
            .create_usage_record(common::entry(
                &gb,
                tenant,
                &format!("cross-gb-{i}"),
                Decimal::ONE,
            ))
            .await
            .expect("gb entry accepted");
        ids.push(e.id);
    }

    // The planted tie: one VCPU row and one GB row, committed together so
    // they share one `xact_id`, with the *lesser* id inserted *second* -- the
    // reverse of the order only the `(xact_id, id)` tiebreak can recover, and
    // the two rows are of *different* types, which the same-type tie test
    // above cannot exercise.
    let t1 = common::entry(&vcpu, tenant, "cross-tie-vcpu", Decimal::ONE);
    let t2 = common::entry(&gb, tenant, "cross-tie-gb", Decimal::ONE);
    let (greater, lesser) = if t1.id > t2.id {
        (&t1, &t2)
    } else {
        (&t2, &t1)
    };
    let mut tx = h.pool.begin().await.expect("begin the shared transaction");
    insert_raw_within(&mut tx, greater).await;
    insert_raw_within(&mut tx, lesser).await;
    tx.commit()
        .await
        .expect("commit both cross-type rows under one transaction id");
    ids.push(t1.id);
    ids.push(t2.id);

    let x1 = common::xact_id_of(&h.pool, t1.id).await;
    let x2 = common::xact_id_of(&h.pool, t2.id).await;
    assert_eq!(
        x1, x2,
        "the planted pair shares one transaction id across two different types"
    );

    // The canonical order: every entry's own `(xact_id, id)`, read back
    // rather than assumed, since it is the database that stamps `xact_id`.
    let mut with_positions = Vec::new();
    for id in &ids {
        let xact_id = common::xact_id_of(&h.pool, *id).await;
        with_positions.push((xact_id, *id));
    }
    with_positions.sort_unstable();
    let expected: Vec<Uuid> = with_positions.into_iter().map(|(_, id)| id).collect();

    // Walked at a limit narrower than either type's own count, so the walk
    // crosses several pages and the outer merge over both lateral iterations
    // runs on every one of them, not just on one page wide enough to hide
    // behind.
    let mut delivered: Vec<Uuid> = Vec::new();
    let mut start = FeedStart::Oldest;
    for _ in 0..20 {
        let page = adapter
            .read_feed_page(&subscription, &scope, start.clone(), None, 3)
            .await
            .expect("a page is served");
        delivered.extend(page.entries.iter().map(|r| r.id));
        if page.entries.is_empty() || delivered.len() >= expected.len() {
            break;
        }
        start = FeedStart::After(page.next.expect("a live page carries a continuation"));
    }

    assert!(
        delivered.len() == expected.len(),
        "the walk must terminate having delivered every planted entry exactly once: \
         got {} of {}",
        delivered.len(),
        expected.len()
    );
    assert_eq!(
        delivered, expected,
        "a cross-type xact_id tie, and every other entry, arrives exactly once, in the \
         (xact_id, id) order, walked page by page across both subscribed types"
    );
}

// 5. A page filled to its limit continues from the last entry's position, and
//    following it delivers the rest exactly once.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_page_filled_to_its_limit_continues_and_the_rest_arrives_exactly_once() {
    let (_h, adapter) = common::start_backend().await;
    let meter = common::meter(common::VCPU_METER);
    let tenant = Uuid::from_u128(1);
    let scope = common::tenant_scope(tenant);
    let subscription = [meter.clone()];

    let mut ids = Vec::new();
    for i in 0..5 {
        let entry = common::entry(&meter, tenant, &format!("feed-limit-{i}"), Decimal::ONE);
        adapter
            .create_usage_record(entry.clone())
            .await
            .expect("the entry is accepted");
        ids.push(entry.id);
    }

    let first_page = adapter
        .read_feed_page(&subscription, &scope, FeedStart::Oldest, None, 2)
        .await
        .expect("a first read is served");
    assert_eq!(
        first_page.entries.len(),
        2,
        "a page filled to its limit carries exactly that many entries"
    );

    let mut delivered: Vec<Uuid> = first_page.entries.iter().map(|r| r.id).collect();
    let mut start = FeedStart::After(
        first_page
            .next
            .expect("a filled page still carries a continuation"),
    );
    for _ in 0..10 {
        if delivered.len() >= ids.len() {
            break;
        }
        let page = adapter
            .read_feed_page(&subscription, &scope, start.clone(), None, 2)
            .await
            .expect("a continuation is served");
        delivered.extend(page.entries.iter().map(|r| r.id));
        start = FeedStart::After(page.next.expect("a live page carries a continuation"));
    }

    assert_eq!(
        delivered, ids,
        "every entry arrives exactly once, in feed order, across as many pages as it takes"
    );
}

// 6. A bounded replay reaching its `until` carries no continuation, and the
//    same replay run twice delivers the same entries in the same order.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_bounded_replay_closes_at_its_until_and_repeats_identically() {
    let (_h, adapter) = common::start_backend().await;
    let meter = common::meter(common::VCPU_METER);
    let tenant = Uuid::from_u128(1);
    let scope = common::tenant_scope(tenant);
    let subscription = [meter.clone()];

    let e1 = common::entry(&meter, tenant, "feed-bounded-1", Decimal::ONE);
    let e2 = common::entry(&meter, tenant, "feed-bounded-2", Decimal::ONE);
    adapter
        .create_usage_record(e1.clone())
        .await
        .expect("the first entry is accepted");
    adapter
        .create_usage_record(e2.clone())
        .await
        .expect("the second entry is accepted");

    let page = adapter
        .read_feed_page(&subscription, &scope, FeedStart::Oldest, None, 10)
        .await
        .expect("a first read is served");
    assert_eq!(page.entries.len(), 2, "both entries settle on one page");
    let until = page.next.expect("a live page carries a continuation");

    for attempt in 0..2 {
        let replay = adapter
            .read_feed_page(
                &subscription,
                &scope,
                FeedStart::Oldest,
                Some(until.clone()),
                10,
            )
            .await
            .expect("a bounded replay is served");
        assert_eq!(
            replay.entries.iter().map(|r| r.id).collect::<Vec<_>>(),
            vec![e1.id, e2.id],
            "attempt {attempt}: a bounded replay delivers the same entries in the same order"
        );
        assert!(
            replay.next.is_none(),
            "attempt {attempt}: a bounded replay that reached its `until` carries no continuation"
        );
    }
}

// 7. A limit of 0 and a limit of MAX_PAGE_SIZE + 1 are each refused as
//    `Internal` naming the bound. <-- Review Focus 1
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_limit_outside_the_published_bound_is_refused_naming_it() {
    let (_h, adapter) = common::start_backend().await;
    let meter = common::meter(common::VCPU_METER);
    let tenant = Uuid::from_u128(1);
    let scope = common::tenant_scope(tenant);
    let subscription = [meter];

    for limit in [0, MAX_PAGE_SIZE + 1] {
        let err = adapter
            .read_feed_page(&subscription, &scope, FeedStart::Oldest, None, limit)
            .await
            .expect_err(&format!(
                "a limit of {limit} is outside the published bound"
            ));
        assert!(
            matches!(err, UsageCollectorPluginError::Internal(_)),
            "refused as Internal: {err}"
        );
        let detail = err.to_string();
        assert!(
            detail.contains(&MAX_PAGE_SIZE.to_string()),
            "the detail names the published bound {MAX_PAGE_SIZE}: {detail}"
        );
    }
}

// 8. A position of the wrong width, and a `FeedStart::After` carrying one, are
//    each refused as `Internal`. <-- Review Focus 2
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_position_of_the_wrong_width_is_refused_as_internal() {
    let (_h, adapter) = common::start_backend().await;
    let meter = common::meter(common::VCPU_METER);
    let tenant = Uuid::from_u128(1);
    let scope = common::tenant_scope(tenant);
    let subscription = [meter];

    let wrong_width = FeedPosition::new(vec![0_u8; 10]).expect("a well-formed, in-bound position");

    let err = adapter
        .read_feed_page(
            &subscription,
            &scope,
            FeedStart::After(wrong_width.clone()),
            None,
            10,
        )
        .await
        .expect_err("a `FeedStart::After` position of the wrong width is refused");
    assert!(
        matches!(err, UsageCollectorPluginError::Internal(_)),
        "refused as Internal: {err}"
    );

    let err = adapter
        .read_feed_page(
            &subscription,
            &scope,
            FeedStart::Oldest,
            Some(wrong_width),
            10,
        )
        .await
        .expect_err("an `until` position of the wrong width is refused too");
    assert!(
        matches!(err, UsageCollectorPluginError::Internal(_)),
        "refused as Internal: {err}"
    );
}

// 9. The scope narrows: an entry of another tenant is absent under a scope
//    admitting only the first, and present under one admitting both.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_scope_narrows_the_feed_to_the_tenants_it_admits() {
    let (_h, adapter) = common::start_backend().await;
    let meter = common::meter(common::VCPU_METER);
    let tenant_a = Uuid::from_u128(1);
    let tenant_b = Uuid::from_u128(2);
    let subscription = [meter.clone()];

    let a = common::entry(&meter, tenant_a, "feed-scope-a", Decimal::ONE);
    let b = common::entry(&meter, tenant_b, "feed-scope-b", Decimal::ONE);
    adapter
        .create_usage_record(a.clone())
        .await
        .expect("tenant a's entry is accepted");
    adapter
        .create_usage_record(b.clone())
        .await
        .expect("tenant b's entry is accepted");

    let narrow_scope = common::tenant_scope(tenant_a);
    let narrow = adapter
        .read_feed_page(&subscription, &narrow_scope, FeedStart::Oldest, None, 10)
        .await
        .expect("a first read under the narrow scope is served");
    assert_eq!(
        narrow.entries.iter().map(|r| r.id).collect::<Vec<_>>(),
        vec![a.id],
        "the second tenant's entry is absent under a scope admitting only the first"
    );

    let wide_scope = ast::Expr::Or(
        Box::new(common::tenant_scope(tenant_a)),
        Box::new(common::tenant_scope(tenant_b)),
    );
    let wide = adapter
        .read_feed_page(&subscription, &wide_scope, FeedStart::Oldest, None, 10)
        .await
        .expect("a first read under the wide scope is served");
    assert_eq!(
        wide.entries.iter().map(|r| r.id).collect::<Vec<_>>(),
        vec![a.id, b.id],
        "both tenants' entries are present under a scope admitting both"
    );
}

// 9b. The compiled scope must filter *inside* the lateral-per-type read, not
// on the outer merge's already-limited output. Thing to get right #2 of the
// lateral-join reshaping: applying the scope outside would let many
// off-scope rows of the same type consume the inner LIMIT before any
// in-scope row is ever considered, truncating the page to nothing even
// though in-scope entries exist.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_narrow_scope_still_finds_its_rows_behind_many_off_scope_rows_of_the_same_type() {
    let (_h, adapter) = common::start_backend().await;
    let meter = common::meter(common::VCPU_METER);
    let off_scope_tenant = Uuid::from_u128(1);
    let in_scope_tenant = Uuid::from_u128(2);
    let subscription = [meter.clone()];
    let scope = common::tenant_scope(in_scope_tenant);

    // Twenty off-scope rows, inserted -- and therefore ordered -- ahead of
    // the three in-scope ones: a per-type LIMIT applied before the scope
    // filter would exhaust itself on these and never reach the rows the
    // scope actually admits.
    for i in 0..20 {
        adapter
            .create_usage_record(common::entry(
                &meter,
                off_scope_tenant,
                &format!("feed-offscope-{i}"),
                Decimal::ONE,
            ))
            .await
            .expect("the off-scope entry is accepted");
    }
    let mut expected = Vec::new();
    for i in 0..3 {
        let e = adapter
            .create_usage_record(common::entry(
                &meter,
                in_scope_tenant,
                &format!("feed-inscope-{i}"),
                Decimal::ONE,
            ))
            .await
            .expect("the in-scope entry is accepted");
        expected.push(e.id);
    }

    // A limit that exactly matches the in-scope count: any row of this page
    // spent on an off-scope entry would starve the page of one it should
    // have carried.
    let page = adapter
        .read_feed_page(&subscription, &scope, FeedStart::Oldest, None, 3)
        .await
        .expect("a first read under the narrow scope is served");

    assert_eq!(
        page.entries.iter().map(|r| r.id).collect::<Vec<_>>(),
        expected,
        "the scope must filter inside the per-type read, so the twenty off-scope rows \
         ahead of these three cost the page nothing"
    );
}

// 10. A mark raised above a position refuses the page with
//     `CursorBeyondRetention`, and a first read over the same meter is still
//     served. Also the counter's own coverage: this test raises its mark
//     before the page read starts, so step 3's fast path alone catches it
//     (see the comment below the test raising step 6's counterpart) — the
//     refusal counter is a single call site shared by both steps (whichever
//     one returns the error), so one refusal here must move it by exactly
//     one.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_mark_above_a_position_refuses_it_but_not_a_first_read() {
    // Ruling D7: this test's subject is the page's *read* of a mark, not the
    // sweep's *write* of one. `retention_sweep_tests::the_mark_raise_only_ever_raises`
    // pins the sweep's write, and Task 5's driven dispatch is what joins the
    // two through a real chunk drop; here the mark is raised directly (via
    // the sweep's own [`RAISE_MARKS_SQL`]) so this test can land before that
    // drive exists. This is not coverage of the retention interlock itself.
    let (h, adapter, provider, exporter) = start_backend_metered().await;
    let meter = common::meter(common::VCPU_METER);
    let tenant = Uuid::from_u128(1);
    let scope = common::tenant_scope(tenant);
    let subscription = [meter.clone()];

    let entry = common::entry(&meter, tenant, "feed-mark", Decimal::ONE);
    adapter
        .create_usage_record(entry.clone())
        .await
        .expect("the entry is accepted");
    let xact = common::xact_id_of(&h.pool, entry.id).await;
    let after = (xact, entry.id);

    raise_mark(
        &h.pool,
        meter.as_str(),
        xact + 1,
        Uuid::from_u128(u128::MAX),
    )
    .await;

    let err = adapter
        .read_feed_page(
            &subscription,
            &scope,
            FeedStart::After(encode_position(after.0, after.1)),
            None,
            10,
        )
        .await
        .expect_err("a mark above the position refuses the continuation");
    assert!(
        matches!(err, UsageCollectorPluginError::CursorBeyondRetention),
        "refused as CursorBeyondRetention: {err}"
    );
    provider.force_flush().expect("flush metrics");
    assert_eq!(
        counter_sum(&exporter, "uc_timescaledb_feed_cursor_refusals_total"),
        1,
        "exactly one CursorBeyondRetention refusal must move the counter by one"
    );

    let first_read = adapter
        .read_feed_page(&subscription, &scope, FeedStart::Oldest, None, 10)
        .await
        .expect("a first read is never refused on the retention floor, marks included");
    assert_eq!(
        first_read.entries.len(),
        1,
        "the meter's one entry is still served on a first read"
    );
}

// Mutation 4 (deleting step 6 while keeping step 3) is the most important
// single mutation in this task: if it does not red, one mark read is tested
// under a claim of two. The test above inserts its mark *before* the page
// read starts, so step 3's fast path alone already catches it, and that
// mutation would not red against it alone.
//
// This test raises the mark on a second connection strictly after step 3 has
// already run and found nothing, and strictly before the page's `COMMIT` — so
// only step 6, the autocommit re-check this mutation deletes, can catch it.
//
// **A deterministic gate, not a wall-clock race.** An earlier version of this
// test tried to land the window by racing `tokio::join!` against wall-clock
// completion order; measured over several runs it failed on the correct
// implementation roughly one round in twenty (the client-side instant a
// commit is acknowledged at is not a reliable proxy for server-side
// statement ordering at sub-millisecond separation) — a flaky test being
// exactly the "bad work" this task's instructions ask to avoid. Instead: an
// `ACCESS EXCLUSIVE` lock on `usage_records`, held open from a second
// connection *before* the read starts, blocks only step 4 (the page
// statement, which needs `ACCESS SHARE` on that table) — steps 2 and 3 touch
// neither `usage_records` nor this lock, so they always run to completion
// (step 3 finding no mark, since none exists yet) before the read can
// possibly reach the block. Polling `pg_stat_activity` for that block is the
// signal that steps 1-3 are done; only then is the mark raised and the lock
// released, so its commit is placed in the exact window every round, with no
// timing dependency at all.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_mark_committed_during_the_page_transaction_is_still_caught() {
    let (h, adapter) = common::start_backend().await;
    let adapter = std::sync::Arc::new(adapter);
    let meter = common::meter(common::VCPU_METER);
    let tenant = Uuid::from_u128(1);
    let scope = common::tenant_scope(tenant);
    let subscription = [meter.clone()];

    for round in 0_u32..5 {
        let entry = common::entry(
            &meter,
            tenant,
            &format!("feed-mark-race-{round}"),
            Decimal::ONE,
        );
        adapter
            .create_usage_record(entry.clone())
            .await
            .unwrap_or_else(|err| panic!("round {round}: the entry is accepted: {err}"));
        let xact = common::xact_id_of(&h.pool, entry.id).await;
        let position = encode_position(xact, entry.id);

        let mut locker = h.pool.begin().await.expect("begin the locking transaction");
        sqlx::query("LOCK TABLE usage_records IN ACCESS EXCLUSIVE MODE")
            .execute(&mut *locker)
            .await
            .expect("acquire the exclusive lock that gates step 4");

        let read_adapter = std::sync::Arc::clone(&adapter);
        let read_scope = scope.clone();
        let read_subscription = subscription.clone();
        let read_task = tokio::spawn(async move {
            read_adapter
                .read_feed_page(
                    &read_subscription,
                    &read_scope,
                    FeedStart::After(position),
                    None,
                    10,
                )
                .await
        });

        // Proof that steps 1-3 have already run: the read's backend is now
        // blocked waiting for the lock step 4 needs.
        wait_for_lock_wait(&h.pool).await;

        raise_mark(
            &h.pool,
            meter.as_str(),
            xact + 1,
            Uuid::from_u128(u128::MAX),
        )
        .await;

        locker
            .commit()
            .await
            .expect("release the lock, unblocking step 4");
        let result = read_task
            .await
            .expect("the spawned page read did not panic");

        assert!(
            matches!(
                result,
                Err(UsageCollectorPluginError::CursorBeyondRetention)
            ),
            "round {round}: the mark committed after step 3 had already run and found nothing, \
             and before the read's own COMMIT, so only step 6 (the authoritative, autocommit, \
             post-COMMIT re-check) could have caught it: {result:?}"
        );
    }
}

/// Polls until some backend on `pool` is blocked waiting for a table-level
/// lock, up to 2.5 s.
///
/// # Panics
///
/// If nothing is found waiting within the budget — the deterministic gate
/// [`a_mark_committed_during_the_page_transaction_is_still_caught`] depends on
/// failed to engage, which is a harness defect in that test rather than a
/// finding about the plugin.
async fn wait_for_lock_wait(pool: &sqlx::PgPool) {
    for _ in 0_u32..500 {
        let waiting: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM pg_stat_activity WHERE wait_event_type = 'Lock'",
        )
        .fetch_one(pool)
        .await
        .expect("read pg_stat_activity");
        if waiting > 0 {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    panic!(
        "no backend blocked on the held lock within 2.5s; the deterministic gate did not engage"
    );
}

// 11. An entry visible to the snapshot but not settled is NOT delivered.
//     <-- Review Focus 5
//
// Completeness, and the one assertion that shows the horizon bound does work
// the snapshot does not already do. A snapshot sees rows from transactions
// above its `xmin` that committed before it was taken; those are exactly the
// entries that are visible and not settled, because the transaction at `xmin`
// can still commit a row at a *lower* position.
//
// Shape: writer A opens a transaction and inserts without committing, so it
// holds an id and pins the horizon. Writer B then inserts and commits, so its
// row is visible to any later snapshot and sits above A's id. A page read now
// must deliver neither — B's row is above the horizon — and the cursor it
// returns must be at or below A's id, so that A's row is still ahead of it
// when A commits.
//
// 20 rounds inside one test rather than repeated runs of one attempt: this
// project measured that 12 single-attempt runs found nothing where 20 rounds
// inside one test reproduced immediately.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_visible_but_unsettled_entry_is_not_delivered_ahead_of_its_gap() {
    let (h, adapter) = common::start_backend().await;
    let meter = common::meter(common::VCPU_METER);
    let tenant = Uuid::from_u128(1);
    let scope = common::tenant_scope(tenant);
    let subscription = [meter.clone()];

    // A warm-up write, fully committed before the race begins. Measured
    // necessary: without it, round 0's writer A (a raw INSERT held open) and
    // writer B (through the adapter) contend over creating this meter's very
    // first hypertable chunk, and B's write comes back `Transient` — a
    // harness hazard the completeness argument says nothing about, not the
    // subject under test.
    let warmup = common::entry(&meter, tenant, "gap-warmup", Decimal::ONE);
    adapter
        .create_usage_record(warmup)
        .await
        .expect("the warm-up entry establishes the meter's chunk before the race begins");

    for round in 0_u32..20 {
        let a = common::entry(&meter, tenant, &format!("gap-a-{round}"), Decimal::ONE);
        let b = common::entry(&meter, tenant, &format!("gap-b-{round}"), Decimal::ONE);

        // The head position before this round's two entries exist. Reading
        // from here rather than from `FeedStart::Oldest` keeps the assertion
        // below about *this round's* entries even once earlier rounds have
        // committed enough rows to fill a 10-row page on their own.
        let before = adapter
            .read_feed_page(
                &subscription,
                &scope,
                FeedStart::Oldest,
                None,
                MAX_PAGE_SIZE,
            )
            .await
            .expect("a baseline read is served")
            .next
            .expect("a live page carries a continuation");

        // Writer A: opened on its own connection and left uncommitted, so it
        // holds the lowest in-progress xact_id for as long as this round
        // needs it to.
        let mut tx_a = h.pool.begin().await.expect("begin writer A's transaction");
        insert_raw_within(&mut tx_a, &a).await;

        // Writer B: an ordinary write through the adapter under test. It
        // opens (and is assigned its xact_id) after A, and commits before the
        // page read below, so its row is visible to any snapshot taken now.
        adapter
            .create_usage_record(b.clone())
            .await
            .unwrap_or_else(|err| panic!("round {round}: writer B's entry is accepted: {err}"));

        let page = adapter
            .read_feed_page(&subscription, &scope, FeedStart::After(before), None, 10)
            .await
            .expect("the page is served");
        assert!(
            page.entries.iter().all(|e| e.id != a.id && e.id != b.id),
            "round {round}: neither writer A's entry (still open) nor writer B's (visible to \
             the snapshot but above the horizon A pins) may be delivered: {:?}",
            page.entries.iter().map(|e| e.id).collect::<Vec<_>>(),
        );
        let cursor_while_a_was_open = page.next.expect("a live page carries a continuation");

        tx_a.commit().await.expect("writer A commits");

        let resumed = adapter
            .read_feed_page(
                &subscription,
                &scope,
                FeedStart::After(cursor_while_a_was_open),
                None,
                10,
            )
            .await
            .expect("resuming from the pre-commit cursor is served");
        let ids: Vec<Uuid> = resumed.entries.iter().map(|e| e.id).collect();
        assert_eq!(
            ids,
            vec![a.id, b.id],
            "round {round}: the cursor issued while writer A was still open must be strictly \
             behind A's entry, so resuming from it delivers A and then B, in feed order"
        );
    }
}

/// Insert `record` directly, within an already-open transaction, so a test
/// can hold it uncommitted across other work. The production write path
/// (`PgRecordStore::create`) always commits before returning, so this is the
/// one way a test can control when a write settles.
///
/// Mirrors `common::insert_raw_entry`'s column list, minus the columns this
/// suite never sets (`subject_id` / `subject_type`, `invalidates` /
/// `reason_code`): every entry this file writes is an ordinary measurement.
async fn insert_raw_within(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    record: &usage_collector_sdk::UsageRecord,
) {
    sqlx::query(
        "INSERT INTO usage_records (id, tenant_id, gts_type_id, type_key, quantity, \
         window_start, window_end, resource_id, resource_type, idempotency_key, \
         invalidates, reason_code, origin, entry_type, accepted_at, metadata) \
         VALUES ($1, $2, $3, 1, $4, $5, $6, $7, $8, $9, NULL, NULL, $10, \
         'record'::usage_entry_type, $11, '{}'::jsonb)",
    )
    .bind(record.id)
    .bind(record.tenant_id)
    .bind(record.gts_type_id.as_str())
    .bind(record.quantity.as_decimal())
    .bind(record.window_start)
    .bind(record.window_end)
    .bind(record.resource_ref.resource_id())
    .bind(record.resource_ref.resource_type())
    .bind(record.idempotency_key.as_str())
    .bind(record.origin.as_str())
    .bind(record.accepted_at)
    .execute(&mut **tx)
    .await
    .expect("insert writer A's row directly, within its own open transaction");
}

// The feed's settled-horizon lag sampler (`docs/DESIGN.md` §4.3): the gauge
// is absent from the exported inventory (not zeroed) while idle -- the
// decisive check, since a `sample_once` mutated to
// `set_feed_horizon_lag(lag.unwrap_or(0.0))` still *returns* `None` while
// idle and would pass a return-value-only assertion undetected -- and then
// strictly grows while a write transaction is held open. Uses the same
// "second connection holding an open write transaction" shape as the gap
// test above, metered like `start_backend_metered` so the exported
// inventory, not just the return value, is observable.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_horizon_monitor_samples_the_oldest_open_write_transaction() {
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
    let monitor = FeedHorizonMonitor::new(h.pool.clone(), metrics);

    // Nothing else holds a write transaction open on a freshly migrated
    // database, so the plugin role sees none: `None`, not an error, and the
    // gauge must not appear in the exported inventory at all.
    let idle = monitor
        .sample_once()
        .await
        .expect("sampling with nothing open must not error");
    assert!(
        idle.is_none(),
        "no open write transaction should report no observation: {idle:?}"
    );
    provider.force_flush().expect("flush metrics");
    assert!(
        !exported_names(&exporter).contains(&"uc_timescaledb_feed_horizon_lag_seconds".to_owned()),
        "an instrument the sampler has built but never recorded on is not exported at all, \
         which is what 'left unset' has to mean for a reader: {:?}",
        exported_names(&exporter)
    );

    let meter = common::meter(common::VCPU_METER);
    let tenant = Uuid::from_u128(1);
    let entry = common::entry(&meter, tenant, "horizon-lag", Decimal::ONE);
    let mut tx = h
        .pool
        .begin()
        .await
        .expect("begin a held write transaction");
    insert_raw_within(&mut tx, &entry).await;

    let first = monitor
        .sample_once()
        .await
        .expect("sampling with the transaction open must not error")
        .expect("an open write transaction is visible to the plugin role");
    // 50ms is ample separation next to a container-backed test's own
    // baseline latency, and the SQL's `extract(epoch FROM ...)` carries
    // sub-millisecond precision, so a strict increase discriminates a real
    // sampler from a constant or stale-cached one -- `>=` would pass either.
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    let second = monitor
        .sample_once()
        .await
        .expect("sampling again must not error")
        .expect("the transaction is still open");
    assert!(
        second > first,
        "the lag must strictly grow across the 50ms gap while the transaction stays open: \
         {first} -> {second}"
    );
    provider.force_flush().expect("flush metrics");
    assert!(
        exported_names(&exporter).contains(&"uc_timescaledb_feed_horizon_lag_seconds".to_owned()),
        "the gauge must be exported once a write transaction has been observed"
    );

    tx.commit().await.expect("release the held transaction");
}

// 12. A page's order survives a decimal-digit-length crossing in `xact_id`.
//
// The pin below is the same pattern `retention_sweep_integration_pg::
// the_position_read_orders_xact_id_numerically_not_lexicographically`
// established for the sweep's own read of `xact_id`: two consecutive inserts
// through the ingest path always share a digit count, so no ordinary fixture
// can straddle the boundary, and the test pins explicit `xid8` values by
// direct SQL to force it.
//
// This is the read `FEED_COLUMNS`'s `xact_id_text` alias protects and the
// earlier `retention_sweep_integration_pg` test does not reach: from Task 3's
// head until this task's fix, `RECORD_COLUMNS` cast `xact_id` to `AS xact_id`
// -- the same name as its own source column -- and `query/feed.rs`'s page
// statement ran `ORDER BY xact_id, id` over that same read list. `PostgreSQL`
// resolves a bare `ORDER BY` name matching both an output and an input column
// to the *output* column, so that statement was silently ordering the feed
// page lexicographically over the rendered digit string rather than
// numerically over the `xid8`, for as long as that alias stood. No test
// caught it, because within one short run every `xact_id` shares a digit
// count. `FEED_COLUMNS`'s `xact_id_text` alias is the fix; this test is the
// oracle against it recurring by some future edit reaching the same state a
// different way.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_digit_length_crossing_in_xact_id_still_orders_the_page_numerically() {
    let (h, adapter) = common::start_backend().await;
    let meter = common::meter(common::VCPU_METER);
    let tenant = Uuid::from_u128(1);
    let scope = common::tenant_scope(tenant);
    let subscription = [meter.clone()];

    let low = adapter
        .create_usage_record(common::entry(
            &meter,
            tenant,
            "digit-cross-low",
            Decimal::ONE,
        ))
        .await
        .expect("the low entry is accepted");
    let high = adapter
        .create_usage_record(common::entry(
            &meter,
            tenant,
            "digit-cross-high",
            Decimal::ONE,
        ))
        .await
        .expect("the high entry is accepted");

    // Pin xid8 values that straddle a digit-length boundary: under a
    // lexicographic (text) comparison "9" sorts above "10".
    sqlx::query("UPDATE usage_records SET xact_id = '9'::xid8 WHERE id = $1")
        .bind(low.id)
        .execute(&h.pool)
        .await
        .expect("pin the low entry's xact_id");
    sqlx::query("UPDATE usage_records SET xact_id = '10'::xid8 WHERE id = $1")
        .bind(high.id)
        .execute(&h.pool)
        .await
        .expect("pin the high entry's xact_id");

    let page = adapter
        .read_feed_page(&subscription, &scope, FeedStart::Oldest, None, 10)
        .await
        .expect("a first read is served");

    assert_eq!(
        page.entries.iter().map(|r| r.id).collect::<Vec<_>>(),
        vec![low.id, high.id],
        "the numerically lesser xact_id (9) must be delivered before the numerically \
         greater one (10), even though \"9\" sorts lexicographically above \"10\"; a bug \
         here means the page ordered by a text-cast output column instead of the xid8 \
         input column"
    );
}

// 13. A bounded replay whose last row exactly fills its limit at `until`
//     carries no continuation.
//
// Test 6 above only ever exercises the short-page arm of the disposition
// (`until` set beyond a page shorter than its limit). This is the other
// shape: the range `[Oldest, until]` holds exactly `limit` rows, so the page
// both fills to its limit *and* reaches `until` on the very same last row.
// `until.is_some() && row_count < limit_as_usize` alone cannot see that —
// only `last_position == until` can — and mistaking it for the ordinary
// filled-to-limit case mints a continuation for a replay that has already
// closed. A consumer that then presents that continuation runs the mark
// check on a position it should never have been handed, and a mark raised
// in between turns a *completed* bounded replay into `CursorBeyondRetention`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_bounded_replay_that_exactly_fills_its_limit_at_until_carries_no_continuation() {
    let (_h, adapter) = common::start_backend().await;
    let meter = common::meter(common::VCPU_METER);
    let tenant = Uuid::from_u128(1);
    let scope = common::tenant_scope(tenant);
    let subscription = [meter.clone()];

    let mut ids = Vec::new();
    for i in 0..4 {
        let entry = common::entry(
            &meter,
            tenant,
            &format!("feed-exact-fill-{i}"),
            Decimal::ONE,
        );
        adapter
            .create_usage_record(entry.clone())
            .await
            .expect("the entry is accepted");
        ids.push(entry.id);
    }

    let first = adapter
        .read_feed_page(&subscription, &scope, FeedStart::Oldest, None, 2)
        .await
        .expect("the first page is served");
    assert_eq!(
        first.entries.iter().map(|r| r.id).collect::<Vec<_>>(),
        ids[..2].to_vec(),
        "the first page fills to its limit with the first two entries"
    );
    let until = first
        .next
        .expect("a page filled to its limit carries a continuation");

    let replay = adapter
        .read_feed_page(&subscription, &scope, FeedStart::Oldest, Some(until), 2)
        .await
        .expect("the bounded replay is served");
    assert_eq!(
        replay.entries.iter().map(|r| r.id).collect::<Vec<_>>(),
        ids[..2].to_vec(),
        "the replay delivers exactly the same two entries the range `[Oldest, until]` holds"
    );
    assert!(
        replay.next.is_none(),
        "a bounded replay whose last row exactly fills its limit at `until` has reached its \
         bound and must carry no continuation, not the filled-to-limit arm's own position: \
         {:?}",
        replay.next
    );
}

// 14. A feed read acquires its connection through the metered pool path, the
//     same as every other operation.
//
// `feed_page` used to call `self.pool.acquire()` directly, bypassing
// `Self::timed_acquire` and so `uc_timescaledb_pool_acquire_duration_seconds`,
// the readiness contract, `uc_timescaledb_backend_errors_total` and
// `uc_timescaledb_tls_handshake_failures_total` on the one path §4.1 item 7
// sizes at the highest sustained read rate the plugin serves. This asserts
// the histogram, since it is the most direct observable: a connection
// acquired outside `Self::timed_acquire` records no observation on it at all.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_feed_read_acquires_through_the_metered_pool_path() {
    let (_h, adapter, provider, exporter) = start_backend_metered().await;
    let meter = common::meter(common::VCPU_METER);
    let tenant = Uuid::from_u128(1);
    let scope = common::tenant_scope(tenant);
    let subscription = [meter.clone()];

    provider.force_flush().expect("flush metrics");
    let before = histogram_count(&exporter, "uc_timescaledb_pool_acquire_duration_seconds");

    adapter
        .read_feed_page(&subscription, &scope, FeedStart::Oldest, None, 10)
        .await
        .expect("a first read is served");

    provider.force_flush().expect("flush metrics");
    let after = histogram_count(&exporter, "uc_timescaledb_pool_acquire_duration_seconds");

    assert!(
        after > before,
        "a feed read must acquire its connection through `Self::timed_acquire`, which is what \
         records `uc_timescaledb_pool_acquire_duration_seconds`; before={before}, after={after}"
    );
}
