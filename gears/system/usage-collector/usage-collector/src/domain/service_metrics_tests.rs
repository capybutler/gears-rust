//! Service-level operational-metrics emission tests (Phase 1: PDP-helper +
//! plugin-host instruments).
//!
//! Each test wires a `Service` with a real `UcMetricsMeter` bound to an
//! in-memory exporter (via `test_support::service_with_metrics`), drives one
//! service method, `force_flush()`es, and asserts the exported instrument
//! series. This proves the shared PDP wrapper in `domain/authz.rs` and the
//! plugin-SPI dispatch wrapper in `Service` emit per DESIGN §3.11.5.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use bigdecimal::BigDecimal;
use rust_decimal::Decimal;
use time::OffsetDateTime;
use toolkit_gts::gts_id;
use toolkit_odata::{ODataQuery, Page as ODataPage, PageInfo};
use usage_collector_sdk::{
    AggregationBucket, AggregationResult, CreateUsageRecord, IdempotencyKey, MetadataKey,
    MeterTypeId, ResourceRef, UsageCollectorError, UsageRecord, UsageRecordStatus,
};
use uuid::Uuid;

use toolkit_security::pep_properties;

use super::{classify_deactivation_plugin_error, classify_query_result, classify_record_error};
use crate::domain::Service;
use crate::domain::authz::usage_record;
use crate::domain::ports::metrics::{
    DeactivationErrorCategory, QueryErrorCategory, RecordErrorCategory, RequestOutcome,
};
use crate::domain::test_support::{
    CountingPermitResolver, CountingTenantPermitResolver, DenyAllResolver, HappyPathPlugin,
    ServiceFixture, UnreachableResolver, authenticated_ctx, counter_sum_with_label, enforcer_for,
    fake_declaration_source_with_fold, fake_declaration_source_with_metadata, gauge_last,
    histogram_count, histogram_count_with_label, histogram_sum, histogram_sum_with_label,
    hub_with_plugin, local_metrics, service_with_metrics_unready_plugin, test_time_range,
};
use crate::domain::type_resolver::{TypeResolver, TypeResolverConfig};
use usage_collector_sdk::UsageCollectorPluginError;

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
        value: Decimal::from(1),
        idempotency_key: IdempotencyKey::new("idem-1").expect("valid idempotency key"),
        corrects_id: None,
        status: UsageRecordStatus::Active,
        window_start: OffsetDateTime::UNIX_EPOCH,
        window_end: OffsetDateTime::UNIX_EPOCH + time::Duration::hours(1),
    }
}

/// The identity-free create-surface twin of [`sample_record`]: mirrors its
/// canonical fields minus the server-owned `id` / `status`, for the
/// `create_usage_record{,s}` entry points which now take `CreateUsageRecord`.
fn sample_create_record() -> CreateUsageRecord {
    CreateUsageRecord {
        gts_type_id: MeterTypeId::new(SAMPLE_METER_TYPE_ID).expect("valid gts_type_id"),
        tenant_id: Uuid::from_u128(2),
        resource_ref: ResourceRef::new("rsc-1", "compute.vm").expect("valid resource ref"),
        subject_ref: None,
        metadata: BTreeMap::new(),
        value: Decimal::from(1),
        idempotency_key: IdempotencyKey::new("idem-1").expect("valid idempotency key"),
        corrects_id: None,
        window_start: OffsetDateTime::UNIX_EPOCH,
        window_end: OffsetDateTime::UNIX_EPOCH + time::Duration::hours(1),
    }
}

/// A page of `n` sample usage records (for a raw-query result-rows assertion).
fn record_page(n: usize) -> ODataPage<UsageRecord> {
    ODataPage {
        items: (0..n).map(|_| sample_record()).collect(),
        page_info: PageInfo {
            next_cursor: None,
            prev_cursor: None,
            limit: 1000,
        },
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
/// entry (identity-free, for the `create_usage_record{,s}` entry points).
fn create_record_with_metadata(key: &str, value: &str) -> CreateUsageRecord {
    let mut record = sample_create_record();
    record.metadata.insert(
        MetadataKey::new(key).expect("valid metadata key"),
        value.to_owned(),
    );
    record
}

// ── Deactivation handler ─────────────────────────────────────────────

#[tokio::test]
async fn deactivation_pdp_deny_records_true_denied_authz_despite_notfound_response() {
    // Prefetch succeeds; PDP denies → the response is existence-oracle
    // collapsed to `NotFound`, but the metric records the TRUE `(denied, authz)`.
    let plugin = HappyPathPlugin::new();
    plugin.set_get_record(sample_record());

    let (service, provider, exporter) = ServiceFixture::default()
        .with_resolver(Arc::new(DenyAllResolver))
        .build_with_metrics(plugin, "test.metrics.deact.deny.v1");

    let result = service
        .deactivate_usage_record(&authenticated_ctx(), Uuid::from_u128(0x1234))
        .await;
    assert!(
        matches!(result, Err(UsageCollectorError::NotFound { .. })),
        "PDP deny must collapse to NotFound on the caller surface: {result:?}",
    );
    provider.force_flush().unwrap();

    assert_eq!(
        counter_sum_with_label(
            &exporter,
            "uc_deactivation_requests_total",
            "outcome",
            "denied"
        ),
        1,
    );
    assert_eq!(
        counter_sum_with_label(
            &exporter,
            "uc_deactivation_requests_total",
            "error_category",
            "authz",
        ),
        1,
    );
    assert_eq!(
        histogram_count(&exporter, "uc_deactivation_duration_seconds"),
        1,
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
async fn ingestion_single_deny_records_rejected_authz_and_duration_no_request_counter() {
    let (service, provider, exporter) = ServiceFixture::default()
        .with_resolver(Arc::new(DenyAllResolver))
        .build_with_metrics(HappyPathPlugin::new(), "test.metrics.ingest.single.v1");

    let _outcome = service
        .create_usage_record(&authenticated_ctx(), sample_create_record())
        .await;
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
            "record_kind",
            "usage"
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
    // Single-emit does NOT increment the batch-only request counter.
    assert_eq!(
        counter_sum_with_label(
            &exporter,
            "uc_ingestion_requests_total",
            "outcome",
            "accepted"
        ),
        0,
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
}

// ── PDP-helper instruments (uc_authz_decisions_total / uc_pdp_failures_total /
//    uc_pdp_duration_seconds) ──────────────────────────────────────────────

// `pdp_permit_emits_permit_decision_and_duration` is gone: its coverage
// (permit → decision=permit, one duration sample, operation=ingest, no
// double-count) is subsumed by
// `per_record_permit_records_exactly_one_permit_no_double_count` below —
// both drove the check through `list_usage_types`, a catalog operation this
// gear no longer has (types-registry owns every type declaration now).

#[tokio::test]
async fn pdp_deny_emits_deny_decision_not_failure() {
    let (service, provider, exporter) = ServiceFixture::default()
        .with_resolver(Arc::new(DenyAllResolver))
        .build_with_metrics(HappyPathPlugin::new(), "test.metrics.pdp.deny.v1");

    // PDP denial short-circuits `create_usage_record_inner` before the plugin
    // or the Type Resolver are ever reached, so an unprogrammed plugin and
    // the fixture's default (inert) resolver are fine here.
    let _outcome = service
        .create_usage_record(&authenticated_ctx(), sample_create_record())
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
        .create_usage_record(&authenticated_ctx(), sample_create_record())
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
// The per-record ingestion and LIST paths authorize in two stages: the PDP
// returns a permit-with-constraints (`Ok(scope)`), then a gear-side gate
// (`scope_admits_attribution_tuple` per-record, `scope_to_odata_filter` for
// LIST) can turn that constrained permit into a deny. `uc_authz_decisions_total`
// must reflect the decision the gear returns — otherwise the deny-anomaly alert
// (DESIGN §3.11.6) never sees the cross-tenant reconnaissance signal.

#[tokio::test]
async fn pdp_permit_with_foreign_tenant_gate_denial_records_deny_not_permit() {
    // The PDP permits, but scopes the grant to ONE tenant (`granted`). The
    // record names a DIFFERENT tenant, so the per-record attribution gate
    // (`scope_admits_attribution_tuple`) turns the constrained permit into a
    // deny. That cross-tenant attempt is exactly the reconnaissance signal the
    // deny-anomaly alert keys off, so the decision counter MUST record `deny` —
    // and MUST NOT record the premature `permit` from the raw PDP return.
    let granted = Uuid::from_u128(0x5001);
    // sample_record() names tenant Uuid::from_u128(2), outside `granted`.
    let resolver =
        CountingPermitResolver::new(pep_properties::OWNER_TENANT_ID, granted.to_string());

    let (service, provider, exporter) = ServiceFixture::default()
        .with_resolver(resolver)
        .build_with_metrics(HappyPathPlugin::new(), "test.metrics.pdp.gatedeny.v1");

    let outcome = service
        .create_usage_record(&authenticated_ctx(), sample_create_record())
        .await;
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
    // The PDP permits, but with a constraint that narrows ONLY by `resource_type`
    // — no `OWNER_TENANT_ID` pin. `scope_to_odata_filter` fails that closed (a
    // tenant-less constraint would AND into the query as a cross-tenant
    // predicate). The effective LIST decision is therefore a deny, so the
    // decision counter must record `deny`, not the raw PDP `permit`.
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
    // Guards the no-double-count invariant of the two-stage per-record
    // authorize: a clean permit (the PDP grants the record's own tenant AND the
    // gate admits) must emit exactly ONE `permit` decision and ONE duration
    // sample — never `permit` + `deny` for the same call, which would corrupt
    // both sides of the deny-anomaly ratio.
    let plugin = HappyPathPlugin::new();
    plugin.set_create_record(sample_record());

    let (service, provider, exporter) = ServiceFixture::default()
        .with_resolver(CountingTenantPermitResolver::new())
        .with_source(fake_declaration_source_with_fold("SUM"))
        .build_with_metrics(plugin, "test.metrics.perrecord.permit.v1");

    service
        .create_usage_record(&authenticated_ctx(), sample_create_record())
        .await
        .expect("permitted single emit persists");
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
    // `get_usage_record` now authorizes via a pre-row compiled-scope PDP
    // request (Task 13 — no per-record attribution attributes), which
    // `CountingAllowAllResolver`'s unconstrained permit would fail closed
    // under `require_constraints(true)` before the plugin is ever
    // dispatched. `CountingPermitResolver` grants a real tenant-narrowing
    // scope instead, so the flow reaches the plugin: the unprogrammed
    // `HappyPathPlugin` then returns `Internal` for `get_usage_record` — a
    // backend-classified fault.
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

#[tokio::test]
async fn plugin_domain_typed_error_does_not_increment_accept_counter() {
    // A domain-typed variant (UsageRecordNotFound) is a caller-visible
    // outcome, NOT a plugin fault — its duration is still observed, but it
    // MUST NOT increment uc_plugin_accept_errors_total. `get_usage_record`
    // authorizes via a pre-row compiled-scope PDP request (Task 13), so
    // (as above) a real permitting resolver is required to reach the
    // plugin dispatch this test means to exercise — `CountingAllowAllResolver`'s
    // unconstrained permit would instead fail closed before the plugin
    // was ever dispatched.
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
    // The per-record ingestion authorize runs under `require_constraints(true)`
    // with a per-record attribution gate, so the PDP fake must actually grant
    // (and the gate admit) `sample_create_record()`'s own tenant for this
    // structural-unready path to be reached at all — a plain `CountingAllowAllResolver`
    // (unconstrained permit) would fail the gate before `resolve_plugin_for`
    // ever runs.
    let (service, provider, exporter) = service_with_metrics_unready_plugin(
        "test.metrics.plugin.unready.v1",
        CountingTenantPermitResolver::new(),
    );

    let _outcome = service
        .create_usage_record(&authenticated_ctx(), sample_create_record())
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

// ── Emit-path plugin-host instrument coverage ───────────────────────────────
//
// The ingestion/emit SPI dispatches (`get_usage_record`, `create_usage_record`
// / `create_usage_records`) route through the same `instrument_spi` /
// `resolve_plugin_for` wrappers as the read paths, so a permitted emit MUST
// land on `uc_plugin_call_duration_seconds` (and, on a backend fault,
// `uc_plugin_accept_errors_total`) under the emit `operation` labels. The
// deny-path emit tests above short-circuit at PDP and never reach the SPI, so
// they cannot guard this wiring; these tests drive the dispatch.
//
// Task 9: the referenced meter's declaration is now resolved through the Type
// Resolver, not a plugin-side `get_usage_type` catalog dispatch — so these
// tests wire a working resolver (`service_with_metrics_and_source`) and no
// longer assert a `get_usage_type` duration sample on the emit path (there is
// none to assert any more).

#[tokio::test]
async fn ingestion_single_success_dispatch_records_plugin_call_duration_per_op() {
    // Permit + declaration resolution + persist echo. The persist
    // `create_usage_record` dispatch is instrumented, contributing one
    // duration sample under its own `operation` label.
    let plugin = HappyPathPlugin::new();
    plugin.set_create_record(sample_record());

    let (service, provider, exporter) = ServiceFixture::default()
        .with_resolver(CountingTenantPermitResolver::new())
        .with_source(fake_declaration_source_with_fold("SUM"))
        .build_with_metrics(plugin, "test.metrics.ingestok.single.v1");

    service
        .create_usage_record(&authenticated_ctx(), sample_create_record())
        .await
        .expect("permitted single emit persists");
    provider.force_flush().unwrap();

    assert_eq!(
        histogram_count_with_label(
            &exporter,
            "uc_plugin_call_duration_seconds",
            "operation",
            "create_usage_record",
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
            "create_usage_record",
        ),
        0,
    );
}

#[tokio::test]
async fn ingestion_batch_success_dispatch_records_plugin_call_duration_per_op() {
    // Two records sharing one gts_id: the declaration resolves once (Task 9's
    // resolver fan-out — see `ingestion_declared_type_tests` in
    // `service_tests.rs` for the dedicated dedup coverage), and the eligible
    // records persist through one `create_usage_records` dispatch — an
    // instrumented completion.
    let plugin = HappyPathPlugin::new();
    plugin.set_create_records(vec![Ok(sample_record()), Ok(sample_record())]);

    let (service, provider, exporter) = ServiceFixture::default()
        .with_resolver(CountingTenantPermitResolver::new())
        .with_source(fake_declaration_source_with_fold("SUM"))
        .build_with_metrics(plugin, "test.metrics.ingestok.batch.v1");

    let per_record = service
        .create_usage_records(
            &authenticated_ctx(),
            vec![sample_create_record(), sample_create_record()],
        )
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
async fn ingestion_single_backend_error_increments_accept_errors_per_op() {
    // Declaration resolves, then the persist SPI faults with `Internal`
    // (backend). The failed dispatch is still a completed dispatch (one
    // duration sample) AND a backend-classified accept error under
    // `operation="create_usage_record"`.
    let plugin = HappyPathPlugin::new();
    plugin.set_create_record_err(UsageCollectorPluginError::internal(
        "usage-collector test fake: simulated persist backend fault",
    ));

    let (service, provider, exporter) = ServiceFixture::default()
        .with_resolver(CountingTenantPermitResolver::new())
        .with_source(fake_declaration_source_with_fold("SUM"))
        .build_with_metrics(plugin, "test.metrics.ingesterr.single.v1");

    let outcome = service
        .create_usage_record(&authenticated_ctx(), sample_create_record())
        .await;
    assert!(outcome.is_err(), "a persist backend fault surfaces as Err");
    provider.force_flush().unwrap();

    assert_eq!(
        counter_sum_with_label(
            &exporter,
            "uc_plugin_accept_errors_total",
            "operation",
            "create_usage_record",
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
            "create_usage_record",
        ),
        1,
        "an error completion is still a dispatch completion",
    );
}

#[tokio::test]
async fn ingestion_batch_backend_error_increments_accept_errors_per_op() {
    // The declaration resolves so the record is eligible and the batch
    // reaches the persist SPI; `create_usage_records` is left unprogrammed,
    // so the stub returns an outer `Internal` transport fault
    // (backend-classified) which surfaces as the batch-level outer `Err`.
    let plugin = HappyPathPlugin::new();

    let (service, provider, exporter) = ServiceFixture::default()
        .with_resolver(CountingTenantPermitResolver::new())
        .with_source(fake_declaration_source_with_fold("SUM"))
        .build_with_metrics(plugin, "test.metrics.ingesterr.batch.v1");

    let outcome = service
        .create_usage_records(&authenticated_ctx(), vec![sample_create_record()])
        .await;
    assert!(
        outcome.is_err(),
        "a batch-level persist transport fault surfaces as an outer Err",
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
    );
}

#[tokio::test]
async fn ingestion_batch_unready_plugin_increments_unready_counter() {
    // The batch path resolves the plugin (via `resolve_plugin_for`) BEFORE the
    // PDP fan-out; a structurally-unready binding short-circuits there and MUST
    // still emit the `unready` accept error. The pre-completion emit path called
    // `get_plugin()` directly and skipped this counter — this guards the switch
    // to `resolve_plugin_for`.
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

// ── Metric-label classifiers: exhaustive arm coverage (pure fns) ────────────
//
// `classify_record_error`, `classify_query_result` and
// `classify_deactivation_plugin_error` decide the `error_category` /
// `outcome` labels operators alert on. The end-to-end tests above exercise only
// the authz / not-found arms; these table-driven unit tests pin EVERY arm of
// the closed §3.11.5 vocabularies, so a misrouted variant is caught here rather
// than as a silently-wrong dashboard series.

/// The canonical sample `gts_type_id` as a typed id, for classifier
/// fixtures exercising the record / meter-reference surface
/// (`list_usage_records`, `query_aggregated_usage_records`, and the
/// `UnknownMetadataKey` variant, which is attributed to the meter the
/// ingested record referenced).
fn meter_gts() -> MeterTypeId {
    MeterTypeId::new(SAMPLE_METER_TYPE_ID).expect("valid gts_type_id")
}

/// The `UsageCollectorError::NotFound` an unresolved `gts_type_id` produces —
/// the Type Resolver's `DomainError::DeclarationNotFound`, lifted through the
/// same bridge `Service::create_usage_record_inner` /
/// `query_aggregated_usage_records` use. Exercises the real production path
/// rather than hand-building the `NotFound` variant, so this fixture cannot
/// drift from what the resolver actually produces.
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
        // A record-resource NotFound (an L1 `corrects_id` reference) stays with
        // the semantics family — NOT folded into catalog absence.
        (
            UsageCollectorError::usage_record_not_found(Uuid::from_u128(7)),
            RecordErrorCategory::SemanticsViolation,
        ),
        (
            UsageCollectorError::corrects_id_not_found(Uuid::from_u128(8)),
            RecordErrorCategory::SemanticsViolation,
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
        (
            UsageCollectorError::negative_counter_value(Decimal::from(-1)),
            RecordErrorCategory::SemanticsViolation,
        ),
        // Conflict: idempotency is its own category; any other conflict reason
        // is semantics_violation.
        (
            UsageCollectorError::idempotency_conflict("k", Uuid::from_u128(9)),
            RecordErrorCategory::IdempotencyConflict,
        ),
        (
            UsageCollectorError::corrects_id_inactive(Uuid::from_u128(10)),
            RecordErrorCategory::SemanticsViolation,
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
        // `InvalidArgument` splits on the typed reason. The over-cap
        // aggregate result stands in for the query-budget / query-surface
        // family: a `$filter` naming a reserved field, an undeclared
        // `group_by` / `metadata_filter` key, or this. The mandatory read
        // range never lands here at all — it is validated at the edge,
        // where the typed parameter is parsed, before the service is
        // entered.
        (
            Err(UsageCollectorError::aggregation_result_too_large(
                usage_collector_sdk::MAX_AGGREGATION_BUCKETS,
            )),
            (RequestOutcome::Error, QueryErrorCategory::QueryBudget),
        ),
        // The other arm, and the reason the classifier reads the reason at
        // all: a continuation refused because its cursor was minted over a
        // different query is not a budget rejection, so collapsing both
        // onto `query_budget` would make the metric unable to tell a
        // caller paging wrongly from a caller scanning too widely.
        (
            Err(UsageCollectorError::cursor_query_mismatch()),
            (RequestOutcome::Error, QueryErrorCategory::FilterMismatch),
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

#[test]
fn classify_deactivation_plugin_error_maps_each_arm() {
    let already = UsageCollectorPluginError::UsageRecordAlreadyInactive {
        id: Uuid::from_u128(1),
    };
    let not_found = UsageCollectorPluginError::UsageRecordNotFound {
        id: Uuid::from_u128(2),
    };
    let transient = UsageCollectorPluginError::transient("backend blip");
    let internal = UsageCollectorPluginError::internal("boom");

    assert_eq!(
        classify_deactivation_plugin_error(&already),
        DeactivationErrorCategory::AlreadyInactive,
    );
    assert_eq!(
        classify_deactivation_plugin_error(&not_found),
        DeactivationErrorCategory::NotFound,
    );
    // Every other plugin fault (retryable or not) is a plugin_error.
    assert_eq!(
        classify_deactivation_plugin_error(&transient),
        DeactivationErrorCategory::PluginError,
    );
    assert_eq!(
        classify_deactivation_plugin_error(&internal),
        DeactivationErrorCategory::PluginError,
    );
}

// ── Query gateway: success + aggregated coverage ────────────────────────────
//
// The deny test above exits at PDP before the inflight guard / dispatch; these
// drive a permitted raw list and a permitted aggregation to completion, pinning
// the `success` outcome, the result-rows observation (off page size for raw /
// bucket count for aggregated), and the duration sample the deny path can't
// reach.

#[tokio::test]
async fn query_raw_success_records_success_rows_and_duration() {
    let plugin = HappyPathPlugin::new();
    plugin.set_list_usage_records_response(record_page(3));

    // `list_usage_records` now resolves the queried meter's declaration
    // (Spec §3.11 `metadata_filter` gating), so a success-path test needs a
    // `DeclarationSource` that actually resolves — the default
    // `UnavailableDeclarationSource` would turn this into a
    // `ServiceUnavailable` before ever reaching the plugin.
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

#[tokio::test]
async fn query_aggregated_success_records_success_rows_and_duration() {
    let plugin = HappyPathPlugin::new();
    plugin.set_query_aggregated_usage_records_response(single_bucket_aggregation());

    // Unlike `service_with_metrics` (whose Type Resolver is inert — see
    // `test_support::inert_type_resolver`), the aggregate path now resolves
    // the queried meter's declaration before dispatch, so this test wires
    // its own resolver over a fake source declaring a fold, rather than
    // going through that shared builder.
    let hub = hub_with_plugin(plugin, "test.metrics.query.aggok.v1", "cyberfabric");
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
        "cyberfabric".to_owned(),
        enforcer_for(tenant_scoped_permit()),
        metrics,
        type_resolver,
        crate::domain::validation::DEFAULT_METADATA_SIZE_CAP_BYTES,
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
async fn ingestion_single_with_metadata_observes_record_metadata_bytes() {
    // A permitted single emit whose resolved declaration declares the key it
    // carries reaches `observe_metadata_bytes` and persists.
    let plugin = HappyPathPlugin::new();
    plugin.set_create_record(sample_record());

    let (service, provider, exporter) = ServiceFixture::default()
        .with_resolver(CountingTenantPermitResolver::new())
        .with_source(fake_declaration_source_with_metadata(&["region"]))
        .build_with_metrics(plugin, "test.metrics.ingest.meta.v1");

    service
        .create_usage_record(
            &authenticated_ctx(),
            create_record_with_metadata("region", "us-east-1"),
        )
        .await
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
async fn ingestion_single_empty_metadata_skips_record_metadata_bytes() {
    // `sample_record()` carries no metadata → `observe_metadata_bytes` returns
    // before recording, so the instrument stays empty even on a clean persist.
    let plugin = HappyPathPlugin::new();
    plugin.set_create_record(sample_record());

    let (service, provider, exporter) = ServiceFixture::default()
        .with_resolver(CountingTenantPermitResolver::new())
        .with_source(fake_declaration_source_with_fold("SUM"))
        .build_with_metrics(plugin, "test.metrics.ingest.nometa.v1");

    service
        .create_usage_record(&authenticated_ctx(), sample_create_record())
        .await
        .expect("a permitted record with no metadata persists");
    provider.force_flush().unwrap();

    assert_eq!(
        histogram_count(&exporter, "uc_record_metadata_bytes"),
        0,
        "an empty-metadata record must record nothing on uc_record_metadata_bytes",
    );
}
