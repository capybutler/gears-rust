#![cfg(feature = "postgres")]
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]
//! `PgRecordStore::reconciliation` against a live `TimescaleDB`. Requires
//! Docker.
//!
//! The behavioural home for the fourth read path, modelled on
//! `records_query_integration_pg.rs`: one test per Review Focus item the
//! task brief calls out, plus the two happy paths. The SDK's
//! `reconciliation-figures` contract check already drives this
//! path's six DESIGN §3.3 cases against this backend through
//! `contract_conformance_pg.rs`; what is here instead is what that check
//! structurally cannot assert — the exact statement shape (index-reachable
//! watermarks, a ranged scan for the summary) and the rollup exclusion,
//! neither of which a contract check written against an in-memory reference
//! backend can pin.

mod common;
use common::StoreFixtures;

use std::num::NonZeroU64;
use std::sync::Arc;

use bigdecimal::BigDecimal;
use opentelemetry::metrics::MeterProvider as _;
use opentelemetry_sdk::metrics::data::{AggregatedMetrics, MetricData};
use opentelemetry_sdk::metrics::{InMemoryMetricExporter, PeriodicReader, SdkMeterProvider};
use rust_decimal::Decimal;
use std::str::FromStr;
use time::Duration;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use usage_collector_sdk::{
    AggregationFold, ObservedQuantity, QuantitySummary, ReconciliationMetadata, TimeRange,
};

use timescaledb_usage_collector_plugin::domain::ports::RecordStore;
use timescaledb_usage_collector_plugin::infra::metrics::Metrics;
use timescaledb_usage_collector_plugin::infra::storage::record_store::PgRecordStore;

async fn setup() -> (common::TsHarness, PgRecordStore) {
    let h = common::bring_up()
        .await
        .expect("timescaledb container (Docker required)");
    let store = common::record_store(&h);
    (h, store)
}

/// Like [`setup`], but the metric inventory writes to a **local** in-memory
/// exporter instead of the process-global provider, so a test can read a
/// histogram back -- the same reason `feed_page_integration_pg`'s
/// `start_backend_metered` gives: `Metrics::with_meter` takes the meter
/// explicitly, so the assertion never depends on global state another test
/// binary is also writing to.
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
    let store = PgRecordStore::new(
        h.pool.clone(),
        metrics,
        CancellationToken::new(),
        h.cfg.feed_acceptance_slack_secs,
    );
    (h, store, provider, exporter)
}

/// Total observation count across the `f64` Histogram data points named
/// `name`. Mirrors `feed_page_integration_pg::histogram_count`; duplicated
/// here for the same reason that file's own copy gives: this file's exporter
/// belongs to a separate integration-test crate, not a sibling of `metrics.rs`'s
/// own unit tests.
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

/// A one-hour range starting at the fixture window start, the range every
/// test below reads unless it says otherwise.
fn hour_range() -> TimeRange {
    TimeRange::new(
        common::fixture_window_start(),
        common::fixture_window_start() + Duration::hours(1),
    )
    .expect("ordered range")
}

#[tokio::test]
async fn a_sum_meter_reports_an_accrued_sum_over_exactly_the_entries_the_range_selects() {
    let (_h, store) = setup().await;
    let meter =
        common::meter("gts.cf.core.uc.usage_record.v1~cf.test._.reconciliation_accrues.v1~");
    let tenant = Uuid::from_u128(0xACC5);
    let range = hour_range();

    let a = store
        .create_fixture(common::entry_over(
            &meter,
            tenant,
            "accrues-a",
            Decimal::new(250, 2), // 2.50
            common::fixture_window_start(),
            common::fixture_window_start() + Duration::minutes(10),
        ))
        .await
        .expect("seed entry a");
    let b = store
        .create_fixture(common::entry_over(
            &meter,
            tenant,
            "accrues-b",
            Decimal::new(375, 2), // 3.75
            common::fixture_window_start() + Duration::minutes(10),
            common::fixture_window_start() + Duration::minutes(20),
        ))
        .await
        .expect("seed entry b");
    // Out of range: covers a period ending after the range's exclusive
    // upper bound.
    store
        .create_fixture(common::entry_over(
            &meter,
            tenant,
            "accrues-outside",
            Decimal::new(99900, 2), // 999.00
            common::fixture_window_start() + Duration::minutes(80),
            common::fixture_window_start() + Duration::minutes(90),
        ))
        .await
        .expect("seed out-of-range entry");

    let answer = store
        .reconciliation(
            tenant,
            &common::meter_ref(meter.clone()),
            range,
            AggregationFold::Sum,
            &common::tenant_scope(tenant),
        )
        .await
        .expect("an ordinary reconciliation read must not fail");

    assert_eq!(
        answer.accepted_count, 2,
        "two entries fall inside the range"
    );
    let expected_total = a
        .quantity
        .as_decimal()
        .to_string()
        .parse::<BigDecimal>()
        .expect("decimal parses")
        + b.quantity
            .as_decimal()
            .to_string()
            .parse::<BigDecimal>()
            .expect("decimal parses");
    assert_eq!(
        answer.quantity_summary,
        QuantitySummary::Accrued(expected_total),
        "the accrual must cover exactly the two in-range entries, not the \
         999.00 entry whose covered period ends outside the range"
    );
}

#[tokio::test]
async fn a_max_meter_reports_the_observation_with_the_greatest_window_end_not_the_greatest_quantity()
 {
    // Review Focus 1. The fold selects the summary's BRANCH and never its
    // ordering: `latest_observation` is by the LATEST total order for every
    // non-accruing fold. The fixture deliberately puts the largest quantity
    // on the earliest period, so a backend reaching for its own MAX fold
    // returns the wrong entry.
    let (_h, store) = setup().await;
    let meter =
        common::meter("gts.cf.core.uc.usage_record.v1~cf.test._.reconciliation_observes.v1~");
    let tenant = Uuid::from_u128(0x0B5E);
    let range = hour_range();

    store
        .create_fixture(common::entry_over(
            &meter,
            tenant,
            "observes-greatest-qty",
            Decimal::new(50, 0),
            common::fixture_window_start(),
            common::fixture_window_start() + Duration::minutes(10),
        ))
        .await
        .expect("seed greatest-quantity entry");
    store
        .create_fixture(common::entry_over(
            &meter,
            tenant,
            "observes-middle",
            Decimal::new(10, 0),
            common::fixture_window_start() + Duration::minutes(10),
            common::fixture_window_start() + Duration::minutes(20),
        ))
        .await
        .expect("seed middle entry");
    let greatest_window_end = store
        .create_fixture(common::entry_over(
            &meter,
            tenant,
            "observes-greatest-window-end",
            Decimal::new(5, 0),
            common::fixture_window_start() + Duration::minutes(20),
            common::fixture_window_start() + Duration::minutes(30),
        ))
        .await
        .expect("seed greatest-window-end entry");

    let answer = store
        .reconciliation(
            tenant,
            &common::meter_ref(meter.clone()),
            range,
            AggregationFold::Max,
            &common::tenant_scope(tenant),
        )
        .await
        .expect("an ordinary reconciliation read must not fail");

    assert_eq!(answer.accepted_count, 3);
    assert_eq!(
        answer.quantity_summary,
        QuantitySummary::Observations(Some(ObservedQuantity {
            count: NonZeroU64::new(3).unwrap(),
            latest: greatest_window_end.quantity,
        })),
        "latest must be the entry with the greatest window_end (quantity 5), \
         not the entry with the greatest quantity (50)"
    );
}

#[tokio::test]
async fn a_range_holding_nothing_but_a_withdrawn_pair_counts_two_and_summarises_none() {
    // Review Focus 3. `accepted_count` keeps both halves because it reports
    // ingestion activity; the summary drops both because it aggregates the
    // meter.
    let (_h, store) = setup().await;
    let meter =
        common::meter("gts.cf.core.uc.usage_record.v1~cf.test._.reconciliation_withdrawn.v1~");
    let tenant = Uuid::from_u128(0x0D8A);
    let range = hour_range();

    let record = store
        .create_fixture(common::entry_over(
            &meter,
            tenant,
            "withdrawn-record",
            Decimal::new(400, 2),
            common::fixture_window_start(),
            common::fixture_window_start() + Duration::minutes(10),
        ))
        .await
        .expect("seed record");
    store
        .create_fixture(common::withdrawal_of(&record))
        .await
        .expect("seed withdrawal");

    let fold = AggregationFold::Count;
    let answer = store
        .reconciliation(
            tenant,
            &common::meter_ref(meter.clone()),
            range,
            fold,
            &common::tenant_scope(tenant),
        )
        .await
        .expect("an ordinary reconciliation read must not fail");

    assert_eq!(
        answer.accepted_count, 2,
        "a record and the invalidation that withdraws it are two accepted \
         entries"
    );
    assert_eq!(
        answer.quantity_summary,
        QuantitySummary::empty_for(fold),
        "the summary excludes both halves of the withdrawn pair"
    );
}

#[tokio::test]
async fn a_range_selecting_nothing_still_reports_both_watermarks() {
    // Review Focus 2. The watermarks are unbounded by the range, so a
    // statement that folds them into the ranged aggregate — or a
    // short-circuit that returns `empty_for(fold)` when the count is zero —
    // breaks this and nothing else catches it.
    let (_h, store) = setup().await;
    let meter =
        common::meter("gts.cf.core.uc.usage_record.v1~cf.test._.reconciliation_empty_range.v1~");
    let tenant = Uuid::from_u128(0xE491);
    let range = hour_range();

    let earlier_accepted_at = common::fixture_window_end() + Duration::minutes(80);
    let later_accepted_at = common::fixture_window_end() + Duration::minutes(90);

    let earlier = store
        .create_fixture(usage_collector_sdk::StoredUsageRecord {
            accepted_at: earlier_accepted_at,
            ..common::entry_over(
                &meter,
                tenant,
                "empty-range-a",
                Decimal::new(100, 2),
                common::fixture_window_start() + Duration::minutes(80),
                common::fixture_window_start() + Duration::minutes(90),
            )
        })
        .await
        .expect("seed earlier out-of-range entry");
    let later = store
        .create_fixture(usage_collector_sdk::StoredUsageRecord {
            accepted_at: later_accepted_at,
            ..common::entry_over(
                &meter,
                tenant,
                "empty-range-b",
                Decimal::new(200, 2),
                common::fixture_window_start() + Duration::minutes(90),
                common::fixture_window_start() + Duration::minutes(100),
            )
        })
        .await
        .expect("seed later out-of-range entry");

    let fold = AggregationFold::Sum;
    let answer = store
        .reconciliation(
            tenant,
            &common::meter_ref(meter.clone()),
            range,
            fold,
            &common::tenant_scope(tenant),
        )
        .await
        .expect("an ordinary reconciliation read must not fail");

    assert_eq!(
        answer.accepted_count, 0,
        "both entries cover a period ending outside the range"
    );
    assert_eq!(answer.quantity_summary, QuantitySummary::empty_for(fold));
    assert_eq!(
        answer.max_accepted_at,
        Some(later.accepted_at),
        "max_accepted_at is unbounded by the range"
    );
    assert_eq!(
        answer.max_window_end,
        Some(later.window_end),
        "max_window_end is unbounded by the range"
    );
    // Both entries are real, and `earlier` is read only to keep it live —
    // the watermarks must be the *greater* of the two.
    assert!(earlier.accepted_at < later.accepted_at);
}

#[tokio::test]
async fn a_scope_holding_no_entries_reports_both_watermarks_absent() {
    let (_h, store) = setup().await;
    let meter =
        common::meter("gts.cf.core.uc.usage_record.v1~cf.test._.reconciliation_no_entries.v1~");
    let tenant = Uuid::from_u128(0xF001);
    let fold = AggregationFold::Max;

    let answer = store
        .reconciliation(
            tenant,
            &common::meter_ref(meter.clone()),
            hour_range(),
            fold,
            &common::tenant_scope(tenant),
        )
        .await
        .expect("a meter never written to must still answer ordinarily");

    assert_eq!(
        answer,
        ReconciliationMetadata::empty_for(fold),
        "a meter this test never wrote to must answer exactly the empty case"
    );
}

#[tokio::test]
async fn a_tenant_outside_the_compiled_scope_answers_field_for_field_as_one_holding_no_entries() {
    // Review Focus 5. Asserted field for field, not just "not an error" and
    // not just a zero count: a plugin applying the scope AFTER the tenant
    // predicate passes a count-only assertion while leaking the watermarks.
    let (_h, store) = setup().await;
    let meter =
        common::meter("gts.cf.core.uc.usage_record.v1~cf.test._.reconciliation_excluded.v1~");
    let admitted_tenant = Uuid::from_u128(0xADD1);
    let excluded_tenant = Uuid::from_u128(0xEEC1);
    let range = hour_range();

    store
        .create_fixture(common::entry_over(
            &meter,
            excluded_tenant,
            "excluded-a",
            Decimal::new(9, 0),
            common::fixture_window_start(),
            common::fixture_window_start() + Duration::minutes(10),
        ))
        .await
        .expect("seed entry under the excluded tenant");
    store
        .create_fixture(common::entry_over(
            &meter,
            excluded_tenant,
            "excluded-b",
            Decimal::new(3, 0),
            common::fixture_window_start() + Duration::minutes(10),
            common::fixture_window_start() + Duration::minutes(20),
        ))
        .await
        .expect("seed a second entry under the excluded tenant");

    let fold = AggregationFold::Max;
    // The call's own `tenant_id` argument names the tenant that actually
    // holds the two entries; the compiled `scope` admits a different one.
    let answer = store
        .reconciliation(
            excluded_tenant,
            &common::meter_ref(meter.clone()),
            range,
            fold,
            &common::tenant_scope(admitted_tenant),
        )
        .await
        .expect("a tenant the compiled scope excludes must answer ordinarily, never an error");

    assert_eq!(
        answer,
        ReconciliationMetadata::empty_for(fold),
        "the compiled scope applies before tenant_id, so an excluded tenant \
         holding real entries must answer exactly as one holding none: \
         accepted_count, the summary, and both watermarks alike"
    );
}

#[tokio::test]
async fn the_summary_matches_the_ledger_read_live_with_no_refresh_in_between() {
    // Plugin DECOMPOSITION §2.8 puts "Serving the summary from the
    // materialised aggregate" explicitly out of scope, and this exercises
    // the live read end-to-end against real TimescaleDB immediately after a
    // write, with no refresh step run in between.
    //
    // **This does not by itself prove the summary skipped the rollup.**
    // `usage_rollup_1h` is a real-time continuous aggregate
    // (`timescaledb.materialized_only = false`,
    // `migrations/0002_usage_rollup.sql`), so it is never empty after a
    // write — a prior version of this comment claimed otherwise and was
    // wrong. Worse, for a `Sum` fold the rollup's signed-netting formula
    // (`SUM(CASE WHEN invalidates IS NULL THEN quantity ELSE -quantity
    // END)`) is mathematically identical to the exact scan's
    // FILTER-excluded sum for every fixture shape, withdrawn pair included
    // — the migration's own comment states the equivalence outright — so no
    // numeric fixture here could ever tell the two apart. The structural
    // guarantee lives in
    // `record_store_tests::the_summary_statement_never_references_the_rollup`,
    // which asserts the rendered SQL never names `usage_rollup_1h`; this
    // test is what that statement looks like wired to a live backend, and
    // stays for that reason rather than for discriminating the rollup.
    let (_h, store) = setup().await;
    let meter =
        common::meter("gts.cf.core.uc.usage_record.v1~cf.test._.reconciliation_exact_scan.v1~");
    let tenant = Uuid::from_u128(0x5CA4);
    let range = hour_range();

    let entry = store
        .create_fixture(common::entry_over(
            &meter,
            tenant,
            "exact-scan",
            Decimal::new(1234, 2), // 12.34
            common::fixture_window_start(),
            common::fixture_window_start() + Duration::minutes(10),
        ))
        .await
        .expect("seed entry");

    let answer = store
        .reconciliation(
            tenant,
            &common::meter_ref(meter.clone()),
            range,
            AggregationFold::Sum,
            &common::tenant_scope(tenant),
        )
        .await
        .expect("an ordinary reconciliation read must not fail");

    assert_eq!(answer.accepted_count, 1);
    assert_eq!(
        answer.quantity_summary,
        QuantitySummary::Accrued(
            BigDecimal::from_str(&entry.quantity.as_decimal().to_string()).expect("decimal parses")
        ),
        "read with no rollup refresh in between, the summary must already \
         see the just-written entry; the materialised rollup would not"
    );
}

// A reconciliation read records `uc_timescaledb_reconciliation_duration_seconds`.
//
// Slice 8 T11 declared this histogram with no explicit bucket layout (ruling
// I3) and wired `PgRecordStore::reconciliation`'s `OpDurationGuard` to it;
// this is the pg-lane pin that a real reconciliation read actually drives it,
// so a later edit that detaches the guard -- or reverts the call site, as
// happened once already in this slice -- reds here rather than surviving as
// a silent "declared and dead" regression that only a unit test recording
// the port directly would miss.
#[tokio::test]
async fn a_reconciliation_read_records_the_reconciliation_duration_histogram() {
    let (_h, store, provider, exporter) = setup_metered().await;
    let meter =
        common::meter("gts.cf.core.uc.usage_record.v1~cf.test._.reconciliation_duration.v1~");
    let tenant = Uuid::from_u128(0xD0E5);
    let range = hour_range();

    provider.force_flush().expect("flush metrics");
    let before = histogram_count(&exporter, "uc_timescaledb_reconciliation_duration_seconds");

    store
        .reconciliation(
            tenant,
            &common::meter_ref(meter.clone()),
            range,
            AggregationFold::Sum,
            &common::tenant_scope(tenant),
        )
        .await
        .expect("a reconciliation read over an empty selection must not fail");

    provider.force_flush().expect("flush metrics");
    let after = histogram_count(&exporter, "uc_timescaledb_reconciliation_duration_seconds");

    assert!(
        after > before,
        "a reconciliation read must record \
         `uc_timescaledb_reconciliation_duration_seconds` via `OpDurationGuard`; \
         before={before}, after={after}"
    );
}
