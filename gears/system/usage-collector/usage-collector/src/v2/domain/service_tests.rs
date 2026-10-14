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
use usage_collector_sdk::v2::{UsageCollectorPluginSpecV2, UsageCollectorPluginV2};

use super::*;
use crate::v2::domain::ports::declarations::DeclarationSource;
use crate::v2::domain::test_support::{MockPlugin, UnreachableResolver, enforcer_for};

/// Enforcer for tests that never reach the PDP path (binding / plugin-host
/// tests); its transport panics if an authz call is ever made.
fn dummy_enforcer() -> PolicyEnforcer {
    enforcer_for(Arc::new(UnreachableResolver))
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
        UsageCollectorPluginSpecV2::gts_type_id()
    )
}

/// JSON content for a `PluginV1<UsageCollectorPluginSpecV2>` instance that
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
    plugin: Arc<dyn UsageCollectorPluginV2>,
) -> (Arc<ClientHub>, Arc<MockTypesRegistryClient>) {
    let hub = Arc::new(ClientHub::default());

    let instance = make_test_instance(instance_id, plugin_content(instance_id, vendor));
    let registry = Arc::new(MockTypesRegistryClient::new().with_instances([instance]));
    hub.register::<dyn TypesRegistryClient>(registry.clone() as Arc<dyn TypesRegistryClient>);

    hub.register_scoped::<dyn UsageCollectorPluginV2>(ClientScope::gts_id(instance_id), plugin);

    (hub, registry)
}

fn hub_with_registry_and_plugin(
    instance_id: &str,
    vendor: &str,
    plugin: Arc<dyn UsageCollectorPluginV2>,
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
        _id: &usage_collector_sdk::v2::MeterTypeId,
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
        crate::v2::domain::test_support::inert_reverse_resolver(Arc::new(NoopMetrics)),
        crate::v2::domain::service::ServiceConfig::default(),
    );

    let resolved = svc.get_plugin().await;
    assert!(
        resolved.is_ok(),
        "expected a resolved scoped handle, got: {:?}",
        resolved.err()
    );
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
//
// Withdrawal-exclusion coverage for `get_usage_record` (that both halves of a
// withdrawn pair stay readable by id) is NOT in this module: it lives in
// `aggregate_query_tests.rs::withdrawal_exclusion_tests::
// a_withdrawn_pair_folds_to_nothing_while_both_stay_readable`, alongside the
// aggregate-fold assertion that test's name leads with and which its five
// siblings in that module are all about.
mod get_usage_record_tests {
    use std::sync::Arc;
    use toolkit_gts::gts_id;

    use usage_collector_sdk::v2::{
        USAGE_RECORD_RESOURCE, UsageCollectorError, UsageCollectorPluginV2, UsageRecord,
    };
    use uuid::Uuid;

    use crate::v2::domain::test_support::{
        DenyAllResolver, RecordingPlugin, ServiceFixture, UnreachableResolver, authenticated_ctx,
        recording_plugin_resolver,
    };

    const HAPPY_RECORD_GTS_ID: &str =
        gts_id!("cf.core.uc.usage_record.v2~cf.mini_chat._.tokens_consumed.v1~");

    fn sample_persisted_record(id: Uuid, tenant_id: Uuid) -> UsageRecord {
        use std::collections::BTreeMap;
        use time::OffsetDateTime;
        use usage_collector_sdk::v2::{IdempotencyKey, MeterTypeId, RecordOrigin, ResourceRef};
        UsageRecord {
            id,
            gts_type_id: MeterTypeId::new(HAPPY_RECORD_GTS_ID).expect("valid gts_type_id"),
            tenant_id,
            resource_ref: ResourceRef::new("rsc-happy", "compute.vm").expect("valid resource ref"),
            subject_ref: None,
            metadata: BTreeMap::new(),
            quantity: crate::v2::domain::test_support::qty("1"),
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
                Arc::clone(&plugin) as Arc<dyn UsageCollectorPluginV2>,
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

    /// Applying the compiled scope on the point lookup is the plugin's
    /// alone — it is the whole filter, and nothing on this side makes an
    /// implementation honour it. A plugin that ignores it would turn this
    /// surface into a cross-tenant read, so the gear re-decides the owning
    /// tenant against the grant and answers the miss the plugin owed.
    #[tokio::test]
    async fn get_usage_record_refuses_a_row_outside_the_granted_tenant() {
        let plugin = RecordingPlugin::new();
        let target = Uuid::from_u128(0x00C0_FFEE);
        // The grant covers tenant 2; this row is another tenant's.
        let foreign_tenant = Uuid::from_u128(0xF0E1);
        plugin.set_get_record(sample_persisted_record(target, foreign_tenant));

        let svc = ServiceFixture::default()
            .with_resolver(recording_plugin_resolver())
            .build(
                Arc::clone(&plugin) as Arc<dyn UsageCollectorPluginV2>,
                "test.usage_collector.get_record.cross_tenant.v1",
            );

        let err = svc
            .get_usage_record(&authenticated_ctx(), target)
            .await
            .expect_err("a row outside the grant must not be served");

        // The same envelope a genuine miss produces: telling the two apart
        // would make this surface an existence oracle.
        assert!(
            matches!(err, UsageCollectorError::NotFound { .. }),
            "expected the miss the plugin owed, got {err:?}"
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
                Arc::clone(&plugin) as Arc<dyn UsageCollectorPluginV2>,
                "test.usage_collector.get_record.plugin_not_found.v1",
            );

        let err = svc
            .get_usage_record(&authenticated_ctx(), target)
            .await
            .expect_err("plugin UsageRecordNotFound MUST surface as NotFound");

        assert!(
            matches!(
                err,
                UsageCollectorError::NotFound { resource_type, ref name, .. }
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
                Arc::clone(&plugin) as Arc<dyn UsageCollectorPluginV2>,
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
                Arc::clone(&plugin) as Arc<dyn UsageCollectorPluginV2>,
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
                Arc::clone(&plugin) as Arc<dyn UsageCollectorPluginV2>,
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
        use usage_collector_sdk::v2::UsageCollectorPluginError;

        // `HappyPathPlugin` exposes no transient path, so this stub returns
        // Transient on `get_usage_record` and fails loudly elsewhere.
        struct TransientGetPlugin;

        #[async_trait::async_trait]
        impl UsageCollectorPluginV2 for TransientGetPlugin {
            async fn create_usage_records(
                &self,
                _records: Vec<usage_collector_sdk::v2::MeteredRecord>,
            ) -> Result<
                Vec<Result<usage_collector_sdk::v2::StoredUsageRecord, UsageCollectorPluginError>>,
                UsageCollectorPluginError,
            > {
                Err(UsageCollectorPluginError::internal(
                    "test_fake: TransientGetPlugin: create_usage_records must not be called",
                ))
            }
            async fn query_aggregated_usage_records(
                &self,
                _meter: &usage_collector_sdk::v2::MeterRef,
                _time_range: usage_collector_sdk::v2::TimeRange,
                _fold: usage_collector_sdk::v2::AggregationFold,
                _query: &toolkit_odata::ODataQuery,
                _scope: &toolkit_odata::ast::Expr,
                _metadata_filter: &[usage_collector_sdk::v2::MetadataFilter],
                _group_by: &[usage_collector_sdk::v2::AggregationDimension],
            ) -> Result<usage_collector_sdk::v2::AggregationResult, UsageCollectorPluginError>
            {
                Err(UsageCollectorPluginError::internal(
                    "test_fake: TransientGetPlugin: query_aggregated_usage_records must not be called",
                ))
            }
            async fn list_usage_records(
                &self,
                _meter: &usage_collector_sdk::v2::MeterRef,
                _time_range: usage_collector_sdk::v2::TimeRange,
                _query: &toolkit_odata::ODataQuery,
                _scope: &toolkit_odata::ast::Expr,
                _metadata_filter: &[usage_collector_sdk::v2::MetadataFilter],
                _keyset: Option<&usage_collector_sdk::v2::Keyset>,
            ) -> Result<usage_collector_sdk::v2::RecordPage, UsageCollectorPluginError>
            {
                Err(UsageCollectorPluginError::internal(
                    "test_fake: TransientGetPlugin: list_usage_records must not be called",
                ))
            }
            async fn get_usage_record(
                &self,
                _id: Uuid,
                _scope: &toolkit_odata::ast::Expr,
                _converged_only: bool,
            ) -> Result<usage_collector_sdk::v2::StoredUsageRecord, UsageCollectorPluginError>
            {
                Err(UsageCollectorPluginError::transient(
                    "test_fake: TransientGetPlugin: simulated prefetch transient",
                ))
            }
            async fn read_feed_page(
                &self,
                _subscription: &usage_collector_sdk::v2::SubscribedMeters,
                _scope: &toolkit_odata::ast::Expr,
                _start: usage_collector_sdk::v2::FeedStart<usage_collector_sdk::v2::FeedPosition>,
                _until: Option<usage_collector_sdk::v2::FeedPosition>,
                _limit: u64,
            ) -> Result<
                usage_collector_sdk::v2::FeedPage<
                    usage_collector_sdk::v2::FeedPosition,
                    usage_collector_sdk::v2::StoredUsageRecord,
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
                _meter: &usage_collector_sdk::v2::MeterRef,
                _time_range: usage_collector_sdk::v2::TimeRange,
                _fold: usage_collector_sdk::v2::AggregationFold,
                _scope: &toolkit_odata::ast::Expr,
            ) -> Result<usage_collector_sdk::v2::ReconciliationMetadata, UsageCollectorPluginError>
            {
                Err(UsageCollectorPluginError::internal(
                    "test_fake: TransientGetPlugin: get_reconciliation_metadata must not be called",
                ))
            }
        }

        let plugin: Arc<dyn UsageCollectorPluginV2> = Arc::new(TransientGetPlugin);
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
        assert!(err.is_infrastructure_retryable());
    }
}
