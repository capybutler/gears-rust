use super::{
    DURATION_BOUNDARIES_SECS, ErrorClass, InsertMode, Metrics, QueryKind, SweepOutcome, label,
};
use crate::domain::retention::KeepReason;

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

/// The description the meter recorded for the instrument named `name`, or
/// `None` if no instrument of that name was exported.
///
/// `Option` rather than a defaulted `String` so a renamed or unexported
/// instrument fails as "not found" instead of masquerading as one with an empty
/// description, which every `contains` assertion below would then report as a
/// missing phrase.
fn description_of(exporter: &InMemoryMetricExporter, name: &str) -> Option<String> {
    let metrics = exporter.get_finished_metrics().unwrap();
    for resource_metrics in &metrics {
        for scope_metrics in resource_metrics.scope_metrics() {
            for metric in scope_metrics.metrics() {
                if metric.name() == name {
                    return Some(metric.description().to_owned());
                }
            }
        }
    }
    None
}

/// The attribute count of every data point of the `u64` counter named `name`.
///
/// An empty vector means the instrument exported no counter data points, which
/// is a different failure from "every point is unlabelled" and must not read as
/// a pass.
fn counter_attribute_counts(exporter: &InMemoryMetricExporter, name: &str) -> Vec<usize> {
    let metrics = exporter.get_finished_metrics().unwrap();
    for resource_metrics in &metrics {
        for scope_metrics in resource_metrics.scope_metrics() {
            for metric in scope_metrics.metrics() {
                if metric.name() == name
                    && let AggregatedMetrics::U64(MetricData::Sum(sum)) = metric.data()
                {
                    return sum
                        .data_points()
                        .map(|dp| dp.attributes().count())
                        .collect();
                }
            }
        }
    }
    Vec::new()
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

/// The recording helpers not exercised by
/// [`recording_helpers_emit_expected_series`] — the stale-dedup counter, a
/// single-row insert, and an aggregated query — emit their expected series
/// through the [`Metrics::with_meter`] seam.
///
/// Also smoke-checks that [`Metrics::new`] builds the full instrument inventory
/// against the process-global provider without panicking. Recordings on that
/// handle are a safe no-op (no reader is installed in the test process), so they
/// cannot be asserted directly — only the local `with_meter` provider is read
/// back. Together with the sibling test, every recording helper now has a value
/// assertion, not just a "does not panic" check.
#[tokio::test]
async fn remaining_recording_helpers_emit_expected_series() {
    // Global-provider construction path (the production entry point): building
    // the full inventory and recording against it must not panic.
    let global = Metrics::new(lazy_pool());
    global.set_ready(true);
    global.inc_dedup_absorbed();
    global.inc_dedup_stale();
    global.record_insert(InsertMode::Single, 0.001);
    global.record_query(QueryKind::Aggregated, 0.002);
    global.inc_backend_error(ErrorClass::Transient);

    // Value assertions via a local in-memory reader.
    let (provider, exporter) = local_provider();
    let metrics = Metrics::with_meter(&provider.meter("uc.timescaledb"), lazy_pool());

    metrics.inc_dedup_stale();
    metrics.inc_dedup_stale();
    metrics.inc_batch_retry();
    metrics.inc_batch_retry();
    metrics.inc_batch_retry();
    metrics.record_insert(InsertMode::Single, 0.001);
    metrics.record_query(QueryKind::Aggregated, 0.002);

    provider.force_flush().unwrap();

    assert_eq!(
        counter_sum(&exporter, "uc_timescaledb_dedup_stale_total"),
        2
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

    // Histogram (labelled): two batch-insert observations.
    metrics.record_insert(InsertMode::Batch, 0.01);
    metrics.record_insert(InsertMode::Batch, 0.02);

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

/// The three at-most-one-invalidation counters emit under the names the
/// inventory declares, and the two rejection counters are **separate
/// instruments** rather than two values of one label.
///
/// That separation is the assertion worth having. A Prometheus label asserts
/// that its arms are one measurement partitioned — which is what makes
/// `sum by (...)` meaningful — and these two arms have different units: the
/// in-batch pre-reject counts refused rows, the index's cross-call refusal
/// counts refused statements. Under one instrument nothing but prose stands
/// between an operator and a meaningless sum. Under two, the unit is in the
/// series name and there is no shared series to sum across.
#[tokio::test]
async fn the_two_rejection_paths_are_separate_instruments_because_their_units_differ() {
    let (provider, exporter) = local_provider();
    let metrics = Metrics::with_meter(&provider.meter("uc.timescaledb"), lazy_pool());

    metrics.inc_invalidation();
    metrics.inc_invalidation();

    metrics.inc_invalidation_rejected_row();
    metrics.inc_invalidation_rejected_row();
    metrics.inc_invalidation_rejected_statement();

    provider.force_flush().unwrap();

    assert_eq!(
        counter_sum(&exporter, "uc_timescaledb_invalidations_total"),
        2,
        "an accepted invalidation entry increments the accepted counter",
    );
    assert_eq!(
        counter_sum(&exporter, "uc_timescaledb_invalidation_rejected_rows_total"),
        2,
        "the in-batch pre-reject counts refused rows",
    );
    assert_eq!(
        counter_sum(
            &exporter,
            "uc_timescaledb_invalidation_rejected_statements_total"
        ),
        1,
        "the index's cross-call refusal counts refused statements",
    );

    // The separation itself: two names, and **no dimension at all** on either.
    // Asserting zero attributes rather than the absence of two particular
    // `scope` values is what makes "the unit belongs in the name" complete — a
    // reintroduced label spelled any other way is caught too. Collapsing the
    // two helpers onto one instrument fails the counts above; reintroducing a
    // label of any spelling fails this.
    for name in [
        "uc_timescaledb_invalidation_rejected_rows_total",
        "uc_timescaledb_invalidation_rejected_statements_total",
    ] {
        assert!(
            exported_names(&exporter).contains(&name.to_owned()),
            "{name} must be its own series, not a label value of a shared one",
        );
        let attrs = counter_attribute_counts(&exporter, name);
        assert!(!attrs.is_empty(), "{name} must export a counter data point");
        assert!(
            attrs.iter().all(|n| *n == 0),
            "{name} must carry no dimension at all; the unit belongs in the \
             name, not in a label. Attribute counts per point: {attrs:?}",
        );
    }
}

/// Each rejection counter's **description** carries its own unit, and the
/// statement counter's carries the retry caveat.
///
/// A Prometheus series carries no rustdoc: the description is the only text
/// that travels with the metric, so the facts an operator needs to read the
/// rate correctly have to be in it. The phrases are asserted **contiguously**,
/// not as loose substrings — checking for "rows" and "statements" separately
/// stays green if the two units are swapped between the instruments, which is
/// the entire content of the warning.
#[tokio::test]
async fn each_rejection_counters_description_carries_its_own_unit() {
    let (provider, exporter) = local_provider();
    let metrics = Metrics::with_meter(&provider.meter("uc.timescaledb"), lazy_pool());
    metrics.inc_invalidation_rejected_row();
    metrics.inc_invalidation_rejected_statement();
    provider.force_flush().unwrap();

    let rows = description_of(&exporter, "uc_timescaledb_invalidation_rejected_rows_total")
        .expect("the in-batch rejection counter is exported");
    assert!(
        rows.contains("one per refused row"),
        "the row counter's unit must travel with the series, got: {rows:?}",
    );

    let statements = description_of(
        &exporter,
        "uc_timescaledb_invalidation_rejected_statements_total",
    )
    .expect("the cross-call rejection counter is exported");
    for phrase in [
        "one per refused statement however many withdrawals it carried",
        "a retried batch counts once per attempt",
        "uc_timescaledb_batch_retries_total",
    ] {
        assert!(
            statements.contains(phrase),
            "the statement counter's description must carry {phrase:?}, got: {statements:?}",
        );
    }
}

/// Every instrument the inventory exports obeys the naming convention the
/// module doc states — asserted off each instrument's **kind**, over the exact
/// exported set.
///
/// The doc names two rules §3.11.5 binds this crate by. This is the mechanism
/// for the naming one; the bounded-label rule has the closed `as_label` enums
/// as its own. Without this the convention is a paragraph, and a new instrument
/// added with a dotted name, a missing `_total` or a `_secs` suffix is caught
/// by nobody — which is how the seven phantom dotted citations this task
/// removed came to look plausible in the first place.
///
/// **Kind, not spelling — including which histograms are durations.** `_total`
/// is asserted of everything the SDK exports as a `Sum`, so all eleven counters
/// are covered rather than the five a hardcoded list of names happened to hold.
/// The `_seconds` suffix and the **`DURATION_BOUNDARIES_SECS` bucket layout**
/// must agree **in both directions**, the layout read back off the exported
/// bounds. Not "every histogram whose name contains duration", which would let
/// `uc_timescaledb_insert_latency_ms` through for the same reason a comment
/// lets a rename through: it only inspects names that already announce
/// themselves. And biconditional rather than one-way, so a non-duration
/// histogram *gaining* the suffix — `uc_timescaledb_batch_rows` renamed to
/// `…_batch_seconds` — reds too. `batch_rows` is the f64 histogram correctly
/// *not* in seconds, and `BATCH_ROW_BOUNDARIES` is what says so.
///
/// **The exported set must equal [`Metrics::declared_instrument_names`]**, not
/// merely reach some floor. A floor cannot notice an instrument disappearing,
/// and it hid an untested belief: that the two observable pool gauges are
/// collected by their callbacks on this path. Equality tests that belief
/// instead of assuming it — it holds, at 24.
///
/// Two mechanisms catch different halves of a new instrument, and neither is
/// quite a guarantee on its own: `declared_instrument_names`' destructure has
/// no `..`, so the compiler will not let anyone *reach* that list without being
/// shown the new field, and this equality stays red until the instrument is
/// both named there and driven in the block below. Adding `foo: _` to silence
/// `E0027`, omitting the string, and never driving it would still be green —
/// two omissions in one edit, while looking at the list.
#[tokio::test]
async fn every_exported_instrument_obeys_the_naming_convention() {
    let (provider, exporter) = local_provider();
    let metrics = Metrics::with_meter(&provider.meter("uc.timescaledb"), lazy_pool());

    // Every recording helper, and every one of these calls is load-bearing:
    // measured by deleting them one at a time, an instrument the SDK has built
    // but never recorded on is **not exported at all** -- counter, histogram
    // and gauge alike. So the equality assertion below reds until a new
    // instrument is driven here, which is what makes "covered the day it is
    // added" true rather than aspirational. The two observable pool gauges have
    // no helper; the same assertion is what establishes that their callbacks
    // fire on flush.
    metrics.record_insert(InsertMode::Single, 0.001);
    metrics.record_query(QueryKind::Raw, 0.001);
    metrics.record_pool_acquire(0.001);
    metrics.record_batch_rows(1.0);
    metrics.inc_dedup_absorbed();
    metrics.inc_dedup_stale();
    metrics.inc_idempotency_conflict();
    metrics.inc_migration_failure();
    metrics.inc_tls_handshake_failure();
    metrics.inc_batch_retry();
    metrics.inc_backend_error(ErrorClass::Internal);
    metrics.inc_query_request(QueryKind::Raw);
    metrics.inc_invalidation();
    metrics.inc_invalidation_rejected_row();
    metrics.inc_invalidation_rejected_statement();
    metrics.set_ready(true);
    metrics.record_retention_sweep(SweepOutcome::Completed, 0.001);
    metrics.inc_retention_chunk_dropped();
    metrics.inc_retention_chunk_kept_unresolved(KeepReason::Unavailable);
    metrics.inc_retention_drop_failure();
    metrics.set_chunks(1);

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
                        // with the equality assertion above -- that already
                        // reds on an undriven instrument, since an unrecorded
                        // one is not exported -- but this one states the local
                        // requirement where the bounds are actually read,
                        // rather than leaving it to hold at a distance.
                        assert!(
                            h.data_points().next().is_some(),
                            "{name} must be recorded on above, or its bucket layout \
                             cannot be read and the _seconds rule passes vacuously",
                        );
                        // Which histograms are durations is decided by the
                        // bucket layout they were built with, not by whether
                        // their name already says "duration". Keying on the
                        // name would let `uc_timescaledb_insert_latency_ms`
                        // through -- it announces nothing the guard looks for --
                        // which is the same shape of hole as trusting a comment.
                        // `uc_timescaledb_batch_rows` is the f64 histogram that
                        // is correctly not in seconds, and it is BATCH_ROW_
                        // BOUNDARIES that says so.
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
                    _ => {}
                }
            }
        }
    }
}
