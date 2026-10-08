//! End-to-end tests for [`UcMetricsMeter`] against an in-memory
//! OpenTelemetry exporter.
//!
//! These assert the exact **rendered Prometheus names, labels, and bucket
//! layouts** from DESIGN §3.11.5 — the architectural contract. A mistyped
//! name (e.g. a missing `_total`) or a wrong bucket set fails here.

use std::collections::BTreeSet;

use opentelemetry::metrics::MeterProvider;
use opentelemetry_sdk::metrics::data::{AggregatedMetrics, MetricData};
use opentelemetry_sdk::metrics::{InMemoryMetricExporter, PeriodicReader, SdkMeterProvider};

use crate::domain::ports::metrics::{
    AuthzDecision, FeedErrorCategory, IngestRequestErrorCategory, IngestRequestOutcome,
    PdpFailureCause, PdpOp, PluginErrorCategory, PluginOp, QueryErrorCategory, QueryKind,
    RecordErrorCategory, RecordOutcome, RequestOutcome, TypeResolutionOutcome,
    UsageCollectorMetrics,
};
use crate::infra::metrics::{UcMetricsMeter, build_default_adapter};
use usage_collector_sdk::{EntryType, RecordOrigin};

const TEST_PREFIX: &str = "uc";

fn local_provider() -> (SdkMeterProvider, InMemoryMetricExporter) {
    let exporter = InMemoryMetricExporter::default();
    let provider = SdkMeterProvider::builder()
        .with_reader(PeriodicReader::builder(exporter.clone()).build())
        .build();
    (provider, exporter)
}

fn meter(provider: &SdkMeterProvider, prefix: &str) -> UcMetricsMeter {
    UcMetricsMeter::new(
        &provider.meter("usage-collector"),
        prefix,
        crate::domain::service::DEFAULT_MAX_BATCH_RECORDS,
    )
}

fn counter_sum(exporter: &InMemoryMetricExporter, name: &str) -> u64 {
    let metrics = exporter.get_finished_metrics().unwrap();
    for rm in &metrics {
        for sm in rm.scope_metrics() {
            for metric in sm.metrics() {
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

fn counter_sum_with_label(
    exporter: &InMemoryMetricExporter,
    name: &str,
    key: &str,
    value: &str,
) -> u64 {
    let metrics = exporter.get_finished_metrics().unwrap();
    for rm in &metrics {
        for sm in rm.scope_metrics() {
            for metric in sm.metrics() {
                if metric.name() == name
                    && let AggregatedMetrics::U64(MetricData::Sum(sum)) = metric.data()
                {
                    return sum
                        .data_points()
                        .filter(|dp| {
                            dp.attributes()
                                .any(|kv| kv.key.as_str() == key && kv.value.as_str() == value)
                        })
                        .map(opentelemetry_sdk::metrics::data::SumDataPoint::value)
                        .sum();
                }
            }
        }
    }
    0
}

fn gauge_last(exporter: &InMemoryMetricExporter, name: &str) -> Option<i64> {
    let metrics = exporter.get_finished_metrics().unwrap();
    for rm in &metrics {
        for sm in rm.scope_metrics() {
            for metric in sm.metrics() {
                if metric.name() == name
                    && let AggregatedMetrics::I64(MetricData::Gauge(g)) = metric.data()
                {
                    return g
                        .data_points()
                        .next()
                        .map(opentelemetry_sdk::metrics::data::GaugeDataPoint::value);
                }
            }
        }
    }
    None
}

fn histogram_count(exporter: &InMemoryMetricExporter, name: &str) -> u64 {
    let metrics = exporter.get_finished_metrics().unwrap();
    for rm in &metrics {
        for sm in rm.scope_metrics() {
            for metric in sm.metrics() {
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

fn histogram_count_with_label(
    exporter: &InMemoryMetricExporter,
    name: &str,
    key: &str,
    value: &str,
) -> u64 {
    let metrics = exporter.get_finished_metrics().unwrap();
    for rm in &metrics {
        for sm in rm.scope_metrics() {
            for metric in sm.metrics() {
                if metric.name() == name
                    && let AggregatedMetrics::F64(MetricData::Histogram(h)) = metric.data()
                {
                    return h
                        .data_points()
                        .filter(|dp| {
                            dp.attributes()
                                .any(|kv| kv.key.as_str() == key && kv.value.as_str() == value)
                        })
                        .map(opentelemetry_sdk::metrics::data::HistogramDataPoint::count)
                        .sum();
                }
            }
        }
    }
    0
}

fn counter_label_count(exporter: &InMemoryMetricExporter, name: &str) -> usize {
    let metrics = exporter.get_finished_metrics().unwrap();
    for rm in &metrics {
        for sm in rm.scope_metrics() {
            for metric in sm.metrics() {
                if metric.name() == name
                    && let AggregatedMetrics::U64(MetricData::Sum(sum)) = metric.data()
                {
                    return sum.data_points().map(|dp| dp.attributes().count()).sum();
                }
            }
        }
    }
    0
}

fn counter_description(exporter: &InMemoryMetricExporter, name: &str) -> Option<String> {
    let metrics = exporter.get_finished_metrics().unwrap();
    for rm in &metrics {
        for sm in rm.scope_metrics() {
            for metric in sm.metrics() {
                if metric.name() == name {
                    return Some(metric.description().to_owned());
                }
            }
        }
    }
    None
}

fn histogram_bounds(exporter: &InMemoryMetricExporter, name: &str) -> Option<Vec<f64>> {
    let metrics = exporter.get_finished_metrics().unwrap();
    for rm in &metrics {
        for sm in rm.scope_metrics() {
            for metric in sm.metrics() {
                if metric.name() == name
                    && let AggregatedMetrics::F64(MetricData::Histogram(h)) = metric.data()
                {
                    return h.data_points().next().map(|dp| dp.bounds().collect());
                }
            }
        }
    }
    None
}

// ── PDP-helper instruments ───────────────────────────────────────────

#[test]
fn pdp_decision_records_authz_decisions_and_duration() {
    let (provider, exporter) = local_provider();
    let m = meter(&provider, TEST_PREFIX);

    m.record_pdp_decision(PdpOp::Ingest, AuthzDecision::Permit, 0.02);
    m.record_pdp_decision(PdpOp::QueryRaw, AuthzDecision::Deny, 0.03);

    provider.force_flush().unwrap();

    assert_eq!(counter_sum(&exporter, "uc_authz_decisions_total"), 2);
    assert_eq!(
        counter_sum_with_label(&exporter, "uc_authz_decisions_total", "decision", "permit"),
        1,
    );
    assert_eq!(
        counter_sum_with_label(&exporter, "uc_authz_decisions_total", "decision", "deny"),
        1,
    );
    assert_eq!(
        counter_sum_with_label(&exporter, "uc_authz_decisions_total", "operation", "ingest"),
        1,
    );
    assert_eq!(histogram_count(&exporter, "uc_pdp_duration_seconds"), 2);
}

#[test]
fn pdp_failure_records_failures_and_duration() {
    let (provider, exporter) = local_provider();
    let m = meter(&provider, TEST_PREFIX);

    m.record_pdp_failure(PdpOp::Ingest, PdpFailureCause::Unreachable, 0.5);

    provider.force_flush().unwrap();

    assert_eq!(counter_sum(&exporter, "uc_pdp_failures_total"), 1);
    assert_eq!(
        counter_sum_with_label(&exporter, "uc_pdp_failures_total", "cause", "unreachable"),
        1,
    );
    // A failure completion is still a completion — duration is observed.
    assert_eq!(histogram_count(&exporter, "uc_pdp_duration_seconds"), 1);
}

#[test]
fn pdp_duration_buckets_match_design_3_11_5() {
    let (provider, exporter) = local_provider();
    let m = meter(&provider, TEST_PREFIX);
    m.record_pdp_decision(PdpOp::Ingest, AuthzDecision::Permit, 0.01);
    provider.force_flush().unwrap();

    assert_eq!(
        histogram_bounds(&exporter, "uc_pdp_duration_seconds"),
        Some(vec![
            0.001, 0.0025, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5
        ]),
    );
}

#[test]
fn pdp_ready_gauge_reflects_binding() {
    let (provider, exporter) = local_provider();
    let m = meter(&provider, TEST_PREFIX);
    m.set_pdp_ready(true);
    provider.force_flush().unwrap();
    assert_eq!(gauge_last(&exporter, "uc_pdp_ready"), Some(1));
}

// ── Plugin-host instruments ──────────────────────────────────────────

#[test]
fn plugin_call_records_duration_with_buckets() {
    let (provider, exporter) = local_provider();
    let m = meter(&provider, TEST_PREFIX);
    m.record_plugin_call(PluginOp::CreateUsageRecords, 0.05);
    provider.force_flush().unwrap();

    assert_eq!(
        histogram_count(&exporter, "uc_plugin_call_duration_seconds"),
        1,
    );
    assert_eq!(
        histogram_bounds(&exporter, "uc_plugin_call_duration_seconds"),
        Some(vec![
            0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.0, 5.0
        ]),
    );
}

#[test]
fn plugin_accept_error_counter_carries_labels() {
    let (provider, exporter) = local_provider();
    let m = meter(&provider, TEST_PREFIX);
    m.record_plugin_accept_error(PluginOp::GetUsageRecord, PluginErrorCategory::Unready);
    m.record_plugin_accept_error(
        PluginOp::CreateUsageRecords,
        PluginErrorCategory::BackendError,
    );
    provider.force_flush().unwrap();

    assert_eq!(counter_sum(&exporter, "uc_plugin_accept_errors_total"), 2);
    assert_eq!(
        counter_sum_with_label(
            &exporter,
            "uc_plugin_accept_errors_total",
            "error_category",
            "unready",
        ),
        1,
    );
    assert_eq!(
        counter_sum_with_label(
            &exporter,
            "uc_plugin_accept_errors_total",
            "operation",
            "create_usage_records",
        ),
        1,
    );
}

#[test]
fn plugin_ready_gauge_reflects_structural_fact() {
    let (provider, exporter) = local_provider();
    let m = meter(&provider, TEST_PREFIX);
    m.set_plugin_ready(false);
    provider.force_flush().unwrap();
    assert_eq!(gauge_last(&exporter, "uc_plugin_ready"), Some(0));
}

// ── Prefix substitution + bootstrap smoke ────────────────────────────

#[test]
fn prefix_is_substituted_into_every_name() {
    let (provider, exporter) = local_provider();
    let m = meter(&provider, "acme");
    m.record_pdp_decision(PdpOp::GetRecord, AuthzDecision::Permit, 0.01);
    provider.force_flush().unwrap();

    assert_eq!(counter_sum(&exporter, "acme_authz_decisions_total"), 1);
    assert_eq!(counter_sum(&exporter, "uc_authz_decisions_total"), 0);
}

#[test]
fn build_default_adapter_binds_to_global_provider_without_panicking() {
    // Panic-guard smoke test only — NOT coverage of exported data. In the test
    // process the global provider is the NoopMeterProvider, so nothing is wired
    // to a reader and the emitted series can't be read back; this only proves
    // construction against the global provider and a record call don't panic.
    let m = build_default_adapter("uc", crate::domain::service::DEFAULT_MAX_BATCH_RECORDS);
    m.set_pdp_ready(true);
    m.record_plugin_call(PluginOp::ListUsageRecords, 0.001);
    // The constructor hands back a live, uniquely-owned handle ready to share.
    assert_eq!(std::sync::Arc::strong_count(&m), 1);
}

// ── Phase 2: ingestion-gateway instruments ───────────────────────────

#[test]
fn ingestion_instruments_render_names_labels_and_buckets() {
    let (provider, exporter) = local_provider();
    let m = meter(&provider, TEST_PREFIX);

    m.observe_ingestion_batch_size(20);
    m.observe_ingestion_duration(0.05, RecordOrigin::Live);
    m.observe_record_metadata_bytes(1500);
    m.record_ingestion_record(
        RecordOutcome::Accepted,
        EntryType::Invalidation,
        RecordOrigin::Backfill,
        RecordErrorCategory::None,
    );
    m.record_ingestion_record(
        RecordOutcome::Rejected,
        EntryType::Record,
        RecordOrigin::Live,
        RecordErrorCategory::MetadataSize,
    );
    m.record_ingestion_request(
        IngestRequestOutcome::Partial,
        IngestRequestErrorCategory::None,
    );
    provider.force_flush().unwrap();

    assert_eq!(histogram_count(&exporter, "uc_ingestion_batch_size"), 1);
    assert_eq!(
        histogram_bounds(&exporter, "uc_ingestion_batch_size"),
        Some(vec![1.0, 2.0, 5.0, 10.0, 20.0, 50.0, 100.0]),
    );
    assert_eq!(
        histogram_count(&exporter, "uc_ingestion_duration_seconds"),
        1
    );
    assert_eq!(
        histogram_bounds(&exporter, "uc_ingestion_duration_seconds"),
        Some(vec![0.01, 0.025, 0.05, 0.1, 0.15, 0.2, 0.3, 0.5, 1.0]),
    );
    assert_eq!(histogram_count(&exporter, "uc_record_metadata_bytes"), 1);
    assert_eq!(
        histogram_bounds(&exporter, "uc_record_metadata_bytes"),
        Some(vec![256.0, 512.0, 1024.0, 2048.0, 4096.0, 8192.0]),
    );
    assert_eq!(counter_sum(&exporter, "uc_ingestion_records_total"), 2);
    assert_eq!(
        counter_sum_with_label(
            &exporter,
            "uc_ingestion_records_total",
            "entry_type",
            "invalidation",
        ),
        1,
    );
    assert_eq!(
        counter_sum_with_label(
            &exporter,
            "uc_ingestion_records_total",
            "error_category",
            "metadata_size",
        ),
        1,
    );
    // The two counter calls were given different origins, so a label pinned
    // to a constant rather than read from the argument fails one of these.
    assert_eq!(
        counter_sum_with_label(&exporter, "uc_ingestion_records_total", "origin", "live"),
        1,
    );
    assert_eq!(
        counter_sum_with_label(
            &exporter,
            "uc_ingestion_records_total",
            "origin",
            "backfill"
        ),
        1,
    );
    assert_eq!(
        counter_sum_with_label(
            &exporter,
            "uc_ingestion_requests_total",
            "outcome",
            "partial"
        ),
        1,
    );
}

/// Every [`RecordErrorCategory`] variant paired with its **hand-written**
/// declared spelling, consumed by both tests below so there is exactly one
/// array to keep in sync with the enum, not two — the same idiom
/// [`ALL_FEED_ERROR_CATEGORIES`] establishes (fix round F1/F2): the test
/// above drives only two of the eight values (`None`, `MetadataSize`), so a
/// misspelled `as_str` on any of the other six — `SemanticsViolation`
/// included — passed unnoticed. The negative assertion elsewhere in this
/// crate (`service_metrics_tests.rs`'s "the invalidation family must not
/// fall back into the semantics one") only checks that
/// `"semantics_violation"` does **not** appear on an unrelated input; it is
/// vacuously true if the spelling itself is wrong, which is exactly why it
/// caught nothing.
///
/// **The second field is a literal, never `RecordErrorCategory::as_str()`**
/// — see [`ALL_FEED_ERROR_CATEGORIES`]'s own doc for why reading the
/// expected label back off the function under test makes the check
/// self-consistent under any mutation to that function.
const ALL_RECORD_ERROR_CATEGORIES: [(RecordErrorCategory, &str); 8] = [
    (RecordErrorCategory::None, "none"),
    (RecordErrorCategory::Authz, "authz"),
    (RecordErrorCategory::UnknownUsageType, "unknown_usage_type"),
    (
        RecordErrorCategory::SemanticsViolation,
        "semantics_violation",
    ),
    (RecordErrorCategory::InvalidationRule, "invalidation_rule"),
    (RecordErrorCategory::MetadataSize, "metadata_size"),
    (
        RecordErrorCategory::IdempotencyConflict,
        "idempotency_conflict",
    ),
    (RecordErrorCategory::PluginError, "plugin_error"),
];

/// Pins every `error_category` wire spelling `uc_ingestion_records_total`
/// declares — a positive assertion per value, not an absence check, so a
/// misspelled `as_str` on any one of them fails here directly.
#[test]
fn every_record_error_category_renders_its_declared_spelling() {
    let (provider, exporter) = local_provider();
    let m = meter(&provider, TEST_PREFIX);

    for (category, _) in ALL_RECORD_ERROR_CATEGORIES {
        m.record_ingestion_record(
            RecordOutcome::Rejected,
            EntryType::Record,
            RecordOrigin::Live,
            category,
        );
    }
    provider.force_flush().unwrap();

    assert_eq!(
        counter_sum(&exporter, "uc_ingestion_records_total"),
        ALL_RECORD_ERROR_CATEGORIES.len() as u64,
    );
    for (_, spelling) in ALL_RECORD_ERROR_CATEGORIES {
        assert_eq!(
            counter_sum_with_label(
                &exporter,
                "uc_ingestion_records_total",
                "error_category",
                spelling,
            ),
            1,
            "expected exactly one \
             uc_ingestion_records_total{{error_category=\"{spelling}\"}} sample",
        );
    }
}

/// Forces a revisit here when [`RecordErrorCategory`] grows a variant — no
/// stronger claim than that — **and** pins [`RecordErrorCategory::as_str`]
/// directly against [`ALL_RECORD_ERROR_CATEGORIES`]'s hand-written
/// spelling, independently of the integration-style check in
/// [`every_record_error_category_renders_its_declared_spelling`]. See
/// [`every_feed_error_category_is_covered_by_the_drive_list`]'s own doc for
/// exactly what the exhaustive-`match` half does and does not guarantee.
#[test]
fn every_record_error_category_is_covered_by_the_vocabulary_test() {
    fn covered(c: RecordErrorCategory) -> bool {
        match c {
            RecordErrorCategory::None
            | RecordErrorCategory::Authz
            | RecordErrorCategory::UnknownUsageType
            | RecordErrorCategory::SemanticsViolation
            | RecordErrorCategory::InvalidationRule
            | RecordErrorCategory::MetadataSize
            | RecordErrorCategory::IdempotencyConflict
            | RecordErrorCategory::PluginError => true,
        }
    }
    for (category, spelling) in ALL_RECORD_ERROR_CATEGORIES {
        assert!(covered(category));
        assert_eq!(
            category.as_str(),
            spelling,
            "ALL_RECORD_ERROR_CATEGORIES's hand-written spelling for {category:?} has \
             drifted from `RecordErrorCategory::as_str`",
        );
    }
}

#[test]
fn batch_size_buckets_end_at_the_configured_cap() {
    use crate::infra::metrics::ingestion_batch_size_buckets;
    assert_eq!(
        ingestion_batch_size_buckets(100),
        vec![1.0, 2.0, 5.0, 10.0, 20.0, 50.0, 100.0]
    );
    assert_eq!(
        ingestion_batch_size_buckets(30),
        vec![1.0, 2.0, 5.0, 10.0, 20.0, 30.0]
    );
    assert_eq!(ingestion_batch_size_buckets(1), vec![1.0]);
    assert_eq!(
        ingestion_batch_size_buckets(500),
        vec![1.0, 2.0, 5.0, 10.0, 20.0, 50.0, 100.0, 500.0]
    );
}

#[test]
fn the_ingestion_instruments_separate_live_from_backfilled_entries() {
    // `uc_ingestion_duration_seconds` carries the latency budget, and a bulk
    // import's latency profile is not the live path's — averaging them
    // together is what would hide a catch-up job degrading live ingestion.
    //
    // The histogram is the half of DESIGN §3.11.5's `origin` pair that no
    // other test can cover: it was label-free before this slice, so both
    // observations used to land in one series, and `histogram_count` /
    // `histogram_bounds` are label-blind — the total of 2 below holds
    // whether these landed in one series or two. Only the per-origin counts
    // tell those apart. The counter's `origin` vocabulary is left to
    // `ingestion_instruments_render_names_labels_and_buckets`, which already
    // drives both values through it.
    let (provider, exporter) = local_provider();
    let m = meter(&provider, TEST_PREFIX);

    m.observe_ingestion_duration(0.05, RecordOrigin::Live);
    m.observe_ingestion_duration(0.4, RecordOrigin::Backfill);
    provider.force_flush().unwrap();

    assert_eq!(
        histogram_count(&exporter, "uc_ingestion_duration_seconds"),
        2
    );
    assert_eq!(
        histogram_count_with_label(&exporter, "uc_ingestion_duration_seconds", "origin", "live"),
        1,
    );
    assert_eq!(
        histogram_count_with_label(
            &exporter,
            "uc_ingestion_duration_seconds",
            "origin",
            "backfill",
        ),
        1,
    );
}

// ── Phase 2: query-gateway instruments ───────────────────────────────

#[test]
fn query_instruments_render_names_labels_and_buckets() {
    let (provider, exporter) = local_provider();
    let m = meter(&provider, TEST_PREFIX);

    m.query_inflight_inc(QueryKind::Aggregated);
    m.query_inflight_inc(QueryKind::Aggregated);
    m.query_inflight_dec(QueryKind::Aggregated);
    m.observe_query_result_rows(QueryKind::Raw, 42);
    m.record_query_request(
        QueryKind::Aggregated,
        RequestOutcome::Success,
        QueryErrorCategory::None,
        0.3,
    );
    m.record_query_request(
        QueryKind::Raw,
        RequestOutcome::Error,
        QueryErrorCategory::QueryBudget,
        0.01,
    );
    provider.force_flush().unwrap();

    // UpDownCounter renders as a non-monotonic sum: +1 +1 -1 = 1.
    assert_eq!(
        query_inflight_value(&exporter, "uc_query_inflight", "aggregated"),
        Some(1),
    );
    assert_eq!(histogram_count(&exporter, "uc_query_result_rows"), 1);
    assert_eq!(
        histogram_bounds(&exporter, "uc_query_result_rows"),
        Some(vec![
            1.0, 10.0, 50.0, 100.0, 500.0, 1000.0, 10000.0, 100_000.0
        ]),
    );
    assert_eq!(histogram_count(&exporter, "uc_query_duration_seconds"), 2);
    assert_eq!(
        histogram_bounds(&exporter, "uc_query_duration_seconds"),
        Some(vec![0.05, 0.1, 0.25, 0.5, 0.75, 1.0, 2.0, 5.0]),
    );
    assert_eq!(counter_sum(&exporter, "uc_query_requests_total"), 2);
    assert_eq!(
        counter_sum_with_label(
            &exporter,
            "uc_query_requests_total",
            "error_category",
            "query_budget"
        ),
        1,
    );
    assert_eq!(
        counter_sum_with_label(
            &exporter,
            "uc_query_requests_total",
            "query_kind",
            "aggregated"
        ),
        1,
    );
}

/// A [`UcMetricsMeter`] paired with the local provider/exporter it is bound
/// to, so a test can drive it and read its emitted series back through the
/// same handle.
struct Recorder {
    meter: UcMetricsMeter,
    provider: SdkMeterProvider,
    exporter: InMemoryMetricExporter,
}

impl std::ops::Deref for Recorder {
    type Target = UcMetricsMeter;

    fn deref(&self) -> &UcMetricsMeter {
        &self.meter
    }
}

fn recorder() -> Recorder {
    let (provider, exporter) = local_provider();
    let meter = meter(&provider, TEST_PREFIX);
    Recorder {
        meter,
        provider,
        exporter,
    }
}

/// Every distinct `query_kind` label value `instrument` emitted on `m`,
/// across every data point of every series recorded so far.
///
/// Reads whichever of the three data shapes the query-gateway instruments
/// use (`u64` `Sum` for the two request counters, `i64` `Sum` for the
/// `UpDownCounter` gauge, `f64` `Histogram` for the two duration/row
/// observations) rather than assuming one, since the four instruments this
/// is read against do not all share a shape.
///
/// The matched value is mapped back onto the closed `&'static str`
/// vocabulary [`crate::domain::ports::metrics::QueryKind::as_str`] defines,
/// rather than returned as the borrowed `Cow<'_, str>` the exporter hands
/// back — panicking on anything else, so a value the enum cannot produce is
/// a loud test failure rather than a silently-accepted fifth label.
fn emitted_query_kinds(m: &Recorder, instrument: &str) -> BTreeSet<&'static str> {
    m.provider.force_flush().expect("flush");
    let metrics = m.exporter.get_finished_metrics().expect("finished metrics");
    let mut kinds = BTreeSet::new();
    let known = |raw: &str| -> &'static str {
        match raw {
            "aggregated" => "aggregated",
            "raw" => "raw",
            "point" => "point",
            "reconciliation" => "reconciliation",
            other => panic!("emitted query_kind `{other}` is not in QueryKind's vocabulary"),
        }
    };
    for rm in &metrics {
        for sm in rm.scope_metrics() {
            for metric in sm.metrics() {
                if metric.name() != instrument {
                    continue;
                }
                match metric.data() {
                    AggregatedMetrics::U64(MetricData::Sum(sum)) => {
                        for dp in sum.data_points() {
                            for kv in dp.attributes() {
                                if kv.key.as_str() == "query_kind" {
                                    kinds.insert(known(&kv.value.as_str()));
                                }
                            }
                        }
                    }
                    AggregatedMetrics::I64(MetricData::Sum(sum)) => {
                        for dp in sum.data_points() {
                            for kv in dp.attributes() {
                                if kv.key.as_str() == "query_kind" {
                                    kinds.insert(known(&kv.value.as_str()));
                                }
                            }
                        }
                    }
                    AggregatedMetrics::F64(MetricData::Histogram(h)) => {
                        for dp in h.data_points() {
                            for kv in dp.attributes() {
                                if kv.key.as_str() == "query_kind" {
                                    kinds.insert(known(&kv.value.as_str()));
                                }
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
    }
    kinds
}

/// Each query instrument emits exactly the `query_kind` values DESIGN
/// §3.11.5 declares for it — which are four different sets, not one.
///
/// Derived from what the code can emit rather than from a restated list: the
/// recorder is driven with every `QueryKind` through every instrument, and
/// the emitted series are read back. A pin that restated §3.11.5's four rows
/// would agree with itself no matter what the enum did.
#[test]
fn each_query_instrument_emits_only_the_query_kinds_its_row_declares() {
    let m = recorder();
    // Every instrument is driven with every `QueryKind`, `Reconciliation` and
    // `Point` included — not only `record_query_request`, which is the only
    // method the first cut of this pin drove. `observe_query_result_rows` and
    // `query_inflight_inc`/`_dec` are never called anywhere else in this
    // test file for `Reconciliation` or `Point`, so without this loop their
    // arms below would read `emitted_query_kinds` against an instrument
    // nothing had written to — an empty set, and `!∅.contains("reconciliation")`
    // is true no matter what the code does. Driving every kind through every
    // instrument is what gives each arm a value it could actually fail on.
    for kind in [
        QueryKind::Aggregated,
        QueryKind::Raw,
        QueryKind::Reconciliation,
        QueryKind::Point,
    ] {
        m.record_query_request(kind, RequestOutcome::Success, QueryErrorCategory::None, 0.1);
        m.observe_query_result_rows(kind, 1);
        m.query_inflight_inc(kind);
        m.query_inflight_dec(kind);
    }
    assert_eq!(
        emitted_query_kinds(&m, "uc_query_requests_total"),
        BTreeSet::from(["aggregated", "raw", "reconciliation", "point"]),
        "DESIGN section 3.11.5 admits `point` here too, for `get_usage_record`; \
         every value this gear emits must be in the row"
    );
    // `uc_query_duration_seconds` gets its own assertion rather than sharing
    // the loop below: DESIGN section 3.11.5 lists `aggregated`, `raw` AND
    // `point` on its row (unlike `uc_query_result_rows` / `uc_query_inflight`,
    // which list `aggregated`, `raw` alone) — the shared message the loop
    // used to give this instrument claimed DESIGN forbids `point` here,
    // which is false. `point` is now driven through `record_query_request`
    // via `QueryKind::Point`, so this set widens to three; `point`'s
    // presence is a fact about what the row admits, not merely about the
    // enum's membership.
    assert_eq!(
        emitted_query_kinds(&m, "uc_query_duration_seconds"),
        BTreeSet::from(["aggregated", "raw", "point"]),
        "DESIGN section 3.11.5 admits `point` here too (for `get_usage_record`) and \
         never `reconciliation`; a call this test makes with `QueryKind::Reconciliation` \
         must never reach this instrument"
    );
    // Now that `QueryKind::Point` is driven through the loop above alongside
    // `Reconciliation`, this is the real negative pin for `point` on both
    // instruments, not just for `reconciliation`: it is enforced through
    // `admits_result_rows_sample` and `admits_inflight_gauge`, both of which
    // give `Point` the same `false` arm as `Reconciliation`.
    for instrument in ["uc_query_result_rows", "uc_query_inflight"] {
        assert_eq!(
            emitted_query_kinds(&m, instrument),
            BTreeSet::from(["aggregated", "raw"]),
            "`{instrument}` admits `aggregated`, `raw` alone in DESIGN section 3.11.5 — no \
             `point` and no `reconciliation` — so a call this test makes with \
             `QueryKind::Reconciliation` or `QueryKind::Point` (or any future kind DESIGN \
             does not list here) must never reach it"
        );
    }
}

// ── Phase 2: feed-gateway instruments ────────────────────────────────

/// Every [`FeedErrorCategory`] variant, the `outcome` its one real producer
/// completes with, and its **hand-written** declared spelling, consumed by
/// the drive loop, the vocabulary loop and
/// [`every_feed_error_category_is_covered_by_the_drive_list`] below so there
/// is exactly one array to keep in sync with the enum, not three.
///
/// Mirrors the idiom
/// `infra::metrics_inventory::metrics_inventory_tests::ALL_TYPE_RESOLUTION_OUTCOMES`
/// establishes (fix round F1): before, this file's drive list and its
/// vocabulary-loop literal were two independently-written 5-element lists,
/// and a sixth enum variant could be added without either one — or the
/// `counter_sum` total below — changing, so a new value passed unnoticed.
///
/// **The third field is a literal, never `FeedErrorCategory::as_str()`.**
/// An earlier draft of this fix round read the expected label back off
/// `as_str()` itself, which made the vocabulary loop self-consistent under
/// any mutation to `as_str()` — the production call and the assertion both
/// read the same (possibly wrong) function, so a mutant renaming
/// `"argument_rejected"` to `"totally_bogus_mutant"` left every assertion
/// passing. Hand-written literals are what make the two sides independent.
const ALL_FEED_ERROR_CATEGORIES: [(RequestOutcome, FeedErrorCategory, &str); 6] = [
    (RequestOutcome::Success, FeedErrorCategory::None, "none"),
    (RequestOutcome::Denied, FeedErrorCategory::Authz, "authz"),
    (
        RequestOutcome::Error,
        FeedErrorCategory::CursorDecode,
        "cursor_decode",
    ),
    (
        RequestOutcome::Error,
        FeedErrorCategory::CursorBeyondRetention,
        "cursor_beyond_retention",
    ),
    (
        RequestOutcome::Error,
        FeedErrorCategory::ArgumentRejected,
        "argument_rejected",
    ),
    (
        RequestOutcome::Error,
        FeedErrorCategory::PluginError,
        "plugin_error",
    ),
];

/// The three DESIGN §3.11.5 feed instruments, rendered.
///
/// Every name here is a literal that appears nowhere in the builder — the
/// meter composes it from a configured prefix — so this is the only place a
/// mistyped `uc_feed_page_entries` or a dropped `_total` is visible at all.
/// The `error_category` loop and the drive loop both walk
/// [`ALL_FEED_ERROR_CATEGORIES`], and
/// [`every_feed_error_category_is_covered_by_the_drive_list`]'s exhaustive
/// `match` is a compile error on a seventh variant — see that test's own doc
/// for exactly what that compile error does and does not guarantee about the
/// array.
#[test]
fn feed_instruments_render_names_labels_and_buckets() {
    let (provider, exporter) = local_provider();
    let m = meter(&provider, TEST_PREFIX);

    m.observe_feed_page_entries(42);
    for (outcome, category, _) in ALL_FEED_ERROR_CATEGORIES {
        m.record_feed_request(outcome, category, 0.2);
    }
    provider.force_flush().unwrap();

    // One sample per `record_feed_request` call, and no more: the duration
    // rides the counter, so the two can never disagree about how many feed
    // requests completed.
    let total = ALL_FEED_ERROR_CATEGORIES.len() as u64;
    assert_eq!(counter_sum(&exporter, "uc_feed_requests_total"), total);
    assert_eq!(
        histogram_count(&exporter, "uc_feed_page_duration_seconds"),
        total,
    );
    assert_eq!(
        histogram_bounds(&exporter, "uc_feed_page_duration_seconds"),
        Some(vec![0.05, 0.1, 0.25, 0.5, 1.0, 2.0, 5.0, 10.0]),
    );

    assert_eq!(histogram_count(&exporter, "uc_feed_page_entries"), 1);
    assert_eq!(
        histogram_bounds(&exporter, "uc_feed_page_entries"),
        Some(vec![1.0, 10.0, 50.0, 100.0, 500.0, 1000.0, 5000.0]),
    );

    for (_, _, spelling) in ALL_FEED_ERROR_CATEGORIES {
        assert_eq!(
            counter_sum_with_label(
                &exporter,
                "uc_feed_requests_total",
                "error_category",
                spelling
            ),
            1,
            "expected exactly one \
             uc_feed_requests_total{{error_category=\"{spelling}\"}} sample",
        );
    }
    for (outcome, expected) in [("success", 1), ("denied", 1), ("error", 4)] {
        assert_eq!(
            counter_sum_with_label(&exporter, "uc_feed_requests_total", "outcome", outcome),
            expected,
            "expected {expected} uc_feed_requests_total{{outcome=\"{outcome}\"}} samples",
        );
    }
}

/// Forces a revisit here when [`FeedErrorCategory`] grows a variant — no
/// stronger claim than that — **and** pins [`FeedErrorCategory::as_str`]
/// directly against [`ALL_FEED_ERROR_CATEGORIES`]'s hand-written spelling,
/// independently of the integration-style check in
/// [`feed_instruments_render_names_labels_and_buckets`].
///
/// The exhaustive `match` is a compile error on a seventh variant, so
/// adding one cannot ship silently *unnoticed by this function*. What it
/// does **not** do is reach into [`ALL_FEED_ERROR_CATEGORIES`] or the two
/// loops above and extend any of them for you — a human still has to widen
/// `covered`'s match arms, the array, and (if the new value needs a
/// non-`Error` outcome) the drive loop by hand.
#[test]
fn every_feed_error_category_is_covered_by_the_drive_list() {
    fn covered(c: FeedErrorCategory) -> bool {
        match c {
            FeedErrorCategory::None
            | FeedErrorCategory::Authz
            | FeedErrorCategory::CursorDecode
            | FeedErrorCategory::CursorBeyondRetention
            | FeedErrorCategory::ArgumentRejected
            | FeedErrorCategory::PluginError => true,
        }
    }
    for (_, category, spelling) in ALL_FEED_ERROR_CATEGORIES {
        assert!(covered(category));
        assert_eq!(
            category.as_str(),
            spelling,
            "ALL_FEED_ERROR_CATEGORIES's hand-written spelling for {category:?} has \
             drifted from `FeedErrorCategory::as_str`",
        );
    }
}

// ── Type Resolver instrument ──────────────────────────────────────────

#[test]
fn type_resolution_counter_renders_name_and_result_labels() {
    let (provider, exporter) = local_provider();
    let m = meter(&provider, TEST_PREFIX);

    m.record_type_resolution(TypeResolutionOutcome::CacheHit);
    m.record_type_resolution(TypeResolutionOutcome::CacheMiss);
    m.record_type_resolution(TypeResolutionOutcome::ServedStale);
    m.record_type_resolution(TypeResolutionOutcome::Restored);
    m.record_type_resolution(TypeResolutionOutcome::Unresolved);
    m.record_type_resolution(TypeResolutionOutcome::RegistryError);
    provider.force_flush().unwrap();

    assert_eq!(counter_sum(&exporter, "uc_type_resolution_total"), 6);
    for result in [
        "cache_hit",
        "cache_miss",
        "served_stale",
        "restored",
        "unresolved",
        "registry_error",
    ] {
        assert_eq!(
            counter_sum_with_label(&exporter, "uc_type_resolution_total", "result", result),
            1,
            "expected exactly one uc_type_resolution_total{{result=\"{result}\"}} sample",
        );
    }
}

#[test]
fn resolved_types_gauge_renders_name_and_count() {
    let (provider, exporter) = local_provider();
    let m = meter(&provider, TEST_PREFIX);

    m.set_resolved_types(3);
    provider.force_flush().unwrap();

    assert_eq!(gauge_last(&exporter, "uc_resolved_types"), Some(3));
}

#[test]
fn declaration_cache_age_gauge_records_some_but_not_none() {
    // Step 11: the `None` arm is easy to pin at the port and leave
    // unexercised in the adapter, where the "record nothing" decision
    // actually lives — so both arms are driven here, against the real
    // adapter and a real exporter, not just the port signature.
    let (provider, exporter) = local_provider();
    let m = meter(&provider, TEST_PREFIX);

    m.set_declaration_cache_age_seconds(None);
    provider.force_flush().unwrap();
    assert_eq!(
        gauge_last(&exporter, "uc_declaration_cache_age_seconds"),
        None,
        "an empty cache must leave no point on this series, not a recorded zero",
    );

    m.set_declaration_cache_age_seconds(Some(5));
    provider.force_flush().unwrap();
    assert_eq!(
        gauge_last(&exporter, "uc_declaration_cache_age_seconds"),
        Some(5),
    );
}

#[test]
fn type_resolution_duration_histogram_has_the_design_buckets_and_only_hit_or_miss_labels() {
    let (provider, exporter) = local_provider();
    let m = meter(&provider, TEST_PREFIX);

    m.observe_type_resolution_duration(TypeResolutionOutcome::CacheHit, 0.0005);
    m.observe_type_resolution_duration(TypeResolutionOutcome::CacheMiss, 0.02);
    // §3.11.5 declares only two `result` values on this row; the other four
    // outcomes must never reach the histogram at all.
    m.observe_type_resolution_duration(TypeResolutionOutcome::ServedStale, 0.02);
    m.observe_type_resolution_duration(TypeResolutionOutcome::Restored, 0.02);
    m.observe_type_resolution_duration(TypeResolutionOutcome::Unresolved, 0.02);
    m.observe_type_resolution_duration(TypeResolutionOutcome::RegistryError, 0.02);
    provider.force_flush().unwrap();

    assert_eq!(
        histogram_count(&exporter, "uc_type_resolution_duration_seconds"),
        2,
        "only the cache_hit and cache_miss observations above must land on this series",
    );
    // Fix-round finding: the count assertion above cannot tell "recorded
    // under the right label" apart from "recorded under a hard-coded
    // wrong one" -- a mutation that labelled every point `result="cache_hit"`
    // would still leave the series at 2 points. These two name the label
    // each observation must carry.
    assert_eq!(
        histogram_count_with_label(
            &exporter,
            "uc_type_resolution_duration_seconds",
            "result",
            "cache_hit",
        ),
        1,
        "the CacheHit observation must carry result=\"cache_hit\"",
    );
    assert_eq!(
        histogram_count_with_label(
            &exporter,
            "uc_type_resolution_duration_seconds",
            "result",
            "cache_miss",
        ),
        1,
        "the CacheMiss observation must carry result=\"cache_miss\"",
    );
    assert_eq!(
        histogram_bounds(&exporter, "uc_type_resolution_duration_seconds"),
        Some(vec![0.0001, 0.001, 0.005, 0.01, 0.05, 0.1, 0.5]),
        "DESIGN \u{a7}3.11.5's bucket layout for uc_type_resolution_duration_seconds",
    );
}

/// Pins three facts about `uc_declaration_mirror_write_failures_total` that
/// nothing else in the tree checks against a real exporter: its exact
/// rendered name (including the `_total` suffix), that it carries **no**
/// label (DESIGN §3.11.5's label column for this row is `\u{2014}`), and that
/// repeated calls sum rather than overwrite. Also pins the companion J14
/// fix on `uc_type_resolution_total`'s own exported description, which an
/// operator reads in their metrics backend rather than in this source —
/// nothing else in the tree reads any instrument's `description()` at all.
#[test]
fn declaration_mirror_write_failure_counter_is_unlabelled_and_the_type_resolution_description_names_restored()
 {
    let (provider, exporter) = local_provider();
    let m = meter(&provider, TEST_PREFIX);

    m.record_declaration_mirror_write_failure();
    m.record_declaration_mirror_write_failure();
    // Drives `uc_type_resolution_total` into the exported set at all --
    // Task 4's own Step 9 measurement established that a declared-but-never
    // -incremented OTel counter is invisible to an in-memory exporter.
    m.record_type_resolution(TypeResolutionOutcome::CacheHit);
    provider.force_flush().unwrap();

    assert_eq!(
        counter_sum(&exporter, "uc_declaration_mirror_write_failures_total"),
        2,
        "two calls to record_declaration_mirror_write_failure must sum to two under the \
         exact DESIGN \u{a7}3.11.5 name, `_total` suffix included -- a renamed or \
         never-incrementing counter would read 0 here",
    );
    assert_eq!(
        counter_label_count(&exporter, "uc_declaration_mirror_write_failures_total"),
        0,
        "DESIGN \u{a7}3.11.5's label column for this row is `\u{2014}` -- any label here, \
         bounded or not, is a cardinality defect ruling I5 exists to catch",
    );

    // Slice 8b final fix round, item 5.1: an equality on the WHOLE
    // description, not `contains("restored")`. This slice's own standing
    // rule is "assert by equality on a field, never `contains` over a
    // rendered line", and the old form broke it in a way that mattered: it
    // was not vacuous (it reds on the deletion it was written for) but it
    // also passed on `"unrestored"`, on `"restored_maybe"`, and on a
    // description that had lost any of the other five `result` values while
    // keeping this one. The equality below pins all six names, their order,
    // and the sentence an operator actually reads in their metrics backend.
    let type_resolution_description = counter_description(&exporter, "uc_type_resolution_total")
        .expect("uc_type_resolution_total is exported once a call drives it");
    assert_eq!(
        type_resolution_description,
        "Completed Type Resolver calls by result (cache_hit / cache_miss / \
         served_stale / restored / unresolved / registry_error)",
        "the exported description (J14) must name every one of this counter's \
         declared `result` values, `restored` included -- an operator reads this \
         string in their metrics backend, not in the source",
    );
}

/// Read the summed value of an `i64` `UpDownCounter` series filtered to a
/// `query_kind` label.
fn query_inflight_value(exporter: &InMemoryMetricExporter, name: &str, kind: &str) -> Option<i64> {
    let metrics = exporter.get_finished_metrics().unwrap();
    for rm in &metrics {
        for sm in rm.scope_metrics() {
            for metric in sm.metrics() {
                if metric.name() == name
                    && let AggregatedMetrics::I64(MetricData::Sum(sum)) = metric.data()
                {
                    return Some(
                        sum.data_points()
                            .filter(|dp| {
                                dp.attributes().any(|kv| {
                                    kv.key.as_str() == "query_kind" && kv.value.as_str() == kind
                                })
                            })
                            .map(opentelemetry_sdk::metrics::data::SumDataPoint::value)
                            .sum(),
                    );
                }
            }
        }
    }
    None
}
