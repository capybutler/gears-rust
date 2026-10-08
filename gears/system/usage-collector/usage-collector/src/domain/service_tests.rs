//! Unit tests for the Plugin Host binding (`Service::get_plugin` /
//! `resolve_plugin`) and the domain service behaviour built on it.
//!
//! Binding coverage: resolve + cache, registry-unavailable retry (no error
//! caching), `PluginNotFound` on no-match / vendor mismatch,
//! `PluginUnavailable` on an empty scoped slot, and a binding that stays
//! monotonic for the Service lifetime (`reset` exercised in tests only).

use std::sync::Arc;

use authz_resolver_sdk::PolicyEnforcer;
use toolkit::client_hub::{ClientHub, ClientScope};
use types_registry_sdk::TypesRegistryClient;
use types_registry_sdk::testing::{
    MockTypesRegistryClient, internal as canonical_internal, make_test_instance,
};
use usage_collector_sdk::{
    UsageCollectorPluginSpecV1, UsageCollectorPluginV1, UsageRecordFilterField,
};

use super::*;
use crate::domain::ports::declarations::DeclarationSource;
use crate::domain::test_support::{HappyPathPlugin, MockPlugin, UnreachableResolver, enforcer_for};

/// Enforcer for tests that never reach the PDP path (binding / plugin-host
/// tests); its transport panics if an authz call is ever made.
fn dummy_enforcer() -> PolicyEnforcer {
    enforcer_for(Arc::new(UnreachableResolver))
}

/// Assert that the scope the service last handed `get_usage_record` is one a
/// conforming backend can translate.
///
/// The question is "does it translate", not "what shape is it", which a
/// `Debug`-string assertion cannot draw: this reads
/// [`HappyPathPlugin::last_get_scope_expr`] and puts the real converter
/// behind the answer. It reads the LAST capture, so every caller sits after
/// an assertion pinning `get_usage_record_calls()` to exactly 1; a test over
/// several targets must pin the count too.
fn assert_translatable_scope(plugin: &HappyPathPlugin, call_site: &str) {
    let scope = plugin
        .last_get_scope_expr()
        .expect("the target pre-read must have dispatched to the plugin");

    toolkit_odata::filter::convert_expr_to_filter_node::<UsageRecordFilterField>(&scope)
        .unwrap_or_else(|err| {
            panic!(
                "the scope `{call_site}` hands `get_usage_record` must be translatable \
                 by a conforming backend; `{scope:?}` was refused as {err:?}"
            )
        });
}

// ── helpers ──────────────────────────────────────────────────────────────

fn empty_hub() -> Arc<ClientHub> {
    Arc::new(ClientHub::default())
}

/// Build the GTS instance ID string for a usage-collector storage-plugin test
/// instance: schema prefix + a 5-token instance suffix.
fn test_instance_id() -> String {
    format!(
        "{}test.usage_collector.mock.instance.v1",
        UsageCollectorPluginSpecV1::gts_type_id()
    )
}

/// JSON content for a `PluginV1<UsageCollectorPluginSpecV1>` instance that
/// `choose_plugin_instance` can successfully parse.
fn plugin_content(gts_id: &str, vendor: &str) -> serde_json::Value {
    serde_json::json!({
        "id": gts_id,
        "vendor": vendor,
        "priority": 0,
        "properties": {}
    })
}

/// Wires a counting `MockTypesRegistryClient` and a scoped plugin into a
/// `ClientHub`. Returns `(hub, registry_arc)` so tests can inspect
/// `list_instance_calls()`.
fn hub_with_counting_registry_and_plugin(
    instance_id: &str,
    vendor: &str,
    plugin: Arc<dyn UsageCollectorPluginV1>,
) -> (Arc<ClientHub>, Arc<MockTypesRegistryClient>) {
    let hub = Arc::new(ClientHub::default());

    let instance = make_test_instance(instance_id, plugin_content(instance_id, vendor));
    let registry = Arc::new(MockTypesRegistryClient::new().with_instances([instance]));
    hub.register::<dyn TypesRegistryClient>(registry.clone() as Arc<dyn TypesRegistryClient>);

    hub.register_scoped::<dyn UsageCollectorPluginV1>(ClientScope::gts_id(instance_id), plugin);

    (hub, registry)
}

fn hub_with_registry_and_plugin(
    instance_id: &str,
    vendor: &str,
    plugin: Arc<dyn UsageCollectorPluginV1>,
) -> Arc<ClientHub> {
    hub_with_counting_registry_and_plugin(instance_id, vendor, plugin).0
}

// ── resolve + cache ───────────────────────────────────────────────────────

// The first dispatch resolves single-flight; the warm call reuses the cached
// id (no extra registry round-trip) and the same scoped Arc.
#[tokio::test]
async fn get_plugin_resolves_then_caches_resolved_instance() {
    let instance_id = test_instance_id();
    let (hub, registry) =
        hub_with_counting_registry_and_plugin(&instance_id, "constructorfabric", MockPlugin::arc());

    let svc = Service::new(hub, "constructorfabric".into(), dummy_enforcer());
    let p1 = svc.get_plugin().await.unwrap();
    let p2 = svc.get_plugin().await.unwrap();

    assert_eq!(
        registry.list_instance_calls(),
        1,
        "resolve_plugin must run exactly once; the warm call must use the cached id"
    );
    assert!(
        Arc::ptr_eq(&p1, &p2),
        "both calls must return the same scoped plugin Arc (cached binding)"
    );
}

// ── registry-unavailable retry (no error caching) ──────────────────────────

// A failing registry surfaces `TypesRegistryUnavailable`, the selector cache
// stays empty, and the NEXT dispatch retries (list_instance_calls == 2).
#[tokio::test]
async fn get_plugin_retries_resolution_on_each_call_when_registry_fails() {
    let hub = Arc::new(ClientHub::default());
    let registry =
        Arc::new(MockTypesRegistryClient::new().with_list_error(canonical_internal("unavailable")));
    hub.register::<dyn TypesRegistryClient>(registry.clone() as Arc<dyn TypesRegistryClient>);

    let svc = Service::new(hub, "constructorfabric".into(), dummy_enforcer());

    let err = svc.get_plugin().await.err().expect("expected Err");
    assert!(
        matches!(err, DomainError::TypesRegistryUnavailable(_)),
        "expected TypesRegistryUnavailable, got: {err:?}"
    );
    assert!(svc.get_plugin().await.is_err());

    assert_eq!(
        registry.list_instance_calls(),
        2,
        "the selector must not cache errors; each dispatch must re-attempt resolution"
    );
}

// An empty hub (no registered TypesRegistryClient) surfaces
// `TypesRegistryUnavailable` from the explicit hub.get map_err.
#[tokio::test]
async fn get_plugin_returns_registry_unavailable_when_hub_empty() {
    let svc = Service::new(empty_hub(), "constructorfabric".into(), dummy_enforcer());
    let err = svc.get_plugin().await.err().expect("expected Err");
    assert!(
        matches!(err, DomainError::TypesRegistryUnavailable(_)),
        "expected TypesRegistryUnavailable, got: {err:?}"
    );
}

// ── PluginNotFound ─────────────────────────────────────────────────────────

// No registered instances -> no match -> `PluginNotFound`.
#[tokio::test]
async fn get_plugin_returns_plugin_not_found_when_no_instances() {
    let hub = Arc::new(ClientHub::default());
    let registry: Arc<dyn TypesRegistryClient> = Arc::new(MockTypesRegistryClient::new());
    hub.register::<dyn TypesRegistryClient>(registry);

    let svc = Service::new(hub, "constructorfabric".into(), dummy_enforcer());
    let err = svc.get_plugin().await.err().expect("expected Err");
    assert!(
        matches!(err, DomainError::PluginNotFound { .. }),
        "expected PluginNotFound, got: {err:?}"
    );
}

// An instance exists but its vendor does not match the configured vendor ->
// `PluginNotFound`.
#[tokio::test]
async fn get_plugin_returns_plugin_not_found_when_vendor_mismatch() {
    let instance_id = test_instance_id();
    let hub = Arc::new(ClientHub::default());
    let instance = make_test_instance(&instance_id, plugin_content(&instance_id, "other-vendor"));
    let registry: Arc<dyn TypesRegistryClient> =
        Arc::new(MockTypesRegistryClient::new().with_instances([instance]));
    hub.register::<dyn TypesRegistryClient>(registry);

    let svc = Service::new(hub, "constructorfabric".into(), dummy_enforcer());
    let err = svc.get_plugin().await.err().expect("expected Err");
    assert!(
        matches!(err, DomainError::PluginNotFound { .. }),
        "expected PluginNotFound, got: {err:?}"
    );
}

// Instance content that fails to deserialize -> `InvalidPluginInstance`.
#[tokio::test]
async fn get_plugin_returns_invalid_when_content_malformed() {
    let instance_id = test_instance_id();
    let hub = Arc::new(ClientHub::default());
    let instance = make_test_instance(
        &instance_id,
        serde_json::json!({ "not": "valid-plugin-content" }),
    );
    let registry: Arc<dyn TypesRegistryClient> =
        Arc::new(MockTypesRegistryClient::new().with_instances([instance]));
    hub.register::<dyn TypesRegistryClient>(registry);

    let svc = Service::new(hub, "constructorfabric".into(), dummy_enforcer());
    let err = svc.get_plugin().await.err().expect("expected Err");
    assert!(
        matches!(err, DomainError::InvalidPluginInstance { .. }),
        "expected InvalidPluginInstance, got: {err:?}"
    );
}

// ── PluginUnavailable (empty scoped slot) ──────────────────────────────────

// The registry resolves but the scoped client is absent -> `try_get_scoped`
// returns None -> `PluginUnavailable`.
#[tokio::test]
async fn get_plugin_returns_unavailable_when_scoped_slot_empty() {
    let instance_id = test_instance_id();
    let hub = Arc::new(ClientHub::default());
    let instance = make_test_instance(
        &instance_id,
        plugin_content(&instance_id, "constructorfabric"),
    );
    let registry: Arc<dyn TypesRegistryClient> =
        Arc::new(MockTypesRegistryClient::new().with_instances([instance]));
    hub.register::<dyn TypesRegistryClient>(registry);

    let svc = Service::new(hub, "constructorfabric".into(), dummy_enforcer());
    let err = svc.get_plugin().await.err().expect("expected Err");
    assert!(
        matches!(err, DomainError::PluginUnavailable { .. }),
        "expected PluginUnavailable, got: {err:?}"
    );
}

// ── monotonic binding for the Service lifetime ─────────────────────────────

// The binding is monotonic for the Service lifetime.
// `GtsPluginSelector::reset` is exercised ONLY in unit tests (there is no
// runtime config-change channel); after a reset the next dispatch re-resolves,
// proving the cache is the only re-resolution trigger.
#[tokio::test]
async fn binding_is_monotonic_until_selector_reset() {
    let instance_id = test_instance_id();
    let (hub, registry) =
        hub_with_counting_registry_and_plugin(&instance_id, "constructorfabric", MockPlugin::arc());

    let svc = Service::new(hub, "constructorfabric".into(), dummy_enforcer());

    // Two warm dispatches reuse the cached binding (monotonic).
    let _ = svc.get_plugin().await.unwrap();
    let _ = svc.get_plugin().await.unwrap();
    assert_eq!(
        registry.list_instance_calls(),
        1,
        "binding must be monotonic: no re-resolution without an explicit reset"
    );

    // Test-only reset clears the cache; the next dispatch re-resolves.
    assert!(
        svc.selector_reset_for_test().await,
        "reset must report a previously-cached value"
    );
    let _ = svc.get_plugin().await.unwrap();
    assert_eq!(
        registry.list_instance_calls(),
        2,
        "after a test-only reset the next dispatch must re-resolve"
    );
}

// A wired registry instance plus a wired scoped client resolves to a handle.
#[tokio::test]
async fn get_plugin_returns_registered_scoped_handle() {
    let instance_id = test_instance_id();
    let hub = hub_with_registry_and_plugin(&instance_id, "constructorfabric", MockPlugin::arc());

    let svc = Service::new(hub, "constructorfabric".into(), dummy_enforcer());
    let resolved = svc.get_plugin().await;
    assert!(
        resolved.is_ok(),
        "expected a resolved scoped handle, got: {:?}",
        resolved.err()
    );
}

// ── injecting a resolver built over a fake declaration source ──────────────
//
// `new_with_metrics` takes a pre-built `Arc<TypeResolver>`, so a test wanting a
// fake declaration source builds a `TypeResolver` over it and passes the
// resolver straight in. This pins that the plugin-host binding path never
// consults the resolver: the source below panics if it is ever called.

/// A [`DeclarationSource`] that panics if invoked — used to prove a code
/// path never consults it.
struct UnreachableDeclarationSource;

#[async_trait::async_trait]
impl DeclarationSource for UnreachableDeclarationSource {
    async fn fetch(
        &self,
        _id: &usage_collector_sdk::MeterTypeId,
    ) -> Result<types_registry_sdk::GtsTypeSchema, DomainError> {
        panic!("UnreachableDeclarationSource::fetch must never be called");
    }

    async fn fetch_by_uuid(
        &self,
        _type_uuid: Uuid,
    ) -> Result<types_registry_sdk::GtsTypeSchema, DomainError> {
        panic!("UnreachableDeclarationSource::fetch_by_uuid must never be called");
    }
}

#[tokio::test]
async fn new_with_metrics_accepts_a_resolver_built_over_a_fake_source() {
    let instance_id = test_instance_id();
    let hub = hub_with_registry_and_plugin(&instance_id, "constructorfabric", MockPlugin::arc());

    let type_resolver = Arc::new(TypeResolver::new(
        Arc::new(UnreachableDeclarationSource),
        TypeResolverConfig {
            ttl: Duration::from_mins(5),
            capacity: 10_000,
        },
        Arc::new(NoopMetrics),
    ));
    let svc = Service::new_with_metrics(
        hub,
        "constructorfabric".to_owned(),
        dummy_enforcer(),
        Arc::new(NoopMetrics),
        type_resolver,
        crate::domain::validation::DEFAULT_METADATA_SIZE_CAP_BYTES,
        crate::domain::test_support::default_covered_period_bounds(),
        crate::domain::service::DEFAULT_MAX_BATCH_RECORDS,
        crate::config::IngestionQuotaConfig::default(),
        crate::domain::service::DEFAULT_UNAVAILABLE_RETRY_AFTER_SECS,
        crate::domain::service::DEFAULT_TARGET_NOT_CONVERGED_RETRY_AFTER_SECS,
        crate::domain::test_support::inert_reverse_resolver(Arc::new(NoopMetrics)),
    );

    let resolved = svc.get_plugin().await;
    assert!(
        resolved.is_ok(),
        "expected a resolved scoped handle, got: {:?}",
        resolved.err()
    );
}

// ── PDP dedup pre-pass in `create_usage_records` ───────────────────────────
//
// Records sharing the same attribution tuple
// `(tenant_id, resource_type, resource_id, subject_id, subject_type)` MUST
// collapse to a single PDP `evaluate` round-trip, projected onto every input
// index in the group.
#[cfg(test)]
mod pdp_dedup_tests {
    use std::collections::BTreeMap;
    use std::sync::Arc;
    use toolkit_gts::gts_id;

    use usage_collector_sdk::{
        CreateUsageRecord, EntryType, IdempotencyKey, MeterTypeId, RecordOrigin, ResourceRef,
        SubjectRef, UsageCollectorPluginV1, UsageRecord,
    };
    use uuid::Uuid;

    use crate::domain::test_support::{
        HappyPathPlugin, ServiceFixture, authenticated_ctx, fake_declaration_source_with_fold,
        recent_window_end, recent_window_start,
    };

    const HAPPY_GTS_ID: &str =
        gts_id!("cf.core.uc.usage_record.v1~cf.mini_chat._.tokens_consumed.v1~");

    /// A [`ServiceFixture`] with an arbitrary working declaration (these tests
    /// exercise PDP dedup, not the declaration), exposing the
    /// [`CountingTenantPermitResolver`] handle.
    fn service_with_counting_permit(
        plugin: Arc<dyn UsageCollectorPluginV1>,
        suffix: &str,
    ) -> (
        Arc<crate::domain::Service>,
        Arc<crate::domain::test_support::CountingTenantPermitResolver>,
    ) {
        ServiceFixture::default()
            .with_source(fake_declaration_source_with_fold("SUM"))
            .build_with_default_resolver_handle(plugin, suffix)
    }

    fn persisted_record(tenant_id: Uuid, resource_id: &str, idem: &str) -> UsageRecord {
        UsageRecord {
            id: Uuid::new_v4(),
            gts_type_id: MeterTypeId::new(HAPPY_GTS_ID).expect("valid gts_type_id"),
            tenant_id,
            resource_ref: ResourceRef::new(resource_id, "compute.vm").expect("valid resource ref"),
            subject_ref: None,
            metadata: BTreeMap::new(),
            quantity: crate::domain::test_support::qty("1"),
            idempotency_key: IdempotencyKey::new(idem).expect("valid idempotency key"),
            accepted_at: recent_window_end(),
            origin: RecordOrigin::Live,
            invalidation: None,
            window_start: recent_window_start(),
            window_end: recent_window_end(),
        }
    }

    fn input_record(tenant_id: Uuid, resource_id: &str, idem: &str) -> CreateUsageRecord {
        // Distinct `idem` values keep records distinct even when they share an
        // attribution tuple.
        CreateUsageRecord {
            entry_type: EntryType::Record,
            gts_type_id: MeterTypeId::new(HAPPY_GTS_ID).expect("valid gts_type_id"),
            tenant_id,
            resource_ref: ResourceRef::new(resource_id, "compute.vm").expect("valid resource ref"),
            subject_ref: None,
            metadata: BTreeMap::new(),
            quantity: crate::domain::test_support::qty("1"),
            idempotency_key: Some(IdempotencyKey::new(idem).expect("valid idempotency key")),
            invalidation: None,
            window_start: recent_window_start(),
            window_end: recent_window_end(),
        }
    }

    fn input_record_with_subject(
        tenant_id: Uuid,
        resource_id: &str,
        subject_id: &str,
        idem: &str,
    ) -> CreateUsageRecord {
        let mut r = input_record(tenant_id, resource_id, idem);
        r.subject_ref =
            Some(SubjectRef::new(subject_id, None::<String>).expect("valid subject ref"));
        r
    }

    /// Five records, identical attribution tuple → exactly one PDP
    /// `evaluate` round-trip.
    #[tokio::test]
    async fn create_usage_records_collapses_pdp_calls_for_identical_attribution_tuple() {
        let plugin = HappyPathPlugin::new();
        let tenant_id = Uuid::from_u128(0xAA);
        let input: Vec<CreateUsageRecord> = (0..5)
            .map(|i| input_record(tenant_id, "rsc-shared", &format!("idem-shared-{i}")))
            .collect();

        plugin.set_create_records(
            input
                .iter()
                .map(|r| Ok(persisted_record(r.tenant_id, "rsc-shared", "idem-persist")))
                .collect(),
        );

        let (service, resolver) = service_with_counting_permit(
            Arc::clone(&plugin) as Arc<dyn UsageCollectorPluginV1>,
            "test.pdp_dedup.shared_tuple.records.v1",
        );

        let results = service
            .create_usage_records(&authenticated_ctx(), input)
            .await
            .expect("batch dispatch succeeded");
        assert_eq!(results.len(), 5);
        assert!(
            results.iter().all(Result::is_ok),
            "every record MUST be accepted under a permit-by-default PDP",
        );

        assert_eq!(
            resolver.calls(),
            1,
            "5 records sharing the same attribution tuple MUST collapse to a single PDP evaluate call (intra-batch dedup); observed {} calls",
            resolver.calls(),
        );
    }

    /// Three records, three distinct `resource_id` values → exactly three PDP
    /// calls: one call per distinct tuple, no dedup possible.
    #[tokio::test]
    async fn create_usage_records_issues_one_pdp_call_per_distinct_attribution_tuple() {
        let plugin = HappyPathPlugin::new();
        let tenant_id = Uuid::from_u128(0xBB);
        let input = vec![
            input_record(tenant_id, "rsc-A", "idem-A"),
            input_record(tenant_id, "rsc-B", "idem-B"),
            input_record(tenant_id, "rsc-C", "idem-C"),
        ];

        plugin.set_create_records(
            input
                .iter()
                .map(|r| {
                    Ok(persisted_record(
                        r.tenant_id,
                        r.resource_ref.resource_id(),
                        r.idempotency_key
                            .as_ref()
                            .expect("fixture record carries a key")
                            .as_str(),
                    ))
                })
                .collect(),
        );

        let (service, resolver) = service_with_counting_permit(
            Arc::clone(&plugin) as Arc<dyn UsageCollectorPluginV1>,
            "test.pdp_dedup.distinct_tuples.records.v1",
        );

        let results = service
            .create_usage_records(&authenticated_ctx(), input)
            .await
            .expect("batch dispatch succeeded");
        assert_eq!(results.len(), 3);
        assert!(results.iter().all(Result::is_ok));

        assert_eq!(
            resolver.calls(),
            3,
            "3 distinct attribution tuples MUST produce 3 PDP evaluate calls; observed {} calls",
            resolver.calls(),
        );
    }

    /// Mixed batch (two records share tuple A, two share tuple B) → exactly
    /// two PDP calls. Pins the projection step: the second call's decision
    /// MUST apply to every input index whose tuple key matches.
    #[tokio::test]
    async fn create_usage_records_projects_pdp_decision_across_shared_tuple_groups() {
        let plugin = HappyPathPlugin::new();
        let tenant_id = Uuid::from_u128(0xCC);
        let input = vec![
            input_record(tenant_id, "rsc-A", "idem-A-0"),
            input_record(tenant_id, "rsc-B", "idem-B-0"),
            input_record(tenant_id, "rsc-A", "idem-A-1"),
            input_record(tenant_id, "rsc-B", "idem-B-1"),
        ];

        plugin.set_create_records(
            input
                .iter()
                .map(|r| {
                    Ok(persisted_record(
                        r.tenant_id,
                        r.resource_ref.resource_id(),
                        r.idempotency_key
                            .as_ref()
                            .expect("fixture record carries a key")
                            .as_str(),
                    ))
                })
                .collect(),
        );

        let (service, resolver) = service_with_counting_permit(
            Arc::clone(&plugin) as Arc<dyn UsageCollectorPluginV1>,
            "test.pdp_dedup.mixed_groups.records.v1",
        );

        let results = service
            .create_usage_records(&authenticated_ctx(), input)
            .await
            .expect("batch dispatch succeeded");
        assert_eq!(results.len(), 4);
        assert!(results.iter().all(Result::is_ok));

        assert_eq!(
            resolver.calls(),
            2,
            "2 distinct attribution tuples carrying 4 records MUST produce 2 PDP evaluate calls; observed {} calls",
            resolver.calls(),
        );
    }

    /// Deny-side projection: when PDP denies one tuple group, every input index
    /// in that group MUST surface as a rejected record, and every index in a
    /// permitted group MUST proceed.
    #[tokio::test]
    async fn create_usage_records_projects_pdp_deny_across_shared_tuple_groups() {
        use crate::domain::service::Service;
        use crate::domain::test_support::{DenyOneResourceResolver, enforcer_for, hub_with_plugin};

        let plugin = HappyPathPlugin::new();
        let tenant_id = Uuid::from_u128(0xEE);
        // Two tuple groups, two records each: rsc-DENY and rsc-OK.
        let input = vec![
            input_record(tenant_id, "rsc-DENY", "idem-D-0"),
            input_record(tenant_id, "rsc-OK", "idem-K-0"),
            input_record(tenant_id, "rsc-DENY", "idem-D-1"),
            input_record(tenant_id, "rsc-OK", "idem-K-1"),
        ];

        // Only the two permitted records reach the plugin SPI.
        plugin.set_create_records(
            input
                .iter()
                .filter(|r| r.resource_ref.resource_id() == "rsc-OK")
                .map(|r| {
                    Ok(persisted_record(
                        r.tenant_id,
                        r.resource_ref.resource_id(),
                        r.idempotency_key
                            .as_ref()
                            .expect("fixture record carries a key")
                            .as_str(),
                    ))
                })
                .collect(),
        );

        let hub = hub_with_plugin(
            Arc::clone(&plugin) as Arc<dyn UsageCollectorPluginV1>,
            "test.pdp_dedup.deny_projection.records.v1",
            "constructorfabric",
        );
        let enforcer = enforcer_for(DenyOneResourceResolver::new("rsc-DENY"));
        let type_resolver = std::sync::Arc::new(crate::domain::type_resolver::TypeResolver::new(
            fake_declaration_source_with_fold("SUM"),
            crate::domain::type_resolver::TypeResolverConfig {
                ttl: std::time::Duration::from_mins(1),
                capacity: 16,
            },
            Arc::new(crate::domain::ports::metrics::NoopMetrics),
        ));
        let service = Arc::new(Service::new_with_metrics(
            hub,
            "constructorfabric".to_owned(),
            enforcer,
            Arc::new(crate::domain::ports::metrics::NoopMetrics),
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

        let results = service
            .create_usage_records(&authenticated_ctx(), input)
            .await
            .expect("batch dispatch succeeded - per-record outcomes carry deny");

        assert_eq!(results.len(), 4);
        for (idx, label) in [(0_usize, "rsc-DENY first"), (2, "rsc-DENY second")] {
            let err = results[idx]
                .as_ref()
                .expect_err(&format!("{label} record MUST surface a per-record deny"));
            assert!(
                matches!(
                    err,
                    usage_collector_sdk::UsageCollectorError::PermissionDenied { .. }
                ),
                "{label}: expected PermissionDenied, got {err:?}",
            );
        }
        for (idx, label) in [(1_usize, "rsc-OK first"), (3, "rsc-OK second")] {
            results[idx]
                .as_ref()
                .unwrap_or_else(|err| panic!("{label} record MUST be accepted (got {err:?})"));
        }
    }

    /// Subject-presence asymmetry: a record WITH a `subject_ref` and a record
    /// WITHOUT one MUST be treated as distinct attribution tuples (`subject_id`
    /// is part of the tuple key only when present).
    #[tokio::test]
    async fn create_usage_records_distinguishes_subject_presence_in_tuple_key() {
        let plugin = HappyPathPlugin::new();
        let tenant_id = Uuid::from_u128(0xDD);
        let input = vec![
            input_record(tenant_id, "rsc-X", "idem-X-0"),
            input_record_with_subject(tenant_id, "rsc-X", "subject-1", "idem-X-1"),
            input_record_with_subject(tenant_id, "rsc-X", "subject-1", "idem-X-2"),
        ];

        plugin.set_create_records(
            input
                .iter()
                .map(|r| {
                    Ok(persisted_record(
                        r.tenant_id,
                        r.resource_ref.resource_id(),
                        r.idempotency_key
                            .as_ref()
                            .expect("fixture record carries a key")
                            .as_str(),
                    ))
                })
                .collect(),
        );

        let (service, resolver) = service_with_counting_permit(
            Arc::clone(&plugin) as Arc<dyn UsageCollectorPluginV1>,
            "test.pdp_dedup.subject_presence.records.v1",
        );

        let results = service
            .create_usage_records(&authenticated_ctx(), input)
            .await
            .expect("batch dispatch succeeded");
        assert_eq!(results.len(), 3);
        assert!(results.iter().all(Result::is_ok));

        assert_eq!(
            resolver.calls(),
            2,
            "subject-absent and subject-present records form distinct tuple keys; observed {} calls",
            resolver.calls(),
        );
    }
}

// ── gts_type_id dedup pre-pass in `create_usage_records` ───────────────────
//
// Records sharing the same meter `gts_type_id` MUST collapse to a single Type
// Resolver round-trip, projected onto every input index referencing that id.
#[cfg(test)]
mod gts_type_id_dedup_tests {
    use std::collections::BTreeMap;
    use std::sync::Arc;
    use toolkit_gts::gts_id;

    use usage_collector_sdk::{
        CreateUsageRecord, EntryType, IdempotencyKey, Invalidation, MeterTypeId, RecordOrigin,
        ResourceRef, USAGE_RECORD_RESOURCE, UsageCollectorError, UsageCollectorPluginV1,
        UsageRecord,
    };
    use uuid::Uuid;

    use crate::domain::test_support::{
        HappyPathPlugin, ServiceFixture, authenticated_ctx, fake_declaration_source_counting,
        fake_declaration_source_with_one_unresolvable, recent_window_end, recent_window_start,
    };

    const GTS_A: &str = gts_id!("cf.core.uc.usage_record.v1~cf.mini_chat._.tokens_consumed.v1~");
    const GTS_B: &str = gts_id!("cf.core.uc.usage_record.v1~cf.mini_chat._.tokens_emitted.v1~");
    const GTS_C: &str = gts_id!("cf.core.uc.usage_record.v1~cf.mini_chat._.tokens_buffered.v1~");

    fn meter_id_for(gts: &str) -> MeterTypeId {
        MeterTypeId::new(gts).expect("valid meter type id")
    }

    fn record_for(gts: &str, tenant_id: Uuid, idem: &str) -> CreateUsageRecord {
        CreateUsageRecord {
            entry_type: EntryType::Record,
            gts_type_id: meter_id_for(gts),
            tenant_id,
            resource_ref: ResourceRef::new("rsc-gts-dedup", "compute.vm")
                .expect("valid resource ref"),
            subject_ref: None,
            metadata: BTreeMap::new(),
            quantity: crate::domain::test_support::qty("1"),
            idempotency_key: Some(IdempotencyKey::new(idem).expect("valid idempotency key")),
            invalidation: None,
            window_start: recent_window_start(),
            window_end: recent_window_end(),
        }
    }

    fn persisted_for(input: &CreateUsageRecord) -> UsageRecord {
        UsageRecord {
            id: Uuid::new_v4(),
            gts_type_id: input.gts_type_id.clone(),
            tenant_id: input.tenant_id,
            resource_ref: input.resource_ref.clone(),
            subject_ref: input.subject_ref.clone(),
            metadata: input.metadata.clone(),
            quantity: input.quantity,
            idempotency_key: input
                .idempotency_key
                .clone()
                .expect("fixture record carries a key"),
            accepted_at: input.window_end,
            origin: RecordOrigin::Live,
            // The submission states the reason alone; the persisted shape
            // pairs it with the gateway-derived target.
            invalidation: input.invalidation.clone().map(|reason| Invalidation {
                target: crate::domain::invalidation::derive_invalidation_target(input)
                    .expect("fixture withdrawal carries a key"),
                reason,
            }),
            window_start: input.window_start,
            window_end: input.window_end,
        }
    }

    // The "5 records, identical gts_type_id → exactly one resolution" case
    // lives in `ingestion_declared_type_tests::a_batch_resolves_each_distinct_type_once`.

    /// Three records, three distinct `gts_type_id`s → exactly three
    /// declaration resolutions (one per distinct id), asking the resolver
    /// for exactly the distinct ids in the batch.
    #[tokio::test]
    async fn create_usage_records_issues_one_resolution_per_distinct_gts_type_id() {
        let plugin = HappyPathPlugin::new();
        let source = fake_declaration_source_counting();

        let tenant_id = Uuid::from_u128(0xFF);
        let input = vec![
            record_for(GTS_A, tenant_id, "idem-A"),
            record_for(GTS_B, tenant_id, "idem-B"),
            record_for(GTS_C, tenant_id, "idem-C"),
        ];

        plugin.set_create_records(input.iter().map(|r| Ok(persisted_for(r))).collect());

        let service = ServiceFixture::default().with_source(source.clone()).build(
            Arc::clone(&plugin) as Arc<dyn UsageCollectorPluginV1>,
            "test.gts_dedup.distinct.records.v1",
        );

        let results = service
            .create_usage_records(&authenticated_ctx(), input)
            .await
            .expect("batch dispatch succeeded");
        assert_eq!(results.len(), 3);
        assert!(results.iter().all(Result::is_ok));

        assert_eq!(
            source.fetch_calls(),
            3,
            "3 distinct gts_type_ids MUST produce 3 declaration resolutions; \
             observed {} calls",
            source.fetch_calls(),
        );

        let mut seen: Vec<String> = source
            .fetch_inputs()
            .into_iter()
            .map(|id| id.to_string())
            .collect();
        seen.sort();
        let mut expected = vec![
            meter_id_for(GTS_A).to_string(),
            meter_id_for(GTS_B).to_string(),
            meter_id_for(GTS_C).to_string(),
        ];
        expected.sort();
        assert_eq!(
            seen, expected,
            "the deduped fan-out MUST ask the resolver for exactly the distinct meter ids",
        );
    }

    /// Mixed batch: `gts_type_id` A resolves, `gts_type_id` B does not.
    /// Records sharing the unresolvable id are all rejected with
    /// `NotFound`, records sharing the resolvable id are accepted.
    #[tokio::test]
    async fn create_usage_records_projects_not_found_to_every_record_sharing_unknown_gts_type_id() {
        let plugin = HappyPathPlugin::new();
        let source = fake_declaration_source_with_one_unresolvable(meter_id_for(GTS_B));

        let tenant_id = Uuid::from_u128(0x11);
        let input = vec![
            record_for(GTS_A, tenant_id, "idem-A-0"),
            record_for(GTS_B, tenant_id, "idem-B-0"),
            record_for(GTS_A, tenant_id, "idem-A-1"),
            record_for(GTS_B, tenant_id, "idem-B-1"),
        ];

        // Only the two resolvable-gts_type_id records reach the SPI.
        let accepted: Vec<_> = input
            .iter()
            .filter(|r| r.gts_type_id.to_string() == GTS_A)
            .map(|r| Ok(persisted_for(r)))
            .collect();
        plugin.set_create_records(accepted);

        let service = ServiceFixture::default()
            .with_source(Arc::clone(&source)
                as Arc<dyn crate::domain::ports::declarations::DeclarationSource>)
            .build(
                Arc::clone(&plugin) as Arc<dyn UsageCollectorPluginV1>,
                "test.gts_dedup.mixed.records.v1",
            );

        let results = service
            .create_usage_records(&authenticated_ctx(), input)
            .await
            .expect("batch dispatch succeeded");
        assert_eq!(results.len(), 4);

        assert!(
            results[0].is_ok(),
            "record at index 0 (gts_type_id A) MUST be accepted, got {:?}",
            results[0],
        );
        assert!(
            results[2].is_ok(),
            "record at index 2 (gts_type_id A) MUST be accepted, got {:?}",
            results[2],
        );
        let expected_name = meter_id_for(GTS_B).to_string();
        for idx in [1usize, 3usize] {
            match results[idx].as_ref() {
                Err(UsageCollectorError::NotFound {
                    resource_type,
                    name,
                    ..
                }) => {
                    assert_eq!(resource_type, USAGE_RECORD_RESOURCE);
                    assert_eq!(
                        name, &expected_name,
                        "record at index {idx} MUST be rejected with NotFound \
                         carrying the unresolvable meter id",
                    );
                }
                other => panic!(
                    "record at index {idx} (gts_type_id B) MUST surface NotFound, \
                     got {other:?}",
                ),
            }
        }

        assert_eq!(
            source.fetch_calls(),
            2,
            "2 distinct gts_type_ids carrying 4 records MUST produce 2 declaration \
             resolutions; observed {} calls",
            source.fetch_calls(),
        );
    }
}

// ── invalidation-target pre-check in `create_usage_records` ───────────────
//
// Entries naming the same target MUST collapse to a single `get_usage_record`
// SPI round-trip, an ordinary measurement MUST NOT trigger a target read at
// all, and the per-index pairing the fan-out produces must stay correct.
#[cfg(test)]
mod invalidation_target_batch_tests {
    use std::collections::BTreeMap;
    use std::sync::Arc;
    use toolkit_gts::gts_id;

    use usage_collector_sdk::{
        ConflictOutcome, CreateUsageRecord, EntryType, IdempotencyKey, Invalidation, MetadataKey,
        MeterTypeId, ReasonCode, ResourceRef, USAGE_RECORD_RESOURCE, UsageCollectorError,
        UsageCollectorPluginError, UsageCollectorPluginV1, UsageRecord, ValidationReason,
    };
    use uuid::Uuid;

    use super::assert_translatable_scope;
    use crate::domain::Service;
    use crate::domain::test_support::{
        HappyPathPlugin, ServiceFixture, UncompilableSiblingPermitResolver, authenticated_ctx,
        counter_sum_with_label, fake_declaration_source_with_fold,
        fake_declaration_source_with_metadata, projected, recent_window_end, recent_window_start,
    };

    const COUNTER_GTS_ID: &str =
        gts_id!("cf.core.uc.usage_record.v1~cf.mini_chat._.tokens_consumed.v1~");

    /// A [`ServiceFixture`] with an arbitrary working declaration — these
    /// tests exercise the target pre-check, not the declaration.
    fn service_with_permit(plugin: Arc<dyn UsageCollectorPluginV1>, suffix: &str) -> Arc<Service> {
        ServiceFixture::default()
            .with_source(fake_declaration_source_with_fold("SUM"))
            .build(plugin, suffix)
    }

    /// The base measurement every fixture here is shaped from. An
    /// invalidation is a faithful copy of it, so a withdrawal is built by
    /// changing exactly the two permitted departures — `entry_type` and the
    /// reason code.
    fn ordinary_record(tenant_id: Uuid, idem: &str) -> CreateUsageRecord {
        CreateUsageRecord {
            entry_type: EntryType::Record,
            gts_type_id: MeterTypeId::new(COUNTER_GTS_ID).expect("valid gts_type_id"),
            tenant_id,
            resource_ref: ResourceRef::new("rsc-target", "compute.vm").expect("valid resource ref"),
            subject_ref: None,
            metadata: BTreeMap::new(),
            quantity: crate::domain::test_support::qty("10"),
            idempotency_key: Some(IdempotencyKey::new(idem).expect("valid idempotency key")),
            invalidation: None,
            window_start: recent_window_start(),
            window_end: recent_window_end(),
        }
    }

    /// The withdrawal of `ordinary_record(tenant_id, idem)`.
    ///
    /// It names no target: the shape has no field for one. It repeats every
    /// identity input of the entry it withdraws, idempotency key included, and
    /// the gateway derives the target from those. Passing `idem` therefore
    /// chooses *which* entry is withdrawn; [`target_id`] says which.
    fn withdrawal_of(tenant_id: Uuid, idem: &str) -> CreateUsageRecord {
        CreateUsageRecord {
            entry_type: EntryType::Invalidation,
            invalidation: Some(ReasonCode::new("emitter_defect").expect("valid reason code")),
            ..ordinary_record(tenant_id, idem)
        }
    }

    /// The persisted measurement `withdrawal_of(tenant_id, idem)` withdraws:
    /// the projection of the very submission that withdrawal copies, so the
    /// pair cannot drift apart.
    fn target_row(tenant_id: Uuid, idem: &str) -> UsageRecord {
        projected(&ordinary_record(tenant_id, idem))
    }

    /// The identifier `withdrawal_of(tenant_id, idem)` resolves.
    fn target_id(tenant_id: Uuid, idem: &str) -> Uuid {
        target_row(tenant_id, idem).id
    }

    /// Five withdrawals of one target MUST collapse to a single
    /// `get_usage_record` SPI round-trip.
    #[tokio::test]
    async fn withdrawals_of_one_target_share_a_single_lookup() {
        let plugin = HappyPathPlugin::new();
        let tenant_id = Uuid::from_u128(0x501);
        plugin.set_get_record(target_row(tenant_id, "idem-w"));

        let input: Vec<CreateUsageRecord> =
            (0..5).map(|_| withdrawal_of(tenant_id, "idem-w")).collect();
        // All five share one dedup identity, so the batch dispatches a single
        // representative; the mock is programmed with one outcome to match.
        let stored = projected(&input[0]);
        plugin.set_create_records(vec![Ok(stored.clone())]);

        let service = service_with_permit(
            Arc::clone(&plugin) as Arc<dyn UsageCollectorPluginV1>,
            "test.target.shared.records.v1",
        );

        let results = service
            .create_usage_records(&authenticated_ctx(), input)
            .await
            .expect("batch dispatch succeeded");
        assert_eq!(results.len(), 5);
        assert!(
            results.iter().all(Result::is_ok),
            "every faithful withdrawal MUST be accepted by the gateway: {results:?}",
        );

        assert_eq!(
            plugin.get_usage_record_calls(),
            1,
            "5 withdrawals of one target MUST collapse to a single \
             get_usage_record SPI dispatch; observed {} calls",
            plugin.get_usage_record_calls(),
        );
        assert_eq!(
            plugin
                .last_create_records_input()
                .expect("dispatched")
                .len(),
            1,
            "one dispatch per identity: 5 withdrawals sharing one dedup identity \
             collapse to a single create_usage_records dispatch",
        );

        assert_translatable_scope(&plugin, "resolve_invalidation_targets");
    }

    /// Three distinct targets MUST produce three `get_usage_record`
    /// dispatches, for exactly those three ids.
    #[tokio::test]
    async fn distinct_targets_each_cost_their_own_lookup() {
        let plugin = HappyPathPlugin::new();
        let tenant_id = Uuid::from_u128(0x502);

        let target_a = target_id(tenant_id, "idem-A");
        let target_b = target_id(tenant_id, "idem-B");
        let target_c = target_id(tenant_id, "idem-C");
        for idem in ["idem-A", "idem-B", "idem-C"] {
            plugin.set_get_record_for(target_id(tenant_id, idem), target_row(tenant_id, idem));
        }

        let input = vec![
            withdrawal_of(tenant_id, "idem-A"),
            withdrawal_of(tenant_id, "idem-B"),
            withdrawal_of(tenant_id, "idem-C"),
        ];
        plugin.set_create_records(input.iter().map(|r| Ok(projected(r))).collect());

        let service = service_with_permit(
            Arc::clone(&plugin) as Arc<dyn UsageCollectorPluginV1>,
            "test.target.distinct.records.v1",
        );

        let results = service
            .create_usage_records(&authenticated_ctx(), input)
            .await
            .expect("batch dispatch succeeded");
        assert_eq!(results.len(), 3);
        assert!(results.iter().all(Result::is_ok), "{results:?}");

        assert_eq!(
            plugin.get_usage_record_calls(),
            3,
            "3 distinct targets MUST produce 3 get_usage_record SPI dispatches; \
             observed {} calls",
            plugin.get_usage_record_calls(),
        );

        let mut seen = plugin.get_usage_record_inputs();
        seen.sort();
        let mut expected = vec![target_a, target_b, target_c];
        expected.sort();
        assert_eq!(
            seen, expected,
            "the deduped fan-out MUST ask the plugin for exactly the distinct targets",
        );
    }

    /// An ordinary measurement MUST cost no target lookup at all. The
    /// *absence* of the dispatch is the pin: a gateway that looked up
    /// unconditionally would pass every other test here while doubling the
    /// plugin traffic of the common path.
    #[tokio::test]
    async fn an_ordinary_record_costs_no_target_lookup() {
        let plugin = HappyPathPlugin::new();
        let tenant_id = Uuid::from_u128(0x503);
        let input: Vec<CreateUsageRecord> = (0..5)
            .map(|i| ordinary_record(tenant_id, &format!("idem-ord-{i}")))
            .collect();
        plugin.set_create_records(input.iter().map(|r| Ok(projected(r))).collect());

        let service = service_with_permit(
            Arc::clone(&plugin) as Arc<dyn UsageCollectorPluginV1>,
            "test.target.no_invalidates.records.v1",
        );

        let results = service
            .create_usage_records(&authenticated_ctx(), input)
            .await
            .expect("batch dispatch succeeded");
        assert_eq!(results.len(), 5);
        assert!(results.iter().all(Result::is_ok), "{results:?}");

        assert_eq!(
            plugin.get_usage_record_calls(),
            0,
            "an entry carrying no `invalidates` MUST NOT trigger a target lookup; \
             observed {} get_usage_record dispatches",
            plugin.get_usage_record_calls(),
        );
    }

    /// A target that resolves to nothing MUST reject every entry naming it, at
    /// its own input index, while an entry naming a resolvable target in the
    /// same batch is still accepted. The rejections sit at indices 0 and 2
    /// with the acceptance between them, so a projection that wrote every
    /// outcome to one slot cannot pass.
    #[tokio::test]
    async fn an_unresolvable_target_rejects_every_entry_naming_it() {
        let plugin = HappyPathPlugin::new();
        let tenant_id = Uuid::from_u128(0x504);

        let good = target_id(tenant_id, "idem-good");
        let missing = target_id(tenant_id, "idem-bad");
        plugin.set_get_record_for(good, target_row(tenant_id, "idem-good"));
        plugin.set_get_usage_record_not_found(missing);

        // Indices 0 and 2 withdraw the same entry, so they repeat one key and
        // resolve one identifier.
        let input = vec![
            withdrawal_of(tenant_id, "idem-bad"),
            withdrawal_of(tenant_id, "idem-good"),
            withdrawal_of(tenant_id, "idem-bad"),
        ];

        // Only the resolvable one reaches the persist SPI.
        plugin.set_create_records(vec![Ok(projected(&input[1]))]);

        let service = service_with_permit(
            Arc::clone(&plugin) as Arc<dyn UsageCollectorPluginV1>,
            "test.target.mixed_not_found.records.v1",
        );

        let results = service
            .create_usage_records(&authenticated_ctx(), input)
            .await
            .expect("batch dispatch succeeded");
        assert_eq!(results.len(), 3);

        assert!(
            results[1].is_ok(),
            "the entry naming a resolvable target MUST be accepted, got {:?}",
            results[1],
        );
        for idx in [0usize, 2] {
            match results[idx].as_ref() {
                Err(UsageCollectorError::NotFound {
                    resource_type,
                    name,
                    detail,
                    ..
                }) if resource_type == USAGE_RECORD_RESOURCE => {
                    assert_eq!(
                        name,
                        &missing.to_string(),
                        "index {idx} MUST name the unresolvable target",
                    );
                    assert!(
                        detail.contains(&missing.to_string()),
                        "index {idx} detail MUST carry the unresolvable target",
                    );
                }
                other => panic!("index {idx} MUST be rejected as NotFound, got {other:?}"),
            }
        }

        assert_eq!(
            plugin.get_usage_record_calls(),
            2,
            "2 distinct targets carrying 3 entries MUST produce 2 get_usage_record \
             dispatches; observed {} calls",
            plugin.get_usage_record_calls(),
        );
    }

    /// Two withdrawals of **different** targets, resolved in one batch, MUST
    /// each be told about the target *they* named. The comparator trusts that
    /// the row and reference it is handed belong together, so the fan-out —
    /// keyed by target, projected back by input index — is the only place they
    /// can come apart. The entries fail in different *kinds* of way, so a swap
    /// of key or index changes which reason lands where.
    #[tokio::test]
    async fn each_rejection_names_the_target_its_own_entry_sent() {
        let plugin = HappyPathPlugin::new();
        let tenant_id = Uuid::from_u128(0x505);

        // Target A resolves to nothing at all.
        let target_a = target_id(tenant_id, "idem-pair-a");
        plugin.set_get_usage_record_not_found(target_a);

        // Target B exists, and the submission copies it unfaithfully.
        let target_b = target_id(tenant_id, "idem-pair-b");
        let mut row_b = target_row(tenant_id, "idem-pair-b");
        row_b.quantity = crate::domain::test_support::qty("999");
        plugin.set_get_record_for(target_b, row_b);

        let input = vec![
            withdrawal_of(tenant_id, "idem-pair-a"),
            withdrawal_of(tenant_id, "idem-pair-b"),
        ];

        let service = service_with_permit(
            Arc::clone(&plugin) as Arc<dyn UsageCollectorPluginV1>,
            "test.target.pairing.records.v1",
        );

        let results = service
            .create_usage_records(&authenticated_ctx(), input)
            .await
            .expect("batch dispatch succeeded");
        assert_eq!(results.len(), 2);

        match results[0].as_ref() {
            Err(UsageCollectorError::NotFound { name, .. }) => assert_eq!(
                name,
                &target_a.to_string(),
                "index 0 MUST be told about the target IT resolved",
            ),
            other => panic!("index 0 MUST be refused as a missing target, got {other:?}"),
        }
        match results[1].as_ref() {
            Err(UsageCollectorError::InvalidArgument {
                reason: ValidationReason::InvalidationFieldMismatch,
                field,
                resource_name,
                ..
            }) => {
                assert_eq!(field, "quantity", "the field that differs MUST be named");
                assert_eq!(
                    resource_name.as_deref(),
                    Some(target_b.to_string().as_str()),
                    "index 1 MUST be told about the target IT resolved",
                );
            }
            other => panic!("index 1 MUST be refused as an unfaithful copy, got {other:?}"),
        }
    }

    /// A submission that breaks the copy rule **and** the metadata rule is
    /// told about the copy.
    ///
    /// Error priority: the target rules run before the declaration's. The
    /// metadata a caller would be told to fix is metadata it has to copy
    /// from the target regardless, so telling it about the metadata first
    /// sends it to fix the wrong thing.
    #[tokio::test]
    async fn a_copy_mismatch_outranks_a_metadata_rejection() {
        let plugin = HappyPathPlugin::new();
        let tenant_id = Uuid::from_u128(0x506);
        let target = target_id(tenant_id, "idem-both-broken");

        // The declaration declares `region` alone, so `zone` is undeclared and
        // fails the closed-shape check. `metadata` is no identity input, so
        // adding it below leaves the resolved target unchanged.
        let mut row = target_row(tenant_id, "idem-both-broken");
        row.quantity = crate::domain::test_support::qty("999");
        plugin.set_get_record_for(target, row);

        let mut submission = withdrawal_of(tenant_id, "idem-both-broken");
        submission.metadata.insert(
            MetadataKey::new("zone").expect("valid metadata key"),
            "eu-1".to_owned(),
        );

        let service = ServiceFixture::default()
            .with_source(fake_declaration_source_with_metadata(&["region"]))
            .build(
                Arc::clone(&plugin) as Arc<dyn UsageCollectorPluginV1>,
                "test.target.priority.records.v1",
            );

        let results = service
            .create_usage_records(&authenticated_ctx(), vec![submission])
            .await
            .expect("batch dispatch succeeded");

        match results[0].as_ref() {
            Err(UsageCollectorError::InvalidArgument { reason, .. }) => assert_eq!(
                *reason,
                ValidationReason::InvalidationFieldMismatch,
                "the copy rejection MUST outrank the metadata one",
            ),
            other => panic!("a submission breaking both rules MUST be rejected, got {other:?}"),
        }
    }

    /// A second withdrawal under another reason code reaches the same dedup
    /// identity: `reason_code` is no identity input, so two withdrawals of
    /// one entry agree on all six. The store reports an ordinary conflict
    /// carrying the stored invalidation, and the gateway reports it as
    /// `AlreadyInvalidated` because the dispatched entry is an invalidation.
    #[tokio::test]
    async fn a_store_conflict_on_a_withdrawal_is_lifted_as_already_invalidated() {
        let plugin = HappyPathPlugin::new();
        let tenant_id = Uuid::from_u128(0x507);
        let target = target_id(tenant_id, "idem-withdrawn");
        plugin.set_get_record_for(target, target_row(tenant_id, "idem-withdrawn"));
        let stored = projected(&withdrawal_of(tenant_id, "idem-withdrawn"));
        let stored_id = stored.id;
        // The conflict carries the key the withdrawal was dispatched under —
        // its target's own; no prefix is reserved.
        plugin.set_create_records(vec![Err(UsageCollectorPluginError::idempotency_conflict(
            "idem-withdrawn",
            crate::domain::test_support::as_stored(stored),
        ))]);
        let service = service_with_permit(
            Arc::clone(&plugin) as Arc<dyn UsageCollectorPluginV1>,
            "test.target.already_invalidated.records.v1",
        );

        let mut second = withdrawal_of(tenant_id, "idem-withdrawn");
        second.invalidation = Some(ReasonCode::new("late_correction").expect("valid reason code"));

        let results = service
            .create_usage_records(&authenticated_ctx(), vec![second])
            .await
            .expect("batch dispatch succeeded");

        match results[0].as_ref() {
            Err(UsageCollectorError::Conflict {
                outcome:
                    ConflictOutcome::AlreadyInvalidated {
                        invalidated_by,
                        reason_code,
                    },
                name,
                ..
            }) => {
                assert_eq!(name, &target.to_string());
                assert_eq!(*invalidated_by, stored_id);
                assert_eq!(reason_code.as_str(), "emitter_defect");
            }
            other => panic!(
                "a conflict on a dispatched invalidation is AlreadyInvalidated, got {other:?}"
            ),
        }
    }

    #[tokio::test]
    async fn a_store_conflict_on_a_record_stays_an_idempotency_conflict() {
        let plugin = HappyPathPlugin::new();
        let tenant_id = Uuid::from_u128(0x508);
        let stored = projected(&ordinary_record(tenant_id, "idem-conflict"));
        let stored_id = stored.id;
        plugin.set_create_records(vec![Err(UsageCollectorPluginError::idempotency_conflict(
            "idem-conflict",
            crate::domain::test_support::as_stored(stored),
        ))]);
        let service = service_with_permit(
            Arc::clone(&plugin) as Arc<dyn UsageCollectorPluginV1>,
            "test.record.idempotency_conflict.records.v1",
        );
        let mut divergent = ordinary_record(tenant_id, "idem-conflict");
        divergent.quantity = crate::domain::test_support::qty("11");

        let results = service
            .create_usage_records(&authenticated_ctx(), vec![divergent])
            .await
            .expect("batch dispatch succeeded");

        match results[0].as_ref() {
            Err(UsageCollectorError::Conflict {
                outcome: ConflictOutcome::IdempotencyConflict,
                name,
                ..
            }) => assert_eq!(name, &stored_id.to_string()),
            other => panic!("a conflict on a record stays IdempotencyConflict, got {other:?}"),
        }
    }

    /// A backend fault on the target read MUST fail the submission, not
    /// reject it: an unreadable target is not an absent one, and a caller
    /// told `NotFound` would stop retrying a withdrawal that is perfectly
    /// valid.
    #[tokio::test]
    async fn a_transient_target_lookup_fails_the_entry_rather_than_rejecting_it() {
        let plugin = HappyPathPlugin::new();
        let tenant_id = Uuid::from_u128(0x508);
        plugin.set_get_usage_record_transient("target store timed out", Some(7));

        let service = service_with_permit(
            Arc::clone(&plugin) as Arc<dyn UsageCollectorPluginV1>,
            "test.target.transient.records.v1",
        );

        let results = service
            .create_usage_records(
                &authenticated_ctx(),
                vec![withdrawal_of(tenant_id, "idem-transient")],
            )
            .await
            .expect("batch dispatch succeeded");

        match results[0].as_ref() {
            Err(UsageCollectorError::ServiceUnavailable {
                detail,
                retry_after_seconds,
            }) => {
                assert_eq!(detail, "target store timed out");
                assert_eq!(*retry_after_seconds, Some(7));
            }
            other => panic!("a transient target read MUST NOT become a rejection, got {other:?}"),
        }
    }

    /// The plugin MUST see the batch in submission order, even though the
    /// target pre-check pushes its verified entries onto the end of
    /// `eligible`. Per-entry results are routed back by input index either
    /// way, so `last_create_records_input` is the only place the property is
    /// observable; the invalidation is at index 0, so removing the sort
    /// reverses the pair rather than leaving it untouched.
    #[tokio::test]
    async fn the_plugin_sees_the_batch_in_submission_order() {
        let plugin = HappyPathPlugin::new();
        let tenant_id = Uuid::from_u128(0x50A);
        plugin.set_get_record_for(
            target_id(tenant_id, "idem-order-withdrawal"),
            target_row(tenant_id, "idem-order-withdrawal"),
        );

        let input = vec![
            withdrawal_of(tenant_id, "idem-order-withdrawal"),
            ordinary_record(tenant_id, "idem-order-plain"),
        ];
        plugin.set_create_records(input.iter().map(|r| Ok(projected(r))).collect());
        let expected_ids: Vec<Uuid> = input.iter().map(|r| projected(r).id).collect();

        let service = service_with_permit(
            Arc::clone(&plugin) as Arc<dyn UsageCollectorPluginV1>,
            "test.target.order.records.v1",
        );

        let results = service
            .create_usage_records(&authenticated_ctx(), input)
            .await
            .expect("batch dispatch succeeded");
        let result_ids: Vec<Uuid> = results
            .iter()
            .map(|result| result.as_ref().expect("accepted").id)
            .collect();
        assert_eq!(
            result_ids, expected_ids,
            "each result is the row for the input at its own position",
        );

        let dispatched = plugin
            .last_create_records_input()
            .expect("the batch reached the persist SPI");
        let keys: Vec<&str> = dispatched
            .iter()
            .map(|r| r.idempotency_key.as_str())
            .collect();
        assert_eq!(
            keys,
            vec!["idem-order-withdrawal", "idem-order-plain"],
            "the deferred target pre-check MUST NOT reorder the batch the \
             plugin is handed, and a withdrawal is dispatched under its \
             target's own key",
        );
    }

    /// A transient on one entry's target MUST NOT abandon the entries
    /// behind it in the pending list.
    ///
    /// The backend-fault arm projects per entry and continues; a version that
    /// returned instead would leave every later slot unfilled. The unreadable
    /// target is first, so the survivors are exactly what a `return` drops.
    #[tokio::test]
    async fn a_transient_on_one_target_does_not_abandon_the_rest_of_the_batch() {
        let plugin = HappyPathPlugin::new();
        let tenant_id = Uuid::from_u128(0x50B);

        let unreadable = target_id(tenant_id, "idem-iso-unreadable");
        plugin.set_get_usage_record_transient_for(unreadable, "target store timed out", Some(7));
        plugin.set_get_record_for(
            target_id(tenant_id, "idem-iso-readable"),
            target_row(tenant_id, "idem-iso-readable"),
        );

        let input = vec![
            withdrawal_of(tenant_id, "idem-iso-unreadable"),
            withdrawal_of(tenant_id, "idem-iso-readable"),
        ];
        plugin.set_create_records(vec![Ok(projected(&input[1]))]);

        let service = service_with_permit(
            Arc::clone(&plugin) as Arc<dyn UsageCollectorPluginV1>,
            "test.target.isolation.records.v1",
        );

        let results = service
            .create_usage_records(&authenticated_ctx(), input)
            .await
            .expect("batch dispatch succeeded");
        assert_eq!(results.len(), 2);

        match results[0].as_ref() {
            Err(UsageCollectorError::ServiceUnavailable { detail, .. }) => {
                assert_eq!(detail, "target store timed out");
            }
            other => panic!("index 0's target was unreadable, got {other:?}"),
        }
        assert!(
            results[1].is_ok(),
            "a backend fault on one entry's target MUST NOT reject — or drop — \
             an entry whose own target read succeeded: {:?}",
            results[1],
        );
    }

    /// A batch MUST label each entry with **its own** `entry_type`.
    ///
    /// The label is captured per input index before the pipeline consumes the
    /// submissions and re-joined to the outcomes by `zip` afterwards; that
    /// join is the only place a label can come apart from its entry, and no
    /// single-emit test reaches it.
    #[tokio::test]
    async fn a_batch_labels_each_entry_with_its_own_entry_type() {
        let plugin = HappyPathPlugin::new();
        let tenant_id = Uuid::from_u128(0x50C);
        plugin.set_get_record_for(
            target_id(tenant_id, "idem-label-withdrawal"),
            target_row(tenant_id, "idem-label-withdrawal"),
        );

        let input = vec![
            ordinary_record(tenant_id, "idem-label-plain"),
            withdrawal_of(tenant_id, "idem-label-withdrawal"),
        ];
        plugin.set_create_records(input.iter().map(|r| Ok(projected(r))).collect());

        let (service, provider, exporter) = ServiceFixture::default()
            .with_source(fake_declaration_source_with_fold("SUM"))
            .build_with_metrics(
                Arc::clone(&plugin) as Arc<dyn UsageCollectorPluginV1>,
                "test.target.batch_label.records.v1",
            );

        let results = service
            .create_usage_records(&authenticated_ctx(), input)
            .await
            .expect("batch dispatch succeeded");
        assert!(results.iter().all(Result::is_ok), "{results:?}");
        provider.force_flush().unwrap();

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
                "entry_type",
                "invalidation",
            ),
            1,
        );
    }

    /// A plugin answering `get_usage_record(a)` with a *different* row MUST be
    /// refused, not believed. The submission is a faithful copy of the row the
    /// plugin returns, so without the id check every other rule passes and the
    /// entry is **accepted**. A host-invariant breach, so `Internal`, and the
    /// message names only the id the caller sent — the row came back from an
    /// unscoped read and its own identity must not cross back.
    #[tokio::test]
    async fn a_plugin_answering_with_the_wrong_row_is_refused_not_believed() {
        let plugin = HappyPathPlugin::new();
        let tenant_id = Uuid::from_u128(0x50D);
        let requested = target_id(tenant_id, "idem-wrong-row");
        let answered = target_id(tenant_id, "idem-other-row");

        // Asked for `requested`, answered with the row for `answered`. The two
        // rows differ in their idempotency key alone, which the copy
        // comparator does not read — so only the id check can catch this.
        plugin.set_get_record_for(requested, target_row(tenant_id, "idem-other-row"));

        // The persist SPI is programmed to succeed even though a correct
        // gateway never reaches it, so a gateway that admitted the entry fails
        // visibly as `Ok(..)` rather than on an unprogrammed SPI.
        let submission = withdrawal_of(tenant_id, "idem-wrong-row");
        plugin.set_create_records(vec![Ok(projected(&submission))]);

        let service = service_with_permit(
            Arc::clone(&plugin) as Arc<dyn UsageCollectorPluginV1>,
            "test.target.wrong_row.records.v1",
        );

        let results = service
            .create_usage_records(&authenticated_ctx(), vec![submission])
            .await
            .expect("batch dispatch succeeded");

        match results[0].as_ref() {
            Err(UsageCollectorError::Internal { detail }) => {
                assert!(
                    detail.contains(&requested.to_string()),
                    "the breach MUST name the id the caller sent: {detail}",
                );
                assert!(
                    !detail.contains(&answered.to_string()),
                    "the breach MUST NOT echo the row's own identity: {detail}",
                );
            }
            other => panic!(
                "a mis-answering plugin MUST NOT have its row believed — this \
                 submission copies it faithfully and would otherwise be \
                 accepted; got {other:?}",
            ),
        }
        assert!(
            plugin.last_create_records_input().is_none(),
            "the breach MUST short-circuit before the persist SPI",
        );
    }

    /// A plugin answering the derived identifier with a row that is itself an
    /// invalidation MUST be refused, not believed: the identifier was derived
    /// with `entry_type = record`, so such a row contradicts it. The row is a
    /// faithful copy in every compared field, so without this guard the entry
    /// is **accepted** and a withdrawal of a withdrawal is persisted. A
    /// host-invariant breach, so `Internal`, naming only the identifier the
    /// gateway asked for.
    #[tokio::test]
    async fn a_plugin_answering_with_an_invalidation_row_is_refused_not_believed() {
        let plugin = HappyPathPlugin::new();
        let tenant_id = Uuid::from_u128(0x50E);
        let target = target_id(tenant_id, "idem-corrupt-row");
        let mut row = target_row(tenant_id, "idem-corrupt-row");
        row.invalidation = Some(Invalidation {
            target: Uuid::from_u128(0x61F),
            reason: ReasonCode::new("emitter_defect").expect("valid reason code"),
        });
        assert_eq!(
            row.id, target,
            "test premise: the row answers the derived id"
        );
        plugin.set_get_record_for(target, row);

        // Programmed so a gateway that believed the row fails visibly as
        // `Ok(..)` rather than on an unprogrammed SPI.
        let submission = withdrawal_of(tenant_id, "idem-corrupt-row");
        plugin.set_create_records(vec![Ok(projected(&submission))]);

        let service = service_with_permit(
            Arc::clone(&plugin) as Arc<dyn UsageCollectorPluginV1>,
            "test.target.invalidation_row.records.v1",
        );

        let results = service
            .create_usage_records(&authenticated_ctx(), vec![submission])
            .await
            .expect("batch dispatch succeeded");

        match results[0].as_ref() {
            Err(UsageCollectorError::Internal { detail }) => assert!(
                detail.contains(&target.to_string()),
                "the breach MUST name the id the gateway asked for: {detail}",
            ),
            other => {
                panic!("a row contradicting its own identifier MUST NOT be believed; got {other:?}")
            }
        }
        assert!(
            plugin.last_create_records_input().is_none(),
            "the breach MUST short-circuit before the persist SPI",
        );
    }

    /// One measurement, one good withdrawal and one bad withdrawal MUST
    /// come back index-aligned, with only the bad one rejected — and the
    /// bad one is last, so a projection that wrote to a fixed slot could
    /// not pass.
    #[tokio::test]
    async fn a_mixed_batch_rejects_only_the_entry_that_broke_a_rule() {
        let plugin = HappyPathPlugin::new();
        let tenant_id = Uuid::from_u128(0x509);

        plugin.set_get_record_for(
            target_id(tenant_id, "idem-mixed-good"),
            target_row(tenant_id, "idem-mixed-good"),
        );
        let mut row_bad = target_row(tenant_id, "idem-mixed-bad");
        row_bad.quantity = crate::domain::test_support::qty("999");
        plugin.set_get_record_for(target_id(tenant_id, "idem-mixed-bad"), row_bad);

        let input = vec![
            ordinary_record(tenant_id, "idem-mixed-plain"),
            withdrawal_of(tenant_id, "idem-mixed-good"),
            withdrawal_of(tenant_id, "idem-mixed-bad"),
        ];
        plugin.set_create_records(vec![Ok(projected(&input[0])), Ok(projected(&input[1]))]);

        let service = service_with_permit(
            Arc::clone(&plugin) as Arc<dyn UsageCollectorPluginV1>,
            "test.target.mixed.records.v1",
        );

        let results = service
            .create_usage_records(&authenticated_ctx(), input)
            .await
            .expect("batch dispatch succeeded");
        assert_eq!(results.len(), 3);
        assert!(results[0].is_ok(), "{:?}", results[0]);
        assert!(results[1].is_ok(), "{:?}", results[1]);
        assert!(
            matches!(
                results[2].as_ref(),
                Err(UsageCollectorError::InvalidArgument {
                    reason: ValidationReason::InvalidationFieldMismatch,
                    ..
                })
            ),
            "only the unfaithful copy MUST be rejected, and at its own index: {:?}",
            results[2],
        );
    }

    #[tokio::test]
    async fn a_target_not_yet_converged_is_a_retryable_conflict() {
        let plugin = HappyPathPlugin::new();
        let tenant_id = Uuid::from_u128(0x5C1);
        let target = target_id(tenant_id, "idem-nc");
        plugin.set_get_usage_record_not_converged(target);
        let service = service_with_permit(
            Arc::clone(&plugin) as Arc<dyn UsageCollectorPluginV1>,
            "test.target.not_converged.records.v1",
        );

        let results = service
            .create_usage_records(
                &authenticated_ctx(),
                vec![withdrawal_of(tenant_id, "idem-nc")],
            )
            .await
            .expect("batch dispatch succeeded");

        match results[0].as_ref() {
            Err(UsageCollectorError::Conflict {
                outcome:
                    ConflictOutcome::TargetNotConverged {
                        retry_after_seconds,
                    },
                name,
                ..
            }) => {
                assert_eq!(name, &target.to_string());
                // This is the one test covering the path that reaches
                // `lift_domain_error` via `resolve_invalidation_targets`'s
                // generic `Some(Err(e))` fallback, so the delay is asserted
                // here rather than absorbed by `..`.
                assert_eq!(
                    *retry_after_seconds,
                    Some(crate::domain::service::DEFAULT_TARGET_NOT_CONVERGED_RETRY_AFTER_SECS),
                    "a not-converged target reached via the batch pre-pass must carry the \
                     configured target_not_converged_retry_after_secs delay"
                );
            }
            other => panic!("a not-converged target is TargetNotConverged, got {other:?}"),
        }
        assert_eq!(plugin.get_usage_record_converged_only_flags(), vec![true]);
        assert!(
            plugin.last_create_records_input().is_none(),
            "nothing is dispatched"
        );
    }

    /// The target is read under the scope compiled from the submitter's own
    /// `create` permit, so another tenant's row answers exactly like an absent
    /// one, never with a field mismatch that would confirm it exists.
    #[tokio::test]
    async fn a_withdrawal_naming_another_tenants_row_answers_as_absent() {
        let plugin = HappyPathPlugin::new();
        let caller_tenant = Uuid::from_u128(0x5A1);
        let other_tenant = Uuid::from_u128(0x5A2);
        let target = target_id(caller_tenant, "idem-cross");
        plugin.set_get_record_for(target, target_row(other_tenant, "idem-cross"));
        let service = service_with_permit(
            Arc::clone(&plugin) as Arc<dyn UsageCollectorPluginV1>,
            "test.target.cross_tenant.records.v1",
        );

        let results = service
            .create_usage_records(
                &authenticated_ctx(),
                vec![withdrawal_of(caller_tenant, "idem-cross")],
            )
            .await
            .expect("batch dispatch succeeded");

        match results[0].as_ref() {
            Err(UsageCollectorError::NotFound {
                reason: usage_collector_sdk::NotFoundReason::InvalidationTargetNotFound,
                name,
                ..
            }) => assert_eq!(name, &target.to_string()),
            other => panic!("an out-of-scope target must read as absent, got {other:?}"),
        }
        assert_translatable_scope(&plugin, "resolve_invalidation_targets");
        let scope = plugin.last_get_scope().expect("the lookup dispatched");
        assert!(
            scope.contains(&format!("{caller_tenant:?}"))
                && !scope.contains(&format!("{target:?}")),
            "the lookup reads under the caller's compiled permit, not `id eq <target>`: {scope}"
        );
    }

    /// A permit the per-entry gate admits but whose scope does not compile
    /// fails the withdrawal closed as `PermissionDenied`, before any target
    /// read, while an ordinary record in the same PDP group is still
    /// accepted. The ordinary record is what proves the gate admitted the
    /// group: a denial there would refuse both entries.
    #[tokio::test]
    async fn a_withdrawal_under_a_permit_scope_that_does_not_compile_is_permission_denied() {
        let plugin = HappyPathPlugin::new();
        let tenant_id = Uuid::from_u128(0x5C1);
        plugin.set_get_record(target_row(tenant_id, "idem-uncompilable-withdrawal"));
        let ordinary = ordinary_record(tenant_id, "idem-uncompilable-plain");
        let stored = projected(&ordinary);
        plugin.set_create_records(vec![Ok(stored.clone())]);
        let service = ServiceFixture::default()
            .with_source(fake_declaration_source_with_fold("SUM"))
            .with_resolver(Arc::new(UncompilableSiblingPermitResolver))
            .build(
                Arc::clone(&plugin) as Arc<dyn UsageCollectorPluginV1>,
                "test.target.uncompilable_scope.records.v1",
            );

        let results = service
            .create_usage_records(
                &authenticated_ctx(),
                vec![
                    withdrawal_of(tenant_id, "idem-uncompilable-withdrawal"),
                    ordinary,
                ],
            )
            .await
            .expect("batch dispatch succeeded");

        assert!(
            matches!(
                results[0],
                Err(UsageCollectorError::PermissionDenied { .. })
            ),
            "a permit scope that does not compile fails the withdrawal closed, got {:?}",
            results[0],
        );
        assert_eq!(
            results[1]
                .as_ref()
                .expect("the ordinary record is accepted"),
            &stored,
        );
        assert_eq!(
            plugin.get_usage_record_calls(),
            0,
            "no target is read under a scope that does not compile",
        );
        assert_eq!(
            plugin
                .last_create_records_input()
                .expect("the ordinary record reached the persist SPI")
                .len(),
            1,
        );
    }

    /// The target cache is keyed by `(target, permit scope)`, not by target
    /// alone: two withdrawals of one target in different PDP groups each read
    /// it under their own group's scope. Two *faithful* withdrawals cannot
    /// land in different groups, since a faithful copy repeats the group key,
    /// so the second withdrawal departs in `resource_ref` alone — its own
    /// lookup succeeds and it is then refused as an unfaithful copy, a verdict
    /// only a row read for it could produce.
    #[tokio::test]
    async fn withdrawals_of_one_target_under_two_permits_each_cost_their_own_lookup() {
        let plugin = HappyPathPlugin::new();
        let tenant_id = Uuid::from_u128(0x5C2);
        let target = target_id(tenant_id, "idem-two-permits");
        plugin.set_get_record(target_row(tenant_id, "idem-two-permits"));
        let faithful = withdrawal_of(tenant_id, "idem-two-permits");
        // `resource_ref` is no identity input, so the second withdrawal
        // resolves the same target while landing in another PDP group.
        let other_resource = CreateUsageRecord {
            resource_ref: ResourceRef::new("rsc-other", "compute.vm").expect("valid resource ref"),
            ..withdrawal_of(tenant_id, "idem-two-permits")
        };
        let stored = projected(&faithful);
        plugin.set_create_records(vec![Ok(stored.clone())]);
        let service = service_with_permit(
            Arc::clone(&plugin) as Arc<dyn UsageCollectorPluginV1>,
            "test.target.two_permits.records.v1",
        );

        let results = service
            .create_usage_records(&authenticated_ctx(), vec![faithful, other_resource])
            .await
            .expect("batch dispatch succeeded");

        assert_eq!(
            plugin.get_usage_record_calls(),
            2,
            "one target under two PDP groups is read once per group",
        );
        assert_eq!(plugin.get_usage_record_inputs(), vec![target, target]);
        assert_eq!(
            results[0]
                .as_ref()
                .expect("the faithful withdrawal is accepted"),
            &stored,
        );
        match results[1].as_ref() {
            Err(UsageCollectorError::InvalidArgument {
                reason: ValidationReason::InvalidationFieldMismatch,
                field,
                ..
            }) => assert_eq!(field, "resource_ref"),
            other => panic!(
                "the withdrawal in the other group resolves against its own read, got {other:?}"
            ),
        }
    }

    #[tokio::test]
    async fn identical_entries_in_one_batch_dispatch_once_and_share_the_stored_row() {
        let plugin = HappyPathPlugin::new();
        let tenant_id = Uuid::from_u128(0x5B1);
        let entry = ordinary_record(tenant_id, "idem-twice");
        let stored = UsageRecord {
            origin: usage_collector_sdk::RecordOrigin::Backfill,
            ..projected(&entry)
        };
        plugin.set_create_records(vec![Ok(stored.clone())]);
        let service = service_with_permit(
            Arc::clone(&plugin) as Arc<dyn UsageCollectorPluginV1>,
            "test.identity.identical_pair.records.v1",
        );

        let results = service
            .create_usage_records(&authenticated_ctx(), vec![entry.clone(), entry])
            .await
            .expect("batch dispatch succeeded");

        assert_eq!(
            plugin
                .last_create_records_input()
                .expect("dispatched")
                .len(),
            1,
            "one dispatch per dedup identity"
        );
        for (position, result) in results.iter().enumerate() {
            assert_eq!(
                result.as_ref().expect("accepted"),
                &stored,
                "entry {position} reports the stored row"
            );
        }
    }

    #[tokio::test]
    async fn a_divergent_later_entry_conflicts_with_the_earlier_one() {
        let plugin = HappyPathPlugin::new();
        let tenant_id = Uuid::from_u128(0x5B2);
        let first = ordinary_record(tenant_id, "idem-split");
        let mut second = first.clone();
        second.quantity = crate::domain::test_support::qty("11");
        let stored = projected(&first);
        plugin.set_create_records(vec![Ok(stored.clone())]);
        let service = service_with_permit(
            Arc::clone(&plugin) as Arc<dyn UsageCollectorPluginV1>,
            "test.identity.divergent_pair.records.v1",
        );

        let results = service
            .create_usage_records(&authenticated_ctx(), vec![first, second])
            .await
            .expect("batch dispatch succeeded");

        assert_eq!(results[0].as_ref().expect("the first is accepted"), &stored);
        match results[1].as_ref() {
            Err(UsageCollectorError::Conflict {
                outcome: ConflictOutcome::IdempotencyConflict,
                name,
                ..
            }) => assert_eq!(name, &stored.id.to_string()),
            other => panic!("the later divergent entry conflicts, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn two_withdrawals_of_one_target_in_one_batch_report_already_invalidated() {
        let plugin = HappyPathPlugin::new();
        let tenant_id = Uuid::from_u128(0x5B3);
        let target = target_id(tenant_id, "idem-w");
        plugin.set_get_record_for(target, target_row(tenant_id, "idem-w"));
        let first = withdrawal_of(tenant_id, "idem-w");
        let mut second = withdrawal_of(tenant_id, "idem-w");
        second.invalidation = Some(ReasonCode::new("late_correction").expect("valid reason code"));
        let stored = projected(&first);
        plugin.set_create_records(vec![Ok(stored.clone())]);
        let service = service_with_permit(
            Arc::clone(&plugin) as Arc<dyn UsageCollectorPluginV1>,
            "test.identity.withdrawal_pair.records.v1",
        );

        let results = service
            .create_usage_records(&authenticated_ctx(), vec![first, second])
            .await
            .expect("batch dispatch succeeded");

        assert_eq!(
            plugin
                .last_create_records_input()
                .expect("dispatched")
                .len(),
            1
        );
        assert!(
            results[0].is_ok(),
            "the first withdrawal is accepted: {:?}",
            results[0]
        );
        match results[1].as_ref() {
            Err(UsageCollectorError::Conflict {
                outcome:
                    ConflictOutcome::AlreadyInvalidated {
                        invalidated_by,
                        reason_code,
                    },
                name,
                ..
            }) => {
                assert_eq!(name, &target.to_string());
                assert_eq!(*invalidated_by, stored.id);
                assert_eq!(reason_code.as_str(), "emitter_defect");
            }
            other => panic!("the later withdrawal is AlreadyInvalidated, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_later_entry_equal_to_what_the_store_holds_is_accepted_with_it() {
        let plugin = HappyPathPlugin::new();
        let tenant_id = Uuid::from_u128(0x5B4);
        let held = ordinary_record(tenant_id, "idem-held");
        let mut first = held.clone();
        first.quantity = crate::domain::test_support::qty("11");
        let stored = projected(&held);
        plugin.set_create_records(vec![Err(UsageCollectorPluginError::idempotency_conflict(
            "idem-held",
            crate::domain::test_support::as_stored(stored.clone()),
        ))]);
        let service = service_with_permit(
            Arc::clone(&plugin) as Arc<dyn UsageCollectorPluginV1>,
            "test.identity.against_stored.records.v1",
        );

        let results = service
            .create_usage_records(&authenticated_ctx(), vec![first, held])
            .await
            .expect("batch dispatch succeeded");

        assert!(
            matches!(
                results[0].as_ref(),
                Err(UsageCollectorError::Conflict {
                    outcome: ConflictOutcome::IdempotencyConflict,
                    ..
                })
            ),
            "the first conflicts with the stored row: {:?}",
            results[0]
        );
        assert_eq!(
            results[1]
                .as_ref()
                .expect("the later entry matches what the store holds"),
            &stored
        );
    }

    #[tokio::test]
    async fn a_later_entry_shares_the_earlier_entrys_backend_failure() {
        let plugin = HappyPathPlugin::new();
        let tenant_id = Uuid::from_u128(0x5B5);
        let entry = ordinary_record(tenant_id, "idem-blip");
        plugin.set_create_records(vec![Err(UsageCollectorPluginError::transient(
            "backend blip",
        ))]);
        let service = service_with_permit(
            Arc::clone(&plugin) as Arc<dyn UsageCollectorPluginV1>,
            "test.identity.shared_failure.records.v1",
        );

        let results = service
            .create_usage_records(&authenticated_ctx(), vec![entry.clone(), entry])
            .await
            .expect("batch dispatch succeeded");

        for (position, result) in results.iter().enumerate() {
            assert!(
                matches!(result, Err(UsageCollectorError::ServiceUnavailable { .. })),
                "entry {position} carries the backend failure: {result:?}"
            );
        }
    }

    #[tokio::test]
    async fn a_withdrawal_of_an_entry_in_the_same_batch_is_not_converged() {
        let plugin = HappyPathPlugin::new();
        let tenant_id = Uuid::from_u128(0x5B6);
        let record = ordinary_record(tenant_id, "idem-same-batch");
        let record_id = projected(&record).id;
        plugin.set_get_usage_record_not_found(record_id);
        plugin.set_create_records(vec![Ok(projected(&record))]);
        let service = service_with_permit(
            Arc::clone(&plugin) as Arc<dyn UsageCollectorPluginV1>,
            "test.identity.same_batch_target.records.v1",
        );

        let results = service
            .create_usage_records(
                &authenticated_ctx(),
                vec![record, withdrawal_of(tenant_id, "idem-same-batch")],
            )
            .await
            .expect("batch dispatch succeeded");

        assert!(
            results[0].is_ok(),
            "the record is accepted: {:?}",
            results[0]
        );
        match results[1].as_ref() {
            Err(UsageCollectorError::Conflict {
                outcome: ConflictOutcome::TargetNotConverged { .. },
                name,
                ..
            }) => assert_eq!(name, &record_id.to_string()),
            other => panic!("a target submitted in the same batch is not converged, got {other:?}"),
        }
    }
}

// ─── usage-emission feature (read-by-id) ─────────────────────────────────
//
// `Service::get_usage_record` authorizes like `list_usage_records`:
// `authz::authorize_get_usage_record_scope` compiles the caller's scope FIRST
// (with no per-record attribution attributes — the id-only boundary has no
// record fields to offer yet), and only a permitted caller's scope reaches
// `get_usage_record(id, scope)`. The point lookup carries no caller filter,
// so that scope is the whole plugin-side filter: a row outside it is never
// returned, so a missing row and a PDP deny surface the same `NotFound` and
// the deny is refused before any plugin dispatch.
mod get_usage_record_tests {
    use std::sync::Arc;
    use toolkit_gts::gts_id;

    use usage_collector_sdk::{
        USAGE_RECORD_RESOURCE, UsageCollectorError, UsageCollectorPluginV1, UsageRecord,
    };
    use uuid::Uuid;

    use crate::domain::test_support::{
        DenyAllResolver, RecordingPlugin, ServiceFixture, UnreachableResolver, authenticated_ctx,
        recording_plugin_resolver,
    };

    const HAPPY_RECORD_GTS_ID: &str =
        gts_id!("cf.core.uc.usage_record.v1~cf.mini_chat._.tokens_consumed.v1~");

    fn sample_persisted_record(id: Uuid, tenant_id: Uuid) -> UsageRecord {
        use std::collections::BTreeMap;
        use time::OffsetDateTime;
        use usage_collector_sdk::{IdempotencyKey, MeterTypeId, RecordOrigin, ResourceRef};
        UsageRecord {
            id,
            gts_type_id: MeterTypeId::new(HAPPY_RECORD_GTS_ID).expect("valid gts_type_id"),
            tenant_id,
            resource_ref: ResourceRef::new("rsc-happy", "compute.vm").expect("valid resource ref"),
            subject_ref: None,
            metadata: BTreeMap::new(),
            quantity: crate::domain::test_support::qty("1"),
            idempotency_key: IdempotencyKey::new("idem-happy").expect("valid idempotency key"),
            accepted_at: OffsetDateTime::UNIX_EPOCH + time::Duration::hours(1),
            origin: RecordOrigin::Live,
            invalidation: None,
            window_start: OffsetDateTime::UNIX_EPOCH,
            window_end: OffsetDateTime::UNIX_EPOCH + time::Duration::hours(1),
        }
    }

    /// PDP permits with a real tenant-narrowing compiled scope, the plugin
    /// returns the row, the service returns it verbatim — in exactly one
    /// `get_usage_record` SPI dispatch.
    #[tokio::test]
    async fn get_usage_record_happy_path_returns_loaded_record() {
        let plugin = RecordingPlugin::new();
        let target = Uuid::from_u128(0x00C0_FFEE);
        let tenant_id = Uuid::from_u128(2);
        plugin.set_get_record(sample_persisted_record(target, tenant_id));

        let svc = ServiceFixture::default()
            .with_resolver(recording_plugin_resolver())
            .build(
                Arc::clone(&plugin) as Arc<dyn UsageCollectorPluginV1>,
                "test.usage_collector.get_record.happy.v1",
            );

        let record = svc
            .get_usage_record(&authenticated_ctx(), target)
            .await
            .expect("happy-path read MUST succeed");

        assert_eq!(record.id, target);
        assert_eq!(record.tenant_id, tenant_id);
        assert_eq!(
            plugin.get_usage_record_calls(),
            1,
            "exactly one SPI dispatch on the happy path; observed {} calls",
            plugin.get_usage_record_calls(),
        );
        assert_eq!(
            plugin.get_usage_record_inputs().last().copied(),
            Some(target),
            "gateway MUST forward the target id verbatim to the plugin",
        );
        assert_eq!(
            plugin.get_usage_record_converged_only_flags(),
            vec![false],
            "the caller-facing point read is not converged-only"
        );
    }

    /// A permitted caller's genuinely-missing target (the plugin reports
    /// `UsageRecordNotFound { id }`) surfaces as `NotFound`, the same envelope
    /// the PDP-deny case below produces.
    #[tokio::test]
    async fn get_usage_record_plugin_not_found_surfaces_as_not_found() {
        let plugin = RecordingPlugin::new();
        let target = Uuid::from_u128(0xDEAD_F00D);
        plugin.set_get_usage_record_not_found(target);

        let svc = ServiceFixture::default()
            .with_resolver(recording_plugin_resolver())
            .build(
                Arc::clone(&plugin) as Arc<dyn UsageCollectorPluginV1>,
                "test.usage_collector.get_record.plugin_not_found.v1",
            );

        let err = svc
            .get_usage_record(&authenticated_ctx(), target)
            .await
            .expect_err("plugin UsageRecordNotFound MUST surface as NotFound");

        assert!(
            matches!(
                err,
                UsageCollectorError::NotFound { ref resource_type, ref name, .. }
                    if resource_type == USAGE_RECORD_RESOURCE && name == &target.to_string()
            ),
            "expected NotFound carrying the target id, got {err:?}",
        );
        assert_eq!(
            plugin.get_usage_record_calls(),
            1,
            "the PDP permitted, so the dispatch DID happen - the plugin, not \
             authorization, is what reported NotFound here",
        );
    }

    /// PDP deny collapses to `NotFound` BEFORE any plugin dispatch: a denied
    /// caller's scope never reaches the plugin, so the row is never queried.
    #[tokio::test]
    async fn get_usage_record_pdp_deny_collapses_to_not_found() {
        let plugin = RecordingPlugin::new();
        let target = Uuid::from_u128(0xFEED);
        let tenant_id = Uuid::from_u128(2);
        plugin.set_get_record(sample_persisted_record(target, tenant_id));

        let svc = ServiceFixture::default()
            .with_resolver(Arc::new(DenyAllResolver))
            .build(
                Arc::clone(&plugin) as Arc<dyn UsageCollectorPluginV1>,
                "test.usage_collector.get_record.pdp_deny.v1",
            );

        let err = svc
            .get_usage_record(&authenticated_ctx(), target)
            .await
            .expect_err("PDP deny MUST surface as NotFound");

        assert!(
            matches!(err, UsageCollectorError::NotFound { .. }),
            "expected NotFound, got {err:?}",
        );
        assert_eq!(
            plugin.get_usage_record_calls(),
            0,
            "authorization runs BEFORE dispatch - a denied caller's compiled \
             scope never reaches the plugin, so it is never even called",
        );
    }

    /// PDP transport failure fails closed, also before any plugin dispatch.
    #[tokio::test]
    async fn get_usage_record_pdp_unreachable_fails_closed() {
        let plugin = RecordingPlugin::new();
        let target = Uuid::from_u128(0xFACE);
        let tenant_id = Uuid::from_u128(2);
        plugin.set_get_record(sample_persisted_record(target, tenant_id));

        let svc = ServiceFixture::default()
            .with_resolver(Arc::new(UnreachableResolver))
            .build(
                Arc::clone(&plugin) as Arc<dyn UsageCollectorPluginV1>,
                "test.usage_collector.get_record.pdp_unreachable.v1",
            );

        let err = svc
            .get_usage_record(&authenticated_ctx(), target)
            .await
            .expect_err("unreachable PDP transport MUST fail closed");

        assert!(
            matches!(err, UsageCollectorError::ServiceUnavailable { .. }),
            "expected ServiceUnavailable, got {err:?}",
        );
        assert_eq!(
            plugin.get_usage_record_calls(),
            0,
            "the PDP call fails before the plugin is ever dispatched",
        );
    }

    /// The plugin receives the compiled PDP scope on the point lookup. The
    /// captured rendering must carry the tenant-narrowing predicate
    /// `recording_plugin_resolver` grants, so a regression wiring an
    /// unrestricted or placeholder filter through is caught.
    #[tokio::test]
    async fn the_point_lookup_passes_the_compiled_scope_to_the_plugin() {
        let plugin = RecordingPlugin::new();
        let target = Uuid::from_u128(0xC0DE);
        plugin.set_get_record(sample_persisted_record(target, Uuid::from_u128(2)));

        let svc = ServiceFixture::default()
            .with_resolver(recording_plugin_resolver())
            .build(
                Arc::clone(&plugin) as Arc<dyn UsageCollectorPluginV1>,
                "test.usage_collector.get_record.scope_capture.v1",
            );

        let _outcome = svc.get_usage_record(&authenticated_ctx(), target).await;

        let scope = plugin
            .last_get_scope()
            .expect("the plugin must receive a compiled scope, not an unscoped read");
        assert!(
            scope.contains("tenant_id"),
            "expected the compiled scope to carry the tenant-narrowing \
             predicate the PDP granted, got {scope:?}",
        );
    }

    /// A plugin-side transient error, on an otherwise-permitted call,
    /// lifts through the canonical chain to `ServiceUnavailable`.
    #[tokio::test]
    async fn get_usage_record_plugin_transient_lifts_to_service_unavailable() {
        use usage_collector_sdk::UsageCollectorPluginError;

        // `HappyPathPlugin` exposes no transient path, so this stub returns
        // Transient on `get_usage_record` and fails loudly elsewhere.
        struct TransientGetPlugin;

        #[async_trait::async_trait]
        impl UsageCollectorPluginV1 for TransientGetPlugin {
            async fn create_usage_records(
                &self,
                _records: Vec<(
                    usage_collector_sdk::MeterRef,
                    usage_collector_sdk::StoredUsageRecord,
                )>,
            ) -> Result<
                Vec<Result<usage_collector_sdk::StoredUsageRecord, UsageCollectorPluginError>>,
                UsageCollectorPluginError,
            > {
                Err(UsageCollectorPluginError::internal(
                    "test_fake: TransientGetPlugin: create_usage_records must not be called",
                ))
            }
            async fn query_aggregated_usage_records(
                &self,
                _meter: &usage_collector_sdk::MeterRef,
                _time_range: usage_collector_sdk::TimeRange,
                _fold: usage_collector_sdk::AggregationFold,
                _query: &toolkit_odata::ODataQuery,
                _metadata_filter: &[usage_collector_sdk::MetadataFilter],
                _group_by: &[usage_collector_sdk::AggregationDimension],
            ) -> Result<usage_collector_sdk::AggregationResult, UsageCollectorPluginError>
            {
                Err(UsageCollectorPluginError::internal(
                    "test_fake: TransientGetPlugin: query_aggregated_usage_records must not be called",
                ))
            }
            async fn list_usage_records(
                &self,
                _meter: &usage_collector_sdk::MeterRef,
                _time_range: usage_collector_sdk::TimeRange,
                _query: &toolkit_odata::ODataQuery,
                _metadata_filter: &[usage_collector_sdk::MetadataFilter],
                _keyset: Option<&usage_collector_sdk::Keyset>,
            ) -> Result<usage_collector_sdk::RecordPage, UsageCollectorPluginError> {
                Err(UsageCollectorPluginError::internal(
                    "test_fake: TransientGetPlugin: list_usage_records must not be called",
                ))
            }
            async fn get_usage_record(
                &self,
                _id: Uuid,
                _scope: &toolkit_odata::ast::Expr,
                _converged_only: bool,
            ) -> Result<usage_collector_sdk::StoredUsageRecord, UsageCollectorPluginError>
            {
                Err(UsageCollectorPluginError::transient(
                    "test_fake: TransientGetPlugin: simulated prefetch transient",
                ))
            }
            async fn read_feed_page(
                &self,
                _subscription: &[usage_collector_sdk::MeterRef],
                _scope: &toolkit_odata::ast::Expr,
                _start: usage_collector_sdk::FeedStart<usage_collector_sdk::FeedPosition>,
                _until: Option<usage_collector_sdk::FeedPosition>,
                _limit: u64,
            ) -> Result<
                usage_collector_sdk::FeedPage<
                    usage_collector_sdk::FeedPosition,
                    usage_collector_sdk::StoredUsageRecord,
                >,
                UsageCollectorPluginError,
            > {
                Err(UsageCollectorPluginError::internal(
                    "test_fake: TransientGetPlugin: read_feed_page must not be called",
                ))
            }
            async fn get_reconciliation_metadata(
                &self,
                _tenant_id: Uuid,
                _meter: &usage_collector_sdk::MeterRef,
                _time_range: usage_collector_sdk::TimeRange,
                _fold: usage_collector_sdk::AggregationFold,
                _scope: &toolkit_odata::ast::Expr,
            ) -> Result<usage_collector_sdk::ReconciliationMetadata, UsageCollectorPluginError>
            {
                Err(UsageCollectorPluginError::internal(
                    "test_fake: TransientGetPlugin: get_reconciliation_metadata must not be called",
                ))
            }
        }

        let plugin: Arc<dyn UsageCollectorPluginV1> = Arc::new(TransientGetPlugin);
        // `recording_plugin_resolver` permits; the fixture's tenant-echoing
        // default would fail closed on this pre-row request and mask the
        // plugin's Transient behind an authz NotFound.
        let svc = ServiceFixture::default()
            .with_resolver(recording_plugin_resolver())
            .build(plugin, "test.usage_collector.get_record.transient.v1");

        let err = svc
            .get_usage_record(&authenticated_ctx(), Uuid::from_u128(0x01))
            .await
            .expect_err("plugin Transient MUST lift to ServiceUnavailable");

        assert!(
            matches!(err, UsageCollectorError::ServiceUnavailable { .. }),
            "expected ServiceUnavailable from the plugin dispatch, got {err:?}",
        );
        assert!(err.is_retryable());
    }
}

#[cfg(test)]
mod private_helpers_tests {
    //! Unit coverage for the private service-module helper
    //! `compose_query_with_scope`.

    use toolkit_odata::ODataQuery;
    use toolkit_odata::ast::{CompareOperator, Expr, Value};
    use toolkit_security::{AccessScope, ScopeConstraint, ScopeFilter, pep_properties};
    use usage_collector_sdk::UsageCollectorError;
    use uuid::Uuid;

    use crate::domain::query::compose_query_with_scope;

    #[test]
    fn allow_all_scope_is_denied_fail_closed() {
        // An `allow_all` scope on the LIST/aggregate path is a degenerate
        // empty-predicate permit, not a happy-path grant. Composition MUST fail
        // closed rather than pass the user filter through unscoped (which would
        // return every tenant's records).
        let user_filter = Expr::Compare(
            Box::new(Expr::Identifier("resource_type".into())),
            CompareOperator::Eq,
            Box::new(Expr::Value(Value::String("compute.vm".into()))),
        );
        let mut user_query = ODataQuery::new();
        user_query.filter = Some(Box::new(user_filter));
        user_query.limit = Some(50);

        let err = compose_query_with_scope(&user_query, &AccessScope::allow_all())
            .expect_err("allow_all -> PermissionDenied");
        assert!(
            matches!(err, UsageCollectorError::PermissionDenied { .. }),
            "allow_all must surface as PermissionDenied, got {err:?}",
        );
    }

    #[test]
    fn empty_user_filter_yields_scope_filter_alone() {
        let scope = AccessScope::single(ScopeConstraint::new(vec![ScopeFilter::eq(
            pep_properties::OWNER_TENANT_ID,
            Uuid::from_u128(0xAA),
        )]));
        let composed = compose_query_with_scope(&ODataQuery::new(), &scope).expect("happy path");
        let f = composed.filter().expect("scope-only filter");
        assert!(matches!(f, Expr::Compare(..)));
    }

    #[test]
    fn user_filter_and_scope_are_and_merged() {
        let user_filter = Expr::Compare(
            Box::new(Expr::Identifier("resource_type".into())),
            CompareOperator::Eq,
            Box::new(Expr::Value(Value::String("compute.vm".into()))),
        );
        let mut user_query = ODataQuery::new();
        user_query.filter = Some(Box::new(user_filter));

        let scope = AccessScope::single(ScopeConstraint::new(vec![ScopeFilter::eq(
            pep_properties::OWNER_TENANT_ID,
            Uuid::from_u128(0xBB),
        )]));
        let composed = compose_query_with_scope(&user_query, &scope).expect("happy path");
        let f = composed.filter().expect("merged filter");
        assert!(matches!(f, Expr::And(..)));
    }

    #[test]
    fn deny_all_scope_short_circuits_to_authorization_error() {
        let err = compose_query_with_scope(&ODataQuery::new(), &AccessScope::deny_all())
            .expect_err("deny_all -> PermissionDenied");
        assert!(
            matches!(err, UsageCollectorError::PermissionDenied { .. }),
            "deny_all must surface as PermissionDenied, got {err:?}",
        );
    }
}

// This module drives `create_usage_records` with a single submission, where
// the PDP / declaration / target / metadata / SPI sequencing is observable one
// stage at a time rather than through the deduped fan-out that the
// `pdp_dedup`, `gts_id_dedup` and `invalidation_target_batch` modules cover.
// One outcome per stage: PDP deny, plugin-reported transient on the persist
// SPI, a target that resolves to nothing, a target that is itself a
// withdrawal, an unfaithful copy, the copy rule outranking the metadata rule,
// the dedup conflict of an invalidation, a backend fault on the target read,
// and the happy path.

mod create_usage_record_path_tests {
    use std::collections::BTreeMap;
    use std::sync::Arc;
    use toolkit_gts::gts_id;

    use usage_collector_sdk::{
        ConflictOutcome, CreateUsageRecord, EntryType, IdempotencyKey, Invalidation, MetadataKey,
        MeterTypeId, ReasonCode, RecordOrigin, ResourceRef, USAGE_RECORD_RESOURCE,
        UsageCollectorError, UsageCollectorPluginError, UsageCollectorPluginV1, UsageRecord,
        ValidationReason,
    };
    use uuid::Uuid;

    use super::assert_translatable_scope;
    use crate::domain::Service;
    use crate::domain::test_support::{
        DenyAllResolver, HappyPathPlugin, MeterNarrowingPermitResolver, ServiceFixture,
        UncompilableSiblingPermitResolver, authenticated_ctx, enforcer_for,
        fake_declaration_source_with_fold, fake_declaration_source_with_metadata, hub_with_plugin,
        projected, recent_window_end, recent_window_start,
    };

    const COUNTER_GTS_ID: &str =
        gts_id!("cf.core.uc.usage_record.v1~cf.mini_chat._.tokens_consumed.v1~");

    /// The base ordinary measurement (no `invalidates`) every test here
    /// shapes; call sites mutate `value` / `gts_type_id` / `invalidation` to
    /// drive the per-stage outcome. `value` drives no accept/reject decision.
    fn counter_record(tenant_id: Uuid, value: i64, idem: &str) -> CreateUsageRecord {
        CreateUsageRecord {
            entry_type: EntryType::Record,
            gts_type_id: MeterTypeId::new(COUNTER_GTS_ID).expect("valid gts_type_id"),
            tenant_id,
            resource_ref: ResourceRef::new("rsc-singular", "compute.vm")
                .expect("valid resource ref"),
            subject_ref: None,
            metadata: BTreeMap::new(),
            quantity: crate::domain::test_support::qty(&value.to_string()),
            idempotency_key: Some(IdempotencyKey::new(idem).expect("valid idempotency key")),
            invalidation: None,
            window_start: recent_window_start(),
            window_end: recent_window_end(),
        }
    }

    /// The withdrawal of `counter_record(tenant_id, 10, idem)`: a faithful
    /// copy, departing only in `entry_type` and the reason. The quantity is
    /// echoed, never negated — an invalidation removes a measurement rather
    /// than offsetting it. It names no target; the gateway derives one from
    /// the identity inputs this copies, so `idem` chooses which entry is
    /// withdrawn and [`target_id`] says which.
    fn counter_withdrawal(tenant_id: Uuid, idem: &str) -> CreateUsageRecord {
        let mut r = counter_record(tenant_id, 10, idem);
        r.entry_type = EntryType::Invalidation;
        r.invalidation = Some(ReasonCode::new("emitter_defect").expect("valid reason code"));
        r
    }

    /// The persisted entry `counter_withdrawal(tenant_id, idem)` withdraws:
    /// the projection of the very submission it copies, so the two carry the
    /// same identity inputs and the derivation lands on this row.
    fn target_row(tenant_id: Uuid, idem: &str) -> UsageRecord {
        UsageRecord {
            // Deliberately not the origin the live submission carries: a
            // mismatch here MUST NOT make the copy unfaithful
            // (`faithful_copy_mismatch` ignores `origin`).
            origin: RecordOrigin::Backfill,
            ..projected(&counter_record(tenant_id, 10, idem))
        }
    }

    /// The identifier `counter_withdrawal(tenant_id, idem)` resolves.
    fn target_id(tenant_id: Uuid, idem: &str) -> Uuid {
        target_row(tenant_id, idem).id
    }

    /// A `Service` over a permit-by-default PDP, a Type Resolver declaring an
    /// arbitrary fold (these tests exercise ingestion, not aggregation), and
    /// the supplied plugin stub.
    fn service_with_permit(plugin: Arc<dyn UsageCollectorPluginV1>, suffix: &str) -> Arc<Service> {
        ServiceFixture::default()
            .with_source(fake_declaration_source_with_fold("SUM"))
            .build(plugin, suffix)
    }

    /// A `Service` over a deny-all PDP. The plugin stub is left unprogrammed,
    /// so a leaked SPI call surfaces as a `not_programmed` `Internal` rather
    /// than the expected authorization envelope.
    fn service_with_deny(plugin: Arc<dyn UsageCollectorPluginV1>, suffix: &str) -> Arc<Service> {
        let hub = hub_with_plugin(plugin, suffix, "constructorfabric");
        let enforcer = enforcer_for(Arc::new(DenyAllResolver) as _);
        Arc::new(Service::new(hub, "constructorfabric".to_owned(), enforcer))
    }

    /// PDP deny ⇒ `PermissionDenied`, with **no** catalog / semantics / SPI
    /// dispatch.
    #[tokio::test]
    async fn create_usage_record_pdp_deny_returns_authorization_before_any_spi_dispatch() {
        let plugin = HappyPathPlugin::new();
        let service = service_with_deny(
            Arc::clone(&plugin) as _,
            "test.singular.pdp_deny.records.v1",
        );

        let record = counter_record(Uuid::from_u128(0x701), 1, "idem-pdp-deny");

        let result = service
            .create_usage_records(&authenticated_ctx(), vec![record])
            .await
            .expect(
                "a PDP deny is a per-record rejection, not a request-wide refusal, so the \
                 batch call itself still succeeds",
            )
            .into_iter()
            .next()
            .expect("one entry in, one slot out");

        let err = result.expect_err("PDP deny MUST surface as Err");

        assert!(
            matches!(err, UsageCollectorError::PermissionDenied { .. }),
            "PDP deny MUST surface as `PermissionDenied`, got {err:?}",
        );
        assert_eq!(
            plugin.get_usage_record_calls(),
            0,
            "PDP deny MUST short-circuit before any target lookup",
        );
        assert!(
            plugin.last_create_records_input().is_none(),
            "PDP deny MUST short-circuit before any persist SPI dispatch",
        );
    }

    /// Plugin-reported `Transient` from the persist SPI ⇒
    /// `ServiceUnavailable` at the SDK boundary, with the
    /// `retry_after_seconds` hint forwarded verbatim.
    #[tokio::test]
    async fn create_usage_record_plugin_transient_lifts_to_service_unavailable() {
        let plugin = HappyPathPlugin::new();
        plugin.set_create_records(vec![Err(UsageCollectorPluginError::transient_with_retry(
            "downstream backend timed out",
            Some(13),
        ))]);

        let service = service_with_permit(
            Arc::clone(&plugin) as _,
            "test.singular.plugin_transient.records.v1",
        );

        let record = counter_record(Uuid::from_u128(0x702), 1, "idem-plugin-transient");

        let result = service
            .create_usage_records(&authenticated_ctx(), vec![record])
            .await
            .expect(
                "the persist SPI answered this one record with Transient inside its Vec \
                 of per-record outcomes, which `settle_dispatched` always routes to the \
                 slot rather than the outer Err - the outer dispatch call itself \
                 (`plugin.create_usage_records`) succeeded",
            )
            .into_iter()
            .next()
            .expect("one entry in, one slot out");

        let err = result.expect_err("plugin transient MUST surface as Err");

        match err {
            UsageCollectorError::ServiceUnavailable {
                detail,
                retry_after_seconds,
            } => {
                assert_eq!(detail, "downstream backend timed out");
                assert_eq!(retry_after_seconds, Some(13));
            }
            other => panic!(
                "plugin Transient MUST lift to `ServiceUnavailable` with the \
                 retry hint forwarded; got {other:?}",
            ),
        }
    }

    /// A faithful withdrawal of a resolvable target is accepted, and costs
    /// exactly one target read.
    #[tokio::test]
    async fn create_usage_record_accepts_a_faithful_withdrawal_after_one_target_lookup() {
        let plugin = HappyPathPlugin::new();
        let tenant_id = Uuid::from_u128(0x703);
        plugin.set_get_record(target_row(tenant_id, "idem-faithful"));

        let submission = counter_withdrawal(tenant_id, "idem-faithful");
        plugin.set_create_records(vec![Ok(projected(&submission))]);

        let service = service_with_permit(
            Arc::clone(&plugin) as _,
            "test.singular.faithful.records.v1",
        );

        service
            .create_usage_records(&authenticated_ctx(), vec![submission])
            .await
            .expect("the measurement is not refused request-wide")
            .into_iter()
            .next()
            .expect("one entry in, one slot out")
            .expect("a faithful withdrawal of a resolvable target MUST be accepted");

        assert_eq!(
            plugin.get_usage_record_calls(),
            1,
            "an invalidation MUST resolve its target exactly once",
        );

        assert_translatable_scope(&plugin, "resolve_invalidation_targets");
    }

    /// The dispatched withdrawal keeps the caller's key, which is its
    /// target's key: no prefix is reserved and nothing is derived in its
    /// place. Keeping it is what lets the gateway resolve the target from the
    /// submission alone, and what makes two withdrawals of one entry collide.
    #[tokio::test]
    async fn create_usage_record_dispatches_an_invalidation_under_its_targets_key() {
        let plugin = HappyPathPlugin::new();
        let tenant_id = Uuid::from_u128(0x704);
        plugin.set_get_record(target_row(tenant_id, "idem-withdrawn"));

        let submission = counter_withdrawal(tenant_id, "idem-withdrawn");
        plugin.set_create_records(vec![Ok(projected(&submission))]);

        let service = service_with_permit(
            Arc::clone(&plugin) as _,
            "test.singular.derived_key.records.v1",
        );

        service
            .create_usage_records(&authenticated_ctx(), vec![submission])
            .await
            .expect("the withdrawal is not refused request-wide")
            .into_iter()
            .next()
            .expect("one entry in, one slot out")
            .expect("a faithful withdrawal of a resolvable target MUST be accepted");

        let dispatched = plugin
            .last_create_records_input()
            .expect("the withdrawal reached the persist SPI")
            .into_iter()
            .next()
            .expect("one entry in, one dispatched record out");
        assert_eq!(
            dispatched.idempotency_key.as_str(),
            "idem-withdrawn",
            "a withdrawal is dispatched under its target's own key",
        );
        assert_eq!(
            dispatched
                .invalidation
                .as_ref()
                .expect("the dispatched entry is a withdrawal")
                .target,
            target_id(tenant_id, "idem-withdrawn"),
            "the gateway stamps the target it derived from the submission",
        );
    }

    /// A derived target the plugin does not hold ⇒ `NotFound` naming it. The
    /// batch path's cover is in `invalidation_target_batch_tests`.
    #[tokio::test]
    async fn create_usage_record_unresolvable_target_returns_not_found() {
        let plugin = HappyPathPlugin::new();

        let tenant_id = Uuid::from_u128(0x705);
        let missing = target_id(tenant_id, "idem-missing-target");
        plugin.set_get_usage_record_not_found(missing);

        let service = service_with_permit(
            Arc::clone(&plugin) as _,
            "test.singular.target_not_found.records.v1",
        );

        let record = counter_withdrawal(tenant_id, "idem-missing-target");

        let result = service
            .create_usage_records(&authenticated_ctx(), vec![record])
            .await
            .expect(
                "an unresolvable target is a per-record rejection: resolve_invalidation_targets \
                 always routes it to the slot, never to the outer Err",
            )
            .into_iter()
            .next()
            .expect("one entry in, one slot out");

        let err = result.expect_err("an unresolvable target MUST surface as Err");

        assert!(
            matches!(
                err,
                UsageCollectorError::NotFound { ref resource_type, ref name, ref detail, .. }
                    if resource_type == USAGE_RECORD_RESOURCE
                        && name == &missing.to_string()
                        && detail.contains(&missing.to_string())
            ),
            "a target that resolves to nothing MUST surface as NotFound naming \
             the identifier the submission resolved; got {err:?}",
        );
        assert!(
            plugin.last_create_records_input().is_none(),
            "an unresolvable target MUST short-circuit before the persist SPI",
        );
    }

    /// Withdrawing a withdrawal resolves the measurement they both copy, not
    /// the withdrawal, so "no invalidation of an invalidation" needs no check:
    /// the submission built to withdraw a withdrawal is, input for input, the
    /// submission that withdraws the measurement.
    #[tokio::test]
    async fn create_usage_record_cannot_express_a_withdrawal_of_a_withdrawal() {
        let plugin = HappyPathPlugin::new();
        let tenant_id = Uuid::from_u128(0x707);
        let measurement = target_row(tenant_id, "idem-withdraw-a-withdrawal");
        let withdrawal = projected(&counter_withdrawal(tenant_id, "idem-withdraw-a-withdrawal"));
        assert_ne!(measurement.id, withdrawal.id, "test premise");
        plugin.set_get_record(measurement.clone());
        plugin.set_create_records(vec![Ok(withdrawal.clone())]);

        let service = service_with_permit(
            Arc::clone(&plugin) as _,
            "test.singular.withdrawal_of_a_withdrawal.records.v1",
        );

        service
            .create_usage_records(
                &authenticated_ctx(),
                vec![counter_withdrawal(tenant_id, "idem-withdraw-a-withdrawal")],
            )
            .await
            .expect("the submission is not refused request-wide")
            .into_iter()
            .next()
            .expect("one entry in, one slot out")
            .expect("the submission withdraws the measurement, and is accepted");

        assert_eq!(
            plugin.get_usage_record_inputs(),
            vec![measurement.id],
            "the derivation asks for the measurement, never for the withdrawal",
        );
    }

    /// The singular path's mirror of the batch row-contradiction guard: a
    /// plugin answering the derived identifier with a row that is itself an
    /// invalidation is refused as a host-invariant breach. The submission is a
    /// faithful copy of what came back, so without the guard it is accepted.
    #[tokio::test]
    async fn create_usage_record_refuses_a_plugin_that_answers_with_an_invalidation_row() {
        let plugin = HappyPathPlugin::new();
        let tenant_id = Uuid::from_u128(0x70C);
        let target = target_id(tenant_id, "idem-corrupt-row");
        let mut row = target_row(tenant_id, "idem-corrupt-row");
        row.invalidation = Some(Invalidation {
            target: Uuid::from_u128(0x81F),
            reason: ReasonCode::new("emitter_defect").expect("valid reason code"),
        });
        assert_eq!(
            row.id, target,
            "test premise: the row answers the derived id"
        );
        plugin.set_get_record(row);

        let submission = counter_withdrawal(tenant_id, "idem-corrupt-row");
        plugin.set_create_records(vec![Ok(projected(&submission))]);

        let service = service_with_permit(
            Arc::clone(&plugin) as _,
            "test.singular.invalidation_row.records.v1",
        );

        let result = service
            .create_usage_records(&authenticated_ctx(), vec![submission])
            .await
            .expect(
                "a row contradicting its own identifier is a per-record breach, caught inside \
                 `resolve_invalidation_targets`, never a request-wide refusal",
            )
            .into_iter()
            .next()
            .expect("one entry in, one slot out");

        let err = result.expect_err("a row contradicting its own identifier MUST surface as Err");

        match err {
            UsageCollectorError::Internal { detail } => assert!(
                detail.contains(&target.to_string()),
                "the breach MUST name the id the gateway asked for: {detail}"
            ),
            other => panic!("expected a host-invariant breach, got {other:?}"),
        }
        assert!(
            plugin.last_create_records_input().is_none(),
            "the breach MUST short-circuit before the persist SPI",
        );
    }

    /// A withdrawal departing from its target in a copied field ⇒
    /// `InvalidationFieldMismatch` **naming the field that differs**. The
    /// message names what differs and never what it differs from: the
    /// target was read unscoped, so echoing its value would make the
    /// rejection an oracle for a row this caller has no grant for.
    #[tokio::test]
    async fn create_usage_record_unfaithful_copy_names_the_field_that_differs() {
        let plugin = HappyPathPlugin::new();
        let tenant_id = Uuid::from_u128(0x708);

        let mut row = target_row(tenant_id, "idem-unfaithful");
        // `99.9`, not `999`: the detail asserted below carries the derived
        // target UUID, so a hex-digits-only sentinel would also match a
        // three-character window of that UUID and flake. The `.` is not a hex
        // digit, which makes the assertion structurally immune.
        row.quantity = crate::domain::test_support::qty("99.9");
        plugin.set_get_record(row);

        let service = service_with_permit(
            Arc::clone(&plugin) as _,
            "test.singular.field_mismatch.records.v1",
        );

        let result = service
            .create_usage_records(
                &authenticated_ctx(),
                vec![counter_withdrawal(tenant_id, "idem-unfaithful")],
            )
            .await
            .expect(
                "an unfaithful copy is a per-record validation rejection, caught inside \
                 `resolve_invalidation_targets`, never a request-wide refusal",
            )
            .into_iter()
            .next()
            .expect("one entry in, one slot out");

        let err = result.expect_err("an unfaithful copy MUST surface as Err");

        match err {
            UsageCollectorError::InvalidArgument {
                reason: ValidationReason::InvalidationFieldMismatch,
                field,
                detail,
                ..
            } => {
                assert_eq!(
                    field, "quantity",
                    "the rejection MUST name the field that differs"
                );
                assert!(
                    !detail.contains("99.9"),
                    "the rejection MUST NOT echo the target's value: {detail}",
                );
            }
            other => panic!(
                "a departure in a copied field MUST surface as \
                 InvalidationFieldMismatch; got {other:?}",
            ),
        }
    }

    /// A submission breaking the copy rule **and** the metadata rule is
    /// told about the copy: the metadata it would be sent to fix is
    /// metadata it has to copy from the target regardless.
    #[tokio::test]
    async fn create_usage_record_copy_mismatch_outranks_a_metadata_rejection() {
        let plugin = HappyPathPlugin::new();
        let tenant_id = Uuid::from_u128(0x709);

        let mut row = target_row(tenant_id, "idem-both-broken");
        row.quantity = crate::domain::test_support::qty("999");
        plugin.set_get_record(row);

        let mut submission = counter_withdrawal(tenant_id, "idem-both-broken");
        submission.metadata.insert(
            MetadataKey::new("zone").expect("valid metadata key"),
            "eu-1".to_owned(),
        );

        // The declaration declares `region` alone, so `zone` fails the
        // closed-shape check.
        let service = ServiceFixture::default()
            .with_source(fake_declaration_source_with_metadata(&["region"]))
            .build(
                Arc::clone(&plugin) as _,
                "test.singular.priority.records.v1",
            );

        let result = service
            .create_usage_records(&authenticated_ctx(), vec![submission])
            .await
            .expect(
                "both rejections are per-record validation outcomes, never a request-wide \
                 refusal",
            )
            .into_iter()
            .next()
            .expect("one entry in, one slot out");

        let err = result.expect_err("a submission breaking both rules MUST surface as Err");

        match err {
            UsageCollectorError::InvalidArgument { reason, .. } => assert_eq!(
                reason,
                ValidationReason::InvalidationFieldMismatch,
                "the copy rejection MUST outrank the metadata one",
            ),
            other => panic!("expected a validation rejection, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn create_usage_record_store_conflict_on_a_withdrawal_is_already_invalidated() {
        let plugin = HappyPathPlugin::new();
        let tenant_id = Uuid::from_u128(0x70A);
        let target = target_id(tenant_id, "idem-withdrawn");
        plugin.set_get_record(target_row(tenant_id, "idem-withdrawn"));
        let stored = projected(&counter_withdrawal(tenant_id, "idem-withdrawn"));
        let stored_id = stored.id;
        plugin.set_create_records(vec![Err(UsageCollectorPluginError::idempotency_conflict(
            "idem-withdrawn",
            crate::domain::test_support::as_stored(stored),
        ))]);
        let service = service_with_permit(
            Arc::clone(&plugin) as _,
            "test.singular.already_invalidated.records.v1",
        );

        let mut second = counter_withdrawal(tenant_id, "idem-withdrawn");
        second.invalidation = Some(ReasonCode::new("late_correction").expect("valid reason code"));

        let result = service
            .create_usage_records(&authenticated_ctx(), vec![second])
            .await
            .expect(
                "the idempotency conflict is lifted by `settle_dispatched` into the slot, not \
                 the outer Err - the dispatch call itself succeeded",
            )
            .into_iter()
            .next()
            .expect("one entry in, one slot out");

        let err = result.expect_err("a second withdrawal MUST surface as Err");

        match err {
            UsageCollectorError::Conflict {
                outcome:
                    ConflictOutcome::AlreadyInvalidated {
                        invalidated_by,
                        reason_code,
                    },
                name,
                ..
            } => {
                assert_eq!(name, target.to_string());
                assert_eq!(invalidated_by, stored_id);
                assert_eq!(reason_code.as_str(), "emitter_defect");
            }
            other => panic!("expected AlreadyInvalidated; got {other:?}"),
        }
    }

    /// A backend fault on the target read MUST fail the submission rather
    /// than reject it: an unreadable target is not an absent one.
    #[tokio::test]
    async fn create_usage_record_transient_target_lookup_lifts_to_service_unavailable() {
        let plugin = HappyPathPlugin::new();
        plugin.set_get_usage_record_transient("target store timed out", Some(7));

        let service = service_with_permit(
            Arc::clone(&plugin) as _,
            "test.singular.target_transient.records.v1",
        );

        let result = service
            .create_usage_records(
                &authenticated_ctx(),
                vec![counter_withdrawal(Uuid::from_u128(0x70B), "idem-t")],
            )
            .await
            .expect(
                "`resolve_invalidation_targets` has no outer-Err path at all: every \
                 `get_usage_record` outcome, transient included, is written to the slot",
            )
            .into_iter()
            .next()
            .expect("one entry in, one slot out");

        let err = result.expect_err("a transient target read MUST surface as Err");

        match err {
            UsageCollectorError::ServiceUnavailable {
                detail,
                retry_after_seconds,
            } => {
                assert_eq!(detail, "target store timed out");
                assert_eq!(retry_after_seconds, Some(7));
            }
            other => panic!("a transient target read MUST NOT become a rejection; got {other:?}"),
        }
    }

    /// A plugin answering `get_usage_record(a)` with a different row MUST be
    /// refused here too. The submission is again a faithful copy of the
    /// returned row, so without the id check it is accepted.
    #[tokio::test]
    async fn create_usage_record_refuses_a_plugin_that_answers_with_the_wrong_row() {
        let plugin = HappyPathPlugin::new();
        let tenant_id = Uuid::from_u128(0x70C);
        let requested = target_id(tenant_id, "idem-wrong-row");
        let answered = target_id(tenant_id, "idem-other-row");
        plugin.set_get_record(target_row(tenant_id, "idem-other-row"));

        // Programmed so a gateway that believed the row fails visibly as
        // `Ok(..)`. The two rows differ in their idempotency key alone, which
        // the copy comparator does not read, so only the id check catches it.
        let submission = counter_withdrawal(tenant_id, "idem-wrong-row");
        plugin.set_create_records(vec![Ok(projected(&submission))]);

        let service = service_with_permit(
            Arc::clone(&plugin) as _,
            "test.singular.wrong_row.records.v1",
        );

        let result = service
            .create_usage_records(&authenticated_ctx(), vec![submission])
            .await
            .expect(
                "a mis-answering plugin is a per-record breach caught inside \
                 `resolve_invalidation_targets`, never a request-wide refusal",
            )
            .into_iter()
            .next()
            .expect("one entry in, one slot out");

        let err = result.expect_err("a mis-answering plugin MUST surface as Err");

        match err {
            UsageCollectorError::Internal { detail } => {
                assert!(detail.contains(&requested.to_string()), "{detail}");
                assert!(!detail.contains(&answered.to_string()), "{detail}");
            }
            other => panic!(
                "a row that is not the row asked for MUST be a host-invariant \
                 breach, not an acceptance; got {other:?}",
            ),
        }
        assert!(
            plugin.last_create_records_input().is_none(),
            "the breach MUST short-circuit before the persist SPI",
        );
    }

    /// Happy path: PDP permit + catalog hit + metadata validation pass +
    /// persist SPI ⇒ `Ok(persisted_record)`. The persisted echo's `id` differs
    /// from the input so the two can be told apart.
    #[tokio::test]
    async fn create_usage_record_happy_path_returns_persisted_echo() {
        let plugin = HappyPathPlugin::new();

        let mut persisted = projected(&counter_record(
            Uuid::from_u128(0xCAFE),
            1,
            "idem-happy-persist",
        ));
        persisted.id = Uuid::from_u128(0xDEAD_C0DE);
        plugin.set_create_records(vec![Ok(persisted.clone())]);

        let service = service_with_permit(
            Arc::clone(&plugin) as _,
            "test.singular.happy_path.records.v1",
        );

        let input = counter_record(Uuid::from_u128(0x706), 1, "idem-happy-input");

        let returned = service
            .create_usage_records(&authenticated_ctx(), vec![input])
            .await
            .expect("the measurement is not refused request-wide")
            .into_iter()
            .next()
            .expect("one entry in, one slot out")
            .expect("happy path MUST return the persisted record");

        assert_eq!(
            returned.id, persisted.id,
            "the returned record MUST be the plugin's persisted echo \
             (not the caller's input)",
        );
        assert_eq!(
            plugin.get_usage_record_calls(),
            0,
            "an entry carrying no `invalidates` MUST NOT trigger a target lookup",
        );
    }

    #[tokio::test]
    async fn create_usage_record_not_converged_target_is_a_retryable_conflict() {
        let plugin = HappyPathPlugin::new();
        let tenant_id = Uuid::from_u128(0x70C);
        let target = target_id(tenant_id, "idem-nc");
        plugin.set_get_usage_record_not_converged(target);
        let service = service_with_permit(
            Arc::clone(&plugin) as _,
            "test.singular.not_converged.records.v1",
        );

        let result = service
            .create_usage_records(
                &authenticated_ctx(),
                vec![counter_withdrawal(tenant_id, "idem-nc")],
            )
            .await
            .expect(
                "a not-converged target is a per-record conflict caught inside \
                 `resolve_invalidation_targets`, never a request-wide refusal",
            )
            .into_iter()
            .next()
            .expect("one entry in, one slot out");

        let err = result.expect_err("a not-converged target is refused");

        match err {
            UsageCollectorError::Conflict {
                outcome: ConflictOutcome::TargetNotConverged { .. },
                name,
                ..
            } => assert_eq!(name, target.to_string()),
            other => panic!("expected TargetNotConverged, got {other:?}"),
        }
        assert_eq!(plugin.get_usage_record_converged_only_flags(), vec![true]);
        assert!(
            plugin.last_create_records_input().is_none(),
            "nothing is dispatched"
        );
    }

    #[tokio::test]
    async fn create_usage_record_naming_another_tenants_row_answers_as_absent() {
        let plugin = HappyPathPlugin::new();
        let caller_tenant = Uuid::from_u128(0x70D);
        let other_tenant = Uuid::from_u128(0x70E);
        let target = target_id(caller_tenant, "idem-cross");
        plugin.set_get_record(target_row(other_tenant, "idem-cross"));
        let service = service_with_permit(
            Arc::clone(&plugin) as _,
            "test.singular.cross_tenant.records.v1",
        );

        let result = service
            .create_usage_records(
                &authenticated_ctx(),
                vec![counter_withdrawal(caller_tenant, "idem-cross")],
            )
            .await
            .expect(
                "an out-of-scope target is a per-record rejection caught inside \
                 `resolve_invalidation_targets`, never a request-wide refusal",
            )
            .into_iter()
            .next()
            .expect("one entry in, one slot out");

        let err = result.expect_err("an out-of-scope target is refused");

        match err {
            UsageCollectorError::NotFound {
                reason: usage_collector_sdk::NotFoundReason::InvalidationTargetNotFound,
                name,
                ..
            } => assert_eq!(name, target.to_string()),
            other => panic!("an out-of-scope target must read as absent, got {other:?}"),
        }
        assert!(
            plugin.last_create_records_input().is_none(),
            "nothing is dispatched"
        );
        assert_eq!(plugin.get_usage_record_calls(), 1);
        assert_translatable_scope(&plugin, "resolve_invalidation_targets");
        let scope = plugin.last_get_scope().expect("the lookup dispatched");
        assert!(
            scope.contains(&format!("{caller_tenant:?}"))
                && !scope.contains(&format!("{other_tenant:?}"))
                && !scope.contains(&format!("{target:?}")),
            "the lookup reads under the caller's compiled permit, not `id eq <target>` or the \
             target's tenant: {scope}"
        );
    }

    /// A permit the per-entry gate admits but whose scope does not compile
    /// fails a withdrawal closed as `PermissionDenied`, before any target
    /// read. The ordinary record submitted first under the same permit is
    /// accepted, which is what proves the gate admitted it.
    #[tokio::test]
    async fn create_usage_record_withdrawal_under_a_permit_scope_that_does_not_compile_is_permission_denied()
     {
        let plugin = HappyPathPlugin::new();
        let tenant_id = Uuid::from_u128(0x70F);
        plugin.set_get_record(target_row(tenant_id, "idem-uncompilable-withdrawal"));
        let ordinary = counter_record(tenant_id, 10, "idem-uncompilable-plain");
        plugin.set_create_records(vec![Ok(projected(&ordinary))]);
        let service = ServiceFixture::default()
            .with_source(fake_declaration_source_with_fold("SUM"))
            .with_resolver(Arc::new(UncompilableSiblingPermitResolver))
            .build(
                Arc::clone(&plugin) as _,
                "test.singular.uncompilable_scope.records.v1",
            );

        service
            .create_usage_records(&authenticated_ctx(), vec![ordinary])
            .await
            .expect("the measurement is not refused request-wide")
            .into_iter()
            .next()
            .expect("one entry in, one slot out")
            .expect("the gate admits the tuple through the tenant constraint");

        let result = service
            .create_usage_records(
                &authenticated_ctx(),
                vec![counter_withdrawal(
                    tenant_id,
                    "idem-uncompilable-withdrawal",
                )],
            )
            .await
            .expect(
                "a scope-compile failure is projected onto the slot inside \
                 `create_usage_records_inner`'s per-entry loop, never the outer Err",
            )
            .into_iter()
            .next()
            .expect("one entry in, one slot out");

        let err = result.expect_err("a permit scope that does not compile is refused");

        assert!(
            matches!(err, UsageCollectorError::PermissionDenied { .. }),
            "a permit scope that does not compile fails closed, got {err:?}",
        );
        assert_eq!(
            plugin.get_usage_record_calls(),
            0,
            "no target is read under a scope that does not compile",
        );
    }

    /// A grant narrowed by meter admits a measurement and **denies a
    /// withdrawal**.
    ///
    /// `gts_type_id` is scopable on the attribution tuple, so
    /// `authz::scope_admits_attribution_tuple` admits the measurement. But a
    /// withdrawal reads its target under the `create` permit's own compiled
    /// scope (`authz::scope_to_odata_filter`), whose projection refuses the
    /// identifier because the storage filter vocabulary excludes it — so the
    /// reservation fires on part of the ingestion path, not only on reads.
    /// The asserted wording is what tells an operator which half it lost.
    #[tokio::test]
    async fn create_usage_record_under_a_meter_narrowed_grant_measures_but_cannot_withdraw() {
        let plugin = HappyPathPlugin::new();
        let tenant_id = Uuid::from_u128(0x71A);
        plugin.set_get_record(target_row(tenant_id, "idem-meter-narrowed-withdrawal"));
        let ordinary = counter_record(tenant_id, 10, "idem-meter-narrowed-plain");
        assert_eq!(
            ordinary.gts_type_id.as_ref(),
            COUNTER_GTS_ID,
            "test premise: the fixture is attributed to the meter the grant names, \
             so the gate's verdict turns on nothing else"
        );
        plugin.set_create_records(vec![Ok(projected(&ordinary))]);
        let service = ServiceFixture::default()
            .with_source(fake_declaration_source_with_fold("SUM"))
            .with_resolver(MeterNarrowingPermitResolver::sole(COUNTER_GTS_ID))
            .build(
                Arc::clone(&plugin) as _,
                "test.singular.meter_narrowed_scope.records.v1",
            );

        service
            .create_usage_records(&authenticated_ctx(), vec![ordinary])
            .await
            .expect("the measurement is not refused request-wide")
            .into_iter()
            .next()
            .expect("one entry in, one slot out")
            .expect("the gate admits a measurement attributed to the granted meter");

        let result = service
            .create_usage_records(
                &authenticated_ctx(),
                vec![counter_withdrawal(
                    tenant_id,
                    "idem-meter-narrowed-withdrawal",
                )],
            )
            .await
            .expect(
                "a scope-compile failure is projected onto the slot inside \
                 `create_usage_records_inner`'s per-entry loop, never the outer Err",
            )
            .into_iter()
            .next()
            .expect("one entry in, one slot out");

        let err = result.expect_err(
            "the withdrawal's target lookup projects the same scope, and the meter \
             cannot be projected",
        );

        let UsageCollectorError::PermissionDenied { detail } = &err else {
            panic!("a scope the target lookup cannot project fails closed, got {err:?}");
        };
        assert!(
            detail.contains("reserved") && detail.contains("gts_type_id"),
            "the refusal an operator reads must name the reserved property; got {detail:?}"
        );
        assert!(
            detail.contains("withdraw"),
            "and it must say which half of the feature it took away, because \
             'denied' alone does not distinguish a meter-narrowed grant that \
             measures from one that cannot; got {detail:?}"
        );
        assert_eq!(
            plugin.get_usage_record_calls(),
            0,
            "no target is read under a scope the projection refuses",
        );
    }
}

// ── Batch size-cap guard in `create_usage_records` ─────────────────────────
//
// Both out-of-bounds arms of the `actual == 0 || actual > max_batch_records`
// entry gate MUST reject with `invalid_batch_size` *before* any plugin
// dispatch. The fixture configures a small cap so the over-cap case does not
// need a 100+-record batch.
#[cfg(test)]
mod batch_size_cap_tests {
    use std::collections::BTreeMap;
    use std::sync::Arc;
    use toolkit_gts::gts_id;

    use usage_collector_sdk::{
        CreateUsageRecord, EntryType, IdempotencyKey, MeterTypeId, ResourceRef,
        UsageCollectorError, UsageCollectorPluginV1, ValidationReason,
    };
    use uuid::Uuid;

    use crate::domain::test_support::{
        HappyPathPlugin, ServiceFixture, authenticated_ctx, recent_window_end, recent_window_start,
    };

    const GTS_ID: &str = gts_id!("cf.core.uc.usage_record.v1~cf.mini_chat._.tokens_consumed.v1~");

    const CAP: usize = 3;

    fn input_record(idem: &str) -> CreateUsageRecord {
        CreateUsageRecord {
            entry_type: EntryType::Record,
            gts_type_id: MeterTypeId::new(GTS_ID).expect("valid gts_type_id"),
            tenant_id: Uuid::from_u128(1),
            resource_ref: ResourceRef::new("rsc-batch-cap", "compute.vm")
                .expect("valid resource ref"),
            subject_ref: None,
            metadata: BTreeMap::new(),
            quantity: crate::domain::test_support::qty("1"),
            idempotency_key: Some(IdempotencyKey::new(idem).expect("valid idempotency key")),
            invalidation: None,
            window_start: recent_window_start(),
            window_end: recent_window_end(),
        }
    }

    fn assert_invalid_batch_size(err: &UsageCollectorError) {
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
    }

    /// Empty batch (`actual == 0`) → `invalid_batch_size`, plugin untouched.
    #[tokio::test]
    async fn create_usage_records_rejects_empty_batch_without_dispatch() {
        let plugin = HappyPathPlugin::new();

        let service = ServiceFixture::default().with_max_batch_records(CAP).build(
            Arc::clone(&plugin) as Arc<dyn UsageCollectorPluginV1>,
            "test.batch_cap.empty.records.v1",
        );

        let err = service
            .create_usage_records(&authenticated_ctx(), Vec::new())
            .await
            .expect_err("empty batch MUST reject");
        assert_invalid_batch_size(&err);

        assert!(
            plugin.last_create_records_input().is_none(),
            "empty batch MUST short-circuit before any plugin dispatch",
        );
    }

    /// Over-cap batch (`CAP + 1`) → `invalid_batch_size`, plugin untouched.
    #[tokio::test]
    async fn create_usage_records_rejects_over_cap_batch_without_dispatch() {
        let plugin = HappyPathPlugin::new();

        let service = ServiceFixture::default().with_max_batch_records(CAP).build(
            Arc::clone(&plugin) as Arc<dyn UsageCollectorPluginV1>,
            "test.batch_cap.over.records.v1",
        );

        let input: Vec<CreateUsageRecord> = (0..=CAP)
            .map(|i| input_record(&format!("idem-cap-{i}")))
            .collect();
        assert_eq!(input.len(), CAP + 1);

        let err = service
            .create_usage_records(&authenticated_ctx(), input)
            .await
            .expect_err("over-cap batch MUST reject");
        assert_invalid_batch_size(&err);

        assert!(
            plugin.last_create_records_input().is_none(),
            "over-cap batch MUST short-circuit before any plugin dispatch",
        );
    }

    /// The service enforces the configured cap it was built with, not a
    /// hard-coded constant.
    #[tokio::test]
    async fn the_configured_cap_is_the_one_enforced() {
        let plugin = HappyPathPlugin::new();
        let service = ServiceFixture::default()
            .with_max_batch_records(CAP)
            .build(plugin, "test.batch_cap.configured.records.v1");
        assert_eq!(service.max_batch_records(), CAP);
        let over: Vec<_> = (0..=CAP)
            .map(|i| input_record(&format!("cap-{i}")))
            .collect();
        let err = service
            .create_usage_records(&authenticated_ctx(), over)
            .await
            .expect_err("CAP + 1 must be refused");
        assert_invalid_batch_size(&err);
    }
}

// ── Service-level server-assigned stamps (in-process / SDK callers) ────────
//
// The create surface is identity-free: callers never supply an `id`. The
// domain `Service` is the single point where a submission acquires one, via
// the SDK projection its `entry_type` selects — see
// [`usage_collector_sdk::derive_usage_record_id`]. These tests drive the
// `Service` create methods directly (NOT through the REST handler) and assert
// the record the plugin RECEIVED carries the derivation. `origin` is stamped
// from the same call, so it is pinned here too.
#[cfg(test)]
mod server_assigned_stamp_tests {
    use std::collections::BTreeMap;
    use std::sync::Arc;

    use toolkit_gts::gts_id;
    use usage_collector_sdk::{
        CreateUsageRecord, EntryType, IdempotencyKey, MeterTypeId, RecordOrigin, ResourceRef,
        UsageCollectorPluginV1, derive_usage_record_id,
    };
    use uuid::Uuid;

    use crate::domain::Service;
    use crate::domain::test_support::{
        HappyPathPlugin, ServiceFixture, authenticated_ctx, fake_declaration_source_with_fold,
        projected, recent_window_end, recent_window_start,
    };

    const GTS_ID: &str = gts_id!("cf.core.uc.usage_record.v1~cf.mini_chat._.tokens_consumed.v1~");

    /// A [`ServiceFixture`] with an arbitrary working declaration — these
    /// tests exercise id derivation, not the declaration.
    fn service_with_permit(plugin: Arc<dyn UsageCollectorPluginV1>, suffix: &str) -> Arc<Service> {
        ServiceFixture::default()
            .with_source(fake_declaration_source_with_fold("SUM"))
            .build(plugin, suffix)
    }

    /// A create submission with a known dedup identity, so a passing assertion
    /// can only mean the service derived the dispatched record's id from it.
    fn input_record(tenant_id: Uuid, idem: &str) -> CreateUsageRecord {
        CreateUsageRecord {
            entry_type: EntryType::Record,
            gts_type_id: MeterTypeId::new(GTS_ID).expect("valid gts_type_id"),
            tenant_id,
            resource_ref: ResourceRef::new("rsc-derive", "compute.vm").expect("valid resource ref"),
            subject_ref: None,
            metadata: BTreeMap::new(),
            quantity: crate::domain::test_support::qty("1"),
            idempotency_key: Some(IdempotencyKey::new(idem).expect("valid idempotency key")),
            invalidation: None,
            window_start: recent_window_start(),
            window_end: recent_window_end(),
        }
    }

    /// One-entry `create_usage_records`: the dispatched record's `id` MUST be
    /// the service-derived value.
    #[tokio::test]
    async fn create_usage_record_stamps_derived_id() {
        let plugin = HappyPathPlugin::new();
        let tenant_id = Uuid::from_u128(0xD1);
        let idem = "idem-derive-singular";
        let input = input_record(tenant_id, idem);
        let expected = derive_usage_record_id(
            input.tenant_id,
            &input.gts_type_id,
            input
                .idempotency_key
                .as_ref()
                .expect("fixture record carries a key"),
            input.window_start,
            input.window_end,
            input.entry_type(),
        );

        // The plugin echoes the record back so the persist SPI succeeds; the
        // assertion reads the CAPTURED dispatched record.
        plugin.set_create_records(vec![Ok(projected(&input))]);

        let service = service_with_permit(
            Arc::clone(&plugin) as Arc<dyn UsageCollectorPluginV1>,
            "test.derived_id.singular.records.v1",
        );

        service
            .create_usage_records(&authenticated_ctx(), vec![input])
            .await
            .expect("the measurement is not refused request-wide")
            .into_iter()
            .next()
            .expect("one entry in, one slot out")
            .expect("happy path MUST accept the record");

        let dispatched = plugin
            .last_create_records_input()
            .expect("plugin received the dispatched batch")
            .into_iter()
            .next()
            .expect("one entry in, one dispatched record out");
        assert_eq!(
            dispatched.id, expected,
            "the SERVICE MUST stamp the dispatched record's id with \
             derive_usage_record_id(tenant_id, gts_type_id, idempotency_key, \
             window_start, window_end, entry_type) - this guards the \
             in-process (non-REST) caller path independently of the handler",
        );
    }

    /// Batch `create_usage_records`: every dispatched record's `id` MUST be its
    /// own service-derived value.
    #[tokio::test]
    async fn create_usage_records_stamps_derived_id() {
        let plugin = HappyPathPlugin::new();
        let tenant_id = Uuid::from_u128(0xD2);
        let idems = ["idem-derive-batch-0", "idem-derive-batch-1"];
        let input: Vec<CreateUsageRecord> = idems
            .iter()
            .map(|idem| input_record(tenant_id, idem))
            .collect();
        let expected: Vec<Uuid> = input
            .iter()
            .map(|r| {
                derive_usage_record_id(
                    r.tenant_id,
                    &r.gts_type_id,
                    r.idempotency_key
                        .as_ref()
                        .expect("fixture record carries a key"),
                    r.window_start,
                    r.window_end,
                    r.entry_type(),
                )
            })
            .collect();

        plugin.set_create_records(input.iter().map(|r| Ok(projected(r))).collect());

        let service = service_with_permit(
            Arc::clone(&plugin) as Arc<dyn UsageCollectorPluginV1>,
            "test.derived_id.batch.records.v1",
        );

        let results = service
            .create_usage_records(&authenticated_ctx(), input)
            .await
            .expect("batch dispatch succeeded");
        assert!(results.iter().all(Result::is_ok));

        let dispatched = plugin
            .last_create_records_input()
            .expect("plugin received the dispatched batch");
        let dispatched_ids: Vec<Uuid> = dispatched.iter().map(|r| r.id).collect();
        assert_eq!(
            dispatched_ids, expected,
            "the SERVICE MUST stamp each dispatched record's id with its own \
             derive_usage_record_id(tenant_id, gts_type_id, idempotency_key, \
             window_start, window_end), overwriting the caller-supplied ids - \
             this guards the in-process (non-REST) batch caller path \
             independently of the handler",
        );
    }

    /// Both live ingestion surfaces MUST stamp `origin = live` on what they
    /// dispatch. Read off the DISPATCHED record, not the returned one: the
    /// plugin echoes a fixture back, so a returned `Live` would only prove the
    /// fixture was built `Live`.
    #[tokio::test]
    async fn the_live_surfaces_stamp_origin_live() {
        let plugin = HappyPathPlugin::new();
        let tenant_id = Uuid::from_u128(0xD3);
        let single = input_record(tenant_id, "idem-origin-singular");
        let batch = vec![input_record(tenant_id, "idem-origin-batch")];

        plugin.set_create_records(vec![Ok(projected(&single))]);

        let service = service_with_permit(
            Arc::clone(&plugin) as Arc<dyn UsageCollectorPluginV1>,
            "test.origin_stamp.live.records.v1",
        );

        service
            .create_usage_records(&authenticated_ctx(), vec![single])
            .await
            .expect("the measurement is not refused request-wide")
            .into_iter()
            .next()
            .expect("one entry in, one slot out")
            .expect("happy path MUST accept the record");
        assert_eq!(
            plugin
                .last_create_records_input()
                .expect("plugin received the dispatched batch")
                .into_iter()
                .next()
                .expect("one entry in, one dispatched record out")
                .origin,
            RecordOrigin::Live,
            "create_usage_records IS the live route, so the entry it \
             dispatches MUST carry origin = live",
        );

        plugin.set_create_records(batch.iter().map(|r| Ok(projected(r))).collect());

        let results = service
            .create_usage_records(&authenticated_ctx(), batch)
            .await
            .expect("batch dispatch succeeded");
        assert!(results.iter().all(Result::is_ok));
        let dispatched_origins: Vec<RecordOrigin> = plugin
            .last_create_records_input()
            .expect("plugin received the dispatched batch")
            .iter()
            .map(|r| r.origin)
            .collect();
        assert_eq!(
            dispatched_origins,
            vec![RecordOrigin::Live],
            "create_usage_records IS the live route, so every entry it \
             dispatches MUST carry origin = live",
        );
    }
}

// ── Server-stamped `accepted_at` ────────────────────────────────────────────
#[cfg(test)]
mod acceptance_stamp_tests {
    use std::collections::BTreeMap;
    use std::sync::Arc;

    use toolkit_gts::gts_id;
    use usage_collector_sdk::{
        CreateUsageRecord, EntryType, IdempotencyKey, MeterTypeId, ResourceRef,
        UsageCollectorPluginV1,
    };
    use uuid::Uuid;

    use crate::domain::test_support::{
        HappyPathPlugin, ServiceFixture, authenticated_ctx, fake_declaration_source_with_fold, qty,
        recent_window_end, recent_window_start,
    };

    const GTS_ID: &str = gts_id!("cf.core.uc.usage_record.v1~cf.mini_chat._.tokens_consumed.v1~");

    fn submission(idem: &str) -> CreateUsageRecord {
        CreateUsageRecord {
            entry_type: EntryType::Record,
            gts_type_id: MeterTypeId::new(GTS_ID).unwrap(),
            tenant_id: Uuid::from_u128(1),
            resource_ref: ResourceRef::new("rsc-stamp", "compute.vm").unwrap(),
            subject_ref: None,
            metadata: BTreeMap::new(),
            quantity: qty("1"),
            idempotency_key: Some(IdempotencyKey::new(idem).unwrap()),
            invalidation: None,
            window_start: recent_window_start(),
            window_end: recent_window_end(),
        }
    }

    #[tokio::test]
    async fn every_entry_of_one_request_carries_one_microsecond_acceptance_instant() {
        let plugin = HappyPathPlugin::new();
        let service = ServiceFixture::default()
            .with_source(fake_declaration_source_with_fold("SUM"))
            .build(
                Arc::clone(&plugin) as Arc<dyn UsageCollectorPluginV1>,
                "test.accepted_at.stamp.records.v1",
            );
        let before = time::OffsetDateTime::now_utc();
        // The plugin's `create_records_response` is left unprogrammed, so the
        // call errors. What is asserted is what the plugin was HANDED, which
        // `HappyPathPlugin` captures regardless of the outcome.
        let _dispatch_outcome = service
            .create_usage_records(
                &authenticated_ctx(),
                vec![
                    submission("stamp-a"),
                    submission("stamp-b"),
                    submission("stamp-c"),
                ],
            )
            .await;
        let after = time::OffsetDateTime::now_utc();

        let dispatched = plugin
            .last_create_records_input()
            .expect("the batch reached the plugin");
        assert_eq!(dispatched.len(), 3);
        let stamp = dispatched[0].accepted_at;
        assert!(
            dispatched.iter().all(|r| r.accepted_at == stamp),
            "one instant per request"
        );
        assert_eq!(
            stamp.nanosecond() % 1_000,
            0,
            "stamped at microsecond precision"
        );
        let floor = before.replace_microsecond(before.microsecond()).unwrap();
        assert!(floor <= stamp && stamp <= after, "stamped during the call");
    }
}

// ── Covered-period preconditions on the batch path ─────────────────────────
//
// The identity derivation is per-submission and fallible, so a rejected
// covered period is a PER-RECORD outcome routed to its own input index —
// never a batch-level failure, never a slot shifted onto a neighbour. The
// surviving vector is not index-aligned with the input, which is the mistake
// these tests exist to catch.
#[cfg(test)]
mod covered_period_batch_tests {
    use std::collections::BTreeMap;
    use std::sync::Arc;

    use toolkit_gts::gts_id;
    use usage_collector_sdk::{
        CreateUsageRecord, EntryType, IdempotencyKey, MeterTypeId, ResourceRef,
        UsageCollectorError, UsageCollectorPluginV1,
    };
    use uuid::Uuid;

    use crate::domain::Service;
    use crate::domain::test_support::{
        DenyOneResourceResolver, HappyPathPlugin, ServiceFixture, authenticated_ctx,
        fake_declaration_source_with_fold, projected, recent_window_end, recent_window_start,
    };

    const GTS_ID: &str = gts_id!("cf.core.uc.usage_record.v1~cf.mini_chat._.tokens_consumed.v1~");

    fn service_with_permit(plugin: Arc<dyn UsageCollectorPluginV1>, suffix: &str) -> Arc<Service> {
        ServiceFixture::default()
            .with_source(fake_declaration_source_with_fold("SUM"))
            .build(plugin, suffix)
    }

    /// Tenant `2` clears the fixture PDP; distinct `idem` values keep the
    /// submissions distinct without changing their attribution tuple, so the
    /// batch takes ONE PDP decision shared by every record — the arrangement
    /// in which a mis-routed index is invisible unless each outcome is
    /// asserted individually.
    fn submission(idem: &str) -> CreateUsageRecord {
        submission_for("rsc-period", idem)
    }

    fn submission_for(resource_id: &str, idem: &str) -> CreateUsageRecord {
        CreateUsageRecord {
            entry_type: EntryType::Record,
            gts_type_id: MeterTypeId::new(GTS_ID).expect("valid gts_type_id"),
            tenant_id: Uuid::from_u128(2),
            resource_ref: ResourceRef::new(resource_id, "compute.vm").expect("valid resource ref"),
            subject_ref: None,
            metadata: BTreeMap::new(),
            quantity: crate::domain::test_support::qty("1"),
            idempotency_key: Some(IdempotencyKey::new(idem).expect("valid idempotency key")),
            invalidation: None,
            window_start: recent_window_start(),
            window_end: recent_window_end(),
        }
    }

    fn rejected_field(
        result: &Result<usage_collector_sdk::UsageRecord, UsageCollectorError>,
    ) -> &str {
        match result {
            Err(UsageCollectorError::InvalidArgument { field, .. }) => field.as_str(),
            other => panic!("expected an InvalidArgument rejection, got {other:?}"),
        }
    }

    /// A batch of [valid, inverted-period, valid]: the middle submission is
    /// rejected at its OWN index and the two valid ones still reach the
    /// plugin, in input order.
    #[tokio::test]
    async fn a_rejected_period_surfaces_at_its_own_input_index() {
        let plugin = HappyPathPlugin::new();
        let good_0 = submission("idem-period-0");
        let mut bad = submission("idem-period-1");
        bad.window_end = bad.window_start - time::Duration::seconds(1);
        let good_2 = submission("idem-period-2");

        // Programming exactly two responses is itself a guard: a batch that
        // dispatched three would trip the dispatched-vs-returned invariant.
        plugin.set_create_records(vec![Ok(projected(&good_0)), Ok(projected(&good_2))]);
        let service = service_with_permit(
            Arc::clone(&plugin) as Arc<dyn UsageCollectorPluginV1>,
            "test.batch.period_mixed.records.v1",
        );

        let results = service
            .create_usage_records(
                &authenticated_ctx(),
                vec![good_0.clone(), bad, good_2.clone()],
            )
            .await
            .expect("a per-record period rejection MUST NOT fail the batch");

        assert_eq!(results.len(), 3, "one result slot per input submission");
        assert!(results[0].is_ok(), "index 0 must be accepted: {results:?}");
        assert_eq!(
            rejected_field(&results[1]),
            "window_end",
            "the rejection must land at index 1, attributed to window_end",
        );
        assert!(results[2].is_ok(), "index 2 must be accepted: {results:?}");

        let dispatched = plugin
            .last_create_records_input()
            .expect("plugin received the surviving batch");
        assert_eq!(
            dispatched
                .iter()
                .map(|r| r.idempotency_key.as_str())
                .collect::<Vec<_>>(),
            vec!["idem-period-0", "idem-period-2"],
            "only the two valid submissions reach the plugin, in input order",
        );
    }

    /// A sub-microsecond bound is the other precondition, and it is rejected
    /// per-record on the batch path too — attributed to whichever bound
    /// carried it.
    #[tokio::test]
    async fn a_sub_microsecond_bound_surfaces_at_its_own_input_index() {
        let plugin = HappyPathPlugin::new();
        let mut bad = submission("idem-period-sub-us");
        bad.window_start = bad.window_start.replace_nanosecond(1).expect("valid nanos");
        let good = submission("idem-period-clean");

        plugin.set_create_records(vec![Ok(projected(&good))]);
        let service = service_with_permit(
            Arc::clone(&plugin) as Arc<dyn UsageCollectorPluginV1>,
            "test.batch.period_sub_us.records.v1",
        );

        let results = service
            .create_usage_records(&authenticated_ctx(), vec![bad, good.clone()])
            .await
            .expect("a per-record period rejection MUST NOT fail the batch");

        assert_eq!(results.len(), 2);
        assert_eq!(rejected_field(&results[0]), "window_start");
        assert!(results[1].is_ok(), "index 1 must be accepted: {results:?}");
    }

    /// Every submission rejected: the batch still returns `Ok` with a filled
    /// slot per input, and the plugin is never dispatched.
    ///
    /// `HappyPathPlugin` is left UNPROGRAMMED: an empty dispatch would surface
    /// as a batch-level `Err`, so `Ok(..)` is evidence the SPI was skipped
    /// rather than called with nothing. It also pins that the tail's "every
    /// slot populated" guard is satisfied by a converted-and-rejected slot,
    /// not by an invariant-breach `Internal`.
    #[tokio::test]
    async fn an_all_rejected_batch_returns_per_record_errors_without_dispatching() {
        let plugin = HappyPathPlugin::new();
        let mut first = submission("idem-period-all-0");
        first.window_end = first.window_start - time::Duration::seconds(1);
        let mut second = submission("idem-period-all-1");
        second.window_end = second
            .window_start
            .replace_nanosecond(7)
            .expect("valid nanos");

        let service = service_with_permit(
            Arc::clone(&plugin) as Arc<dyn UsageCollectorPluginV1>,
            "test.batch.period_all_rejected.records.v1",
        );

        let results = service
            .create_usage_records(&authenticated_ctx(), vec![first, second])
            .await
            .expect("an all-rejected batch is still an Ok batch");

        assert_eq!(results.len(), 2);
        assert_eq!(rejected_field(&results[0]), "window_end");
        assert_eq!(rejected_field(&results[1]), "window_end");
        assert!(
            plugin.last_create_records_input().is_none(),
            "no submission survived, so the persist SPI must not be dispatched",
        );
    }

    /// All three per-record outcomes in ONE batch — accepted,
    /// period-rejected, PDP-denied — deliberately interleaved so the
    /// rejected slot sits BETWEEN the other two.
    ///
    /// The interleaving is the point. The period-rejected submission never
    /// enters `derived`, so any pass that re-`enumerate()`s the surviving
    /// vector shifts the denial from input index 2 onto index 1, OVERWRITING
    /// the period error and promoting the denied record to accepted. A grouped
    /// layout would hide that as a mere invariant breach on an empty slot.
    #[tokio::test]
    async fn all_three_per_record_outcomes_coexist_at_their_own_input_indices() {
        let plugin = HappyPathPlugin::new();

        // Index 0: valid, permitted. Index 1: valid tuple but an inverted
        // period. Index 2: valid period, denied tuple (`rsc-DENY`).
        let accepted = submission_for("rsc-OK", "idem-three-way-accepted");
        let mut bad_period = submission_for("rsc-OK", "idem-three-way-period");
        bad_period.window_end = bad_period.window_start - time::Duration::seconds(1);
        let denied = submission_for("rsc-DENY", "idem-three-way-denied");

        // Exactly ONE record survives to the SPI; programming one response is
        // itself a guard against a batch that dispatched two.
        plugin.set_create_records(vec![Ok(projected(&accepted))]);

        let service = ServiceFixture::default()
            .with_source(fake_declaration_source_with_fold("SUM"))
            .with_resolver(DenyOneResourceResolver::new("rsc-DENY"))
            .build(
                Arc::clone(&plugin) as Arc<dyn UsageCollectorPluginV1>,
                "test.batch.period_three_way.records.v1",
            );

        let expected_accepted_id = projected(&accepted).id;
        let results = service
            .create_usage_records(&authenticated_ctx(), vec![accepted, bad_period, denied])
            .await
            .expect("a mixed batch is still an Ok batch");

        assert_eq!(
            results.len(),
            3,
            "one result slot per input submission, whatever each outcome was",
        );
        assert_eq!(
            results[0]
                .as_ref()
                .expect("index 0 is permitted and well-formed")
                .id,
            expected_accepted_id,
            "index 0 must carry its OWN derived id",
        );
        assert_eq!(
            rejected_field(&results[1]),
            "window_end",
            "index 1 must carry the period rejection naming the offending \
             bound - not the denial that follows it in the input",
        );
        assert!(
            matches!(
                results[2],
                Err(UsageCollectorError::PermissionDenied { .. })
            ),
            "index 2 must carry the PDP denial, not an acceptance: {:?}",
            results[2],
        );

        let dispatched = plugin
            .last_create_records_input()
            .expect("the one surviving submission reached the plugin");
        assert_eq!(
            dispatched
                .iter()
                .map(|r| r.idempotency_key.as_str())
                .collect::<Vec<_>>(),
            vec!["idem-three-way-accepted"],
            "neither the rejected period nor the denied tuple may be dispatched",
        );
    }
}

// ── The live path's two-sided covered-period bound, at the service ─────────
//
// `covered_period_tests.rs` pins the rule itself. These pin that the Ingestion
// Gateway applies it — before the PDP call and before any plugin dispatch —
// and that it governs an invalidation over the period the invalidation copies.
mod covered_period_bounds_tests {
    use std::collections::BTreeMap;
    use std::sync::Arc;

    use toolkit_gts::gts_id;
    use usage_collector_sdk::{
        BACKFILL_ROUTE_PATH, CreateUsageRecord, EntryType, IdempotencyKey, MeterTypeId, ReasonCode,
        ResourceRef, UsageCollectorError, UsageCollectorPluginV1, ValidationReason,
    };
    use uuid::Uuid;

    use crate::domain::Service;
    use crate::domain::test_support::{
        HappyPathPlugin, ServiceFixture, authenticated_ctx, fake_declaration_source_with_fold,
        projected, recent_window_end, recent_window_start,
    };

    const GTS_ID: &str = gts_id!("cf.core.uc.usage_record.v1~cf.mini_chat._.tokens_consumed.v1~");

    /// A [`ServiceFixture`] with an arbitrary working declaration — the bound
    /// is enforced ahead of the resolver, so the declaration only has to let
    /// an admitted entry through.
    fn service_with_permit(plugin: Arc<dyn UsageCollectorPluginV1>, suffix: &str) -> Arc<Service> {
        ServiceFixture::default()
            .with_source(fake_declaration_source_with_fold("SUM"))
            .build(plugin, suffix)
    }

    /// A submission the live path admits: the shared recent covered period,
    /// an hour long and closed an hour ago.
    fn fresh_record(tenant_id: Uuid, idem: &str) -> CreateUsageRecord {
        CreateUsageRecord {
            entry_type: EntryType::Record,
            gts_type_id: MeterTypeId::new(GTS_ID).expect("valid gts_type_id"),
            tenant_id,
            resource_ref: ResourceRef::new("rsc-bounds", "compute.vm").expect("valid resource ref"),
            subject_ref: None,
            metadata: BTreeMap::new(),
            quantity: crate::domain::test_support::qty("1"),
            idempotency_key: Some(IdempotencyKey::new(idem).expect("valid idempotency key")),
            invalidation: None,
            window_start: recent_window_start(),
            window_end: recent_window_end(),
        }
    }

    /// The same submission, differing in the covered period alone: a month
    /// back, well beyond the live past tolerance and nowhere near the
    /// boundary, so the outcome does not depend on runner load. Offset from
    /// the memoised [`recent_window_end`] rather than a fresh `now_utc()`, so
    /// two submissions built by separate calls carry the SAME period — a
    /// per-call clock read lands them microseconds apart, and a withdrawal
    /// built that way is refused for `InvalidationFieldMismatch` instead.
    fn stale_record(tenant_id: Uuid, idem: &str) -> CreateUsageRecord {
        let window_end = recent_window_end() - time::Duration::days(30);
        CreateUsageRecord {
            entry_type: EntryType::Record,
            window_start: window_end - time::Duration::hours(1),
            window_end,
            ..fresh_record(tenant_id, idem)
        }
    }

    fn invalid_argument(err: &UsageCollectorError) -> (&ValidationReason, &str) {
        match err {
            UsageCollectorError::InvalidArgument { reason, detail, .. } => (reason, detail),
            other => panic!("expected InvalidArgument, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn the_live_path_refuses_a_period_older_than_the_past_tolerance_and_names_the_route() {
        let plugin = HappyPathPlugin::new();
        let submission = stale_record(Uuid::from_u128(0xC1), "idem-stale");
        // Armed to succeed, so the rejection can only come from the bound.
        plugin.set_create_records(vec![Ok(projected(&submission))]);
        let service = service_with_permit(
            Arc::clone(&plugin) as Arc<dyn UsageCollectorPluginV1>,
            "test.bounds.live_past.record.v1",
        );

        let result = service
            .create_usage_records(&authenticated_ctx(), vec![submission])
            .await
            .expect(
                "a rejected covered period is a precondition of that submission's identity \
                 derivation, per-record, never a request-wide refusal",
            )
            .into_iter()
            .next()
            .expect("one entry in, one slot out");

        let err = result.expect_err("a period 30 days old is beyond the 48-hour live tolerance");

        let (reason, detail) = invalid_argument(&err);
        assert_eq!(*reason, ValidationReason::PastWindow);
        assert!(
            detail.contains(BACKFILL_ROUTE_PATH),
            "the rejection MUST name the route the entry belongs on: {detail}",
        );
        assert_eq!(
            plugin.last_create_records_input(),
            None,
            "a refused period MUST NOT reach the storage plugin",
        );
    }

    #[tokio::test]
    async fn the_live_path_refuses_a_period_ending_beyond_the_future_tolerance() {
        // The only test that reaches the future tolerance THROUGH the Service
        // — the unit tests build `CoveredPeriodBounds` by hand, so they never
        // exercise the configured value's route from `[usage_collector]` to
        // this call. Without this, a projection that fed `backfill_window` in
        // as the future tolerance would ship green.
        let plugin = HappyPathPlugin::new();
        // `recent_window_end()` is an hour BEHIND now, so two hours on from it
        // is an hour AHEAD — outside a five-minute tolerance and nowhere near
        // the boundary. Anchored on the memoised fixture for the reason
        // `stale_record` documents.
        let window_end = recent_window_end() + time::Duration::hours(2);
        let submission = CreateUsageRecord {
            entry_type: EntryType::Record,
            window_start: window_end - time::Duration::hours(1),
            window_end,
            ..fresh_record(Uuid::from_u128(0xC5), "idem-future")
        };
        plugin.set_create_records(vec![Ok(projected(&submission))]);
        let service = service_with_permit(
            Arc::clone(&plugin) as Arc<dyn UsageCollectorPluginV1>,
            "test.bounds.live_future.record.v1",
        );

        let result = service
            .create_usage_records(&authenticated_ctx(), vec![submission])
            .await
            .expect(
                "a rejected covered period is a precondition of that submission's identity \
                 derivation, per-record, never a request-wide refusal",
            )
            .into_iter()
            .next()
            .expect("one entry in, one slot out");

        let err =
            result.expect_err("a period ending an hour from now is beyond the 5-minute tolerance");

        let (reason, detail) = invalid_argument(&err);
        assert_eq!(*reason, ValidationReason::FutureWindow);
        assert!(
            !detail.contains(BACKFILL_ROUTE_PATH),
            "the backfill route lifts the PAST bound only, so pointing a \
             clock-skewed emitter at it would send a defect somewhere it is \
             just as invalid: {detail}",
        );
        assert_eq!(
            plugin.last_create_records_input(),
            None,
            "a refused period MUST NOT reach the storage plugin",
        );
    }

    #[tokio::test]
    async fn the_live_path_refuses_a_withdrawal_over_a_period_older_than_the_past_tolerance() {
        // The bound belongs to the path, not to the entry kind: an
        // invalidation is a faithful copy of its target, so its `window_end`
        // IS the target's, and withdrawing a closed month is refused on the
        // live path exactly as a fresh measurement of that month would be.
        //
        // If the bound read the arrival instant instead of the copied period,
        // this submission would be accepted and a correction of closed history
        // would persist reading `origin = live`.
        let tenant_id = Uuid::from_u128(0xC2);

        // A measurement whose period closed a month ago.
        let measurement = stale_record(tenant_id, "idem-target");
        let target_row = projected(&measurement);
        // A faithful copy of it, departing only in `entry_type` and the
        // reason: it repeats the target's idempotency key, which resolves the
        // target, and its covered period is the target's.
        let withdrawal = CreateUsageRecord {
            entry_type: EntryType::Invalidation,
            invalidation: Some(ReasonCode::new("emitter_defect").expect("valid reason code")),
            ..stale_record(tenant_id, "idem-target")
        };

        let plugin = HappyPathPlugin::new();
        plugin.set_get_record(target_row);
        plugin.set_create_records(vec![Ok(projected(&withdrawal))]);
        let service = service_with_permit(
            Arc::clone(&plugin) as Arc<dyn UsageCollectorPluginV1>,
            "test.bounds.live_past.withdrawal.v1",
        );

        let result = service
            .create_usage_records(&authenticated_ctx(), vec![withdrawal])
            .await
            .expect(
                "a rejected covered period is a precondition of that submission's identity \
                 derivation, per-record, never a request-wide refusal",
            )
            .into_iter()
            .next()
            .expect("one entry in, one slot out");

        let err = result.expect_err("a withdrawal of a closed month belongs on the backfill route");

        // The rejection must arrive on the PERIOD, not on the target: assert
        // the typed reason and that the target was never read. Neither holds
        // if the bound moved behind the target lookup.
        let (reason, detail) = invalid_argument(&err);
        assert_eq!(*reason, ValidationReason::PastWindow);
        assert!(detail.contains(BACKFILL_ROUTE_PATH), "{detail}");
        assert_eq!(
            plugin.get_usage_record_calls(),
            0,
            "the period bound MUST refuse the entry before the target is read",
        );
    }

    #[tokio::test]
    async fn a_month_long_period_that_just_closed_is_ordinary_live_consumption() {
        // The bound reads the END of the covered period, so a monthly accrual
        // meter emitting the moment its month closes is admitted even though
        // the period it covers is fifteen times the live past tolerance.
        // Hand this call site `window_start` instead of `window_end` and only
        // this test goes red: every other ingestion fixture covers an hour, so
        // the swap is invisible inside a tolerance measured in days.
        let plugin = HappyPathPlugin::new();
        let tenant_id = Uuid::from_u128(0xC4);
        // Truncated to the microsecond: a covered-period bound above
        // microsecond precision is rejected outright, because the identity
        // derivation reads a fixed-width microsecond form, and
        // `clock_gettime` reports nanoseconds on Linux.
        let now = time::OffsetDateTime::now_utc();
        let window_end = now
            .replace_nanosecond(now.microsecond() * 1_000)
            .expect("a microsecond-truncated nanosecond count is in range")
            - time::Duration::minutes(1);
        let submission = CreateUsageRecord {
            entry_type: EntryType::Record,
            window_start: window_end - time::Duration::days(30),
            window_end,
            ..fresh_record(tenant_id, "idem-month-long")
        };
        plugin.set_create_records(vec![Ok(projected(&submission))]);
        let service = service_with_permit(
            Arc::clone(&plugin) as Arc<dyn UsageCollectorPluginV1>,
            "test.bounds.live_past.month_long.v1",
        );

        service
            .create_usage_records(&authenticated_ctx(), vec![submission])
            .await
            .expect("the measurement is not refused request-wide")
            .into_iter()
            .next()
            .expect("one entry in, one slot out")
            .expect("a 30-day period that closed a minute ago is live consumption");
    }

    #[tokio::test]
    async fn a_batch_rejects_only_the_entries_whose_period_is_out_of_bounds() {
        // Per-submission, at its own input index, with the surviving entries
        // still dispatched: a batch-level rejection would make one stale entry
        // discard a whole import.
        let plugin = HappyPathPlugin::new();
        let tenant_id = Uuid::from_u128(0xC3);
        let input = vec![
            fresh_record(tenant_id, "idem-ok-0"),
            stale_record(tenant_id, "idem-stale-1"),
            fresh_record(tenant_id, "idem-ok-2"),
        ];
        // Two per-record outcomes, not three: a refused period never reaches
        // the plugin, and a third would silently absorb a routing mistake.
        plugin.set_create_records(vec![Ok(projected(&input[0])), Ok(projected(&input[2]))]);

        let service = service_with_permit(
            Arc::clone(&plugin) as Arc<dyn UsageCollectorPluginV1>,
            "test.bounds.live_past.batch.v1",
        );

        let results = service
            .create_usage_records(&authenticated_ctx(), input)
            .await
            .expect("a refused period is per-entry, never batch-level");

        assert_eq!(results.len(), 3, "one result per input, in input order");
        assert!(results[0].is_ok(), "{:?}", results[0]);
        assert!(results[2].is_ok(), "{:?}", results[2]);
        let Err(err) = &results[1] else {
            panic!("index 1 must carry the rejection, at its own index");
        };
        assert_eq!(*invalid_argument(err).0, ValidationReason::PastWindow);

        let dispatched = plugin
            .last_create_records_input()
            .expect("the surviving entries MUST still be dispatched");
        assert_eq!(
            dispatched
                .iter()
                .map(|r| r.idempotency_key.as_str())
                .collect::<Vec<_>>(),
            vec!["idem-ok-0", "idem-ok-2"],
            "only the out-of-bounds entry is withheld",
        );
    }
}

// ── The backfill route ─────────────────────────────────────────────────────
//
// `Service::backfill_usage_records` is the live batch body under a different
// `origin`, so what is pinned here is what the origin changes: the marker
// every accepted entry carries, the past bound it lifts, that the lift reaches
// a withdrawal of closed history, and the PDP verb it derives per entry from
// the covered period. The live path's own cases are in
// `covered_period_bounds_tests` above.
mod backfill_route_tests {
    use std::collections::BTreeMap;
    use std::sync::Arc;

    use authz_resolver_sdk::AuthZResolverApi;
    use toolkit_gts::gts_id;
    use usage_collector_sdk::{
        BACKFILL_ROUTE_PATH, CreateUsageRecord, EntryType, IdempotencyKey, MeterTypeId, ReasonCode,
        RecordOrigin, ResourceRef, UsageCollectorError, UsageCollectorPluginV1, ValidationReason,
    };
    use uuid::Uuid;

    use crate::domain::Service;
    use crate::domain::test_support::{
        ActionRecordingPermitResolver, HappyPathPlugin, ServiceFixture, authenticated_ctx,
        default_covered_period_bounds, fake_declaration_source_with_fold, projected_with_origin,
        recent_window_end, recent_window_start,
    };

    const GTS_ID: &str = gts_id!("cf.core.uc.usage_record.v1~cf.mini_chat._.tokens_consumed.v1~");

    fn service_with_permit(plugin: Arc<dyn UsageCollectorPluginV1>, suffix: &str) -> Arc<Service> {
        ServiceFixture::default()
            .with_source(fake_declaration_source_with_fold("SUM"))
            .build(plugin, suffix)
    }

    /// A submission over the shared recent covered period — the one the
    /// live path admits.
    fn fresh_record(tenant_id: Uuid, resource_id: &str, idem: &str) -> CreateUsageRecord {
        CreateUsageRecord {
            entry_type: EntryType::Record,
            gts_type_id: MeterTypeId::new(GTS_ID).expect("valid gts_type_id"),
            tenant_id,
            resource_ref: ResourceRef::new(resource_id, "compute.vm").expect("valid resource ref"),
            subject_ref: None,
            metadata: BTreeMap::new(),
            quantity: crate::domain::test_support::qty("1"),
            idempotency_key: Some(IdempotencyKey::new(idem).expect("valid idempotency key")),
            invalidation: None,
            window_start: recent_window_start(),
            window_end: recent_window_end(),
        }
    }

    /// The same submission over a covered period `days` old. Offset from the
    /// memoised [`recent_window_end`] rather than a fresh `now_utc()`, so two
    /// submissions built by separate calls carry the SAME period — two clock
    /// reads derive two different ids and make a withdrawal an unfaithful
    /// copy of its target.
    fn aged_record(tenant_id: Uuid, resource_id: &str, idem: &str, days: i64) -> CreateUsageRecord {
        let window_end = recent_window_end() - time::Duration::days(days);
        CreateUsageRecord {
            entry_type: EntryType::Record,
            window_start: window_end - time::Duration::hours(1),
            window_end,
            ..fresh_record(tenant_id, resource_id, idem)
        }
    }

    fn invalid_argument(err: &UsageCollectorError) -> (&ValidationReason, &str) {
        match err {
            UsageCollectorError::InvalidArgument { reason, detail, .. } => (reason, detail),
            other => panic!("expected InvalidArgument, got {other:?}"),
        }
    }

    /// The fresh entry is the load-bearing half: a batch of nothing but aged
    /// periods would also be stamped correctly by a route that derived
    /// `origin` from the period's age rather than from the entry point. The
    /// assertion reads what the gateway HANDED the plugin, not what the plugin
    /// handed back — the echo is a fixture and would answer `backfill`
    /// however the entry was stamped.
    #[tokio::test]
    async fn the_backfill_route_stamps_every_accepted_entry_with_backfill() {
        let plugin = HappyPathPlugin::new();
        let tenant_id = Uuid::from_u128(0xD1);
        let input = vec![
            aged_record(tenant_id, "rsc-import", "idem-import-aged", 30),
            fresh_record(tenant_id, "rsc-import", "idem-import-fresh"),
        ];
        plugin.set_create_records(
            input
                .iter()
                .map(|r| Ok(projected_with_origin(r, RecordOrigin::Backfill)))
                .collect(),
        );
        let service = service_with_permit(
            Arc::clone(&plugin) as Arc<dyn UsageCollectorPluginV1>,
            "test.backfill.stamp.records.v1",
        );

        let results = service
            .backfill_usage_records(&authenticated_ctx(), input)
            .await
            .expect("batch dispatch succeeded");

        assert_eq!(results.len(), 2);
        assert!(results.iter().all(Result::is_ok), "{results:?}");

        let dispatched = plugin
            .last_create_records_input()
            .expect("both entries reached the storage plugin");
        assert_eq!(
            dispatched.iter().map(|r| r.origin).collect::<Vec<_>>(),
            vec![RecordOrigin::Backfill, RecordOrigin::Backfill],
            "the route stamps its own origin on every entry it admits, \
             whatever each entry's covered period is",
        );
        assert_eq!(
            results[0].as_ref().expect("accepted").origin,
            RecordOrigin::Backfill,
            "and the marker survives to the caller",
        );
    }

    /// Both halves in one test so they cannot drift apart: the live path
    /// refuses the period and names the route, and the route admits it.
    #[tokio::test]
    async fn the_backfill_route_admits_the_period_the_live_path_rejected() {
        let plugin = HappyPathPlugin::new();
        let tenant_id = Uuid::from_u128(0xD2);
        let stale = aged_record(tenant_id, "rsc-both", "idem-both", 30);
        plugin.set_create_records(vec![Ok(projected_with_origin(
            &stale,
            RecordOrigin::Backfill,
        ))]);
        let service = service_with_permit(
            Arc::clone(&plugin) as Arc<dyn UsageCollectorPluginV1>,
            "test.backfill.admits.records.v1",
        );

        let live_result = service
            .create_usage_records(&authenticated_ctx(), vec![stale.clone()])
            .await
            .expect(
                "a rejected covered period is a precondition of that submission's identity \
                 derivation, per-record, never a request-wide refusal - and the rejection \
                 happens before persist dispatch, so the batch response programmed below for \
                 the backfill attempt is left untouched",
            )
            .into_iter()
            .next()
            .expect("one entry in, one slot out");
        let live_err = live_result.expect_err("30 days is beyond the 48-hour live past tolerance");
        let (reason, detail) = invalid_argument(&live_err);
        assert_eq!(*reason, ValidationReason::PastWindow);
        assert!(
            detail.contains(BACKFILL_ROUTE_PATH),
            "the rejection names the route this test then exercises: {detail}",
        );

        let results = service
            .backfill_usage_records(&authenticated_ctx(), vec![stale])
            .await
            .expect("batch dispatch succeeded");
        assert_eq!(results.len(), 1);
        assert!(
            results[0].is_ok(),
            "the route exists for exactly the period the live path named it \
             for: {results:?}",
        );
    }

    /// The route lifts the past bound and nothing else. A future-dated entry
    /// is a clock-skewed emitter, and pointing one at the import route would
    /// send a defect somewhere it is just as invalid. The rejection is
    /// per-record, at the entry's input index, because a refused covered
    /// period is a precondition of that submission's identity derivation
    /// rather than of the batch.
    #[tokio::test]
    async fn the_backfill_route_refuses_a_period_ending_beyond_the_future_tolerance() {
        assert!(
            time::Duration::hours(1) > default_covered_period_bounds().future_tolerance,
            "the fixture puts the period an hour ahead of now and needs that \
             to be outside the configured tolerance, which is {:?}",
            default_covered_period_bounds().future_tolerance,
        );

        let plugin = HappyPathPlugin::new();
        // `recent_window_end()` is an hour BEHIND now, so two hours on from it
        // is an hour AHEAD. Anchored on the memoised fixture for the reason
        // `aged_record` documents.
        let window_end = recent_window_end() + time::Duration::hours(2);
        let submission = CreateUsageRecord {
            entry_type: EntryType::Record,
            window_start: window_end - time::Duration::hours(1),
            window_end,
            ..fresh_record(Uuid::from_u128(0xD5), "rsc-future", "idem-import-future")
        };
        // Armed to succeed, so the rejection below can only come from the bound.
        plugin.set_create_records(vec![Ok(projected_with_origin(
            &submission,
            RecordOrigin::Backfill,
        ))]);
        let service = service_with_permit(
            Arc::clone(&plugin) as Arc<dyn UsageCollectorPluginV1>,
            "test.backfill.future.records.v1",
        );

        let results = service
            .backfill_usage_records(&authenticated_ctx(), vec![submission])
            .await
            .expect("the batch itself dispatches; the entry is refused in its own slot");

        assert_eq!(results.len(), 1);
        let err = results[0]
            .as_ref()
            .expect_err("the import route lifts the past bound, not the future one");
        let (reason, detail) = invalid_argument(err);
        assert_eq!(*reason, ValidationReason::FutureWindow);
        assert!(
            !detail.contains(BACKFILL_ROUTE_PATH),
            "the entry is already ON the backfill route, so naming it as the \
             place the entry belongs would be a loop: {detail}",
        );
        assert_eq!(
            plugin.last_create_records_input(),
            None,
            "a refused period MUST NOT reach the storage plugin",
        );
    }

    /// The target is built with `origin = Live` and withdrawn on the backfill
    /// route. `origin` is NOT one of the fields the faithful-copy rule
    /// compares, so a `live` target and a `backfill` withdrawal is the
    /// ordinary pair rather than a mismatch. If the comparator ever started
    /// reading `origin`, this is the test that goes red.
    #[tokio::test]
    async fn a_withdrawal_of_closed_history_is_refused_live_and_accepted_on_backfill() {
        let tenant_id = Uuid::from_u128(0xD3);

        // A measurement whose period closed a month ago, persisted as the live
        // path would have left it.
        let measurement = aged_record(tenant_id, "rsc-withdraw", "idem-target", 30);
        let target_row = projected_with_origin(&measurement, RecordOrigin::Live);
        assert_eq!(
            target_row.origin,
            RecordOrigin::Live,
            "the target must be a LIVE entry, or the pair this test is about \
             is not the pair it built",
        );
        // A faithful copy of it, departing only in `entry_type` and the
        // reason, repeating the key that resolves the target.
        let withdrawal = CreateUsageRecord {
            entry_type: EntryType::Invalidation,
            invalidation: Some(ReasonCode::new("emitter_defect").expect("valid reason code")),
            ..aged_record(tenant_id, "rsc-withdraw", "idem-target", 30)
        };

        let plugin = HappyPathPlugin::new();
        plugin.set_get_record(target_row.clone());
        plugin.set_create_records(vec![Ok(projected_with_origin(
            &withdrawal,
            RecordOrigin::Backfill,
        ))]);
        let service = service_with_permit(
            Arc::clone(&plugin) as Arc<dyn UsageCollectorPluginV1>,
            "test.backfill.withdrawal.records.v1",
        );

        let live_result = service
            .create_usage_records(&authenticated_ctx(), vec![withdrawal.clone()])
            .await
            .expect(
                "a rejected covered period is a precondition of that submission's identity \
                 derivation, per-record, never a request-wide refusal - and the rejection \
                 happens before the target lookup, so neither programmed batch response is \
                 touched by this attempt",
            )
            .into_iter()
            .next()
            .expect("one entry in, one slot out");
        let live_err =
            live_result.expect_err("a withdrawal of a closed month belongs on the backfill route");
        let (reason, detail) = invalid_argument(&live_err);
        assert_eq!(*reason, ValidationReason::PastWindow);
        assert!(detail.contains(BACKFILL_ROUTE_PATH), "{detail}");

        let results = service
            .backfill_usage_records(&authenticated_ctx(), vec![withdrawal])
            .await
            .expect("batch dispatch succeeded");
        assert_eq!(results.len(), 1);
        let accepted = results[0]
            .as_ref()
            .expect("a faithful withdrawal of a live target is accepted here");
        assert_eq!(accepted.origin, RecordOrigin::Backfill);
        assert_eq!(
            plugin.get_usage_record_calls(),
            1,
            "the target was read exactly once, on the backfill attempt; the live \
             attempt was refused by the period bound before the lookup",
        );
        let dispatched = plugin
            .last_create_records_input()
            .expect("the withdrawal reached the storage plugin");
        assert_eq!(dispatched[0].origin, RecordOrigin::Backfill);
    }

    /// [`crate::domain::authz::AttributionTupleKey`] carries `action` in its
    /// hash/eq so a batch bound to different actions cannot collapse onto a
    /// single PDP decision. Both entries here share ONE attribution tuple, so
    /// only the action keeps them apart: drop `action` from the key and the
    /// entry reaching past the backfill window rides in on the other's
    /// `create` permit. The assertion is on the recorded action STRINGS —
    /// counting calls would pass for the wrong reason, since a dedup broken on
    /// some other field also makes two calls.
    #[tokio::test]
    async fn one_backfill_batch_mixing_window_sides_authorizes_two_distinct_actions() {
        let bounds = default_covered_period_bounds();
        let inside_days = 30_i64;
        let beyond_days = 120_i64;
        assert!(
            time::Duration::days(inside_days) < bounds.backfill_window
                && time::Duration::days(beyond_days) > bounds.backfill_window,
            "the fixture's two periods MUST straddle the configured backfill \
             window ({:?}) or this test silently stops mixing actions",
            bounds.backfill_window,
        );

        let tenant_id = Uuid::from_u128(0xD4);
        let resolver = ActionRecordingPermitResolver::new();
        let plugin = HappyPathPlugin::new();
        // One attribution tuple, two idempotency keys: the submissions are
        // distinct entries that a PDP dedup keyed on anything but `action`
        // would fold together.
        let input = vec![
            aged_record(tenant_id, "rsc-mixed", "idem-inside", inside_days),
            aged_record(tenant_id, "rsc-mixed", "idem-beyond", beyond_days),
        ];
        plugin.set_create_records(
            input
                .iter()
                .map(|r| Ok(projected_with_origin(r, RecordOrigin::Backfill)))
                .collect(),
        );
        let service = ServiceFixture::default()
            .with_source(fake_declaration_source_with_fold("SUM"))
            .with_resolver(Arc::clone(&resolver) as Arc<dyn AuthZResolverApi>)
            .build(
                Arc::clone(&plugin) as Arc<dyn UsageCollectorPluginV1>,
                "test.backfill.mixed.records.v1",
            );

        let results = service
            .backfill_usage_records(&authenticated_ctx(), input)
            .await
            .expect("batch dispatch succeeded");
        assert!(
            results.iter().all(Result::is_ok),
            "crossing the backfill window selects a different verb; it does \
             NOT reject the entry: {results:?}",
        );

        assert_eq!(
            resolver.actions_sorted(),
            vec!["backfill".to_owned(), "create".to_owned()],
            "one batch, one attribution tuple, two actions: the PDP must see \
             both",
        );
    }
}

// A meter declares exactly one fold and a caller cannot choose one, so there
// is no (op, kind) pair left to validate: these tests pin that the declared
// fold is what reaches the plugin, that an unresolvable type fails closed, and
// the unrelated `MAX_AGGREGATION_BUCKETS` cap.
mod aggregate_declared_fold_tests {
    use toolkit_gts::gts_id;
    use toolkit_odata::ODataQuery;
    use toolkit_security::SecurityContext;
    use usage_collector_sdk::{
        AggregationBucket, AggregationFold, AggregationResult, MAX_AGGREGATION_BUCKETS,
        MeterTypeId, UsageCollectorError, ValidationReason,
    };

    use crate::domain::test_support::{
        RECORDING_PLUGIN_SUFFIX, RecordingPlugin, ServiceFixture, authenticated_ctx,
        fake_declaration_source_not_found, fake_declaration_source_with_fold,
        fake_declaration_source_with_fold_by_id, recording_plugin_resolver, test_time_range,
    };

    const GTS_ID: &str = gts_id!("cf.core.uc.usage_record.v1~cf.mini_chat._.tokens_consumed.v1~");
    // Shares the `cf.mini_chat._.tokens_` name prefix with `GTS_ID` above,
    // but names a distinct meter.
    const GTS_ID_B: &str = gts_id!("cf.core.uc.usage_record.v1~cf.mini_chat._.tokens_produced.v1~");

    fn meter_id() -> MeterTypeId {
        MeterTypeId::new(GTS_ID).expect("valid gts_type_id")
    }

    fn meter_id_b() -> MeterTypeId {
        MeterTypeId::new(GTS_ID_B).expect("valid gts_type_id")
    }

    /// A `Service` + [`RecordingPlugin`] spy over `source`, wired against the
    /// fixed-tenant PDP fake the aggregate surface requires.
    fn service_with_recording_plugin(
        source: std::sync::Arc<dyn crate::domain::ports::declarations::DeclarationSource>,
    ) -> (
        std::sync::Arc<crate::domain::Service>,
        std::sync::Arc<RecordingPlugin>,
    ) {
        let plugin = RecordingPlugin::new();
        let service = ServiceFixture::default()
            .with_source(source)
            .with_resolver(recording_plugin_resolver())
            .build(
                std::sync::Arc::clone(&plugin)
                    as std::sync::Arc<dyn usage_collector_sdk::UsageCollectorPluginV1>,
                RECORDING_PLUGIN_SUFFIX,
            );
        (service, plugin)
    }

    fn ctx() -> SecurityContext {
        authenticated_ctx()
    }

    #[tokio::test]
    async fn aggregate_serves_the_fold_the_declaration_names() {
        // The request carries no aggregation parameter. Whatever fold reaches
        // the plugin must have come from the resolved declaration.
        let source = fake_declaration_source_with_fold("MAX");
        let (svc, spy) = service_with_recording_plugin(source);
        spy.set_query_aggregated_usage_records_response(AggregationResult { buckets: vec![] });

        svc.query_aggregated_usage_records(
            &ctx(),
            meter_id(),
            test_time_range(),
            &ODataQuery::default(),
            &[],
            &[],
        )
        .await
        .expect("aggregates");

        assert_eq!(spy.last_fold(), Some(AggregationFold::Max));
    }

    #[tokio::test]
    async fn aggregate_serves_every_declared_fold_not_a_hard_coded_one() {
        // Covers more than one fold value so a service hard-coded to always
        // push `AggregationFold::Sum` cannot pass: every fold a declaration
        // can name must reach the plugin unchanged, not just one of them.
        for fold in [
            AggregationFold::Sum,
            AggregationFold::Count,
            AggregationFold::Max,
            AggregationFold::Min,
            AggregationFold::Latest,
        ] {
            let source = fake_declaration_source_with_fold(fold.as_str());
            let (svc, spy) = service_with_recording_plugin(source);
            spy.set_query_aggregated_usage_records_response(AggregationResult { buckets: vec![] });

            svc.query_aggregated_usage_records(
                &ctx(),
                meter_id(),
                test_time_range(),
                &ODataQuery::default(),
                &[],
                &[],
            )
            .await
            .unwrap_or_else(|e| panic!("fold {fold:?} MUST dispatch, got {e:?}"));

            assert_eq!(
                spy.last_fold(),
                Some(fold),
                "the fold reaching the plugin must be the one the declaration names",
            );
        }
    }

    /// Two meters sharing a name prefix but declaring different folds each
    /// resolve to their own. `fake_declaration_source_with_fold_by_id` is the
    /// one double here able to answer two ids differently; every other fixture
    /// answers the same fold whatever id it is asked for.
    #[tokio::test]
    async fn two_meters_sharing_a_name_prefix_with_different_folds_each_resolve_to_their_own() {
        let source =
            fake_declaration_source_with_fold_by_id(&[(meter_id(), "SUM"), (meter_id_b(), "MAX")]);
        let (svc, spy) = service_with_recording_plugin(source);
        spy.set_query_aggregated_usage_records_response(AggregationResult { buckets: vec![] });

        svc.query_aggregated_usage_records(
            &ctx(),
            meter_id(),
            test_time_range(),
            &ODataQuery::default(),
            &[],
            &[],
        )
        .await
        .expect("the SUM meter aggregates");
        assert_eq!(
            spy.last_fold(),
            Some(AggregationFold::Sum),
            "meter_id() must resolve to its OWN declared fold, SUM",
        );

        svc.query_aggregated_usage_records(
            &ctx(),
            meter_id_b(),
            test_time_range(),
            &ODataQuery::default(),
            &[],
            &[],
        )
        .await
        .expect("the MAX meter aggregates");
        assert_eq!(
            spy.last_fold(),
            Some(AggregationFold::Max),
            "meter_id_b() must resolve to its OWN declared fold, MAX, despite \
             sharing meter_id()'s name prefix",
        );
    }

    #[tokio::test]
    async fn aggregate_fails_closed_when_the_type_does_not_resolve() {
        let source = fake_declaration_source_not_found();
        let (svc, spy) = service_with_recording_plugin(source);

        let err = svc
            .query_aggregated_usage_records(
                &ctx(),
                meter_id(),
                test_time_range(),
                &ODataQuery::default(),
                &[],
                &[],
            )
            .await
            .expect_err("an unresolvable type must not reach the plugin");

        assert!(
            matches!(err, UsageCollectorError::NotFound { .. }),
            "expected NotFound, got {err:?}",
        );
        assert_eq!(
            spy.calls(),
            0,
            "the plugin must not be dispatched for an unresolvable type",
        );
    }

    // Build an `AggregationResult` with exactly `n` empty-key buckets. Mirrors
    // what the plugin's `LIMIT MAX_AGGREGATION_BUCKETS + 1` produces on an
    // over-cardinality `group_by`.
    fn result_with_buckets(n: usize) -> AggregationResult {
        AggregationResult {
            buckets: (0..n)
                .map(|_| AggregationBucket {
                    key: Vec::new(),
                    value: None,
                })
                .collect(),
        }
    }

    #[tokio::test]
    async fn over_cap_bucket_count_is_rejected() {
        // One bucket over the cap — exactly what the plugin's `LIMIT cap + 1`
        // yields on overflow — must lift to a client-fixable 400, not a page.
        let source = fake_declaration_source_with_fold("COUNT");
        let (svc, spy) = service_with_recording_plugin(source);
        spy.set_query_aggregated_usage_records_response(result_with_buckets(
            MAX_AGGREGATION_BUCKETS + 1,
        ));

        match svc
            .query_aggregated_usage_records(
                &ctx(),
                meter_id(),
                test_time_range(),
                &ODataQuery::default(),
                &[],
                &[],
            )
            .await
        {
            Err(UsageCollectorError::InvalidArgument { reason, .. }) => {
                assert_eq!(reason, ValidationReason::AggregationResultTooLarge);
            }
            other => panic!("expected AGGREGATION_RESULT_TOO_LARGE 400, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn at_cap_bucket_count_is_allowed() {
        // Exactly at the cap is the boundary — `>` must not reject it.
        let source = fake_declaration_source_with_fold("COUNT");
        let (svc, spy) = service_with_recording_plugin(source);
        spy.set_query_aggregated_usage_records_response(result_with_buckets(
            MAX_AGGREGATION_BUCKETS,
        ));

        let result = svc
            .query_aggregated_usage_records(
                &ctx(),
                meter_id(),
                test_time_range(),
                &ODataQuery::default(),
                &[],
                &[],
            )
            .await
            .expect("a result exactly at the cap must be returned");
        assert_eq!(result.buckets.len(), MAX_AGGREGATION_BUCKETS);
    }
}

/// Whether a meter-narrowed PDP grant can still run the aggregate read.
///
/// `authorize_list_usage_records` serves both `Service::list_usage_records`
/// and `Service::query_aggregated_usage_records` under `actions::LIST`, so
/// `POST /records/aggregate` sits in the same blast radius as the raw list.
/// `authz_tests` pins the gate-level denial; these pin the route. The
/// round-trip goes through `MeterNarrowingPermitResolver` so it passes
/// `compile_constraint`, where the permit->deny move happens; a hand-built
/// `AccessScope` would skip that step and prove nothing.
///
/// There is no silent widening to catch: this SPI carries the authorized
/// scope through no channel but the composed `$filter`, and
/// `scope_to_odata_filter` denies the whole projection the moment any one
/// constraint names `gts_type_id`. The SDK compiler's drop-and-continue
/// fail-open on a capability-gated operator is the one carve-out — see
/// [`crate::domain::test_support::UncompilableSiblingPermitResolver`] — and it
/// stays theoretical for every shape driven below.
mod g17_aggregate_read_measurement_tests {
    use std::sync::Arc;

    use authz_resolver_sdk::AuthZResolverApi;
    use toolkit_gts::gts_id;
    use toolkit_odata::ODataQuery;
    use toolkit_security::SecurityContext;
    use usage_collector_sdk::{
        AggregationResult, MeterTypeId, UsageCollectorError, UsageCollectorPluginV1,
    };

    use crate::domain::authz::usage_record;
    use crate::domain::test_support::{
        HappyPathPlugin, MeterNarrowingPermitResolver, ServiceFixture, authenticated_ctx,
        fake_declaration_source_with_fold, test_time_range,
    };

    /// The meter the PDP grant is narrowed to, and the one the request
    /// queries. Whether the two match is irrelevant to the outcome:
    /// `authorize_list_usage_records` runs — and denies, when it denies —
    /// before `TypeResolver::resolve` ever reads `gts_type_id`.
    const OTHER_METER: &str =
        gts_id!("cf.core.uc.usage_record.v1~cf.mini_chat._.tokens_consumed.v1~");

    fn meter() -> MeterTypeId {
        MeterTypeId::new(OTHER_METER).expect("valid gts_type_id")
    }

    fn ctx() -> SecurityContext {
        authenticated_ctx()
    }

    /// Mirrors `authz_tests::assert_denied_as_reserved_meter`, rewritten here
    /// rather than reused: that module is private to `domain::authz`, and its
    /// helper is typed for `DomainError::AuthorizationDenied` while this gate
    /// one layer up returns `UsageCollectorError::PermissionDenied`.
    fn assert_aggregate_denied_as_reserved_meter(err: &UsageCollectorError, label: &str) {
        let UsageCollectorError::PermissionDenied { detail } = err else {
            panic!("{label}: expected a PermissionDenied carrying a PDP reason, got {err:?}");
        };
        assert!(
            detail.contains("reserved"),
            "{label}: the aggregate denial must name the property as reserved \
             on the scope projection; got {detail:?}"
        );
        assert!(
            detail.contains(usage_record::PROP_GTS_TYPE_ID),
            "{label}: the aggregate denial must name the offending property; \
             got {detail:?}"
        );
        assert!(
            !detail.contains("unknown property"),
            "{label}: the aggregate denial must be distinguishable from the \
             unknown-property denial - an operator reading the log has to \
             tell a deliberate reservation from a policy naming an attribute \
             this gear never had; got {detail:?}"
        );
    }

    /// A meter-narrowed grant cannot run an aggregate read at all: both
    /// denied, by name.
    ///
    /// Both constructors are driven because they could in principle answer
    /// differently — `sole` against a projection that denies outright,
    /// `with_tenant_only_sibling` against one that could instead drop the
    /// meter half and serve the sibling alone. If either ever starts serving,
    /// this fails loud.
    #[tokio::test]
    async fn a_meter_narrowed_grant_on_the_aggregate_read() {
        for (label, resolver) in [
            ("sole", MeterNarrowingPermitResolver::sole(OTHER_METER)),
            (
                "with_tenant_only_sibling",
                MeterNarrowingPermitResolver::with_tenant_only_sibling(OTHER_METER),
            ),
        ] {
            let plugin = HappyPathPlugin::new();
            // Programmed so a regression that lets the denial through fails as
            // `expect_err` on an `Ok` page, not as "not programmed".
            plugin
                .set_query_aggregated_usage_records_response(AggregationResult { buckets: vec![] });
            let suffix = format!("test.g17.aggregate_read.{label}.v1");
            let svc = ServiceFixture::default()
                .with_source(fake_declaration_source_with_fold("SUM"))
                .with_resolver(resolver as Arc<dyn AuthZResolverApi>)
                .build(
                    Arc::clone(&plugin) as Arc<dyn UsageCollectorPluginV1>,
                    &suffix,
                );

            let err = svc
                .query_aggregated_usage_records(
                    &ctx(),
                    meter(),
                    test_time_range(),
                    &ODataQuery::new(),
                    &[],
                    &[],
                )
                .await
                .expect_err(
                    "a meter-narrowed grant must not serve the aggregate read \
                     wider than it was granted",
                );
            assert_aggregate_denied_as_reserved_meter(&err, label);

            assert!(
                plugin.last_aggregate_query().is_none(),
                "{label}: the plugin must not be dispatched when the PDP \
                 denies - got a recorded aggregate query"
            );
        }
    }
}

// ── The aggregate read dispatches the COMPOSED filter ───────────────────────
//
// `query_aggregated_usage_records`'s SPI signature carries the authorized
// scope through no channel but the `$filter` it hands the plugin: unlike
// `get_usage_record`, `read_feed_page` and `get_reconciliation_metadata`, it
// takes no `scope: &ast::Expr`. So on this read path "the composed filter
// reached the plugin" *is* tenant isolation, and nothing else observes it.
//
// `query::compose_query_with_scope` is unit-tested in `private_helpers_tests`,
// but that pins the function, not the dispatch: nothing there fails if the
// service composes a query and then hands the plugin the caller's own. These
// two tests are what red on that cross-tenant read amplification.
mod aggregate_composed_filter_dispatch_tests {
    use std::sync::Arc;

    use toolkit_gts::gts_id;
    use toolkit_odata::ODataQuery;
    use toolkit_odata::ast::{CompareOperator, Expr, Value};
    use toolkit_security::SecurityContext;
    use usage_collector_sdk::{AggregationResult, MeterTypeId, UsageCollectorPluginV1};
    use uuid::Uuid;

    use crate::domain::Service;
    use crate::domain::test_support::{
        RECORDING_PLUGIN_SUFFIX, RecordingPlugin, ServiceFixture, authenticated_ctx,
        fake_declaration_source_with_metadata, recording_plugin_resolver, test_time_range,
    };

    const GTS_ID: &str = gts_id!("cf.core.uc.usage_record.v1~cf.mini_chat._.tokens_consumed.v1~");

    /// The tenant [`recording_plugin_resolver`]'s fixed `OWNER_TENANT_ID`
    /// constraint names. `authz::scope_to_odata_filter` projects that PEP
    /// property to the `tenant_id` wire field as a UUID value, so this is the
    /// literal the composed `$filter` has to carry.
    fn scope_tenant() -> Uuid {
        Uuid::from_u128(2)
    }

    fn meter_id() -> MeterTypeId {
        MeterTypeId::new(GTS_ID).expect("valid gts_type_id")
    }

    fn ctx() -> SecurityContext {
        authenticated_ctx()
    }

    /// A `Service` plus its [`RecordingPlugin`] spy, over a declaration
    /// declaring no metadata keys (nothing here filters on one) and the
    /// fixed-tenant PDP fake the aggregate path requires.
    fn svc_and_spy() -> (Arc<Service>, Arc<RecordingPlugin>) {
        let plugin = RecordingPlugin::new();
        plugin.set_query_aggregated_usage_records_response(AggregationResult { buckets: vec![] });
        let service = ServiceFixture::default()
            .with_source(fake_declaration_source_with_metadata(&[]))
            .with_resolver(recording_plugin_resolver())
            .build(
                Arc::clone(&plugin) as Arc<dyn UsageCollectorPluginV1>,
                RECORDING_PLUGIN_SUFFIX,
            );
        (service, plugin)
    }

    /// Every `Eq` comparison of an identifier against a literal anywhere in
    /// `expr`, as `(field, rendered-value)` pairs.
    ///
    /// Walks the whole tree rather than matching the root: composition
    /// AND-merges the scope predicate *under* whatever shape the caller's
    /// filter had, so the scope is a leaf once the caller supplies one of
    /// their own. Matching the root would pass on `Expr::And(..)` alone.
    ///
    /// The value is carried as its `Debug` rendering because
    /// `toolkit_odata::ast::Value` derives no `PartialEq`. That names the
    /// variant as well as the payload, so a UUID-typed `tenant_id` predicate
    /// stays distinguishable from a string-typed one carrying the same digits.
    fn eq_leaves(expr: &Expr) -> Vec<(String, String)> {
        match expr {
            Expr::Compare(lhs, CompareOperator::Eq, rhs) => match (lhs.as_ref(), rhs.as_ref()) {
                (Expr::Identifier(field), Expr::Value(value)) => {
                    vec![(field.clone(), format!("{value:?}"))]
                }
                _ => Vec::new(),
            },
            Expr::And(lhs, rhs) | Expr::Or(lhs, rhs) => {
                let mut found = eq_leaves(lhs);
                found.extend(eq_leaves(rhs));
                found
            }
            Expr::Not(inner) => eq_leaves(inner),
            _ => Vec::new(),
        }
    }

    /// One `(field, rendered-value)` pair, built through [`eq_leaves`]' own
    /// rendering so the expectation cannot drift from it.
    fn leaf(field: &str, value: &Value) -> (String, String) {
        (field.to_owned(), format!("{value:?}"))
    }

    /// The dispatched aggregate `$filter`'s `Eq` leaves, or a panic naming
    /// what was missing.
    fn dispatched_eq_leaves(spy: &RecordingPlugin) -> Vec<(String, String)> {
        let dispatched = spy
            .last_aggregate_query()
            .expect("the plugin MUST have been dispatched");
        let filter = dispatched
            .filter()
            .expect(
                "the aggregate dispatch MUST carry a $filter: the composed \
                 one is the only channel this SPI has for the authorized \
                 scope, so a `None` filter is an unscoped cross-tenant read",
            )
            .clone();
        eq_leaves(&filter)
    }

    #[tokio::test]
    async fn an_unfiltered_aggregate_read_still_dispatches_the_scope_predicate() {
        // The caller supplies no `$filter`, so the dispatched one is entirely
        // server-injected: dispatching the caller's query here would mean
        // `filter: None` — every row of every tenant inside the range.
        let (svc, spy) = svc_and_spy();

        svc.query_aggregated_usage_records(
            &ctx(),
            meter_id(),
            test_time_range(),
            &ODataQuery::new(),
            &[],
            &[],
        )
        .await
        .expect("an unfiltered aggregate read under a tenant-scoped permit is a complete request");

        assert_eq!(
            dispatched_eq_leaves(&spy),
            vec![leaf("tenant_id", &Value::Uuid(scope_tenant()))],
            "the aggregate dispatch MUST carry the PDP scope's own \
             `tenant_id` predicate as its whole $filter when the caller \
             supplied none - anything else is an unscoped aggregate over \
             every tenant's rows",
        );
    }

    #[tokio::test]
    async fn the_aggregate_dispatch_carries_the_scope_predicate_beside_the_callers_own() {
        // The intersection branch: the composed filter must be
        // `caller AND scope`. Both halves are asserted — the caller half rules
        // out a service that *replaced* the caller's filter with the scope.
        let caller = {
            let expr = toolkit_odata::parse_filter_string("resource_id eq 'r1'")
                .expect("test filter parses")
                .into_expr();
            ODataQuery::from(Some(expr))
        };
        assert_eq!(
            eq_leaves(caller.filter().expect("the caller's own filter")),
            vec![leaf("resource_id", &Value::String("r1".to_owned()))],
            "precondition: the caller's filter names no tenant, so an \
             assertion that the dispatched one does can only be satisfied \
             by composition",
        );

        let (svc, spy) = svc_and_spy();
        svc.query_aggregated_usage_records(
            &ctx(),
            meter_id(),
            test_time_range(),
            &caller,
            &[],
            &[],
        )
        .await
        .expect("a filtered aggregate read under a tenant-scoped permit is a complete request");

        let leaves = dispatched_eq_leaves(&spy);
        assert!(
            leaves.contains(&leaf("tenant_id", &Value::Uuid(scope_tenant()))),
            "the aggregate dispatch MUST carry the PDP scope's `tenant_id` \
             predicate: this SPI has no `scope` parameter, so a filter \
             without it is a cross-tenant read that still answers Ok; got \
             {leaves:?}",
        );
        assert!(
            leaves.contains(&leaf("resource_id", &Value::String("r1".to_owned()))),
            "and MUST still carry the caller's own predicate: composition \
             is an intersection, not a substitution; got {leaves:?}",
        );
    }
}

// Ingestion validates against the meter's resolved declaration instead of a
// plugin-owned catalog row: the per-distinct-`gts_id` fan-out in
// `create_usage_records` is a resolver fan-out over the same Type Resolver the
// aggregate path uses.
mod ingestion_declared_type_tests {
    use std::collections::BTreeMap;

    use std::sync::Arc;

    use toolkit_gts::gts_id;
    use toolkit_security::SecurityContext;
    use usage_collector_sdk::{
        CreateUsageRecord, EntryType, IdempotencyKey, MetadataKey, MeterTypeId, ResourceRef,
        UsageCollectorPluginV1,
    };
    use uuid::Uuid;

    use crate::domain::Service;
    use crate::domain::ports::declarations::DeclarationSource;
    use crate::domain::test_support::{
        RECORDING_PLUGIN_SUFFIX, RecordingPlugin, ServiceFixture, authenticated_ctx,
        fake_declaration_source_counting, fake_declaration_source_not_found,
        fake_declaration_source_with_fold, fake_declaration_source_with_metadata, projected,
        recent_window_end, recent_window_start, recording_plugin_resolver,
    };

    const GTS_ID: &str = gts_id!("cf.core.uc.usage_record.v1~cf.mini_chat._.tokens_consumed.v1~");

    fn ctx() -> SecurityContext {
        authenticated_ctx()
    }

    /// A `Service` + [`RecordingPlugin`] spy over `source` at the default
    /// metadata size cap, wired against the fixed-tenant PDP fake.
    fn service_with_recording_plugin(
        source: Arc<dyn DeclarationSource>,
    ) -> (Arc<Service>, Arc<RecordingPlugin>) {
        service_with_recording_plugin_and_cap(
            source,
            crate::domain::validation::DEFAULT_METADATA_SIZE_CAP_BYTES,
        )
    }

    /// Variant of [`service_with_recording_plugin`] taking an explicit
    /// `metadata_size_cap_bytes`.
    fn service_with_recording_plugin_and_cap(
        source: Arc<dyn DeclarationSource>,
        metadata_size_cap_bytes: usize,
    ) -> (Arc<Service>, Arc<RecordingPlugin>) {
        let plugin = RecordingPlugin::new();
        let service = ServiceFixture::default()
            .with_source(source)
            .with_resolver(recording_plugin_resolver())
            .with_cap(metadata_size_cap_bytes)
            .build(
                Arc::clone(&plugin) as Arc<dyn UsageCollectorPluginV1>,
                RECORDING_PLUGIN_SUFFIX,
            );
        (service, plugin)
    }

    /// A minimal valid `CreateUsageRecord`, tenant-matched to
    /// `service_with_recording_plugin`'s fixed-tenant PDP permit
    /// (`Uuid::from_u128(2)`) so it clears authorization regardless of the
    /// declaration under test.
    fn valid_create_record() -> CreateUsageRecord {
        CreateUsageRecord {
            entry_type: EntryType::Record,
            gts_type_id: MeterTypeId::new(GTS_ID).expect("valid gts_type_id"),
            tenant_id: Uuid::from_u128(2),
            resource_ref: ResourceRef::new("rsc-ingestion-declared", "compute.vm")
                .expect("valid resource ref"),
            subject_ref: None,
            metadata: BTreeMap::new(),
            quantity: crate::domain::test_support::qty("1"),
            idempotency_key: Some(
                IdempotencyKey::new(format!("idem-{}", Uuid::new_v4()))
                    .expect("valid idempotency key"),
            ),
            invalidation: None,
            window_start: recent_window_start(),
            window_end: recent_window_end(),
        }
    }

    #[tokio::test]
    async fn ingestion_rejects_an_undeclared_metadata_key() {
        let source = fake_declaration_source_with_metadata(&["region"]);
        let (svc, spy) = service_with_recording_plugin(source);

        let mut record = valid_create_record();
        record.metadata.insert(
            MetadataKey::new("tier").expect("valid key"),
            "gold".to_owned(),
        );

        let result = svc
            .create_usage_records(&ctx(), vec![record])
            .await
            .expect(
                "an undeclared metadata key is a per-record validation rejection, never a \
                 request-wide refusal",
            )
            .into_iter()
            .next()
            .expect("one entry in, one slot out");

        let err = result.expect_err("an undeclared key must be rejected before persistence");
        assert!(
            err.to_string().contains("tier"),
            "the rejection must name the offending key: {err}",
        );
        // `spy.calls()` counts `query_aggregated_usage_records` dispatches
        // only; `last_create_records_input` is the create-path probe.
        assert!(
            spy.last_create_records_input().is_none(),
            "a rejected entry must not reach the plugin",
        );
    }

    #[tokio::test]
    async fn ingestion_accepts_declared_metadata_keys() {
        let source = fake_declaration_source_with_metadata(&["region"]);
        let (svc, spy) = service_with_recording_plugin(source);

        let mut record = valid_create_record();
        record.metadata.insert(
            MetadataKey::new("region").expect("valid key"),
            "eu-west-1".to_owned(),
        );
        spy.set_create_records(vec![Ok(projected(&record))]);

        svc.create_usage_records(&ctx(), vec![record])
            .await
            .expect("the measurement is not refused request-wide")
            .into_iter()
            .next()
            .expect("one entry in, one slot out")
            .expect("a declared metadata key must be accepted");
        assert!(
            spy.last_create_records_input().is_some(),
            "an accepted entry must reach the persist SPI",
        );
    }

    #[tokio::test]
    async fn ingestion_fails_closed_when_the_type_does_not_resolve() {
        // 2.1 fail-closed: an unresolvable reference is rejected, never
        // admitted unvalidated to protect ingestion availability.
        let source = fake_declaration_source_not_found();
        let (svc, spy) = service_with_recording_plugin(source);

        let result = svc
            .create_usage_records(&ctx(), vec![valid_create_record()])
            .await
            .expect(
                "an unresolvable declaration is a per-record rejection (the declaration \
                 pre-pass writes to the slot), never a request-wide refusal",
            )
            .into_iter()
            .next()
            .expect("one entry in, one slot out");
        assert!(result.is_err());
        assert!(
            spy.last_create_records_input().is_none(),
            "an unresolvable type must never reach the plugin",
        );
    }

    #[tokio::test]
    async fn a_batch_resolves_each_distinct_type_once() {
        // The resolver fan-out replaces the catalog fan-out: one resolution
        // per distinct gts_id, not one per record.
        let source = fake_declaration_source_counting();
        let (svc, spy) = service_with_recording_plugin(source.clone());

        let records = vec![
            valid_create_record(),
            valid_create_record(),
            valid_create_record(),
        ];
        spy.set_create_records(records.iter().map(|r| Ok(projected(r))).collect());

        let results = svc
            .create_usage_records(&ctx(), records)
            .await
            .expect("batch accepted");
        assert!(results.iter().all(Result::is_ok));

        assert_eq!(
            source.fetch_calls(),
            1,
            "three records of one type resolve once",
        );
    }

    /// `Service::metadata_size_cap_bytes` actually drives the cap
    /// `validate_submit_record_metadata` enforces — not a hard-coded constant.
    /// `validation_tests.rs` pins the parameter but cannot prove `Service`
    /// forwards its own field.
    #[tokio::test]
    async fn ingestion_honors_a_configured_non_default_metadata_size_cap() {
        let source = fake_declaration_source_with_metadata(&["blob"]);
        let configured_cap = 16;
        let (svc, spy) = service_with_recording_plugin_and_cap(source, configured_cap);

        let mut record = valid_create_record();
        record
            .metadata
            .insert(MetadataKey::new("blob").expect("valid key"), "x".repeat(64));

        let result = svc
            .create_usage_records(&ctx(), vec![record])
            .await
            .expect(
                "a metadata-size-cap rejection is a per-record validation outcome, never a \
                 request-wide refusal",
            )
            .into_iter()
            .next()
            .expect("one entry in, one slot out");

        let err = result.expect_err("a payload over the configured cap must be rejected");
        assert!(
            err.to_string().contains(&configured_cap.to_string()),
            "the rejection must name the configured cap, not a hard-coded default: {err}",
        );
        assert!(
            spy.last_create_records_input().is_none(),
            "a rejected entry must not reach the plugin",
        );
    }

    /// The ingestion path accepts a `COUNT` meter's entry with any quantity
    /// value: ingestion never consults the declared fold.
    /// `ResolvedDeclaration::aggregation_fold`'s only production readers are
    /// `query_aggregated_usage_records` and `get_reconciliation_metadata`.
    #[tokio::test]
    async fn ingestion_accepts_a_count_meters_entry_with_any_quantity_value() {
        let source = fake_declaration_source_with_fold("COUNT");
        let (svc, spy) = service_with_recording_plugin(source);

        // Zero, a plain integer and a fractional value: an ingestion path
        // wrongly validating the quantity against the fold could not accept
        // all three uniformly.
        for quantity in ["0", "1", "123.456"] {
            let mut record = valid_create_record();
            record.quantity = crate::domain::test_support::qty(quantity);
            record.idempotency_key = Some(
                IdempotencyKey::new(format!("idem-{}", Uuid::new_v4()))
                    .expect("valid idempotency key"),
            );
            spy.set_create_records(vec![Ok(projected(&record))]);

            svc.create_usage_records(&ctx(), vec![record])
                .await
                .expect("the measurement is not refused request-wide")
                .into_iter()
                .next()
                .expect("one entry in, one slot out")
                .unwrap_or_else(|e| {
                    panic!("quantity {quantity} must be accepted for a COUNT meter, got {e:?}")
                });
        }
        assert!(
            spy.last_create_records_input().is_some(),
            "an accepted COUNT-meter entry must reach the persist SPI",
        );
    }
}

// The query-surface gate is stated in [`crate::domain::query`]; its
// pure-function coverage lives in `query_tests.rs`. This module proves the
// checks are wired into `Service::list_usage_records` /
// `Service::query_aggregated_usage_records` — right position (before
// dispatch), right arguments (the resolved declaration's `declared_keys`).
mod query_admissibility_tests {
    use std::sync::Arc;

    use toolkit_gts::gts_id;
    use toolkit_odata::{ODataQuery, ast};
    use toolkit_security::SecurityContext;
    use usage_collector_sdk::{
        AggregationDimension, AggregationResult, MetadataFilter, MetadataKey, MeterTypeId,
        RecordPage, UsageCollectorError, UsageCollectorPluginV1, ValidationReason,
    };

    use crate::domain::Service;
    use crate::domain::ports::declarations::DeclarationSource;
    use crate::domain::test_support::{
        RECORDING_PLUGIN_SUFFIX, RecordingPlugin, ServiceFixture, authenticated_ctx,
        fake_declaration_source_not_found, fake_declaration_source_with_metadata,
        recording_plugin_resolver, test_time_range,
    };

    const GTS_ID: &str = gts_id!("cf.core.uc.usage_record.v1~cf.mini_chat._.tokens_consumed.v1~");

    fn meter_id() -> MeterTypeId {
        MeterTypeId::new(GTS_ID).expect("valid gts_type_id")
    }

    fn ctx() -> SecurityContext {
        authenticated_ctx()
    }

    /// An `ODataQuery` whose `$filter` is a bare `reserved_field eq 'x'`
    /// predicate, so whatever the request is rejected for is the
    /// admissibility gate under test. The mandatory read range travels beside
    /// the filter as a typed parameter and contributes no conjunct.
    fn filter_naming(reserved_field: &str) -> ODataQuery {
        ODataQuery::from(Some(ast::Expr::Compare(
            Box::new(ast::Expr::Identifier(reserved_field.to_owned())),
            ast::CompareOperator::Eq,
            Box::new(ast::Expr::Value(ast::Value::String("x".to_owned()))),
        )))
    }

    /// An `ODataQuery` whose `$filter` is `field eq '<value>'`, with the
    /// literal under the caller's control — `filter_naming` fixes it at
    /// `"x"`, which cannot express an off-label closed-set literal.
    fn filter_eq(field: &str, value: &str) -> ODataQuery {
        ODataQuery::from(Some(ast::Expr::Compare(
            Box::new(ast::Expr::Identifier(field.to_owned())),
            ast::CompareOperator::Eq,
            Box::new(ast::Expr::Value(ast::Value::String(value.to_owned()))),
        )))
    }

    /// A `Service` + [`RecordingPlugin`] spy over `source`, wired against the
    /// fixed-tenant PDP fake both read paths require.
    fn service_with_recording_plugin(
        source: Arc<dyn DeclarationSource>,
    ) -> (Arc<Service>, Arc<RecordingPlugin>) {
        let plugin = RecordingPlugin::new();
        let service = ServiceFixture::default()
            .with_source(source)
            .with_resolver(recording_plugin_resolver())
            .build(
                Arc::clone(&plugin) as Arc<dyn UsageCollectorPluginV1>,
                RECORDING_PLUGIN_SUFFIX,
            );
        (service, plugin)
    }

    /// Assert `err` is the canonical reserved-filter-field rejection.
    fn assert_reserved_field_rejection(err: &UsageCollectorError) {
        match err {
            UsageCollectorError::InvalidArgument { field, reason, .. } => {
                assert_eq!(field, "$filter", "attributes to $filter");
                assert_eq!(*reason, ValidationReason::Validation);
            }
            other => panic!("expected InvalidArgument/Validation, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn list_rejects_a_filter_naming_gts_type_id() {
        let (svc, spy) = service_with_recording_plugin(fake_declaration_source_with_metadata(&[]));

        let err = svc
            .list_usage_records(
                &ctx(),
                meter_id(),
                test_time_range(),
                &filter_naming("gts_type_id"),
                &[],
            )
            .await
            .expect_err("a $filter naming gts_type_id must be rejected");
        assert_reserved_field_rejection(&err);
        // `RecordingPlugin::calls()` counts `query_aggregated_usage_records`
        // dispatches only; the list path's "did it dispatch" oracle is
        // `last_list_time_range`.
        assert!(
            spy.last_list_time_range().is_none(),
            "a rejected filter must never reach the plugin",
        );
    }

    #[tokio::test]
    async fn list_rejects_a_filter_naming_a_covered_period_bound() {
        // Both bounds are filterable-schema fields — they have to be for the
        // `(window_end, id)` keyset to resolve to a column — so the
        // reserved-field guard is the only thing between a `$filter`
        // predicate and the plugin. The case-varied spellings are here
        // because `FilterField::from_name` compares with
        // `eq_ignore_ascii_case`, so `WINDOW_END` folds to `WindowEnd` rather
        // than dead-ending as an unknown field; the guard matches that.
        for field in ["window_start", "window_end", "WINDOW_END", "Window_Start"] {
            let (svc, spy) =
                service_with_recording_plugin(fake_declaration_source_with_metadata(&[]));

            let err = svc
                .list_usage_records(
                    &ctx(),
                    meter_id(),
                    test_time_range(),
                    &filter_naming(field),
                    &[],
                )
                .await
                .expect_err("a $filter naming a covered-period bound must be rejected");
            assert_reserved_field_rejection(&err);
            assert!(
                spy.last_list_time_range().is_none(),
                "a rejected filter must never reach the plugin: {field}",
            );
        }
    }

    #[tokio::test]
    async fn list_accepts_a_filter_naming_no_reserved_field() {
        let (svc, spy) = service_with_recording_plugin(fake_declaration_source_with_metadata(&[]));
        spy.set_list_usage_records_response(RecordPage {
            items: vec![],
            next: None,
        });

        svc.list_usage_records(
            &ctx(),
            meter_id(),
            test_time_range(),
            &ODataQuery::default(),
            &[],
        )
        .await
        .expect("no reserved field named: the plugin must be reached");
    }

    #[tokio::test]
    async fn aggregate_rejects_a_filter_naming_a_window_bound() {
        let (svc, spy) = service_with_recording_plugin(fake_declaration_source_with_metadata(&[]));

        let err = svc
            .query_aggregated_usage_records(
                &ctx(),
                meter_id(),
                test_time_range(),
                &filter_naming("window_start"),
                &[],
                &[],
            )
            .await
            .expect_err("a $filter naming window_start must be rejected");
        assert_reserved_field_rejection(&err);
        assert_eq!(
            spy.calls(),
            0,
            "a rejected filter must never reach the plugin",
        );
    }

    #[tokio::test]
    async fn aggregate_rejects_an_undeclared_group_by_dimension() {
        let (svc, spy) =
            service_with_recording_plugin(fake_declaration_source_with_metadata(&["region"]));

        let dims = [AggregationDimension::Metadata(
            MetadataKey::new("tier").expect("valid key"),
        )];
        let err = svc
            .query_aggregated_usage_records(
                &ctx(),
                meter_id(),
                test_time_range(),
                &ODataQuery::default(),
                &[],
                &dims,
            )
            .await
            .expect_err("an undeclared group_by dimension must be rejected");
        assert!(
            err.to_string().contains("tier"),
            "the rejection must name the offending key: {err}",
        );
        assert_eq!(
            spy.calls(),
            0,
            "a rejected group_by must never reach the plugin",
        );
    }

    #[tokio::test]
    async fn aggregate_accepts_a_declared_group_by_dimension() {
        let (svc, spy) =
            service_with_recording_plugin(fake_declaration_source_with_metadata(&["region"]));
        spy.set_query_aggregated_usage_records_response(AggregationResult { buckets: vec![] });

        let dims = [AggregationDimension::Metadata(
            MetadataKey::new("region").expect("valid key"),
        )];
        svc.query_aggregated_usage_records(
            &ctx(),
            meter_id(),
            test_time_range(),
            &ODataQuery::default(),
            &[],
            &dims,
        )
        .await
        .expect("a declared group_by dimension must be accepted");
        assert_eq!(spy.calls(), 1, "an accepted group_by must reach the plugin");
    }

    #[tokio::test]
    async fn aggregate_group_by_admissibility_is_recomputed_per_request() {
        // Two independently-resolved declarations for the same meter id, one
        // declaring `region` and one not: the SAME dimension is rejected
        // against the first and accepted against the second, so the admissible
        // set comes from the declaration resolved for *this* request.
        let dims = [AggregationDimension::Metadata(
            MetadataKey::new("region").expect("valid key"),
        )];

        let (svc_before, _spy_before) =
            service_with_recording_plugin(fake_declaration_source_with_metadata(&[]));
        let err = svc_before
            .query_aggregated_usage_records(
                &ctx(),
                meter_id(),
                test_time_range(),
                &ODataQuery::default(),
                &[],
                &dims,
            )
            .await
            .expect_err("not yet declared: must be rejected");
        assert!(err.to_string().contains("region"));

        let (svc_after, spy_after) =
            service_with_recording_plugin(fake_declaration_source_with_metadata(&["region"]));
        spy_after
            .set_query_aggregated_usage_records_response(AggregationResult { buckets: vec![] });
        svc_after
            .query_aggregated_usage_records(
                &ctx(),
                meter_id(),
                test_time_range(),
                &ODataQuery::default(),
                &[],
                &dims,
            )
            .await
            .expect("declared a moment later: must now be admissible, without a restart");
    }

    // ── metadata_filter admissibility ─────────────────────────────────────
    //
    // `metadata_filter` is the dynamic-key side channel that exists because
    // `toolkit-odata`'s grammar cannot express a filter over a JSON map key.
    // It never flows through `$filter`, so it needs its own declared-keys
    // gate on both read paths.

    /// Build a `MetadataFilter` for `key` with a single candidate value.
    fn metadata_filter(key: &str) -> MetadataFilter {
        MetadataFilter::new(key, ["x".to_owned()]).expect("valid metadata filter")
    }

    #[tokio::test]
    async fn list_rejects_an_undeclared_metadata_filter_key() {
        let (svc, _spy) =
            service_with_recording_plugin(fake_declaration_source_with_metadata(&["region"]));

        let filters = [metadata_filter("tier")];
        let err = svc
            .list_usage_records(
                &ctx(),
                meter_id(),
                test_time_range(),
                &ODataQuery::default(),
                &filters,
            )
            .await
            .expect_err("an undeclared metadata_filter key must be rejected");
        assert!(
            err.to_string().contains("tier"),
            "the rejection must name the offending key: {err}",
        );
    }

    #[tokio::test]
    async fn list_accepts_a_declared_metadata_filter_key() {
        let (svc, spy) =
            service_with_recording_plugin(fake_declaration_source_with_metadata(&["region"]));
        spy.set_list_usage_records_response(RecordPage {
            items: vec![],
            next: None,
        });

        let filters = [metadata_filter("region")];
        svc.list_usage_records(
            &ctx(),
            meter_id(),
            test_time_range(),
            &ODataQuery::default(),
            &filters,
        )
        .await
        .expect("a declared metadata_filter key must be accepted, reaching the plugin");
    }

    #[tokio::test]
    async fn aggregate_rejects_an_undeclared_metadata_filter_key() {
        let (svc, spy) =
            service_with_recording_plugin(fake_declaration_source_with_metadata(&["region"]));

        let filters = [metadata_filter("tier")];
        let err = svc
            .query_aggregated_usage_records(
                &ctx(),
                meter_id(),
                test_time_range(),
                &ODataQuery::default(),
                &filters,
                &[],
            )
            .await
            .expect_err("an undeclared metadata_filter key must be rejected");
        assert!(
            err.to_string().contains("tier"),
            "the rejection must name the offending key: {err}",
        );
        assert_eq!(
            spy.calls(),
            0,
            "a rejected metadata_filter must never reach the plugin",
        );
    }

    #[tokio::test]
    async fn aggregate_accepts_a_declared_metadata_filter_key() {
        let (svc, spy) =
            service_with_recording_plugin(fake_declaration_source_with_metadata(&["region"]));
        spy.set_query_aggregated_usage_records_response(AggregationResult { buckets: vec![] });

        let filters = [metadata_filter("region")];
        svc.query_aggregated_usage_records(
            &ctx(),
            meter_id(),
            test_time_range(),
            &ODataQuery::default(),
            &filters,
            &[],
        )
        .await
        .expect("a declared metadata_filter key must be accepted");
        assert_eq!(
            spy.calls(),
            1,
            "an accepted metadata_filter must reach the plugin",
        );
    }

    #[tokio::test]
    async fn metadata_filter_admissibility_is_recomputed_per_request() {
        // The `group_by` recomputation proof mirrored onto `metadata_filter`,
        // on the list path.
        let filters = [metadata_filter("region")];

        let (svc_before, _spy_before) =
            service_with_recording_plugin(fake_declaration_source_with_metadata(&[]));
        let err = svc_before
            .list_usage_records(
                &ctx(),
                meter_id(),
                test_time_range(),
                &ODataQuery::default(),
                &filters,
            )
            .await
            .expect_err("not yet declared: must be rejected");
        assert!(err.to_string().contains("region"));

        let (svc_after, spy_after) =
            service_with_recording_plugin(fake_declaration_source_with_metadata(&["region"]));
        spy_after.set_list_usage_records_response(RecordPage {
            items: vec![],
            next: None,
        });
        svc_after
            .list_usage_records(
                &ctx(),
                meter_id(),
                test_time_range(),
                &ODataQuery::default(),
                &filters,
            )
            .await
            .expect("declared a moment later: must now be admissible, without a restart");
    }

    /// The `metadata_filter` count cap holds on the aggregate path too. That
    /// path has no cursor to blow, so the reason here is the published
    /// `maxItems: 16` on the wire contract rather than a pagination bound.
    #[tokio::test]
    async fn the_metadata_filter_caps_hold_on_the_aggregate_path() {
        let (svc, spy) = service_with_recording_plugin(fake_declaration_source_with_metadata(&[]));
        let filters: Vec<MetadataFilter> = (0..17)
            .map(|i| MetadataFilter::new(format!("k{i}"), ["v"]).expect("valid"))
            .collect();
        let err = svc
            .query_aggregated_usage_records(
                &ctx(),
                meter_id(),
                test_time_range(),
                &ODataQuery::new(),
                &filters,
                &[],
            )
            .await
            .expect_err("17 predicates exceeds the cap on every surface");
        // None of `k0..k16` is declared either, so `InvalidArgument` alone
        // would also match `unknown_metadata_key`; the reason and detail are
        // pinned so disabling the cap check alone turns this red.
        let UsageCollectorError::InvalidArgument {
            ref field,
            ref reason,
            ref detail,
            ..
        } = err
        else {
            panic!("expected InvalidArgument, got {err:?}");
        };
        assert_eq!(field, "metadata", "the cap rejection blames the bag");
        // `reason` is the structural discriminator (it rides onto
        // `field_violations[0].reason` in `sdk_error_mapping.rs`):
        // `too_many_metadata_filters` carries `Validation`, while
        // `unknown_metadata_key` carries `UnknownMetadataKey`.
        assert_eq!(
            *reason,
            ValidationReason::Validation,
            "the rejection MUST be the count cap, not unknown_metadata_key \
             (ValidationReason::UnknownMetadataKey), which shares the same \
             `metadata` field; got {reason:?}"
        );
        // `reason` cannot carry the offending count; `detail` pins that.
        assert!(
            detail.contains("17") && detail.contains("16"),
            "the rejection detail MUST name the count cap (17 over a cap of 16): {detail}"
        );
        assert_eq!(
            spy.calls(),
            0,
            "the cap is checked before dispatch, so the plugin saw nothing"
        );
    }

    #[tokio::test]
    async fn list_now_fails_closed_when_the_type_does_not_resolve() {
        // Gating `metadata_filter` against the declared keys requires
        // resolving a declaration, so an unresolvable type fails closed as a
        // pre-dispatch 404 — the list-path mirror of
        // `aggregate_fails_closed_when_the_type_does_not_resolve`.
        let (svc, _spy) = service_with_recording_plugin(fake_declaration_source_not_found());

        let err = svc
            .list_usage_records(
                &ctx(),
                meter_id(),
                test_time_range(),
                &ODataQuery::default(),
                &[],
            )
            .await
            .expect_err("an unresolvable type must not reach the plugin");
        assert!(
            matches!(err, UsageCollectorError::NotFound { .. }),
            "expected NotFound, got {err:?}",
        );
    }

    // ── Closed-label `$filter` literals ───────────────────────────────────
    //
    // `entry_type` and `origin` are closed-label columns: an off-label
    // `entry_type` literal raises PostgreSQL 22P02 at the backend (a 500 on
    // caller input), and an off-label `origin` literal silently answers an
    // empty page. `query::reject_off_label_literals` refuses both before
    // dispatch, on both read paths. The property under test here is "before
    // dispatch", which only the plugin-double call count can show; the
    // function's own return value is pinned in `query_tests.rs`.

    #[tokio::test]
    async fn list_rejects_an_off_label_entry_type_literal_before_dispatch() {
        let (svc, spy) = service_with_recording_plugin(fake_declaration_source_with_metadata(&[]));

        let err = svc
            .list_usage_records(
                &ctx(),
                meter_id(),
                test_time_range(),
                &filter_eq("entry_type", "Record"),
                &[],
            )
            .await
            .expect_err("`Record` is not a label");
        let UsageCollectorError::InvalidArgument { ref detail, .. } = err else {
            panic!("expected InvalidArgument, got {err:?}");
        };
        assert!(
            detail.contains("Record"),
            "the refusal must echo the offending value; got {detail}"
        );
        // `last_list_time_range` is the list path's "did it dispatch" oracle;
        // `calls()` counts aggregate dispatches only.
        assert!(
            spy.last_list_time_range().is_none(),
            "an off-label literal must never reach the plugin"
        );
    }

    #[tokio::test]
    async fn aggregate_rejects_an_off_label_origin_literal_before_dispatch() {
        let (svc, spy) = service_with_recording_plugin(fake_declaration_source_with_metadata(&[]));

        let err = svc
            .query_aggregated_usage_records(
                &ctx(),
                meter_id(),
                test_time_range(),
                &filter_eq("origin", "imported"),
                &[],
                &[],
            )
            .await
            .expect_err("`imported` is not a label");
        let UsageCollectorError::InvalidArgument { ref detail, .. } = err else {
            panic!("expected InvalidArgument, got {err:?}");
        };
        assert!(
            detail.contains("imported"),
            "the refusal must echo the offending value; got {detail}"
        );
        assert_eq!(
            spy.calls(),
            0,
            "an off-label literal must never reach the plugin"
        );
    }

    #[tokio::test]
    async fn list_accepts_a_filter_naming_a_label_in_the_closed_set() {
        // The positive direction: a label the set contains must still reach
        // the plugin, so the guard discriminates the value rather than
        // refusing `entry_type` / `origin` outright.
        let (svc, spy) = service_with_recording_plugin(fake_declaration_source_with_metadata(&[]));
        spy.set_list_usage_records_response(RecordPage {
            items: vec![],
            next: None,
        });

        svc.list_usage_records(
            &ctx(),
            meter_id(),
            test_time_range(),
            &filter_eq("entry_type", "record"),
            &[],
        )
        .await
        .expect("`record` is an admissible label");
        assert!(
            spy.last_list_time_range().is_some(),
            "an admissible label must reach the plugin"
        );
    }
}

// The mandatory read range is a typed `TimeRange` parameter on both read
// paths, never a `$filter` conjunct. Two things need pinning, neither visible
// in a status code: that the caller's range is the one the SPI is handed, and
// that a caller supplying no `$filter` is a complete request. A range dropped
// between gateway and plugin is an unbounded scan that still answers `Ok`, so
// every assertion here is on what the plugin received.
mod read_path_time_range_tests {
    use std::sync::Arc;

    use toolkit_gts::gts_id;
    use toolkit_odata::ODataQuery;
    use toolkit_security::SecurityContext;
    use usage_collector_sdk::{
        AggregationResult, MeterTypeId, RecordPage, TimeRange, UsageCollectorPluginV1,
    };

    use crate::domain::Service;
    use crate::domain::test_support::{
        RECORDING_PLUGIN_SUFFIX, RecordingPlugin, ServiceFixture, authenticated_ctx,
        fake_declaration_source_with_metadata, recording_plugin_resolver, test_time_range,
    };

    const GTS_ID: &str = gts_id!("cf.core.uc.usage_record.v1~cf.mini_chat._.tokens_consumed.v1~");

    fn meter_id() -> MeterTypeId {
        MeterTypeId::new(GTS_ID).expect("valid gts_type_id")
    }

    fn ctx() -> SecurityContext {
        authenticated_ctx()
    }

    /// A `Service` plus its [`RecordingPlugin`] spy, over a declaration with
    /// no metadata keys and the fixed-tenant PDP fake both read paths require.
    fn svc_and_spy() -> (Arc<Service>, Arc<RecordingPlugin>) {
        let plugin = RecordingPlugin::new();
        let service = ServiceFixture::default()
            .with_source(fake_declaration_source_with_metadata(&[]))
            .with_resolver(recording_plugin_resolver())
            .build(
                Arc::clone(&plugin) as Arc<dyn UsageCollectorPluginV1>,
                RECORDING_PLUGIN_SUFFIX,
            );
        (service, plugin)
    }

    #[tokio::test]
    async fn list_forwards_the_typed_time_range_to_the_plugin() {
        let (svc, spy) = svc_and_spy();
        spy.set_list_usage_records_response(RecordPage {
            items: vec![],
            next: None,
        });

        let range = test_time_range();
        svc.list_usage_records(&ctx(), meter_id(), range, &ODataQuery::default(), &[])
            .await
            .expect("list succeeds");

        assert_eq!(
            spy.last_list_time_range(),
            Some(range),
            "the caller's range MUST reach the SPI verbatim: a substituted or \
             dropped range is an unbounded scan that still answers Ok",
        );
    }

    #[tokio::test]
    async fn aggregate_forwards_the_typed_time_range_to_the_plugin() {
        let (svc, spy) = svc_and_spy();
        spy.set_query_aggregated_usage_records_response(AggregationResult { buckets: vec![] });

        let range = test_time_range();
        svc.query_aggregated_usage_records(
            &ctx(),
            meter_id(),
            range,
            &ODataQuery::default(),
            &[],
            &[],
        )
        .await
        .expect("aggregate succeeds");

        assert_eq!(
            spy.last_aggregate_time_range(),
            Some(range),
            "the aggregate path has no page-size ceiling, so the range is its \
             only scan bound and MUST reach the SPI verbatim",
        );
    }

    #[tokio::test]
    async fn the_range_the_plugin_receives_tracks_the_one_the_caller_passed() {
        // The forwarding tests above would pass against a service that
        // hard-coded a single range; two dispatches with two different ranges
        // through the same service rule that out.
        let (svc, spy) = svc_and_spy();

        let first = test_time_range();
        let second = TimeRange::new(
            first.upper_exclusive(),
            first.upper_exclusive() + time::Duration::hours(1),
        )
        .expect("the adjacent hour is a valid range");
        assert_ne!(first, second, "precondition: the two ranges differ");

        spy.set_list_usage_records_response(RecordPage {
            items: vec![],
            next: None,
        });
        svc.list_usage_records(&ctx(), meter_id(), first, &ODataQuery::default(), &[])
            .await
            .expect("first list succeeds");
        assert_eq!(spy.last_list_time_range(), Some(first));

        spy.set_list_usage_records_response(RecordPage {
            items: vec![],
            next: None,
        });
        svc.list_usage_records(&ctx(), meter_id(), second, &ODataQuery::default(), &[])
            .await
            .expect("second list succeeds");
        assert_eq!(
            spy.last_list_time_range(),
            Some(second),
            "the second dispatch MUST carry the second range, not a range \
             fixed at construction or cached from the first call",
        );
    }

    /// `PageInfo.limit` is clamped, not merely defaulted, because an
    /// in-process caller's `query.limit` never passes through REST's
    /// `prepare_list_query` `$top` cap the way a REST request's does.
    ///
    /// `RecordPage` carries no limit for the plugin to report, so the gear
    /// computes `PageInfo.limit` from `composed.limit`. Unclamped,
    /// `limit: Some(5000)` would report `5000` on a page a conforming plugin
    /// still caps at `MAX_PAGE_SIZE`, and `limit: Some(0)` would report `0` on
    /// a page that served one row — and `local_client.rs` forwards the page
    /// verbatim, so either number reaches a calling gear as a false claim.
    #[tokio::test]
    async fn an_in_process_limit_outside_the_page_size_bounds_is_clamped_on_the_way_out() {
        use crate::domain::service::{DEFAULT_PAGE_SIZE, MAX_PAGE_SIZE};

        let (svc, spy) = svc_and_spy();
        spy.set_list_usage_records_response(RecordPage {
            items: vec![],
            next: None,
        });
        let mut over = ODataQuery::new();
        over.limit = Some(5000);
        let page = svc
            .list_usage_records(&ctx(), meter_id(), test_time_range(), &over, &[])
            .await
            .expect("list succeeds");
        assert_eq!(
            page.page_info.limit, MAX_PAGE_SIZE,
            "a limit above the page-size ceiling must be clamped down to it, \
             not reported as the caller's own oversized request",
        );

        let (svc, spy) = svc_and_spy();
        spy.set_list_usage_records_response(RecordPage {
            items: vec![],
            next: None,
        });
        let mut zero = ODataQuery::new();
        zero.limit = Some(0);
        let page = svc
            .list_usage_records(&ctx(), meter_id(), test_time_range(), &zero, &[])
            .await
            .expect("list succeeds");
        assert_eq!(
            page.page_info.limit, 1,
            "a limit of zero must be clamped up to the floor of 1, not \
             reported as zero",
        );

        let (svc, spy) = svc_and_spy();
        spy.set_list_usage_records_response(RecordPage {
            items: vec![],
            next: None,
        });
        let page = svc
            .list_usage_records(
                &ctx(),
                meter_id(),
                test_time_range(),
                &ODataQuery::new(),
                &[],
            )
            .await
            .expect("list succeeds");
        assert_eq!(
            page.page_info.limit, DEFAULT_PAGE_SIZE,
            "an absent limit reports the gear's own default, mirroring the \
             production plugin's unset-limit default",
        );
    }

    /// A `$filter` that constrains something other than time — the shape a
    /// caller wanting both a predicate and a range sends. The forwarding tests
    /// above pass `ODataQuery::default()`, so the pair covers both "no
    /// `$filter` at all" and "a `$filter` naming no time predicate".
    fn filter_without_a_time_predicate() -> ODataQuery {
        ODataQuery::from(Some(
            toolkit_odata::parse_filter_string("resource_id eq 'r1'")
                .expect("filter parses")
                .into_expr(),
        ))
    }

    #[tokio::test]
    async fn list_needs_no_time_window_inside_the_filter() {
        let (svc, spy) = svc_and_spy();
        spy.set_list_usage_records_response(RecordPage {
            items: vec![],
            next: None,
        });

        let range = test_time_range();
        svc.list_usage_records(
            &ctx(),
            meter_id(),
            range,
            &filter_without_a_time_predicate(),
            &[],
        )
        .await
        .expect("a $filter naming no time predicate is a complete list request");

        assert_eq!(
            spy.last_list_time_range(),
            Some(range),
            "a non-empty $filter must not disturb the typed range on its way \
             to the SPI",
        );
    }

    #[tokio::test]
    async fn aggregate_needs_no_time_window_inside_the_filter() {
        let (svc, spy) = svc_and_spy();
        spy.set_query_aggregated_usage_records_response(AggregationResult { buckets: vec![] });

        let range = test_time_range();
        svc.query_aggregated_usage_records(
            &ctx(),
            meter_id(),
            range,
            &filter_without_a_time_predicate(),
            &[],
            &[],
        )
        .await
        .expect("a $filter naming no time predicate is a complete aggregate request");

        assert_eq!(
            spy.last_aggregate_time_range(),
            Some(range),
            "a non-empty $filter must not disturb the typed range on its way \
             to the SPI",
        );
    }
}

// The keyset floor and gateway-owned cursor are stated in
// [`crate::domain::query`]. These tests cover the surface no REST test can
// reach: a caller holding a `Service` and handing it an `ODataQuery` of their
// own construction. An unfloored order is not visible in a status code — it is
// a keyset the plugin cannot continue, or one that silently drops rows at a
// page boundary — so every assertion is on the order the plugin received.
mod read_path_keyset_floor_tests {
    use std::sync::Arc;

    use toolkit_gts::gts_id;
    use toolkit_odata::{CursorV1, ODataOrderBy, ODataQuery, OrderKey, SortDir};
    use toolkit_security::SecurityContext;
    use usage_collector_sdk::{
        MeterTypeId, RecordPage, UsageCollectorError, UsageCollectorPluginV1,
    };

    use crate::domain::Service;
    use crate::domain::query::read_fingerprint;
    use crate::domain::test_support::{
        RECORDING_PLUGIN_SUFFIX, RecordingPlugin, ServiceFixture, authenticated_ctx,
        fake_declaration_source_with_metadata, recording_plugin_resolver, test_time_range,
    };

    const GTS_ID: &str = gts_id!("cf.core.uc.usage_record.v1~cf.mini_chat._.tokens_consumed.v1~");

    fn meter_id() -> MeterTypeId {
        MeterTypeId::new(GTS_ID).expect("valid gts_type_id")
    }

    fn ctx() -> SecurityContext {
        authenticated_ctx()
    }

    /// A `Service` over the [`RecordingPlugin`] spy, a declaration with no
    /// metadata keys, and the fixed-tenant PDP fake both read paths require.
    fn svc_and_spy() -> (Arc<Service>, Arc<RecordingPlugin>) {
        let plugin = RecordingPlugin::new();
        let service = ServiceFixture::default()
            .with_source(fake_declaration_source_with_metadata(&[]))
            .with_resolver(recording_plugin_resolver())
            .build(
                Arc::clone(&plugin) as Arc<dyn UsageCollectorPluginV1>,
                RECORDING_PLUGIN_SUFFIX,
            );
        (service, plugin)
    }

    /// An [`ODataQuery`] whose order is exactly `keys`.
    fn query_ordered_by(keys: &[(&str, SortDir)]) -> ODataQuery {
        let mut query = ODataQuery::new();
        query.order = ODataOrderBy(
            keys.iter()
                .map(|(field, dir)| OrderKey {
                    field: (*field).to_owned(),
                    dir: *dir,
                })
                .collect(),
        );
        query
    }

    /// The order the spy last received, as owned `(field, direction)` pairs.
    fn received_order(spy: &RecordingPlugin) -> Vec<(String, SortDir)> {
        spy.last_list_order()
            .expect("the plugin MUST have been dispatched")
    }

    fn expected(keys: &[(&str, SortDir)]) -> Vec<(String, SortDir)> {
        keys.iter()
            .map(|(field, dir)| ((*field).to_owned(), *dir))
            .collect()
    }

    #[tokio::test]
    async fn an_in_process_caller_with_no_order_still_reaches_the_plugin_with_the_canonical_keyset()
    {
        // The reason the floor lives in the domain rather than in
        // `prepare_list_query`: an in-process caller hands the service an
        // `ODataQuery::default()`, whose order is empty, and the SPI documents
        // `query.order` as populated.
        let (svc, spy) = svc_and_spy();
        spy.set_list_usage_records_response(RecordPage {
            items: vec![],
            next: None,
        });

        svc.list_usage_records(
            &ctx(),
            meter_id(),
            test_time_range(),
            &ODataQuery::default(),
            &[],
        )
        .await
        .expect("an empty order is a complete in-process list request");

        assert_eq!(
            received_order(&spy),
            expected(&[("window_end", SortDir::Asc), ("id", SortDir::Asc)]),
            "the plugin MUST receive the canonical keyset, not the caller's \
             empty order",
        );
    }

    #[tokio::test]
    async fn an_in_process_caller_order_keeps_its_keys_and_gains_the_suffix() {
        // The floor is a floor, not a substitution: a sound caller order
        // survives and only gains the canonical fields it does not name, in
        // its own direction, so the row-value comparison stays uniform.
        let (svc, spy) = svc_and_spy();
        spy.set_list_usage_records_response(RecordPage {
            items: vec![],
            next: None,
        });

        svc.list_usage_records(
            &ctx(),
            meter_id(),
            test_time_range(),
            &query_ordered_by(&[("resource_id", SortDir::Desc)]),
            &[],
        )
        .await
        .expect("a uniform order on a mandatory attribute is admissible");

        assert_eq!(
            received_order(&spy),
            expected(&[
                ("resource_id", SortDir::Desc),
                ("window_end", SortDir::Desc),
                ("id", SortDir::Desc),
            ]),
        );
    }

    #[tokio::test]
    async fn an_in_process_order_the_floor_cannot_repair_never_reaches_the_plugin() {
        // Mixed directions and a nullable leading key are unsound keysets
        // appending a suffix cannot fix, so the service refuses before
        // dispatch. `created_at` is in the same bucket: it is not a record
        // attribute, and the classification is a fail-closed allowlist.
        for order in [
            vec![("resource_id", SortDir::Asc), ("tenant_id", SortDir::Desc)],
            vec![("subject_id", SortDir::Asc)],
            vec![("created_at", SortDir::Asc)],
        ] {
            let (svc, spy) = svc_and_spy();
            spy.set_list_usage_records_response(RecordPage {
                items: vec![],
                next: None,
            });

            let err = svc
                .list_usage_records(
                    &ctx(),
                    meter_id(),
                    test_time_range(),
                    &query_ordered_by(&order),
                    &[],
                )
                .await
                .expect_err("an unsound keyset must be refused before dispatch");
            assert!(
                matches!(err, UsageCollectorError::InvalidArgument { .. }),
                "expected InvalidArgument for {order:?}, got {err:?}",
            );
            assert!(
                spy.last_list_order().is_none(),
                "a refused order MUST NOT reach the plugin: {order:?}",
            );
        }
    }

    /// A continuation request as the REST handler hands it to the service.
    ///
    /// Minted the way a conforming plugin does (`to_signed_tokens`) and
    /// then decoded the way `prepare_list_query` step 3 does
    /// (`ODataOrderBy::from_signed_tokens`), so the order under test is
    /// derived from the token rather than set alongside it.
    ///
    /// `f` is minted the same way: set to [`read_fingerprint`] over the
    /// caller's query and range, the value the gear itself would mint into
    /// `f` were this token built by a real first-page dispatch rather than
    /// by hand. Without it every fixture here would be refused as a cursor
    /// bound to another query, and these tests are about the order.
    fn cursor_request_ordered_by(keys: &[(&str, SortDir)]) -> ODataQuery {
        let signed = query_ordered_by(keys).order.to_signed_tokens();
        let mut query = ODataQuery::new();
        query.order =
            ODataOrderBy::from_signed_tokens(&signed).expect("a non-empty order round-trips");
        query.cursor = Some(CursorV1 {
            // Distinguishable per index, not one literal repeated: a
            // positional-alignment assertion cannot catch a transposition if
            // every value reads the same.
            k: (0..query.order.0.len())
                .map(|i| format!("boundary-{i}"))
                .collect(),
            o: query.order.0[0].dir,
            s: signed,
            f: Some(read_fingerprint(
                &meter_id(),
                test_time_range(),
                &query,
                &[],
            )),
            d: "fwd".to_owned(),
        });
        query
    }

    #[tokio::test]
    async fn a_conforming_continuation_reaches_the_plugin_with_the_order_it_was_minted_under() {
        // The service picks which mode of the floor to run, so this is where
        // "a continuation is checked, not extended" can fail: nothing stops
        // the service calling `establish_keyset_order` instead of
        // `require_continuation_keyset`. The plugin must receive the token's
        // order with the same keys, directions and width, which is what keeps
        // the continuation predicate aligned with its boundary values.
        let keys = [
            ("resource_id", SortDir::Desc),
            ("window_end", SortDir::Desc),
            ("id", SortDir::Desc),
        ];
        let (svc, spy) = svc_and_spy();
        spy.set_list_usage_records_response(RecordPage {
            items: vec![],
            next: None,
        });

        svc.list_usage_records(
            &ctx(),
            meter_id(),
            test_time_range(),
            &cursor_request_ordered_by(&keys),
            &[],
        )
        .await
        .expect("a conforming continuation is a complete request");

        assert_eq!(
            received_order(&spy),
            expected(&keys),
            "a continuation MUST reach the plugin exactly as the token \
             bound it: a widened order no longer lines up with the \
             boundary values the token carries",
        );
        // The strip only does anything on a continuation; a first-page
        // dispatch carries no cursor to strip. The plugin must never see a
        // wire token — see [`crate::domain::query`].
        assert!(
            spy.last_list_query()
                .expect("the plugin MUST have been dispatched")
                .cursor
                .is_none(),
            "the gateway must strip CursorV1 before dispatch on a \
             continuation, not only on a first page where there was never \
             one to strip",
        );
        let keyset = spy
            .last_list_keyset()
            .expect("a continuation carries boundary values the gateway must extract and hand on");
        assert_eq!(
            keyset.values(),
            (0..keys.len())
                .map(|i| format!("boundary-{i}"))
                .collect::<Vec<_>>(),
            "the plugin must receive exactly the token's own boundary \
             values, one per key and in the token's own order - \
             distinguishable per index, so a transposition would fail \
             this and not just a length check",
        );
        assert_eq!(
            keyset.direction(),
            SortDir::Desc,
            "the keyset's direction is the bound order's single direction",
        );
    }

    #[tokio::test]
    async fn a_continuation_the_floor_would_have_to_widen_never_reaches_the_plugin() {
        // On a cursor request the order must already be a sound keyset, not be
        // extended into one: appending `window_end` to a one-key token would
        // hand the plugin a two-key order against one boundary value — a
        // silently wrong page. Only a forged token or a non-conforming plugin
        // gets here.
        let (svc, spy) = svc_and_spy();
        spy.set_list_usage_records_response(RecordPage {
            items: vec![],
            next: None,
        });

        let err = svc
            .list_usage_records(
                &ctx(),
                meter_id(),
                test_time_range(),
                &cursor_request_ordered_by(&[("resource_id", SortDir::Asc)]),
                &[],
            )
            .await
            .expect_err("a token bound to a non-unique keyset must be refused");
        match err {
            // The 400 blames the token, not an `$orderby` the caller cannot
            // send alongside a cursor — but the gear does not spell that
            // attribution or its wire code (Spec §3.13). Both are
            // `toolkit_odata`'s, derived from the error carried here, so
            // this pins the carried error and `infra::sdk_error_mapping`
            // pins the projection.
            UsageCollectorError::CursorRejected { source, .. } => assert!(
                matches!(source, toolkit_odata::Error::InvalidCursor),
                "expected upstream's InvalidCursor, got {source:?}",
            ),
            other => panic!("expected a cursor rejection, got {other:?}"),
        }
        assert!(
            spy.last_list_order().is_none(),
            "a refused continuation MUST NOT reach the plugin",
        );
    }

    /// A `d: "bwd"` token with an otherwise-sound order, correct signed
    /// tokens and a matching fingerprint is refused on direction alone.
    ///
    /// `admit_continuation`'s other checks validate the *order* and never
    /// read `cursor.d`, and `CursorV1::decode` validates only that `d` is one
    /// of `{"fwd", "bwd"}`, not that it is the one this read path supports.
    /// The raw path is forward-only by design, so without
    /// `require_forward_cursor` such a token is served a forward page with
    /// `200`.
    #[tokio::test]
    async fn a_backward_cursor_never_reaches_the_plugin() {
        let (svc, spy) = svc_and_spy();
        spy.set_list_usage_records_response(RecordPage {
            items: vec![],
            next: None,
        });

        let keys = [("window_end", SortDir::Asc), ("id", SortDir::Asc)];
        let signed = query_ordered_by(&keys).order.to_signed_tokens();
        let mut query = ODataQuery::new();
        query.order =
            ODataOrderBy::from_signed_tokens(&signed).expect("a non-empty order round-trips");
        query.cursor = Some(CursorV1 {
            k: vec![
                "2026-01-01T00:00:00Z".to_owned(),
                "00000000-0000-0000-0000-000000000001".to_owned(),
            ],
            o: SortDir::Asc,
            s: signed,
            f: Some(read_fingerprint(
                &meter_id(),
                test_time_range(),
                &query,
                &[],
            )),
            // The one thing wrong with this token.
            d: "bwd".to_owned(),
        });

        let err = svc
            .list_usage_records(&ctx(), meter_id(), test_time_range(), &query, &[])
            .await
            .expect_err("a backward cursor must be refused: the raw path reads forward only");
        match err {
            UsageCollectorError::CursorRejected { source, .. } => assert!(
                matches!(source, toolkit_odata::Error::InvalidCursor),
                "expected upstream's InvalidCursor, got {source:?}",
            ),
            other => panic!("expected a cursor rejection, got {other:?}"),
        }
        assert!(
            spy.last_list_order().is_none(),
            "a refused continuation MUST NOT reach the plugin",
        );
    }
}

/// The query a keyset continuation is bound to, enforced where both
/// surfaces pass through it.
///
/// `CursorV1::f` exists so a caller who changes their query between pages is
/// refused rather than served a continuation minted over a different row set.
/// Four inputs decide that row set: the caller's `$filter` and the three typed
/// parameters `gts_type_id`, the read range and `metadata_filter`. None
/// travels inside `$filter`, so each has to enter the fingerprint explicitly
/// — otherwise page 2 of a January query continues from a cursor minted over
/// February, and a wrong page is an `Ok`.
///
/// Asserted through the service rather than the handler: `filter_hash` is
/// `None` for an in-process caller and `validate_cursor_against` skips its
/// comparison when either side is `None`, so this is the caller an edge-only
/// check leaves unprotected.
mod read_path_cursor_fingerprint_tests {
    use std::sync::Arc;

    use std::collections::BTreeMap;

    use time::OffsetDateTime;
    use toolkit_gts::gts_id;
    use toolkit_odata::{CursorV1, ODataOrderBy, ODataQuery, OrderKey, SortDir};
    use toolkit_security::SecurityContext;
    use usage_collector_sdk::{
        IdempotencyKey, Keyset, MetadataFilter, MeterTypeId, RECORD_ID_FIELD, RecordOrigin,
        RecordPage, ResourceRef, TimeRange, UsageCollectorError, UsageCollectorPluginV1,
        UsageRecord, WINDOW_END_FIELD,
    };
    use uuid::Uuid;

    use crate::domain::Service;
    use crate::domain::query::read_fingerprint;
    use crate::domain::test_support::{
        RECORDING_PLUGIN_SUFFIX, RecordingPlugin, ServiceFixture, authenticated_ctx,
        fake_declaration_source_with_metadata, qty, recording_plugin_resolver, test_time_range,
    };

    const GTS_ID: &str = gts_id!("cf.core.uc.usage_record.v1~cf.mini_chat._.tokens_consumed.v1~");

    fn meter_id() -> MeterTypeId {
        MeterTypeId::new(GTS_ID).expect("valid gts_type_id")
    }

    fn ctx() -> SecurityContext {
        authenticated_ctx()
    }

    /// Same shape as `read_path_keyset_floor_tests`.
    fn svc_and_spy() -> (Arc<Service>, Arc<RecordingPlugin>) {
        svc_and_spy_declaring(&[])
    }

    /// The same, over a declaration declaring exactly `declared` metadata
    /// keys: an undeclared key is refused by the query-surface gate long
    /// before the fingerprint is compared.
    fn svc_and_spy_declaring(declared: &[&str]) -> (Arc<Service>, Arc<RecordingPlugin>) {
        let plugin = RecordingPlugin::new();
        let service = ServiceFixture::default()
            .with_source(fake_declaration_source_with_metadata(declared))
            .with_resolver(recording_plugin_resolver())
            .build(
                Arc::clone(&plugin) as Arc<dyn UsageCollectorPluginV1>,
                RECORDING_PLUGIN_SUFFIX,
            );
        (service, plugin)
    }

    /// A second meter, so a cursor can be replayed against another one.
    /// `FakeDeclarationSource` resolves every id alike, so both are usable.
    fn other_meter_id() -> MeterTypeId {
        MeterTypeId::new(gts_id!(
            "cf.core.uc.usage_record.v1~cf.mini_chat._.other_meter.v1~"
        ))
        .expect("valid gts_type_id")
    }

    /// A [`MetadataFilter`] on `key` over `values`.
    fn filter_on(key: &str, values: &[&str]) -> MetadataFilter {
        MetadataFilter::new(key, values.iter().copied()).expect("valid metadata filter")
    }

    /// A range one hour wide, `offset` nanoseconds past the epoch.
    fn range_at(offset_nanos: i64) -> TimeRange {
        let from = time::OffsetDateTime::UNIX_EPOCH + time::Duration::nanoseconds(offset_nanos);
        TimeRange::new(from, from + time::Duration::hours(1)).expect("a one-hour range")
    }

    /// An [`ODataQuery`] whose `$filter` is the parsed `filter` string.
    fn query_with_filter(filter: &str) -> ODataQuery {
        let expr = toolkit_odata::parse_filter_string(filter)
            .expect("test filter parses")
            .into_expr();
        ODataQuery::from(Some(expr))
    }

    /// One persisted row, so the double's page can carry a continuation
    /// without tripping `query::verify_returned_keyset`'s empty-page guard.
    fn fixture_row(meter: &MeterTypeId) -> UsageRecord {
        UsageRecord {
            id: Uuid::from_u128(0xFEED),
            gts_type_id: meter.clone(),
            tenant_id: Uuid::from_u128(1),
            resource_ref: ResourceRef::new("rsc", "compute.vm").expect("valid resource ref"),
            subject_ref: None,
            metadata: BTreeMap::new(),
            quantity: qty("1"),
            idempotency_key: IdempotencyKey::new("idem-mint-fingerprint")
                .expect("valid idempotency key"),
            accepted_at: OffsetDateTime::UNIX_EPOCH,
            origin: RecordOrigin::Live,
            invalidation: None,
            window_start: OffsetDateTime::UNIX_EPOCH,
            window_end: OffsetDateTime::UNIX_EPOCH + time::Duration::hours(1),
        }
    }

    /// Page one of `query` over `range`, dispatched for real, returning the
    /// gateway's own read fingerprint as minted into the page's
    /// `next_cursor.f`.
    ///
    /// The mint leg of every round trip below. A real dispatch rather than a
    /// `read_fingerprint` call, so the value under test is the one the service
    /// actually minted. Read off the minted token, since
    /// `composed.filter_hash` is stripped to `None` before dispatch.
    async fn mint_fingerprint(
        meter: &MeterTypeId,
        range: TimeRange,
        query: &ODataQuery,
        metadata: &[MetadataFilter],
        declared: &[&str],
    ) -> String {
        let (svc, spy) = svc_and_spy_declaring(declared);
        spy.set_list_usage_records_response(RecordPage {
            items: vec![crate::domain::test_support::as_stored(fixture_row(meter))],
            next: Some(
                Keyset::new(["2026-01-01T00:00:00Z", "an-id"], SortDir::Asc)
                    .expect("a non-empty, in-budget keyset"),
            ),
        });
        let page = svc
            .list_usage_records(&ctx(), meter.clone(), range, query, metadata)
            .await
            .expect("page one is a complete request");
        let token = page
            .page_info
            .next_cursor
            .as_deref()
            .expect("the double reports a further page");
        CursorV1::decode(token)
            .expect("the gateway mints a decodable token")
            .f
            .expect("every mint carries the gateway's own read fingerprint")
    }

    /// `query` again, carrying a continuation bound to `keys` verbatim — no
    /// flooring, so the order can be made deliberately unsound.
    ///
    /// The order round-trips through the signed tokens exactly as the handler
    /// rebuilds it, so what is under test is a decoded order rather than one
    /// set alongside the token.
    fn cursor_request_bound_to(query: &ODataQuery, keys: &[(&str, SortDir)]) -> ODataQuery {
        let signed = ODataOrderBy(
            keys.iter()
                .map(|(field, dir)| OrderKey {
                    field: (*field).to_owned(),
                    dir: *dir,
                })
                .collect(),
        )
        .to_signed_tokens();

        let mut out = query.clone();
        out.order =
            ODataOrderBy::from_signed_tokens(&signed).expect("a non-empty order round-trips");
        out.cursor = Some(CursorV1 {
            k: out.order.0.iter().map(|_| "boundary".to_owned()).collect(),
            o: out.order.0[0].dir,
            s: signed,
            f: None,
            d: "fwd".to_owned(),
        });
        out
    }

    fn continuation_of(query: &ODataQuery, fingerprint: &str) -> ODataQuery {
        let mut floored = query.clone();
        crate::domain::query::establish_keyset_order(&mut floored)
            .expect("an empty order is floorable");
        let signed = floored.order.to_signed_tokens();

        let mut page_two = query.clone();
        page_two.order =
            ODataOrderBy::from_signed_tokens(&signed).expect("a non-empty order round-trips");
        page_two.cursor = Some(CursorV1 {
            k: page_two
                .order
                .0
                .iter()
                .map(|_| "boundary".to_owned())
                .collect(),
            o: page_two.order.0[0].dir,
            s: signed,
            f: Some(fingerprint.to_owned()),
            d: "fwd".to_owned(),
        });
        page_two
    }

    /// The 400 blames the token rather than an `$orderby` the caller cannot
    /// send alongside a cursor. The gear does not spell the wire code — that
    /// comes from the `toolkit_odata` error the refusal carries — so this pins
    /// the carried error and `infra::sdk_error_mapping` pins the projection.
    fn assert_query_mismatch(err: &UsageCollectorError) {
        match err {
            UsageCollectorError::CursorRejected { source, .. } => assert!(
                matches!(source, toolkit_odata::Error::FilterMismatch),
                "expected upstream's FilterMismatch, got {source:?}",
            ),
            other => panic!("expected a cursor rejection, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_cursor_minted_under_the_same_range_is_served() {
        // The half a "just refuse every cursor" edit would break. The gateway
        // recomputes its fingerprint fresh on every dispatch, first page or
        // continuation alike, so an unchanged query mints page three's cursor
        // with the identical value page one minted page two's with.
        let caller = query_with_filter("resource_id eq 'r1'");
        let range = test_time_range();
        let fingerprint = mint_fingerprint(&meter_id(), range, &caller, &[], &[]).await;

        let (svc, spy) = svc_and_spy();
        spy.set_list_usage_records_response(RecordPage {
            items: vec![crate::domain::test_support::as_stored(fixture_row(
                &meter_id(),
            ))],
            next: Some(
                Keyset::new(["2026-01-01T00:00:00Z", "an-id"], SortDir::Asc)
                    .expect("a non-empty, in-budget keyset"),
            ),
        });
        let page_two = svc
            .list_usage_records(
                &ctx(),
                meter_id(),
                range,
                &continuation_of(&caller, &fingerprint),
                &[],
            )
            .await
            .expect("page two of an unchanged query MUST be served");

        assert_eq!(
            spy.last_list_time_range(),
            Some(range),
            "the continuation must reach the plugin with the range it was \
             minted under",
        );
        let dispatched = spy
            .last_list_query()
            .expect("the plugin MUST have been dispatched");
        assert_eq!(
            dispatched
                .order
                .0
                .iter()
                .map(|key| (key.field.clone(), key.dir))
                .collect::<Vec<_>>(),
            vec![
                ("window_end".to_owned(), SortDir::Asc),
                ("id".to_owned(), SortDir::Asc),
            ],
            "and with the order rebuilt from the token's signed keys",
        );
        let token_three = page_two
            .page_info
            .next_cursor
            .as_deref()
            .expect("page two reports a further page");
        let minted_three =
            CursorV1::decode(token_three).expect("the gateway mints a decodable token");
        assert_eq!(
            minted_three.f,
            Some(fingerprint),
            "page three's minted cursor must carry the same fingerprint \
             page one's did: the gateway recomputes it fresh from the same \
             caller query and range on every dispatch, so a continuation \
             mints no differently than a first page would",
        );
    }

    #[tokio::test]
    async fn a_cursor_minted_under_a_different_range_never_reaches_the_plugin() {
        // The reason the range is in the fingerprint: same caller, same
        // `$filter`, a different range.
        let caller = query_with_filter("resource_id eq 'r1'");
        let january = test_time_range();
        let february = range_at(60 * 60 * 24 * 31 * 1_000_000_000);
        assert_ne!(january, february, "precondition: two different ranges");
        let fingerprint = mint_fingerprint(&meter_id(), january, &caller, &[], &[]).await;

        let (svc, spy) = svc_and_spy();
        spy.set_list_usage_records_response(RecordPage {
            items: vec![],
            next: None,
        });
        let err = svc
            .list_usage_records(
                &ctx(),
                meter_id(),
                february,
                &continuation_of(&caller, &fingerprint),
                &[],
            )
            .await
            .expect_err("a cursor minted over another range must be refused");

        assert_query_mismatch(&err);
        assert!(
            spy.last_list_order().is_none(),
            "a refused continuation MUST NOT reach the plugin: being served \
             is a wrong page, and a wrong page is an Ok",
        );
    }

    #[tokio::test]
    async fn a_cursor_differing_only_below_the_microsecond_is_rejected() {
        // The truncation hole `TimeRange::canonical_form` exists to avoid:
        // if the range rendering reused the identity derivation's fixed
        // six-digit-microsecond form, these two ranges would fingerprint
        // identically and this cursor would be served.
        let caller = query_with_filter("resource_id eq 'r1'");
        let coarse = range_at(0);
        let nudged = range_at(1);
        let fingerprint = mint_fingerprint(&meter_id(), coarse, &caller, &[], &[]).await;

        let (svc, spy) = svc_and_spy();
        spy.set_list_usage_records_response(RecordPage {
            items: vec![],
            next: None,
        });
        let err = svc
            .list_usage_records(
                &ctx(),
                meter_id(),
                nudged,
                &continuation_of(&caller, &fingerprint),
                &[],
            )
            .await
            .expect_err("a sub-microsecond range change must still be refused");

        assert_query_mismatch(&err);
        assert!(spy.last_list_order().is_none());
    }

    #[tokio::test]
    async fn a_cursor_minted_under_a_different_filter_never_reaches_the_plugin() {
        // An implementation that fingerprinted the range alone would lose
        // `$filter` silently while every range test above stayed green.
        let range = test_time_range();
        let fingerprint = mint_fingerprint(
            &meter_id(),
            range,
            &query_with_filter("resource_id eq 'r1'"),
            &[],
            &[],
        )
        .await;
        let page_two = query_with_filter("resource_id eq 'r2'");

        let (svc, spy) = svc_and_spy();
        spy.set_list_usage_records_response(RecordPage {
            items: vec![],
            next: None,
        });
        let err = svc
            .list_usage_records(
                &ctx(),
                meter_id(),
                range,
                &continuation_of(&page_two, &fingerprint),
                &[],
            )
            .await
            .expect_err("a cursor minted over another filter must be refused");

        assert_query_mismatch(&err);
        assert!(spy.last_list_order().is_none());
    }

    #[tokio::test]
    async fn a_cursor_carrying_no_fingerprint_never_reaches_the_plugin() {
        // `validate_cursor_against` skips its comparison whenever either side
        // is absent. The cursor is caller-supplied JSON, so admitting an
        // absent `f` leaves the whole check optional at the caller's
        // discretion.
        let caller = query_with_filter("resource_id eq 'r1'");
        let range = test_time_range();
        let mut page_two = continuation_of(&caller, "unused");
        page_two.cursor.as_mut().expect("cursor").f = None;

        let (svc, spy) = svc_and_spy();
        spy.set_list_usage_records_response(RecordPage {
            items: vec![],
            next: None,
        });
        let err = svc
            .list_usage_records(&ctx(), meter_id(), range, &page_two, &[])
            .await
            .expect_err("an unbound cursor must be refused");

        assert_query_mismatch(&err);
        assert!(spy.last_list_order().is_none());
    }

    #[tokio::test]
    async fn a_cursor_minted_against_another_meter_never_reaches_the_plugin() {
        // `gts_type_id` is a typed parameter, so no filter hash covers it:
        // without it in the fingerprint a caller can continue a cursor against
        // a different meter and be served rows the token knows nothing about.
        let caller = query_with_filter("resource_id eq 'r1'");
        let range = test_time_range();
        let fingerprint = mint_fingerprint(&meter_id(), range, &caller, &[], &[]).await;

        let (svc, spy) = svc_and_spy();
        spy.set_list_usage_records_response(RecordPage {
            items: vec![],
            next: None,
        });
        let err = svc
            .list_usage_records(
                &ctx(),
                other_meter_id(),
                range,
                &continuation_of(&caller, &fingerprint),
                &[],
            )
            .await
            .expect_err("a cursor minted against another meter must be refused");

        assert_query_mismatch(&err);
        assert!(
            spy.last_list_order().is_none(),
            "a refused continuation MUST NOT reach the plugin",
        );
    }

    #[tokio::test]
    async fn a_cursor_minted_under_a_different_metadata_filter_never_reaches_the_plugin() {
        // The other typed row-selector, and one `$filter` cannot express: the
        // grammar has no surface for a dynamic JSON-map key, so a filter hash
        // reaches none of it.
        let caller = query_with_filter("resource_id eq 'r1'");
        let range = test_time_range();
        let declared = ["region"];
        let fingerprint = mint_fingerprint(
            &meter_id(),
            range,
            &caller,
            &[filter_on("region", &["eu"])],
            &declared,
        )
        .await;

        let (svc, spy) = svc_and_spy_declaring(&declared);
        spy.set_list_usage_records_response(RecordPage {
            items: vec![],
            next: None,
        });
        let err = svc
            .list_usage_records(
                &ctx(),
                meter_id(),
                range,
                &continuation_of(&caller, &fingerprint),
                &[filter_on("region", &["us"])],
            )
            .await
            .expect_err("a cursor minted under another metadata filter must be refused");

        assert_query_mismatch(&err);
        assert!(
            spy.last_list_order().is_none(),
            "a refused continuation MUST NOT reach the plugin",
        );
    }

    #[tokio::test]
    async fn a_metadata_filter_respelled_the_same_way_still_paginates() {
        // The over-rejection guard: a REST caller's repeated `metadata.<key>`
        // parameters arrive in query-string order with duplicates intact, so
        // page two of an unchanged query can carry the same value set in a
        // different spelling. A fingerprint reading the slice verbatim would
        // refuse it.
        let caller = query_with_filter("resource_id eq 'r1'");
        let range = test_time_range();
        let declared = ["region", "tier"];
        let fingerprint = mint_fingerprint(
            &meter_id(),
            range,
            &caller,
            &[
                filter_on("region", &["eu", "us"]),
                filter_on("tier", &["gold"]),
            ],
            &declared,
        )
        .await;

        let (svc, spy) = svc_and_spy_declaring(&declared);
        spy.set_list_usage_records_response(RecordPage {
            items: vec![],
            next: None,
        });
        svc.list_usage_records(
            &ctx(),
            meter_id(),
            range,
            &continuation_of(&caller, &fingerprint),
            // Entries swapped, values re-ordered, one value repeated — all
            // three are things a caller's own query string does.
            &[
                filter_on("tier", &["gold"]),
                filter_on("region", &["us", "eu", "us"]),
            ],
        )
        .await
        .expect("a re-spelled but identical metadata filter MUST still paginate");

        assert!(
            spy.last_list_order().is_some(),
            "the continuation MUST have reached the plugin",
        );
    }

    #[tokio::test]
    async fn an_in_process_order_diverging_from_its_token_never_reaches_the_plugin() {
        // The order the plugin sorts by must be the order the token's boundary
        // values were minted under. Each caller order below is itself a sound
        // keyset and the fingerprint matches, so no structural check sees the
        // divergence; left uncaught, the plugin compares a row-value tuple
        // against boundary values in another order — a misaligned
        // continuation served as an `Ok`. The handler overwrites `query.order`
        // from the token, so only an in-process caller can express this.
        let caller = query_with_filter("resource_id eq 'r1'");
        let range = test_time_range();
        let fingerprint = mint_fingerprint(&meter_id(), range, &caller, &[], &[]).await;

        // Permuted, narrower, and wider-with-a-different-lead.
        for caller_order in [
            vec![
                (RECORD_ID_FIELD, SortDir::Asc),
                (WINDOW_END_FIELD, SortDir::Asc),
            ],
            vec![(WINDOW_END_FIELD, SortDir::Asc)],
            vec![
                ("resource_id", SortDir::Asc),
                (WINDOW_END_FIELD, SortDir::Asc),
                (RECORD_ID_FIELD, SortDir::Asc),
            ],
        ] {
            // A conforming token, then the caller's contradictory order
            // laid over it.
            let mut divergent = continuation_of(&caller, &fingerprint);
            divergent.order = ODataOrderBy(
                caller_order
                    .iter()
                    .map(|(field, dir)| OrderKey {
                        field: (*field).to_owned(),
                        dir: *dir,
                    })
                    .collect(),
            );

            let (svc, spy) = svc_and_spy();
            spy.set_list_usage_records_response(RecordPage {
                items: vec![],
                next: None,
            });
            svc.list_usage_records(&ctx(), meter_id(), range, &divergent, &[])
                .await
                .expect("the token itself is conforming, so this must be served");

            assert_eq!(
                spy.last_list_order()
                    .expect("the plugin MUST have been dispatched"),
                vec![
                    (WINDOW_END_FIELD.to_owned(), SortDir::Asc),
                    (RECORD_ID_FIELD.to_owned(), SortDir::Asc),
                ],
                "the plugin MUST sort by the order the TOKEN bound, never \
                 the caller's {caller_order:?}: a row-value tuple compared \
                 against boundary values in another order is a silently \
                 wrong page",
            );
        }
    }

    #[tokio::test]
    async fn an_in_process_token_whose_signed_keys_do_not_decode_never_reaches_the_plugin() {
        // A shape only the binding can catch: the token's `s` is malformed so
        // no order can be derived from it, but the caller supplied a sound
        // `order` of their own that a structural check would read instead.
        // The handler rejects this at the edge, so only an in-process caller
        // can express it.
        let caller = query_with_filter("resource_id eq 'r1'");
        let range = test_time_range();
        let fingerprint = mint_fingerprint(&meter_id(), range, &caller, &[], &[]).await;

        let mut malformed = continuation_of(&caller, &fingerprint);
        malformed.cursor.as_mut().expect("cursor").s = String::new();

        let (svc, spy) = svc_and_spy();
        spy.set_list_usage_records_response(RecordPage {
            items: vec![],
            next: None,
        });
        let err = svc
            .list_usage_records(&ctx(), meter_id(), range, &malformed, &[])
            .await
            .expect_err("a token whose signed keys do not decode must be refused");

        match err {
            UsageCollectorError::CursorRejected { source, .. } => assert!(
                matches!(source, toolkit_odata::Error::InvalidCursor),
                "expected upstream's InvalidCursor, got {source:?}",
            ),
            other => panic!("expected a cursor rejection, got {other:?}"),
        }
        assert!(spy.last_list_order().is_none());
    }

    #[tokio::test]
    async fn a_sound_caller_order_cannot_launder_an_unsound_token() {
        // Why the binding runs BEFORE the structural check. The token is bound
        // to `+resource_id` alone, which is no keyset; the caller supplies the
        // canonical order beside it. Check-then-bind would validate the
        // caller's sound order and then overwrite it with the token's unsound
        // one, handing the plugin the very order the check exists to refuse.
        let caller = query_with_filter("resource_id eq 'r1'");
        let range = test_time_range();
        let fingerprint = mint_fingerprint(&meter_id(), range, &caller, &[], &[]).await;

        let mut laundered = cursor_request_bound_to(&caller, &[("resource_id", SortDir::Asc)]);
        laundered.cursor.as_mut().expect("cursor").f = Some(fingerprint);
        laundered.order = ODataOrderBy(vec![
            OrderKey {
                field: WINDOW_END_FIELD.to_owned(),
                dir: SortDir::Asc,
            },
            OrderKey {
                field: RECORD_ID_FIELD.to_owned(),
                dir: SortDir::Asc,
            },
        ]);

        let (svc, spy) = svc_and_spy();
        spy.set_list_usage_records_response(RecordPage {
            items: vec![],
            next: None,
        });
        let err = svc
            .list_usage_records(&ctx(), meter_id(), range, &laundered, &[])
            .await
            .expect_err("an unsound token must be refused whatever order accompanies it");

        assert!(matches!(err, UsageCollectorError::CursorRejected { .. }));
        assert!(
            spy.last_list_order().is_none(),
            "a sound caller order MUST NOT launder an unsound token past the check",
        );
    }

    #[tokio::test]
    async fn a_doubly_defective_cursor_is_refused_for_its_order_first() {
        // `require_continuation_keyset` checks structure before relevance, and
        // the order is caller-visible: a token BOTH bound to an unsound keyset
        // and minted over another query carries `InvalidCursor`, and would
        // carry `FilterMismatch` if the two checks were swapped — a different
        // wire reason code and `error_category`. Every other cursor test is
        // defective in exactly one way, so none of them distinguishes this.
        let caller = query_with_filter("resource_id eq 'r1'");
        let range = test_time_range();

        // Bound to a one-key order (not a keyset: it names neither
        // canonical field) AND carrying a fingerprint from another range.
        let stale = mint_fingerprint(
            &meter_id(),
            range_at(60 * 60 * 24 * 1_000_000_000),
            &caller,
            &[],
            &[],
        )
        .await;
        let mut doubly_defective =
            cursor_request_bound_to(&caller, &[("resource_id", SortDir::Asc)]);
        doubly_defective.cursor.as_mut().expect("cursor").f = Some(stale);

        let (svc, spy) = svc_and_spy();
        spy.set_list_usage_records_response(RecordPage {
            items: vec![],
            next: None,
        });
        let err = svc
            .list_usage_records(&ctx(), meter_id(), range, &doubly_defective, &[])
            .await
            .expect_err("a doubly-defective token must be refused");

        match err {
            UsageCollectorError::CursorRejected { source, .. } => assert!(
                matches!(source, toolkit_odata::Error::InvalidCursor),
                "the ORDER defect must be reported: it is checked first, and \
                 a swap would silently re-label this as FilterMismatch; got \
                 {source:?}",
            ),
            other => panic!("expected a cursor rejection, got {other:?}"),
        }
        assert!(spy.last_list_order().is_none());
    }

    #[tokio::test]
    async fn the_dispatched_fingerprint_is_computed_from_the_callers_filter_not_the_composed_one() {
        // `compose_query_with_scope` AND-merges the server-injected PDP scope
        // into `$filter` and preserves the caller's `filter_hash`: the scope
        // is not caller-controlled, so a fingerprint over the composed filter
        // embeds a value the next request's recomputation cannot reproduce.
        // The round trip is stable either way here (the PDP fake returns the
        // same scope every call), so this asserts the value itself.
        let caller = query_with_filter("resource_id eq 'r1'");
        let range = test_time_range();

        let (svc, spy) = svc_and_spy();
        spy.set_list_usage_records_response(RecordPage {
            items: vec![crate::domain::test_support::as_stored(fixture_row(
                &meter_id(),
            ))],
            next: Some(
                Keyset::new(["2026-01-01T00:00:00Z", "an-id"], SortDir::Asc)
                    .expect("a non-empty, in-budget keyset"),
            ),
        });
        let page = svc
            .list_usage_records(&ctx(), meter_id(), range, &caller, &[])
            .await
            .expect("page one is a complete request");

        let dispatched = spy
            .last_list_query()
            .expect("the plugin MUST have been dispatched");
        assert_ne!(
            toolkit_odata::short_filter_hash(dispatched.filter()),
            toolkit_odata::short_filter_hash(caller.filter()),
            "precondition: the PDP scope must actually have narrowed the \
             composed $filter, or this test cannot tell the two apart",
        );
        let token = page
            .page_info
            .next_cursor
            .as_deref()
            .expect("page one reports a further page");
        let minted = CursorV1::decode(token).expect("the gateway mints a decodable token");
        assert_eq!(
            minted.f,
            Some(read_fingerprint(&meter_id(), range, &caller, &[])),
            "the fingerprint the gateway mints MUST be computed from the \
             caller's filter and range, never the PDP-composed filter",
        );
    }
}

// ── The gateway owns the raw-path cursor ───────────────────
//
// `cpt-cf-usage-collector-dod-gateway-owned-cursor` requires the plugin
// receive "a structured keyset of the last row's sort values" and never mint,
// encode or interpret a wire token. Before the reshape the plugin minted the
// token and the gear passed it through; this module is the oracle for the
// reversed obligation, on both edges of one dispatch: what the plugin is
// handed (no cursor, no keyset on a first page) and what the caller gets back
// (a token the gear minted from the plugin's keyset).
mod gateway_owned_cursor_tests {
    use std::collections::BTreeMap;
    use std::sync::{Arc, Mutex};

    use async_trait::async_trait;
    use time::OffsetDateTime;
    use toolkit_gts::gts_id;
    use toolkit_odata::{CursorV1, ODataQuery, SortDir};
    use toolkit_security::SecurityContext;
    use usage_collector_sdk::{
        IdempotencyKey, Keyset, MetadataFilter, MeterRef, MeterTypeId, RecordOrigin, RecordPage,
        ResourceRef, TimeRange, UsageCollectorPluginError, UsageCollectorPluginV1, UsageRecord,
    };
    use uuid::Uuid;

    use crate::domain::Service;
    use crate::domain::test_support::{
        ServiceFixture, authenticated_ctx, fake_declaration_source_with_metadata, qty,
        recording_plugin_resolver, test_time_range,
    };

    const GTS_ID: &str = gts_id!("cf.core.uc.usage_record.v1~cf.mini_chat._.tokens_consumed.v1~");

    fn meter_id() -> MeterTypeId {
        MeterTypeId::new(GTS_ID).expect("valid gts_type_id")
    }

    fn ctx() -> SecurityContext {
        authenticated_ctx()
    }

    /// What one `list_usage_records` dispatch handed [`ListCallRecorder`].
    #[derive(Debug, Clone, Default)]
    struct RecordedListCall {
        /// Whether `query.cursor` was `None` on this dispatch: the plugin
        /// must never see a wire token ([`crate::domain::query`]).
        cursor_was_none: bool,
        /// `query.filter_hash` as dispatched. The SPI doc says the slot
        /// carries no guarantee on this method and MUST NOT be read, so a
        /// conforming dispatch strips it to `None` on every call.
        filter_hash: Option<String>,
        /// The `keyset` parameter this dispatch was handed.
        keyset: Option<Keyset>,
    }

    /// A `list_usage_records`-only double; every other SPI method fails
    /// loudly. It records what it was handed and always answers a fixed page
    /// carrying a continuation, so one call shows both what the plugin
    /// received and what the gateway minted from what it returned.
    #[derive(Default)]
    struct ListCallRecorder {
        last_call: Mutex<Option<RecordedListCall>>,
    }

    impl ListCallRecorder {
        fn last_call(&self) -> Option<RecordedListCall> {
            self.last_call.lock().expect("mutex").clone()
        }

        /// The boundary values of the fixed [`Keyset`] this double's page
        /// always carries.
        fn returned_keyset_values() -> Vec<String> {
            vec![
                "2026-01-01T00:00:00Z".to_owned(),
                Uuid::from_u128(0xF00D).to_string(),
            ]
        }

        /// The one row this double's page always carries. A page carrying a
        /// continuation keyset MUST NOT be empty
        /// (`query::verify_returned_keyset`): a continuation names the last
        /// row of the page it continues.
        fn returned_row() -> UsageRecord {
            UsageRecord {
                id: Uuid::from_u128(0xF00D),
                gts_type_id: meter_id(),
                tenant_id: Uuid::from_u128(1),
                resource_ref: ResourceRef::new("rsc", "compute.vm").expect("valid resource ref"),
                subject_ref: None,
                metadata: BTreeMap::new(),
                quantity: qty("1"),
                idempotency_key: IdempotencyKey::new("idem-list-call-recorder")
                    .expect("valid idempotency key"),
                accepted_at: OffsetDateTime::UNIX_EPOCH,
                origin: RecordOrigin::Live,
                invalidation: None,
                window_start: OffsetDateTime::UNIX_EPOCH,
                window_end: OffsetDateTime::UNIX_EPOCH + time::Duration::hours(1),
            }
        }
    }

    #[async_trait]
    impl UsageCollectorPluginV1 for ListCallRecorder {
        async fn create_usage_records(
            &self,
            _records: Vec<(MeterRef, usage_collector_sdk::StoredUsageRecord)>,
        ) -> Result<
            Vec<Result<usage_collector_sdk::StoredUsageRecord, UsageCollectorPluginError>>,
            UsageCollectorPluginError,
        > {
            Err(UsageCollectorPluginError::internal(
                "test_fake: ListCallRecorder: create_usage_records must not be called",
            ))
        }

        async fn query_aggregated_usage_records(
            &self,
            _meter: &MeterRef,
            _time_range: TimeRange,
            _fold: usage_collector_sdk::AggregationFold,
            _query: &ODataQuery,
            _metadata_filter: &[MetadataFilter],
            _group_by: &[usage_collector_sdk::AggregationDimension],
        ) -> Result<usage_collector_sdk::AggregationResult, UsageCollectorPluginError> {
            Err(UsageCollectorPluginError::internal(
                "test_fake: ListCallRecorder: query_aggregated_usage_records must not be called",
            ))
        }

        async fn list_usage_records(
            &self,
            _meter: &MeterRef,
            _time_range: TimeRange,
            query: &ODataQuery,
            _metadata_filter: &[MetadataFilter],
            keyset: Option<&Keyset>,
        ) -> Result<RecordPage, UsageCollectorPluginError> {
            *self.last_call.lock().expect("mutex") = Some(RecordedListCall {
                cursor_was_none: query.cursor.is_none(),
                filter_hash: query.filter_hash.clone(),
                keyset: keyset.cloned(),
            });
            Ok(RecordPage {
                items: vec![crate::domain::test_support::as_stored(Self::returned_row())],
                next: Some(
                    Keyset::new(Self::returned_keyset_values(), SortDir::Asc)
                        .expect("a non-empty, in-budget keyset"),
                ),
            })
        }

        async fn get_usage_record(
            &self,
            _id: Uuid,
            _scope: &toolkit_odata::ast::Expr,
            _converged_only: bool,
        ) -> Result<usage_collector_sdk::StoredUsageRecord, UsageCollectorPluginError> {
            Err(UsageCollectorPluginError::internal(
                "test_fake: ListCallRecorder: get_usage_record must not be called",
            ))
        }

        async fn read_feed_page(
            &self,
            _subscription: &[MeterRef],
            _scope: &toolkit_odata::ast::Expr,
            _start: usage_collector_sdk::FeedStart<usage_collector_sdk::FeedPosition>,
            _until: Option<usage_collector_sdk::FeedPosition>,
            _limit: u64,
        ) -> Result<
            usage_collector_sdk::FeedPage<
                usage_collector_sdk::FeedPosition,
                usage_collector_sdk::StoredUsageRecord,
            >,
            UsageCollectorPluginError,
        > {
            Err(UsageCollectorPluginError::internal(
                "test_fake: ListCallRecorder: read_feed_page must not be called",
            ))
        }

        async fn get_reconciliation_metadata(
            &self,
            _tenant_id: Uuid,
            _meter: &MeterRef,
            _time_range: TimeRange,
            _fold: usage_collector_sdk::AggregationFold,
            _scope: &toolkit_odata::ast::Expr,
        ) -> Result<usage_collector_sdk::ReconciliationMetadata, UsageCollectorPluginError>
        {
            Err(UsageCollectorPluginError::internal(
                "test_fake: ListCallRecorder: get_reconciliation_metadata must not be called",
            ))
        }
    }

    fn service_with_list_recorder(plugin: Arc<ListCallRecorder>) -> Arc<Service> {
        ServiceFixture::default()
            .with_source(fake_declaration_source_with_metadata(&[]))
            .with_resolver(recording_plugin_resolver())
            .build(
                plugin as Arc<dyn UsageCollectorPluginV1>,
                "test.usage_collector.list.gateway_owned_cursor.v1",
            )
    }

    /// The gateway mints the raw page's continuation, and the plugin never
    /// sees a wire cursor — it receives a structured keyset of the last row's
    /// sort values instead. See [`crate::domain::query`].
    #[tokio::test]
    async fn the_gateway_mints_the_continuation_and_the_plugin_receives_a_keyset() {
        let recorder = Arc::new(ListCallRecorder::default());
        let svc = service_with_list_recorder(Arc::clone(&recorder));

        let page = svc
            .list_usage_records(
                &ctx(),
                meter_id(),
                test_time_range(),
                &ODataQuery::new(),
                &[],
            )
            .await
            .expect("a first page is served");

        // What the plugin was handed: no wire cursor, no filter_hash, and
        // no keyset on a first page.
        let call = recorder.last_call().expect("the plugin was dispatched");
        assert!(
            call.cursor_was_none,
            "the gateway must strip CursorV1 before dispatch; the plugin saw one"
        );
        assert!(
            call.filter_hash.is_none(),
            "the gateway must strip filter_hash before dispatch (ruling H35, \
             the same MUST-NOT as the cursor); the plugin saw {:?}",
            call.filter_hash
        );
        assert!(
            call.keyset.is_none(),
            "a first page carries no seek key; got {:?}",
            call.keyset
        );

        // What the caller got: a token the gear minted, bound to the order
        // the gear dispatched and carrying the gear's own fingerprint.
        let token = page
            .page_info
            .next_cursor
            .as_deref()
            .expect("the recorder's page reports a further page");
        let minted = CursorV1::decode(token).expect("the gateway mints a decodable token");
        assert_eq!(
            minted.k,
            ListCallRecorder::returned_keyset_values(),
            "the token's boundary values are the keyset the plugin returned"
        );
        assert!(
            minted.f.is_some(),
            "the minted token carries the gear's read fingerprint"
        );
        assert_eq!(minted.d, "fwd", "the raw path reads forward only");
        // `s` is what `bind_continuation_order` rebuilds the follow-up
        // dispatch's order from; a wrong `s` is a silently wrong page.
        assert_eq!(
            minted.s, "+window_end,+id",
            "the token must bind to the order the gateway actually \
             dispatched under (the floored canonical default, since this \
             call named none), not an order recomputed from the caller's \
             own unfloored request",
        );
        assert_eq!(
            minted.o,
            SortDir::Asc,
            "the token's recorded direction is the keyset's own direction",
        );
    }

    /// A minted token, fed back as the next request's cursor, reaches the
    /// plugin with exactly the keyset it returned — the mint→decode→seek seam
    /// closed end to end. The pg walk tests drive `Option<&Keyset>` directly,
    /// the other gear tests feed hand-built `CursorV1`s, and the test above
    /// mints a token it never re-dispatches; this is the one that walks a page
    /// boundary through the wire token as a real two-page read does.
    #[tokio::test]
    async fn a_minted_token_fed_back_reaches_the_plugin_with_the_keyset_it_returned() {
        let recorder = Arc::new(ListCallRecorder::default());
        let svc = service_with_list_recorder(Arc::clone(&recorder));

        let page_one = svc
            .list_usage_records(
                &ctx(),
                meter_id(),
                test_time_range(),
                &ODataQuery::new(),
                &[],
            )
            .await
            .expect("a first page is served");
        let token = page_one
            .page_info
            .next_cursor
            .clone()
            .expect("the recorder's page reports a further page");

        let mut continuation = ODataQuery::new();
        continuation.cursor =
            Some(CursorV1::decode(&token).expect("the gateway mints a decodable token"));

        svc.list_usage_records(&ctx(), meter_id(), test_time_range(), &continuation, &[])
            .await
            .expect("the gateway's own minted token is honoured on its own follow-up");

        let call = recorder
            .last_call()
            .expect("the plugin was dispatched on the follow-up");
        assert!(
            call.cursor_was_none,
            "the gateway must strip the wire cursor on every paged call, \
             the continuation included - not only on a first page where \
             there was never one to strip",
        );
        assert!(
            call.filter_hash.is_none(),
            "the gateway must strip filter_hash on every paged call too \
             (ruling H35) - the plugin saw {:?}",
            call.filter_hash
        );
        let received = call
            .keyset
            .expect("a continuation carries the decoded keyset");
        let expected = Keyset::new(ListCallRecorder::returned_keyset_values(), SortDir::Asc)
            .expect("a non-empty, in-budget keyset");
        assert_eq!(
            received, expected,
            "the keyset the plugin receives on the follow-up must equal the \
             one it returned on the page that minted this token - `Keyset`'s \
             `PartialEq` is exactly this comparison, and anything else means \
             the mint, the encode/decode round trip, or the extraction back \
             out dropped or corrupted a value",
        );
    }
}

/// The gear mints the raw path's `next_cursor` from whatever [`Keyset`] a
/// plugin hands back on [`usage_collector_sdk::RecordPage::next`], so
/// `Service::list_usage_records` is the only place to catch one that cannot
/// be bound: the wrong number of boundary values, a direction disagreeing with
/// the dispatched order, or a token on a page with no rows to continue from.
/// Each is a host-contract breach rather than caller input, on the distinction
/// [`crate::domain::Service::read_usage_feed`] draws for the feed's
/// over-length page.
mod returned_keyset_verification_tests {
    use std::collections::BTreeMap;
    use std::sync::Arc;

    use time::OffsetDateTime;
    use toolkit_gts::gts_id;
    use toolkit_odata::{CursorV1, ODataQuery, SortDir};
    use usage_collector_sdk::{
        IdempotencyKey, Keyset, MetadataFilter, MeterTypeId, RecordOrigin, RecordPage, ResourceRef,
        UsageCollectorError, UsageRecord,
    };
    use uuid::Uuid;

    use crate::domain::Service;
    use crate::domain::test_support::{
        RECORDING_PLUGIN_SUFFIX, RecordingPlugin, ServiceFixture, authenticated_ctx,
        fake_declaration_source_with_metadata, qty, recording_plugin_resolver, test_time_range,
    };

    const GTS_ID: &str = gts_id!("cf.core.uc.usage_record.v1~cf.mini_chat._.tokens_consumed.v1~");

    fn meter() -> MeterTypeId {
        MeterTypeId::new(GTS_ID).expect("valid gts_type_id")
    }

    /// One persisted row, so a page that is meant to look non-empty does.
    fn sample_record() -> UsageRecord {
        UsageRecord {
            id: Uuid::from_u128(0xBEEF),
            gts_type_id: meter(),
            tenant_id: Uuid::from_u128(1),
            resource_ref: ResourceRef::new("rsc", "compute.vm").expect("valid resource ref"),
            subject_ref: None,
            metadata: BTreeMap::new(),
            quantity: qty("1"),
            idempotency_key: IdempotencyKey::new("idem-returned-keyset")
                .expect("valid idempotency key"),
            accepted_at: OffsetDateTime::UNIX_EPOCH,
            origin: RecordOrigin::Live,
            invalidation: None,
            window_start: OffsetDateTime::UNIX_EPOCH,
            window_end: OffsetDateTime::UNIX_EPOCH + time::Duration::hours(1),
        }
    }

    /// A `Service` whose plugin answers a one-row page carrying `keyset` as
    /// its continuation. `region` is declared so the metadata-filter test
    /// below can narrow on it.
    fn service_with_list_keyset(keyset: Keyset) -> Arc<Service> {
        let plugin = RecordingPlugin::new();
        plugin.set_list_usage_records_response(RecordPage {
            items: vec![crate::domain::test_support::as_stored(sample_record())],
            next: Some(keyset),
        });
        ServiceFixture::default()
            .with_source(fake_declaration_source_with_metadata(&["region"]))
            .with_resolver(recording_plugin_resolver())
            .build(plugin, RECORDING_PLUGIN_SUFFIX)
    }

    /// The same, over a declaration declaring every one of `keys` — a wider
    /// `metadata_filter` surface than [`service_with_list_keyset`]'s lone
    /// `region`.
    fn service_with_list_keyset_declaring(keyset: Keyset, keys: &[&str]) -> Arc<Service> {
        let plugin = RecordingPlugin::new();
        plugin.set_list_usage_records_response(RecordPage {
            items: vec![crate::domain::test_support::as_stored(sample_record())],
            next: Some(keyset),
        });
        ServiceFixture::default()
            .with_source(fake_declaration_source_with_metadata(keys))
            .with_resolver(recording_plugin_resolver())
            .build(plugin, RECORDING_PLUGIN_SUFFIX)
    }

    /// A `Service` whose plugin answers an EMPTY page that still carries
    /// `keyset`.
    fn service_with_empty_page_and_keyset(keyset: Keyset) -> Arc<Service> {
        let plugin = RecordingPlugin::new();
        plugin.set_list_usage_records_response(RecordPage {
            items: vec![],
            next: Some(keyset),
        });
        ServiceFixture::default()
            .with_source(fake_declaration_source_with_metadata(&[]))
            .with_resolver(recording_plugin_resolver())
            .build(plugin, RECORDING_PLUGIN_SUFFIX)
    }

    /// A keyset of the wrong width is a host-contract breach, not a page: a
    /// plugin returning three values for a two-key order would mint a token
    /// whose boundary values cannot line up with the order it binds.
    /// `Internal`, not `InvalidArgument` — no request the caller can send
    /// avoids it.
    #[tokio::test]
    async fn a_returned_keyset_of_the_wrong_arity_is_refused_as_internal() {
        let svc = service_with_list_keyset(
            Keyset::new(["a", "b", "c"], SortDir::Asc).expect("three values"),
        );
        let err = svc
            .list_usage_records(
                &authenticated_ctx(),
                meter(),
                test_time_range(),
                &ODataQuery::new(),
                &[],
            )
            .await
            .expect_err("a three-value keyset against a two-key order is a breach");
        let UsageCollectorError::Internal { ref detail } = err else {
            panic!("expected Internal, got {err:?}");
        };
        assert!(
            detail.contains('3') && detail.contains('2'),
            "the detail must name both widths so an operator can see which \
             side is wrong; got {detail}"
        );
    }

    /// The same for the direction, which the arity check cannot catch.
    #[tokio::test]
    async fn a_returned_keyset_sorting_against_the_dispatched_order_is_refused() {
        // The gear dispatches Asc: no caller order, so the appended pair.
        let svc = service_with_list_keyset(
            Keyset::new(["2026-01-01T00:00:00Z", "an-id"], SortDir::Desc).expect("two values"),
        );
        let err = svc
            .list_usage_records(
                &authenticated_ctx(),
                meter(),
                test_time_range(),
                &ODataQuery::new(),
                &[],
            )
            .await
            .expect_err("a Desc keyset against an Asc order is a breach");
        assert!(
            matches!(err, UsageCollectorError::Internal { .. }),
            "got {err:?}"
        );
    }

    /// An empty page carrying a continuation. Arity and direction both check
    /// out, so neither guard above catches it, and following the token yields
    /// another empty page with another token — a hang rather than a wrong
    /// value.
    #[tokio::test]
    async fn an_empty_page_carrying_a_continuation_is_refused() {
        let svc = service_with_empty_page_and_keyset(
            Keyset::new(["2026-01-01T00:00:00Z", "an-id"], SortDir::Asc).expect("two values"),
        );
        let err = svc
            .list_usage_records(
                &authenticated_ctx(),
                meter(),
                test_time_range(),
                &ODataQuery::new(),
                &[],
            )
            .await
            .expect_err("a continuation with no rows to continue from is a breach");
        let UsageCollectorError::Internal { ref detail } = err else {
            panic!("expected Internal, got {err:?}");
        };
        assert!(
            detail.contains("empty"),
            "the detail must say the page was empty, which is the fact an \
             operator acts on; got {detail}"
        );
    }

    /// The gear mints from its own `read_fingerprint`, so the fingerprint
    /// must still bind all three typed parameters: a caller who changes
    /// `metadata_filter` between pages is refused, not served a page computed
    /// under a different selection.
    #[tokio::test]
    async fn a_cursor_is_refused_when_the_metadata_filter_changed_between_pages() {
        let svc = service_with_list_keyset(
            Keyset::new(["2026-01-01T00:00:00Z", "an-id"], SortDir::Asc).expect("two values"),
        );
        let first = svc
            .list_usage_records(
                &authenticated_ctx(),
                meter(),
                test_time_range(),
                &ODataQuery::new(),
                &[],
            )
            .await
            .expect("a first page is served");
        let token = first
            .page_info
            .next_cursor
            .as_deref()
            .expect("the double reports a further page");

        let mut resumed = ODataQuery::new();
        resumed.cursor = Some(CursorV1::decode(token).expect("decodable"));
        let narrowed = [MetadataFilter::new("region", ["eu"]).expect("a valid filter")];

        let err = svc
            .list_usage_records(
                &authenticated_ctx(),
                meter(),
                test_time_range(),
                &resumed,
                &narrowed,
            )
            .await
            .expect_err("page two under a different metadata_filter is a different query");
        assert!(
            matches!(err, UsageCollectorError::CursorRejected { .. }),
            "a changed typed parameter is a filter mismatch on the cursor, \
             not a 500; got {err:?}"
        );
    }

    /// A minted cursor stays inside the published `maxLength` at the widest
    /// filter set **and** the widest keyset the surface admits: 16 predicates
    /// of 32 values each plus the widest admissible `$orderby`, with the
    /// keyset's own contribution bounded separately by `MAX_KEYSET_BYTES`.
    ///
    /// Both axes sit at their actual worst case, not merely their arity: the
    /// keyset below uses `MAX_KEYSET_BYTES`'s own per-field worst-case values
    /// (`keyset.rs`, derived in `keyset_tests.rs`) rather than same-length
    /// placeholders — two of the eight fields are 256-character
    /// caller-supplied strings, so an arity-only widening leaves the composed
    /// worst case unpinned.
    #[tokio::test]
    async fn the_widest_admissible_request_mints_a_cursor_inside_the_published_bound() {
        let values: Vec<String> = (0..crate::domain::query::MAX_METADATA_FILTER_VALUES)
            .map(|i| format!("value-{i:04}"))
            .collect();
        let keys: Vec<String> = (0..crate::domain::query::MAX_METADATA_FILTERS)
            .map(|i| format!("key-{i:04}"))
            .collect();
        let filters: Vec<MetadataFilter> = keys
            .iter()
            .map(|key| MetadataFilter::new(key.clone(), values.clone()).expect("valid"))
            .collect();
        let declared_keys: Vec<&str> = keys.iter().map(String::as_str).collect();

        let order = toolkit_odata::ODataOrderBy(
            usage_collector_sdk::KEYSET_SAFE_RECORD_FIELDS
                .iter()
                .map(|f| toolkit_odata::OrderKey {
                    field: (*f).to_owned(),
                    dir: SortDir::Asc,
                })
                .collect(),
        );

        let four_byte_char = '\u{1F600}'; // 😀 — 1 char, 4 UTF-8 bytes, no JSON escaping needed
        let attribution_at_cap: String = std::iter::repeat_n(four_byte_char, 256).collect();
        let uuid_width = "00000000-0000-0000-0000-000000000001".to_owned();
        let rfc3339_at_width = "2023-11-14T23:13:20.123456789Z".to_owned();
        let origin_worst = "backfill".to_owned();
        let worst_case_keyset_values: Vec<String> = usage_collector_sdk::KEYSET_SAFE_RECORD_FIELDS
            .iter()
            .map(|field| match *field {
                "resource_id" | "resource_type" => attribution_at_cap.clone(),
                "id" | "tenant_id" => uuid_width.clone(),
                "window_start" | "window_end" | "accepted_at" => rfc3339_at_width.clone(),
                "origin" => origin_worst.clone(),
                other => panic!(
                    "KEYSET_SAFE_RECORD_FIELDS grew a field (`{other}`) this test does not \
                     yet assign a worst-case value for; classify it above rather than \
                     silently narrowing the keyset axis back to an arity-only test"
                ),
            })
            .collect();
        assert_eq!(
            serde_json::to_string(&worst_case_keyset_values)
                .expect("Vec<String> always serializes")
                .len(),
            usage_collector_sdk::MAX_KEYSET_BYTES,
            "precondition: this keyset must sit at the published bound's actual worst case, \
             not merely at some value inside it, or this test proves nothing about the bound",
        );

        let svc = service_with_list_keyset_declaring(
            Keyset::new(worst_case_keyset_values, SortDir::Asc)
                .expect("exactly MAX_KEYSET_BYTES is inside the bound (inclusive)"),
            &declared_keys,
        );

        let page = svc
            .list_usage_records(
                &authenticated_ctx(),
                meter(),
                test_time_range(),
                &ODataQuery::new().with_order(order),
                &filters,
            )
            .await
            .expect("the widest admissible request is served");

        let token = page
            .page_info
            .next_cursor
            .as_deref()
            .expect("the double reports a further page");
        assert!(
            token.len() <= crate::domain::feed::MAX_CURSOR_TOKEN_CHARS,
            "a minted cursor must stay inside the published maxLength; the widest \
             admissible request produced {} bytes against a bound of {}",
            token.len(),
            crate::domain::feed::MAX_CURSOR_TOKEN_CHARS,
        );
    }
}

// ── Withdrawal exclusion from the fold ─────────────────────────────────────
//
// The gear folds nothing: `query_aggregated_usage_records` dispatches to the
// plugin and returns what it computes, so these tests bind no real storage
// plugin. They hold the reference semantics
// ([`usage_collector_sdk::UsageCollectorPluginV1::query_aggregated_usage_records`])
// against the in-memory `FoldingPlugin`, and catch a gear-side regression in
// what the plugin is handed: a dropped `time_range`, or a fold other than the
// declared one, moves these numbers.
mod withdrawal_exclusion_tests {
    use std::collections::BTreeMap;
    use std::sync::Arc;

    use bigdecimal::BigDecimal;
    use time::OffsetDateTime;
    use toolkit_gts::gts_id;
    use toolkit_odata::ODataQuery;
    use toolkit_security::SecurityContext;
    use usage_collector_sdk::{
        AggregationFold, AggregationResult, EntryType, IdempotencyKey, MeterTypeId, RecordOrigin,
        ResourceRef, UsageCollectorPluginV1, UsageRecord, derive_usage_record_id,
    };
    use uuid::Uuid;

    use crate::domain::Service;
    use crate::domain::test_support::{
        FOLDING_PLUGIN_SUFFIX, FoldingPlugin, ServiceFixture, authenticated_ctx,
        fake_declaration_source_with_fold, recording_plugin_resolver, test_time_range,
    };

    const GTS_ID: &str = gts_id!("cf.core.uc.usage_record.v1~cf.mini_chat._.tokens_consumed.v1~");

    /// The tenant every entry here is attributed to — the one
    /// [`recording_plugin_resolver`] returns as its fixed `OWNER_TENANT_ID`
    /// constraint, so the composed scope names the fixture's own rows.
    fn tenant_id() -> Uuid {
        Uuid::from_u128(2)
    }

    fn meter_id() -> MeterTypeId {
        MeterTypeId::new(GTS_ID).expect("valid gts_type_id")
    }

    fn ctx() -> SecurityContext {
        authenticated_ctx()
    }

    /// A persisted measurement inside [`test_time_range`], carrying `quantity`
    /// and ending `minutes` after the epoch. Built through
    /// [`derive_usage_record_id`] rather than a random `id`, so the identity
    /// is the one an invalidation's reference has to name.
    fn measurement(idem: &str, value: &str, minutes: i64) -> UsageRecord {
        let window_start = OffsetDateTime::UNIX_EPOCH;
        let window_end = OffsetDateTime::UNIX_EPOCH + time::Duration::minutes(minutes);
        let idempotency_key = IdempotencyKey::new(idem).expect("valid idempotency key");
        UsageRecord {
            id: derive_usage_record_id(
                tenant_id(),
                &meter_id(),
                &idempotency_key,
                window_start,
                window_end,
                EntryType::Record,
            ),
            gts_type_id: meter_id(),
            tenant_id: tenant_id(),
            resource_ref: ResourceRef::new("rsc-fold", "compute.vm").expect("valid resource ref"),
            subject_ref: None,
            metadata: BTreeMap::new(),
            quantity: value.parse().expect("valid decimal quantity"),
            idempotency_key,
            accepted_at: window_end,
            origin: RecordOrigin::Live,
            invalidation: None,
            window_start,
            window_end,
        }
    }

    /// A `Service` over a fresh [`FoldingPlugin`], serving `fold` as the
    /// declared one.
    fn service_over_folding_plugin(fold: AggregationFold) -> (Arc<Service>, Arc<FoldingPlugin>) {
        let plugin = FoldingPlugin::new();
        let service = ServiceFixture::default()
            .with_source(fake_declaration_source_with_fold(fold.as_str()))
            .with_resolver(recording_plugin_resolver())
            .build(
                Arc::clone(&plugin) as Arc<dyn UsageCollectorPluginV1>,
                FOLDING_PLUGIN_SUFFIX,
            );
        (service, plugin)
    }

    /// The one bucket a no-grouping aggregate answers with.
    fn single_bucket(result: &AggregationResult) -> Option<BigDecimal> {
        match result.buckets.as_slice() {
            [bucket] => {
                assert!(bucket.key.is_empty(), "no grouping was requested");
                bucket.value.clone()
            }
            other => panic!("a no-grouping aggregate answers with one bucket, got {other:?}"),
        }
    }

    fn big(literal: &str) -> BigDecimal {
        literal.parse().expect("valid decimal literal")
    }

    async fn aggregate(service: &Service) -> AggregationResult {
        service
            .query_aggregated_usage_records(
                &ctx(),
                meter_id(),
                test_time_range(),
                &ODataQuery::default(),
                &[],
                &[],
            )
            .await
            .expect("the aggregate must reach the plugin and fold")
    }

    #[tokio::test]
    async fn a_withdrawn_pair_folds_to_nothing_while_both_stay_readable() {
        // A fold that admitted the echoed quantity would double-count the
        // very measurement the withdrawal removes. Both entries carry one
        // covered period, so no range selects one of the pair without the
        // other.
        let (svc, plugin) = service_over_folding_plugin(AggregationFold::Sum);
        let withdrawn = measurement("idem-withdrawn", "42.5", 30);
        plugin.store(measurement("idem-survivor", "7.5", 10));
        let invalidation = plugin.store_withdrawn(withdrawn.clone());

        assert_eq!(
            single_bucket(&aggregate(&svc).await),
            Some(big("7.5")),
            "only the entry nothing withdrew may reach the fold",
        );

        // …and the ledger paths return every entry as persisted: leaving a
        // withdrawn pair out of a locally computed fold is the reader's
        // obligation, and the invalidation's reference is what they read.
        let page = svc
            .list_usage_records(
                &ctx(),
                meter_id(),
                test_time_range(),
                &ODataQuery::default(),
                &[],
            )
            .await
            .expect("the raw path must reach the plugin");
        assert_eq!(
            page.items.len(),
            3,
            "the raw path returns every entry as persisted, withdrawn or not",
        );

        for id in [withdrawn.id, invalidation.id] {
            let row = svc
                .get_usage_record(&ctx(), id)
                .await
                .expect("both entries of a withdrawn pair stay readable by id");
            assert_eq!(row.id, id);
        }
        assert_eq!(
            invalidation.invalidation.as_ref().map(|inv| inv.target),
            Some(withdrawn.id),
            "the invalidation names the entry it withdrew, which is what a \
             consumer folding on its own side reads",
        );
    }

    #[tokio::test]
    async fn excluding_only_the_target_double_counts() {
        // A plugin that dropped the withdrawn record but kept the echoed
        // invalidation reports the measurement it was told to remove. Over
        // the pair alone the wrong answer under `SUM` is 42.5 and the right
        // one is an empty selection, so this fails loudly.
        let (svc, plugin) = service_over_folding_plugin(AggregationFold::Sum);
        plugin.store_withdrawn(measurement("idem-only-pair", "42.5", 30));

        assert_eq!(
            single_bucket(&aggregate(&svc).await),
            None,
            "a withdrawn pair leaves nothing to fold; 42.5 would be the \
             echoed quantity counted once more",
        );
    }

    #[tokio::test]
    async fn an_orphan_invalidation_still_contributes_nothing() {
        // An invalidation contributes nothing to a fold whether or not its
        // target is in the selection. Retention is plugin-owned, so a
        // conforming deployment can purge a target and keep the entry that
        // withdrew it — a reachable state, not a malformed ledger. It is also
        // the one input shape that tells the two half-rules apart: over a
        // conforming pair the invalidation is a faithful copy, so "kept the
        // target" and "kept the invalidation" fold to the same number.
        let (svc, plugin) = service_over_folding_plugin(AggregationFold::Sum);
        plugin.store(measurement("idem-survivor", "7.5", 10));
        plugin.store(FoldingPlugin::withdrawal_of(
            &measurement("idem-purged", "42.5", 30),
            RecordOrigin::Live,
        ));

        assert_eq!(
            single_bucket(&aggregate(&svc).await),
            Some(big("7.5")),
            "an orphan invalidation contributes nothing; 42.5 would be the \
             echoed quantity admitted with its target already gone",
        );
    }

    #[tokio::test]
    async fn every_declared_fold_excludes_the_pair() {
        // Withdrawal is the primitive precisely because its meaning does not
        // depend on what a quantity means, so every declared fold must agree.
        //
        // Two pairs are withdrawn, their quantities straddling the survivors'
        // — 99 above and 1 below. With a single withdrawn quantity, either
        // `MAX` or `MIN` would answer the same whether the pair leaked in or
        // not. Both pairs also end later than either survivor, so `LATEST`
        // moves too: admitting them moves every fold.
        //
        // The answers are collected before a single assertion, so a defect
        // reports every fold it moved rather than stopping at the first.
        let mut answers = Vec::new();
        for fold in [
            AggregationFold::Sum,
            AggregationFold::Count,
            AggregationFold::Max,
            AggregationFold::Min,
            AggregationFold::Latest,
        ] {
            let (svc, plugin) = service_over_folding_plugin(fold);
            plugin.store(measurement("idem-low", "7.5", 10));
            plugin.store(measurement("idem-high", "20", 20));
            plugin.store_withdrawn(measurement("idem-big", "99", 30));
            plugin.store_withdrawn(measurement("idem-small", "1", 40));

            answers.push((fold, single_bucket(&aggregate(&svc).await)));
        }

        assert_eq!(
            answers,
            vec![
                (AggregationFold::Sum, Some(big("27.5"))),
                (AggregationFold::Count, Some(big("2"))),
                (AggregationFold::Max, Some(big("20"))),
                (AggregationFold::Min, Some(big("7.5"))),
                (AggregationFold::Latest, Some(big("20"))),
            ],
            "every declared fold must see the two survivors alone",
        );
    }

    // ── The declared LATEST tie-break ──────────────────────────────────────
    //
    // Not withdrawal exclusion; here because this is where the `FoldingPlugin`
    // fixtures live. `fold_over`'s `Latest` arm orders on greatest
    // `window_end`, then `accepted_at`, then `id`, and every case above ties
    // on none of them, so the tie-breaks reached no oracle in this crate. The
    // SDK's `latest-tie-break` check covers a real storage plugin, not this
    // double.
    //
    // Two tests rather than one combined case: a double that got `accepted_at`
    // and `id` wrong in compensating directions would pass a single assertion
    // over a single population.

    /// A pair tying on `window_end` is decided by the greater `accepted_at`,
    /// with the `id` order made to disagree.
    #[tokio::test]
    async fn latest_breaks_a_window_end_tie_on_accepted_at() {
        let (svc, plugin) = service_over_folding_plugin(AggregationFold::Latest);

        // Both entries end the same minute, so `window_end` decides nothing.
        // Which derived id is the greater is a digest's business, so the
        // fixture reads it off and gives the *later* `accepted_at` to the
        // **lesser** id — putting `accepted_at` and `id` in opposition, so a
        // fold that fell straight through to `id` answers the other entry.
        let one = measurement("idem-tie-accepted-one", "0", 30);
        let two = measurement("idem-tie-accepted-two", "0", 30);
        assert_ne!(one.id, two.id, "two idempotency keys derive two ids");
        assert_eq!(
            one.window_end, two.window_end,
            "the pair must tie on window_end or the fold never reaches accepted_at"
        );
        let (lesser, greater) = if one.id < two.id {
            (one, two)
        } else {
            (two, one)
        };

        let lesser_end = lesser.window_end;
        plugin.store(UsageRecord {
            quantity: "11".parse().expect("valid decimal quantity"),
            accepted_at: lesser_end + time::Duration::minutes(5),
            ..lesser
        });
        let greater_end = greater.window_end;
        plugin.store(UsageRecord {
            quantity: "22".parse().expect("valid decimal quantity"),
            accepted_at: greater_end,
            ..greater
        });

        assert_eq!(
            single_bucket(&aggregate(&svc).await),
            Some(big("11")),
            "LATEST takes the greater `accepted_at` over the greater `id`; 22 \
             is what a fold falling from `window_end` straight to `id` answers",
        );
    }

    /// A pair tying on `window_end` **and** `accepted_at` is decided by the
    /// greater `id`, with insertion order made to disagree.
    #[tokio::test]
    async fn latest_breaks_an_accepted_at_tie_on_the_greater_id() {
        let (svc, plugin) = service_over_folding_plugin(AggregationFold::Latest);

        // `measurement` stamps `accepted_at` from `window_end`, so a pair
        // sharing a covered period leaves `id` the only key left to decide
        // with. The greater-`id` entry is stored **first**, against the order
        // a fold that ran out of keys would answer in: `max_by_key` keeps the
        // last of several equal maxima, so dropping `id` picks the second.
        let one = measurement("idem-tie-id-one", "0", 30);
        let two = measurement("idem-tie-id-two", "0", 30);
        assert_ne!(one.id, two.id, "two idempotency keys derive two ids");
        assert_eq!(
            one.window_end, two.window_end,
            "the pair must tie on window_end"
        );
        assert_eq!(
            one.accepted_at, two.accepted_at,
            "the pair must tie on accepted_at too, or `id` never decides"
        );
        let (lesser, greater) = if one.id < two.id {
            (one, two)
        } else {
            (two, one)
        };

        plugin.store(UsageRecord {
            quantity: "33".parse().expect("valid decimal quantity"),
            ..greater
        });
        plugin.store(UsageRecord {
            quantity: "44".parse().expect("valid decimal quantity"),
            ..lesser
        });

        assert_eq!(
            single_bucket(&aggregate(&svc).await),
            Some(big("33")),
            "LATEST takes the greater `id` once both keys above it tie; 44 is \
             what a fold that ran out of keys and fell to insertion order answers",
        );
    }
}

// ── The feed gateway: `Service::read_usage_feed` ───────────────────────────
//
// The pure cursor core (minting, the subscription binding, the page-size
// bounds) is pinned in `feed_tests.rs`. This module pins the *dispatch*: that
// authorization runs ahead of any cursor diagnosis, that each cursor-bearing
// parameter is decoded on its own field name, that the resolved `limit`, the
// subscription and the `until` bound reach the SPI unchanged, and — the rule
// the whole component turns on — that the plugin's continuation is MAPPED
// rather than decided.
mod read_usage_feed_tests {
    use std::sync::Arc;

    use toolkit_gts::gts_id;
    use toolkit_odata::{CursorV1, SortDir, ast};
    use toolkit_security::{
        AccessScope, ScopeConstraint, ScopeFilter, ScopeValue, SecurityContext, pep_properties,
    };
    use usage_collector_sdk::{
        CursorField, FeedPage, FeedPosition, FeedStart, FeedSubscription, IdempotencyKey,
        MeterTypeId, RecordOrigin, ResourceRef, UsageCollectorError, UsageCollectorPluginError,
        UsageCollectorPluginV1, UsageRecord, ValidationReason,
    };
    use uuid::Uuid;

    use crate::domain::Service;
    use crate::domain::authz::{scope_to_odata_filter, usage_record};
    use crate::domain::feed::{
        DEFAULT_FEED_LIMIT, MAX_SUBSCRIPTION_TYPES, mint_cursor, position_from_cursor,
    };
    use crate::domain::test_support::{
        CountingPermitResolver, DenyAllResolver, HappyPathPlugin, RECORDING_PLUGIN_SUFFIX,
        ServiceFixture, authenticated_ctx, fake_declaration_source_with_fold, qty,
        recent_window_end, recent_window_start, recording_plugin_resolver,
    };

    const GTS_ID: &str = gts_id!("cf.core.uc.usage_record.v1~cf.mini_chat._.tokens_consumed.v1~");
    const OTHER_GTS_ID: &str = gts_id!("cf.core.uc.usage_record.v1~cf.mini_chat._.seats_used.v1~");

    fn ctx() -> SecurityContext {
        authenticated_ctx()
    }

    /// More than one type, so a subscription that reached the SPI truncated —
    /// or one the gateway rebuilt from somewhere other than its argument — is
    /// visible rather than coincidentally right.
    fn subscription() -> FeedSubscription {
        FeedSubscription::new([
            MeterTypeId::new(GTS_ID).expect("valid gts_type_id"),
            MeterTypeId::new(OTHER_GTS_ID).expect("valid gts_type_id"),
        ])
        .expect("a two-type subscription is non-empty")
    }

    fn position(bytes: &[u8]) -> FeedPosition {
        FeedPosition::new(bytes.to_vec()).expect("a non-empty position is valid")
    }

    /// A settled entry, distinguishable from every other `feed_entry(n)`.
    /// Several fields vary: an entry pass-through that dropped, duplicated or
    /// reordered the page leaves a `Vec` of the right length and the right
    /// *kind* of contents, which a fixture of identical rows cannot see.
    fn feed_entry(n: u8) -> usage_collector_sdk::StoredUsageRecord {
        UsageRecord {
            id: Uuid::from_u128(u128::from(n)),
            gts_type_id: MeterTypeId::new(GTS_ID).expect("valid gts_type_id"),
            tenant_id: Uuid::from_u128(2),
            resource_ref: ResourceRef::new(format!("rsc-{n}"), "compute.vm")
                .expect("valid resource ref"),
            subject_ref: None,
            metadata: std::collections::BTreeMap::new(),
            quantity: qty(&n.to_string()),
            idempotency_key: IdempotencyKey::new(format!("idem-{n}"))
                .expect("valid idempotency key"),
            accepted_at: recent_window_end(),
            origin: RecordOrigin::Live,
            invalidation: None,
            window_start: recent_window_start(),
            window_end: recent_window_end(),
        }
        // A plugin's page names its meter by reference and the gateway
        // re-attaches the identifier from the subscription it resolved, so the
        // reference must be what this suite's declaration double resolves.
        .into_stored(crate::domain::test_support::declared_uuid(
            &MeterTypeId::new(GTS_ID).expect("valid gts_type_id"),
        ))
    }

    /// [`feed_entry`] under a caller-chosen meter — the only departure, and
    /// the whole of what the per-entry mapping test reads: this is the case
    /// that can tell a per-entry lookup from a per-page one.
    fn feed_entry_on(meter_id: &str, n: u8) -> usage_collector_sdk::StoredUsageRecord {
        let meter = MeterTypeId::new(meter_id).expect("valid gts_type_id");
        UsageRecord {
            gts_type_id: meter.clone(),
            ..feed_entry(n).into_usage_record(meter.clone())
        }
        .into_stored(crate::domain::test_support::declared_uuid(&meter))
    }

    /// `served` as the caller sees it: the gateway re-attaches the
    /// subscription's identifier to every entry a plugin answers with, so a
    /// test comparing a served page against what it programmed has to
    /// compare against the re-attached shape rather than the stored one.
    fn reattached(served: &[usage_collector_sdk::StoredUsageRecord]) -> Vec<UsageRecord> {
        let meter = MeterTypeId::new(GTS_ID).expect("valid gts_type_id");
        served
            .iter()
            .map(|entry| entry.clone().into_usage_record(meter.clone()))
            .collect()
    }

    /// A feed cursor bound to [`subscription`], carrying `bytes`.
    fn cursor_at(bytes: &[u8]) -> CursorV1 {
        mint_cursor(&position(bytes), &subscription())
    }

    /// A well-formed `CursorV1` that is not a feed cursor: the raw read
    /// path's shape, which `position_from_cursor` refuses on its sentinel. It
    /// decodes, so a gateway that examined it would have something to say.
    fn foreign_cursor() -> CursorV1 {
        CursorV1 {
            k: vec!["2026-01-01T00:00:00Z".to_owned()],
            o: SortDir::Asc,
            s: "+window_end,+id".to_owned(),
            f: Some("deadbeef".to_owned()),
            d: "fwd".to_owned(),
        }
    }

    /// A `Service` over a [`HappyPathPlugin`] whose feed slot is programmed
    /// with `outcome`, wired against the read-path PDP fake.
    ///
    /// `with_source` gives [`subscription`]'s meters a resolvable
    /// declaration: the feed read path resolves every subscribed meter to its
    /// reference before dispatch, so a fixture without a working Type
    /// Resolver backend would fail closed before reaching `plugin`.
    fn service_with(
        outcome: Result<
            FeedPage<FeedPosition, usage_collector_sdk::StoredUsageRecord>,
            UsageCollectorPluginError,
        >,
    ) -> (Arc<Service>, Arc<HappyPathPlugin>) {
        let plugin = HappyPathPlugin::new();
        match outcome {
            Ok(page) => plugin.set_read_feed_page(page),
            Err(err) => plugin.set_read_feed_page_err(err),
        }
        let service = ServiceFixture::default()
            .with_source(fake_declaration_source_with_fold("SUM"))
            .with_resolver(recording_plugin_resolver())
            .build(
                Arc::clone(&plugin) as Arc<dyn UsageCollectorPluginV1>,
                RECORDING_PLUGIN_SUFFIX,
            );
        (service, plugin)
    }

    /// The same, with a PDP that denies every evaluation. The feed slot is
    /// left unprogrammed on purpose: reaching it at all is a failure.
    fn denied_service() -> (Arc<Service>, Arc<HappyPathPlugin>) {
        let plugin = HappyPathPlugin::new();
        let service = ServiceFixture::default()
            .with_resolver(Arc::new(DenyAllResolver))
            .build(
                Arc::clone(&plugin) as Arc<dyn UsageCollectorPluginV1>,
                RECORDING_PLUGIN_SUFFIX,
            );
        (service, plugin)
    }

    /// Each entry is re-attached with the identifier of **its own** meter,
    /// not with one meter's for the whole page.
    ///
    /// A page spans the whole subscription, so it carries entries of every
    /// subscribed meter, and the gateway looks each entry's reference up in
    /// the map it built. Every other feed test programs entries under one
    /// meter, so an implementation that checked membership and then stapled
    /// `meters[0].id` onto every entry passes all of them; this is the test
    /// that fails against it.
    ///
    /// The entries are under *different* meters and asserted per entry:
    /// whichever `FeedSubscription`'s sorted order puts first, one entry is
    /// not under `meters[0]`, so the defect moves its identifier whatever the
    /// sort does.
    #[tokio::test]
    async fn each_feed_entry_is_named_by_the_meter_its_own_reference_maps_to() {
        let first = feed_entry_on(GTS_ID, 1);
        let second = feed_entry_on(OTHER_GTS_ID, 2);
        assert_ne!(
            first.gts_type_uuid, second.gts_type_uuid,
            "test setup: the two entries must carry different references, or \
             the mapping has nothing to tell apart",
        );
        let (svc, _plugin) = service_with(Ok(FeedPage {
            entries: vec![first.clone(), second.clone()],
            next: Some(position(&[1])),
        }));

        let page = svc
            .read_usage_feed(&ctx(), &subscription(), FeedStart::Oldest, None, Some(50))
            .await
            .expect("a page over a two-meter subscription is served");

        let named: Vec<&str> = page
            .entries
            .iter()
            .map(|entry| entry.gts_type_id.as_str())
            .collect();
        assert_eq!(
            named,
            vec![GTS_ID, OTHER_GTS_ID],
            "each entry must be named by the meter its own reference maps to, \
             in the order the plugin settled them; a gateway that stapled one \
             meter's identifier onto the whole page names both entries the same",
        );
    }

    /// Fails if the gateway ever synthesises a `None` continuation: on a live
    /// read that tells a consumer a stream it is still following has ended,
    /// and nothing downstream would notice. A next cursor comes back with
    /// every page of a live read, short pages included.
    ///
    /// Also pins what the gateway handed the SPI: the whole subscription,
    /// `Oldest` as `Oldest`, no `until`, the resolved default page size, and
    /// the compiled PDP scope the page is filtered by. None of them is
    /// visible in the page that comes back, and each is something a gateway
    /// can drop while still answering `200`.
    #[tokio::test]
    async fn an_empty_live_page_keeps_its_continuation() {
        let (svc, plugin) = service_with(Ok(FeedPage {
            entries: vec![],
            next: Some(position(&[7])),
        }));

        let page = svc
            .read_usage_feed(&ctx(), &subscription(), FeedStart::Oldest, None, None)
            .await
            .expect("an empty live page is served");

        assert!(page.entries.is_empty());
        let next = page.next.expect(
            "an empty live page still carries its cursor; returning None here would report a \
             live stream as ended",
        );
        // Not merely `is_some()`: a cursor minted over another subscription,
        // or without the binding, is a token the next request refuses.
        // Recovering the plugin's own position through the subscription-bound
        // decode says the gateway minted the continuation the plugin issued.
        let recovered = position_from_cursor(&next, &subscription(), CursorField::Cursor)
            .expect("the minted continuation must decode against the same subscription");
        assert_eq!(recovered.as_bytes(), position(&[7]).as_bytes());

        let dispatch = plugin
            .last_read_feed_page_input()
            .expect("the feed page must have been dispatched");
        let dispatched_ids: Vec<MeterTypeId> = dispatch
            .types
            .iter()
            .map(|meter| meter.id.clone())
            .collect();
        assert_eq!(
            dispatched_ids,
            subscription().types(),
            "the whole subscription must reach the SPI; a truncated one silently drops a type \
             the consumer is still reading",
        );
        assert_eq!(dispatch.start, FeedStart::Oldest);
        assert!(dispatch.until.is_none());
        assert_eq!(
            dispatch.limit, DEFAULT_FEED_LIMIT,
            "a caller naming no limit must reach the SPI with the resolved default, never a \
             bound the plugin would refuse",
        );
        // The authorization decision itself, and the one dispatch argument no
        // other test here reads. `HappyPathPlugin` replays its programmed page
        // whatever scope it is handed, so passing a constant-true expression
        // leaves every other assertion green while serving a cross-tenant
        // page. Compared against `scope_to_odata_filter` of the fixture
        // resolver's own permit rather than a hand-written `Expr`: restating
        // the projection's output would pin the shape, not the decision.
        //
        // Compared through `Debug` because `ast::Expr` derives no `PartialEq`.
        // That is sound here: both sides come out of the same projection
        // function, so a rendering change moves them together.
        assert_eq!(
            format!("{:?}", dispatch.scope),
            format!("{:?}", expected_feed_scope()),
            "the page must be filtered by the scope this caller's permit compiled to",
        );
    }

    /// The `ast::Expr` `recording_plugin_resolver`'s permit projects to. That
    /// resolver answers one `OWNER_TENANT_ID` constraint whatever the request,
    /// and `scope_to_odata_filter` is the same projection
    /// `authorize_read_usage_feed` runs.
    fn expected_feed_scope() -> ast::Expr {
        scope_to_odata_filter(&AccessScope::single(ScopeConstraint::new(vec![
            ScopeFilter::eq(
                pep_properties::OWNER_TENANT_ID,
                ScopeValue::String(Uuid::from_u128(2).to_string()),
            ),
        ])))
        .expect("the fixture's tenant-pinned permit projects")
    }

    /// Fails if a live page the plugin served with no continuation is refused,
    /// or repaired, rather than served as it stands.
    ///
    /// The obligation to carry a next cursor is the plugin's, and this gateway
    /// diagnoses a violation rather than deciding `next` for it: the entries
    /// on such a page are correct and final, so refusing would deny a charging
    /// consumer rows it is entitled to over the plugin's bookkeeping, and
    /// repairing is worse still since the gateway holds no position to mint
    /// one from. The `tracing::error!` emitted alongside is not asserted —
    /// nothing here captures it — but the disposition is.
    #[tokio::test]
    async fn a_live_page_without_a_continuation_is_served_not_refused() {
        let served = vec![feed_entry(3)];
        let (svc, _plugin) = service_with(Ok(FeedPage {
            entries: served.clone(),
            next: None,
        }));

        let page = svc
            .read_usage_feed(&ctx(), &subscription(), FeedStart::Oldest, None, None)
            .await
            .expect("a non-conforming live page is diagnosed, not refused");

        assert_eq!(
            page.entries,
            reattached(&served),
            "the entries must reach the caller unchanged, bar the identifier the gateway \
             staples back on; the plugin's defect is in the continuation, not in the rows",
        );
        assert!(
            page.next.is_none(),
            "the gateway must not invent a continuation it holds no position for",
        );
    }

    /// Fails if a page longer than the limit it was dispatched under is
    /// handed to a caller.
    ///
    /// `FeedPage.entries` carries a published `maxItems` and the resolved
    /// `limit` is at most that, so a page over the limit is over the schema
    /// too — the feed's half of the aggregate path's over-cap refusal.
    ///
    /// Discriminating on the variant *and* the detail: `Internal` is also what
    /// an unrecognised `FeedStart` raises, so `is_err()` alone would pass
    /// under a gateway that tripped on something else.
    #[tokio::test]
    async fn a_page_longer_than_its_limit_is_refused_not_served() {
        let over_limit = vec![feed_entry(1), feed_entry(2), feed_entry(3)];
        let (svc, _plugin) = service_with(Ok(FeedPage {
            entries: over_limit,
            next: Some(position(&[5])),
        }));

        let err = svc
            .read_usage_feed(&ctx(), &subscription(), FeedStart::Oldest, None, Some(2))
            .await
            .expect_err("a page longer than its limit is not served");

        match err {
            UsageCollectorError::Internal { detail } => {
                assert!(
                    detail.contains("3 feed entries") && detail.contains("limit of 2"),
                    "the refusal must name both figures so an operator can attribute it, \
                     got: {detail}"
                );
            }
            other => panic!("expected Internal naming the over-long page, got {other:?}"),
        }
    }

    /// Fails if an in-process caller can hand the plugin a subscription
    /// wider than the published ceiling.
    ///
    /// `feed::build_subscription` enforces that ceiling on the repeated query
    /// parameter and its only call site is the REST handler;
    /// `FeedSubscription::new` enforces only non-emptiness. So this is
    /// reachable through the in-process client, and it matters here because
    /// the storage plugin shapes its feed page statement as one lateral join
    /// per subscribed type.
    ///
    /// Asserted on `field`, not merely the variant: this path's other
    /// `InvalidArgument`s name `limit` and `cursor`.
    #[tokio::test]
    async fn an_in_process_subscription_over_the_ceiling_is_refused_before_dispatch() {
        let (svc, plugin) = service_with(Ok(FeedPage {
            entries: vec![],
            next: None,
        }));
        let wide = FeedSubscription::new((0..=MAX_SUBSCRIPTION_TYPES).map(|i| {
            MeterTypeId::new(format!(
                "gts.cf.core.uc.usage_record.v1~cf.uc_wide._.meter{i}.v1~"
            ))
            .expect("valid gts_type_id")
        }))
        .expect("a wide subscription is still non-empty");

        let err = svc
            .read_usage_feed(&ctx(), &wide, FeedStart::Oldest, None, None)
            .await
            .expect_err("a subscription past the published ceiling is refused");

        match err {
            UsageCollectorError::InvalidArgument { field, .. } => {
                assert_eq!(field, "gts_type_id");
            }
            other => panic!("expected InvalidArgument on `gts_type_id`, got {other:?}"),
        }
        assert!(
            plugin.last_read_feed_page_input().is_none(),
            "an over-wide subscription must never reach the SPI",
        );
    }

    /// Fails if the feed serves a read under a PDP permit whose constraint
    /// this gear cannot project.
    ///
    /// The fail-closed posture [`scope_to_odata_filter`] gives
    /// `authorize_read_usage_feed`, driven on the one permit shape that
    /// reaches that gate: a permit carrying **no** constraints fails closed
    /// upstream in `PolicyEnforcer` and says nothing about this gear's
    /// projection, while a non-empty constraint that does not pin
    /// `owner_tenant_id` compiles fine and is then denied there.
    ///
    /// Deleting the `scope_to_odata_filter` call from
    /// `authorize_read_usage_feed` reds this.
    #[tokio::test]
    async fn a_permit_without_tenant_narrowing_denies_the_feed_before_dispatch() {
        let plugin = HappyPathPlugin::new();
        let service = ServiceFixture::default()
            .with_resolver(CountingPermitResolver::new(
                usage_record::PROP_RESOURCE_TYPE,
                "compute.vm".to_owned(),
            ))
            .build(
                Arc::clone(&plugin) as Arc<dyn UsageCollectorPluginV1>,
                RECORDING_PLUGIN_SUFFIX,
            );

        let err = service
            .read_usage_feed(&ctx(), &subscription(), FeedStart::Oldest, None, None)
            .await
            .expect_err("a permit this gear cannot project must not serve a page");

        assert!(
            matches!(err, UsageCollectorError::PermissionDenied { .. }),
            "expected PermissionDenied from the projection gate; got {err:?}"
        );
        assert!(
            plugin.last_read_feed_page_input().is_none(),
            "a read that failed closed must never reach the SPI",
        );
    }

    /// Fails if a **short** live page loses its continuation, and if the
    /// entries the plugin served do not reach the caller intact.
    ///
    /// The neighbouring tests use empty pages, so a gateway that inferred
    /// "fewer entries than the limit ⇒ caught up ⇒ no continuation" would pass
    /// both. A next cursor comes back with every page of a live read, short
    /// pages included; a `None` here tells a consumer a stream it is still
    /// following has ended.
    ///
    /// It carries the entry-payload assertion too, because nothing else here
    /// reads `page.entries`: every other test programs an empty page, and the
    /// metrics suite observes the page *size* rather than its contents.
    #[tokio::test]
    async fn a_short_live_page_keeps_its_continuation_and_its_entries() {
        let served = vec![feed_entry(1), feed_entry(2)];
        let (svc, plugin) = service_with(Ok(FeedPage {
            entries: served.clone(),
            next: Some(position(&[9])),
        }));

        let page = svc
            .read_usage_feed(&ctx(), &subscription(), FeedStart::Oldest, None, Some(50))
            .await
            .expect("a short live page is served");

        assert_eq!(
            page.entries,
            reattached(&served),
            "the entries the plugin settled must reach the caller unchanged and in order, bar \
             the identifier the gateway staples back on",
        );
        assert!(
            page.next.is_some(),
            "a short live page still carries its cursor; returning None here would report a \
             live stream as ended on the very page a backlog drains to",
        );

        // The premise asserted rather than assumed: this page is short of the
        // limit it was read under. Without it "short" would be a property of
        // the fixture's prose rather than of the run.
        let dispatch = plugin
            .last_read_feed_page_input()
            .expect("the feed page must have been dispatched");
        assert_eq!(dispatch.limit, 50);
        assert!(
            page.entries.len() < usize::try_from(dispatch.limit).expect("a limit fits usize"),
            "the fixture must serve fewer entries than the limit, or this is not a short page",
        );
    }

    /// Fails if a `start` cursor defect is reported on any field but `cursor`;
    /// [`an_unusable_until_is_refused_on_its_own_field`] covers the other
    /// half, and neither implies the other. `position_from_cursor`'s `field`
    /// argument decides nothing except which error is produced, so the call
    /// sites can drift independently.
    ///
    /// Against a **permitted** service: a denying one short-circuits at the
    /// PDP and never reaches the decode, which is what
    /// [`a_denied_caller_is_denied_before_the_cursor_is_examined`] pins.
    #[tokio::test]
    async fn an_unusable_start_cursor_is_refused_on_the_cursor_field() {
        let foreign = foreign_cursor();
        let (svc, plugin) = service_with(Ok(FeedPage {
            entries: vec![],
            next: None,
        }));

        let err = svc
            .read_usage_feed(
                &ctx(),
                &subscription(),
                FeedStart::After(&foreign),
                None,
                None,
            )
            .await
            .expect_err("a `start` cursor that is not a feed cursor is refused");

        match err {
            UsageCollectorError::CursorRejected { field, .. } => {
                assert_eq!(field, CursorField::Cursor);
            }
            other => panic!("expected CursorRejected on `cursor`, got {other:?}"),
        }
        assert!(
            plugin.last_read_feed_page_input().is_none(),
            "an unusable resume cursor must be refused before dispatch",
        );
    }

    /// Fails if a backward feed cursor reaches the SPI.
    ///
    /// The feed's counterpart to the raw path's
    /// `a_backward_cursor_never_reaches_the_plugin`, asserted at the service
    /// rather than only at `position_from_cursor`: the guard is worth nothing
    /// if the read path around it dispatches anyway.
    ///
    /// The fixture is [`cursor_at`]'s minted cursor with `d` flipped, so
    /// sentinel, fingerprint and position are what this gear itself mints and
    /// direction is the only thing wrong with the token.
    #[tokio::test]
    async fn a_backward_feed_cursor_never_reaches_the_plugin() {
        let mut backward = cursor_at(&[1]);
        // The one thing wrong with this token.
        backward.d = "bwd".to_owned();
        let (svc, plugin) = service_with(Ok(FeedPage {
            entries: vec![],
            next: None,
        }));

        let err = svc
            .read_usage_feed(
                &ctx(),
                &subscription(),
                FeedStart::After(&backward),
                None,
                None,
            )
            .await
            .expect_err("a backward cursor must be refused: the feed reads forward only");

        match err {
            UsageCollectorError::CursorRejected { source, field, .. } => {
                assert!(
                    matches!(source, toolkit_odata::Error::InvalidCursor),
                    "expected upstream's InvalidCursor, got {source:?}",
                );
                assert_eq!(field, CursorField::Cursor);
            }
            other => panic!("expected a cursor rejection, got {other:?}"),
        }
        assert!(
            plugin.last_read_feed_page_input().is_none(),
            "a refused continuation MUST NOT reach the plugin",
        );
    }

    /// Fails if a backward `until` bound reaches the SPI.
    ///
    /// The `until` half of the pair above. `read_usage_feed` decodes each
    /// cursor-bearing parameter at its own call site, so a guard reached from
    /// one is not reached from the other, and the field the refusal names is
    /// what tells a consumer which token to drop.
    #[tokio::test]
    async fn a_backward_until_bound_never_reaches_the_plugin() {
        let mut backward = cursor_at(&[9]);
        backward.d = "bwd".to_owned();
        let (svc, plugin) = service_with(Ok(FeedPage {
            entries: vec![],
            next: None,
        }));

        let err = svc
            .read_usage_feed(
                &ctx(),
                &subscription(),
                FeedStart::Oldest,
                Some(&backward),
                None,
            )
            .await
            .expect_err("a backward `until` bound must be refused");

        match err {
            UsageCollectorError::CursorRejected { field, .. } => {
                assert_eq!(field, CursorField::Until);
            }
            other => panic!("expected CursorRejected on `until`, got {other:?}"),
        }
        assert!(
            plugin.last_read_feed_page_input().is_none(),
            "a refused bound MUST NOT reach the plugin",
        );
    }

    /// Fails if the gateway synthesises a `Some` when the plugin said the
    /// bounded replay is done.
    ///
    /// The mirror of the test above, and neither stands alone: a gateway
    /// hard-coding `None` passes this one and fails that one, and a gateway
    /// hard-coding `Some` fails this one. Only the pair pins the `map`.
    #[tokio::test]
    async fn a_bounded_replay_at_its_until_returns_no_continuation() {
        let until = cursor_at(&[9]);
        let (svc, _plugin) = service_with(Ok(FeedPage {
            entries: vec![],
            next: None,
        }));

        let page = svc
            .read_usage_feed(
                &ctx(),
                &subscription(),
                FeedStart::Oldest,
                Some(&until),
                None,
            )
            .await
            .expect("a completed bounded replay is served");

        assert!(
            page.next.is_none(),
            "a completed bounded replay carries no continuation",
        );
    }

    /// Fails if a malformed cursor is diagnosed before the caller is
    /// authorized, which would tell an unauthorized caller that their token
    /// was well-formed — and, for a token that *is* well-formed, whether it
    /// names a subscription they may not read.
    ///
    /// Discriminating on the variant: a gateway that decoded first would
    /// answer `CursorRejected` for this input, because the cursor supplied
    /// is not a feed cursor. Both orderings produce an `Err`; only one
    /// produces this variant.
    #[tokio::test]
    async fn a_denied_caller_is_denied_before_the_cursor_is_examined() {
        let (svc, plugin) = denied_service();
        let foreign = foreign_cursor();

        let err = svc
            .read_usage_feed(
                &ctx(),
                &subscription(),
                FeedStart::After(&foreign),
                None,
                None,
            )
            .await
            .expect_err("a denied caller is denied");

        assert!(
            matches!(err, UsageCollectorError::PermissionDenied { .. }),
            "expected PermissionDenied before any cursor diagnosis; got {err:?}"
        );
        assert!(
            plugin.last_read_feed_page_input().is_none(),
            "a denied read must never reach the SPI",
        );
    }

    /// Fails if the gateway invents an ordering comparison between `until`
    /// and the resume cursor. Positions are opaque bytes here, so the
    /// gateway has no basis for one; the plugin answers an empty page.
    ///
    /// The `Ok` alone would not catch a gateway that silently *dropped*
    /// `until` instead of comparing it — an unbounded read answering `200` —
    /// so the recorded dispatch is asserted: both bounds must arrive at the
    /// SPI as the caller gave them.
    #[tokio::test]
    async fn an_until_behind_the_cursor_is_passed_through_not_compared() {
        let later = cursor_at(&[9]);
        let earlier = cursor_at(&[1]);
        let (svc, plugin) = service_with(Ok(FeedPage {
            entries: vec![],
            next: None,
        }));

        let page = svc
            .read_usage_feed(
                &ctx(),
                &subscription(),
                FeedStart::After(&later),
                Some(&earlier),
                None,
            )
            .await
            .expect("the gateway does not refuse this; the plugin answers it");

        assert!(page.entries.is_empty());
        let dispatch = plugin
            .last_read_feed_page_input()
            .expect("the read must have reached the SPI rather than being refused here");
        assert_eq!(dispatch.start, FeedStart::After(position(&[9])));
        assert_eq!(
            dispatch.until.map(|p| p.as_bytes().to_vec()),
            Some(vec![1]),
            "the `until` bound must reach the plugin exactly as given, behind the cursor or not",
        );
    }

    /// Fails if the plugin's retention refusal does not reach the caller as
    /// the documented 400. The lift lives in `domain/error.rs`; this pins
    /// that the feed path reaches it.
    ///
    /// Discriminating on the typed reason, not the variant: the feed path
    /// raises `InvalidArgument` for an out-of-bound `limit` too.
    #[tokio::test]
    async fn a_plugin_retention_refusal_surfaces_as_cursor_beyond_retention() {
        let resume = cursor_at(&[4]);
        let (svc, _plugin) = service_with(Err(UsageCollectorPluginError::CursorBeyondRetention));

        let err = svc
            .read_usage_feed(
                &ctx(),
                &subscription(),
                FeedStart::After(&resume),
                None,
                None,
            )
            .await
            .expect_err("a refused cursor is an error");

        match err {
            UsageCollectorError::InvalidArgument { field, reason, .. } => {
                assert_eq!(field, "cursor");
                assert_eq!(reason, ValidationReason::CursorBeyondRetention);
            }
            other => panic!("expected InvalidArgument/CursorBeyondRetention, got {other:?}"),
        }
    }

    /// Fails if the `until` bound is refused under the `cursor` parameter's
    /// name: a consumer fixing a stale token needs to be told which one.
    ///
    /// Discriminating on `field`, since every guard on the decode path raises
    /// the same `CursorRejected` variant — a variant-only assertion would pass
    /// with the `until` half decoded under `CursorField::Cursor`.
    #[tokio::test]
    async fn an_unusable_until_is_refused_on_its_own_field() {
        let foreign = foreign_cursor();
        let (svc, plugin) = service_with(Ok(FeedPage {
            entries: vec![],
            next: None,
        }));

        let err = svc
            .read_usage_feed(
                &ctx(),
                &subscription(),
                FeedStart::Oldest,
                Some(&foreign),
                None,
            )
            .await
            .expect_err("an `until` that is not a feed cursor is refused");

        match err {
            UsageCollectorError::CursorRejected { field, .. } => {
                assert_eq!(field, CursorField::Until);
            }
            other => panic!("expected CursorRejected on `until`, got {other:?}"),
        }
        assert!(
            plugin.last_read_feed_page_input().is_none(),
            "an unusable bound must be refused before dispatch",
        );
    }

    /// Fails if a page size outside the published bound reaches the plugin,
    /// which documents exactly that as a host-contract breach it answers
    /// with `Internal` — turning a caller's 400 into a 500.
    ///
    /// Discriminating on `field`: this path's other `InvalidArgument` is the
    /// retention refusal, which names `cursor`.
    #[tokio::test]
    async fn a_limit_outside_the_published_bound_is_refused_before_dispatch() {
        let (svc, plugin) = service_with(Ok(FeedPage {
            entries: vec![],
            next: None,
        }));

        let err = svc
            .read_usage_feed(&ctx(), &subscription(), FeedStart::Oldest, None, Some(0))
            .await
            .expect_err("a zero-entry page is refused");

        match err {
            UsageCollectorError::InvalidArgument { field, .. } => assert_eq!(field, "limit"),
            other => panic!("expected InvalidArgument on `limit`, got {other:?}"),
        }
        assert!(
            plugin.last_read_feed_page_input().is_none(),
            "an out-of-bound limit must never reach the SPI",
        );
    }
}

/// `Service::get_reconciliation_metadata` — the Query Gateway's
/// operator-facing read path.
mod get_reconciliation_metadata_tests {
    use std::sync::Arc;

    use toolkit_gts::gts_id;
    use toolkit_security::SecurityContext;
    use usage_collector_sdk::{
        AggregationFold, MeterTypeId, TimeRange, UsageCollectorError, UsageCollectorPluginV1,
    };
    use uuid::Uuid;

    use crate::domain::Service;
    use crate::domain::ports::declarations::DeclarationSource;
    use crate::domain::test_support::{
        DenyOneSubjectResolver, RecordingReconciliationPlugin, ServiceFixture,
        fake_declaration_source_not_found, fake_declaration_source_with_fold, test_time_range,
    };

    const GTS_ID: &str = gts_id!("cf.core.uc.usage_record.v1~cf.mini_chat._.tokens_consumed.v1~");

    /// Registration suffix this module's `Service` fixtures build under.
    const SUFFIX: &str = "test.usage_collector.reconciliation.plugin.v1";

    /// The subject [`DenyOneSubjectResolver`] permits (with an
    /// `OWNER_TENANT_ID` grant scoped to this subject's own tenant — see
    /// [`service_with`]).
    const PERMIT_SUBJECT: Uuid = Uuid::from_u128(1);
    /// A second permitted subject naming a different home tenant, for
    /// [`the_compiled_pdp_scope_reaches_the_plugin`]: two permitted callers
    /// whose compiled scopes must differ prove the scope the plugin receives
    /// is tied to the caller rather than a shared placeholder.
    const OTHER_PERMIT_SUBJECT: Uuid = Uuid::from_u128(3);
    /// The subject [`DenyOneSubjectResolver`] denies.
    const DENY_SUBJECT: Uuid = Uuid::from_u128(0xDEAD);

    /// [`PERMIT_SUBJECT`]'s home tenant.
    const PERMIT_TENANT: Uuid = Uuid::from_u128(2);
    /// [`OTHER_PERMIT_SUBJECT`]'s home tenant — distinct from
    /// [`PERMIT_TENANT`].
    const OTHER_PERMIT_TENANT: Uuid = Uuid::from_u128(4);

    /// The tenant scope parameter every fixture here passes to
    /// `get_reconciliation_metadata`. Deliberately distinct from either
    /// `SecurityContext`'s own tenant: it is dispatched straight to the plugin
    /// rather than read by the PDP request, so conflating the two in a fixture
    /// would hide them being mixed up in the code.
    const FIXTURE_TENANT: Uuid = Uuid::from_u128(0x7E57);

    fn fixture_meter() -> MeterTypeId {
        MeterTypeId::new(GTS_ID).expect("valid gts_type_id")
    }

    fn fixture_range() -> TimeRange {
        test_time_range()
    }

    /// A permitted caller. [`DenyOneSubjectResolver`] permits every subject
    /// but [`DENY_SUBJECT`], scoping the grant to this context's own
    /// [`PERMIT_TENANT`].
    fn permit_ctx() -> SecurityContext {
        SecurityContext::builder()
            .subject_id(PERMIT_SUBJECT)
            .subject_tenant_id(PERMIT_TENANT)
            .subject_type("user")
            .build()
            .expect("authenticated context")
    }

    /// A second permitted caller, naming [`OTHER_PERMIT_TENANT`] rather than
    /// [`permit_ctx`]'s, so the scope the plugin receives can be shown to
    /// track the calling context.
    fn other_permit_ctx() -> SecurityContext {
        SecurityContext::builder()
            .subject_id(OTHER_PERMIT_SUBJECT)
            .subject_tenant_id(OTHER_PERMIT_TENANT)
            .subject_type("user")
            .build()
            .expect("authenticated context")
    }

    /// A denied caller — same shape as [`permit_ctx`], differing only in the
    /// subject [`DenyOneSubjectResolver`] is built to deny.
    fn deny_ctx() -> SecurityContext {
        SecurityContext::builder()
            .subject_id(DENY_SUBJECT)
            .subject_tenant_id(PERMIT_TENANT)
            .subject_type("user")
            .build()
            .expect("authenticated context")
    }

    /// A `DeclarationSource` resolving the queried meter to a declaration
    /// naming `fold`.
    fn declaration_with_fold(fold: AggregationFold) -> Arc<dyn DeclarationSource> {
        fake_declaration_source_with_fold(fold.as_str())
    }

    /// A `DeclarationSource` that never resolves any meter.
    fn no_such_declaration() -> Arc<dyn DeclarationSource> {
        fake_declaration_source_not_found()
    }

    /// A `Service` over `plugin`, resolving every meter through
    /// `declaration`, wired against [`DenyOneSubjectResolver`] so
    /// [`permit_ctx`] and [`deny_ctx`] reach opposite PDP decisions through
    /// one fixture.
    fn service_with(
        plugin: Arc<dyn UsageCollectorPluginV1>,
        declaration: Arc<dyn DeclarationSource>,
    ) -> Arc<Service> {
        ServiceFixture::default()
            .with_source(declaration)
            .with_resolver(DenyOneSubjectResolver::new(DENY_SUBJECT))
            .build(plugin, SUFFIX)
    }

    #[tokio::test]
    async fn a_reconciliation_read_resolves_the_declaration_and_passes_its_fold_to_the_plugin() {
        // The gear resolves the queried type and takes the declared fold,
        // which reaches the plugin as a parameter. A gear that passed a
        // default would report the wrong branch for every gauge meter.
        let plugin = RecordingReconciliationPlugin::answering_for(AggregationFold::Max);
        let service = service_with(plugin.clone(), declaration_with_fold(AggregationFold::Max));
        service
            .get_reconciliation_metadata(
                &permit_ctx(),
                FIXTURE_TENANT,
                fixture_meter(),
                fixture_range(),
            )
            .await
            .expect("a permitted read over a resolvable meter must succeed");
        assert_eq!(plugin.last_fold(), Some(AggregationFold::Max));
    }

    #[tokio::test]
    async fn the_compiled_pdp_scope_reaches_the_plugin() {
        // The compiled scope carries the authorization decision and is the
        // argument a double is likeliest to discard. One caller's scope
        // "looking tenant-shaped" rules nothing out, since a fixed
        // placeholder every permit shares would look the same. So two
        // permitted callers name two different tenants
        // (`DenyOneSubjectResolver` compiles each grant from its own caller's
        // subject tenant), and both that each scope names *that* caller's
        // tenant and that the two disagree are required.
        let plugin = RecordingReconciliationPlugin::answering_for(AggregationFold::Sum);
        let service = service_with(plugin.clone(), declaration_with_fold(AggregationFold::Sum));

        service
            .get_reconciliation_metadata(
                &permit_ctx(),
                FIXTURE_TENANT,
                fixture_meter(),
                fixture_range(),
            )
            .await
            .expect("a permitted read must succeed");
        let scope_for_permit_ctx = plugin
            .last_scope()
            .expect("the plugin must receive a scope");
        assert!(
            format!("{scope_for_permit_ctx:?}").contains(&PERMIT_TENANT.to_string()),
            "the plugin must receive the PDP-compiled scope naming THIS caller's own tenant \
             ({PERMIT_TENANT}), not a placeholder: {scope_for_permit_ctx:?}"
        );

        service
            .get_reconciliation_metadata(
                &other_permit_ctx(),
                FIXTURE_TENANT,
                fixture_meter(),
                fixture_range(),
            )
            .await
            .expect("a second permitted read, under a different tenant, must also succeed");
        let scope_for_other_ctx = plugin
            .last_scope()
            .expect("the plugin must receive a scope");
        assert!(
            format!("{scope_for_other_ctx:?}").contains(&OTHER_PERMIT_TENANT.to_string()),
            "the plugin must receive the PDP-compiled scope naming THIS caller's own tenant \
             ({OTHER_PERMIT_TENANT}), not a placeholder: {scope_for_other_ctx:?}"
        );

        // `ast::Expr` carries no `PartialEq`, so the compiled scopes are
        // compared by the `Debug` rendering the assertions above inspect.
        assert_ne!(
            format!("{scope_for_permit_ctx:?}"),
            format!("{scope_for_other_ctx:?}"),
            "two permitted calls naming different tenants must reach the plugin with two \
             different compiled scopes; a fixed placeholder scope shared by every permit would \
             pass both assertions above and fail only this one"
        );
    }

    #[tokio::test]
    async fn a_denied_read_dispatches_nothing() {
        let plugin = RecordingReconciliationPlugin::answering_for(AggregationFold::Sum);
        let service = service_with(plugin.clone(), declaration_with_fold(AggregationFold::Sum));
        service
            .get_reconciliation_metadata(
                &deny_ctx(),
                FIXTURE_TENANT,
                fixture_meter(),
                fixture_range(),
            )
            .await
            .expect_err("a denial must fail closed");
        assert_eq!(plugin.call_count(), 0, "a denial must dispatch nothing");
    }

    #[tokio::test]
    async fn authorization_precedes_declaration_resolution() {
        // A caller with no permission must not learn whether a GTS type
        // exists. The same order `query_aggregated_usage_records` uses.
        let service = service_with(
            RecordingReconciliationPlugin::answering_for(AggregationFold::Sum),
            no_such_declaration(),
        );
        let err = service
            .get_reconciliation_metadata(
                &deny_ctx(),
                FIXTURE_TENANT,
                fixture_meter(),
                fixture_range(),
            )
            .await
            .expect_err("expected the denial");
        assert!(
            matches!(err, UsageCollectorError::PermissionDenied { .. }),
            "an unresolvable meter under a denying PDP must answer the denial, not a 404 that \
             reveals the meter does not exist: got {err:?}"
        );
    }

    #[tokio::test]
    async fn a_plugin_returning_the_wrong_branch_is_an_internal_error() {
        // Without this guard a `SUM` meter answered with an observation count
        // serialises as a valid-looking body.
        let plugin = RecordingReconciliationPlugin::answering_for(AggregationFold::Max); // wrong branch
        let service = service_with(plugin, declaration_with_fold(AggregationFold::Sum));
        let err = service
            .get_reconciliation_metadata(
                &permit_ctx(),
                FIXTURE_TENANT,
                fixture_meter(),
                fixture_range(),
            )
            .await
            .expect_err("a branch/fold disagreement is a host-contract breach");
        assert!(
            matches!(err, UsageCollectorError::Internal { .. }),
            "got {err:?}"
        );
    }
}
