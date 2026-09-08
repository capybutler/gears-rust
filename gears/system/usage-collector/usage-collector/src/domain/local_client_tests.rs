//! Unit tests for the local client surface.
//!
//! Coverage: the service-delegating methods (usage-record ingestion and
//! read-by-id) clear the trait boundary and hit the PDP preflight inside
//! the domain service — with an unreachable PDP and/or a missing storage
//! plugin the surface error is a fail-closed envelope.
//!
//! Plus the two read paths' mandatory `TimeRange`: the client is a
//! forwarding shim, and a shim that silently substituted a range would
//! turn every in-process read into an unbounded scan that still answers
//! `Ok`, so the assertion is on what the plugin behind the service was
//! handed rather than on the call succeeding.
//!
//! Plus the backfill delegation, where what is worth pinning is which
//! service method it reaches: the live and the backfill batch share a
//! signature and differ only in the origin each stamps, so the wrong
//! target compiles and still answers `Ok` for anything recent.

use std::sync::Arc;

use toolkit::client_hub::ClientHub;
use toolkit_security::SecurityContext;
use usage_collector_sdk::{
    IdempotencyKey, RecordOrigin, ResourceRef, UsageCollectorClientV1, UsageCollectorError,
};
use uuid::Uuid;

use crate::domain::test_support::{
    HappyPathPlugin, RECORDING_PLUGIN_SUFFIX, RecordingPlugin, ServiceFixture, UnreachableResolver,
    enforcer_for, fake_declaration_source_with_fold, fake_declaration_source_with_metadata,
    projected_with_origin, recent_window_end, recent_window_start, recording_plugin_resolver,
    test_time_range,
};

use super::*;

fn make_client() -> UsageCollectorLocalClient {
    let hub = Arc::new(ClientHub::new());
    let enforcer = enforcer_for(Arc::new(UnreachableResolver));
    let svc = Arc::new(Service::new(hub, "cyberfabric".to_owned(), enforcer));
    UsageCollectorLocalClient::new(svc)
}

fn authenticated_ctx() -> SecurityContext {
    SecurityContext::builder()
        .subject_id(Uuid::from_u128(1))
        .subject_tenant_id(Uuid::from_u128(2))
        .build()
        .expect("authenticated context")
}

#[tokio::test]
async fn get_usage_record_fails_closed_without_plugin_or_pdp() {
    // `get_usage_record` resolves the storage plugin first so it can
    // pre-fetch the target record's attribution tuple before PDP
    // authorization. With NO plugin registered AND an unreachable PDP
    // the call MUST still fail closed — the observable error here is
    // `PluginUnavailable` (plugin resolution runs before authz), which
    // lifts to `ServiceUnavailable` at the canonical envelope boundary.
    // The smoke test guards "the SDK trait never silently succeeds when
    // the host is misconfigured."
    let client = make_client();
    let err = client
        .get_usage_record(&authenticated_ctx(), Uuid::from_u128(0xFEED))
        .await
        .expect_err("misconfigured host must fail closed");
    assert!(
        matches!(err, UsageCollectorError::ServiceUnavailable { .. }),
        "expected fail-closed envelope, got {err:?}"
    );
}

const FIXTURE_METER_GTS_ID: &str =
    toolkit_gts::gts_id!("cf.core.uc.usage_record.v1~cf.mini_chat._.tokens_consumed.v1~");

/// A client over a `Service` bound to a [`RecordingPlugin`] spy, wired with
/// the fixed-tenant PDP fake and a resolvable declaration both read paths
/// need. Unlike [`make_client`] this one reaches the plugin.
fn client_and_spy() -> (UsageCollectorLocalClient, Arc<RecordingPlugin>) {
    let plugin = RecordingPlugin::new();
    let svc = ServiceFixture::default()
        .with_source(fake_declaration_source_with_metadata(&[]))
        .with_resolver(recording_plugin_resolver())
        .build(
            Arc::clone(&plugin) as Arc<dyn usage_collector_sdk::UsageCollectorPluginV1>,
            RECORDING_PLUGIN_SUFFIX,
        );
    (UsageCollectorLocalClient::new(svc), plugin)
}

fn fixture_meter_id() -> MeterTypeId {
    MeterTypeId::new(FIXTURE_METER_GTS_ID).expect("valid gts_type_id")
}

#[tokio::test]
async fn list_usage_records_forwards_the_typed_range_to_the_plugin() {
    let (client, spy) = client_and_spy();
    spy.set_list_usage_records_response(ODataPage::empty(0));

    let range = test_time_range();
    client
        .list_usage_records(
            &authenticated_ctx(),
            fixture_meter_id(),
            range,
            &ODataQuery::default(),
            &[],
        )
        .await
        .expect("an in-process list over a bounded range succeeds");

    assert_eq!(
        spy.last_list_time_range(),
        Some(range),
        "the client MUST forward the caller's range verbatim: a substituted \
         one is an unbounded scan that still answers Ok",
    );
}

#[tokio::test]
async fn query_aggregated_usage_records_forwards_the_typed_range_to_the_plugin() {
    let (client, spy) = client_and_spy();
    spy.set_query_aggregated_usage_records_response(usage_collector_sdk::AggregationResult {
        buckets: vec![],
    });

    let range = test_time_range();
    client
        .query_aggregated_usage_records(
            &authenticated_ctx(),
            fixture_meter_id(),
            range,
            &ODataQuery::default(),
            &[],
            &[],
        )
        .await
        .expect("an in-process aggregation over a bounded range succeeds");

    assert_eq!(
        spy.last_aggregate_time_range(),
        Some(range),
        "the aggregate path has no page-size ceiling, so the range is its \
         only scan bound and MUST reach the plugin verbatim",
    );
}

// ── The backfill route across the trait boundary ───────────────────────────
//
// `backfill_usage_records` is a delegation like its neighbours, but it is
// the one whose *target* is observable: the live and the backfill batch
// share a signature and differ only in the origin each stamps, so a
// delegation wired to `create_usage_records` would still compile and still
// answer `Ok` for anything recent. What the assertions below discriminate
// on is therefore the marker the gateway handed the storage plugin, and a
// covered period only the backfill route admits.

const BACKFILL_PLUGIN_SUFFIX: &str = "test.local.backfill.records.v1";

/// A submission over the memoised recent covered period — the one the live
/// path admits — or, with `days_old > 0`, the same submission over a period
/// that far back and differing in nothing else.
///
/// Offset from [`recent_window_end`] rather than from a fresh `now_utc()`,
/// so two submissions built by separate calls carry the same period rather
/// than two clock reads microseconds apart.
fn backfill_submission(idem: &str, days_old: i64) -> CreateUsageRecord {
    let age = time::Duration::days(days_old);
    CreateUsageRecord {
        gts_type_id: fixture_meter_id(),
        tenant_id: Uuid::from_u128(2),
        resource_ref: ResourceRef::new("rsc-import", "compute.vm").expect("valid resource ref"),
        subject_ref: None,
        metadata: std::collections::BTreeMap::new(),
        value: rust_decimal::Decimal::from(1),
        idempotency_key: IdempotencyKey::new(idem).expect("valid idempotency key"),
        invalidation: None,
        window_start: recent_window_start() - age,
        window_end: recent_window_end() - age,
    }
}

#[tokio::test]
async fn backfill_usage_records_reaches_the_backfill_route_not_the_live_batch() {
    let plugin = HappyPathPlugin::new();
    // Thirty days back: past the live path's 48-hour past tolerance, inside
    // the 90-day backfill window (so the entry still authorizes `create`
    // and the fixture needs no elevated grant). The fresh entry beside it
    // is the load-bearing half of the origin assertion — a batch of nothing
    // but aged periods would also be stamped `backfill` by a route that
    // derived the marker from how old a period is rather than from the
    // entry point it arrived on.
    let input = vec![
        backfill_submission("idem-local-import-aged", 30),
        backfill_submission("idem-local-import-fresh", 0),
    ];
    plugin.set_create_records(
        input
            .iter()
            .map(|r| Ok(projected_with_origin(r, RecordOrigin::Backfill)))
            .collect(),
    );
    let svc = ServiceFixture::default()
        .with_source(fake_declaration_source_with_fold("SUM"))
        .build(
            Arc::clone(&plugin) as Arc<dyn usage_collector_sdk::UsageCollectorPluginV1>,
            BACKFILL_PLUGIN_SUFFIX,
        );
    let client = UsageCollectorLocalClient::new(svc);

    let results = client
        .backfill_usage_records(&authenticated_ctx(), input)
        .await
        .expect(
            "the backfill route admits both entries and dispatches both; a \
             delegation to the live batch would reject the aged one \
             per-record with PAST_WINDOW and dispatch only the fresh one, \
             which the two-entry echo then fails to match",
        );

    assert_eq!(results.len(), 2);
    assert!(results.iter().all(Result::is_ok), "{results:?}");

    let dispatched = plugin
        .last_create_records_input()
        .expect("both entries reached the storage plugin");
    assert_eq!(
        dispatched.iter().map(|r| r.origin).collect::<Vec<_>>(),
        vec![RecordOrigin::Backfill, RecordOrigin::Backfill],
        "the client MUST forward to the backfill route: delegating to \
         `create_usage_records` instead would stamp `live` on an import",
    );
}
