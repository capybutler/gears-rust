//! Service-level operational-metrics emission tests.
//!
//! Each test wires a `Service` with a real `UcMetricsMeter` bound to an
//! in-memory exporter, drives one service method, `force_flush()`es, and
//! asserts the exported instrument series — proving the shared PDP wrapper in
//! `domain/authz.rs` and the plugin-SPI dispatch wrapper in `Service` emit per
//! DESIGN §3.11.5.
//!
//! The source-scanning tests below are the source-side half of the §3.11.5
//! cardinality pin (emission-side half: `infra/metrics_inventory_tests.rs`).
//! `no_label_key_constant_in_ports_metrics_declares_an_unbounded_identifier` is
//! the load-bearing one: the per-method token scan catches a forbidden
//! `key::TENANT_ID` only because the constant happens to be *named*
//! `TENANT_ID`. Both live here because this module owns `production_sources()`,
//! the file-walking oracle they need.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use bigdecimal::BigDecimal;
use toolkit_gts::gts_id;
use toolkit_odata::{ODataQuery, ast};
use tracing_subscriber::layer::SubscriberExt;
use usage_collector_sdk::{
    AggregationBucket, AggregationFold, AggregationResult, AlreadyInvalidatedArgs,
    CreateUsageRecord, EntryType, IdempotencyKey, MetadataKey, MeterRef, MeterTypeId,
    NotFoundReason, ReasonCode, RecordOrigin, RecordPage, ResourceRef, StoredUsageRecord,
    USAGE_RECORD_RESOURCE, UsageCollectorError, UsageRecord, ValidationReason,
};
use uuid::Uuid;

use authz_resolver_sdk::AuthZResolverApi;
use toolkit_security::pep_properties;

use opentelemetry_sdk::metrics::{InMemoryMetricExporter, SdkMeterProvider};
use toolkit_security::SecurityContext;

use super::{classify_feed_result, classify_query_result, classify_record_error};
use crate::config::IngestionQuotaConfig;
use crate::domain::Service;
use crate::domain::authz::usage_record;
use crate::domain::ports::metrics::{
    FeedErrorCategory, IngestRequestErrorCategory, IngestRequestOutcome, PluginOp,
    QueryErrorCategory, RecordErrorCategory, RecordOutcome, RequestOutcome, UsageCollectorMetrics,
};
use crate::domain::test_support::{
    ActionRecordingPermitResolver, CountingPermitResolver, CountingTenantPermitResolver,
    DenyAllResolver, HappyPathPlugin, RecordingReconciliationPlugin, ServiceFixture,
    UnreachableResolver, authenticated_ctx, counter_label_pairs, counter_points, counter_sum,
    counter_sum_with_label, enforcer_for, fake_declaration_source_with_fold,
    fake_declaration_source_with_metadata, gauge_label_pairs, gauge_last, histogram_count,
    histogram_count_with_label, histogram_sum, histogram_sum_with_label, hub_with_plugin,
    local_metrics, projected, qty, recent_window_end, recent_window_start,
    service_with_metrics_unready_plugin, test_time_range,
};
use crate::domain::type_resolver::{TypeResolver, TypeResolverConfig};
use usage_collector_sdk::{UsageCollectorPluginError, UsageCollectorPluginV1};

const SAMPLE_METER_TYPE_ID: &str =
    gts_id!("cf.core.uc.usage_record.v1~example.usage._.bytes_in.v1~");

fn sample_record() -> UsageRecord {
    UsageRecord {
        id: Uuid::from_u128(0x1234),
        gts_type_id: MeterTypeId::new(SAMPLE_METER_TYPE_ID).expect("valid gts_type_id"),
        tenant_id: Uuid::from_u128(2),
        resource_ref: ResourceRef::new("rsc-1", "compute.vm").expect("valid resource ref"),
        subject_ref: None,
        metadata: BTreeMap::new(),
        quantity: qty("1"),
        idempotency_key: IdempotencyKey::new("idem-1").expect("valid idempotency key"),
        accepted_at: recent_window_end(),
        origin: RecordOrigin::Live,
        invalidation: None,
        window_start: recent_window_start(),
        window_end: recent_window_end(),
    }
}

/// The identity-free create-surface twin of [`sample_record`]: its canonical
/// fields minus the server-owned `id`, for the `create_usage_records` entry
/// point which takes `CreateUsageRecord`.
fn sample_create_record() -> CreateUsageRecord {
    CreateUsageRecord {
        entry_type: EntryType::Record,
        gts_type_id: MeterTypeId::new(SAMPLE_METER_TYPE_ID).expect("valid gts_type_id"),
        tenant_id: Uuid::from_u128(2),
        resource_ref: ResourceRef::new("rsc-1", "compute.vm").expect("valid resource ref"),
        subject_ref: None,
        metadata: BTreeMap::new(),
        quantity: qty("1"),
        idempotency_key: Some(IdempotencyKey::new("idem-1").expect("valid idempotency key")),
        invalidation: None,
        window_start: recent_window_start(),
        window_end: recent_window_end(),
    }
}

/// A page of `n` sample usage records (for a raw-query result-rows assertion).
fn record_page(n: usize) -> RecordPage {
    RecordPage {
        items: (0..n)
            .map(|_| crate::domain::test_support::as_stored(sample_record()))
            .collect(),
        next: None,
    }
}

/// A PDP permit scoped to `sample_record()`'s tenant (`Uuid::from_u128(2)`),
/// which projects to a `tenant_id` `OData` filter — the shape a successful raw /
/// aggregated LIST needs (a tenant-less scope fails closed in projection).
fn tenant_scoped_permit() -> Arc<CountingPermitResolver> {
    CountingPermitResolver::new(
        pep_properties::OWNER_TENANT_ID,
        Uuid::from_u128(2).to_string(),
    )
}

/// A single-bucket aggregated result (the no-grouping case), for the
/// aggregated result-rows assertion.
fn single_bucket_aggregation() -> AggregationResult {
    AggregationResult {
        buckets: vec![AggregationBucket {
            key: Vec::new(),
            value: Some(BigDecimal::from(42)),
        }],
    }
}

/// A sample create-surface submission carrying a single `key=value` metadata
/// entry (identity-free, for the `create_usage_records` entry point).
fn create_record_with_metadata(key: &str, value: &str) -> CreateUsageRecord {
    let mut record = sample_create_record();
    record.metadata.insert(
        MetadataKey::new(key).expect("valid metadata key"),
        value.to_owned(),
    );
    record
}

// ── By-id point lookup ───────────────────────────────────────────────

#[tokio::test]
async fn point_lookup_pdp_deny_records_a_true_deny_despite_the_notfound_response() {
    // The point lookup answers a denied caller with `NotFound` so it cannot be
    // used as an existence oracle, but `uc_authz_decisions_total` must still
    // carry the decision the PDP actually returned — collapsing the metric too
    // would blind the deny-anomaly alert (DESIGN §3.11.6) to exactly the
    // reconnaissance the collapse exists to frustrate.
    let plugin = HappyPathPlugin::new();
    plugin.set_get_record(sample_record());

    let (service, provider, exporter) = ServiceFixture::default()
        .with_resolver(Arc::new(DenyAllResolver))
        .build_with_metrics(plugin, "test.metrics.get_record.deny.v1");

    let result = service
        .get_usage_record(&authenticated_ctx(), Uuid::from_u128(0x1234))
        .await;
    assert!(
        matches!(result, Err(UsageCollectorError::NotFound { .. })),
        "PDP deny must collapse to NotFound on the caller surface: {result:?}",
    );
    provider.force_flush().unwrap();

    assert_eq!(
        counter_sum_with_label(&exporter, "uc_authz_decisions_total", "decision", "deny"),
        1,
        "the operator-facing decision counter MUST record the deny the PDP \
         returned, not the NotFound the caller was handed",
    );
    assert_eq!(
        counter_sum_with_label(
            &exporter,
            "uc_authz_decisions_total",
            "operation",
            "get_record",
        ),
        1,
        "the deny MUST be attributed to the point-lookup operation",
    );
    assert_eq!(
        counter_sum_with_label(&exporter, "uc_authz_decisions_total", "decision", "permit"),
        0,
        "a denied lookup MUST NOT also record a permit",
    );
}

#[tokio::test]
async fn point_lookup_pdp_deny_labels_the_query_counter_denied_authz_like_its_siblings() {
    // `uc_query_requests_total`'s vocabulary is declared once for every
    // `query_kind` (DESIGN §3.11.5), so a PDP deny labels `(denied, authz)` on the
    // point surface too. It cannot reach that through the classifier —
    // `collapse_deny_to_not_found` rewrites the deny into a `NotFound` first, and
    // that collapse must stay — so it is classified from the PDP's own outcome.
    let plugin = HappyPathPlugin::new();
    plugin.set_get_record(sample_record());

    let (service, provider, exporter) = ServiceFixture::default()
        .with_resolver(Arc::new(DenyAllResolver))
        .build_with_metrics(plugin, "test.metrics.get_record.denylabels.v1");

    let result = service
        .get_usage_record(&authenticated_ctx(), Uuid::from_u128(0x1234))
        .await;
    assert!(
        matches!(result, Err(UsageCollectorError::NotFound { .. })),
        "the collapse must survive this change: a denied caller still reads \
         NotFound, never a deny: {result:?}",
    );
    provider.force_flush().expect("flush");

    assert_eq!(
        counter_sum_with_label(&exporter, "uc_query_requests_total", "query_kind", "point"),
        1,
        "the denied point lookup MUST record one attempt on the point surface",
    );
    assert_eq!(
        counter_sum_with_label(&exporter, "uc_query_requests_total", "outcome", "denied"),
        1,
        "a PDP deny MUST label `outcome=denied`, as it does on the three \
         sibling query surfaces, not the `error` the collapsed NotFound \
         would otherwise produce",
    );
    assert_eq!(
        counter_sum_with_label(
            &exporter,
            "uc_query_requests_total",
            "error_category",
            "authz"
        ),
        1,
        "a PDP deny MUST label `error_category=authz`, not the \
         `unknown_usage_type` the collapsed NotFound would otherwise produce",
    );
    assert_eq!(
        counter_sum_with_label(&exporter, "uc_query_requests_total", "outcome", "error"),
        0,
        "a deny is not an error on this counter; an operator graphing deny \
         rate across `query_kind` must not have to special-case `point`",
    );
}

#[tokio::test]
async fn point_lookup_genuine_miss_labels_record_not_found_not_unknown_usage_type() {
    // With denies routed away from the classifier by the test above, every
    // `NotFound` reaching it here is a record that genuinely is not there — not
    // what `unknown_usage_type` names (the Type Resolver's `DeclarationNotFound`).
    // Folding them would make "point not-found volume" move when a meter type goes
    // undeclared, and vice versa.
    let plugin = HappyPathPlugin::new();
    let missing = Uuid::from_u128(0x1234);
    plugin.set_get_usage_record_not_found(missing);

    let (service, provider, exporter) = ServiceFixture::default()
        .with_resolver(tenant_scoped_permit())
        .build_with_metrics(plugin, "test.metrics.get_record.miss.v1");

    let result = service
        .get_usage_record(&authenticated_ctx(), missing)
        .await;
    assert!(
        matches!(result, Err(UsageCollectorError::NotFound { .. })),
        "expected the plugin's miss to surface as NotFound: {result:?}",
    );
    provider.force_flush().expect("flush");

    assert_eq!(
        counter_sum_with_label(
            &exporter,
            "uc_query_requests_total",
            "error_category",
            "record_not_found",
        ),
        1,
        "a record that is genuinely absent MUST be labelled for what it is",
    );
    assert_eq!(
        counter_sum_with_label(
            &exporter,
            "uc_query_requests_total",
            "error_category",
            "unknown_usage_type",
        ),
        0,
        "`unknown_usage_type` names an unresolvable `gts_type_id`, and the \
         point lookup resolves no declaration to fail at; leaving a miss on \
         that label makes the two conditions one series",
    );
    assert_eq!(
        counter_sum_with_label(&exporter, "uc_query_requests_total", "outcome", "error"),
        1,
        "a miss is still an `error` outcome, as it is on the sibling surfaces",
    );
}

#[tokio::test]
async fn a_point_lookup_feeds_the_counter_and_the_duration_but_not_the_rows_histogram() {
    // DESIGN §3.11.5 declares `point` on `uc_query_requests_total` and
    // `uc_query_duration_seconds`, but declares `uc_query_result_rows` with
    // `aggregated`, `raw` only — so the negative half below is what rules out
    // an implementation that (wrongly) recorded all three.
    let plugin = HappyPathPlugin::new();
    plugin.set_get_record(sample_record());

    let (service, provider, exporter) = ServiceFixture::default()
        .with_resolver(tenant_scoped_permit())
        .build_with_metrics(plugin, "test.metrics.get_record.point.v1");

    let result = service
        .get_usage_record(&authenticated_ctx(), Uuid::from_u128(0x1234))
        .await;
    assert!(
        result.is_ok(),
        "expected a successful point lookup: {result:?}"
    );
    provider.force_flush().expect("flush");

    assert_eq!(
        counter_sum_with_label(&exporter, "uc_query_requests_total", "query_kind", "point"),
        1,
    );
    assert_eq!(
        histogram_count_with_label(
            &exporter,
            "uc_query_duration_seconds",
            "query_kind",
            "point",
        ),
        1,
    );
    // The negative half. DESIGN §3.11.5 declares `uc_query_result_rows` with
    // `aggregated`, `raw` only, so `point` must appear on it NOWHERE.
    assert_eq!(
        histogram_count_with_label(&exporter, "uc_query_result_rows", "query_kind", "point"),
        0,
        "uc_query_result_rows declares `aggregated`, `raw` only; a point sample there \
         would be a label the published row does not carry",
    );
}

// ── Query gateway ────────────────────────────────────────────────────

#[tokio::test]
async fn query_raw_deny_records_denied_authz_and_inflight_net_zero() {
    let (service, provider, exporter) = ServiceFixture::default()
        .with_resolver(Arc::new(DenyAllResolver))
        .build_with_metrics(HappyPathPlugin::new(), "test.metrics.query.deny.v1");

    let _outcome = service
        .list_usage_records(
            &authenticated_ctx(),
            MeterTypeId::new(SAMPLE_METER_TYPE_ID).expect("valid gts_type_id"),
            test_time_range(),
            &ODataQuery::default(),
            &[],
        )
        .await;
    provider.force_flush().unwrap();

    assert_eq!(
        counter_sum_with_label(&exporter, "uc_query_requests_total", "query_kind", "raw"),
        1,
    );
    assert_eq!(
        counter_sum_with_label(&exporter, "uc_query_requests_total", "outcome", "denied"),
        1,
    );
    assert_eq!(histogram_count(&exporter, "uc_query_duration_seconds"), 1);
    // Deny occurs before the inflight guard is entered → the gauge must not
    // leak a positive value (0 or never-emitted).
    assert!(matches!(
        gauge_last(&exporter, "uc_query_inflight"),
        None | Some(0)
    ));
}

// ── Ingestion gateway ────────────────────────────────────────────────

#[tokio::test]
async fn ingestion_deny_records_rejected_authz_duration_and_a_partial_request_point() {
    // What the recorded point SAYS is decided by the code, not by the shape one
    // might expect: a submission whose every entry was rejected is reported
    // `outcome=partial, error_category=none`, NOT `rejected`/`authz`.
    // `create_usage_records_for_origin` chooses `IngestRequestOutcome::Partial`
    // whenever `per_record.iter().any(Result::is_err)`; the request-wide `rejected`
    // arm is reserved for refusals that produced no per-record verdicts at all, and
    // `IngestRequestErrorCategory` has no `Authz` variant. This is batch-path
    // vocabulary tied to REST's 207 semantics, recorded as a divergence.
    let (service, provider, exporter) = ServiceFixture::default()
        .with_resolver(Arc::new(DenyAllResolver))
        .build_with_metrics(HappyPathPlugin::new(), "test.metrics.ingest.single.v1");

    let outcome = service
        .create_usage_records(&authenticated_ctx(), vec![sample_create_record()])
        .await
        .expect("a PDP deny is a per-record verdict, not a request-wide refusal")
        .into_iter()
        .next()
        .expect("one entry in, one slot out");
    assert!(
        outcome.is_err(),
        "the deny must land in the entry's own slot: {outcome:?}",
    );
    provider.force_flush().unwrap();

    assert_eq!(
        counter_sum_with_label(
            &exporter,
            "uc_ingestion_records_total",
            "outcome",
            "rejected"
        ),
        1,
    );
    assert_eq!(
        counter_sum_with_label(
            &exporter,
            "uc_ingestion_records_total",
            "entry_type",
            "record"
        ),
        1,
    );
    assert_eq!(
        counter_sum_with_label(
            &exporter,
            "uc_ingestion_records_total",
            "error_category",
            "authz",
        ),
        1,
    );
    assert_eq!(
        histogram_count(&exporter, "uc_ingestion_duration_seconds"),
        1
    );
    // The adapter tests prove the instruments emit whatever origin they are
    // handed; these prove the live wrapper hands them `live`. Without them,
    // hardcoding `RecordOrigin::Backfill` in the wrapper passes the suite.
    assert_eq!(
        counter_sum_with_label(&exporter, "uc_ingestion_records_total", "origin", "live"),
        1,
    );
    assert_eq!(
        histogram_count_with_label(&exporter, "uc_ingestion_duration_seconds", "origin", "live"),
        1,
    );
    // A one-entry submission completes a request like any other, so the counter
    // moves — to `partial`, for the reason the header comment sets out.
    assert_eq!(
        counter_sum_with_label(
            &exporter,
            "uc_ingestion_requests_total",
            "outcome",
            "partial"
        ),
        1,
        "a one-entry submission whose single entry was PDP-denied still completes \
         a request, and the batch vocabulary reports an all-rejected submission as \
         `partial` (DESIGN's HTTP-207 outcome), not `rejected`",
    );
    assert_eq!(
        counter_sum_with_label(
            &exporter,
            "uc_ingestion_requests_total",
            "error_category",
            "none",
        ),
        1,
        "`partial` carries `error_category=none`: the authz category lives on the \
         per-record counter asserted above, and the request-wide label does not \
         repeat it",
    );
    // The divergence, pinned rather than narrated: a submission every one of
    // whose entries was refused reads `partial`, which an operator could
    // reasonably expect to read `rejected`. If a later slice distinguishes the
    // all-rejected case, these two assertions have to be re-derived.
    assert_eq!(
        counter_sum_with_label(
            &exporter,
            "uc_ingestion_requests_total",
            "outcome",
            "rejected"
        ),
        0,
        "`rejected` is reserved for refusals that produced no per-record verdicts \
         at all (cap, quota, unready plugin); this submission produced one",
    );
    assert_eq!(
        counter_sum_with_label(
            &exporter,
            "uc_ingestion_requests_total",
            "error_category",
            "authz",
        ),
        0,
        "there is no request-wide authz category to record: \
         `IngestRequestErrorCategory` declares five values and `authz` is not \
         among them",
    );
}

#[tokio::test]
async fn ingestion_batch_all_denied_observes_batch_size_and_partial_request() {
    let (service, provider, exporter) = ServiceFixture::default()
        .with_resolver(Arc::new(DenyAllResolver))
        .build_with_metrics(HappyPathPlugin::new(), "test.metrics.ingest.batch.v1");

    let result = service
        .create_usage_records(
            &authenticated_ctx(),
            vec![sample_create_record(), sample_create_record()],
        )
        .await;
    assert!(
        result.is_ok(),
        "batch returns per-record outcomes, not an outer Err"
    );
    provider.force_flush().unwrap();

    assert_eq!(histogram_count(&exporter, "uc_ingestion_batch_size"), 1);
    // Two records, both PDP-denied → two rejected/authz per-record increments.
    assert_eq!(
        counter_sum_with_label(
            &exporter,
            "uc_ingestion_records_total",
            "outcome",
            "rejected"
        ),
        2,
    );
    // Any per-record rejection → the request is HTTP 207 → outcome="partial".
    assert_eq!(
        counter_sum_with_label(
            &exporter,
            "uc_ingestion_requests_total",
            "outcome",
            "partial"
        ),
        1,
    );
    assert_eq!(
        histogram_count(&exporter, "uc_ingestion_duration_seconds"),
        1
    );
    // Both entries travelled the live batch wrapper, so both per-entry
    // increments and the one duration observation carry `origin="live"`.
    assert_eq!(
        counter_sum_with_label(&exporter, "uc_ingestion_records_total", "origin", "live"),
        2,
    );
    assert_eq!(
        histogram_count_with_label(&exporter, "uc_ingestion_duration_seconds", "origin", "live"),
        1,
    );
}

#[tokio::test]
async fn ingestion_backfill_batch_labels_both_instruments_backfill() {
    // The backfill route shares the live route's telemetry block, so what needs
    // pinning is the `origin` it hands both ingestion instruments.
    // `DenyAllResolver` keeps the batch short of the plugin: these labels are
    // recorded on the completion path whatever each per-record outcome was.
    let (service, provider, exporter) = ServiceFixture::default()
        .with_resolver(Arc::new(DenyAllResolver))
        .build_with_metrics(HappyPathPlugin::new(), "test.metrics.backfill.batch.v1");

    let result = service
        .backfill_usage_records(
            &authenticated_ctx(),
            vec![sample_create_record(), sample_create_record()],
        )
        .await;
    assert!(
        result.is_ok(),
        "batch returns per-record outcomes, not an outer Err"
    );
    provider.force_flush().unwrap();

    assert_eq!(
        counter_sum_with_label(
            &exporter,
            "uc_ingestion_records_total",
            "origin",
            "backfill"
        ),
        2,
    );
    assert_eq!(
        histogram_count_with_label(
            &exporter,
            "uc_ingestion_duration_seconds",
            "origin",
            "backfill"
        ),
        1,
    );
    // No live share at all: a wrapper that stamped `Live` would satisfy an
    // assertion on the totals but not this one.
    assert_eq!(
        counter_sum_with_label(&exporter, "uc_ingestion_records_total", "origin", "live"),
        0,
    );
}

#[tokio::test]
async fn the_pdp_operation_label_follows_the_backfill_route_not_the_verb() {
    // `PdpOp::Backfill.as_str()` and `usage_record::actions::BACKFILL` are both
    // the string "backfill" and mean different things: a route label versus an
    // elevated verb. `sample_create_record`'s period is inside the configured
    // backfill window, so this batch is where the two disagree — labelled
    // `operation="backfill"`, authorized against `create`. A route left on
    // `PdpOp::Ingest` would fold a bulk import's PDP latency and denial rate into
    // live emission's series unnoticed.
    let resolver = ActionRecordingPermitResolver::new();
    let (service, provider, exporter) = ServiceFixture::default()
        .with_source(fake_declaration_source_with_fold("SUM"))
        .with_resolver(Arc::clone(&resolver) as Arc<dyn AuthZResolverApi>)
        .build_with_metrics(HappyPathPlugin::new(), "test.metrics.backfill.pdp.v1");

    let _outcome = service
        .backfill_usage_records(&authenticated_ctx(), vec![sample_create_record()])
        .await;
    provider.force_flush().unwrap();

    assert_eq!(
        resolver.actions_sorted(),
        vec!["create".to_owned()],
        "a period inside the backfill window needs no privilege a live \
         emission does not",
    );
    assert_eq!(
        counter_sum_with_label(
            &exporter,
            "uc_authz_decisions_total",
            "operation",
            "backfill"
        ),
        1,
    );
    assert_eq!(
        counter_sum_with_label(&exporter, "uc_authz_decisions_total", "operation", "ingest"),
        0,
        "the label is the route, and this entry did not travel the live one",
    );
}

// ── Ingestion: the entry_type share and the invalidation error family ──
//
// `uc_ingestion_records_total` carries the throughput NFR, and `entry_type` is
// what makes the correction share visible in the ingestion profile at all
// (DESIGN §3.11.5). Both tests drive the real service, so the label comes from
// the submission's own declared `entry_type`.

/// The persisted measurement [`sample_withdrawal`] withdraws: the
/// projection of [`sample_create_record`], so it carries the identity the
/// gateway derives rather than a hand-picked one.
fn sample_target_row() -> UsageRecord {
    projected(&sample_create_record())
}

/// A faithful withdrawal of [`sample_target_row`]: the create-surface twin
/// of `sample_create_record`, repeating its idempotency key and departing
/// only in `entry_type` and the reason.
fn sample_withdrawal() -> CreateUsageRecord {
    CreateUsageRecord {
        entry_type: EntryType::Invalidation,
        invalidation: Some(ReasonCode::new("emitter_defect").expect("valid reason code")),
        ..sample_create_record()
    }
}

#[tokio::test]
async fn an_invalidation_counts_under_its_own_entry_type() {
    let plugin = HappyPathPlugin::new();
    plugin.set_get_record(sample_target_row());
    plugin.set_create_records(vec![Ok(sample_record())]);

    let (service, provider, exporter) = ServiceFixture::default()
        .with_resolver(CountingTenantPermitResolver::new())
        .with_source(fake_declaration_source_with_fold("SUM"))
        .build_with_metrics(plugin, "test.metrics.ingest.entry_type.v1");

    service
        .create_usage_records(&authenticated_ctx(), vec![sample_withdrawal()])
        .await
        .expect("a faithful withdrawal of a resolvable target is not refused request-wide")
        .into_iter()
        .next()
        .expect("one entry in, one slot out")
        .expect("a faithful withdrawal of a resolvable target is accepted");
    provider.force_flush().unwrap();

    assert_eq!(
        counter_sum_with_label(
            &exporter,
            "uc_ingestion_records_total",
            "entry_type",
            "invalidation",
        ),
        1,
    );
    // And nothing landed on the measurement series — a label that counted
    // every entry the same way would satisfy the assertion above alone.
    assert_eq!(
        counter_sum_with_label(
            &exporter,
            "uc_ingestion_records_total",
            "entry_type",
            "record"
        ),
        0,
    );
}

#[tokio::test]
async fn an_invalidation_rule_rejection_carries_its_own_error_category() {
    let plugin = HappyPathPlugin::new();
    // The target departs from the submission in `quantity`, so the copy rule
    // rejects it.
    let mut row = sample_target_row();
    row.quantity = qty("999");
    plugin.set_get_record(row);

    let (service, provider, exporter) = ServiceFixture::default()
        .with_resolver(CountingTenantPermitResolver::new())
        .with_source(fake_declaration_source_with_fold("SUM"))
        .build_with_metrics(plugin, "test.metrics.ingest.invalidation_rule.v1");

    let err = service
        .create_usage_records(&authenticated_ctx(), vec![sample_withdrawal()])
        .await
        .expect("a copy-rule rejection is a per-record verdict, not a request-wide refusal")
        .into_iter()
        .next()
        .expect("one entry in, one slot out")
        .expect_err("an unfaithful copy is rejected");
    assert!(
        matches!(err, UsageCollectorError::InvalidArgument { .. }),
        "expected the copy rejection, got {err:?}",
    );
    provider.force_flush().unwrap();

    assert_eq!(
        counter_sum_with_label(
            &exporter,
            "uc_ingestion_records_total",
            "error_category",
            "invalidation_rule",
        ),
        1,
    );
    assert_eq!(
        counter_sum_with_label(
            &exporter,
            "uc_ingestion_records_total",
            "error_category",
            "semantics_violation",
        ),
        0,
        "the invalidation family must not fall back into the semantics one",
    );
}

// ── PDP-helper instruments (uc_authz_decisions_total / uc_pdp_failures_total /
//    uc_pdp_duration_seconds) ──────────────────────────────────────────────

#[tokio::test]
async fn pdp_deny_emits_deny_decision_not_failure() {
    let (service, provider, exporter) = ServiceFixture::default()
        .with_resolver(Arc::new(DenyAllResolver))
        .build_with_metrics(HappyPathPlugin::new(), "test.metrics.pdp.deny.v1");

    // PDP denial settles the entry in its own slot inside
    // `create_usage_records_inner` before the plugin or the Type Resolver are
    // reached, so an unprogrammed plugin and an inert resolver are fine here.
    let _outcome = service
        .create_usage_records(&authenticated_ctx(), vec![sample_create_record()])
        .await;
    provider.force_flush().unwrap();

    assert_eq!(
        counter_sum_with_label(&exporter, "uc_authz_decisions_total", "decision", "deny"),
        1,
    );
    // A deny is a decision, not a failure.
    assert_eq!(
        counter_sum_with_label(&exporter, "uc_pdp_failures_total", "operation", "ingest"),
        0,
    );
}

#[tokio::test]
async fn pdp_unreachable_emits_failure_not_decision() {
    let (service, provider, exporter) = ServiceFixture::default()
        .with_resolver(Arc::new(UnreachableResolver))
        .build_with_metrics(HappyPathPlugin::new(), "test.metrics.pdp.unreachable.v1");

    let _outcome = service
        .create_usage_records(&authenticated_ctx(), vec![sample_create_record()])
        .await;
    provider.force_flush().unwrap();

    assert_eq!(
        counter_sum_with_label(&exporter, "uc_pdp_failures_total", "cause", "unreachable"),
        1,
    );
    // A transport failure is not a decision.
    assert_eq!(
        counter_sum_with_label(&exporter, "uc_authz_decisions_total", "operation", "ingest"),
        0,
    );
    // Failure completions still observe duration.
    assert_eq!(histogram_count(&exporter, "uc_pdp_duration_seconds"), 1);
}

// ── Two-stage authorization: the decision counter records the EFFECTIVE gear
//    decision, not the raw `access_scope_with` return ──────────────────────────
//
// The PDP returns a permit-with-constraints, then a gear-side gate
// (`scope_admits_attribution_tuple` per-record, `scope_to_odata_filter` for LIST)
// can turn it into a deny. `uc_authz_decisions_total` must reflect the gear's
// decision, or DESIGN §3.11.6's deny-anomaly alert never sees the cross-tenant
// reconnaissance signal.

#[tokio::test]
async fn pdp_permit_with_foreign_tenant_gate_denial_records_deny_not_permit() {
    // The PDP permits but scopes the grant to ONE tenant (`granted`); the record
    // names a DIFFERENT one, so `scope_admits_attribution_tuple` turns the
    // constrained permit into a deny — the reconnaissance signal the deny-anomaly
    // alert keys off. The counter MUST record `deny`, never the raw PDP `permit`.
    let granted = Uuid::from_u128(0x5001);
    // sample_record() names tenant Uuid::from_u128(2), outside `granted`.
    let resolver =
        CountingPermitResolver::new(pep_properties::OWNER_TENANT_ID, granted.to_string());

    let (service, provider, exporter) = ServiceFixture::default()
        .with_resolver(resolver)
        .build_with_metrics(HappyPathPlugin::new(), "test.metrics.pdp.gatedeny.v1");

    let outcome = service
        .create_usage_records(&authenticated_ctx(), vec![sample_create_record()])
        .await
        .expect("an attribution-gate denial is a per-record verdict, not a request-wide refusal")
        .into_iter()
        .next()
        .expect("one entry in, one slot out");
    assert!(
        outcome.is_err(),
        "a record attributed to a tenant outside the granted scope must be denied",
    );
    provider.force_flush().unwrap();

    assert_eq!(
        counter_sum_with_label(&exporter, "uc_authz_decisions_total", "decision", "deny"),
        1,
        "the attribution-gate denial is the effective decision and must record `deny`",
    );
    assert_eq!(
        counter_sum_with_label(&exporter, "uc_authz_decisions_total", "decision", "permit"),
        0,
        "the premature permit from the raw PDP return must be suppressed",
    );
    assert_eq!(
        counter_sum_with_label(&exporter, "uc_authz_decisions_total", "operation", "ingest"),
        1,
        "exactly one decision sample for the one ingest authorization (no double-count)",
    );
    assert_eq!(histogram_count(&exporter, "uc_pdp_duration_seconds"), 1);
}

#[tokio::test]
async fn list_projection_denial_records_deny_not_permit() {
    // The PDP permits with a constraint that narrows ONLY by `resource_type` —
    // no `OWNER_TENANT_ID` pin — which `scope_to_odata_filter` fails closed. The
    // effective LIST decision is a deny, so the counter must record `deny`.
    let resolver =
        CountingPermitResolver::new(usage_record::PROP_RESOURCE_TYPE, "compute.vm".to_owned());

    let (service, provider, exporter) = ServiceFixture::default()
        .with_resolver(resolver)
        .build_with_metrics(HappyPathPlugin::new(), "test.metrics.list.projdeny.v1");

    let outcome = service
        .list_usage_records(
            &authenticated_ctx(),
            MeterTypeId::new(SAMPLE_METER_TYPE_ID).expect("valid gts_type_id"),
            test_time_range(),
            &ODataQuery::default(),
            &[],
        )
        .await;
    assert!(
        outcome.is_err(),
        "a tenant-less PDP scope must fail closed on the LIST path",
    );
    provider.force_flush().unwrap();

    assert_eq!(
        counter_sum_with_label(&exporter, "uc_authz_decisions_total", "decision", "deny"),
        1,
        "a scope that fails projection is a fail-closed deny, not a permit",
    );
    assert_eq!(
        counter_sum_with_label(&exporter, "uc_authz_decisions_total", "decision", "permit"),
        0,
        "the premature permit from the raw PDP return must be suppressed",
    );
    assert_eq!(
        counter_sum_with_label(
            &exporter,
            "uc_authz_decisions_total",
            "operation",
            "query_raw"
        ),
        1,
    );
}

#[tokio::test]
async fn per_record_permit_records_exactly_one_permit_no_double_count() {
    // The no-double-count invariant: a clean permit (the PDP grants the record's
    // own tenant AND the gate admits) emits exactly ONE `permit` decision and ONE
    // duration sample — never `permit` + `deny`, which would corrupt both sides of
    // the deny-anomaly ratio.
    let plugin = HappyPathPlugin::new();
    plugin.set_create_records(vec![Ok(sample_record())]);

    let (service, provider, exporter) = ServiceFixture::default()
        .with_resolver(CountingTenantPermitResolver::new())
        .with_source(fake_declaration_source_with_fold("SUM"))
        .build_with_metrics(plugin, "test.metrics.perrecord.permit.v1");

    service
        .create_usage_records(&authenticated_ctx(), vec![sample_create_record()])
        .await
        .expect("a permitted one-entry submission is not refused request-wide")
        .into_iter()
        .next()
        .expect("one entry in, one slot out")
        .expect("a permitted entry persists");
    provider.force_flush().unwrap();

    assert_eq!(
        counter_sum_with_label(&exporter, "uc_authz_decisions_total", "decision", "permit"),
        1,
    );
    assert_eq!(
        counter_sum_with_label(&exporter, "uc_authz_decisions_total", "decision", "deny"),
        0,
    );
    assert_eq!(
        counter_sum_with_label(&exporter, "uc_authz_decisions_total", "operation", "ingest"),
        1,
    );
    assert_eq!(histogram_count(&exporter, "uc_pdp_duration_seconds"), 1);
}

// ── Plugin-host instruments (uc_plugin_call_duration_seconds /
//    uc_plugin_accept_errors_total / uc_plugin_ready) ─────────────────────

#[tokio::test]
async fn plugin_backend_error_records_duration_counter_and_ready() {
    // `get_usage_record` authorizes via a pre-row compiled-scope PDP request, so
    // an unconstrained permit would fail closed under `require_constraints(true)`
    // before dispatch. `CountingPermitResolver` grants a real tenant-narrowing
    // scope, so the flow reaches the plugin and the unprogrammed `HappyPathPlugin`
    // returns a backend-classified `Internal`.
    let (service, provider, exporter) = ServiceFixture::default()
        .with_resolver(CountingPermitResolver::new(
            pep_properties::OWNER_TENANT_ID,
            Uuid::from_u128(2).to_string(),
        ))
        .build_with_metrics(HappyPathPlugin::new(), "test.metrics.plugin.backend.v1");

    let _outcome = service
        .get_usage_record(&authenticated_ctx(), Uuid::from_u128(0x01))
        .await;
    provider.force_flush().unwrap();

    assert_eq!(
        histogram_count(&exporter, "uc_plugin_call_duration_seconds"),
        1,
        "error completions are still dispatch completions",
    );
    assert_eq!(
        counter_sum_with_label(
            &exporter,
            "uc_plugin_accept_errors_total",
            "error_category",
            "backend_error",
        ),
        1,
    );
    assert_eq!(
        counter_sum_with_label(
            &exporter,
            "uc_plugin_accept_errors_total",
            "operation",
            "get_usage_record",
        ),
        1,
    );
    // A successful structural binding sets the readiness gauge to 1.
    assert_eq!(gauge_last(&exporter, "uc_plugin_ready"), Some(1));
}

#[tokio::test(start_paused = true)]
async fn plugin_dispatch_timeout_records_duration_and_timeout_counter() {
    let (metrics, provider, exporter) = local_metrics();

    let result = tokio::time::timeout(
        Duration::from_secs(1),
        super::instrument_spi(metrics.as_ref(), PluginOp::GetUsageRecord, async {
            tokio::time::sleep(Duration::from_secs(5)).await;
            Ok::<(), UsageCollectorPluginError>(())
        }),
    )
    .await
    .expect("plugin dispatch must be bounded");

    assert!(
        matches!(result, Err(UsageCollectorPluginError::Transient { ref detail, retry_after_seconds: None }) if detail.contains("timed out")),
        "dispatch timeout should become a retryable plugin transient: {result:?}"
    );
    provider.force_flush().unwrap();

    assert_eq!(
        histogram_count(&exporter, "uc_plugin_call_duration_seconds"),
        1,
        "timeout completions are still dispatch completions",
    );
    assert_eq!(
        counter_sum_with_label(
            &exporter,
            "uc_plugin_accept_errors_total",
            "error_category",
            "timeout",
        ),
        1,
    );
}

#[tokio::test]
async fn plugin_domain_typed_error_does_not_increment_accept_counter() {
    // A domain-typed variant (`UsageRecordNotFound`) is a caller-visible
    // outcome, NOT a plugin fault — its duration is still observed, but it MUST
    // NOT increment `uc_plugin_accept_errors_total`. As above, a real permitting
    // resolver is required to reach the plugin dispatch at all.
    let plugin = HappyPathPlugin::new();
    let id = Uuid::from_u128(0x02);
    plugin.set_get_usage_record_not_found(id);

    let (service, provider, exporter) = ServiceFixture::default()
        .with_resolver(CountingPermitResolver::new(
            pep_properties::OWNER_TENANT_ID,
            Uuid::from_u128(2).to_string(),
        ))
        .build_with_metrics(plugin, "test.metrics.plugin.domain.v1");

    let _outcome = service.get_usage_record(&authenticated_ctx(), id).await;
    provider.force_flush().unwrap();

    assert_eq!(
        histogram_count(&exporter, "uc_plugin_call_duration_seconds"),
        1,
    );
    assert_eq!(
        counter_sum_with_label(
            &exporter,
            "uc_plugin_accept_errors_total",
            "operation",
            "get_usage_record",
        ),
        0,
        "domain-typed plugin errors must not count as accept errors",
    );
}

#[tokio::test]
async fn plugin_unready_increments_unready_counter_and_zeroes_ready() {
    // The ingestion path resolves the plugin handle (`resolve_plugin_for`) ahead of
    // the PDP fan-out, so a structurally-unready binding short-circuits the whole
    // submission — an outer `Err`, not a per-record one. A permitting resolver
    // keeps the failure the readiness one.
    let (service, provider, exporter) = service_with_metrics_unready_plugin(
        "test.metrics.plugin.unready.v1",
        CountingTenantPermitResolver::new(),
    );

    let _outcome = service
        .create_usage_records(&authenticated_ctx(), vec![sample_create_record()])
        .await;
    provider.force_flush().unwrap();

    assert_eq!(
        counter_sum_with_label(
            &exporter,
            "uc_plugin_accept_errors_total",
            "error_category",
            "unready",
        ),
        1,
    );
    assert_eq!(gauge_last(&exporter, "uc_plugin_ready"), Some(0));
    // No SPI invocation occurred, so no duration sample is recorded.
    assert_eq!(
        histogram_count(&exporter, "uc_plugin_call_duration_seconds"),
        0,
    );
}

/// A `try_get_scoped` miss (DESIGN §3.2 Plugin Host) — "a dispatch that reached
/// no plugin at all" — carries the configured `unavailable_retry_after_secs` delay
/// (DESIGN §3.8), like a hintless plugin `Transient`.
/// `service_with_metrics_unready_plugin` hard-codes
/// `DEFAULT_UNAVAILABLE_RETRY_AFTER_SECS`, so that is what is asserted.
#[tokio::test]
async fn a_try_get_scoped_miss_also_carries_the_configured_unavailable_delay() {
    let (service, _provider, _exporter) = service_with_metrics_unready_plugin(
        "test.err.unready.retry.v1",
        CountingTenantPermitResolver::new(),
    );

    let outcome = service
        .create_usage_records(&authenticated_ctx(), vec![sample_create_record()])
        .await;

    // Request-wide: an unready binding is resolved before any entry is
    // dispatched, so the refusal is the outer `Err` and never a slot.
    let err = outcome.expect_err("an unready plugin binding must reject the dispatch");
    assert_eq!(
        err.retry_after(),
        Some(Duration::from_secs(
            crate::domain::service::DEFAULT_UNAVAILABLE_RETRY_AFTER_SECS
        )),
        "a try_get_scoped miss must carry the configured unavailable_retry_after_secs \
         delay, not None; got {err:?}"
    );
}

// ── Emit-path plugin-host instrument coverage ───────────────────────────────
//
// The emit SPI dispatches route through the same `instrument_spi` /
// `resolve_plugin_for` wrappers as the read paths, so a permitted emit MUST land
// on `uc_plugin_call_duration_seconds` (and, on a backend fault,
// `uc_plugin_accept_errors_total`) under the emit `operation` labels. The
// deny-path emit tests above short-circuit at PDP and cannot guard this wiring.
// The referenced meter's declaration resolves through the Type Resolver, so these
// tests wire a working resolver and assert no catalog dispatch.

#[tokio::test]
async fn ingestion_one_entry_success_dispatch_records_plugin_call_duration_per_op() {
    // Permit + declaration resolution + persist echo. The persist dispatch is
    // instrumented, contributing one duration sample under its own `operation`
    // label — `create_usage_records`, the gear's one ingestion dispatch.
    let plugin = HappyPathPlugin::new();
    plugin.set_create_records(vec![Ok(sample_record())]);

    let (service, provider, exporter) = ServiceFixture::default()
        .with_resolver(CountingTenantPermitResolver::new())
        .with_source(fake_declaration_source_with_fold("SUM"))
        .build_with_metrics(plugin, "test.metrics.ingestok.single.v1");

    service
        .create_usage_records(&authenticated_ctx(), vec![sample_create_record()])
        .await
        .expect("a permitted one-entry submission is not refused request-wide")
        .into_iter()
        .next()
        .expect("one entry in, one slot out")
        .expect("a permitted entry persists");
    provider.force_flush().unwrap();

    assert_eq!(
        histogram_count_with_label(
            &exporter,
            "uc_plugin_call_duration_seconds",
            "operation",
            "create_usage_records",
        ),
        1,
        "the persist SPI dispatch MUST contribute exactly one duration sample",
    );
    // A clean persist raises no backend fault.
    assert_eq!(
        counter_sum_with_label(
            &exporter,
            "uc_plugin_accept_errors_total",
            "operation",
            "create_usage_records",
        ),
        0,
    );
}

#[tokio::test]
async fn ingestion_batch_success_dispatch_records_plugin_call_duration_per_op() {
    // Two records sharing one gts_id but with distinct dedup identities — the
    // gateway collapses identical entries to a single dispatch, so identical
    // submissions would not exercise this batch-of-two path. The declaration
    // resolves once and the eligible records persist through one
    // `create_usage_records` dispatch.
    let plugin = HappyPathPlugin::new();
    plugin.set_create_records(vec![Ok(sample_record()), Ok(sample_record())]);

    let mut second = sample_create_record();
    second.idempotency_key = Some(IdempotencyKey::new("idem-2").expect("valid idempotency key"));

    let (service, provider, exporter) = ServiceFixture::default()
        .with_resolver(CountingTenantPermitResolver::new())
        .with_source(fake_declaration_source_with_fold("SUM"))
        .build_with_metrics(plugin, "test.metrics.ingestok.batch.v1");

    let per_record = service
        .create_usage_records(&authenticated_ctx(), vec![sample_create_record(), second])
        .await
        .expect("batch returns per-record outcomes, not an outer Err");
    assert!(
        per_record.iter().all(Result::is_ok),
        "both permitted records persist",
    );
    provider.force_flush().unwrap();

    assert_eq!(
        histogram_count_with_label(
            &exporter,
            "uc_plugin_call_duration_seconds",
            "operation",
            "create_usage_records",
        ),
        1,
        "the batch persist SPI dispatch MUST contribute exactly one duration sample",
    );
    // Every record persisted → the batch request completes as `accepted` (not
    // `partial`, which needs at least one per-record rejection).
    assert_eq!(
        counter_sum_with_label(
            &exporter,
            "uc_ingestion_requests_total",
            "outcome",
            "accepted",
        ),
        1,
    );
    assert_eq!(
        counter_sum_with_label(
            &exporter,
            "uc_ingestion_requests_total",
            "error_category",
            "none",
        ),
        1,
    );
    // An all-success request must NOT record `partial`.
    assert_eq!(
        counter_sum_with_label(
            &exporter,
            "uc_ingestion_requests_total",
            "outcome",
            "partial",
        ),
        0,
    );
}

#[tokio::test]
async fn ingestion_one_entry_backend_error_increments_accept_errors_per_op() {
    // Declaration resolves, then the persist SPI faults with `Internal` (backend).
    // The failed dispatch is still a completed dispatch (one duration sample) AND a
    // backend-classified accept error.
    //
    // The fault is induced by leaving `create_usage_records` unprogrammed so the
    // whole RPC answers `Internal`: `instrument_spi` reads the SPI call's OUTER
    // `Result` only, so a per-slot `Err` inside an outer `Ok` is a record outcome
    // and not a plugin accept error.
    let plugin = HappyPathPlugin::new();

    let (service, provider, exporter) = ServiceFixture::default()
        .with_resolver(CountingTenantPermitResolver::new())
        .with_source(fake_declaration_source_with_fold("SUM"))
        .build_with_metrics(plugin, "test.metrics.ingesterr.single.v1");

    let outcome = service
        .create_usage_records(&authenticated_ctx(), vec![sample_create_record()])
        .await;
    assert!(
        outcome.is_err(),
        "a whole-RPC persist fault surfaces as the outer Err, not a per-record slot",
    );
    provider.force_flush().unwrap();

    assert_eq!(
        counter_sum_with_label(
            &exporter,
            "uc_plugin_accept_errors_total",
            "operation",
            "create_usage_records",
        ),
        1,
    );
    assert_eq!(
        counter_sum_with_label(
            &exporter,
            "uc_plugin_accept_errors_total",
            "error_category",
            "backend_error",
        ),
        1,
    );
    assert_eq!(
        histogram_count_with_label(
            &exporter,
            "uc_plugin_call_duration_seconds",
            "operation",
            "create_usage_records",
        ),
        1,
        "an error completion is still a dispatch completion",
    );
}

#[tokio::test]
async fn ingestion_batch_backend_error_increments_accept_errors_per_op() {
    // Both declarations resolve, so the batch reaches the persist SPI;
    // `create_usage_records` is unprogrammed, so the stub's outer `Internal`
    // transport fault surfaces as the batch-level outer `Err`.
    //
    // TWO entries, with distinct dedup identities — `dispatch_eligible_entries`
    // collapses identical entries to one representative, so identical submissions
    // would travel as one and this would not be a batch. That is what keeps it
    // distinct from its one-entry sibling
    // `ingestion_one_entry_backend_error_increments_accept_errors_per_op`.
    let plugin = HappyPathPlugin::new();

    let mut second = sample_create_record();
    second.idempotency_key = Some(IdempotencyKey::new("idem-2").expect("valid idempotency key"));

    let (service, provider, exporter) = ServiceFixture::default()
        .with_resolver(CountingTenantPermitResolver::new())
        .with_source(fake_declaration_source_with_fold("SUM"))
        .build_with_metrics(plugin, "test.metrics.ingesterr.batch.v1");

    let outcome = service
        .create_usage_records(&authenticated_ctx(), vec![sample_create_record(), second])
        .await;
    assert!(
        outcome.is_err(),
        "a batch-level persist transport fault surfaces as an outer Err",
    );
    provider.force_flush().unwrap();

    // One, not two. `instrument_spi` records the accept-error counter once per SPI
    // *dispatch*, and a submission of any size crosses the persist SPI exactly
    // once, so a two-entry submission reading 2 would mean the host had split the
    // batch into per-record dispatches.
    assert_eq!(
        counter_sum_with_label(
            &exporter,
            "uc_plugin_accept_errors_total",
            "operation",
            "create_usage_records",
        ),
        1,
        "a two-entry submission crosses the persist SPI once, so one failed \
         dispatch is one accept error",
    );
    assert_eq!(
        counter_sum_with_label(
            &exporter,
            "uc_plugin_accept_errors_total",
            "error_category",
            "backend_error",
        ),
        1,
    );
    assert_eq!(
        histogram_count_with_label(
            &exporter,
            "uc_plugin_call_duration_seconds",
            "operation",
            "create_usage_records",
        ),
        1,
        "one dispatch, one duration sample, whatever the entry count",
    );
}

#[tokio::test]
async fn ingestion_batch_unready_plugin_increments_unready_counter() {
    // The batch path resolves the plugin (via `resolve_plugin_for`) BEFORE the
    // PDP fan-out; a structurally-unready binding short-circuits there and MUST
    // still emit the `unready` accept error.
    let (service, provider, exporter) = service_with_metrics_unready_plugin(
        "test.metrics.ingestunready.batch.v1",
        CountingTenantPermitResolver::new(),
    );

    let outcome = service
        .create_usage_records(&authenticated_ctx(), vec![sample_create_record()])
        .await;
    assert!(
        outcome.is_err(),
        "an unready plugin binding short-circuits the batch with an outer Err",
    );
    provider.force_flush().unwrap();

    assert_eq!(
        counter_sum_with_label(
            &exporter,
            "uc_plugin_accept_errors_total",
            "error_category",
            "unready",
        ),
        1,
    );
    assert_eq!(gauge_last(&exporter, "uc_plugin_ready"), Some(0));
    // Resolution failed before any SPI dispatch, so no duration sample lands.
    assert_eq!(
        histogram_count(&exporter, "uc_plugin_call_duration_seconds"),
        0,
    );
    // A whole-request (outer `Err`) failure is a single `rejected` batch
    // request classified as `plugin_error` — distinct from the per-record
    // `partial`/`accepted` arms.
    assert_eq!(
        counter_sum_with_label(
            &exporter,
            "uc_ingestion_requests_total",
            "outcome",
            "rejected",
        ),
        1,
    );
    assert_eq!(
        counter_sum_with_label(
            &exporter,
            "uc_ingestion_requests_total",
            "error_category",
            "plugin_error",
        ),
        1,
    );
}

// ── Ingestion gateway: the structural batch-size cap ───────────
//
// `create_usage_records_for_origin`'s `1..=max_batch_records` structural gate
// (empty or over-cap) rejects before any plugin dispatch. What this section guards
// is that the refusal is a request-wide `validation` outcome on
// `uc_ingestion_requests_total`, not an unrecorded rejection — DESIGN §3.11.5
// declares `validation` on that counter without narrowing it to per-entry
// semantics.

/// Asserts `err` is the `invalid_batch_size` shape: an `InvalidArgument` on
/// the `records` field under `ValidationReason::Validation`, whose `detail`
/// carries the `actual`/`min`/`max` bounds the caller was rejected against.
fn assert_invalid_batch_size(err: &UsageCollectorError, actual: usize, min: usize, max: usize) {
    assert!(
        matches!(
            err,
            UsageCollectorError::InvalidArgument {
                reason: ValidationReason::Validation,
                field,
                ..
            } if field == "records"
        ),
        "expected invalid_batch_size InvalidArgument on `records`, got {err:?}",
    );
    if let UsageCollectorError::InvalidArgument { detail, .. } = err {
        let expected = format!("batch size {actual} out of bounds (expected [{min}, {max}])");
        assert!(
            detail.contains(&expected),
            "expected detail to contain {expected:?}, got {detail:?}",
        );
    }
}

#[tokio::test]
async fn over_cap_submission_is_counted_as_a_request_wide_validation_rejection() {
    // The cap arm in `Service::create_usage_records_for_origin` records through
    // `Service::record_structural_cap_rejection`, which the REST edge's own copy
    // of the cap also calls; the handler-level arm is pinned separately in
    // `api::rest::handlers::usage_records::usage_records_tests`.
    let (service, provider, exporter) = ServiceFixture::default()
        .with_max_batch_records(2)
        .build_with_metrics(HappyPathPlugin::new(), "test.metrics.ingest.capover.v1");

    let err = service
        .create_usage_records(
            &authenticated_ctx(),
            vec![
                sample_create_record(),
                sample_create_record(),
                sample_create_record(),
            ],
        )
        .await
        .expect_err("3 records over a cap of 2 is rejected whole");
    assert_invalid_batch_size(&err, 3, 1, 2);
    provider.force_flush().unwrap();

    assert_eq!(
        counter_sum_with_label(
            &exporter,
            "uc_ingestion_requests_total",
            "outcome",
            "rejected"
        ),
        1,
    );
    assert_eq!(
        counter_sum_with_label(
            &exporter,
            "uc_ingestion_requests_total",
            "error_category",
            "validation",
        ),
        1,
    );
}

#[tokio::test]
async fn empty_submission_is_counted_as_a_request_wide_validation_rejection() {
    // Same oracle as the over-cap arm, on the other half of the same `||`
    // (`actual == 0`).
    let (service, provider, exporter) = ServiceFixture::default()
        .with_max_batch_records(2)
        .build_with_metrics(HappyPathPlugin::new(), "test.metrics.ingest.capempty.v1");

    let err = service
        .create_usage_records(&authenticated_ctx(), Vec::new())
        .await
        .expect_err("an empty submission is rejected whole");
    assert_invalid_batch_size(&err, 0, 1, 2);
    provider.force_flush().unwrap();

    assert_eq!(
        counter_sum_with_label(
            &exporter,
            "uc_ingestion_requests_total",
            "outcome",
            "rejected"
        ),
        1,
    );
    assert_eq!(
        counter_sum_with_label(
            &exporter,
            "uc_ingestion_requests_total",
            "error_category",
            "validation",
        ),
        1,
    );
}

#[tokio::test]
async fn a_one_entry_submission_records_one_accepted_request_point() {
    // A one-entry submission moves `uc_ingestion_requests_total` like any
    // other.
    let plugin = HappyPathPlugin::new();
    plugin.set_create_records(vec![Ok(sample_record())]);
    let (service, provider, exporter) = ServiceFixture::default()
        .with_resolver(CountingTenantPermitResolver::new())
        .with_source(fake_declaration_source_with_fold("SUM"))
        .build_with_metrics(plugin, "test.metrics.ingest.onereq.v1");

    service
        .create_usage_records(&authenticated_ctx(), vec![sample_create_record()])
        .await
        .expect("a one-entry submission is not refused request-wide")
        .into_iter()
        .next()
        .expect("one entry in, one slot out")
        .expect("a well-formed entry persists");
    provider.force_flush().unwrap();

    assert_eq!(
        counter_sum_with_label(
            &exporter,
            "uc_ingestion_requests_total",
            "outcome",
            "accepted"
        ),
        1,
        "a one-entry submission records exactly one accepted request point",
    );
}

#[tokio::test]
async fn a_one_entry_submission_observes_batch_size_one() {
    // Every single emit is a one-entry batch and must be observed as one, or
    // `uc_ingestion_batch_size` silently under-reports the gear's smallest and
    // most common submission.
    let plugin = HappyPathPlugin::new();
    plugin.set_create_records(vec![Ok(sample_record())]);
    let (service, provider, exporter) = ServiceFixture::default()
        .with_resolver(CountingTenantPermitResolver::new())
        .with_source(fake_declaration_source_with_fold("SUM"))
        .build_with_metrics(plugin, "test.metrics.ingest.onesize.v1");

    service
        .create_usage_records(&authenticated_ctx(), vec![sample_create_record()])
        .await
        .expect("a one-entry submission is not refused request-wide");
    provider.force_flush().unwrap();

    assert_eq!(
        histogram_count(&exporter, "uc_ingestion_batch_size"),
        1,
        "a one-entry submission contributes exactly one batch-size sample",
    );
    // The magnitude, not only the sample count: a histogram observed with `0`
    // is also one sample, and would under-report the submission while passing
    // the assertion above.
    assert!(
        (histogram_sum(&exporter, "uc_ingestion_batch_size") - 1.0).abs() < f64::EPSILON,
        "the one sample must carry the submitted entry count (1), got {}",
        histogram_sum(&exporter, "uc_ingestion_batch_size"),
    );
}

// ── Metric-label classifiers: exhaustive arm coverage (pure fns) ────────────
//
// `classify_record_error` and `classify_query_result` decide the
// `error_category` / `outcome` labels operators alert on. The end-to-end tests
// above reach only some of the label values and cannot reach the rest without a
// fixture per arm, so these table-driven unit tests pin EVERY arm of the closed
// §3.11.5 vocabularies.

/// The canonical sample `gts_type_id` as a typed id, for classifier fixtures on
/// the record / meter-reference surface (`list_usage_records`,
/// `query_aggregated_usage_records`, and `UnknownMetadataKey`).
fn meter_gts() -> MeterTypeId {
    MeterTypeId::new(SAMPLE_METER_TYPE_ID).expect("valid gts_type_id")
}

/// The `UsageCollectorError::NotFound` an unresolved `gts_type_id` produces —
/// the Type Resolver's `DomainError::DeclarationNotFound`, lifted through the
/// same bridge production uses, so this fixture cannot drift from what the
/// resolver actually produces.
fn unresolved_type_not_found() -> UsageCollectorError {
    crate::domain::error::DomainError::declaration_not_found(&meter_gts()).into()
}

/// `(input result, expected (outcome, error_category))` row for the query
/// classifier table.
type QueryClassifierCase = (
    Result<(), UsageCollectorError>,
    (RequestOutcome, QueryErrorCategory),
);

#[test]
fn classify_record_error_maps_each_arm() {
    let cases: Vec<(UsageCollectorError, RecordErrorCategory)> = vec![
        (
            UsageCollectorError::permission_denied("pdp"),
            RecordErrorCategory::Authz,
        ),
        // An unresolved `gts_type_id` (the Type Resolver's `DeclarationNotFound`)
        // → unknown_usage_type.
        (
            unresolved_type_not_found(),
            RecordErrorCategory::UnknownUsageType,
        ),
        // The other two `NotFoundReason`s. Both name a uuid, so only the typed
        // reason separates them (see
        // `not_found_classifies_by_typed_reason_not_by_parsing_the_name`).
        (
            UsageCollectorError::usage_record_not_found(Uuid::from_u128(7)),
            RecordErrorCategory::SemanticsViolation,
        ),
        (
            UsageCollectorError::invalidation_target_not_found(Uuid::from_u128(8)),
            RecordErrorCategory::InvalidationRule,
        ),
        // The two metadata reasons are the ONLY InvalidArgument arms that map to
        // metadata_size; any other validation reason is semantics_violation.
        (
            UsageCollectorError::metadata_size_exceeded(9000, 8192),
            RecordErrorCategory::MetadataSize,
        ),
        (
            UsageCollectorError::unknown_metadata_key(&meter_gts(), "region"),
            RecordErrorCategory::MetadataSize,
        ),
        // The `InvalidArgument` catch-all, held by a live reason rather than by
        // prose: `Validation` stands for every reason that reaches it from a
        // constructor in this workspace. `SemanticsViolation` is NOT among them —
        // the variant is reserved and no constructor produces it (see
        // `usage_collector_sdk::reason`). Without a case here a mutation of that
        // arm passes, because every other row names a reason handled explicitly.
        (
            UsageCollectorError::invalid_batch_size(0, 1, 1000),
            RecordErrorCategory::SemanticsViolation,
        ),
        // The copy rule carries its own category (DESIGN §3.11.5), so a
        // correction backlog is legible without reading `detail`. It is the only
        // invalidation rejection reaching this arm as an `InvalidArgument`.
        (
            UsageCollectorError::invalidation_field_mismatch("quantity", Uuid::from_u128(11)),
            RecordErrorCategory::InvalidationRule,
        ),
        // Conflict: idempotency is its own category, and so (DESIGN `:2239`) is
        // at-most-one — a second invalidation rejected as already invalidated is
        // `idempotency_conflict`, NOT a member of the invalidation-rule family
        // below. `ConflictReason::Unknown` is reachable only through `from_wire`.
        (
            UsageCollectorError::idempotency_conflict("k", Uuid::from_u128(9)),
            RecordErrorCategory::IdempotencyConflict,
        ),
        // A second invalidation under another reason code is the dedup conflict
        // DESIGN `:2239` names — see
        // `already_invalidated_is_an_idempotency_conflict_and_not_converged_is_not`.
        (
            UsageCollectorError::already_invalidated(AlreadyInvalidatedArgs {
                target: Uuid::from_u128(10),
                invalidated_by: Uuid::from_u128(13),
                reason_code: usage_collector_sdk::ReasonCode::new("emitter_defect")
                    .expect("valid reason code"),
            }),
            RecordErrorCategory::IdempotencyConflict,
        ),
        // Target not yet converged IS the invalidation family's — the
        // conformant half the split must not move.
        (
            UsageCollectorError::target_not_converged(Uuid::from_u128(14), None),
            RecordErrorCategory::InvalidationRule,
        ),
        // Anything unclassified is a plugin_error.
        (
            UsageCollectorError::internal("boom"),
            RecordErrorCategory::PluginError,
        ),
        (
            UsageCollectorError::service_unavailable("down", None),
            RecordErrorCategory::PluginError,
        ),
    ];
    for (err, expected) in cases {
        assert_eq!(
            classify_record_error(&err),
            expected,
            "misclassified {err:?}"
        );
    }
}

#[test]
fn already_invalidated_is_an_idempotency_conflict_and_not_converged_is_not() {
    // DESIGN `:2239` puts these two in DIFFERENT categories in one sentence. A
    // test asserting only one half would pass against a rewrite moving both.
    assert_eq!(
        classify_record_error(&UsageCollectorError::already_invalidated(
            AlreadyInvalidatedArgs {
                target: Uuid::from_u128(10),
                invalidated_by: Uuid::from_u128(13),
                reason_code: usage_collector_sdk::ReasonCode::new("emitter_defect")
                    .expect("valid reason code"),
            }
        )),
        RecordErrorCategory::IdempotencyConflict,
    );
    assert_eq!(
        classify_record_error(&UsageCollectorError::target_not_converged(
            Uuid::from_u128(14),
            None
        )),
        RecordErrorCategory::InvalidationRule,
        "TargetNotConverged was already conformant; DESIGN `:2239` says \
         `invalidation_rule` covers it, so this half must NOT move with the other",
    );
}

/// A target that resolves to nothing is an invalidation-rule rejection.
///
/// DESIGN §3.11.5 scopes `invalidation_rule` to "the target-resolution and copy
/// rules alone". Target resolution surfaces as `NotFound`, which a plugin's own
/// `UsageRecordNotFound` also reaches, so nothing but the typed reason separates
/// them — a label classified by substring match on a caller-facing string stops
/// matching the day the string is reworded.
///
/// The cases are asserted together because the discriminator is under test: any
/// one alone passes against a function returning a constant. Each carries the same
/// uuid `name` on purpose, so a `Uuid::parse_str(name)` discriminator and the
/// typed reason disagree about it.
#[test]
fn not_found_classifies_by_typed_reason_not_by_parsing_the_name() {
    let target = Uuid::new_v4();

    assert_eq!(
        classify_record_error(&UsageCollectorError::invalidation_target_not_found(target)),
        RecordErrorCategory::InvalidationRule,
        "an `invalidates` resolving to nothing is the ADR's valid-reference \
         rule and belongs with the other invalidation rules",
    );
    assert_eq!(
        classify_record_error(&UsageCollectorError::usage_record_not_found(target)),
        RecordErrorCategory::SemanticsViolation,
        "an ordinary missing entry is not an invalidation rule, and it carries \
         a uuid `name` exactly like the case above, which is why parsing \
         `name` could never separate them",
    );
    assert_eq!(
        classify_record_error(&UsageCollectorError::NotFound {
            resource_type: USAGE_RECORD_RESOURCE.to_owned(),
            name: target.to_string(),
            reason: NotFoundReason::DeclarationNotFound,
            detail: "GTS type not declared".to_owned(),
        }),
        RecordErrorCategory::UnknownUsageType,
        "the reason decides the category and the `name` does not: the \
         retired discriminator read a uuid `name` as an ordinary missing \
         entry and would have counted this as semantics_violation, so this \
         is the one case of the three that separates the two rules",
    );
}

#[test]
fn classify_query_result_maps_each_arm() {
    let cases: Vec<QueryClassifierCase> = vec![
        (Ok(()), (RequestOutcome::Success, QueryErrorCategory::None)),
        (
            Err(UsageCollectorError::permission_denied("pdp")),
            (RequestOutcome::Denied, QueryErrorCategory::Authz),
        ),
        (
            Err(unresolved_type_not_found()),
            (RequestOutcome::Error, QueryErrorCategory::UnknownUsageType),
        ),
        // The other reachable `NotFoundReason` on a query path, and the one the
        // three callers above cannot raise: the by-id point lookup resolves no
        // declaration, so its miss is a missing row, not a missing meter type.
        (
            Err(UsageCollectorError::usage_record_not_found(
                Uuid::from_u128(7),
            )),
            (RequestOutcome::Error, QueryErrorCategory::RecordNotFound),
        ),
        // Unreachable from a query path (it is the ingestion side's, via
        // `classify_record_error`), pinned so the arm is not deleted as
        // dead by a pass that reads only what the four callers raise.
        (
            Err(UsageCollectorError::invalidation_target_not_found(
                Uuid::from_u128(8),
            )),
            (RequestOutcome::Error, QueryErrorCategory::UnknownUsageType),
        ),
        // Every `InvalidArgument` is one category. The over-cap aggregate result
        // stands in for the query-budget / query-surface family. The mandatory read
        // range never lands here: it is validated at the edge, before the service
        // is entered.
        (
            Err(UsageCollectorError::aggregation_result_too_large(
                usage_collector_sdk::MAX_AGGREGATION_BUCKETS,
            )),
            (RequestOutcome::Error, QueryErrorCategory::QueryBudget),
        ),
        // The other arm, and why the classifier reads the upstream `toolkit_odata`
        // error a `CursorRejected` carries: a continuation refused because its
        // cursor was minted over a different query is not a budget rejection, and
        // collapsing both onto `query_budget` would leave the metric unable to tell
        // wrong paging from too-wide scanning.
        (
            Err(UsageCollectorError::cursor_query_mismatch()),
            (RequestOutcome::Error, QueryErrorCategory::FilterMismatch),
        ),
        // The same variant's other arm, which folds into `query_budget` for want
        // of a category that fits — the one known imprecision on this seam,
        // pinned so a vocabulary pass has to move it deliberately.
        (
            Err(UsageCollectorError::inadmissible_cursor_keyset(
                "mixed directions",
            )),
            (RequestOutcome::Error, QueryErrorCategory::QueryBudget),
        ),
        (
            Err(UsageCollectorError::internal("boom")),
            (RequestOutcome::Error, QueryErrorCategory::PluginError),
        ),
        (
            Err(UsageCollectorError::service_unavailable("down", None)),
            (RequestOutcome::Error, QueryErrorCategory::PluginError),
        ),
    ];
    for (result, expected) in cases {
        assert_eq!(
            classify_query_result(&result),
            expected,
            "misclassified {result:?}",
        );
    }
}

// ── Query gateway: success + aggregated coverage ────────────────────────────
//
// The deny test above exits at PDP before the inflight guard / dispatch; these
// drive a permitted raw list and a permitted aggregation to completion, pinning
// the `success` outcome, the result-rows observation (page size for raw / bucket
// count for aggregated), and the duration sample the deny path cannot reach.

#[tokio::test]
async fn query_raw_success_records_success_rows_and_duration() {
    let plugin = HappyPathPlugin::new();
    plugin.set_list_usage_records_response(record_page(3));

    // `list_usage_records` resolves the queried meter's declaration (Spec §3.11
    // `metadata_filter` gating), so a success-path test needs a
    // `DeclarationSource` that actually resolves — the default
    // `UnavailableDeclarationSource` would raise `ServiceUnavailable` first.
    let (service, provider, exporter) = ServiceFixture::default()
        .with_source(fake_declaration_source_with_fold("SUM"))
        .with_resolver(tenant_scoped_permit())
        .build_with_metrics(plugin, "test.metrics.query.rawok.v1");

    let page = service
        .list_usage_records(
            &authenticated_ctx(),
            meter_gts(),
            test_time_range(),
            &ODataQuery::default(),
            &[],
        )
        .await
        .expect("a permitted raw query succeeds");
    assert_eq!(page.items.len(), 3);
    provider.force_flush().unwrap();

    assert_eq!(
        counter_sum_with_label(&exporter, "uc_query_requests_total", "outcome", "success"),
        1,
    );
    assert_eq!(
        counter_sum_with_label(&exporter, "uc_query_requests_total", "query_kind", "raw"),
        1,
    );
    assert_eq!(
        counter_sum_with_label(
            &exporter,
            "uc_query_requests_total",
            "error_category",
            "none"
        ),
        1,
    );
    // Result rows observed exactly once, off the returned page size (3).
    assert_eq!(
        histogram_count_with_label(&exporter, "uc_query_result_rows", "query_kind", "raw"),
        1,
    );
    assert!(
        (histogram_sum_with_label(&exporter, "uc_query_result_rows", "query_kind", "raw") - 3.0)
            .abs()
            < f64::EPSILON,
        "the raw result-rows observation must carry the page size (3)",
    );
    assert_eq!(
        histogram_count_with_label(&exporter, "uc_query_duration_seconds", "query_kind", "raw"),
        1,
    );
}

/// Pins that a Type-Resolver-sourced `TypesRegistryUnavailable` still answers
/// `retry_after() == None` — the claim `domain::error::lift_domain_error`'s doc
/// makes about origin being discriminated by control flow (which function calls
/// the lift versus which stays on the bare `?`), not by any property of the
/// `DomainError` value itself (a plain `String`).
#[tokio::test]
async fn a_type_resolver_sourced_types_registry_unavailable_still_answers_no_retry_delay() {
    let plugin = HappyPathPlugin::new();

    // No `.with_source(...)`: the default inert `UnavailableDeclarationSource`
    // is what must be reached here.
    let service = ServiceFixture::default()
        .with_resolver(tenant_scoped_permit())
        .build(plugin, "test.metrics.query.tr_unavail.v1");

    let err = service
        .list_usage_records(
            &authenticated_ctx(),
            meter_gts(),
            test_time_range(),
            &ODataQuery::default(),
            &[],
        )
        .await
        .expect_err("an inert Type Resolver must fail the declaration lookup");

    assert_eq!(
        err.retry_after(),
        None,
        "a Type-Resolver-sourced TypesRegistryUnavailable must NOT carry the \
         configured unavailable_retry_after_secs default -- that default is \
         reserved for the Plugin Host's own registry lookup \
         (resolve_plugin_for / lift_domain_error), a different origin \
         producing the identical DomainError shape; got {err:?}"
    );

    // The lift half: `From<DomainError> for UsageCollectorError`'s bare
    // `TypesRegistryUnavailable` arm, reached here through `?` with no
    // `lift_domain_error` in between. The raising-site half of the pin stays
    // green without this assertion, which is why both are asserted.
    match err {
        UsageCollectorError::ServiceUnavailable { detail, .. } => {
            assert!(
                detail.contains(meter_gts().as_str()),
                "the lift must carry the Type Resolver's wrapped detail \
                 through (ruling K25) rather than substitute the SDK's \
                 fixed `types_registry_unavailable` string, which never \
                 named a meter at all; got detail: {detail:?}"
            );
        }
        other => panic!("expected ServiceUnavailable, got {other:?}"),
    }
}

#[tokio::test]
async fn query_aggregated_success_records_success_rows_and_duration() {
    let plugin = HappyPathPlugin::new();
    plugin.set_query_aggregated_usage_records_response(single_bucket_aggregation());

    // The aggregate path resolves the queried meter's declaration before
    // dispatch, so this test wires its own resolver over a fake source declaring
    // a fold rather than going through the shared (inert-resolver) builder.
    let hub = hub_with_plugin(plugin, "test.metrics.query.aggok.v1", "constructorfabric");
    let (metrics, provider, exporter) = local_metrics();
    let type_resolver = Arc::new(TypeResolver::new(
        fake_declaration_source_with_fold("SUM"),
        TypeResolverConfig {
            ttl: Duration::from_mins(1),
            capacity: 16,
        },
        metrics.clone(),
    ));
    let service = Arc::new(Service::new_with_metrics(
        hub,
        "constructorfabric".to_owned(),
        enforcer_for(tenant_scoped_permit()),
        metrics,
        type_resolver,
        crate::domain::validation::DEFAULT_METADATA_SIZE_CAP_BYTES,
        crate::domain::test_support::default_covered_period_bounds(),
        crate::domain::service::DEFAULT_MAX_BATCH_RECORDS,
        crate::config::IngestionQuotaConfig::default(),
        crate::domain::service::DEFAULT_UNAVAILABLE_RETRY_AFTER_SECS,
        crate::domain::service::DEFAULT_TARGET_NOT_CONVERGED_RETRY_AFTER_SECS,
        crate::domain::test_support::inert_reverse_resolver(Arc::new(
            crate::domain::ports::metrics::NoopMetrics,
        )),
    ));

    let result = service
        .query_aggregated_usage_records(
            &authenticated_ctx(),
            meter_gts(),
            test_time_range(),
            &ODataQuery::default(),
            &[],
            &[],
        )
        .await
        .expect("a permitted aggregation succeeds");
    assert_eq!(result.buckets.len(), 1);
    provider.force_flush().unwrap();

    assert_eq!(
        counter_sum_with_label(&exporter, "uc_query_requests_total", "outcome", "success"),
        1,
    );
    assert_eq!(
        counter_sum_with_label(
            &exporter,
            "uc_query_requests_total",
            "query_kind",
            "aggregated",
        ),
        1,
    );
    // Aggregated result rows are observed off the bucket count (1), NOT a page
    // size — this is the branch the raw success test cannot cover.
    assert_eq!(
        histogram_count_with_label(
            &exporter,
            "uc_query_result_rows",
            "query_kind",
            "aggregated",
        ),
        1,
    );
    assert!(
        (histogram_sum_with_label(
            &exporter,
            "uc_query_result_rows",
            "query_kind",
            "aggregated",
        ) - 1.0)
            .abs()
            < f64::EPSILON,
        "the aggregated result-rows observation must carry the bucket count (1)",
    );
    assert_eq!(
        histogram_count_with_label(
            &exporter,
            "uc_query_duration_seconds",
            "query_kind",
            "aggregated",
        ),
        1,
    );
}

// ── Emission: uc_record_metadata_bytes observe / skip ───────────────────────

#[tokio::test]
async fn ingestion_one_entry_with_metadata_observes_record_metadata_bytes() {
    // A permitted one-entry submission whose resolved declaration declares the
    // key it carries reaches `observe_metadata_bytes` and persists, once per
    // record.
    let plugin = HappyPathPlugin::new();
    plugin.set_create_records(vec![Ok(sample_record())]);

    let (service, provider, exporter) = ServiceFixture::default()
        .with_resolver(CountingTenantPermitResolver::new())
        .with_source(fake_declaration_source_with_metadata(&["region"]))
        .build_with_metrics(plugin, "test.metrics.ingest.meta.v1");

    service
        .create_usage_records(
            &authenticated_ctx(),
            vec![create_record_with_metadata("region", "us-east-1")],
        )
        .await
        .expect("a permitted one-entry submission is not refused request-wide")
        .into_iter()
        .next()
        .expect("one entry in, one slot out")
        .expect("a permitted record with declared metadata persists");
    provider.force_flush().unwrap();

    // A record carrying metadata contributes exactly one observation, whose
    // magnitude is the serialized JSON byte size (> 0).
    assert_eq!(histogram_count(&exporter, "uc_record_metadata_bytes"), 1);
    assert!(
        histogram_sum(&exporter, "uc_record_metadata_bytes") > 0.0,
        "the serialized metadata size must be a positive byte count",
    );
}

#[tokio::test]
async fn ingestion_one_entry_empty_metadata_skips_record_metadata_bytes() {
    // `sample_record()` carries no metadata → `observe_metadata_bytes` returns
    // before recording, so the instrument stays empty even on a clean persist.
    let plugin = HappyPathPlugin::new();
    plugin.set_create_records(vec![Ok(sample_record())]);

    let (service, provider, exporter) = ServiceFixture::default()
        .with_resolver(CountingTenantPermitResolver::new())
        .with_source(fake_declaration_source_with_fold("SUM"))
        .build_with_metrics(plugin, "test.metrics.ingest.nometa.v1");

    service
        .create_usage_records(&authenticated_ctx(), vec![sample_create_record()])
        .await
        .expect("a permitted one-entry submission is not refused request-wide")
        .into_iter()
        .next()
        .expect("one entry in, one slot out")
        .expect("a permitted record with no metadata persists");
    provider.force_flush().unwrap();

    assert_eq!(
        histogram_count(&exporter, "uc_record_metadata_bytes"),
        0,
        "an empty-metadata record must record nothing on uc_record_metadata_bytes",
    );
}

// ── Feed gateway ────────────────────────────────────────────────────────────
//
// `Service::read_usage_feed` is instrumented on two planes and both are pinned
// here. Its own DESIGN §3.11.5 instruments (`uc_feed_requests_total`,
// `uc_feed_page_duration_seconds`, `uc_feed_page_entries`) are the obvious ones.
// The shared foundation plane is the one a task-scoped diff cannot see: the feed
// goes through `resolve_plugin_for` and `instrument_spi` like every other SPI
// dispatch, so a permitted read contributes a
// `uc_plugin_call_duration_seconds{operation="read_feed_page"}` sample and an
// unready binding an `unready` accept error under the same operation label.

/// A subscription over the sample meter, and the two cursor-free arguments
/// every feed test below passes.
fn feed_subscription() -> usage_collector_sdk::FeedSubscription {
    usage_collector_sdk::FeedSubscription::new([meter_gts()])
        .expect("a one-type subscription is non-empty")
}

fn feed_page(
    entries: usize,
) -> usage_collector_sdk::FeedPage<
    usage_collector_sdk::FeedPosition,
    usage_collector_sdk::StoredUsageRecord,
> {
    usage_collector_sdk::FeedPage {
        entries: (0..entries)
            .map(|_| crate::domain::test_support::as_stored(sample_record()))
            .collect(),
        next: Some(
            usage_collector_sdk::FeedPosition::new(vec![7]).expect("a non-empty position is valid"),
        ),
    }
}

#[tokio::test]
async fn feed_success_records_its_own_instruments_and_the_shared_plugin_call() {
    let plugin = HappyPathPlugin::new();
    plugin.set_read_feed_page(feed_page(3));

    let (service, provider, exporter) = ServiceFixture::default()
        .with_source(fake_declaration_source_with_fold("SUM"))
        .with_resolver(tenant_scoped_permit())
        .build_with_metrics(plugin, "test.metrics.feed.ok.v1");

    service
        .read_usage_feed(
            &authenticated_ctx(),
            &feed_subscription(),
            usage_collector_sdk::FeedStart::Oldest,
            None,
            None,
        )
        .await
        .expect("a permitted feed read succeeds");
    provider.force_flush().unwrap();

    assert_eq!(
        counter_sum_with_label(&exporter, "uc_feed_requests_total", "outcome", "success"),
        1,
    );
    assert_eq!(
        counter_sum_with_label(
            &exporter,
            "uc_feed_requests_total",
            "error_category",
            "none"
        ),
        1,
    );
    assert_eq!(
        histogram_count(&exporter, "uc_feed_page_duration_seconds"),
        1,
    );
    // Observed once, carrying the served page size — the entries histogram
    // is what tells a caught-up consumer (empty pages) from a backlog.
    assert_eq!(histogram_count(&exporter, "uc_feed_page_entries"), 1);
    assert!(
        (histogram_sum(&exporter, "uc_feed_page_entries") - 3.0).abs() < f64::EPSILON,
        "the page-entries observation must carry the served page size",
    );
    // The shared plane: the feed dispatched through `instrument_spi` under
    // its own SPI operation label, exactly as every other read path does.
    assert_eq!(
        histogram_count_with_label(
            &exporter,
            "uc_plugin_call_duration_seconds",
            "operation",
            "read_feed_page",
        ),
        1,
        "the feed SPI dispatch MUST contribute exactly one duration sample",
    );
    assert_eq!(gauge_last(&exporter, "uc_plugin_ready"), Some(1));
}

#[tokio::test]
async fn feed_deny_records_denied_authz_and_observes_no_page_entries() {
    let plugin = HappyPathPlugin::new();
    let (service, provider, exporter) = ServiceFixture::default()
        .with_resolver(Arc::new(DenyAllResolver))
        .build_with_metrics(plugin, "test.metrics.feed.deny.v1");

    let outcome = service
        .read_usage_feed(
            &authenticated_ctx(),
            &feed_subscription(),
            usage_collector_sdk::FeedStart::Oldest,
            None,
            None,
        )
        .await;
    assert!(outcome.is_err());
    provider.force_flush().unwrap();

    assert_eq!(
        counter_sum_with_label(&exporter, "uc_feed_requests_total", "outcome", "denied"),
        1,
    );
    assert_eq!(
        counter_sum_with_label(
            &exporter,
            "uc_feed_requests_total",
            "error_category",
            "authz"
        ),
        1,
    );
    // A deny is still a completion, so the duration is sampled — but no page
    // was served, and a `0` observation here would be indistinguishable from
    // the empty page a caught-up live reader legitimately receives.
    assert_eq!(
        histogram_count(&exporter, "uc_feed_page_duration_seconds"),
        1,
    );
    assert_eq!(histogram_count(&exporter, "uc_feed_page_entries"), 0);
    // The PDP plane carries the feed's own operation label, not the raw
    // path's: a dashboard that could not separate them would report the
    // charging feed's denial rate as the audit path's.
    assert_eq!(
        counter_sum_with_label(
            &exporter,
            "uc_authz_decisions_total",
            "operation",
            "read_feed"
        ),
        1,
    );
    assert_eq!(
        counter_sum_with_label(&exporter, "uc_authz_decisions_total", "decision", "deny"),
        1,
    );
}

/// A feed cursor bound to [`feed_subscription`], for the **supplied-cursor**
/// half of the retention refusal. Not interchangeable with `FeedStart::Oldest`:
/// DESIGN section 3.2 admits the refusal for a caller-supplied cursor only, so
/// which of the two a retention test passes decides which series it asserts
/// about.
fn feed_resume_cursor() -> toolkit_odata::CursorV1 {
    crate::domain::feed::mint_cursor(
        &usage_collector_sdk::FeedPosition::new(vec![4]).expect("a non-empty position is valid"),
        &feed_subscription(),
    )
}

#[tokio::test]
async fn feed_retention_refusal_is_a_caller_outcome_not_a_plugin_fault() {
    let plugin = HappyPathPlugin::new();
    plugin.set_read_feed_page_err(UsageCollectorPluginError::CursorBeyondRetention);

    let (service, provider, exporter) = ServiceFixture::default()
        .with_source(fake_declaration_source_with_fold("SUM"))
        .with_resolver(tenant_scoped_permit())
        .build_with_metrics(plugin, "test.metrics.feed.retention.v1");

    // A **supplied** cursor: the only start for which this refusal is a caller
    // outcome at all — see the sibling test below for the `Oldest` half.
    let resume = feed_resume_cursor();
    let outcome = service
        .read_usage_feed(
            &authenticated_ctx(),
            &feed_subscription(),
            usage_collector_sdk::FeedStart::After(&resume),
            None,
            None,
        )
        .await;
    assert!(outcome.is_err());
    provider.force_flush().unwrap();

    // §3.11's one feed alert reads exactly this series.
    assert_eq!(
        counter_sum_with_label(
            &exporter,
            "uc_feed_requests_total",
            "error_category",
            "cursor_beyond_retention",
        ),
        1,
    );
    assert_eq!(
        counter_sum_with_label(&exporter, "uc_feed_requests_total", "outcome", "error"),
        1,
    );
    // An error completion is still a dispatch completion.
    assert_eq!(
        histogram_count_with_label(
            &exporter,
            "uc_plugin_call_duration_seconds",
            "operation",
            "read_feed_page",
        ),
        1,
    );
    // And it is NOT a backend fault: `backend_error_category` admits only
    // `Transient` / `Internal`, so a replay refusal must leave the plugin's
    // accept-error counter alone. Counting it there would page an operator
    // about a store that is behaving exactly as documented.
    assert_eq!(
        counter_sum_with_label(
            &exporter,
            "uc_plugin_accept_errors_total",
            "operation",
            "read_feed_page",
        ),
        0,
    );
}

/// Fails if a retention refusal answered to a request that carried **no**
/// cursor fires DESIGN section 3.11.6's one feed alert.
///
/// That alert reads
/// `rate(uc_feed_requests_total{error_category="cursor_beyond_retention"})`
/// and means "a consumer is falling behind the retention floor". DESIGN section
/// 3.2 fixes that the refusal reads a caller-supplied cursor only, so a plugin
/// answering a `FeedStart::Oldest` request with one is in breach of the host
/// contract, and counting it there pages an operator about a consumer that is
/// not behind at all.
///
/// Both halves are asserted and neither implies the other: the category
/// assertion alone passes under a gateway that swallowed the refusal and served
/// an empty page; the error assertion alone under one that still counted it as a
/// replay refusal.
#[tokio::test]
async fn a_retention_refusal_on_an_oldest_start_is_a_plugin_breach_not_a_replay_refusal() {
    let plugin = HappyPathPlugin::new();
    plugin.set_read_feed_page_err(UsageCollectorPluginError::CursorBeyondRetention);

    let (service, provider, exporter) = ServiceFixture::default()
        .with_source(fake_declaration_source_with_fold("SUM"))
        .with_resolver(tenant_scoped_permit())
        .build_with_metrics(plugin, "test.metrics.feed.oldeststart.v1");

    let outcome = service
        .read_usage_feed(
            &authenticated_ctx(),
            &feed_subscription(),
            usage_collector_sdk::FeedStart::Oldest,
            None,
            None,
        )
        .await;

    match outcome {
        Err(UsageCollectorError::Internal { detail }) => {
            assert!(
                detail.contains("CURSOR_BEYOND_RETENTION"),
                "the internal error must name the breach it lifts, got: {detail}"
            );
        }
        other => panic!("expected Internal naming the plugin-contract breach, got {other:?}"),
    }

    provider.force_flush().unwrap();

    assert_eq!(
        counter_sum_with_label(
            &exporter,
            "uc_feed_requests_total",
            "error_category",
            "cursor_beyond_retention",
        ),
        0,
        "a cursor-free request cannot be a replay refusal; section 3.11.6's feed alert must \
         not fire on a plugin-contract breach",
    );
    assert_eq!(
        counter_sum_with_label(
            &exporter,
            "uc_feed_requests_total",
            "error_category",
            "plugin_error",
        ),
        1,
        "the breach is counted under the residual category, which is what it is",
    );
}

/// Fails if the feed resolves its plugin handle outside `resolve_plugin_for`.
///
/// A read path that reached `get_plugin` directly, or held its own handle,
/// answers the same error to the caller and emits nothing here.
#[tokio::test]
async fn feed_unready_plugin_increments_the_unready_counter_under_its_own_op() {
    let (service, provider, exporter) = service_with_metrics_unready_plugin(
        "test.metrics.feed.unready.v1",
        CountingPermitResolver::new(
            pep_properties::OWNER_TENANT_ID,
            Uuid::from_u128(2).to_string(),
        ),
    );

    let outcome = service
        .read_usage_feed(
            &authenticated_ctx(),
            &feed_subscription(),
            usage_collector_sdk::FeedStart::Oldest,
            None,
            None,
        )
        .await;
    assert!(outcome.is_err());
    provider.force_flush().unwrap();

    assert_eq!(
        counter_sum_with_label(
            &exporter,
            "uc_plugin_accept_errors_total",
            "operation",
            "read_feed_page",
        ),
        1,
    );
    assert_eq!(
        counter_sum_with_label(
            &exporter,
            "uc_plugin_accept_errors_total",
            "error_category",
            "unready",
        ),
        1,
    );
    assert_eq!(gauge_last(&exporter, "uc_plugin_ready"), Some(0));
    // No SPI invocation occurred, so no duration sample is recorded.
    assert_eq!(
        histogram_count(&exporter, "uc_plugin_call_duration_seconds"),
        0,
    );
    // The feed's own counter still completes the request, under the
    // category a readiness failure belongs to.
    assert_eq!(
        counter_sum_with_label(
            &exporter,
            "uc_feed_requests_total",
            "error_category",
            "plugin_error",
        ),
        1,
    );
}

/// `(input result, expected (outcome, error_category))` row for the feed
/// classifier table.
type FeedClassifierCase = (
    Result<(), UsageCollectorError>,
    (RequestOutcome, FeedErrorCategory),
);

#[test]
fn classify_feed_result_maps_each_arm() {
    let cases: Vec<FeedClassifierCase> = vec![
        (Ok(()), (RequestOutcome::Success, FeedErrorCategory::None)),
        (
            Err(UsageCollectorError::permission_denied("pdp")),
            (RequestOutcome::Denied, FeedErrorCategory::Authz),
        ),
        // Both halves of the decode path raise `CursorRejected` and both land
        // here — `CursorDecode` is one category for the lot, so the `until` case
        // is pinned anyway as the one a reader would expect to differ.
        (
            Err(UsageCollectorError::inadmissible_cursor_keyset("garbage")),
            (RequestOutcome::Error, FeedErrorCategory::CursorDecode),
        ),
        (
            Err(UsageCollectorError::inadmissible_until_keyset("garbage")),
            (RequestOutcome::Error, FeedErrorCategory::CursorDecode),
        ),
        // The replay refusal, discriminated by the typed reason the lift
        // attaches — not by the variant, which the limit case below shares.
        (
            Err(UsageCollectorError::from(
                crate::domain::error::DomainError::CursorBeyondRetention,
            )),
            (
                RequestOutcome::Error,
                FeedErrorCategory::CursorBeyondRetention,
            ),
        ),
        // A page size outside the published bound is a caller-surface rejection,
        // carried by `FeedErrorCategory::ArgumentRejected`. See
        // `an_argument_rejection_from_behind_the_service_is_not_a_plugin_error`
        // for the properties this row has no room to state.
        (
            Err(crate::domain::feed::resolve_limit(Some(0))
                .expect_err("zero is outside the published bound")),
            (RequestOutcome::Error, FeedErrorCategory::ArgumentRejected),
        ),
        // The other half of the same population: a subscription over
        // `MAX_SUBSCRIPTION_TYPES` is the second caller of the same
        // `feed::invalid_argument` helper with the same
        // `ValidationReason::Validation`.
        (
            Err(crate::domain::feed::require_subscription_breadth(
                &usage_collector_sdk::FeedSubscription::new(
                    (0..=crate::domain::feed::MAX_SUBSCRIPTION_TYPES).map(|i| {
                        MeterTypeId::new(format!(
                            "gts.cf.core.uc.usage_record.v1~cf.uc_wide._.meter{i}.v1~"
                        ))
                        .expect("valid meter id")
                    }),
                )
                .expect("a hundred-and-one-type subscription is non-empty"),
            )
            .expect_err("one more than MAX_SUBSCRIPTION_TYPES is outside the published bound")),
            (RequestOutcome::Error, FeedErrorCategory::ArgumentRejected),
        ),
        (
            Err(UsageCollectorError::internal("boom")),
            (RequestOutcome::Error, FeedErrorCategory::PluginError),
        ),
        // A PDP outage folds here too, and that is ruled rather than a residue
        // nobody named. A transport failure and a plugin fault both surface as
        // `ServiceUnavailable` at this seam, and `Authz` above is reserved for a
        // *completed* decision, so an unreachable `authz-resolver` is
        // indistinguishable here from a broken store. DESIGN §3.11.5 already
        // declares `uc_pdp_failures_total` with its own §3.11.6 alert row for
        // this cause, so a seventh value here would duplicate an instrument that
        // exists. If a later slice adds one anyway, this row is what it changes.
        (
            Err(UsageCollectorError::service_unavailable("down", None)),
            (RequestOutcome::Error, FeedErrorCategory::PluginError),
        ),
    ];
    for (result, expected) in cases {
        assert_eq!(
            classify_feed_result(&result),
            expected,
            "misclassified {result:?}",
        );
    }
}

#[test]
fn an_argument_rejection_from_behind_the_service_is_not_a_plugin_error() {
    // A `limit` outside 1..=1000 arriving from an in-process SDK caller never
    // reaches the REST edge's `parse_limit`, so it is an argument rejection with
    // no plugin involved.
    //
    // The oracle discriminates on the NEIGHBOUR value, not on deletion: every
    // refusal on this surface produces an `Err`, so asserting "not None" would
    // pass under the old `plugin_error` folding too.
    let err = crate::domain::feed::resolve_limit(Some(0))
        .expect_err("zero is outside the published bound");
    let (outcome, category) = classify_feed_result::<()>(&Err(err));
    assert_eq!(outcome, RequestOutcome::Error);
    assert_ne!(
        category,
        FeedErrorCategory::PluginError,
        "a caller-surface argument rejection must not be counted as a plugin fault",
    );
    assert_eq!(category, FeedErrorCategory::ArgumentRejected);
}

// ── Reconciliation gateway ──────────────────────────────────────────────────
//
// `Service::get_reconciliation_metadata` has no query-gateway instrument of its
// own beyond `uc_query_requests_total` (see `QueryKind::Reconciliation`'s own
// doc — it is the one `QueryKind` `uc_query_duration_seconds` does not admit).
// Its shared-plane contribution is therefore the whole of what a task-scoped
// diff cannot see: a permitted read dispatches through `resolve_plugin_for` /
// `instrument_spi`, contributing a `uc_plugin_call_duration_seconds` sample, and
// authorizes through the shared PDP wrapper, contributing a
// `uc_authz_decisions_total{operation="reconciliation", decision="permit"}` one.

#[tokio::test]
async fn reconciliation_success_records_the_shared_authz_and_plugin_instruments() {
    let plugin = RecordingReconciliationPlugin::answering_for(AggregationFold::Sum);
    let (service, provider, exporter) = ServiceFixture::default()
        .with_source(fake_declaration_source_with_fold("SUM"))
        .with_resolver(tenant_scoped_permit())
        .build_with_metrics(plugin, "test.metrics.reconciliation.ok.v1");

    service
        .get_reconciliation_metadata(
            &authenticated_ctx(),
            Uuid::from_u128(2),
            meter_gts(),
            test_time_range(),
        )
        .await
        .expect("a permitted reconciliation read over a resolvable meter succeeds");
    provider.force_flush().unwrap();

    // The shared plugin-host plane: the reconciliation SPI dispatch went through
    // `instrument_spi` under its own SPI operation label.
    assert_eq!(
        histogram_count_with_label(
            &exporter,
            "uc_plugin_call_duration_seconds",
            "operation",
            "get_reconciliation_metadata",
        ),
        1,
        "the reconciliation SPI dispatch MUST contribute exactly one duration sample",
    );
    // The shared PDP-helper plane: the reconciliation read's own `operation`
    // label, permitted.
    assert_eq!(
        counter_sum_with_label(
            &exporter,
            "uc_authz_decisions_total",
            "operation",
            "reconciliation",
        ),
        1,
    );
    assert_eq!(
        counter_sum_with_label(&exporter, "uc_authz_decisions_total", "decision", "permit"),
        1,
    );
}

// ── Ingestion gateway: the per-subject ingestion quota (DESIGN §3.2) ────────
//
// One token bucket per calling subject, charged the submitted entry count after
// the entry cap and before the PDP call, on the live and backfill routes alike.
// The tests below pin the charge order, the shared bucket, the counter's
// magnitude, and the per-record counter it must leave alone.

/// A metrics-instrumented [`Service`] with a chosen entry cap and a chosen
/// per-subject allowance, positionally
/// `(max_batch_records, burst_entries, sustained_entries_per_sec)`.
///
/// `sustained_entries_per_sec = 0` is the "no refill" setting every test below
/// uses: a bucket that never refills makes the allowance a fixed budget, so an
/// assertion about what a submission *spent* cannot be confounded by elapsed
/// wall-clock time between two `await`s. It is below what
/// `UsageCollectorConfig::validate` admits, which is why it is reachable only
/// through this fixture and never from a deployment.
///
/// The plugin handle comes back with the service because [`HappyPathPlugin`]'s
/// programmed outcomes are `take()`n, one per SPI call
/// (`UsageCollectorPluginError` is deliberately `!Clone`). A test driving more
/// than one *admitted* submission re-programs between them through
/// [`program_echo`]; a submission the quota refuses never reaches the SPI.
fn service_with_cap_and_quota(
    max_batch_records: usize,
    burst_entries: u64,
    sustained_entries_per_sec: u64,
    suffix: &str,
) -> (
    Arc<HappyPathPlugin>,
    Arc<Service>,
    SdkMeterProvider,
    InMemoryMetricExporter,
) {
    let plugin = HappyPathPlugin::new();
    let (service, provider, exporter) = ServiceFixture::default()
        .with_max_batch_records(max_batch_records)
        .with_source(fake_declaration_source_with_fold("SUM"))
        .with_ingestion_quota(IngestionQuotaConfig {
            sustained_entries_per_sec,
            burst_entries,
            idle_eviction_secs: 900,
        })
        .build_with_metrics(
            Arc::clone(&plugin) as Arc<dyn usage_collector_sdk::UsageCollectorPluginV1>,
            suffix,
        );
    (plugin, service, provider, exporter)
}

/// Program `n` `Ok` echoes for the next batch SPI call.
fn program_echo(plugin: &HappyPathPlugin, n: usize) {
    plugin.set_create_records((0..n).map(|_| Ok(sample_record())).collect());
}

/// `n` distinct create submissions.
///
/// Distinct `idempotency_key`s, not `vec![record(); n]`: identical submissions
/// derive one dedup identity and collapse intra-batch, so a test that means `n`
/// entries has to send `n` *different* ones or it measures dedup.
fn quota_batch(n: usize) -> Vec<CreateUsageRecord> {
    (0..n)
        .map(|i| {
            let mut record = sample_create_record();
            record.idempotency_key =
                Some(IdempotencyKey::new(format!("idem-quota-{i}")).expect("valid key"));
            record
        })
        .collect()
}

/// Asserts `err` is the quota rejection's **detail**, not merely its variant:
/// the 429 carries the allowance it was measured against, the count that was
/// submitted, and a retry delay. A variant assertion would not discriminate —
/// the structural cap rejection and the quota rejection are both whole-request
/// refusals on the same route, and what tells them apart is what each one says.
fn assert_quota_rejection(err: &UsageCollectorError, allowance: u64, submitted: usize) {
    let UsageCollectorError::ResourceExhausted {
        retry_after_seconds,
        detail,
    } = err
    else {
        panic!("expected a ResourceExhausted quota rejection, got {err:?}");
    };
    assert!(
        detail.contains(&allowance.to_string()) && detail.contains(&submitted.to_string()),
        "the rejection must name the allowance ({allowance}) and the submitted count \
         ({submitted}) so an operator can tell a burst from a sustained overrun; got {detail:?}",
    );
    assert!(
        *retry_after_seconds > 0,
        "a refused submission must name a delay after which one of its size fits; got 0",
    );
}

#[tokio::test]
async fn an_over_cap_submission_is_rejected_on_the_cap_and_costs_no_tokens() {
    // DESIGN §3.2 makes this order load-bearing: the cap bounds the cost at
    // `max_batch_records`, which is what keeps the retry delay computable and
    // the `burst_entries >= max_batch_records` startup rule coherent.
    //
    // The counter assertion below is the second half of the oracle rather than a
    // second red: it rules out a charge that happened and was admitted, which no
    // `expect` can see.
    let (plugin, service, provider, exporter) =
        service_with_cap_and_quota(2, 5, 0, "test.metrics.quota.caporder.v1");
    let ctx = authenticated_ctx();

    let err = service
        .create_usage_records(&ctx, quota_batch(5))
        .await
        .expect_err("5 entries over a cap of 2 is rejected whole");
    assert_invalid_batch_size(&err, 5, 1, 2);

    // The whole allowance must still be there: 5 tokens against a bucket that
    // never refills, so a submission of 2 is admissible only if the cap
    // rejection above spent nothing.
    program_echo(&plugin, 2);
    service
        .create_usage_records(&ctx, quota_batch(2))
        .await
        .expect("the cap rejection spent no tokens, so a 2-entry batch still fits");
    provider.force_flush().unwrap();

    // Derived from what the recorder emitted, not from the absence of an error:
    // a charge that happened and was admitted would leave this at 0 too.
    assert_eq!(
        counter_sum(&exporter, "uc_ingestion_quota_rejections_total"),
        0,
        "neither submission was throttled, so the quota counter must not have moved",
    );
}

#[tokio::test]
async fn backfill_and_live_draw_the_same_bucket_in_both_directions() {
    // DESIGN §3.2: "Backfill draws the same allowance as live emission. Workload
    // isolation, not a separate budget, is what distinguishes the routes."
    //
    // Both directions in one test, because a one-direction test passes against a
    // build where backfill shares live's bucket but live does not share
    // backfill's. `assert_quota_rejection` asserts the rejection's shape rather
    // than the call merely having failed, because an un-programmed plugin fails
    // the batch for an unrelated reason either way.
    let ctx = authenticated_ctx();

    // live exhausts → backfill throttled
    let (plugin, service, _provider, _exporter) =
        service_with_cap_and_quota(10, 10, 0, "test.metrics.quota.sharedlive.v1");
    program_echo(&plugin, 10);
    service
        .create_usage_records(&ctx, quota_batch(10))
        .await
        .expect("10 entries against an allowance of 10 is admitted");
    let err = service
        .backfill_usage_records(&ctx, quota_batch(1))
        .await
        .expect_err("backfill draws the same exhausted bucket");
    assert_quota_rejection(&err, 10, 1);

    // backfill exhausts → live throttled
    let (plugin2, service2, _provider2, _exporter2) =
        service_with_cap_and_quota(10, 10, 0, "test.metrics.quota.sharedback.v1");
    program_echo(&plugin2, 10);
    service2
        .backfill_usage_records(&ctx, quota_batch(10))
        .await
        .expect("10 imported entries against an allowance of 10 is admitted");
    let err = service2
        .create_usage_records(&ctx, quota_batch(1))
        .await
        .expect_err("live draws the same exhausted bucket");
    assert_quota_rejection(&err, 10, 1);
}

#[tokio::test]
async fn the_quota_counter_moves_by_the_entry_count_not_by_one() {
    // DESIGN §3.11.5 (`:2243`): "Incremented by the submitted entry count when a
    // submission is rejected for exceeding its allowance. This carries throttled
    // volume." An off-by-N here is invisible to a test that asserts only
    // "incremented".
    let (plugin, service, provider, exporter) =
        service_with_cap_and_quota(100, 1, 0, "test.metrics.quota.magnitude.v1");
    let ctx = authenticated_ctx();
    program_echo(&plugin, 1);
    service
        .create_usage_records(&ctx, quota_batch(1))
        .await
        .expect("the one token the bucket holds admits a one-entry batch");

    let err = service
        .create_usage_records(&ctx, quota_batch(7))
        .await
        .expect_err("7 entries against an exhausted bucket is throttled");
    assert_quota_rejection(&err, 1, 7);
    provider.force_flush().unwrap();

    assert_eq!(
        counter_sum(&exporter, "uc_ingestion_quota_rejections_total"),
        7,
        "the counter carries throttled volume, so it moves by the submitted entry count",
    );
}

#[tokio::test]
async fn a_quota_rejection_does_not_touch_the_per_record_counter() {
    // DESIGN §3.11.5 (`:2239`): "A quota rejection does not increment this
    // counter: the charge happens before `entry_type` is validated, so the label
    // would be unknown." Derived from what the recorder emits, not from that
    // sentence: the assertion reads the exporter back.
    //
    // One, not three: `counter_points` counts DATA POINTS, and three entries
    // sharing one `outcome`/`entry_type`/`origin`/`error_category` tuple are one
    // point carrying a value of 3. That is the right oracle anyway — the claim is
    // that this instrument is untouched, and a single point is already a breach.
    let (_plugin, service, provider, exporter) =
        service_with_cap_and_quota(100, 0, 0, "test.metrics.quota.norecords.v1");

    let err = service
        .create_usage_records(&authenticated_ctx(), quota_batch(3))
        .await
        .expect_err("an allowance of 0 throttles every submission");
    assert_quota_rejection(&err, 0, 3);
    provider.force_flush().unwrap();

    assert_eq!(
        counter_points(&exporter, "uc_ingestion_records_total"),
        0,
        "uc_ingestion_records_total must not move on a quota rejection: entry_type is \
         not yet validated at the charge point, so its required label is unknown",
    );
    // The companion positive, so the absence above cannot be satisfied by a
    // service that simply did nothing.
    assert_eq!(
        counter_sum(&exporter, "uc_ingestion_quota_rejections_total"),
        3,
    );
}

#[tokio::test]
async fn the_cap_and_the_quota_rejections_are_told_apart_by_their_error_category() {
    // Both are whole-request 4xx refusals on the same route, raised before any
    // entry is validated. The error variants differ, but an operator reading a
    // dashboard does not see variants — both land on
    // `uc_ingestion_requests_total{outcome="rejected"}`, and `error_category` is
    // the only thing that discriminates them.
    let (_plugin, service, provider, exporter) =
        service_with_cap_and_quota(2, 1, 0, "test.metrics.quota.vscap.v1");
    let ctx = authenticated_ctx();

    // Over the cap of 2 → a structural `validation` rejection.
    let cap_err = service
        .create_usage_records(&ctx, quota_batch(3))
        .await
        .expect_err("3 entries over a cap of 2");
    assert_invalid_batch_size(&cap_err, 3, 1, 2);

    // Within the cap, over the allowance of 1 → a `quota` rejection.
    let quota_err = service
        .create_usage_records(&ctx, quota_batch(2))
        .await
        .expect_err("2 entries against an allowance of 1");
    assert_quota_rejection(&quota_err, 1, 2);

    // The fixture's two bounds must differ, or "within the cap and over the
    // allowance" names no submission at all and this test proves nothing.
    assert_ne!(
        service.max_batch_records(),
        1,
        "this test's oracle needs the entry cap and the allowance to be different \
         numbers: the quota arm submits a batch the cap admits and the bucket refuses",
    );
    provider.force_flush().unwrap();

    assert_eq!(
        counter_sum_with_label(
            &exporter,
            "uc_ingestion_requests_total",
            "error_category",
            "validation",
        ),
        1,
        "the cap rejection and only the cap rejection is `validation`",
    );
    assert_eq!(
        counter_sum_with_label(
            &exporter,
            "uc_ingestion_requests_total",
            "error_category",
            "quota",
        ),
        1,
        "the quota rejection and only the quota rejection is `quota`",
    );
    assert_eq!(
        counter_sum_with_label(
            &exporter,
            "uc_ingestion_requests_total",
            "outcome",
            "rejected",
        ),
        2,
        "both refusals are whole-request rejections on the same counter, which is \
         exactly why the error_category has to carry the difference",
    );
}

#[tokio::test]
async fn every_anonymous_caller_draws_the_one_nil_subject_bucket() {
    // `SecurityContext::anonymous()` sets `subject_id` to the nil UUID
    // (`toolkit-security/src/context.rs`), so the quota's key for every
    // anonymous caller is the same value and they share one bucket. This gear
    // ACCEPTS that rather than refusing an anonymous context before the charge:
    // the shared bucket is the conservative reading — anonymous traffic is
    // bounded in aggregate at one subject's allowance, where any per-caller
    // keying of an unauthenticated identity would hand out a fresh allowance per
    // caller. The REST routes are `.authenticated()` and the in-process path's
    // own authorization is the PDP's, downstream of the charge, so nothing here
    // is the gear's only gate on an anonymous caller.
    let (plugin, service, _provider, _exporter) =
        service_with_cap_and_quota(10, 1, 0, "test.metrics.quota.anon.v1");

    let first = SecurityContext::anonymous();
    let second = SecurityContext::anonymous();
    assert_eq!(
        first.subject_id(),
        second.subject_id(),
        "the shared-bucket claim rests on two separately built anonymous contexts \
         carrying the same subject id; if that stopped being true, re-derive this \
         decision rather than deleting the test",
    );
    assert_ne!(
        first.subject_id(),
        authenticated_ctx().subject_id(),
        "and on the anonymous key being a different key from an authenticated \
         subject's, or the test below would be measuring one subject twice",
    );

    program_echo(&plugin, 1);
    service
        .create_usage_records(&first, vec![sample_create_record()])
        .await
        .expect("the first anonymous submission is not refused request-wide")
        .into_iter()
        .next()
        .expect("one entry in, one slot out")
        .expect("the first anonymous submission takes the one token");
    // Request-wide: the quota refuses before any entry is dispatched, so the
    // rejection is the outer `Err` and never a per-record slot.
    let err = service
        .create_usage_records(&second, vec![sample_create_record()])
        .await
        .expect_err("a second anonymous caller draws the first one's bucket");
    assert_quota_rejection(&err, 1, 1);

    // An authenticated subject is unaffected: a separate key, a separate
    // bucket, so the aggregate anonymous bound is not a global one.
    program_echo(&plugin, 1);
    service
        .create_usage_records(&authenticated_ctx(), vec![sample_create_record()])
        .await
        .expect("an authenticated subject's submission is not refused request-wide")
        .into_iter()
        .next()
        .expect("one entry in, one slot out")
        .expect("an authenticated subject holds its own allowance");
}

#[tokio::test]
async fn the_buckets_gauge_counts_one_bucket_per_charging_subject() {
    // DESIGN §3.11.5 (`:2273`): "Live quota buckets, one per recently-active
    // calling subject. Watches map growth and confirms idle eviction is keeping
    // up." Sampled on every charge, not only on a rejection: every charge below
    // is admitted, so a rejection-only gauge would never be written at all.
    let (plugin, service, provider, exporter) =
        service_with_cap_and_quota(10, 10, 0, "test.metrics.quota.gauge.v1");

    program_echo(&plugin, 1);
    service
        .create_usage_records(&authenticated_ctx(), vec![sample_create_record()])
        .await
        .expect("not refused request-wide")
        .into_iter()
        .next()
        .expect("one entry in, one slot out")
        .expect("admitted");
    program_echo(&plugin, 1);
    service
        .create_usage_records(&SecurityContext::anonymous(), vec![sample_create_record()])
        .await
        .expect("not refused request-wide")
        .into_iter()
        .next()
        .expect("one entry in, one slot out")
        .expect("admitted");
    provider.force_flush().unwrap();

    assert_eq!(
        gauge_last(&exporter, "uc_ingestion_quota_buckets_active"),
        Some(2),
        "two distinct charging subjects leave two resident buckets",
    );
}

// ── §8.2: the label vocabulary pin, derived rather than restated ───────────

/// Every [`IngestRequestErrorCategory`] this gear can record.
///
/// The wildcard-free `match` in [`declared_by_section_3_11_5`] below forces a
/// visit here when a variant is added: it stops compiling until the new variant
/// is given an arm, in the one function that also has to answer whether
/// §3.11.5's row declares it.
const ALL_INGEST_REQUEST_ERROR_CATEGORIES: [IngestRequestErrorCategory; 5] = [
    IngestRequestErrorCategory::None,
    IngestRequestErrorCategory::MissingSecurityContext,
    IngestRequestErrorCategory::PluginError,
    IngestRequestErrorCategory::Validation,
    IngestRequestErrorCategory::Quota,
];

/// `uc_ingestion_requests_total`'s `error_category` vocabulary exactly as DESIGN
/// §3.11.5 publishes it, including the values this gear has no path to emit
/// yet.
///
/// Transcribed from the document because it is what the *emitted* set is
/// compared against. The comparison is a subset check in that direction only: a
/// value the gear emits that the row does not declare is a published-contract
/// breach, while a declared value the gear cannot yet reach is unimplemented
/// coverage, not a defect.
const SECTION_3_11_5_INGEST_REQUEST_ERROR_CATEGORIES: [&str; 8] = [
    "none",
    "missing_security_context",
    "authz",
    "unresolved_type",
    "validation",
    "metadata_size",
    "quota",
    "plugin_error",
];

/// Whether §3.11.5's row declares `category`'s label value.
///
/// Exhaustive and wildcard-free on purpose — see
/// [`ALL_INGEST_REQUEST_ERROR_CATEGORIES`].
const fn declared_by_section_3_11_5(category: IngestRequestErrorCategory) -> bool {
    match category {
        IngestRequestErrorCategory::None
        | IngestRequestErrorCategory::MissingSecurityContext
        | IngestRequestErrorCategory::PluginError
        | IngestRequestErrorCategory::Validation
        | IngestRequestErrorCategory::Quota => true,
    }
}

#[tokio::test]
async fn the_ingestion_request_counter_emits_only_labels_section_3_11_5_declares() {
    // Derived from what the recorder EMITS, not restated from §3.11.5: a
    // restated pin would pass while a forbidden label shipped.
    //
    // Each arm separately falsifiable:
    //
    //  1. every emitted `error_category` value is one §3.11.5 declares;
    //  2. `quota` is emitted here;
    //  3. `validation` is emitted here too, and is NOT a nowhere-else claim.
    //
    // The asymmetry in arm 3 is the point: §3.11.5 gives `validation` to this
    // counter (`:2238`) AND to `uc_ingestion_records_total` (`:2239`), while
    // `quota` belongs to this counter alone. A pin treating the two values the
    // same way is wrong about one of them, and the nowhere-else half of that is
    // pinned separately below.
    let (metrics, provider, exporter) = local_metrics();
    for category in ALL_INGEST_REQUEST_ERROR_CATEGORIES {
        assert!(
            declared_by_section_3_11_5(category),
            "this gear records an error_category DESIGN §3.11.5's row does not \
             declare: {category:?}",
        );
        metrics.record_ingestion_request(IngestRequestOutcome::Rejected, category);
    }
    provider.force_flush().unwrap();

    let emitted = counter_label_pairs(&exporter, "uc_ingestion_requests_total");
    let emitted_categories: std::collections::BTreeSet<&str> = emitted
        .iter()
        .filter(|(key, _)| key == "error_category")
        .map(|(_, value)| value.as_str())
        .collect();

    assert!(
        !emitted_categories.is_empty(),
        "the recorder emitted no error_category label at all; this pin would then \
         be vacuous",
    );
    for value in &emitted_categories {
        assert!(
            SECTION_3_11_5_INGEST_REQUEST_ERROR_CATEGORIES.contains(value),
            "`{value}` is emitted on uc_ingestion_requests_total but DESIGN §3.11.5's \
             row does not declare it; the label vocabulary is part of the published \
             contract",
        );
    }
    assert!(
        emitted_categories.contains("quota"),
        "the quota rejection's label value must actually reach the counter; emitted: \
         {emitted_categories:?}",
    );
    assert!(
        emitted_categories.contains("validation"),
        "the structural cap rejection's label value must too (ruling G7); emitted: \
         {emitted_categories:?}",
    );
    // No label key beyond the two §3.11.5 gives this counter.
    let emitted_keys: std::collections::BTreeSet<&str> =
        emitted.iter().map(|(key, _)| key.as_str()).collect();
    assert_eq!(
        emitted_keys,
        ["error_category", "outcome"].into_iter().collect(),
        "uc_ingestion_requests_total carries `outcome` and `error_category` and \
         nothing else",
    );
}

#[tokio::test]
async fn quota_is_a_label_value_on_the_request_counter_and_nowhere_else() {
    // The nowhere-else half, and the half `validation` deliberately does NOT
    // get: §3.11.5 declares `validation` on both the request counter (`:2238`)
    // and the per-record counter (`:2239`), so a symmetric pin would be
    // asserting something the document contradicts.
    //
    // Derived from emissions: drive EVERY label-bearing ingestion instrument
    // through every value of its own vocabulary, then look for `quota` anywhere
    // other than the request counter.
    let (metrics, provider, exporter) = local_metrics();
    for category in ALL_INGEST_REQUEST_ERROR_CATEGORIES {
        metrics.record_ingestion_request(IngestRequestOutcome::Rejected, category);
    }
    for category in [
        RecordErrorCategory::None,
        RecordErrorCategory::Authz,
        RecordErrorCategory::UnknownUsageType,
        RecordErrorCategory::SemanticsViolation,
        RecordErrorCategory::InvalidationRule,
        RecordErrorCategory::MetadataSize,
        RecordErrorCategory::IdempotencyConflict,
        RecordErrorCategory::PluginError,
    ] {
        metrics.record_ingestion_record(
            RecordOutcome::Rejected,
            EntryType::Record,
            RecordOrigin::Live,
            category,
        );
    }
    metrics.record_quota_rejection(3);
    metrics.set_quota_buckets_active(1);
    provider.force_flush().unwrap();

    assert!(
        counter_label_pairs(&exporter, "uc_ingestion_requests_total")
            .contains(&("error_category".to_owned(), "quota".to_owned())),
        "the positive half: `quota` must be on the request counter, or the \
         nowhere-else half below is vacuously true",
    );
    for instrument in [
        "uc_ingestion_records_total",
        "uc_ingestion_quota_rejections_total",
    ] {
        let offending: Vec<_> = counter_label_pairs(&exporter, instrument)
            .into_iter()
            .filter(|(_, value)| value == "quota")
            .collect();
        assert!(
            offending.is_empty(),
            "`quota` is declared on uc_ingestion_requests_total alone, but \
             {instrument} emitted {offending:?}",
        );
    }
    // `validation` gets the positive arm and no nowhere-else arm, by design.
    assert!(
        counter_label_pairs(&exporter, "uc_ingestion_requests_total")
            .contains(&("error_category".to_owned(), "validation".to_owned())),
        "`validation` is declared on this counter too (ruling G7)",
    );
}

#[tokio::test]
async fn neither_new_quota_instrument_emits_any_label() {
    // DESIGN §3.11.5 gives `uc_ingestion_quota_rejections_total` (`:2243`) and
    // `uc_ingestion_quota_buckets_active` (`:2273`) a label column of `—`: they
    // are unlabelled, there being deliberately no per-tenant tier to carry even
    // in aggregate (§3.2). This is the test that catches an adapter acting on a
    // narrative gloss rather than on the rows.
    let (metrics, provider, exporter) = local_metrics();
    metrics.record_quota_rejection(7);
    metrics.set_quota_buckets_active(4);
    provider.force_flush().unwrap();

    // The positives first, so "no labels" cannot be satisfied by an
    // instrument that was never written to at all.
    assert_eq!(
        counter_sum(&exporter, "uc_ingestion_quota_rejections_total"),
        7,
    );
    assert_eq!(
        gauge_last(&exporter, "uc_ingestion_quota_buckets_active"),
        Some(4),
    );
    assert!(
        counter_label_pairs(&exporter, "uc_ingestion_quota_rejections_total").is_empty(),
        "uc_ingestion_quota_rejections_total is unlabelled (DESIGN.md:2243); emitted {:?}",
        counter_label_pairs(&exporter, "uc_ingestion_quota_rejections_total"),
    );
    assert!(
        gauge_label_pairs(&exporter, "uc_ingestion_quota_buckets_active").is_empty(),
        "uc_ingestion_quota_buckets_active is unlabelled (DESIGN.md:2273); emitted {:?}",
        gauge_label_pairs(&exporter, "uc_ingestion_quota_buckets_active"),
    );
}

// ── §10.1: the two absence pins ────────────────────────────────────────────

/// Every production `.rs` file under the crate's `src/`, as
/// `(repo-relative path, contents)`.
///
/// Derived by walking the tree and reading, not by naming files: a pin that
/// globs a path stops covering the code the moment the code moves. Test modules
/// (`*_tests.rs`, `test_support.rs`) are excluded because the absences below are
/// claims about what the gear *ships*.
///
/// Fails loudly and names the file on any I/O error, rather than returning a
/// short list that would make an absence assertion pass by accident.
fn production_sources() -> Vec<(String, String)> {
    fn walk(dir: &std::path::Path, out: &mut Vec<(String, String)>) {
        let entries = std::fs::read_dir(dir)
            .unwrap_or_else(|e| panic!("cannot read directory {}: {e}", dir.display()));
        for entry in entries {
            let entry =
                entry.unwrap_or_else(|e| panic!("cannot read an entry of {}: {e}", dir.display()));
            let path = entry.path();
            if path.is_dir() {
                walk(&path, out);
                continue;
            }
            if path.extension().is_none_or(|ext| ext != "rs") {
                continue;
            }
            let name = path
                .file_name()
                .expect("a file has a name")
                .to_string_lossy()
                .into_owned();
            if name.ends_with("_tests.rs") || name == "test_support.rs" {
                continue;
            }
            let text = std::fs::read_to_string(&path)
                .unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()));
            out.push((path.display().to_string(), text));
        }
    }

    let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut out = Vec::new();
    walk(&src, &mut out);
    assert!(
        !out.is_empty(),
        "no production source found under {}; an absence pin over an empty corpus \
         asserts nothing",
        src.display(),
    );
    out
}

#[test]
fn there_is_exactly_one_quota_bucket_and_it_is_keyed_on_the_subject_alone() {
    // DESIGN §3.2: "Every submission is charged against one token bucket, keyed
    // on `subject_id` from the ingestion security context", and "Backfill draws
    // the same allowance as live emission. Workload isolation, not a separate
    // budget, is what distinguishes the routes."
    //
    // Derived by content: the corpus is every production file that mentions the
    // quota type, found by reading, and the bucket module is the file that
    // declares it — neither is named by path.
    let sources = production_sources();

    let mentioning: Vec<&(String, String)> = sources
        .iter()
        .filter(|(_, text)| text.contains("IngestionQuota"))
        .collect();
    assert!(
        !mentioning.is_empty(),
        "no production source mentions IngestionQuota; this pin would be vacuous",
    );

    // Arm A — one construction, for the whole service.
    let constructions: Vec<(&str, usize)> = mentioning
        .iter()
        .map(|(path, text)| (path.as_str(), text.matches("IngestionQuota::new(").count()))
        .filter(|(_, count)| *count > 0)
        .collect();
    let total: usize = constructions.iter().map(|(_, count)| *count).sum();
    assert_eq!(
        total, 1,
        "exactly one set of quota buckets exists for the whole service; found {total} \
         construction site(s): {constructions:?}",
    );

    // Arms B and C — the bucket module itself, located by what it declares.
    let (bucket_module, bucket_text) = sources
        .iter()
        .find(|(_, text)| text.contains("pub struct IngestionQuota {"))
        .expect("a production source declares `pub struct IngestionQuota {`");
    // Code lines only. A doc comment that writes `HashMap<..>` as prose, or
    // that uses the word "origin" in a sentence, is not a second bucket — the
    // claim is about what the module declares, so the scan reads declarations.
    let code: Vec<(usize, &str)> = bucket_text
        .lines()
        .enumerate()
        .map(|(index, line)| (index + 1, line.trim()))
        .filter(|(_, line)| !line.starts_with("//"))
        .collect();

    let maps: Vec<(usize, &str)> = code
        .iter()
        .filter(|(_, line)| line.contains("HashMap<"))
        .copied()
        .collect();
    assert!(
        !maps.is_empty(),
        "{bucket_module} declares no keyed map at all; this pin would be vacuous",
    );
    for (line_number, line) in &maps {
        let at = line.find("HashMap<").expect("the filter found it");
        assert!(
            line[at..].starts_with("HashMap<Uuid,"),
            "{bucket_module}:{line_number} keys a quota map on something other than the \
             subject id: `{line}`",
        );
    }
    for token in ["origin", "route", "RecordOrigin", "backfill"] {
        let hits: Vec<(usize, &str)> = code
            .iter()
            .filter(|(_, line)| line.to_lowercase().contains(&token.to_lowercase()))
            .copied()
            .collect();
        assert!(
            hits.is_empty(),
            "{bucket_module} names `{token}` in code, so the bucket can be told which \
             route charged it; G1 requires one bucket that cannot: {hits:?}",
        );
    }
}

#[test]
fn neither_new_quota_instrument_is_declared_with_a_subject_or_tenant_label() {
    // §3.11.5's cardinality rule, which names this quota as its own worked
    // example: "because `subject_id` and `tenant_id` cannot be labels, per-caller
    // quota utilisation cannot be exposed as a metric".
    //
    // The emission-side twin is `neither_new_quota_instrument_emits_any_label`.
    // This one is the source-side pin §10.1 owes, because an exporter read can
    // only observe the label sets the call sites it drove happened to produce,
    // while a label added on a *different* call site would ship unobserved. The
    // adapter is located by content, not by path.
    let sources = production_sources();
    let (adapter_path, adapter) = sources
        .iter()
        .find(|(_, text)| text.contains("impl UsageCollectorMetrics for UcMetricsMeter"))
        .expect("a production source implements UsageCollectorMetrics for UcMetricsMeter");

    for method in ["fn record_quota_rejection", "fn set_quota_buckets_active"] {
        let start = adapter.find(method).unwrap_or_else(|| {
            panic!("{adapter_path} does not implement `{method}`; this pin would be vacuous")
        });
        // The method body runs to the next `fn ` at the same nesting, which
        // is enough of a bound for a token scan and needs no parser.
        let rest = &adapter[start + method.len()..];
        let body = &rest[..rest.find("\n    fn ").unwrap_or(rest.len())];
        for token in ["KeyValue", "subject", "tenant", "key::", "tier"] {
            assert!(
                !body.contains(token),
                "{adapter_path}'s `{method}` names `{token}`; DESIGN §3.11.5 gives both \
                 quota instruments a label column of `—`, and its cardinality rule \
                 forbids subject_id / tenant_id as labels outright",
            );
        }
        assert!(
            body.contains("&[]"),
            "{adapter_path}'s `{method}` must record against an empty attribute slice; \
             if the call shape changed, re-derive this pin rather than deleting it",
        );
    }
}

/// §3.11.5's "Label cardinality" paragraph, verbatim: *"`tenant_id`,
/// `resource_id`, `subject_id`, `gts_type_id`, `request_id`, `trace_id`,
/// idempotency keys — **must not** be used as metric labels."*
///
/// Kept local rather than shared with `infra::metrics_inventory_tests`'s
/// identical `FORBIDDEN_LABEL_KEYS`: two independent transcriptions of one
/// document paragraph fail *independently*, where a single shared constant would
/// let one transcription error silence both halves of the cardinality pin at
/// once. Nothing pins the two lists against each other, so a divergence between
/// them would go unnoticed — the trade this duplication accepts.
const FORBIDDEN_LABEL_KEYS: &[&str] = &[
    "tenant_id",
    "resource_id",
    "subject_id",
    "gts_type_id",
    "request_id",
    "trace_id",
    "idempotency_key",
];

#[test]
fn no_metrics_recorder_method_is_passed_an_unbounded_identifier_as_a_label() {
    // The source-side half of the §3.11.5 cardinality pin, widened from the
    // quota-only sibling above to every `UsageCollectorMetrics` method the
    // adapter implements. The emission-side twin
    // (`no_emitted_instrument_carries_an_unbounded_identifier_as_a_label` in
    // `infra/metrics_inventory_tests.rs`) can only see the label sets its driving
    // set's call sites happened to produce; this one reads every recorder body.
    //
    // Both sides are case-folded, so a body naming the forbidden identifier only
    // via a SCREAMING_SNAKE constant (`key::TENANT_ID`) is still caught.
    let sources = production_sources();
    let (adapter_path, adapter) = sources
        .iter()
        .find(|(_, text)| text.contains("impl UsageCollectorMetrics for UcMetricsMeter"))
        .expect("a production source implements UsageCollectorMetrics for UcMetricsMeter");

    let methods = [
        "fn set_pdp_ready",
        "fn record_pdp_decision",
        "fn record_pdp_failure",
        "fn set_plugin_ready",
        "fn record_plugin_call",
        "fn record_plugin_accept_error",
        "fn observe_ingestion_batch_size",
        "fn observe_ingestion_duration",
        "fn observe_record_metadata_bytes",
        "fn record_ingestion_record",
        "fn record_ingestion_request",
        "fn record_quota_rejection",
        "fn set_quota_buckets_active",
        "fn query_inflight_inc",
        "fn query_inflight_dec",
        "fn observe_query_result_rows",
        "fn record_query_request",
        "fn record_feed_request",
        "fn observe_feed_page_entries",
        "fn record_type_resolution",
        "fn set_resolved_types",
        "fn set_declaration_cache_age_seconds",
        "fn observe_type_resolution_duration",
        "fn record_declaration_mirror_write_failure",
    ];

    // Derived from the adapter's own text, not hardcoded: counts `\n    fn `
    // inside the bounded `impl UsageCollectorMetrics for UcMetricsMeter`
    // block, so a trait method added without updating `methods` above reds
    // here instead of silently narrowing what the scan below covers.
    let impl_start = adapter
        .find("impl UsageCollectorMetrics for UcMetricsMeter")
        .expect("located by the `find` above");
    let impl_rest = &adapter[impl_start..];
    let impl_body = &impl_rest[..impl_rest.find("\n}").map_or(impl_rest.len(), |i| i + 1)];
    let method_count = impl_body.matches("\n    fn ").count();
    assert_eq!(
        methods.len(),
        method_count,
        "`impl UsageCollectorMetrics for UcMetricsMeter` declares {method_count} methods \
         (counted from its own text); the token-scan list above names {} — keep the two \
         in sync, or a new method's forbidden label goes unscanned",
        methods.len(),
    );

    for method in methods {
        let start = adapter.find(method).unwrap_or_else(|| {
            panic!("{adapter_path} does not implement `{method}`; this pin would be vacuous")
        });
        let rest = &adapter[start + method.len()..];
        // The method body runs to the next `fn `, OR — for the LAST method in
        // the impl, which has no next `fn ` to find — to the impl block's own
        // closing brace. Without the fallback, the last method's bound ran to
        // end-of-file, folding every later item into its "body".
        let body_end = rest
            .find("\n    fn ")
            .or_else(|| rest.find("\n}"))
            .unwrap_or(rest.len());
        let body = &rest[..body_end];
        let body_lower = body.to_lowercase();
        for token in FORBIDDEN_LABEL_KEYS {
            assert!(
                !body_lower.contains(token),
                "{adapter_path}'s `{method}` names `{token}`; DESIGN §3.11.5's \
                 cardinality rule forbids it as a metric label outright",
            );
        }
    }
}

#[test]
fn no_label_key_constant_in_ports_metrics_declares_an_unbounded_identifier() {
    // The load-bearing half of the source-side pin: complete over every declared
    // label-key constant regardless of which recorder uses it and regardless of
    // the identifier's name at the call site. A per-method token scan — even
    // case-folded — only catches a forbidden identifier spelled out inside the
    // method body; `key::TENANT_ID` defeats that. Reading the constant's own
    // VALUE in `ports::metrics::key` sidesteps naming entirely.
    let sources = production_sources();
    let (ports_path, ports_text) = sources
        .iter()
        .find(|(_, text)| text.contains("pub mod key {"))
        .expect("a production source declares `pub mod key {`");

    let start = ports_text
        .find("pub mod key {")
        .expect("found by the search above");
    let rest = &ports_text[start..];
    let key_module = &rest[..rest.find("\n}").map_or(rest.len(), |i| i + 1)];

    let mut constants_checked = 0usize;
    for line in key_module.lines() {
        let Some(eq) = line.find("= \"") else {
            continue;
        };
        let value_start = eq + 3;
        let Some(value_len) = line[value_start..].find('"') else {
            continue;
        };
        let value = &line[value_start..value_start + value_len];
        constants_checked += 1;
        assert!(
            !FORBIDDEN_LABEL_KEYS.contains(&value),
            "{ports_path}'s `key` module declares a label-key constant whose value is \
             `{value}`; DESIGN §3.11.5's cardinality rule forbids it as a metric label \
             outright, regardless of which recorder uses it or what the constant is named",
        );
    }
    assert!(
        constants_checked > 0,
        "{ports_path}'s `key` module declares no string constants at all; this pin would \
         be vacuous",
    );
}

/// The REST seam in miniature: a gateway that decoded 5 entries, kept the one
/// that survived, and tells the service both numbers. Returned as a pair so each
/// test below names the two counts side by side and an `assert_ne!` can guard
/// that they differ — the whole oracle of both tests is that the gear reads the
/// larger one.
fn one_decoded_of_five_submitted() -> (Vec<CreateUsageRecord>, usize) {
    (quota_batch(1), 5)
}

/// The zero-decoded sibling of [`one_decoded_of_five_submitted`]: a submission
/// whose every entry failed the gateway's own wire decode, so nothing survives
/// to reach the pipeline, yet the submission still arrived and must still be
/// charged (DESIGN §3.2). This is the shape
/// `create_usage_records_with_submitted_count`'s own doc names: "`records` may
/// be **empty** while `submitted_before_decode` is not".
fn all_malformed_of_five_submitted() -> (Vec<CreateUsageRecord>, usize) {
    (Vec::new(), 5)
}

#[tokio::test]
async fn the_cap_judges_the_submitted_count_not_the_decoded_one() {
    // The first of the two gates. The cap and the quota sit one after the other
    // on the same request, and DESIGN §3.2 makes that order load-bearing
    // precisely because the cap is what bounds the quota's cost — a cap reading
    // one count and a quota reading another breaks the relationship silently.
    // This test and its sibling below pin that they read the same number, driven
    // through `create_usage_records_with_submitted_count`, the seam the REST
    // gateway uses: the entries that decoded (one) alongside the count that
    // arrived (five).
    let (decoded, submitted) = one_decoded_of_five_submitted();
    assert_ne!(
        decoded.len(),
        submitted,
        "the oracle is that the gear was handed {} decoded entries and told \
         {submitted} were submitted; equal counts would prove nothing",
        decoded.len(),
    );

    let (_plugin, service, _provider, _exporter) =
        service_with_cap_and_quota(4, 1_000, 0, "test.metrics.quota.gatescap.v1");
    let err = service
        .create_usage_records_with_submitted_count(&authenticated_ctx(), decoded, submitted)
        .await
        .expect_err("5 entries submitted over a cap of 4 is rejected whole");
    // The cap's own detail must name 5, not 1: a cap reporting the decoded
    // count would tell the caller to shrink a batch it did not send.
    assert_invalid_batch_size(&err, 5, 1, 4);
}

#[tokio::test]
async fn the_quota_judges_the_submitted_count_not_the_decoded_one() {
    // The second gate. DESIGN §3.2: "the flood being bounded is submitted
    // volume, **and a caller cannot escape the charge by submitting entries that
    // fail later**." A wire-decode failure is the earliest kind of "fails later",
    // so the count the charge reads has to be the one that arrived.
    //
    // Same submission as the test above, now inside the cap (10) and over the
    // allowance (4): admitted if the charge reads 1, refused if it reads 5. Which
    // is why `assert_quota_rejection` asserts the rejection's shape and its
    // submitted count rather than the call merely having failed.
    let (decoded, submitted) = one_decoded_of_five_submitted();
    assert_ne!(
        decoded.len(),
        submitted,
        "the oracle is that the gear was handed {} decoded entries and told \
         {submitted} were submitted; equal counts would prove nothing",
        decoded.len(),
    );

    let (_plugin, service, _provider, _exporter) =
        service_with_cap_and_quota(10, 4, 0, "test.metrics.quota.gatesquota.v1");
    let err = service
        .create_usage_records_with_submitted_count(&authenticated_ctx(), decoded, submitted)
        .await
        .expect_err("5 entries submitted against an allowance of 4 is rejected whole");
    assert_quota_rejection(&err, 4, 5);
}

#[tokio::test]
async fn an_in_process_batch_caller_submits_exactly_what_it_dispatches() {
    // The companion to the test above, and the reason the submitted count is a
    // parameter rather than a second clock on the handler: an in-process caller
    // has no decode step between itself and the service, so what it passes IS
    // what it submitted. `create_usage_records` therefore derives the count from
    // the vector it was given, and the shared body's own `debug_assert!` — not
    // the assertions below — is what catches the two seams drifting.
    let (plugin, service, _provider, _exporter) =
        service_with_cap_and_quota(10, 3, 0, "test.metrics.quota.inprocess.v1");
    let ctx = authenticated_ctx();

    program_echo(&plugin, 3);
    service
        .create_usage_records(&ctx, quota_batch(3))
        .await
        .expect("3 entries against an allowance of 3 is admitted");
    let err = service
        .create_usage_records(&ctx, quota_batch(1))
        .await
        .expect_err("the first submission spent all three tokens");
    assert_quota_rejection(&err, 3, 1);
}

#[tokio::test]
async fn an_all_malformed_submission_records_request_scoped_telemetry_but_not_per_entry_outcomes() {
    // An **admitted** all-malformed submission — one whose every entry failed
    // the gateway's own wire decode, inside the cap and inside the quota —
    // reaches `create_usage_records_for_origin` and is charged, so it must still
    // report rather than returning silently through `records.is_empty()`.
    //
    // The split is three-way, not a blanket "record everything":
    // `uc_ingestion_requests_total` (request-wide, no `entry_type`) and
    // `uc_ingestion_batch_size` (samples the submitted count, unlabelled) both
    // can and must fire; `uc_ingestion_records_total` must NOT, because DESIGN
    // §3.11.5 scopes it to decoded entries and this submission has none. A test
    // asserting only the positive half would pass against an implementation that
    // counted the malformed entries anyway.
    let (decoded, submitted) = all_malformed_of_five_submitted();
    assert!(
        decoded.is_empty(),
        "the oracle is that nothing decoded at all; a non-empty fixture would \
         prove nothing about this seam",
    );

    let (_plugin, service, provider, exporter) =
        service_with_cap_and_quota(10, 10, 0, "test.metrics.ingest.allmalformed.v1");
    let per_record = service
        .create_usage_records_with_submitted_count(&authenticated_ctx(), decoded, submitted)
        .await
        .expect("5 submitted, 0 decoded, inside the cap and the quota: admitted");
    assert!(
        per_record.is_empty(),
        "no entry decoded, so there is nothing for the pipeline to return"
    );
    provider.force_flush().unwrap();

    // Positive half: the request-wide and batch-wide instruments fire. The
    // request reads `partial` — DESIGN's own vocabulary for
    // `IngestRequestOutcome::Partial` is "at least one per-record rejection
    // (HTTP 207)", and every one of the 5 submitted entries was rejected, just
    // before the service ever saw it.
    assert_eq!(
        counter_sum_with_label(
            &exporter,
            "uc_ingestion_requests_total",
            "outcome",
            "partial"
        ),
        1,
        "an admitted all-malformed submission still completes a request",
    );
    assert_eq!(
        counter_sum_with_label(
            &exporter,
            "uc_ingestion_requests_total",
            "error_category",
            "none"
        ),
        1,
        "accepted/partial carries error_category=none, same as every other non-whole-rejection",
    );
    assert_eq!(
        histogram_count(&exporter, "uc_ingestion_batch_size"),
        1,
        "the histogram samples the submitted count, which was 5, not the decoded one",
    );
    assert_eq!(
        histogram_count(&exporter, "uc_ingestion_duration_seconds"),
        1,
        "the submission cost tokens and ran the cap/quota checks, so it has a duration",
    );

    // Negative half: the per-entry counter is scoped to decoded entries and
    // must stay silent, because this submission decoded none.
    assert_eq!(
        counter_sum(&exporter, "uc_ingestion_records_total"),
        0,
        "no entry decoded, and DESIGN \u{a7}3.11.5 scopes this counter to \
         decoded entries",
    );
}

// ── The DESIGN §3.11.5 inventory pin's driving set ──────────────────────────
//
// Lives here, beside the fixtures it drives, rather than in `test_support.rs`:
// every fixture it uses is private to this module, so only the two public entry
// points below need promoting. A driver one module away from its fixtures has to
// see them, which inverts the module dependency — editing a leaf fixture would
// then change the meaning of an `infra` pin two layers away.
//
// `drive_every_operation_class` (names) and
// `drive_every_operation_class_label_triples` (label triples) both read off
// `drive_every_operation_class_exporters`'s exporters, so the driving calls are
// written exactly once. See `infra::metrics_inventory`'s module doc for what
// each test built on these two functions is pinning.
//
// Several DESIGN §3.11.5 instruments are unreachable from the nominal operation
// classes under a single happy-path `Service`, so each of the scenarios below
// builds a `Service` of its own and pushes its own exporter:
//
// - Reconciliation: `HappyPathPlugin::get_reconciliation_metadata` has no
//   programmable success response, so reaching the shared PDP / plugin-host
//   plane for it needs its own `Service` over `RecordingReconciliationPlugin`.
// - `uc_plugin_accept_errors_total` needs a structurally unready plugin binding
//   (`service_with_metrics_unready_plugin`); every call in the main driving set
//   is programmed to succeed.
// - `uc_pdp_failures_total` needs a PDP transport failure
//   (`UnreachableResolver`); every other driver uses a resolver that decides.
// - `uc_ingestion_quota_rejections_total` needs a submission that exceeds its
//   bucket's allowance, hence its own quota-constrained `Service`.
// - `uc_record_metadata_bytes` is observed only when the submitted record
//   carries metadata the resolved declaration declares, hence its own `Service`
//   over `fake_declaration_source_with_metadata`.
// - `uc_declaration_mirror_write_failures_total` needs a resolver whose
//   declaration mirror refuses every write (`ServiceFixture::with_failing_mirror`);
//   every other driver builds through `TypeResolver::new`'s no-op default mirror.
async fn drive_every_operation_class_exporters() -> Vec<InMemoryMetricExporter> {
    let mut exporters = Vec::new();

    // The DESIGN §3.11.5 operation classes, over one metrics-wired
    // `Service` and one `HappyPathPlugin`, so the shared PDP / plugin-host
    // plane (`uc_authz_decisions_total`, `uc_pdp_duration_seconds`,
    // `uc_plugin_call_duration_seconds`, `uc_plugin_ready`,
    // `uc_ingestion_quota_buckets_active`) is reached by every one of them.
    let plugin = HappyPathPlugin::new();
    let (service, provider, exporter) = ServiceFixture::default()
        .with_source(fake_declaration_source_with_fold("SUM"))
        .with_resolver(tenant_scoped_permit())
        .build_with_metrics(
            Arc::clone(&plugin) as Arc<dyn usage_collector_sdk::UsageCollectorPluginV1>,
            "test.metrics.inventory.pin.v1",
        );

    // One ingestion call: a second would drive the same instruments twice.
    program_echo(&plugin, 1);
    let _outcome = service
        .create_usage_records(&authenticated_ctx(), vec![sample_create_record()])
        .await;

    plugin.set_list_usage_records_response(record_page(1));
    let _outcome = service
        .list_usage_records(
            &authenticated_ctx(),
            meter_gts(),
            test_time_range(),
            &ODataQuery::default(),
            &[],
        )
        .await;

    plugin.set_query_aggregated_usage_records_response(single_bucket_aggregation());
    let _outcome = service
        .query_aggregated_usage_records(
            &authenticated_ctx(),
            meter_gts(),
            test_time_range(),
            &ODataQuery::default(),
            &[],
            &[],
        )
        .await;

    plugin.set_get_record(sample_record());
    let _outcome = service
        .get_usage_record(&authenticated_ctx(), Uuid::from_u128(0x1234))
        .await;

    plugin.set_read_feed_page(feed_page(1));
    let _outcome = service
        .read_usage_feed(
            &authenticated_ctx(),
            &feed_subscription(),
            usage_collector_sdk::FeedStart::Oldest,
            None,
            None,
        )
        .await;

    provider.force_flush().expect("force_flush");
    exporters.push(exporter);

    // Reconciliation: `HappyPathPlugin::get_reconciliation_metadata` has no
    // programmable success response, so this drives its own `Service` over
    // `RecordingReconciliationPlugin`.
    let recon_plugin = RecordingReconciliationPlugin::answering_for(AggregationFold::Sum);
    let (recon_service, recon_provider, recon_exporter) = ServiceFixture::default()
        .with_source(fake_declaration_source_with_fold("SUM"))
        .with_resolver(tenant_scoped_permit())
        .build_with_metrics(recon_plugin, "test.metrics.inventory.reconciliation.v1");
    let _outcome = recon_service
        .get_reconciliation_metadata(
            &authenticated_ctx(),
            Uuid::from_u128(2),
            meter_gts(),
            test_time_range(),
        )
        .await;
    recon_provider.force_flush().expect("force_flush");
    exporters.push(recon_exporter);

    // `uc_plugin_accept_errors_total` / the unready half of `uc_plugin_ready`.
    let (unready_service, unready_provider, unready_exporter) = service_with_metrics_unready_plugin(
        "test.metrics.inventory.unready.v1",
        CountingTenantPermitResolver::new(),
    );
    let _outcome = unready_service
        .create_usage_records(&authenticated_ctx(), vec![sample_create_record()])
        .await;
    unready_provider.force_flush().expect("force_flush");
    exporters.push(unready_exporter);

    // `uc_pdp_failures_total`.
    let (pdp_service, pdp_provider, pdp_exporter) = ServiceFixture::default()
        .with_resolver(Arc::new(UnreachableResolver))
        .build_with_metrics(HappyPathPlugin::new(), "test.metrics.inventory.pdp.v1");
    let _outcome = pdp_service
        .create_usage_records(&authenticated_ctx(), vec![sample_create_record()])
        .await;
    pdp_provider.force_flush().expect("force_flush");
    exporters.push(pdp_exporter);

    // `uc_ingestion_quota_rejections_total`: a one-token bucket admits the
    // first one-entry submission and throttles the second.
    let (quota_plugin, quota_service, quota_provider, quota_exporter) =
        service_with_cap_and_quota(10, 1, 0, "test.metrics.inventory.quota.v1");
    program_echo(&quota_plugin, 1);
    let _outcome = quota_service
        .create_usage_records(&authenticated_ctx(), vec![sample_create_record()])
        .await;
    let _outcome = quota_service
        .create_usage_records(&authenticated_ctx(), vec![sample_create_record()])
        .await;
    quota_provider.force_flush().expect("force_flush");
    exporters.push(quota_exporter);

    // `uc_record_metadata_bytes`: observed only when the submitted record carries
    // metadata the resolved declaration declares, which the main driving set's
    // source does not — hence `fake_declaration_source_with_metadata`.
    let metadata_plugin = HappyPathPlugin::new();
    let (metadata_service, metadata_provider, metadata_exporter) = ServiceFixture::default()
        .with_source(fake_declaration_source_with_metadata(&["region"]))
        .with_resolver(tenant_scoped_permit())
        .build_with_metrics(
            Arc::clone(&metadata_plugin) as Arc<dyn usage_collector_sdk::UsageCollectorPluginV1>,
            "test.metrics.inventory.metadata.v1",
        );
    program_echo(&metadata_plugin, 1);
    let mut record_with_metadata = sample_create_record();
    record_with_metadata.metadata.insert(
        MetadataKey::new("region").expect("valid metadata key"),
        "us-east-1".to_owned(),
    );
    let _outcome = metadata_service
        .create_usage_records(&authenticated_ctx(), vec![record_with_metadata])
        .await;
    metadata_provider.force_flush().expect("force_flush");
    exporters.push(metadata_exporter);

    // `uc_declaration_mirror_write_failures_total`: a resolver whose mirror
    // refuses every write. The declaration still resolves and the record is
    // still accepted (ADR-0015 statement 4), so this counter is the only
    // observable difference — which is also why no other scenario reaches it.
    let mirror_plugin = HappyPathPlugin::new();
    let (mirror_service, mirror_provider, mirror_exporter) = ServiceFixture::default()
        .with_source(fake_declaration_source_with_fold("SUM"))
        .with_resolver(tenant_scoped_permit())
        .with_failing_mirror()
        .build_with_metrics(
            Arc::clone(&mirror_plugin) as Arc<dyn usage_collector_sdk::UsageCollectorPluginV1>,
            "test.metrics.inventory.mirror.v1",
        );
    program_echo(&mirror_plugin, 1);
    let _outcome = mirror_service
        .create_usage_records(&authenticated_ctx(), vec![sample_create_record()])
        .await;
    mirror_provider.force_flush().expect("force_flush");
    exporters.push(mirror_exporter);

    exporters
}

/// Drive one of every DESIGN §3.11.5 operation class (plus the extensions
/// [`drive_every_operation_class_exporters`] documents) and return every
/// instrument name exported across all of it.
///
/// No other helper answers "what did the whole gear emit" — only "what did this
/// one instrument do". `pub` (not `pub(crate)` — clippy's `redundant_pub_crate`,
/// since the enclosing module is already only `pub(crate)`) so
/// `infra::metrics_inventory_tests` can reach it.
pub async fn drive_every_operation_class() -> std::collections::BTreeSet<String> {
    drive_every_operation_class_exporters()
        .await
        .iter()
        .flat_map(crate::domain::test_support::exported_instrument_names)
        .collect()
}

/// The label-triple twin of [`drive_every_operation_class`]: every
/// `(instrument, label_key, label_value)` triple the same driving set produced,
/// built on [`counter_label_pairs`] and [`gauge_label_pairs`].
///
/// **It reaches a minority of the adapter's instruments.** The rest are
/// invisible to this reader for three distinct reasons, not one: the histograms
/// (`AggregatedMetrics::F64(MetricData::Histogram)`, which neither helper matches
/// — no histogram label-pair reader exists at all); the unlabelled
/// counters/gauges (`uc_plugin_ready`, `uc_pdp_ready`,
/// `uc_ingestion_quota_buckets_active`, `uc_ingestion_quota_rejections_total`,
/// `uc_resolved_types`, `uc_declaration_cache_age_seconds`,
/// `uc_declaration_mirror_write_failures_total` — reachable in principle, but
/// recorded against `&[]`, so there is no label to surface); and **one that is
/// neither**: `uc_query_inflight` is an `i64_up_down_counter`, which exports as
/// `AggregatedMetrics::I64(MetricData::Sum)` — matched by neither
/// `counter_label_pairs` (`U64(Sum)`) nor `gauge_label_pairs` (`I64(Gauge)`).
/// That third shape is the omission worth stating explicitly: labelled,
/// non-histogram, and still falling through.
/// `infra::metrics_inventory_tests`'s module doc names the source-side scan that
/// covers what this reader cannot reach.
pub async fn drive_every_operation_class_label_triples() -> Vec<(String, String, String)> {
    let mut triples = Vec::new();
    for exporter in drive_every_operation_class_exporters().await {
        for name in crate::domain::test_support::exported_instrument_names(&exporter) {
            for (key, value) in counter_label_pairs(&exporter, &name) {
                triples.push((name.clone(), key, value));
            }
            for (key, value) in gauge_label_pairs(&exporter, &name) {
                triples.push((name.clone(), key, value));
            }
        }
    }
    triples
}

// ── The plugin-host span ────────────────────────────────────────────────────
//
// DESIGN §3.11.4: "The Plugin Host opens that span around each dispatch, so
// end-to-end traces span gateway → core → plugin → backend." `instrument_spi` is
// the sole SPI-dispatch wrapper (nine call sites, one function), so proving its
// span here proves it at every call site.
//
// The vacuity hazard in its strongest form: a span is invisible without a
// subscriber, so with none installed there is nothing a test could fail on. This
// section installs a REAL capturing subscriber via
// `tracing::subscriber::set_default` — thread-local and guard-scoped, so
// parallel `cargo test` runs do not cross-capture each other's spans — copying
// the mechanism from `gears/system/api-gateway/tests/access_log_tests.rs`. That
// file captures EVENTS via `on_event`; this section needs SPANS, so the hook is
// different — see `SpanCaptureLayer`'s doc.
//
// The second hazard: a span that exists but does not enclose the dispatch looks
// correct in a diff. "A span was created" and "the dispatch ran inside it" are
// proved by two different mechanisms below (`SpanCaptureLayer` vs
// `SpanProbePlugin`) for exactly that reason.

/// One recorded span, as [`SpanCaptureLayer::on_new_span`] saw it: its id,
/// name, and `operation` field value (`None` if the span carried no such
/// field).
type CapturedSpan = (tracing::span::Id, String, Option<String>);

/// One recorded [`SpanProbePlugin`] dispatch: the SPI method name, and the
/// [`tracing::Span::current`] id active when its body ran (`None` if no
/// span was current at all).
type ProbeMark = (&'static str, Option<tracing::span::Id>);

/// Captures every span this process's tracing backend sees created, recording
/// its name and `operation` field value (if any), keyed by
/// [`tracing::span::Id`].
///
/// Hook used: `on_new_span`, not `on_enter`/`on_exit`. `instrument_spi` gives
/// its span the `operation` value in the `info_span!(...)` call itself, so the
/// span is already fully attributed the instant `on_new_span` fires. The
/// complementary "the dispatch ran INSIDE it" half is proved by
/// [`SpanProbePlugin`] below, which reads `tracing::Span::current()` from inside
/// the plugin body — a fact no `Layer` hook observes directly, since a layer
/// learns a span's lifecycle, not what code runs while it is current.
#[derive(Clone, Default)]
struct SpanCaptureLayer {
    spans: Arc<Mutex<Vec<CapturedSpan>>>,
}

/// Reads the `operation` field off a span's `Attributes`, ignoring every
/// other field the span might carry.
struct OperationFieldVisitor(Option<String>);

impl tracing::field::Visit for OperationFieldVisitor {
    fn record_debug(&mut self, _field: &tracing::field::Field, _value: &dyn std::fmt::Debug) {}

    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        if field.name() == "operation" {
            self.0 = Some(value.to_owned());
        }
    }
}

impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for SpanCaptureLayer {
    fn on_new_span(
        &self,
        attrs: &tracing::span::Attributes<'_>,
        id: &tracing::span::Id,
        _ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        let mut visitor = OperationFieldVisitor(None);
        attrs.record(&mut visitor);
        self.spans.lock().expect("mutex").push((
            id.clone(),
            attrs.metadata().name().to_owned(),
            visitor.0,
        ));
    }
}

/// A storage-plugin decorator that records, for each SPI method it forwards to
/// `inner`, the [`tracing::Span::current`] id active at the moment its own async
/// body actually runs.
///
/// This is how [`every_spi_dispatch_runs_inside_a_host_opened_span`] tells "a
/// span exists" apart from "the dispatch ran inside it": a span entered and
/// exited *before* `instrument_spi`'s `fut.await` is ever polled creates and
/// closes the span with nothing polled while it is current, so by the time this
/// probe runs, `tracing::Span::current()` reports whatever span (if any) was
/// active beforehand — never the one it opened. No `Layer` hook sees this: it is
/// a property of the callee, readable only from the callee.
struct SpanProbePlugin {
    inner: Arc<dyn UsageCollectorPluginV1>,
    marks: Arc<Mutex<Vec<ProbeMark>>>,
}

impl SpanProbePlugin {
    fn new(inner: Arc<dyn UsageCollectorPluginV1>, marks: Arc<Mutex<Vec<ProbeMark>>>) -> Self {
        Self { inner, marks }
    }

    fn mark(&self, op: &'static str) {
        self.marks
            .lock()
            .expect("mutex")
            .push((op, tracing::Span::current().id()));
    }
}

#[async_trait]
impl UsageCollectorPluginV1 for SpanProbePlugin {
    async fn create_usage_records(
        &self,
        records: Vec<(MeterRef, StoredUsageRecord)>,
    ) -> Result<Vec<Result<StoredUsageRecord, UsageCollectorPluginError>>, UsageCollectorPluginError>
    {
        self.mark("create_usage_records");
        self.inner.create_usage_records(records).await
    }

    async fn get_usage_record(
        &self,
        id: Uuid,
        scope: &ast::Expr,
        converged_only: bool,
    ) -> Result<StoredUsageRecord, UsageCollectorPluginError> {
        self.mark("get_usage_record");
        self.inner.get_usage_record(id, scope, converged_only).await
    }

    async fn query_aggregated_usage_records(
        &self,
        meter: &MeterRef,
        time_range: usage_collector_sdk::TimeRange,
        fold: AggregationFold,
        query: &ODataQuery,
        metadata_filter: &[usage_collector_sdk::MetadataFilter],
        group_by: &[usage_collector_sdk::AggregationDimension],
    ) -> Result<AggregationResult, UsageCollectorPluginError> {
        self.mark("query_aggregated_usage_records");
        self.inner
            .query_aggregated_usage_records(
                meter,
                time_range,
                fold,
                query,
                metadata_filter,
                group_by,
            )
            .await
    }

    async fn list_usage_records(
        &self,
        meter: &MeterRef,
        time_range: usage_collector_sdk::TimeRange,
        query: &ODataQuery,
        metadata_filter: &[usage_collector_sdk::MetadataFilter],
        keyset: Option<&usage_collector_sdk::Keyset>,
    ) -> Result<RecordPage, UsageCollectorPluginError> {
        self.mark("list_usage_records");
        self.inner
            .list_usage_records(meter, time_range, query, metadata_filter, keyset)
            .await
    }

    async fn read_feed_page(
        &self,
        subscription: &[MeterRef],
        scope: &ast::Expr,
        start: usage_collector_sdk::FeedStart<usage_collector_sdk::FeedPosition>,
        until: Option<usage_collector_sdk::FeedPosition>,
        limit: u64,
    ) -> Result<
        usage_collector_sdk::FeedPage<
            usage_collector_sdk::FeedPosition,
            usage_collector_sdk::StoredUsageRecord,
        >,
        UsageCollectorPluginError,
    > {
        self.mark("read_feed_page");
        self.inner
            .read_feed_page(subscription, scope, start, until, limit)
            .await
    }

    async fn get_reconciliation_metadata(
        &self,
        tenant_id: Uuid,
        meter: &MeterRef,
        time_range: usage_collector_sdk::TimeRange,
        fold: AggregationFold,
        scope: &ast::Expr,
    ) -> Result<usage_collector_sdk::ReconciliationMetadata, UsageCollectorPluginError> {
        self.mark("get_reconciliation_metadata");
        self.inner
            .get_reconciliation_metadata(tenant_id, meter, time_range, fold, scope)
            .await
    }
}

/// Installs a **process-global**, once-per-binary `tracing` default at `TRACE`,
/// discarding everything it writes.
///
/// `tracing` caches each callsite's `Interest` **process-wide** the first time it
/// is evaluated. Hundreds of sibling tests in this file dispatch through
/// `instrument_spi` with no subscriber installed at all; whichever reaches the
/// `plugin_spi_dispatch` callsite first gets it cached as `Interest::never`, and
/// every later evaluation — including this test's own thread-local override —
/// then short-circuits *before* consulting the active dispatcher.
///
/// **The asymmetry this relies on is dispatcher *persistence*, not whether
/// installing one rebuilds the cache.** Both `tracing::subscriber::set_default`
/// and `set_global_default` build their `Dispatch` via `Dispatch::new`, which
/// unconditionally walks every registered callsite and recomputes its
/// `Interest` — installing either kind rebuilds the cache, equally. What differs
/// is how long the installed `Dispatch` stays alive, and `Interest::and` only
/// keeps a shared answer while *every currently-alive* dispatcher agrees.
/// `set_global_default` leaks its `Dispatch`, so a global sink is alive for the
/// rest of the binary and the combined answer can never again be `Never`;
/// `set_default`'s dies with its `DefaultGuard`, leaving the callsite free to be
/// re-poisoned by some later, unrelated `Dispatch::new()`. A `TRACE`-level global
/// installed once, here, closes that window for the rest of the process.
/// Precedent:
/// `gears/system/cluster/plugins/redis-cluster-plugin/tests/common/mod.rs`'s
/// `install_global_capture`.
fn ensure_plugin_spi_dispatch_callsite_is_interesting() {
    use std::sync::OnceLock;
    static INSTALLED: OnceLock<()> = OnceLock::new();
    INSTALLED.get_or_init(|| {
        let subscriber = tracing_subscriber::fmt()
            .with_writer(std::io::sink)
            .with_max_level(tracing::Level::TRACE)
            .finish();
        // Ignore an already-installed global: nothing else in this binary
        // installs one, and losing the race only means this caller's sink
        // goes unused — harmless, since its output was always discarded.
        let _installed = tracing::subscriber::set_global_default(subscriber);
    });
}

/// Every [`PluginOp::ALL`] entry's SPI dispatch creates a `plugin_spi_dispatch`
/// span carrying its `operation`, and the dispatch runs while that span is the
/// active one.
///
/// Hooks: [`SpanCaptureLayer::on_new_span`] proves the first half (the span
/// exists, named right, carrying the right `operation`); [`SpanProbePlugin`]'s
/// `tracing::Span::current()` read — no `Layer` hook — proves the second (the
/// dispatch ran *inside* it). Driven over `PluginOp::ALL` so a seventh SPI method
/// is covered here with no edit to this test, and every operation is drivable
/// from this file's existing fixtures, so the `match` below carries no skipped
/// arm.
#[tokio::test]
async fn every_spi_dispatch_runs_inside_a_host_opened_span() {
    ensure_plugin_spi_dispatch_callsite_is_interesting();

    for op in PluginOp::ALL.iter().copied() {
        let layer = SpanCaptureLayer::default();
        let spans = layer.spans.clone();
        let marks: Arc<Mutex<Vec<ProbeMark>>> = Arc::new(Mutex::new(Vec::new()));

        let subscriber = tracing_subscriber::registry().with(layer);
        let _guard = tracing::subscriber::set_default(subscriber);

        match op {
            PluginOp::CreateUsageRecords => {
                let plugin = HappyPathPlugin::new();
                plugin.set_create_records(vec![Ok(sample_record())]);
                let probe = Arc::new(SpanProbePlugin::new(plugin, marks.clone()));
                let service = ServiceFixture::default()
                    .with_resolver(CountingTenantPermitResolver::new())
                    .with_source(fake_declaration_source_with_fold("SUM"))
                    .build(probe, "test.span.create.records.v1");
                service
                    .create_usage_records(&authenticated_ctx(), vec![sample_create_record()])
                    .await
                    .expect("batch returns per-record outcomes, not an outer Err");
            }
            PluginOp::GetUsageRecord => {
                let plugin = HappyPathPlugin::new();
                plugin.set_get_record(sample_record());
                let probe = Arc::new(SpanProbePlugin::new(plugin, marks.clone()));
                let service = ServiceFixture::default()
                    .with_resolver(tenant_scoped_permit())
                    .build(probe, "test.span.get.record.v1");
                service
                    .get_usage_record(&authenticated_ctx(), Uuid::from_u128(0x1234))
                    .await
                    .expect("a permitted point lookup succeeds");
            }
            PluginOp::ListUsageRecords => {
                let plugin = HappyPathPlugin::new();
                plugin.set_list_usage_records_response(record_page(1));
                let probe = Arc::new(SpanProbePlugin::new(plugin, marks.clone()));
                let service = ServiceFixture::default()
                    .with_source(fake_declaration_source_with_fold("SUM"))
                    .with_resolver(tenant_scoped_permit())
                    .build(probe, "test.span.list.records.v1");
                service
                    .list_usage_records(
                        &authenticated_ctx(),
                        meter_gts(),
                        test_time_range(),
                        &ODataQuery::default(),
                        &[],
                    )
                    .await
                    .expect("a permitted raw query succeeds");
            }
            PluginOp::QueryAggregatedUsageRecords => {
                let plugin = HappyPathPlugin::new();
                plugin.set_query_aggregated_usage_records_response(single_bucket_aggregation());
                let probe = Arc::new(SpanProbePlugin::new(plugin, marks.clone()));
                let service = ServiceFixture::default()
                    .with_source(fake_declaration_source_with_fold("SUM"))
                    .with_resolver(tenant_scoped_permit())
                    .build(probe, "test.span.query.aggregated.v1");
                service
                    .query_aggregated_usage_records(
                        &authenticated_ctx(),
                        meter_gts(),
                        test_time_range(),
                        &ODataQuery::default(),
                        &[],
                        &[],
                    )
                    .await
                    .expect("a permitted aggregation succeeds");
            }
            PluginOp::ReadFeedPage => {
                let plugin = HappyPathPlugin::new();
                plugin.set_read_feed_page(feed_page(1));
                let probe = Arc::new(SpanProbePlugin::new(plugin, marks.clone()));
                let service = ServiceFixture::default()
                    .with_source(fake_declaration_source_with_fold("SUM"))
                    .with_resolver(tenant_scoped_permit())
                    .build(probe, "test.span.read.feed.v1");
                service
                    .read_usage_feed(
                        &authenticated_ctx(),
                        &feed_subscription(),
                        usage_collector_sdk::FeedStart::Oldest,
                        None,
                        None,
                    )
                    .await
                    .expect("a permitted feed read succeeds");
            }
            PluginOp::GetReconciliationMetadata => {
                let plugin = RecordingReconciliationPlugin::answering_for(AggregationFold::Sum);
                let probe = Arc::new(SpanProbePlugin::new(plugin, marks.clone()));
                let service = ServiceFixture::default()
                    .with_source(fake_declaration_source_with_fold("SUM"))
                    .with_resolver(tenant_scoped_permit())
                    .build(probe, "test.span.reconciliation.metadata.v1");
                service
                    .get_reconciliation_metadata(
                        &authenticated_ctx(),
                        Uuid::from_u128(2),
                        meter_gts(),
                        test_time_range(),
                    )
                    .await
                    .expect("a permitted reconciliation read succeeds");
            }
        }

        let recorded_spans = spans.lock().expect("mutex");
        let span_entry = recorded_spans
            .iter()
            .find(|(_, _, operation)| operation.as_deref() == Some(op.as_str()))
            .unwrap_or_else(|| panic!("no span carrying operation={:?} was recorded", op.as_str()));
        assert_eq!(
            span_entry.1, "plugin_spi_dispatch",
            "{op:?}: the recorded span's name",
        );

        let recorded_marks = marks.lock().expect("mutex");
        let mark_entry = recorded_marks
            .iter()
            .find(|(name, _)| *name == op.as_str())
            .unwrap_or_else(|| panic!("the plugin fake was never dispatched for {op:?}"));
        assert!(
            mark_entry.1.is_some(),
            "{op:?}: the dispatch observed NO current span -- it ran outside every span",
        );
        assert_eq!(
            mark_entry.1.as_ref(),
            Some(&span_entry.0),
            "{op:?}: the dispatch ran inside a DIFFERENT span than the one \
             instrument_spi opened for it -- the span does not enclose the \
             dispatch",
        );
    }
}

// ── The attribution-boundary fixture ────────────────────────────────────
//
// `cpt-cf-usage-collector-dod-slo-latency-attribution-boundary`'s
// Assertion: "for the same request population, the end-to-end figure and
// the dispatch-span figure are both reported, and their difference is the
// gear-attributed cost. An induced plugin slowdown moves the dispatch-span
// figure and leaves the difference stable." This is a fixture, not a load
// run: one request shape, driven twice, with the plugin's own delay as the
// only thing that changes between the two runs.
//
// The dispatch-span figure is read out of a REAL `tracing` span rather than from
// `instrument_spi`'s own `Instant::now()`/`elapsed()` pair, which wraps the
// identical `await` and so would prove nothing about the span construct this
// identifier actually names. That needs the two-part capture mechanism above:
// `ensure_plugin_spi_dispatch_callsite_is_interesting`'s process-global sink (so
// the `plugin_spi_dispatch` callsite's `Interest` is never cached `Never` by an
// earlier subscriber-less sibling test in this same binary) plus a guard-scoped
// `set_default` for this test's own capture.

/// Captures the wall-clock lifetime of every span this process's tracing backend
/// opens and closes -- `on_new_span` records the start instant (plus the span's
/// name and `operation` field, via [`OperationFieldVisitor`]); `on_close` reads
/// the elapsed time since and records the finished triple.
///
/// Where [`SpanCaptureLayer`] above proves a span was *opened* with the right
/// name and field, this one proves how long it stayed open — the "dispatch-span
/// figure" the attribution-boundary Assertion asks a test to report.
/// One still-open span tracked by [`SpanDurationLayer`]: when it was
/// created, its name, and its `operation` field value (if any).
type OpenSpan = (std::time::Instant, String, Option<String>);

/// One closed span's final record: its name, `operation` field value (if
/// any), and how long it stayed open.
type ClosedSpan = (String, Option<String>, Duration);

#[derive(Clone, Default)]
struct SpanDurationLayer {
    open: Arc<Mutex<BTreeMap<u64, OpenSpan>>>,
    closed: Arc<Mutex<Vec<ClosedSpan>>>,
}

impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for SpanDurationLayer {
    fn on_new_span(
        &self,
        attrs: &tracing::span::Attributes<'_>,
        id: &tracing::span::Id,
        _ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        let mut visitor = OperationFieldVisitor(None);
        attrs.record(&mut visitor);
        self.open.lock().expect("mutex").insert(
            id.into_u64(),
            (
                std::time::Instant::now(),
                attrs.metadata().name().to_owned(),
                visitor.0,
            ),
        );
    }

    fn on_close(&self, id: tracing::span::Id, _ctx: tracing_subscriber::layer::Context<'_, S>) {
        if let Some((start, name, operation)) =
            self.open.lock().expect("mutex").remove(&id.into_u64())
        {
            self.closed
                .lock()
                .expect("mutex")
                .push((name, operation, start.elapsed()));
        }
    }
}

impl SpanDurationLayer {
    /// The duration of the one closed span named `plugin_spi_dispatch`
    /// carrying `operation = op`. Panics if the count of such closed spans
    /// is not exactly one -- this fixture drives exactly one dispatch per
    /// run, so "exactly one" is itself part of what this helper checks.
    fn sole_dispatch_duration(&self, op: &str) -> Duration {
        let closed = self.closed.lock().expect("mutex");
        let matches: Vec<&ClosedSpan> = closed
            .iter()
            .filter(|(name, operation, _)| {
                name == "plugin_spi_dispatch" && operation.as_deref() == Some(op)
            })
            .collect();
        assert_eq!(
            matches.len(),
            1,
            "expected exactly one closed plugin_spi_dispatch span for \
             operation={op}, got {}: {:?}",
            matches.len(),
            *closed
        );
        matches[0].2
    }
}

/// A storage-plugin decorator that sleeps for `delay` before delegating
/// `create_usage_records` to `inner`, and changes nothing else -- the
/// induced plugin slowdown this fixture needs, isolated to the one SPI
/// method the fixture drives. The delay sits on the batch method because
/// that is the only persist SPI the host dispatches.
struct SlowCreatePlugin {
    inner: Arc<dyn UsageCollectorPluginV1>,
    delay: Duration,
}

impl SlowCreatePlugin {
    fn new(inner: Arc<dyn UsageCollectorPluginV1>, delay: Duration) -> Self {
        Self { inner, delay }
    }
}

#[async_trait]
impl UsageCollectorPluginV1 for SlowCreatePlugin {
    async fn create_usage_records(
        &self,
        records: Vec<(MeterRef, StoredUsageRecord)>,
    ) -> Result<Vec<Result<StoredUsageRecord, UsageCollectorPluginError>>, UsageCollectorPluginError>
    {
        tokio::time::sleep(self.delay).await;
        self.inner.create_usage_records(records).await
    }

    async fn get_usage_record(
        &self,
        id: Uuid,
        scope: &ast::Expr,
        converged_only: bool,
    ) -> Result<StoredUsageRecord, UsageCollectorPluginError> {
        self.inner.get_usage_record(id, scope, converged_only).await
    }

    async fn query_aggregated_usage_records(
        &self,
        meter: &MeterRef,
        time_range: usage_collector_sdk::TimeRange,
        fold: AggregationFold,
        query: &ODataQuery,
        metadata_filter: &[usage_collector_sdk::MetadataFilter],
        group_by: &[usage_collector_sdk::AggregationDimension],
    ) -> Result<AggregationResult, UsageCollectorPluginError> {
        self.inner
            .query_aggregated_usage_records(
                meter,
                time_range,
                fold,
                query,
                metadata_filter,
                group_by,
            )
            .await
    }

    async fn list_usage_records(
        &self,
        meter: &MeterRef,
        time_range: usage_collector_sdk::TimeRange,
        query: &ODataQuery,
        metadata_filter: &[usage_collector_sdk::MetadataFilter],
        keyset: Option<&usage_collector_sdk::Keyset>,
    ) -> Result<RecordPage, UsageCollectorPluginError> {
        self.inner
            .list_usage_records(meter, time_range, query, metadata_filter, keyset)
            .await
    }

    async fn read_feed_page(
        &self,
        subscription: &[MeterRef],
        scope: &ast::Expr,
        start: usage_collector_sdk::FeedStart<usage_collector_sdk::FeedPosition>,
        until: Option<usage_collector_sdk::FeedPosition>,
        limit: u64,
    ) -> Result<
        usage_collector_sdk::FeedPage<
            usage_collector_sdk::FeedPosition,
            usage_collector_sdk::StoredUsageRecord,
        >,
        UsageCollectorPluginError,
    > {
        self.inner
            .read_feed_page(subscription, scope, start, until, limit)
            .await
    }

    async fn get_reconciliation_metadata(
        &self,
        tenant_id: Uuid,
        meter: &MeterRef,
        time_range: usage_collector_sdk::TimeRange,
        fold: AggregationFold,
        scope: &ast::Expr,
    ) -> Result<usage_collector_sdk::ReconciliationMetadata, UsageCollectorPluginError> {
        self.inner
            .get_reconciliation_metadata(tenant_id, meter, time_range, fold, scope)
            .await
    }
}

/// Drives one `create_usage_records` dispatch through a plugin delayed by
/// `delay`, returning `(end_to_end, dispatch_span)` -- the two figures
/// `cpt-cf-usage-collector-dod-slo-latency-attribution-boundary`'s
/// Assertion requires reported together.
async fn drive_one_dispatch_with_plugin_delay(delay: Duration) -> (Duration, Duration) {
    ensure_plugin_spi_dispatch_callsite_is_interesting();

    let layer = SpanDurationLayer::default();
    let subscriber = tracing_subscriber::registry().with(layer.clone());
    let _guard = tracing::subscriber::set_default(subscriber);

    let plugin = HappyPathPlugin::new();
    plugin.set_create_records(vec![Ok(sample_record())]);
    let slow = Arc::new(SlowCreatePlugin::new(plugin, delay));
    let service = ServiceFixture::default()
        .with_resolver(CountingTenantPermitResolver::new())
        .with_source(fake_declaration_source_with_fold("SUM"))
        .build(slow, "test.span.attribution.boundary.v1");

    let start = std::time::Instant::now();
    service
        .create_usage_records(&authenticated_ctx(), vec![sample_create_record()])
        .await
        .expect("a permitted one-entry submission is not refused request-wide")
        .into_iter()
        .next()
        .expect("one entry in, one slot out")
        .expect("a permitted entry persists");
    let end_to_end = start.elapsed();

    let dispatch_span = layer.sole_dispatch_duration(PluginOp::CreateUsageRecords.as_str());
    (end_to_end, dispatch_span)
}

/// Fixture for the Assertion's two sentences together: the end-to-end and
/// dispatch-span figures for one request population (driven twice, same
/// shape, only the plugin's own delay changed), with an induced plugin
/// slowdown moving the dispatch-span figure while the difference -- the
/// gear-attributed cost -- stays stable.
// @cpt-dod:cpt-cf-usage-collector-dod-slo-latency-attribution-boundary:p1
#[tokio::test]
async fn an_induced_plugin_slowdown_moves_the_dispatch_span_figure_and_leaves_the_gear_attributed_difference_stable()
 {
    const BASELINE_DELAY: Duration = Duration::from_millis(30);
    const SLOW_DELAY: Duration = Duration::from_millis(150);

    let (baseline_e2e, baseline_span) = drive_one_dispatch_with_plugin_delay(BASELINE_DELAY).await;
    let (slow_e2e, slow_span) = drive_one_dispatch_with_plugin_delay(SLOW_DELAY).await;

    let baseline_e2e_ms = baseline_e2e.as_secs_f64() * 1000.0;
    let baseline_span_ms = baseline_span.as_secs_f64() * 1000.0;
    let slow_e2e_ms = slow_e2e.as_secs_f64() * 1000.0;
    let slow_span_ms = slow_span.as_secs_f64() * 1000.0;

    // The induced slowdown (120ms) moves the dispatch-span figure. 80ms is
    // a deliberately loose lower bound -- a third slack below the induced
    // delta -- so ordinary scheduler jitter cannot flip this assertion.
    assert!(
        slow_span_ms - baseline_span_ms >= 80.0,
        "dispatch-span figure did not move with the induced plugin \
         slowdown: baseline={baseline_span_ms}ms, slow={slow_span_ms}ms"
    );

    // The gear-attributed cost -- end-to-end minus dispatch-span -- stays
    // stable: its two measurements must land within 40ms of each other,
    // far tighter than the 120ms the span itself moved by.
    let baseline_overhead_ms = baseline_e2e_ms - baseline_span_ms;
    let slow_overhead_ms = slow_e2e_ms - slow_span_ms;
    assert!(
        (baseline_overhead_ms - slow_overhead_ms).abs() <= 40.0,
        "the end-to-end-minus-dispatch-span difference moved with the \
         plugin slowdown instead of staying stable: \
         baseline_overhead={baseline_overhead_ms}ms, slow_overhead={slow_overhead_ms}ms \
         (baseline e2e={baseline_e2e_ms}ms/span={baseline_span_ms}ms, \
         slow e2e={slow_e2e_ms}ms/span={slow_span_ms}ms)"
    );
}
