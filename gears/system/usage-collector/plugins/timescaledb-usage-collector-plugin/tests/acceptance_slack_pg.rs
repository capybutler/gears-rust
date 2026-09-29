#![cfg(feature = "postgres")]
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]
//! The write path's acceptance-slack guard (`docs/DESIGN.md` §3.6
//! `cpt-cf-uc-plugin-seq-ingest-dedup`), at a narrow slack. Requires Docker.
//!
//! Every other pg suite runs at `common::HARNESS_ACCEPTANCE_SLACK_SECS`, under
//! which the guard admits everything. This suite is the only place its refusal
//! runs, and the only place
//! `uc_timescaledb_stale_acceptance_rejections_total` is driven against a live
//! ledger.

mod common;

use std::sync::Arc;

use opentelemetry::metrics::MeterProvider as _;
use opentelemetry_sdk::metrics::data::{AggregatedMetrics, MetricData};
use opentelemetry_sdk::metrics::{InMemoryMetricExporter, PeriodicReader, SdkMeterProvider};
use rust_decimal::Decimal;
use time::{Duration, OffsetDateTime};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use usage_collector_sdk::{UsageCollectorPluginError, UsageRecord};

use timescaledb_usage_collector_plugin::domain::ports::RecordStore;
use timescaledb_usage_collector_plugin::infra::metrics::Metrics;
use timescaledb_usage_collector_plugin::infra::storage::record_store::PgRecordStore;

/// Narrow enough that a fixture instant is far outside it, wide enough that a
/// clock-relative entry is comfortably inside on a loaded CI machine.
const NARROW_SLACK_SECS: u64 = 120;

/// This suite's own tenant. `common` exports no tenant ids; each suite declares
/// the ones it needs, as `rollup_aggregate_integration_pg` does.
const TENANT: Uuid = Uuid::from_u128(0x5_1AC);

/// The counter this suite asserts on, checked against the crate's own inventory
/// by [`the_asserted_counter_name_is_declared`].
///
/// Named in one place for the reason `records_ingest_integration_pg` gives for
/// its own list: [`counter_sum`] answers **0** for an instrument that was never
/// recorded and **0** for one that does not exist, so a rename would turn the
/// zero assertion below into a tautology and leave the non-zero one failing for
/// a reason that names nothing.
const STALE_ACCEPTANCE_COUNTER: &str = "uc_timescaledb_stale_acceptance_rejections_total";

/// The substring of the refusal a stale-acceptance `Transient` carries.
///
/// Transcribed by hand from `record_store.rs`'s `STALE_ACCEPTANCE_MESSAGE`,
/// which is private: a `Transient` alone does not discriminate this refusal
/// from the retention-race one the same function also produces, and an oracle
/// derived from the constant under test could not see it being reworded onto
/// the wrong arm.
const STALE_ACCEPTANCE_MESSAGE: &str = "acceptance instant outside the configured acceptance slack";

/// `common::bring_up_with` plus `common::record_store`, at a narrow slack.
///
/// The slack is a fourth parameter on `bring_up_with` rather than a second
/// builder, so one function still owns the harness config, and
/// `common::record_store` reads it back off the harness rather than being told
/// it a second time.
async fn setup() -> (common::TsHarness, PgRecordStore) {
    let h = common::bring_up_with(30, 2, 16, NARROW_SLACK_SECS)
        .await
        .expect("timescaledb container (Docker required)");
    let store = common::record_store(&h);
    (h, store)
}

/// [`setup`] with the store's metric inventory wired to a **local** in-memory
/// exporter.
///
/// Local rather than the process-global provider, for the reason
/// `records_ingest_integration_pg::setup_metered` gives: `Metrics::with_meter`
/// takes the meter explicitly, so a recording assertion never depends on global
/// state another test binary is also writing to.
async fn setup_metered() -> (
    common::TsHarness,
    PgRecordStore,
    SdkMeterProvider,
    InMemoryMetricExporter,
) {
    let h = common::bring_up_with(30, 2, 16, NARROW_SLACK_SECS)
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

/// Total of the `u64` counter data points named `name`.
///
/// **0 for an instrument that exists and was never recorded, and 0 for one that
/// does not exist.** That ambiguity is closed by
/// [`the_asserted_counter_name_is_declared`] rather than here, because an
/// OpenTelemetry counter with no recorded value need not be exported at all, so
/// "absent" is the normal reading of a legitimate zero.
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

/// An entry at `accepted_at`, on this suite's tenant and meter.
///
/// **No `common::rederive` call.** `accepted_at` is not one of the six identity
/// inputs `derive_usage_record_id` takes, so overriding it does not move the
/// `id` and re-deriving would be a no-op that reads as though it were needed.
fn entry_accepted_at(idem: &str, accepted_at: OffsetDateTime) -> UsageRecord {
    let m = common::meter(common::VCPU_METER);
    UsageRecord {
        accepted_at,
        ..common::entry(&m, TENANT, idem, Decimal::ONE)
    }
}

/// A displacement far outside [`NARROW_SLACK_SECS`], so a test's own scheduling
/// latency cannot move a fixture across the boundary in either direction.
fn seconds_outside_the_slack() -> Duration {
    Duration::seconds(i64::try_from(NARROW_SLACK_SECS).expect("fits i64") * 10)
}

/// Assert `err` is the guard's own refusal, not merely some `Transient`.
///
/// `create` answers `Transient` on the retention race too, from the same
/// function, so `matches!(err, Transient { .. })` alone cannot tell a backend
/// that refused the acceptance instant from one that lost a conflicting row to
/// retention. The message is what separates them.
fn assert_stale_acceptance(err: &UsageCollectorPluginError, context: &str) {
    match err {
        UsageCollectorPluginError::Transient { detail, .. } => assert!(
            detail.contains(STALE_ACCEPTANCE_MESSAGE),
            "{context}: the refusal must name the acceptance slack, not another \
             transient of the same shape: {detail}"
        ),
        other => panic!("{context}: the refusal is retryable, so the host can re-stamp: {other:?}"),
    }
}

/// The counter this suite names must be one the crate actually declares.
///
/// `counter_sum` reads a renamed instrument as a legitimate zero, which would
/// make the zero assertion below pass for the wrong reason. This is the single
/// place that fails on a rename, and it fails naming the instrument.
// `#[tokio::test]` and not `#[test]`: `connect_lazy` opens no connection but
// does spawn the pool's background maintenance task, which needs a runtime.
// No Docker, no container - the one test in this file that needs neither.
#[tokio::test]
async fn the_asserted_counter_name_is_declared() {
    let pool = sqlx::postgres::PgPoolOptions::new()
        .connect_lazy("postgres://user:pass@localhost/db")
        .expect("a syntactically valid DSN yields a lazy pool without connecting");
    let declared = Metrics::new(pool).declared_instrument_names();
    assert!(
        declared.contains(&STALE_ACCEPTANCE_COUNTER),
        "`{STALE_ACCEPTANCE_COUNTER}` is asserted on in this file but is not among the \
         crate's declared instruments, so every assertion naming it reads 0 whatever \
         the code does. Declared: {declared:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_entry_accepted_now_is_admitted_and_counts_no_rejection() {
    // The paired positive. An assertion that something is refused is vacuous
    // against a backend that refuses everything, and most of this file asserts
    // a refusal. The counter is read here too, because a backend that
    // incremented it on every write would satisfy every non-zero assertion
    // below.
    let (_h, store, provider, exporter) = setup_metered().await;
    store
        .create(entry_accepted_at("slack-fresh", OffsetDateTime::now_utc()))
        .await
        .expect("an entry accepted now is admitted");

    provider.force_flush().expect("flush metrics");
    assert_eq!(
        counter_sum(&exporter, STALE_ACCEPTANCE_COUNTER),
        0,
        "an admitted entry is not a rejection"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_entry_accepted_too_long_ago_is_refused_and_counted() {
    let (_h, store, provider, exporter) = setup_metered().await;
    let stale = OffsetDateTime::now_utc() - seconds_outside_the_slack();
    let err = store
        .create(entry_accepted_at("slack-old", stale))
        .await
        .expect_err("a stale acceptance must be refused");
    assert_stale_acceptance(&err, "a past-dated acceptance");

    // Definition-of-done item 3: the refusal is counted, by this instrument and
    // not by `uc_timescaledb_dedup_stale_total`, which is the retention race's.
    provider.force_flush().expect("flush metrics");
    assert_eq!(
        counter_sum(&exporter, STALE_ACCEPTANCE_COUNTER),
        1,
        "one refusal, counted once"
    );
    assert_eq!(
        counter_sum(&exporter, "uc_timescaledb_dedup_stale_total"),
        0,
        "and not charged to the retention race's counter, which answers the \
         same `Transient` from the same function"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_entry_accepted_too_far_ahead_is_refused_as_transient() {
    // `docs/DESIGN.md` §3.6: within the slack of the statement's own
    // `statement_timestamp()`, "in either direction". A one-sided predicate
    // passes every past-dated test and admits an entry from next year, which
    // would order ahead of everything real in acceptance terms.
    let (_h, store) = setup().await;
    let ahead = OffsetDateTime::now_utc() + seconds_outside_the_slack();
    let err = store
        .create(entry_accepted_at("slack-ahead", ahead))
        .await
        .expect_err("a future acceptance must be refused");
    assert_stale_acceptance(&err, "a future-dated acceptance");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stale_retry_of_a_stored_identity_is_refused_rather_than_absorbed() {
    // `docs/DESIGN.md` §3.6: "**The guard's verdict takes precedence**: a row
    // not admitted is `Transient` … even when its identity exists".
    //
    // This is the ordering rule between two independent outcomes, and it has
    // its own test because a backend that resolved the conflict first would
    // answer `IdempotencyConflict` or absorb, and would pass every other
    // assertion in this file. The retry carries the same six identity inputs as
    // the stored entry, since only `accepted_at` differs.
    let (_h, store) = setup().await;
    let fresh = entry_accepted_at("slack-precedence", OffsetDateTime::now_utc());
    store.create(fresh.clone()).await.expect("stored");

    let stale_retry = UsageRecord {
        accepted_at: OffsetDateTime::now_utc() - seconds_outside_the_slack(),
        ..fresh
    };
    let err = store
        .create(stale_retry)
        .await
        .expect_err("the guard decides before the identity does");
    assert_stale_acceptance(&err, "a stale retry of a stored identity");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_batch_refuses_a_stale_retry_of_a_stored_identity_too() {
    // The batch analogue of the test above. `DESIGN.md` §3.6
    // `cpt-cf-uc-plugin-seq-ingest-batch` states the precedence rule per row —
    // "a row not admitted is a stale-acceptance `Transient` even when its
    // identity exists" — and the batch decides it in a different place: the
    // per-row `Admission` is what keeps a refused identity out of the conflict
    // read-back, where the single path returns before the read. A batch that
    // read every not-won row back and resolved it would absorb this row, since
    // `accepted_at` takes no part in the caller-supplied comparison.
    let (_h, store) = setup().await;
    let fresh = entry_accepted_at("slack-batch-precedence", OffsetDateTime::now_utc());
    store.create(fresh.clone()).await.expect("stored");

    let stale_retry = UsageRecord {
        accepted_at: OffsetDateTime::now_utc() - seconds_outside_the_slack(),
        ..fresh
    };
    let out = store
        .create_batch(vec![stale_retry])
        .await
        .expect("the call itself succeeds; the rejection is per row");
    let err = out[0]
        .as_ref()
        .expect_err("the guard decides before the identity does, per row");
    assert_stale_acceptance(err, "a batched stale retry of a stored identity");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_batch_refuses_only_its_stale_rows() {
    // Per-row `admitted`, per §3.6 `cpt-cf-uc-plugin-seq-ingest-batch`: "a
    // conflict or rejection on one record never fails the others".
    let (_h, store) = setup().await;
    let fresh = entry_accepted_at("slack-batch-fresh", OffsetDateTime::now_utc());
    let stale = entry_accepted_at(
        "slack-batch-stale",
        OffsetDateTime::now_utc() - seconds_outside_the_slack(),
    );
    let out = store
        .create_batch(vec![fresh, stale])
        .await
        .expect("the call itself succeeds; the rejection is per row");
    assert!(out[0].is_ok(), "the fresh row is admitted: {:?}", out[0]);
    let err = out[1]
        .as_ref()
        .expect_err("the stale row alone is refused, in its input position");
    assert_stale_acceptance(err, "the stale row of a mixed batch");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_batch_counts_every_refused_row_not_every_refused_identity() {
    // Two input rows of one refused identity are two refusals and two
    // increments, exactly as two input rows of one absorbed identity are two
    // absorbs (`resolve_batch`). Counting per distinct identity instead would
    // under-report the rejection rate an operator alerts on, and every other
    // assertion in this file uses one row per identity, so none of them can see
    // it.
    let (_h, store, provider, exporter) = setup_metered().await;
    let stale = entry_accepted_at(
        "slack-batch-twice",
        OffsetDateTime::now_utc() - seconds_outside_the_slack(),
    );
    let out = store
        .create_batch(vec![stale.clone(), stale])
        .await
        .expect("the call itself succeeds; the rejections are per row");
    assert_eq!(out.len(), 2);
    for (i, r) in out.iter().enumerate() {
        assert_stale_acceptance(
            r.as_ref().expect_err("both rows are refused"),
            &format!("row {i} of a two-row batch of one refused identity"),
        );
    }

    provider.force_flush().expect("flush metrics");
    assert_eq!(
        counter_sum(&exporter, STALE_ACCEPTANCE_COUNTER),
        2,
        "two refused rows, two increments, though they are one identity"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_refused_row_is_never_written_to_the_ledger() {
    // The refusal is not merely reported: the statement's `WHERE admitted` is
    // what keeps the row out of the ledger, so nothing is written. Asserted
    // directly, because a path that inserted the row and then reported
    // `Transient` would pass every assertion above while leaving an entry the
    // feed would serve.
    let (h, store) = setup().await;
    let stale = entry_accepted_at(
        "slack-no-row",
        OffsetDateTime::now_utc() - seconds_outside_the_slack(),
    );
    let id = stale.id;
    let err = store
        .create(stale)
        .await
        .expect_err("a stale acceptance must be refused");
    assert_stale_acceptance(&err, "a refused entry");

    let stored: i64 = sqlx::query_scalar("SELECT count(*) FROM usage_records WHERE id = $1")
        .bind(id)
        .fetch_one(&h.pool)
        .await
        .expect("count the refused entry's rows");
    assert_eq!(stored, 0, "a refused entry is not written at all");
}
