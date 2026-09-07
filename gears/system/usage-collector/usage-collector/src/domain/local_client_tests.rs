//! Unit tests for the local client surface.
//!
//! Coverage: the service-delegating methods (usage-record ingestion +
//! deactivation + read-by-id) clear the trait boundary and hit the PDP
//! preflight inside the domain service — with an unreachable PDP and/or a
//! missing storage plugin the surface error is a fail-closed envelope.
//!
//! Plus the two read paths' mandatory `TimeRange`: the client is a
//! forwarding shim, and a shim that silently substituted a range would
//! turn every in-process read into an unbounded scan that still answers
//! `Ok`, so the assertion is on what the plugin behind the service was
//! handed rather than on the call succeeding.

use std::sync::Arc;

use toolkit::client_hub::ClientHub;
use toolkit_security::SecurityContext;
use usage_collector_sdk::{UsageCollectorClientV1, UsageCollectorError};
use uuid::Uuid;

use crate::domain::test_support::{
    RECORDING_PLUGIN_SUFFIX, RecordingPlugin, ServiceFixture, UnreachableResolver, enforcer_for,
    fake_declaration_source_with_metadata, recording_plugin_resolver, test_time_range,
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
async fn deactivate_usage_record_fails_closed_without_plugin_or_pdp() {
    // `deactivate_usage_record` now resolves the storage plugin first
    // (so it can pre-fetch the target record and feed the loaded
    // attribution tuple into PDP). With NO plugin registered in the hub
    // AND an unreachable PDP, the call MUST still fail closed — the
    // observable error here is `PluginUnavailable` (plugin resolution
    // runs before authz now), which lifts to `ServiceUnavailable` at the
    // canonical envelope boundary. The point of this smoke test is "the
    // SDK trait never silently succeeds when the host is misconfigured."
    let client = make_client();
    let err = client
        .deactivate_usage_record(&authenticated_ctx(), Uuid::from_u128(0xFEED))
        .await
        .expect_err("misconfigured host must fail closed");
    // Any of these variants is a fail-closed envelope — the specific one
    // depends on what the host resolves first (types-registry → plugin
    // selection → PDP). The invariant the smoke test guards is "never
    // Ok(()) when the host is misconfigured."
    assert!(
        matches!(err, UsageCollectorError::ServiceUnavailable { .. }),
        "expected fail-closed envelope, got {err:?}"
    );
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

const READ_METER_GTS_ID: &str =
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

fn read_meter_id() -> MeterTypeId {
    MeterTypeId::new(READ_METER_GTS_ID).expect("valid gts_type_id")
}

#[tokio::test]
async fn list_usage_records_forwards_the_typed_range_to_the_plugin() {
    let (client, spy) = client_and_spy();
    spy.set_list_usage_records_response(ODataPage::empty(0));

    let range = test_time_range();
    client
        .list_usage_records(
            &authenticated_ctx(),
            read_meter_id(),
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
            read_meter_id(),
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
