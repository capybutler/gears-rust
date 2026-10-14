//! Unit tests for the SPI methods' own obligations — the ones that are the
//! method's rather than any builder's.
//!
//! Almost every test here is the same discriminator. The store is built over a
//! **lazy pool at a dead DSN**, so reaching the pool answers `Transient` (a
//! pool timeout) and stopping before it answers `Internal`. That is what pins
//! this module's stated build-before-acquire order: a query that cannot be
//! rendered must never reach the pool, and one that can must. Either half
//! alone would leave the ordering unpinned, which is why both are written for
//! the paths that have them.
//!
//! The statements these methods send have their own, DB-free, oracles under
//! `super::super::query`; nothing here re-asserts a SQL string.

use std::sync::Arc;
use std::time::Duration;

use sqlx::postgres::PgPoolOptions;
use tokio_util::sync::CancellationToken;
use usage_collector_sdk::v2::{AggregationFold, USAGE_RECORD_BASE_TYPE, UsageCollectorPluginError};

use super::PgRecordStore;
use crate::domain::ports::RecordStore;
use crate::infra::metrics::Metrics;
use crate::infra::storage::test_support::{
    RECONCILIATION_TENANT, SCOPE_TENANT_A, fold_query, lazy_store, list_meter, list_query,
    list_range, parse_scope, unit_meter,
};

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
    let store = PgRecordStore::new(pool, metrics, CancellationToken::new(), 120);

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

#[tokio::test]
async fn a_scope_that_fails_to_translate_never_reaches_the_pool() {
    // The half the pure test above cannot see: that `get` propagates the
    // refusal instead of reading without it. The store's pool is lazy and
    // points at nothing, so any path that got as far as acquiring a connection
    // answers `Transient` (a pool timeout). An `Internal` naming the scope is
    // therefore proof the lookup stopped before it read anything.
    let store = lazy_store();
    let scope = parse_scope(&format!("gts_type_uuid eq '{USAGE_RECORD_BASE_TYPE}'"));

    let Err(err) = store.get(uuid::Uuid::from_u128(1), &scope).await else {
        unreachable!("an untranslatable scope must not yield a row");
    };

    match err {
        UsageCollectorPluginError::Internal(message) => assert!(
            message.contains("gts_type_uuid"),
            "the refusal reaches the caller as-is. got: {message}"
        ),
        other => unreachable!(
            "an untranslatable scope must stop the lookup before it acquires a \
             connection; reaching the pool would answer Transient. got: {other:?}"
        ),
    }
}

#[tokio::test]
async fn a_page_that_cannot_be_built_never_reaches_the_pool() {
    // The half the pure tests above cannot see: that `list` propagates the
    // refusal instead of reading without it. The store's pool is lazy and
    // points at nothing, so any path that got as far as acquiring a connection
    // answers `Transient` (a pool timeout). An `Internal` naming the field is
    // therefore proof the read stopped before it touched anything.
    let store = lazy_store();
    let query = list_query().with_filter(parse_scope(&format!(
        "gts_type_uuid eq '{USAGE_RECORD_BASE_TYPE}'"
    )));

    let Err(err) = store
        .list(list_meter(), list_range(), &query, &[], None)
        .await
    else {
        unreachable!("an untranslatable filter must not yield a page");
    };

    match err {
        UsageCollectorPluginError::Internal(message) => assert!(
            message.contains("gts_type_uuid"),
            "the refusal reaches the caller as-is. got: {message}"
        ),
        other => unreachable!(
            "an untranslatable filter must stop the read before it acquires a \
             connection; reaching the pool would answer Transient. got: {other:?}"
        ),
    }
}

#[tokio::test]
async fn a_fold_that_cannot_be_built_never_reaches_the_pool() {
    // `aggregate` claims the statement is built before a connection is acquired,
    // which `the_ungrouped_fold_still_reaches_the_pool` cannot pin: `Transient` is
    // what a pool timeout answers under *either* order. This is the half that pins
    // the order — the fold analogue of
    // `a_page_that_cannot_be_built_never_reaches_the_pool`.
    //
    // `gts_type_uuid` is a real column but not a filterable field, so
    // `record_column` refuses it and `build_aggregate_sql` errs. Against a lazy
    // pool at a dead DSN, an `Internal` naming the field is proof the fold stopped
    // before it touched anything.
    let store = lazy_store();
    let query = fold_query().with_filter(parse_scope(&format!(
        "gts_type_uuid eq '{USAGE_RECORD_BASE_TYPE}'"
    )));

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
        unreachable!("an untranslatable filter must not yield an aggregate");
    };

    match err {
        UsageCollectorPluginError::Internal(message) => assert!(
            message.contains("gts_type_uuid"),
            "the refusal reaches the caller as-is. got: {message}"
        ),
        other => unreachable!(
            "an untranslatable filter must stop the fold before it acquires a \
             connection; reaching the pool would answer Transient. got: {other:?}"
        ),
    }
}

#[tokio::test]
async fn the_ungrouped_fold_still_reaches_the_pool() {
    // The half every pure test above is blind to: that the single empty-keyed
    // bucket is `PostgreSQL`'s answer to a bare aggregate rather than something
    // this method shortcuts to. The mutation is a `return` that skips the query:
    //
    //     if group_by.is_empty() {
    //         return Ok(AggregationResult { buckets: Vec::new() });
    //     }
    //
    // The store's pool is lazy and points at nothing, so a path that acquired a
    // connection answers `Transient` and one that short-circuited answers `Ok`. It
    // also pins `aggregate`'s stated build-before-acquire order for the ungrouped
    // case.
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
        unreachable!(
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

#[tokio::test]
async fn a_store_serves_from_the_rollup_unless_switched_off() {
    assert!(
        lazy_store().rollup_enabled,
        "the rollup path is on by default"
    );
    assert!(!lazy_store().without_rollup().rollup_enabled);
}

#[tokio::test]
async fn a_reconciliation_read_that_cannot_be_built_never_reaches_the_pool() {
    // The method-level analogue of `a_fold_that_cannot_be_built_never_reaches_the_pool`:
    // both statements are built before a connection is acquired, so an
    // untranslatable scope must answer `Internal` against a lazy pool at a
    // dead DSN rather than `Transient`.
    let store = lazy_store();

    let Err(err) = store
        .reconciliation(
            RECONCILIATION_TENANT,
            &unit_meter(),
            list_range(),
            AggregationFold::Sum,
            &parse_scope(&format!("gts_type_uuid eq '{USAGE_RECORD_BASE_TYPE}'")),
        )
        .await
    else {
        unreachable!("an untranslatable scope must not yield reconciliation metadata");
    };

    match err {
        UsageCollectorPluginError::Internal(message) => assert!(
            message.contains("gts_type_uuid"),
            "the refusal reaches the caller as-is. got: {message}"
        ),
        other => unreachable!(
            "an untranslatable scope must stop the read before it acquires a \
             connection; reaching the pool would answer Transient. got: {other:?}"
        ),
    }
}

#[tokio::test]
async fn an_ordinary_reconciliation_read_still_reaches_the_pool() {
    // The other half of the discriminator: a well-formed scope must reach the
    // pool, so a lazy pool at a dead DSN answers `Transient` (a pool
    // timeout) rather than a short-circuited `Ok`.
    let store = lazy_store();

    let Err(err) = store
        .reconciliation(
            RECONCILIATION_TENANT,
            &unit_meter(),
            list_range(),
            AggregationFold::Sum,
            &parse_scope(&format!("tenant_id eq {SCOPE_TENANT_A}")),
        )
        .await
    else {
        unreachable!("a lazy pool at a dead DSN cannot answer Ok");
    };

    assert!(
        matches!(err, UsageCollectorPluginError::Transient { .. }),
        "a well-formed reconciliation read must reach the pool; anything \
         else means it answered without asking. got: {err:?}"
    );
}
