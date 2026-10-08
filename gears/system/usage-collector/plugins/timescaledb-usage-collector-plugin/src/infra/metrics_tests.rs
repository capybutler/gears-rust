use super::{DURATION_BOUNDARIES_SECS, ErrorClass, Metrics, QueryKind, SweepOutcome, label};
use crate::domain::retention::KeepReason;
use crate::infra::storage::query::rollup::FallbackReason;
use crate::infra::storage::rollup_maintenance::{RefreshJobStatus, RefreshPolicy};

use opentelemetry::metrics::MeterProvider;
use opentelemetry_sdk::metrics::data::{AggregatedMetrics, MetricData};
use opentelemetry_sdk::metrics::{InMemoryMetricExporter, PeriodicReader, SdkMeterProvider};
use sqlx::postgres::PgPoolOptions;

/// A lazy pool (`connect_lazy`) yields a `PgPool` handle without opening a
/// connection, so the metrics tests stay pure no-DB unit tests. `connect_lazy`
/// spawns the pool's background maintenance task, which is why these are
/// `#[tokio::test]` (a Tokio context is required; no DB connection is opened).
fn lazy_pool() -> sqlx::PgPool {
    PgPoolOptions::new()
        .connect_lazy("postgres://user:pass@localhost/db")
        .expect("a syntactically valid DSN yields a lazy pool without connecting")
}

/// A local `SdkMeterProvider` backed by an in-memory exporter. Local (not the
/// process-global) provider so the recording assertions are parallel-safe:
/// [`Metrics::with_meter`] takes the meter explicitly, so this test never
/// mutates `opentelemetry::global` state.
fn local_provider() -> (SdkMeterProvider, InMemoryMetricExporter) {
    let exporter = InMemoryMetricExporter::default();
    let provider = SdkMeterProvider::builder()
        .with_reader(PeriodicReader::builder(exporter.clone()).build())
        .build();
    (provider, exporter)
}

/// Total of all `u64` Sum (counter) data points named `name`.
fn counter_sum(exporter: &InMemoryMetricExporter, name: &str) -> u64 {
    let metrics = exporter.get_finished_metrics().unwrap();
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

/// Total of the `u64` Sum (counter) data points named `name` carrying
/// `label_key == label_value`.
fn counter_sum_with_label(
    exporter: &InMemoryMetricExporter,
    name: &str,
    label_key: &str,
    label_value: &str,
) -> u64 {
    let metrics = exporter.get_finished_metrics().unwrap();
    for resource_metrics in &metrics {
        for scope_metrics in resource_metrics.scope_metrics() {
            for metric in scope_metrics.metrics() {
                if metric.name() == name
                    && let AggregatedMetrics::U64(MetricData::Sum(sum)) = metric.data()
                {
                    return sum
                        .data_points()
                        .filter(|dp| {
                            dp.attributes().any(|kv| {
                                kv.key.as_str() == label_key && kv.value.as_str() == label_value
                            })
                        })
                        .map(opentelemetry_sdk::metrics::data::SumDataPoint::value)
                        .sum();
                }
            }
        }
    }
    0
}

/// Every instrument name the meter exported, sorted.
fn exported_names(exporter: &InMemoryMetricExporter) -> Vec<String> {
    let metrics = exporter.get_finished_metrics().unwrap();
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

/// Last value of the `u64` Gauge named `name`, if recorded.
fn gauge_last_u64(exporter: &InMemoryMetricExporter, name: &str) -> Option<u64> {
    let metrics = exporter.get_finished_metrics().unwrap();
    for resource_metrics in &metrics {
        for scope_metrics in resource_metrics.scope_metrics() {
            for metric in scope_metrics.metrics() {
                if metric.name() == name
                    && let AggregatedMetrics::U64(MetricData::Gauge(g)) = metric.data()
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

/// Total observation count across the `f64` Histogram data points named `name`.
fn histogram_count(exporter: &InMemoryMetricExporter, name: &str) -> u64 {
    let metrics = exporter.get_finished_metrics().unwrap();
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

/// Bucket boundaries of the first `f64` Histogram data point named `name`.
///
/// The oracle in
/// [`the_two_open_layout_histograms_carry_no_explicit_bucket_layout`], where the
/// pinned property is "matches the SDK's own default", read back off a locally
/// recorded default-configured histogram rather than a hardcoded list.
fn histogram_bounds(exporter: &InMemoryMetricExporter, name: &str) -> Vec<f64> {
    let metrics = exporter.get_finished_metrics().unwrap();
    for resource_metrics in &metrics {
        for scope_metrics in resource_metrics.scope_metrics() {
            for metric in scope_metrics.metrics() {
                if metric.name() == name
                    && let AggregatedMetrics::F64(MetricData::Histogram(h)) = metric.data()
                {
                    return h
                        .data_points()
                        .next()
                        .map(|dp| dp.bounds().collect())
                        .unwrap_or_default();
                }
            }
        }
    }
    Vec::new()
}

/// The recording helpers [`recording_helpers_emit_expected_series`] does not
/// exercise emit their expected series through the [`Metrics::with_meter`] seam.
///
/// Also smoke-checks that [`Metrics::new`] builds the full inventory against the
/// process-global provider without panicking. Recordings on that handle are a
/// safe no-op (no reader is installed in the test process), so only the local
/// `with_meter` provider is read back.
#[tokio::test]
async fn remaining_recording_helpers_emit_expected_series() {
    // Global-provider construction path (the production entry point): building
    // the full inventory and recording against it must not panic.
    let global = Metrics::new(lazy_pool());
    global.set_ready(true);
    global.inc_dedup_absorbed();
    global.inc_dedup_stale();
    global.record_insert(0.001);
    global.record_query(QueryKind::Aggregated, 0.002);
    global.inc_backend_error(ErrorClass::Transient);

    // Value assertions via a local in-memory reader.
    let (provider, exporter) = local_provider();
    let metrics = Metrics::with_meter(&provider.meter("uc.timescaledb"), lazy_pool());

    metrics.inc_dedup_stale();
    metrics.inc_dedup_stale();
    metrics.inc_stale_acceptance_rejection();
    metrics.inc_batch_retry();
    metrics.inc_batch_retry();
    metrics.inc_batch_retry();
    metrics.record_insert(0.001);
    metrics.record_query(QueryKind::Aggregated, 0.002);

    provider.force_flush().unwrap();

    assert_eq!(
        counter_sum(&exporter, "uc_timescaledb_dedup_stale_total"),
        2
    );
    // Driven once, against the twice-driven counter beside it: the two are
    // separate series, and a helper wired to the wrong instrument would show up
    // here as one of them carrying the other's count.
    assert_eq!(
        counter_sum(
            &exporter,
            "uc_timescaledb_stale_acceptance_rejections_total"
        ),
        1
    );
    assert_eq!(
        counter_sum(&exporter, "uc_timescaledb_batch_retries_total"),
        3,
    );
    assert_eq!(
        histogram_count(&exporter, "uc_timescaledb_insert_duration_seconds"),
        1,
    );
    assert_eq!(
        histogram_count(&exporter, "uc_timescaledb_query_duration_seconds"),
        1,
    );
}

/// With an in-memory reader installed, the recording helpers must emit the
/// expected counter / gauge / histogram series — covering a plain counter, a
/// label-split counter, a gauge, and a histogram.
#[tokio::test]
async fn recording_helpers_emit_expected_series() {
    let (provider, exporter) = local_provider();
    let metrics = Metrics::with_meter(&provider.meter("uc.timescaledb"), lazy_pool());

    // Counter (plain): three absorbed-dedup increments accumulate to 3.
    metrics.inc_dedup_absorbed();
    metrics.inc_dedup_absorbed();
    metrics.inc_dedup_absorbed();

    // Counter (labelled): backend errors split by `error_category`.
    metrics.inc_backend_error(ErrorClass::Transient);
    metrics.inc_backend_error(ErrorClass::Transient);
    metrics.inc_backend_error(ErrorClass::Internal);

    // Gauge: last-value semantics.
    metrics.set_ready(true);

    // Histogram (unlabelled): two insert observations.
    metrics.record_insert(0.01);
    metrics.record_insert(0.02);

    provider.force_flush().unwrap();

    assert_eq!(
        counter_sum(&exporter, "uc_timescaledb_dedup_absorbed_total"),
        3,
    );
    assert_eq!(
        counter_sum(&exporter, "uc_timescaledb_backend_errors_total"),
        3,
    );
    assert_eq!(
        counter_sum_with_label(
            &exporter,
            "uc_timescaledb_backend_errors_total",
            label::ERROR_CATEGORY,
            label::ERROR_CATEGORY_TRANSIENT,
        ),
        2,
    );
    assert_eq!(gauge_last_u64(&exporter, "uc_timescaledb_ready"), Some(1));
    assert_eq!(
        histogram_count(&exporter, "uc_timescaledb_insert_duration_seconds"),
        2,
    );
}

/// Every instrument the inventory exports obeys the naming convention the
/// module doc states — asserted off each instrument's **kind**, over the exact
/// exported set.
///
/// The module doc names the rules §3.11.5 binds this crate by. This is the
/// mechanism for the naming one; the bounded-label rule has the closed
/// `as_label` enums as its own. Without this the convention is a paragraph and a
/// new instrument with a dotted name, a missing `_total` or a `_secs` suffix is
/// caught by nobody.
///
/// **Kind, not spelling — including which histograms are durations.** `_total`
/// is asserted of everything the SDK exports as a `Sum`, so every declared
/// counter is covered rather than the subset a hardcoded name list held. The
/// `_seconds` suffix and the **`DURATION_BOUNDARIES_SECS` bucket layout** must
/// agree **in both directions**, the layout read back off the exported bounds —
/// not "every histogram whose name contains duration", which only inspects names
/// that already announce themselves, and biconditional so a non-duration
/// histogram *gaining* the suffix reds too. `batch_rows` is the f64 histogram
/// correctly *not* in seconds, and `BATCH_ROW_BOUNDARIES` is what says so.
///
/// **The exported set must equal [`Metrics::declared_instrument_names`]**, not
/// merely reach some floor. A floor cannot notice an instrument disappearing,
/// and it hid an untested belief: that the observable pool gauges are collected
/// by their callbacks on this path.
///
/// Two mechanisms catch different halves of a new instrument, neither quite a
/// guarantee alone: `declared_instrument_names`' destructure has no `..`, so the
/// compiler will not let anyone *reach* that list without being shown the new
/// field, and this equality stays red until the instrument is both named there
/// and driven below. Adding `foo: _` to silence `E0027`, omitting the string,
/// and never driving it would still be green.
///
/// [`OPEN_LAYOUT_HISTOGRAMS`] is excepted from the bounds check below: those
/// histograms' layout is deliberately the SDK default rather than the explicit
/// one.
const OPEN_LAYOUT_HISTOGRAMS: &[&str] = &[
    "uc_timescaledb_feed_page_duration_seconds",
    "uc_timescaledb_reconciliation_duration_seconds",
];

#[tokio::test]
async fn every_exported_instrument_obeys_the_naming_convention() {
    let (provider, exporter) = local_provider();
    let metrics = Metrics::with_meter(&provider.meter("uc.timescaledb"), lazy_pool());

    // Every recording helper, and every call is load-bearing: an instrument the
    // SDK has built but never recorded on is **not exported at all** -- counter,
    // histogram and gauge alike. So the equality assertion below reds until a
    // new instrument is driven here. The observable pool gauges have no helper;
    // that same assertion establishes that their callbacks fire on flush.
    metrics.record_insert(0.001);
    metrics.record_query(QueryKind::Raw, 0.001);
    metrics.record_pool_acquire(0.001);
    metrics.record_feed_page_duration(0.001);
    metrics.record_reconciliation_duration(0.001);
    metrics.record_batch_rows(1.0);
    metrics.inc_dedup_absorbed();
    metrics.inc_dedup_stale();
    metrics.inc_stale_acceptance_rejection();
    metrics.inc_idempotency_conflict();
    metrics.inc_migration_failure();
    metrics.inc_tls_handshake_failure();
    metrics.inc_batch_retry();
    metrics.inc_backend_error(ErrorClass::Internal);
    metrics.inc_query_request(QueryKind::Raw);
    metrics.inc_invalidation();
    metrics.set_ready(true);
    metrics.record_retention_sweep(SweepOutcome::Completed, 0.001);
    metrics.inc_retention_chunk_dropped();
    metrics.inc_retention_chunk_kept_unresolved(KeepReason::Unavailable);
    metrics.inc_retention_drop_failure();
    metrics.set_chunks(1);
    metrics.record_aggregate_path(None);
    metrics.add_rollup_rows_deleted(3);
    metrics.set_rollup_refresh_status(&RefreshJobStatus {
        policy: RefreshPolicy::History,
        failing: false,
        secs_since_success: Some(1.0),
    });
    metrics.set_rollup_refresh_policies(2);
    metrics.inc_feed_cursor_refusal();
    metrics.set_feed_horizon_lag(1.5);

    provider.force_flush().unwrap();

    let mut declared = metrics.declared_instrument_names();
    declared.sort_unstable();
    assert_eq!(
        exported_names(&exporter),
        declared,
        "the exported inventory must be exactly what Metrics declares: an extra \
         name means an instrument nobody drives here, a missing one means an \
         instrument that is built but never reaches a reader",
    );

    let recorded = exporter.get_finished_metrics().unwrap();
    for resource_metrics in &recorded {
        for scope_metrics in resource_metrics.scope_metrics() {
            for metric in scope_metrics.metrics() {
                let name = metric.name();
                assert!(
                    name.starts_with("uc_timescaledb_"),
                    "{name} must sit in the plugin's own sub-namespace",
                );
                assert!(
                    name.bytes()
                        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_'),
                    "{name} must be a full literal Prometheus name: snake_case, no dots",
                );
                assert!(!name.ends_with('_'), "{name} must not end in a separator");

                // The suffix rules, off the exported kind. `_total` on every
                // Sum and `_seconds` on every duration histogram is what makes
                // the rendered series name identical whether the downstream
                // collector runs with `add_metric_suffixes` on or off.
                match metric.data() {
                    AggregatedMetrics::U64(MetricData::Sum(_)) => assert!(
                        name.ends_with("_total"),
                        "{name} is exported as a counter and must end in _total",
                    ),
                    AggregatedMetrics::F64(MetricData::Histogram(h)) => {
                        // Without a data point there are no bounds to read and
                        // the check below is vacuously true. Belt and braces
                        // with the equality assertion above, stated where the
                        // bounds are actually read.
                        assert!(
                            h.data_points().next().is_some(),
                            "{name} must be recorded on above, or its bucket layout \
                             cannot be read and the _seconds rule passes vacuously",
                        );
                        // Which histograms are durations is decided by the
                        // bucket layout they were built with, not by whether
                        // their name says "duration": keying on the name only
                        // inspects names that already announce themselves.
                        // `batch_rows` is the f64 histogram correctly not in
                        // seconds, and BATCH_ROW_BOUNDARIES says so.
                        // OPEN_LAYOUT_HISTOGRAMS are duration histograms that
                        // correctly end in `_seconds` but carry no explicit
                        // bucket layout, their bounds being the SDK default.
                        // Excepting them by name, rather than loosening the
                        // bounds-based check for every histogram, keeps the
                        // check below pinned to an explicit layout for every
                        // other duration series; the open-layout property is
                        // `the_two_open_layout_histograms_carry_no_explicit_bucket_layout`'s
                        // job.
                        if OPEN_LAYOUT_HISTOGRAMS.contains(&name) {
                            assert!(
                                name.ends_with("_seconds"),
                                "{name} is one of the open-layout duration histograms \
                                 and must still end in _seconds",
                            );
                        } else {
                            let is_duration = h
                                .data_points()
                                .any(|dp| dp.bounds().eq(DURATION_BOUNDARIES_SECS.iter().copied()));
                            assert_eq!(
                                is_duration,
                                name.ends_with("_seconds"),
                                "{name}: the duration bucket layout and the _seconds \
                                 suffix must agree in both directions -- a duration \
                                 histogram that loses the suffix and a non-duration one \
                                 that gains it are both wrong",
                            );
                        }
                    }
                    _ => {}
                }
            }
        }
    }
}

#[tokio::test]
async fn the_aggregate_path_counter_splits_by_path_and_reason() {
    let (provider, exporter) = local_provider();
    let metrics = Metrics::with_meter(&provider.meter("uc.timescaledb"), lazy_pool());

    metrics.record_aggregate_path(None);
    metrics.record_aggregate_path(None);
    metrics.record_aggregate_path(Some(FallbackReason::FilterField));
    provider.force_flush().unwrap();

    let name = "uc_timescaledb_aggregate_path_total";
    assert_eq!(
        counter_sum_with_label(
            &exporter,
            name,
            label::AGGREGATE_PATH,
            label::AGGREGATE_PATH_ROLLUP
        ),
        2
    );
    assert_eq!(
        counter_sum_with_label(
            &exporter,
            name,
            label::AGGREGATE_PATH,
            label::AGGREGATE_PATH_SCAN
        ),
        1
    );
    assert_eq!(
        counter_sum_with_label(&exporter, name, label::FALLBACK_REASON, "filter_field"),
        1
    );
    assert_eq!(
        counter_sum_with_label(
            &exporter,
            name,
            label::FALLBACK_REASON,
            label::FALLBACK_REASON_NONE
        ),
        2
    );
}

/// The refresh-policies gauge holds the last value it was set to, including
/// zero: a sample that finds no policies must overwrite a stale non-empty
/// reading. Two independent providers, one per value, because
/// `get_finished_metrics` accumulates across every flush of one exporter and
/// `gauge_last_u64` reads the first batch it finds.
#[tokio::test]
async fn the_refresh_policies_gauge_reports_its_last_value_including_zero() {
    let (provider, exporter) = local_provider();
    let metrics = Metrics::with_meter(&provider.meter("uc.timescaledb"), lazy_pool());
    metrics.set_rollup_refresh_policies(2);
    provider.force_flush().unwrap();
    assert_eq!(
        gauge_last_u64(&exporter, "uc_timescaledb_rollup_refresh_policies"),
        Some(2)
    );

    let (provider, exporter) = local_provider();
    let metrics = Metrics::with_meter(&provider.meter("uc.timescaledb"), lazy_pool());
    metrics.set_rollup_refresh_policies(5);
    metrics.set_rollup_refresh_policies(0);
    provider.force_flush().unwrap();
    assert_eq!(
        gauge_last_u64(&exporter, "uc_timescaledb_rollup_refresh_policies"),
        Some(0),
        "the most recent set_rollup_refresh_policies call wins, including zero"
    );
}

#[tokio::test]
async fn refresh_status_sets_both_gauges_under_the_policy_label() {
    let (provider, exporter) = local_provider();
    let metrics = Metrics::with_meter(&provider.meter("uc.timescaledb"), lazy_pool());
    metrics.set_rollup_refresh_status(&RefreshJobStatus {
        policy: RefreshPolicy::Live,
        failing: true,
        secs_since_success: Some(90.0),
    });
    provider.force_flush().unwrap();
    assert_eq!(
        gauge_last_u64(&exporter, "uc_timescaledb_rollup_refresh_job_failing"),
        Some(1)
    );
    assert!(
        exported_names(&exporter).contains(&"uc_timescaledb_rollup_refresh_age_seconds".to_owned())
    );
}

/// The late-convergence counter is exported at zero from construction, so the
/// series the deployment guide names exists before anything could increment it.
#[tokio::test]
async fn the_late_convergence_counter_is_published_at_zero() {
    let (provider, exporter) = local_provider();
    let _metrics = Metrics::with_meter(&provider.meter("uc.timescaledb"), lazy_pool());
    provider.force_flush().unwrap();

    assert!(
        exported_names(&exporter)
            .contains(&"uc_timescaledb_dedup_late_convergence_total".to_owned()),
        "the counter must be exported without a recording helper being called"
    );
    assert_eq!(
        counter_sum(&exporter, "uc_timescaledb_dedup_late_convergence_total"),
        0
    );
}

#[tokio::test]
async fn the_horizon_lag_gauge_is_absent_until_something_sets_it() {
    // §4.3 makes this gauge best-effort: "left unset when the plugin role
    // cannot see other roles' sessions", and it misses a prepared transaction,
    // which has no backend. **Unset means absent, not zero** — a zero would read
    // as a healthy instance with no long transaction, the very condition the
    // gauge exists to distinguish from "cannot tell".
    let (provider, exporter) = local_provider();
    let metrics = Metrics::with_meter(&provider.meter("uc.timescaledb"), lazy_pool());
    metrics.inc_feed_cursor_refusal();
    provider.force_flush().expect("flush");
    assert!(
        !exported_names(&exporter).contains(&"uc_timescaledb_feed_horizon_lag_seconds".to_owned()),
        "an instrument the plugin has built but never recorded on is not \
         exported at all, which is what 'left unset' has to mean for a reader"
    );
}

// ── Entry 39: instrument-name test binding ──────────────────────────────────

/// Every instrument-name literal this file's own assertions hand-copy, named
/// once here rather than trusted at each occurrence (`counter_sum`,
/// `counter_sum_with_label`, `histogram_count`, `histogram_bounds`,
/// `gauge_last_u64`, and `exported_names(&exporter).contains(...)`).
///
/// `cpt-cf-uc-plugin-dod-declared-name-test-binding` and
/// `cpt-cf-uc-plugin-algo-instrument-name-assertion` step 1
/// (`inst-name-read-declaration`) prohibit a hand-copied instrument name in a
/// test, with no carve-out for the declaration module's own test file. This
/// file's call sites still spell each name out directly — every helper above
/// takes a bare `name: &str`, and `Metrics` offers no per-field name constant to
/// call instead — so this binds the *occurrences* rather than rewriting each
/// one: every literal is named here and checked against the declaration by one
/// test below, the mechanism `tests/records_ingest_integration_pg.rs`'s
/// `the_asserted_counter_names_are_all_declared` and `tests/acceptance_slack_pg.rs`
/// already use.
///
/// **Excludes `"oracle_default_layout_histogram"`**
/// (`the_two_open_layout_histograms_carry_no_explicit_bucket_layout`): a
/// test-local calibration histogram built directly against the meter, with no
/// row in `declared_instrument_names()` to bind to, so including it here would
/// fail `declared.contains(..)` by construction rather than by a rename.
///
/// **This list is itself checked, not only trusted**, by
/// [`every_uc_timescaledb_literal_outside_the_name_lists_is_asserted`] below:
/// every double-quoted, namespace-prefixed string literal anywhere else in this
/// file must be a member of it. An omitted literal is caught here rather than by
/// a reviewer.
const ASSERTED_INSTRUMENT_NAMES: &[&str] = &[
    "uc_timescaledb_dedup_stale_total",
    "uc_timescaledb_stale_acceptance_rejections_total",
    "uc_timescaledb_batch_retries_total",
    "uc_timescaledb_insert_duration_seconds",
    "uc_timescaledb_query_duration_seconds",
    "uc_timescaledb_dedup_absorbed_total",
    "uc_timescaledb_backend_errors_total",
    "uc_timescaledb_ready",
    "uc_timescaledb_aggregate_path_total",
    "uc_timescaledb_rollup_refresh_policies",
    "uc_timescaledb_rollup_refresh_job_failing",
    "uc_timescaledb_rollup_refresh_age_seconds",
    "uc_timescaledb_dedup_late_convergence_total",
    "uc_timescaledb_feed_horizon_lag_seconds",
    "uc_timescaledb_feed_page_duration_seconds",
    "uc_timescaledb_reconciliation_duration_seconds",
];

/// Every double-quoted, namespace-prefixed string literal in this file, outside
/// [`ASSERTED_INSTRUMENT_NAMES`]'s own definition and
/// [`DESIGN_4_3_INSTRUMENTS`]'s, must be a member of
/// [`ASSERTED_INSTRUMENT_NAMES`].
///
/// Cross-checking every literal actually used against the list closes two holes:
/// a literal omitted from the list fails here directly, and a rename that
/// updates production plus both hand-typed name lists -- leaving each call
/// site's own literal untouched -- strands that literal outside the renamed
/// list, which this test catches.
///
/// Scans this file's own source, read back from disk at
/// `env!("CARGO_MANIFEST_DIR")` by the companion test below, not the compiled
/// `&'static str` constants: the question is what literal *text* this file
/// spells out, not what a `const` evaluates to.
///
/// Two exclusions, each checked against the file as it stands:
///
/// 1. **The two name-list definitions**: their own literals are the source of
///    truth, not something checked against itself.
/// 2. **A bare namespace-prefix literal with nothing after it**
///    (`every_exported_instrument_obeys_the_naming_convention`'s `starts_with`
///    check): a namespace check rather than a reference to one instrument, so it
///    is excluded by length rather than by span.
///
/// **This function's own source contains no contiguous literal that its own scan
/// would re-find**, which earlier versions repeatedly did -- each matching its
/// own mention before the real target further down the file. The fix that holds:
/// never write the searched-for text as one contiguous literal here at all. The
/// namespace root (no trailing underscore, matching nothing either search looks
/// for) and the double quote are joined at runtime via
/// [`find_const_array_decl`]'s `name` parameter and via `format!`, so the bytes
/// that would match are never adjacent in this function's own source.
fn find_const_array_decl(source: &str, name: &str) -> usize {
    let suffix = ": &[&str] = &[";
    let mut search_from = 0usize;
    loop {
        let rel = source[search_from..]
            .find(name)
            .unwrap_or_else(|| panic!("no `const {name}{suffix}` found in this file"));
        let name_at = search_from + rel;
        let const_prefix = ["const", " "].concat();
        if source[..name_at].ends_with(&const_prefix)
            && source[name_at + name.len()..].starts_with(suffix)
        {
            return name_at - const_prefix.len();
        }
        search_from = name_at + name.len();
    }
}

fn uc_timescaledb_literals_outside_name_lists(source: &str) -> Vec<String> {
    let asserted_at = find_const_array_decl(source, "ASSERTED_INSTRUMENT_NAMES");
    let asserted_end = asserted_at
        + source[asserted_at..]
            .find("];")
            .expect("ASSERTED_INSTRUMENT_NAMES's array literal closes with `];`")
        + 2;
    let design_at = find_const_array_decl(source, "DESIGN_4_3_INSTRUMENTS");
    let design_end = design_at
        + source[design_at..]
            .find("];")
            .expect("DESIGN_4_3_INSTRUMENTS's array literal closes with `];`")
        + 2;

    let bare_root = "uc_timescaledb";
    let bare_prefix = format!("{bare_root}_");
    let needle = format!("\"{bare_prefix}");
    let mut found = Vec::new();
    let mut search_from = 0usize;
    while let Some(rel) = source[search_from..].find(needle.as_str()) {
        let lit_start = search_from + rel;
        if (asserted_at..asserted_end).contains(&lit_start)
            || (design_at..design_end).contains(&lit_start)
        {
            search_from = lit_start + needle.len();
            continue;
        }
        let name_start = lit_start + 1; // past the opening `"`
        let close = source[name_start..].find('"').map_or_else(
            || panic!("unterminated string literal at byte {lit_start}"),
            |rel_close| name_start + rel_close,
        );
        let name = &source[name_start..close];
        assert!(
            name.bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_'),
            "{name:?} at byte {lit_start} is not a plain snake_case instrument name; widen or \
             fix this scan"
        );
        if name != bare_prefix {
            found.push(name.to_owned());
        }
        search_from = close + 1;
    }
    found
}

#[test]
fn every_uc_timescaledb_literal_outside_the_name_lists_is_asserted() {
    let path =
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/infra/metrics_tests.rs");
    let source = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()));
    let found = uc_timescaledb_literals_outside_name_lists(&source);
    assert!(
        !found.is_empty(),
        "this scan found no uc_timescaledb_* literal outside the two name lists, which is not \
         true of this file today -- the scan itself is broken, not the file"
    );
    let unasserted: Vec<&String> = found
        .iter()
        .filter(|name| !ASSERTED_INSTRUMENT_NAMES.contains(&name.as_str()))
        .collect();
    assert!(
        unasserted.is_empty(),
        "these uc_timescaledb_* literals are hand-copied somewhere in this file but are not in \
         ASSERTED_INSTRUMENT_NAMES, so every_hand_copied_instrument_name_is_declared never \
         checks them: {unasserted:?}"
    );
}

/// Every name in [`ASSERTED_INSTRUMENT_NAMES`] must be one
/// `Metrics::declared_instrument_names()` actually declares -- the binding
/// `cpt-cf-uc-plugin-dod-declared-name-test-binding` names, applied to this
/// file rather than only to the two external integration suites.
///
/// A production rename that updates a construction call in
/// `Metrics::with_meter` together with `declared_instrument_names()`'s own vec
/// entry drops the old literal out of `declared`, and this test fails naming
/// exactly that literal — rather than relying on whichever of this file's other
/// hand-copying assertions happens to also go red. Most do, by asserting a
/// non-zero value against a renamed-away series reading back zero, but not all:
/// the horizon-lag gauge's absence check was caught only by
/// `every_exported_instrument_obeys_the_naming_convention`.
#[tokio::test]
async fn every_hand_copied_instrument_name_is_declared() {
    let declared = Metrics::new(lazy_pool()).declared_instrument_names();
    let undeclared: Vec<&&str> = ASSERTED_INSTRUMENT_NAMES
        .iter()
        .filter(|name| !declared.contains(*name))
        .collect();
    assert!(
        undeclared.is_empty(),
        "these names are hand-copied into this file's assertions but are not among \
         Metrics::declared_instrument_names(), so a rename has moved silently out from \
         under them: {undeclared:?}",
    );
}

// ── §4.3 Metric Inventory conformance pin ───────────────────────────────────

/// Plugin DESIGN §4.3's instrument inventory, transcribed by hand.
///
/// Transcribed by hand from `docs/DESIGN.md` §4.3, never derived from
/// [`Metrics`]' own fields — the point is to disagree with the declarations when
/// they are incomplete. The row count is asserted below.
const DESIGN_4_3_INSTRUMENTS: &[&str] = &[
    // ── Performance (5) ──
    "uc_timescaledb_insert_duration_seconds",
    "uc_timescaledb_query_duration_seconds",
    "uc_timescaledb_pool_acquire_duration_seconds",
    "uc_timescaledb_feed_page_duration_seconds",
    "uc_timescaledb_reconciliation_duration_seconds",
    // ── Efficiency (6) ──
    "uc_timescaledb_pool_connections_active",
    "uc_timescaledb_pool_connections_idle",
    "uc_timescaledb_dedup_absorbed_total",
    "uc_timescaledb_dedup_stale_total",
    "uc_timescaledb_batch_rows",
    "uc_timescaledb_query_requests_total",
    // ── Reliability (10) ──
    "uc_timescaledb_backend_errors_total",
    "uc_timescaledb_batch_retries_total",
    "uc_timescaledb_idempotency_conflicts_total",
    "uc_timescaledb_invalidations_total",
    "uc_timescaledb_dedup_late_convergence_total",
    "uc_timescaledb_migration_failures_total",
    "uc_timescaledb_ready",
    "uc_timescaledb_feed_horizon_lag_seconds",
    "uc_timescaledb_feed_cursor_refusals_total",
    "uc_timescaledb_stale_acceptance_rejections_total",
    // ── Security (1) ──
    "uc_timescaledb_tls_handshake_failures_total",
    // ── Retention & Rollup (11) ──
    "uc_timescaledb_retention_sweeps_total",
    "uc_timescaledb_retention_sweep_duration_seconds",
    "uc_timescaledb_retention_chunks_dropped_total",
    "uc_timescaledb_retention_chunks_kept_unresolved_total",
    "uc_timescaledb_retention_drop_failures_total",
    "uc_timescaledb_chunks",
    "uc_timescaledb_aggregate_path_total",
    "uc_timescaledb_rollup_rows_deleted_total",
    "uc_timescaledb_rollup_refresh_age_seconds",
    "uc_timescaledb_rollup_refresh_job_failing",
    "uc_timescaledb_rollup_refresh_policies",
];

/// Asserts the partition between `Metrics::declared_instrument_names` and
/// [`DESIGN_4_3_INSTRUMENTS`] holds.
#[tokio::test]
async fn declared_names_are_exactly_design_4_3() {
    let (provider, _exporter) = local_provider();
    let metrics = Metrics::with_meter(&provider.meter("test"), lazy_pool());
    let declared: std::collections::BTreeSet<&str> =
        metrics.declared_instrument_names().into_iter().collect();

    let missing: Vec<&&str> = DESIGN_4_3_INSTRUMENTS
        .iter()
        .filter(|n| !declared.contains(**n))
        .collect();
    assert!(
        missing.is_empty(),
        "plugin DESIGN §4.3 declares these and `Metrics` does not: {missing:?}",
    );

    let undocumented: Vec<&&str> = declared
        .iter()
        .filter(|n| !DESIGN_4_3_INSTRUMENTS.contains(*n))
        .collect();
    assert!(
        undocumented.is_empty(),
        "`Metrics` declares these and plugin DESIGN §4.3 does not: {undocumented:?}",
    );
}

#[test]
fn the_plugin_transcription_carries_thirty_three_rows() {
    assert_eq!(
        DESIGN_4_3_INSTRUMENTS.len(),
        33,
        "plugin DESIGN \u{a7}4.3 declares 33 instruments across five driver groups",
    );
}

/// `inst-decl-open-buckets`: names and labels fixed, layout left to the design.
/// This is the clause a later edit would break by "finishing" the declaration
/// with a sibling's boundaries — `with_boundaries(DURATION_BOUNDARIES_SECS.to_vec())`
/// on either histogram reds here.
///
/// The oracle is a histogram built with no explicit boundaries, recorded against
/// the same provider in this test rather than a hardcoded boundary list, so this
/// asserts "no layout declared in this crate" rather than today's SDK default
/// and survives an `OTel` upgrade that changes it.
#[tokio::test]
async fn the_two_open_layout_histograms_carry_no_explicit_bucket_layout() {
    let (provider, exporter) = local_provider();
    let metrics = Metrics::with_meter(&provider.meter("uc.timescaledb"), lazy_pool());

    // The oracle: a default-configured histogram, built against the same
    // meter and read back through the same exporter, with no
    // `with_boundaries` call of its own.
    let oracle = provider
        .meter("uc.timescaledb")
        .f64_histogram("oracle_default_layout_histogram")
        .build();

    metrics.record_feed_page_duration(0.01);
    metrics.record_reconciliation_duration(0.02);
    oracle.record(0.01, &[]);

    provider.force_flush().unwrap();

    let default_bounds = histogram_bounds(&exporter, "oracle_default_layout_histogram");
    assert!(
        !default_bounds.is_empty(),
        "the oracle histogram itself must have recorded bounds, or this test \
         asserts nothing",
    );

    for name in [
        "uc_timescaledb_feed_page_duration_seconds",
        "uc_timescaledb_reconciliation_duration_seconds",
    ] {
        assert_eq!(
            histogram_bounds(&exporter, name),
            default_bounds,
            "{name} must carry the SDK's own default bucket layout -- ruling \
             I3 fixes this instrument's name and labels only and leaves the \
             layout open with the design (DESIGN.md \u{a7}4.5)",
        );
    }
}
